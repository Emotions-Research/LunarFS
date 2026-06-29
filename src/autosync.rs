use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use crate::cas::{hash_to_hex, Hash, Store};
use crate::tree::{deserialize_tree, MODE_DIR};

// ---------------------------------------------------------------------------
// Seam types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceKind {
    Human,
    Shared,
    Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteEventKind {
    Write,
    Create,
    Delete,
    Rename,
}

#[derive(Debug, Clone)]
pub struct WriteEvent {
    pub path: String,
    pub kind: WriteEventKind,
    pub at_ms: u64,
}

// ---------------------------------------------------------------------------
// Trait seams
// ---------------------------------------------------------------------------

/// Event source seam. Real impl wraps a channel fed from FUSE callbacks;
/// test impl lets the test push synthetic WriteEvents.
pub trait WatchSource: Send {
    fn drain(&mut self) -> Vec<WriteEvent>;
}

/// Clock seam. Real impl uses wall time; test impl is an AtomicU64 the test advances.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

/// Snapshot seam. Returns (root_hash_hex, all_blob_hashes_hex) for the current state.
pub trait Snapshotter: Send {
    fn snapshot(&self) -> anyhow::Result<(String, Vec<String>)>;
}

/// Blob upload seam (Epic 1 missing-blobs + upload + ref-advance).
pub trait BlobUploader: Send {
    /// Returns the subset of `hashes` absent on the remote.
    fn missing(&self, hashes: &[String]) -> anyhow::Result<Vec<String>>;
    /// Uploads the blobs identified by `hashes`, fetching bytes from the local store.
    fn upload(&self, hashes: &[String]) -> anyhow::Result<()>;
    /// Advances the workspace ref to `root`.
    fn put_ref(&self, workspace: &str, root: &str) -> anyhow::Result<()>;
}

// ---------------------------------------------------------------------------
// Real impls (no new deps)
// ---------------------------------------------------------------------------

/// Channel-backed WatchSource. A `Sender<WriteEvent>` is held by the FUSE write path;
/// the engine drains from the `Receiver` on each tick.
pub struct ChannelWatchSource {
    rx: std::sync::mpsc::Receiver<WriteEvent>,
}

impl ChannelWatchSource {
    pub fn new(rx: std::sync::mpsc::Receiver<WriteEvent>) -> Self {
        Self { rx }
    }
}

impl WatchSource for ChannelWatchSource {
    fn drain(&mut self) -> Vec<WriteEvent> {
        const MAX_DRAIN: usize = 4096;
        let mut events: Vec<WriteEvent> = Vec::with_capacity(64);
        for _ in 0..MAX_DRAIN {
            match self.rx.try_recv() {
                Ok(e) => events.push(e),
                Err(_) => break,
            }
        }
        events
    }
}

/// Wall-clock impl.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

// ---------------------------------------------------------------------------
// Debounce gate
// ---------------------------------------------------------------------------

/// Arms on each event, fires exactly once per quiet period of >= `debounce_ms`.
pub struct DebounceGate {
    debounce_ms: u64,
    last_event_ms: Option<u64>,
    settled: bool,
}

impl DebounceGate {
    pub fn new(debounce_ms: u64) -> Self {
        assert!(debounce_ms > 0, "debounce_ms must be positive");
        assert!(debounce_ms <= 3_600_000, "debounce_ms must not exceed 1 hour");
        Self { debounce_ms, last_event_ms: None, settled: false }
    }

    /// Record an event at `now_ms`, (re)arming the timer.
    pub fn on_event(&mut self, now_ms: u64) {
        self.last_event_ms = Some(now_ms);
        self.settled = false;
    }

    /// Returns true exactly once when `now_ms - last_event_ms >= debounce_ms`.
    /// Returns false on every subsequent call until the gate is reset or a new event arrives.
    pub fn check_settle(&mut self, now_ms: u64) -> bool {
        let last = match self.last_event_ms {
            Some(t) => t,
            None => return false,
        };
        if self.settled {
            return false;
        }
        if now_ms.saturating_sub(last) >= self.debounce_ms {
            self.settled = true;
            return true;
        }
        false
    }

    /// Reset after a successful pipeline run; arms for the next burst.
    pub fn reset_after_settle(&mut self) {
        self.last_event_ms = None;
        self.settled = false;
    }
}

// ---------------------------------------------------------------------------
// Auto-sync engine
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct SyncResult {
    pub root: String,
    pub uploaded: usize,
}

pub struct AutoSyncEngine {
    watch: Box<dyn WatchSource>,
    clock: Arc<dyn Clock>,
    snapshotter: Box<dyn Snapshotter>,
    uploader: Box<dyn BlobUploader>,
    workspace: String,
    kind: WorkspaceKind,
    gate: DebounceGate,
    disabled: bool,
}

impl AutoSyncEngine {
    /// `disabled` should be `is_autosync_disabled()` in production or `false` in tests.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        watch: Box<dyn WatchSource>,
        clock: Arc<dyn Clock>,
        snapshotter: Box<dyn Snapshotter>,
        uploader: Box<dyn BlobUploader>,
        workspace: impl Into<String>,
        kind: WorkspaceKind,
        debounce_ms: u64,
        disabled: bool,
    ) -> Self {
        assert!(debounce_ms > 0, "debounce_ms must be positive");
        let workspace = workspace.into();
        assert!(!workspace.is_empty(), "workspace must not be empty");
        Self {
            watch,
            clock,
            snapshotter,
            uploader,
            workspace,
            kind,
            gate: DebounceGate::new(debounce_ms),
            disabled,
        }
    }

    /// Process one tick: drain events, update the gate, run the pipeline if settled.
    ///
    /// Agent workspaces and disabled engines always return None without touching the
    /// gate or pipeline. On pipeline error the ref is left unadvanced; the gate stays
    /// armed so the next event burst causes a retry.
    pub fn tick(&mut self) -> Option<SyncResult> {
        assert!(!self.workspace.is_empty(), "workspace must not be empty");
        if self.disabled || matches!(self.kind, WorkspaceKind::Agent) {
            return None;
        }
        let now = self.clock.now_ms();
        let events = self.watch.drain();
        for _ in &events {
            self.gate.on_event(now);
        }
        if !self.gate.check_settle(now) {
            return None;
        }
        match self.pipeline_inner() {
            Ok(result) => {
                self.gate.reset_after_settle();
                Some(result)
            }
            Err(e) => {
                eprintln!(
                    "autosync[{}]: pipeline error (ref unadvanced, will retry on next settle): {}",
                    self.workspace, e
                );
                None
            }
        }
    }

    /// Background loop: tick at `poll_ms` intervals until `stop` is set.
    pub fn run_blocking(&mut self, poll_ms: u64, stop: &std::sync::atomic::AtomicBool) {
        assert!(poll_ms > 0, "poll_ms must be positive");
        assert!(poll_ms <= 60_000, "poll_ms must not exceed 60 seconds");
        const MAX_TICKS: usize = 10_000_000;
        for _ in 0..MAX_TICKS {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            self.tick();
            std::thread::sleep(std::time::Duration::from_millis(poll_ms));
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
        }
    }

    fn pipeline_inner(&self) -> anyhow::Result<SyncResult> {
        assert!(!self.workspace.is_empty(), "workspace must not be empty in pipeline");
        let (root, all_hashes) = self.snapshotter.snapshot()?;
        anyhow::ensure!(!root.is_empty(), "snapshot returned empty root hash");
        let missing = self.uploader.missing(&all_hashes)?;
        let upload_count = missing.len();
        self.uploader.upload(&missing)?;
        self.uploader.put_ref(&self.workspace, &root)?;
        Ok(SyncResult { root, uploaded: upload_count })
    }
}

// ---------------------------------------------------------------------------
// Real Snapshotter: walk_repo + tree traversal
// ---------------------------------------------------------------------------

pub struct WalkSnapshotter {
    store: Arc<dyn Store>,
    mount_path: PathBuf,
}

impl WalkSnapshotter {
    pub fn new(store: Arc<dyn Store>, mount_path: impl Into<PathBuf>) -> Self {
        let mount_path = mount_path.into();
        assert!(mount_path.is_dir(), "mount_path must be an existing directory");
        Self { store, mount_path }
    }
}

impl Snapshotter for WalkSnapshotter {
    fn snapshot(&self) -> anyhow::Result<(String, Vec<String>)> {
        assert!(self.mount_path.is_dir(), "mount_path must exist at snapshot time");
        let root = crate::ingest::walk_repo(&*self.store, &self.mount_path)
            .map_err(|e| anyhow::anyhow!("walk_repo failed: {}", e))?;
        let all = collect_reachable_hashes(&*self.store, &root)?;
        let hex_hashes: Vec<String> = all.iter().map(hash_to_hex).collect();
        Ok((hash_to_hex(&root), hex_hashes))
    }
}

/// Collect all blob hashes reachable from `root` (trees and files).
/// Same algorithm as sync::collect_all_hashes but operates on the public Store API.
fn collect_reachable_hashes(store: &dyn Store, root: &Hash) -> anyhow::Result<Vec<Hash>> {
    const MAX_NODES: usize = 65_536;
    let mut all: Vec<Hash> = Vec::new();
    let mut visited: HashSet<Hash> = HashSet::new();
    let mut stack: Vec<(Hash, bool)> = vec![(*root, true)];
    let mut count = 0usize;

    while let Some((hash, is_tree)) = stack.pop() {
        if visited.contains(&hash) {
            continue;
        }
        count += 1;
        anyhow::ensure!(count <= MAX_NODES, "tree DAG exceeds {} nodes", MAX_NODES);
        visited.insert(hash);
        all.push(hash);
        if !is_tree {
            continue;
        }
        let blob = store
            .get(&hash)
            .map_err(|e| anyhow::anyhow!("read tree blob {}: {}", hash_to_hex(&hash), e))?
            .ok_or_else(|| anyhow::anyhow!("tree blob {} not in store", hash_to_hex(&hash)))?;
        let entries = deserialize_tree(&blob)
            .map_err(|e| anyhow::anyhow!("deserialize tree {}: {}", hash_to_hex(&hash), e))?;
        for entry in entries {
            stack.push((entry.hash, entry.mode == MODE_DIR));
        }
    }

    Ok(all)
}

// ---------------------------------------------------------------------------
// Real BlobUploader: thin sync wrapper over HttpRemote
// ---------------------------------------------------------------------------

pub struct HttpBlobUploader {
    remote: crate::remote::HttpRemote,
    store: Arc<dyn Store>,
    rt: tokio::runtime::Runtime,
}

impl HttpBlobUploader {
    pub fn new(
        remote: crate::remote::HttpRemote,
        store: Arc<dyn Store>,
    ) -> anyhow::Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| anyhow::anyhow!("tokio runtime: {}", e))?;
        Ok(Self { remote, store, rt })
    }
}

impl BlobUploader for HttpBlobUploader {
    fn missing(&self, hashes: &[String]) -> anyhow::Result<Vec<String>> {
        assert!(hashes.len() <= 65_536, "hash list exceeds cap");
        let parsed: Vec<Hash> = hashes
            .iter()
            .map(|h| {
                crate::cas::hex_to_hash(h)
                    .map_err(|e| anyhow::anyhow!("bad hash {}: {}", h, e))
            })
            .collect::<anyhow::Result<_>>()?;
        let missing = self.rt.block_on(self.remote.missing_blobs(&parsed, None))?;
        Ok(missing.iter().map(hash_to_hex).collect())
    }

    fn upload(&self, hashes: &[String]) -> anyhow::Result<()> {
        assert!(hashes.len() <= 65_536, "hash list exceeds cap");
        for hex in hashes {
            let hash = crate::cas::hex_to_hash(hex)
                .map_err(|e| anyhow::anyhow!("bad hash {}: {}", hex, e))?;
            let data = self
                .store
                .get(&hash)
                .map_err(|e| anyhow::anyhow!("local store read {}: {}", hex, e))?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "blob {} listed as missing but absent from local store",
                        hex
                    )
                })?;
            self.rt.block_on(self.remote.put_blob(&hash, data, None))?;
        }
        Ok(())
    }

    fn put_ref(&self, workspace: &str, root: &str) -> anyhow::Result<()> {
        assert!(!workspace.is_empty(), "workspace must not be empty");
        assert!(!root.is_empty(), "root must not be empty");
        let hash = crate::cas::hex_to_hash(root)
            .map_err(|e| anyhow::anyhow!("bad root hash {}: {}", root, e))?;
        self.rt.block_on(self.remote.put_ref(workspace, &hash))
    }
}

// ---------------------------------------------------------------------------
// Off-switch helper (callers use this; tests pass disabled=false directly)
// ---------------------------------------------------------------------------

/// Returns true when LUNAR_AUTOSYNC_DISABLED is set in the environment.
pub fn is_autosync_disabled() -> bool {
    std::env::var("LUNAR_AUTOSYNC_DISABLED").is_ok()
}

// ---------------------------------------------------------------------------
// Tests (fully hermetic: no real filesystem watcher, no real timers, no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    // -- Fake WatchSource --

    struct FakeWatchSource {
        queue: Arc<Mutex<Vec<WriteEvent>>>,
    }

    impl FakeWatchSource {
        fn new() -> (Self, Arc<Mutex<Vec<WriteEvent>>>) {
            let q = Arc::new(Mutex::new(Vec::new()));
            (Self { queue: Arc::clone(&q) }, q)
        }
    }

    impl WatchSource for FakeWatchSource {
        fn drain(&mut self) -> Vec<WriteEvent> {
            let mut q = self.queue.lock().expect("FakeWatchSource queue lock poisoned");
            std::mem::take(&mut *q)
        }
    }

    // -- Fake Clock --

    struct FakeClock(Arc<AtomicU64>);

    impl FakeClock {
        fn new(initial: u64) -> (Self, Arc<AtomicU64>) {
            let inner = Arc::new(AtomicU64::new(initial));
            (FakeClock(Arc::clone(&inner)), inner)
        }
    }

    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::Relaxed)
        }
    }

    // -- Fake Snapshotter --

    struct FakeSnapshotter {
        root: String,
        all_hashes: Vec<String>,
    }

    impl Snapshotter for FakeSnapshotter {
        fn snapshot(&self) -> anyhow::Result<(String, Vec<String>)> {
            assert!(!self.root.is_empty(), "FakeSnapshotter root must not be empty");
            Ok((self.root.clone(), self.all_hashes.clone()))
        }
    }

    // -- Fake BlobUploader --

    #[derive(Clone)]
    struct Recorded {
        uploaded: Arc<Mutex<Vec<String>>>,
        refs: Arc<Mutex<Vec<String>>>,
    }

    impl Recorded {
        fn new() -> Self {
            Self {
                uploaded: Arc::new(Mutex::new(Vec::new())),
                refs: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn uploaded_hashes(&self) -> Vec<String> {
            self.uploaded.lock().expect("uploaded lock poisoned").clone()
        }

        fn advanced_refs(&self) -> Vec<String> {
            self.refs.lock().expect("refs lock poisoned").clone()
        }
    }

    struct FakeBlobUploader {
        already_present: HashSet<String>,
        recorded: Recorded,
    }

    impl FakeBlobUploader {
        fn new(already_present: &[&str], recorded: Recorded) -> Self {
            Self {
                already_present: already_present.iter().map(|s| s.to_string()).collect(),
                recorded,
            }
        }
    }

    impl BlobUploader for FakeBlobUploader {
        fn missing(&self, hashes: &[String]) -> anyhow::Result<Vec<String>> {
            assert!(hashes.len() <= 65_536, "hash list too large");
            Ok(hashes
                .iter()
                .filter(|h| !self.already_present.contains(*h))
                .cloned()
                .collect())
        }

        fn upload(&self, hashes: &[String]) -> anyhow::Result<()> {
            assert!(hashes.len() <= 65_536, "hash list too large");
            self.recorded
                .uploaded
                .lock()
                .expect("upload lock poisoned")
                .extend(hashes.iter().cloned());
            Ok(())
        }

        fn put_ref(&self, _workspace: &str, root: &str) -> anyhow::Result<()> {
            assert!(!root.is_empty(), "put_ref root must not be empty");
            self.recorded
                .refs
                .lock()
                .expect("refs lock poisoned")
                .push(root.to_string());
            Ok(())
        }
    }

    fn make_event(path: &str) -> WriteEvent {
        WriteEvent { path: path.to_string(), kind: WriteEventKind::Write, at_ms: 0 }
    }

    const DEBOUNCE_MS: u64 = 500;

    // -----------------------------------------------------------------------
    // Test 1: human workspace -- burst collapses to one settle, only changed
    //         blobs uploaded, unchanged blobs deduped away.
    // -----------------------------------------------------------------------
    #[test]
    fn human_workspace_debounce_and_dedup() {
        let changed = "aaaa000000000000000000000000000000000000000000000000000000000001";
        let present = "bbbb000000000000000000000000000000000000000000000000000000000002";
        let root_hex = "cccc000000000000000000000000000000000000000000000000000000000003";

        let (watch_src, event_queue) = FakeWatchSource::new();
        let (fake_clock, clock_val) = FakeClock::new(0);
        let recorded = Recorded::new();

        let mut engine = AutoSyncEngine::new(
            Box::new(watch_src),
            Arc::new(fake_clock),
            Box::new(FakeSnapshotter {
                root: root_hex.to_string(),
                all_hashes: vec![changed.to_string(), present.to_string()],
            }),
            Box::new(FakeBlobUploader::new(&[present], recorded.clone())),
            "my-ws",
            WorkspaceKind::Human,
            DEBOUNCE_MS,
            false,
        );

        // Push three events at T=0.
        for path in &["a.txt", "b.txt", "c.txt"] {
            event_queue.lock().unwrap().push(make_event(path));
        }

        // Tick at T=0: events drained and gate armed; debounce not elapsed.
        clock_val.store(0, Ordering::Relaxed);
        assert!(engine.tick().is_none(), "no settle at T=0");
        assert!(recorded.advanced_refs().is_empty(), "no refs at T=0");

        // Tick at T=499: still within debounce window.
        clock_val.store(499, Ordering::Relaxed);
        assert!(engine.tick().is_none(), "no settle at T=499 (< 500ms)");

        // Re-arm with one more event at T=499.
        event_queue.lock().unwrap().push(make_event("d.txt"));
        assert!(engine.tick().is_none(), "no settle immediately after re-arm");

        // Tick at T=1000: > 500ms since last event at T=499.
        clock_val.store(1000, Ordering::Relaxed);
        let result = engine.tick();
        assert!(result.is_some(), "settle must fire at T=1000");

        // Exactly one ref advanced to the snapshot root.
        let refs = recorded.advanced_refs();
        assert_eq!(refs.len(), 1, "exactly one ref must be advanced");
        assert_eq!(refs[0], root_hex, "ref must match snapshot root");

        // Only the missing (changed) blob uploaded; present deduped away.
        let uploaded = recorded.uploaded_hashes();
        assert_eq!(uploaded.len(), 1, "only the missing blob must be uploaded");
        assert_eq!(uploaded[0], changed, "changed blob must be uploaded");
        assert!(
            !uploaded.contains(&present.to_string()),
            "already-present blob must NOT be uploaded"
        );

        // Second tick at same time: no duplicate settle (gate reset after pipeline).
        assert!(engine.tick().is_none(), "no second settle without new events");
        assert_eq!(recorded.advanced_refs().len(), 1, "ref count must still be 1");
    }

    // -----------------------------------------------------------------------
    // Test 2: agent workspace -- NEVER advances ref, NEVER uploads, even after
    //         debounce window elapses.
    // -----------------------------------------------------------------------
    #[test]
    fn agent_workspace_never_advances() {
        let changed = "aaaa000000000000000000000000000000000000000000000000000000000001";
        let root_hex = "cccc000000000000000000000000000000000000000000000000000000000003";

        let (watch_src, event_queue) = FakeWatchSource::new();
        let (fake_clock, clock_val) = FakeClock::new(0);
        let recorded = Recorded::new();

        let mut engine = AutoSyncEngine::new(
            Box::new(watch_src),
            Arc::new(fake_clock),
            Box::new(FakeSnapshotter {
                root: root_hex.to_string(),
                all_hashes: vec![changed.to_string()],
            }),
            Box::new(FakeBlobUploader::new(&[], recorded.clone())),
            "agent-ws",
            WorkspaceKind::Agent,
            DEBOUNCE_MS,
            false,
        );

        for path in &["x.txt", "y.txt"] {
            event_queue.lock().unwrap().push(make_event(path));
        }
        clock_val.store(1000, Ordering::Relaxed);

        let result = engine.tick();
        assert!(result.is_none(), "agent workspace must NEVER settle");
        assert!(recorded.advanced_refs().is_empty(), "agent must produce zero refs");
        assert!(recorded.uploaded_hashes().is_empty(), "agent must upload nothing");
    }

    // -----------------------------------------------------------------------
    // Test 3: debounce gate fires exactly once per burst, resets cleanly.
    // -----------------------------------------------------------------------
    #[test]
    fn debounce_gate_fires_once_per_burst() {
        let mut gate = DebounceGate::new(500);

        gate.on_event(0);
        assert!(!gate.check_settle(0), "must not settle at same time as event");
        assert!(!gate.check_settle(499), "must not settle before debounce_ms");
        assert!(gate.check_settle(500), "must settle exactly at debounce boundary");
        assert!(!gate.check_settle(1000), "must not settle twice without a new event");

        gate.reset_after_settle();
        assert!(!gate.check_settle(1000), "after reset, must not settle without a new event");

        gate.on_event(1000);
        assert!(!gate.check_settle(1499), "new burst: must not settle before debounce");
        assert!(gate.check_settle(1500), "new burst: must settle at debounce boundary");
    }

    // -----------------------------------------------------------------------
    // Test 4: disabled engine is fully inert for human workspaces.
    // -----------------------------------------------------------------------
    #[test]
    fn disabled_engine_is_inert() {
        let root_hex = "cccc000000000000000000000000000000000000000000000000000000000003";
        let (watch_src, event_queue) = FakeWatchSource::new();
        let (fake_clock, clock_val) = FakeClock::new(0);
        let recorded = Recorded::new();

        let mut engine = AutoSyncEngine::new(
            Box::new(watch_src),
            Arc::new(fake_clock),
            Box::new(FakeSnapshotter {
                root: root_hex.to_string(),
                all_hashes: vec!["hash1".to_string()],
            }),
            Box::new(FakeBlobUploader::new(&[], recorded.clone())),
            "ws",
            WorkspaceKind::Human,
            500,
            true, // disabled
        );

        event_queue.lock().unwrap().push(make_event("file.txt"));
        clock_val.store(1000, Ordering::Relaxed);

        assert!(engine.tick().is_none(), "disabled engine must not tick");
        assert!(recorded.advanced_refs().is_empty(), "disabled engine must not advance refs");
        assert!(recorded.uploaded_hashes().is_empty(), "disabled engine must not upload");
    }

    // -----------------------------------------------------------------------
    // Test 5: shared workspace behaves identically to human.
    // -----------------------------------------------------------------------
    #[test]
    fn shared_workspace_auto_syncs() {
        let root_hex = "dddd000000000000000000000000000000000000000000000000000000000004";
        let blob = "eeee000000000000000000000000000000000000000000000000000000000005";
        let (watch_src, event_queue) = FakeWatchSource::new();
        let (fake_clock, clock_val) = FakeClock::new(0);
        let recorded = Recorded::new();

        let mut engine = AutoSyncEngine::new(
            Box::new(watch_src),
            Arc::new(fake_clock),
            Box::new(FakeSnapshotter {
                root: root_hex.to_string(),
                all_hashes: vec![blob.to_string()],
            }),
            Box::new(FakeBlobUploader::new(&[], recorded.clone())),
            "shared-ws",
            WorkspaceKind::Shared,
            DEBOUNCE_MS,
            false,
        );

        event_queue.lock().unwrap().push(make_event("file.txt"));
        clock_val.store(0, Ordering::Relaxed);
        assert!(engine.tick().is_none(), "no settle before debounce");

        clock_val.store(500, Ordering::Relaxed);
        assert!(engine.tick().is_some(), "shared workspace must settle after debounce");
        assert_eq!(recorded.advanced_refs().len(), 1, "shared workspace must advance ref");
    }
}
