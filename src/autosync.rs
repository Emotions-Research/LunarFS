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

/// Outcome of a BlobUploader::push call.
pub enum PushOutcome {
    /// The ref was advanced; `uploaded` blobs were transferred.
    Landed { uploaded: usize },
    /// The server rejected the CAS because its root differs from expected_root.
    /// `current_root` is the server's actual root (hex) for the caller to rebase against.
    Conflict { current_root: String },
}

/// Blob upload seam: CAS-aware push and rebase.
pub trait BlobUploader: Send {
    /// Upload missing blobs for `root` and CAS-advance the workspace ref against
    /// `expected_root` (hex, None on first push / unconditional). Returns the outcome
    /// so the engine can handle conflict without a panic or bail.
    fn push(
        &self,
        workspace: &str,
        root: &str,
        expected_root: Option<&str>,
    ) -> anyhow::Result<PushOutcome>;
    /// Re-pull the remote workspace root and all reachable blobs into the local store.
    /// Returns the new remote root hex so the engine can update expected_root.
    fn rebase(&self, workspace: &str) -> anyhow::Result<String>;
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
// Notify-backed WatchSource
// ---------------------------------------------------------------------------

/// Returns true when `path` should be SKIPPED (noise). Checks every path component.
pub fn is_noise_path(path: &std::path::Path) -> bool {
    for component in path.components() {
        let s = component.as_os_str().to_string_lossy();
        if s == ".git" || s == ".lunar" {
            return true;
        }
    }
    let name = match path.file_name() {
        Some(n) => n.to_string_lossy().into_owned(),
        None => return false,
    };
    // editor temp/swap suffixes and socket files
    if name.ends_with(".swp")
        || name.ends_with('~')
        || name.ends_with(".tmp")
        || name.ends_with(".sock")
    {
        return true;
    }
    // vim atomic-rename: basename of all ASCII digits (e.g. 4913)
    if !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    // emacs lock file: .#<name>
    if name.starts_with(".#") {
        return true;
    }
    // emacs autosave: #<name># (at least two chars total so a lone '#' is not matched)
    if name.starts_with('#') && name.ends_with('#') && name.len() > 1 {
        return true;
    }
    false
}

/// notify-backed WatchSource. Owns the live watcher (dropping it stops watching)
/// and drains translated+filtered WriteEvents from an internal mpsc receiver.
pub struct NotifyWatchSource {
    _watcher: notify::RecommendedWatcher,
    rx: std::sync::mpsc::Receiver<WriteEvent>,
}

impl NotifyWatchSource {
    /// Begin recursively watching `root`. Native fs events are filtered for noise
    /// and translated to WriteEvent, then sent through an internal Sender<WriteEvent>.
    pub fn new(root: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        use notify::Watcher;
        let root = root.as_ref();
        assert!(
            root.is_dir(),
            "NotifyWatchSource root must be an existing directory"
        );
        let (tx, rx) = std::sync::mpsc::channel::<WriteEvent>();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let event = match res {
                Ok(e) => e,
                Err(err) => {
                    eprintln!("notify watcher error: {}", err);
                    return;
                }
            };
            let kind = match event.kind {
                notify::EventKind::Create(_) => WriteEventKind::Create,
                notify::EventKind::Modify(notify::event::ModifyKind::Name(_)) => {
                    WriteEventKind::Rename
                }
                notify::EventKind::Modify(_) => WriteEventKind::Write,
                notify::EventKind::Remove(_) => WriteEventKind::Delete,
                _ => return,
            };
            let at_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            for path in event.paths {
                if is_noise_path(&path) {
                    continue;
                }
                let _ = tx.send(WriteEvent {
                    path: path.to_string_lossy().into_owned(),
                    kind,
                    at_ms,
                });
            }
        })
        .map_err(|e| anyhow::anyhow!("notify watcher creation failed: {}", e))?;
        watcher
            .watch(root, notify::RecursiveMode::Recursive)
            .map_err(|e| anyhow::anyhow!("notify watch failed: {}", e))?;
        Ok(Self {
            _watcher: watcher,
            rx,
        })
    }
}

impl WatchSource for NotifyWatchSource {
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
        assert!(
            debounce_ms <= 3_600_000,
            "debounce_ms must not exceed 1 hour"
        );
        Self {
            debounce_ms,
            last_event_ms: None,
            settled: false,
        }
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
    /// Distinct file paths that changed in this sync burst (set by tick; for run_once = uploaded).
    pub changed_files: usize,
    /// Elapsed ms for snapshot + push, measured via the Clock seam.
    pub elapsed_ms: u64,
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
    /// Last root that was successfully landed on the remote (hex). Passed as
    /// expected_root on the next push so a concurrent remote change is detected
    /// rather than clobbered. None = unconditional first push.
    expected_root: Option<String>,
    /// Distinct file paths seen since the last successful settle; cleared after each push.
    pending_paths: HashSet<String>,
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
            expected_root: None,
            pending_paths: HashSet::new(),
        }
    }

    /// Seed the last-known remote root so the first push uses CAS rather than
    /// unconditional overwrite. Call at startup with `remote.get_ref(workspace).ok()`.
    pub fn seed_expected_root(&mut self, root: Option<String>) {
        self.expected_root = root;
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
        for e in &events {
            self.gate.on_event(now);
            self.pending_paths.insert(e.path.clone());
        }
        if !self.gate.check_settle(now) {
            return None;
        }
        let n_changed = self.pending_paths.len();
        match self.pipeline_inner() {
            Ok(mut result) => {
                result.changed_files = n_changed;
                self.gate.reset_after_settle();
                self.pending_paths.clear();
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

    /// Background loop: tick at `poll_ms` intervals until `stop` is set,
    /// calling `on_sync` with each SyncResult produced by a successful settle.
    pub fn run_blocking_with(
        &mut self,
        poll_ms: u64,
        stop: &std::sync::atomic::AtomicBool,
        on_sync: &mut dyn FnMut(&SyncResult),
    ) {
        assert!(poll_ms > 0, "poll_ms must be positive");
        assert!(poll_ms <= 60_000, "poll_ms must not exceed 60 seconds");
        const MAX_TICKS: usize = 10_000_000;
        for _ in 0..MAX_TICKS {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            if let Some(ref r) = self.tick() {
                on_sync(r);
            }
            std::thread::sleep(std::time::Duration::from_millis(poll_ms));
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
        }
    }

    /// Background loop (no callback). Delegates to run_blocking_with with a no-op.
    pub fn run_blocking(&mut self, poll_ms: u64, stop: &std::sync::atomic::AtomicBool) {
        self.run_blocking_with(poll_ms, stop, &mut |_| {});
    }

    /// Perform exactly one snapshot + push without waiting for fs events.
    /// Returns Err when the engine is disabled or has WorkspaceKind::Agent.
    pub fn run_once(&mut self) -> anyhow::Result<SyncResult> {
        assert!(!self.workspace.is_empty(), "workspace must not be empty");
        if self.disabled || matches!(self.kind, WorkspaceKind::Agent) {
            anyhow::bail!("sync disabled");
        }
        let mut result = self.pipeline_inner()?;
        // No fs events drove this sync; use uploaded as a proxy for changed files.
        result.changed_files = result.uploaded;
        Ok(result)
    }

    fn pipeline_inner(&mut self) -> anyhow::Result<SyncResult> {
        const MAX_CONFLICT_RETRIES: usize = 3;
        assert!(
            !self.workspace.is_empty(),
            "workspace must not be empty in pipeline"
        );
        let start_ms = self.clock.now_ms();

        for _ in 0..MAX_CONFLICT_RETRIES {
            let (root, _) = self.snapshotter.snapshot()?;
            anyhow::ensure!(!root.is_empty(), "snapshot returned empty root hash");

            match self
                .uploader
                .push(&self.workspace, &root, self.expected_root.as_deref())?
            {
                PushOutcome::Landed { uploaded } => {
                    self.expected_root = Some(root.clone());
                    let elapsed_ms = self.clock.now_ms().saturating_sub(start_ms);
                    // changed_files is set by the caller (tick or run_once).
                    return Ok(SyncResult {
                        root,
                        uploaded,
                        changed_files: 0,
                        elapsed_ms,
                    });
                }
                PushOutcome::Conflict { current_root } => {
                    eprintln!(
                        "autosync[{}]: CAS conflict (local root {}, server current_root {}); re-basing",
                        self.workspace, root, current_root
                    );
                    let new_remote_root = self.uploader.rebase(&self.workspace)?;
                    self.expected_root = Some(new_remote_root);
                }
            }
        }

        eprintln!(
            "autosync[{}]: CAS conflict persists after {} retries; will retry on next settle",
            self.workspace, MAX_CONFLICT_RETRIES
        );
        anyhow::bail!("CAS conflict: {} retries exhausted", MAX_CONFLICT_RETRIES)
    }
}

/// Format a concise status line for a completed sync.
///
/// Example: `synced 1a2b3c4d5e6f  3 files  812ms`
pub fn format_sync_status(result: &SyncResult) -> String {
    assert!(!result.root.is_empty(), "SyncResult root must not be empty");
    let short_root: String = result.root.chars().take(12).collect();
    let file_word = if result.changed_files == 1 {
        "file"
    } else {
        "files"
    };
    format!(
        "synced {}  {} {}  {}ms",
        short_root, result.changed_files, file_word, result.elapsed_ms
    )
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
        assert!(
            mount_path.is_dir(),
            "mount_path must be an existing directory"
        );
        Self { store, mount_path }
    }
}

impl Snapshotter for WalkSnapshotter {
    fn snapshot(&self) -> anyhow::Result<(String, Vec<String>)> {
        assert!(
            self.mount_path.is_dir(),
            "mount_path must exist at snapshot time"
        );
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
    pub fn new(remote: crate::remote::HttpRemote, store: Arc<dyn Store>) -> anyhow::Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| anyhow::anyhow!("tokio runtime: {}", e))?;
        Ok(Self { remote, store, rt })
    }
}

impl BlobUploader for HttpBlobUploader {
    fn push(
        &self,
        workspace: &str,
        root: &str,
        expected_root: Option<&str>,
    ) -> anyhow::Result<PushOutcome> {
        assert!(!workspace.is_empty(), "workspace must not be empty");
        assert!(!root.is_empty(), "root must not be empty");
        let root_hash = crate::cas::hex_to_hash(root)
            .map_err(|e| anyhow::anyhow!("bad root hash {}: {}", root, e))?;
        let exp_hash: Option<Hash> = expected_root
            .map(|h| {
                crate::cas::hex_to_hash(h)
                    .map_err(|e| anyhow::anyhow!("bad expected_root {}: {}", h, e))
            })
            .transpose()?;
        let result = self.rt.block_on(crate::sync::push_cas(
            &*self.store,
            &root_hash,
            &self.remote,
            workspace,
            exp_hash.as_ref(),
        ))?;
        match result.outcome {
            crate::remote::CasRefOutcome::Committed => Ok(PushOutcome::Landed {
                uploaded: result.uploaded,
            }),
            crate::remote::CasRefOutcome::Conflict { current_root, .. } => {
                Ok(PushOutcome::Conflict {
                    current_root: hash_to_hex(&current_root),
                })
            }
        }
    }

    fn rebase(&self, workspace: &str) -> anyhow::Result<String> {
        assert!(!workspace.is_empty(), "workspace must not be empty");
        let root = self
            .rt
            .block_on(crate::sync::pull(&self.remote, workspace, &*self.store))?;
        Ok(hash_to_hex(&root))
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
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    // -- Fake WatchSource --

    struct FakeWatchSource {
        queue: Arc<Mutex<Vec<WriteEvent>>>,
    }

    impl FakeWatchSource {
        fn new() -> (Self, Arc<Mutex<Vec<WriteEvent>>>) {
            let q = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    queue: Arc::clone(&q),
                },
                q,
            )
        }
    }

    impl WatchSource for FakeWatchSource {
        fn drain(&mut self) -> Vec<WriteEvent> {
            let mut q = self
                .queue
                .lock()
                .expect("FakeWatchSource queue lock poisoned");
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
            assert!(
                !self.root.is_empty(),
                "FakeSnapshotter root must not be empty"
            );
            Ok((self.root.clone(), self.all_hashes.clone()))
        }
    }

    // -- Fake BlobUploader --
    // Records push(workspace, root, expected_root) and rebase(workspace) calls.
    // Outcomes are consumed in FIFO order; default when queue is empty: Landed { uploaded: 0 }.

    #[derive(Clone)]
    struct FakeUploaderHandle {
        push_calls: Arc<Mutex<Vec<(String, String, Option<String>)>>>,
        rebase_calls: Arc<Mutex<Vec<String>>>,
    }

    impl FakeUploaderHandle {
        fn new() -> Self {
            Self {
                push_calls: Arc::new(Mutex::new(Vec::new())),
                rebase_calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn push_calls(&self) -> Vec<(String, String, Option<String>)> {
            self.push_calls
                .lock()
                .expect("push_calls lock poisoned")
                .clone()
        }

        fn rebase_calls(&self) -> Vec<String> {
            self.rebase_calls
                .lock()
                .expect("rebase_calls lock poisoned")
                .clone()
        }
    }

    struct FakeBlobUploader {
        outcomes: Arc<Mutex<std::collections::VecDeque<PushOutcome>>>,
        rebase_root: String,
        handle: FakeUploaderHandle,
    }

    impl FakeBlobUploader {
        fn new(outcomes: Vec<PushOutcome>, rebase_root: &str, handle: FakeUploaderHandle) -> Self {
            Self {
                outcomes: Arc::new(Mutex::new(outcomes.into())),
                rebase_root: rebase_root.to_string(),
                handle,
            }
        }

        fn always_land(uploaded: usize, handle: FakeUploaderHandle) -> Self {
            Self::new(vec![PushOutcome::Landed { uploaded }], "unused", handle)
        }
    }

    impl BlobUploader for FakeBlobUploader {
        fn push(
            &self,
            workspace: &str,
            root: &str,
            expected_root: Option<&str>,
        ) -> anyhow::Result<PushOutcome> {
            assert!(!workspace.is_empty(), "workspace must not be empty");
            assert!(!root.is_empty(), "root must not be empty");
            self.handle
                .push_calls
                .lock()
                .expect("push_calls lock poisoned")
                .push((
                    workspace.to_string(),
                    root.to_string(),
                    expected_root.map(|s| s.to_string()),
                ));
            let outcome = self
                .outcomes
                .lock()
                .expect("outcomes lock poisoned")
                .pop_front()
                .unwrap_or(PushOutcome::Landed { uploaded: 0 });
            Ok(outcome)
        }

        fn rebase(&self, workspace: &str) -> anyhow::Result<String> {
            assert!(!workspace.is_empty(), "workspace must not be empty");
            self.handle
                .rebase_calls
                .lock()
                .expect("rebase_calls lock poisoned")
                .push(workspace.to_string());
            Ok(self.rebase_root.clone())
        }
    }

    fn make_event(path: &str) -> WriteEvent {
        WriteEvent {
            path: path.to_string(),
            kind: WriteEventKind::Write,
            at_ms: 0,
        }
    }

    const DEBOUNCE_MS: u64 = 500;

    // -----------------------------------------------------------------------
    // Test 1: human workspace -- burst collapses to one settle, push called
    //         exactly once with correct workspace and root.
    // -----------------------------------------------------------------------
    #[test]
    fn human_workspace_debounce_and_dedup() {
        let root_hex = "cccc000000000000000000000000000000000000000000000000000000000003";

        let (watch_src, event_queue) = FakeWatchSource::new();
        let (fake_clock, clock_val) = FakeClock::new(0);
        let handle = FakeUploaderHandle::new();

        let mut engine = AutoSyncEngine::new(
            Box::new(watch_src),
            Arc::new(fake_clock),
            Box::new(FakeSnapshotter {
                root: root_hex.to_string(),
                all_hashes: vec![],
            }),
            Box::new(FakeBlobUploader::always_land(1, handle.clone())),
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
        assert!(handle.push_calls().is_empty(), "no push calls at T=0");

        // Tick at T=499: still within debounce window.
        clock_val.store(499, Ordering::Relaxed);
        assert!(engine.tick().is_none(), "no settle at T=499 (< 500ms)");

        // Re-arm with one more event at T=499.
        event_queue.lock().unwrap().push(make_event("d.txt"));
        assert!(
            engine.tick().is_none(),
            "no settle immediately after re-arm"
        );

        // Tick at T=1000: > 500ms since last event at T=499.
        clock_val.store(1000, Ordering::Relaxed);
        let result = engine.tick();
        assert!(result.is_some(), "settle must fire at T=1000");

        // Exactly one push call with the correct workspace and root.
        let calls = handle.push_calls();
        assert_eq!(calls.len(), 1, "exactly one push call per burst");
        assert_eq!(
            calls[0].0, "my-ws",
            "push workspace must match engine workspace"
        );
        assert_eq!(calls[0].1, root_hex, "push root must match snapshot root");
        assert_eq!(calls[0].2, None, "expected_root must be None on first push");

        let sync_result = result.unwrap();
        assert_eq!(
            sync_result.root, root_hex,
            "SyncResult root must match snapshot root"
        );
        assert_eq!(
            sync_result.uploaded, 1,
            "SyncResult uploaded comes from PushOutcome"
        );
        assert_eq!(sync_result.changed_files, 4, "4 distinct paths: a, b, c, d");
        assert_eq!(
            sync_result.elapsed_ms, 0,
            "FakeClock not advanced during pipeline"
        );

        // Second tick at same time: no duplicate settle (gate reset after pipeline).
        assert!(
            engine.tick().is_none(),
            "no second settle without new events"
        );
        assert_eq!(
            handle.push_calls().len(),
            1,
            "no second push without new events"
        );
    }

    // -----------------------------------------------------------------------
    // Test 2: agent workspace -- NEVER calls push or rebase, even after the
    //         debounce window elapses.
    // -----------------------------------------------------------------------
    #[test]
    fn agent_workspace_never_advances() {
        let root_hex = "cccc000000000000000000000000000000000000000000000000000000000003";

        let (watch_src, event_queue) = FakeWatchSource::new();
        let (fake_clock, clock_val) = FakeClock::new(0);
        let handle = FakeUploaderHandle::new();

        let mut engine = AutoSyncEngine::new(
            Box::new(watch_src),
            Arc::new(fake_clock),
            Box::new(FakeSnapshotter {
                root: root_hex.to_string(),
                all_hashes: vec![],
            }),
            Box::new(FakeBlobUploader::new(vec![], "unused", handle.clone())),
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
        assert!(
            handle.push_calls().is_empty(),
            "agent must produce zero push calls"
        );
        assert!(
            handle.rebase_calls().is_empty(),
            "agent must produce zero rebase calls"
        );
    }

    // -----------------------------------------------------------------------
    // Test 3: debounce gate fires exactly once per burst, resets cleanly.
    // -----------------------------------------------------------------------
    #[test]
    fn debounce_gate_fires_once_per_burst() {
        let mut gate = DebounceGate::new(500);

        gate.on_event(0);
        assert!(
            !gate.check_settle(0),
            "must not settle at same time as event"
        );
        assert!(
            !gate.check_settle(499),
            "must not settle before debounce_ms"
        );
        assert!(
            gate.check_settle(500),
            "must settle exactly at debounce boundary"
        );
        assert!(
            !gate.check_settle(1000),
            "must not settle twice without a new event"
        );

        gate.reset_after_settle();
        assert!(
            !gate.check_settle(1000),
            "after reset, must not settle without a new event"
        );

        gate.on_event(1000);
        assert!(
            !gate.check_settle(1499),
            "new burst: must not settle before debounce"
        );
        assert!(
            gate.check_settle(1500),
            "new burst: must settle at debounce boundary"
        );
    }

    // -----------------------------------------------------------------------
    // Test 4: disabled engine is fully inert for human workspaces.
    // -----------------------------------------------------------------------
    #[test]
    fn disabled_engine_is_inert() {
        let root_hex = "cccc000000000000000000000000000000000000000000000000000000000003";
        let (watch_src, event_queue) = FakeWatchSource::new();
        let (fake_clock, clock_val) = FakeClock::new(0);
        let handle = FakeUploaderHandle::new();

        let mut engine = AutoSyncEngine::new(
            Box::new(watch_src),
            Arc::new(fake_clock),
            Box::new(FakeSnapshotter {
                root: root_hex.to_string(),
                all_hashes: vec![],
            }),
            Box::new(FakeBlobUploader::new(vec![], "unused", handle.clone())),
            "ws",
            WorkspaceKind::Human,
            500,
            true, // disabled
        );

        event_queue.lock().unwrap().push(make_event("file.txt"));
        clock_val.store(1000, Ordering::Relaxed);

        assert!(engine.tick().is_none(), "disabled engine must not tick");
        assert!(
            handle.push_calls().is_empty(),
            "disabled engine must not call push"
        );
        assert!(
            handle.rebase_calls().is_empty(),
            "disabled engine must not call rebase"
        );
    }

    // -----------------------------------------------------------------------
    // Test 5: shared workspace behaves identically to human.
    // -----------------------------------------------------------------------
    #[test]
    fn shared_workspace_auto_syncs() {
        let root_hex = "dddd000000000000000000000000000000000000000000000000000000000004";
        let (watch_src, event_queue) = FakeWatchSource::new();
        let (fake_clock, clock_val) = FakeClock::new(0);
        let handle = FakeUploaderHandle::new();

        let mut engine = AutoSyncEngine::new(
            Box::new(watch_src),
            Arc::new(fake_clock),
            Box::new(FakeSnapshotter {
                root: root_hex.to_string(),
                all_hashes: vec![],
            }),
            Box::new(FakeBlobUploader::always_land(0, handle.clone())),
            "shared-ws",
            WorkspaceKind::Shared,
            DEBOUNCE_MS,
            false,
        );

        event_queue.lock().unwrap().push(make_event("file.txt"));
        clock_val.store(0, Ordering::Relaxed);
        assert!(engine.tick().is_none(), "no settle before debounce");

        clock_val.store(500, Ordering::Relaxed);
        assert!(
            engine.tick().is_some(),
            "shared workspace must settle after debounce"
        );
        assert_eq!(
            handle.push_calls().len(),
            1,
            "shared workspace must call push exactly once"
        );
    }

    // -----------------------------------------------------------------------
    // Test 6: conflict triggers rebase then landing on the second attempt.
    // Verifies: (a) rebase called once, (b) second push receives expected_root
    // from the rebase result, (c) engine expected_root ends at the landed root,
    // (d) no unconditional clobber path is exercised (put_ref not in the trait).
    // -----------------------------------------------------------------------
    #[test]
    fn conflict_triggers_rebase_then_lands() {
        let remote_b = "bbbb000000000000000000000000000000000000000000000000000000000002";
        let root_hex = "cccc000000000000000000000000000000000000000000000000000000000003";

        let (watch_src, event_queue) = FakeWatchSource::new();
        let (fake_clock, clock_val) = FakeClock::new(0);
        let handle = FakeUploaderHandle::new();

        let outcomes = vec![
            PushOutcome::Conflict {
                current_root: remote_b.to_string(),
            },
            PushOutcome::Landed { uploaded: 2 },
        ];

        let mut engine = AutoSyncEngine::new(
            Box::new(watch_src),
            Arc::new(fake_clock),
            Box::new(FakeSnapshotter {
                root: root_hex.to_string(),
                all_hashes: vec![],
            }),
            Box::new(FakeBlobUploader::new(outcomes, remote_b, handle.clone())),
            "my-ws",
            WorkspaceKind::Human,
            DEBOUNCE_MS,
            false,
        );

        // Arm the gate.
        event_queue.lock().unwrap().push(make_event("x.txt"));
        clock_val.store(0, Ordering::Relaxed);
        assert!(engine.tick().is_none(), "no settle at T=0");

        // Settle: first push returns Conflict, engine rebases, second push lands.
        clock_val.store(1000, Ordering::Relaxed);
        let result = engine.tick();
        assert!(result.is_some(), "must settle after debounce");

        let sync_result = result.unwrap();
        assert_eq!(
            sync_result.root, root_hex,
            "SyncResult root must be the snapshot root"
        );
        assert_eq!(
            sync_result.uploaded, 2,
            "uploaded comes from the landing PushOutcome"
        );
        assert_eq!(sync_result.changed_files, 1, "one distinct path: x.txt");
        assert_eq!(
            sync_result.elapsed_ms, 0,
            "FakeClock not advanced during pipeline"
        );

        let push_calls = handle.push_calls();
        assert_eq!(
            push_calls.len(),
            2,
            "push must be called twice (initial + retry after rebase)"
        );

        // (a) rebase called exactly once with the correct workspace.
        let rebase_calls = handle.rebase_calls();
        assert_eq!(rebase_calls.len(), 1, "rebase must be called exactly once");
        assert_eq!(
            rebase_calls[0], "my-ws",
            "rebase must receive the workspace name"
        );

        // (b) second push received expected_root equal to the rebase result.
        assert_eq!(
            push_calls[0].2, None,
            "first push must have expected_root == None (engine starts blank)"
        );
        assert_eq!(
            push_calls[1].2,
            Some(remote_b.to_string()),
            "second push must have expected_root from rebase result"
        );

        // (c) engine's expected_root ends at the landed snapshot root.
        assert_eq!(
            engine.expected_root,
            Some(root_hex.to_string()),
            "engine expected_root must end at the landed snapshot root"
        );
    }

    // -----------------------------------------------------------------------
    // is_noise_path filter tests (pure, no fs access)
    // -----------------------------------------------------------------------
    #[test]
    fn is_noise_path_skips_git_components() {
        assert!(is_noise_path(std::path::Path::new(".git/HEAD")));
        assert!(is_noise_path(std::path::Path::new("sub/.git/config")));
    }

    #[test]
    fn is_noise_path_skips_lunar_component() {
        assert!(is_noise_path(std::path::Path::new(".lunar/control")));
    }

    #[test]
    fn is_noise_path_skips_editor_artifacts() {
        assert!(is_noise_path(std::path::Path::new("foo.swp")));
        assert!(is_noise_path(std::path::Path::new("bar~")));
        assert!(is_noise_path(std::path::Path::new("baz.tmp")));
        assert!(is_noise_path(std::path::Path::new("4913")));
        assert!(is_noise_path(std::path::Path::new(".#lock")));
        assert!(is_noise_path(std::path::Path::new("#auto#")));
        assert!(is_noise_path(std::path::Path::new("ctl.sock")));
    }

    #[test]
    fn is_noise_path_passes_normal_files() {
        assert!(!is_noise_path(std::path::Path::new("a.txt")));
        assert!(!is_noise_path(std::path::Path::new("dir/b.rs")));
        assert!(!is_noise_path(std::path::Path::new("src/main.rs")));
        // ".git" as a substring of a filename is not a path component
        assert!(!is_noise_path(std::path::Path::new("notes.git.txt")));
    }

    // -----------------------------------------------------------------------
    // NotifyWatchSource real-fs smoke test
    // Note: if FSEvents batching makes this flaky in CI, add #[ignore] here.
    // -----------------------------------------------------------------------
    #[test]
    fn notify_watch_source_smoke_real_fs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut src = NotifyWatchSource::new(dir.path()).expect("NotifyWatchSource::new");

        // Let the backend register the watch before writing
        std::thread::sleep(std::time::Duration::from_millis(50));

        let file_path = dir.path().join("hello.txt");
        std::fs::write(&file_path, b"hello world").expect("write normal file");

        // Poll up to 50 * 20 ms = 1 s for the event
        let mut found_hello = false;
        for _ in 0..50usize {
            let evs = src.drain();
            if evs.iter().any(|e| {
                e.path.ends_with("hello.txt")
                    && matches!(e.kind, WriteEventKind::Create | WriteEventKind::Write)
            }) {
                found_hello = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            found_hello,
            "expected Create or Write event for hello.txt within ~1 s"
        );

        // Write noise paths; verify the filter blocks them before they reach drain
        let git_dir = dir.path().join(".git");
        std::fs::create_dir_all(&git_dir).ok();
        std::fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main").ok();
        std::fs::write(dir.path().join("foo.swp"), b"swap data").ok();

        // Allow time for any noise events to arrive
        for _ in 0..15usize {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let mut noise_leaked = false;
        for _ in 0..10usize {
            let evs = src.drain();
            for e in &evs {
                if is_noise_path(std::path::Path::new(&e.path)) {
                    noise_leaked = true;
                }
            }
            if noise_leaked {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            !noise_leaked,
            "noise paths must not leak through into drain output"
        );
    }

    // -----------------------------------------------------------------------
    // Test (a): format_sync_status renders short hash, count, and elapsed ms.
    // -----------------------------------------------------------------------
    #[test]
    fn format_sync_status_renders_correctly() {
        let result = SyncResult {
            root: "1a2b3c4d5e6f7890abcd1234567890123456789012345678901234567890abcd".to_string(),
            uploaded: 5,
            changed_files: 3,
            elapsed_ms: 812,
        };
        let s = format_sync_status(&result);
        assert!(
            s.contains("1a2b3c4d5e6f"),
            "must contain 12-char short hash"
        );
        assert!(s.contains("3"), "must contain changed-file count");
        assert!(s.contains("812ms"), "must contain elapsed ms");
        // Must NOT contain more than 12 chars of the root hash in one run.
        assert!(
            !s.contains("1a2b3c4d5e6f78"),
            "must not exceed 12-char hash prefix"
        );
    }

    // -----------------------------------------------------------------------
    // Test (b): run_once on a Human engine returns Ok; changed_files == uploaded.
    // -----------------------------------------------------------------------
    #[test]
    fn run_once_human_returns_ok_changed_files_eq_uploaded() {
        let root_hex = "aaaa000000000000000000000000000000000000000000000000000000000001";
        let handle = FakeUploaderHandle::new();
        let (watch_src, _) = FakeWatchSource::new();
        let (fake_clock, _) = FakeClock::new(0);

        let mut engine = AutoSyncEngine::new(
            Box::new(watch_src),
            Arc::new(fake_clock),
            Box::new(FakeSnapshotter {
                root: root_hex.to_string(),
                all_hashes: vec![],
            }),
            Box::new(FakeBlobUploader::always_land(2, handle.clone())),
            "test-ws",
            WorkspaceKind::Human,
            500,
            false,
        );

        let result = engine
            .run_once()
            .expect("run_once must succeed on Human engine");
        assert_eq!(result.uploaded, 2, "uploaded from PushOutcome");
        assert_eq!(
            result.changed_files, result.uploaded,
            "changed_files == uploaded for run_once"
        );
        assert_eq!(result.root, root_hex, "root matches snapshot");
    }

    // -----------------------------------------------------------------------
    // Test (c): run_once on a disabled engine returns Err.
    // -----------------------------------------------------------------------
    #[test]
    fn run_once_disabled_returns_err() {
        let handle = FakeUploaderHandle::new();
        let (watch_src, _) = FakeWatchSource::new();
        let (fake_clock, _) = FakeClock::new(0);

        let mut engine = AutoSyncEngine::new(
            Box::new(watch_src),
            Arc::new(fake_clock),
            Box::new(FakeSnapshotter {
                root: "aaaa000000000000000000000000000000000000000000000000000000000001"
                    .to_string(),
                all_hashes: vec![],
            }),
            Box::new(FakeBlobUploader::always_land(1, handle)),
            "test-ws",
            WorkspaceKind::Human,
            500,
            true, // disabled
        );

        assert!(
            engine.run_once().is_err(),
            "run_once on disabled engine must return Err"
        );
    }

    // -----------------------------------------------------------------------
    // Test (d): changed_files counts DISTINCT paths across a burst.
    //           a.txt + a.txt + b.txt -> changed_files == 2.
    // -----------------------------------------------------------------------
    #[test]
    fn changed_files_counts_distinct_paths_in_burst() {
        let root_hex = "eeee000000000000000000000000000000000000000000000000000000000005";
        let (watch_src, event_queue) = FakeWatchSource::new();
        let (fake_clock, clock_val) = FakeClock::new(0);
        let handle = FakeUploaderHandle::new();

        let mut engine = AutoSyncEngine::new(
            Box::new(watch_src),
            Arc::new(fake_clock),
            Box::new(FakeSnapshotter {
                root: root_hex.to_string(),
                all_hashes: vec![],
            }),
            Box::new(FakeBlobUploader::always_land(1, handle)),
            "ws",
            WorkspaceKind::Human,
            DEBOUNCE_MS,
            false,
        );

        // Two events for a.txt, one for b.txt -> 2 distinct paths.
        event_queue.lock().unwrap().push(make_event("a.txt"));
        event_queue.lock().unwrap().push(make_event("a.txt"));
        event_queue.lock().unwrap().push(make_event("b.txt"));

        clock_val.store(0, Ordering::Relaxed);
        assert!(engine.tick().is_none(), "no settle at T=0");

        clock_val.store(1000, Ordering::Relaxed);
        let result = engine.tick().expect("must settle");
        assert_eq!(
            result.changed_files, 2,
            "a.txt + b.txt = 2 distinct paths despite 3 events"
        );
    }

    // -----------------------------------------------------------------------
    // Test (e): run_blocking_with calls the callback exactly once per settle.
    // -----------------------------------------------------------------------
    #[test]
    fn run_blocking_with_callback_fires_once_per_settle() {
        let root_hex = "ffff000000000000000000000000000000000000000000000000000000000006";
        let (watch_src, event_queue) = FakeWatchSource::new();
        let (fake_clock, clock_val) = FakeClock::new(0);
        let handle = FakeUploaderHandle::new();

        let mut engine = AutoSyncEngine::new(
            Box::new(watch_src),
            Arc::new(fake_clock),
            Box::new(FakeSnapshotter {
                root: root_hex.to_string(),
                all_hashes: vec![],
            }),
            Box::new(FakeBlobUploader::always_land(1, handle)),
            "ws",
            WorkspaceKind::Human,
            DEBOUNCE_MS,
            false,
        );

        // Arm the gate at T=0 without settling.
        event_queue.lock().unwrap().push(make_event("f.txt"));
        assert!(engine.tick().is_none(), "no settle at T=0");

        // Advance clock so the first tick inside run_blocking_with will settle.
        clock_val.store(1000, Ordering::Relaxed);

        let stop = AtomicBool::new(false);
        let mut cb_count = 0usize;

        std::thread::scope(|s| {
            s.spawn(|| {
                // Let the loop run a handful of ticks then stop it.
                std::thread::sleep(std::time::Duration::from_millis(50));
                stop.store(true, Ordering::Relaxed);
            });
            engine.run_blocking_with(1, &stop, &mut |_| cb_count += 1);
        });

        assert_eq!(cb_count, 1, "callback must fire exactly once for one burst");
    }
}
