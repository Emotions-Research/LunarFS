use anyhow::Result;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// WsId is the user-facing string identifier for a workspace.
// It is distinct from overlay::WorkspaceId (i64 SQLite rowid).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WsId(pub String);

/// An agent workspace record persisted in the control-plane store.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub id: WsId,
    pub label: Option<String>,
    pub metadata: BTreeMap<String, String>,
    /// The base ref this workspace was forked from (e.g. a root hash or label).
    pub base_ref: String,
    /// None for persistent workspaces, Some(duration) for ephemeral ones.
    pub ttl: Option<Duration>,
    pub created_at: SystemTime,
    pub ephemeral: bool,
}

/// Injectable clock seam. Tests use FakeClock; production uses SystemWsClock.
pub trait WsClock: Send + Sync {
    fn now(&self) -> SystemTime;
}

pub struct SystemWsClock;

impl WsClock for SystemWsClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Clock that can be advanced by tests without sleeping.
pub struct FakeClock {
    time: Mutex<SystemTime>,
}

impl FakeClock {
    pub fn new(t: SystemTime) -> Self {
        Self { time: Mutex::new(t) }
    }

    pub fn advance(&self, d: Duration) {
        let mut t = self.time.lock().expect("FakeClock lock poisoned");
        *t += d;
    }
}

impl WsClock for FakeClock {
    fn now(&self) -> SystemTime {
        *self.time.lock().expect("FakeClock lock poisoned")
    }
}

/// CoW overlay backend seam. The real impl uses the Epic 3 primitive;
/// tests use InMemoryBackend with no filesystem or mount calls.
pub trait OverlayBackend: Send + Sync {
    /// Create a CoW overlay for `ws` forked from `base_ref` (O(1), no data copy).
    fn fork(&self, base_ref: &str, ws: &WsId) -> Result<()>;
    /// Write `data` into `ws`'s overlay at `path`.
    fn write(&self, ws: &WsId, path: &str, data: &[u8]) -> Result<()>;
    /// Read from `ws`'s overlay at `path`. Returns None when the path has no overlay entry.
    fn read(&self, ws: &WsId, path: &str) -> Result<Option<Vec<u8>>>;
    /// Destroy the overlay for `ws`, dropping all overlay state and the base ref pointer.
    fn destroy(&self, ws: &WsId) -> Result<()>;
    /// Return true when the overlay for `ws` exists.
    fn exists(&self, ws: &WsId) -> Result<bool>;
}

/// In-memory backend for deterministic tests. No filesystem, no mounts, no downloads.
///
/// Each forked workspace gets its own overlay (path -> bytes). Bases are tracked
/// but contain no file data by default; the isolation property is verified by
/// checking that sibling workspace overlays are independent.
pub struct InMemoryBackend {
    // ws_id -> overlay file map
    overlays: Mutex<HashMap<String, HashMap<String, Vec<u8>>>>,
    // tracks which ws_ids have been forked (controls exists())
    forked: Mutex<HashSet<String>>,
}

impl InMemoryBackend {
    pub fn new() -> Self {
        Self {
            overlays: Mutex::new(HashMap::new()),
            forked: Mutex::new(HashSet::new()),
        }
    }
}

impl Default for InMemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl OverlayBackend for InMemoryBackend {
    fn fork(&self, _base_ref: &str, ws: &WsId) -> Result<()> {
        assert!(!ws.0.is_empty(), "WsId must not be empty in fork");
        let mut forked = self.forked.lock().expect("forked lock poisoned");
        let mut overlays = self.overlays.lock().expect("overlays lock poisoned");
        forked.insert(ws.0.clone());
        overlays.entry(ws.0.clone()).or_default();
        Ok(())
    }

    fn write(&self, ws: &WsId, path: &str, data: &[u8]) -> Result<()> {
        assert!(!ws.0.is_empty(), "WsId must not be empty in write");
        assert!(path.len() <= 4096, "path must not exceed 4096 bytes");
        let mut overlays = self.overlays.lock().expect("overlays lock poisoned");
        let ws_overlay = overlays
            .get_mut(&ws.0)
            .ok_or_else(|| anyhow::anyhow!("workspace {} not found in backend", ws.0))?;
        ws_overlay.insert(path.to_string(), data.to_vec());
        Ok(())
    }

    fn read(&self, ws: &WsId, path: &str) -> Result<Option<Vec<u8>>> {
        assert!(!ws.0.is_empty(), "WsId must not be empty in read");
        assert!(path.len() <= 4096, "path must not exceed 4096 bytes");
        let overlays = self.overlays.lock().expect("overlays lock poisoned");
        let ws_overlay = overlays
            .get(&ws.0)
            .ok_or_else(|| anyhow::anyhow!("workspace {} not found in backend", ws.0))?;
        Ok(ws_overlay.get(path).cloned())
    }

    fn destroy(&self, ws: &WsId) -> Result<()> {
        assert!(!ws.0.is_empty(), "WsId must not be empty in destroy");
        let mut overlays = self.overlays.lock().expect("overlays lock poisoned");
        let mut forked = self.forked.lock().expect("forked lock poisoned");
        overlays.remove(&ws.0);
        forked.remove(&ws.0);
        Ok(())
    }

    fn exists(&self, ws: &WsId) -> Result<bool> {
        assert!(!ws.0.is_empty(), "WsId must not be empty in exists");
        let forked = self.forked.lock().expect("forked lock poisoned");
        Ok(forked.contains(&ws.0))
    }
}

/// Filesystem-backed overlay backend. Delegates the CoW concept to directory
/// structure: fork is O(1) (creates an empty overlay dir + a base-ref marker),
/// writes are scoped to the workspace dir, destroy removes the dir tree.
pub struct LocalFsBackend {
    root: std::path::PathBuf,
}

impl LocalFsBackend {
    pub fn new(root: std::path::PathBuf) -> Self {
        assert!(!root.as_os_str().is_empty(), "LocalFsBackend root must not be empty");
        Self { root }
    }

    fn ws_dir(&self, ws: &WsId) -> std::path::PathBuf {
        assert!(!ws.0.is_empty(), "WsId must not be empty");
        self.root.join(&ws.0)
    }
}

impl OverlayBackend for LocalFsBackend {
    fn fork(&self, base_ref: &str, ws: &WsId) -> Result<()> {
        assert!(!base_ref.is_empty(), "base_ref must not be empty in fork");
        assert!(!ws.0.is_empty(), "WsId must not be empty in fork");
        let dir = self.ws_dir(ws);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(".base_ref"), base_ref)?;
        assert!(dir.is_dir(), "workspace dir must exist after fork");
        Ok(())
    }

    fn write(&self, ws: &WsId, path: &str, data: &[u8]) -> Result<()> {
        assert!(!path.is_empty(), "path must not be empty in write");
        assert!(path.len() <= 4096, "path must not exceed 4096 bytes");
        let dir = self.ws_dir(ws);
        let target = dir.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, data)?;
        Ok(())
    }

    fn read(&self, ws: &WsId, path: &str) -> Result<Option<Vec<u8>>> {
        assert!(path.len() <= 4096, "path must not exceed 4096 bytes");
        let target = self.ws_dir(ws).join(path);
        if !target.exists() {
            return Ok(None);
        }
        Ok(Some(std::fs::read(&target)?))
    }

    fn destroy(&self, ws: &WsId) -> Result<()> {
        assert!(!ws.0.is_empty(), "WsId must not be empty in destroy");
        let dir = self.ws_dir(ws);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        assert!(!dir.exists(), "workspace dir must not exist after destroy");
        Ok(())
    }

    fn exists(&self, ws: &WsId) -> Result<bool> {
        assert!(!ws.0.is_empty(), "WsId must not be empty in exists");
        Ok(self.ws_dir(ws).is_dir())
    }
}

/// Generate a random 16-hex-char workspace ID using the system RNG.
pub fn new_ws_id() -> WsId {
    let mut bytes = [0u8; 8];
    getrandom::getrandom(&mut bytes).expect("getrandom must succeed for workspace ID generation");
    let hex: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
    WsId(hex)
}

/// Parameters for creating a new workspace (bundles the user-supplied fields).
pub struct WorkspaceSpec {
    pub base_ref: String,
    pub label: Option<String>,
    pub metadata: BTreeMap<String, String>,
    /// None for a persistent workspace, Some for an ephemeral one.
    pub ttl: Option<Duration>,
}

// --- Lifecycle API ---

/// Create a new workspace via instant CoW fork and persist it to the store.
pub fn create_workspace(
    backend: &dyn OverlayBackend,
    store: &dyn crate::store::WorkspaceStore,
    clock: &dyn WsClock,
    id: WsId,
    spec: WorkspaceSpec,
) -> Result<Workspace> {
    assert!(!id.0.is_empty(), "workspace id must not be empty");
    assert!(!spec.base_ref.is_empty(), "base_ref must not be empty");
    let ephemeral = spec.ttl.is_some();
    let created_at = clock.now();
    backend.fork(&spec.base_ref, &id)?;
    let ws = Workspace {
        id,
        label: spec.label,
        metadata: spec.metadata,
        base_ref: spec.base_ref,
        ttl: spec.ttl,
        created_at,
        ephemeral,
    };
    store.save(&ws)?;
    Ok(ws)
}

/// Destroy a workspace: drop the overlay, remove the store record.
/// Returns an error when the workspace does not exist.
pub fn destroy_workspace(
    backend: &dyn OverlayBackend,
    store: &dyn crate::store::WorkspaceStore,
    id: &WsId,
) -> Result<()> {
    assert!(!id.0.is_empty(), "workspace id must not be empty");
    if !backend.exists(id)? {
        anyhow::bail!("workspace {} does not exist", id.0);
    }
    backend.destroy(id)?;
    store.remove(id)?;
    assert!(!backend.exists(id)?, "backend must not have overlay after destroy");
    Ok(())
}

/// List all persisted workspace records.
pub fn list_workspaces(store: &dyn crate::store::WorkspaceStore) -> Result<Vec<Workspace>> {
    store.list_all()
}

/// Sweep expired ephemeral workspaces. Returns the count of workspaces destroyed.
/// Non-ephemeral workspaces (ttl is None) are never touched.
pub fn sweep_ttl(
    backend: &dyn OverlayBackend,
    store: &dyn crate::store::WorkspaceStore,
    now: SystemTime,
) -> Result<usize> {
    let all = store.list_all()?;
    assert!(all.len() <= 1_000_000, "workspace list exceeds sanity cap");
    let mut swept = 0;
    for ws in all {
        let Some(ttl) = ws.ttl else { continue };
        let expiry = ws.created_at + ttl;
        if now < expiry {
            continue;
        }
        if backend.exists(&ws.id)? {
            backend.destroy(&ws.id)?;
        }
        store.remove(&ws.id)?;
        swept += 1;
    }
    Ok(swept)
}

/// Format a SystemTime as seconds since UNIX_EPOCH (for display).
pub fn secs_since_epoch(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{InMemoryWorkspaceStore, WorkspaceStore as _};

    fn fake_clock() -> FakeClock {
        FakeClock::new(UNIX_EPOCH)
    }

    fn spec(base_ref: &str, ttl: Option<Duration>) -> WorkspaceSpec {
        WorkspaceSpec {
            base_ref: base_ref.to_string(),
            label: None,
            metadata: BTreeMap::new(),
            ttl,
        }
    }

    // (a) Isolation: writes to workspace A do not appear in workspace B,
    // and neither A nor the base (empty by construction) contain B's writes.
    #[test]
    fn test_isolation() {
        let backend = InMemoryBackend::new();
        let store = InMemoryWorkspaceStore::new();
        let clock = fake_clock();

        let id_a = WsId("ws-a".to_string());
        let id_b = WsId("ws-b".to_string());

        create_workspace(&backend, &store, &clock, id_a.clone(), spec("base-v1", None))
            .expect("create A");
        create_workspace(&backend, &store, &clock, id_b.clone(), spec("base-v1", None))
            .expect("create B");

        backend.write(&id_a, "file.txt", b"hello from A").expect("write to A");

        // B does not see A's write.
        let b_view = backend.read(&id_b, "file.txt").expect("read from B");
        assert!(b_view.is_none(), "B must not see A's write");

        // A still has its own write.
        let a_view = backend.read(&id_a, "file.txt").expect("read from A");
        assert_eq!(a_view, Some(b"hello from A".to_vec()), "A must see its own write");

        backend.write(&id_b, "other.txt", b"hello from B").expect("write to B");

        // A does not see B's write.
        let a_view2 = backend.read(&id_a, "other.txt").expect("read A for B's path");
        assert!(a_view2.is_none(), "A must not see B's write");
    }

    // (b) Clean destroy: after destroy, backend.exists is false, store.get is None,
    // and list_workspaces no longer includes the destroyed workspace.
    #[test]
    fn test_destroy_clean() {
        let backend = InMemoryBackend::new();
        let store = InMemoryWorkspaceStore::new();
        let clock = fake_clock();

        let id = WsId("ws-x".to_string());
        create_workspace(&backend, &store, &clock, id.clone(), spec("base", None))
            .expect("create workspace");

        assert!(backend.exists(&id).expect("exists before destroy"), "must exist before destroy");
        assert!(store.get(&id).expect("store get before destroy").is_some(), "must be in store before destroy");

        destroy_workspace(&backend, &store, &id).expect("destroy must succeed");

        assert!(!backend.exists(&id).expect("exists after destroy"), "backend must not have overlay");
        assert!(store.get(&id).expect("store get after destroy").is_none(), "store must have no record");

        let list = list_workspaces(&store).expect("list after destroy");
        assert!(!list.iter().any(|w| w.id == id), "list must not contain destroyed workspace");
    }

    // (c) TTL sweep: advancing the clock past an ephemeral workspace's TTL triggers
    // cleanup; a persistent workspace (ttl None) survives the sweep.
    #[test]
    fn test_ttl_sweep() {
        let backend = InMemoryBackend::new();
        let store = InMemoryWorkspaceStore::new();
        let clock = fake_clock();

        let ephemeral_id = WsId("ws-ephemeral".to_string());
        let persistent_id = WsId("ws-persistent".to_string());
        let ttl = Duration::from_secs(3600);

        create_workspace(
            &backend, &store, &clock,
            ephemeral_id.clone(), spec("base", Some(ttl)),
        ).expect("create ephemeral");
        create_workspace(
            &backend, &store, &clock,
            persistent_id.clone(), spec("base", None),
        ).expect("create persistent");

        // Advance past TTL.
        clock.advance(Duration::from_secs(3601));
        let now = clock.now();

        let swept = sweep_ttl(&backend, &store, now).expect("sweep_ttl");
        assert_eq!(swept, 1, "exactly one workspace should have been swept");

        // Ephemeral is gone from backend and store.
        assert!(!backend.exists(&ephemeral_id).expect("exists ephemeral after sweep"), "ephemeral overlay must be gone");
        assert!(store.get(&ephemeral_id).expect("store get ephemeral").is_none(), "ephemeral record must be gone");

        // Persistent survives.
        assert!(backend.exists(&persistent_id).expect("exists persistent after sweep"), "persistent overlay must survive");
        assert!(store.get(&persistent_id).expect("store get persistent").is_some(), "persistent record must survive");
    }

    // destroy_workspace returns an error when the workspace does not exist.
    #[test]
    fn test_destroy_nonexistent_errors() {
        let backend = InMemoryBackend::new();
        let store = InMemoryWorkspaceStore::new();
        let id = WsId("ws-ghost".to_string());
        let err = destroy_workspace(&backend, &store, &id);
        assert!(err.is_err(), "destroy on nonexistent workspace must error");
    }

    // Partial TTL: a workspace that has not yet expired must survive the sweep.
    #[test]
    fn test_ttl_sweep_not_yet_expired() {
        let backend = InMemoryBackend::new();
        let store = InMemoryWorkspaceStore::new();
        let clock = fake_clock();

        let id = WsId("ws-fresh".to_string());
        let ttl = Duration::from_secs(3600);

        create_workspace(&backend, &store, &clock, id.clone(), spec("base", Some(ttl)))
            .expect("create ephemeral");

        // Advance only partway through the TTL.
        clock.advance(Duration::from_secs(1800));
        let now = clock.now();
        let swept = sweep_ttl(&backend, &store, now).expect("sweep_ttl");

        assert_eq!(swept, 0, "no workspaces should be swept before TTL expires");
        assert!(backend.exists(&id).expect("exists"), "workspace must still exist");
    }
}
