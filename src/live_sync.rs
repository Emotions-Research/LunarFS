// Live-ref reconciliation: NotifyChannel seam, server ref store, client reconciler,
// polling fallback. The ref move is the ONLY change signal.
//
// In-process channel is used by the deterministic gate. Socket channel is used
// only by the LUNAR_SMOKE=1 smoke (never opened in normal test runs).

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ─── Shared types ─────────────────────────────────────────────────────────────

pub type BlobHash = String;
pub type SnapshotId = String;
pub type WorkspaceId = String;

#[derive(Debug, Clone)]
pub struct LiveSnapshot {
    pub id: SnapshotId,
    pub entries: HashMap<String, BlobHash>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RefMoveEvent {
    pub workspace_id: WorkspaceId,
    pub snapshot_id: SnapshotId,
}

#[derive(Debug, Clone)]
pub struct ClientMount {
    pub snapshot_id: Option<SnapshotId>,
    pub entries: HashMap<String, BlobHash>,
}

#[derive(Debug, Clone)]
pub struct ReconcileResult {
    pub snapshot_id: SnapshotId,
    pub fetched_blobs: Vec<BlobHash>,
    pub skipped_blobs: Vec<BlobHash>,
}

// ─── SubscriptionGuard ────────────────────────────────────────────────────────

/// RAII unsubscribe: dropping the guard removes the handler from the channel.
pub struct SubscriptionGuard {
    on_drop: Option<Box<dyn FnOnce() + Send>>,
}

impl SubscriptionGuard {
    pub fn new(f: impl FnOnce() + Send + 'static) -> Self {
        Self { on_drop: Some(Box::new(f)) }
    }
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        if let Some(f) = self.on_drop.take() {
            f();
        }
    }
}

// ─── NotifyChannel trait ──────────────────────────────────────────────────────

/// Transport seam. In-process impl for the gate; socket impl for smoke.
pub trait NotifyChannel: Send + Sync {
    fn subscribe(
        &self,
        workspace_id: &str,
        handler: Box<dyn Fn(RefMoveEvent) + Send + Sync>,
    ) -> SubscriptionGuard;
    fn publish(&self, event: RefMoveEvent);
    fn close(&self);
}

// ─── InProcessNotifyChannel ───────────────────────────────────────────────────

type HandlerFn = Arc<dyn Fn(RefMoveEvent) + Send + Sync>;
type HandlerVec = Vec<(usize, HandlerFn)>;

struct InProcessInner {
    handlers: HashMap<String, HandlerVec>,
    closed: bool,
}

pub struct InProcessNotifyChannel {
    inner: Arc<Mutex<InProcessInner>>,
    next_id: AtomicUsize,
}

impl InProcessNotifyChannel {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(InProcessInner {
                handlers: HashMap::new(),
                closed: false,
            })),
            next_id: AtomicUsize::new(0),
        }
    }
}

impl Default for InProcessNotifyChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl NotifyChannel for InProcessNotifyChannel {
    fn subscribe(
        &self,
        workspace_id: &str,
        handler: Box<dyn Fn(RefMoveEvent) + Send + Sync>,
    ) -> SubscriptionGuard {
        assert!(!workspace_id.is_empty(), "workspace_id must not be empty");
        let sub_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let h: HandlerFn = Arc::from(handler);
        {
            let mut inner = self.inner.lock().expect("InProcessNotifyChannel subscribe lock");
            assert!(!inner.closed, "subscribe on a closed channel");
            inner.handlers.entry(workspace_id.to_string()).or_default().push((sub_id, h));
        }
        let inner_ref = Arc::clone(&self.inner);
        let ws = workspace_id.to_string();
        SubscriptionGuard::new(move || {
            let mut s = inner_ref.lock().expect("SubscriptionGuard drop lock");
            if let Some(subs) = s.handlers.get_mut(&ws) {
                subs.retain(|(id, _)| *id != sub_id);
            }
        })
    }

    fn publish(&self, event: RefMoveEvent) {
        assert!(!event.workspace_id.is_empty(), "publish: workspace_id must not be empty");
        assert!(!event.snapshot_id.is_empty(), "publish: snapshot_id must not be empty");
        // Collect handler Arcs under the lock, then release before calling them.
        // This prevents deadlock if a handler re-enters the channel.
        let handlers: Vec<HandlerFn> = {
            let inner = self.inner.lock().expect("InProcessNotifyChannel publish lock");
            if inner.closed {
                return;
            }
            inner
                .handlers
                .get(&event.workspace_id)
                .map(|subs| subs.iter().map(|(_, h)| Arc::clone(h)).collect())
                .unwrap_or_default()
        };
        for h in handlers {
            h(event.clone());
        }
    }

    fn close(&self) {
        let mut inner = self.inner.lock().expect("InProcessNotifyChannel close lock");
        inner.handlers.clear();
        inner.closed = true;
    }
}

// ─── ServerApi trait ──────────────────────────────────────────────────────────

/// Server-side authoritative surface the client reads from.
pub trait ServerApi: Send + Sync {
    fn get_ref(&self, workspace_id: &str) -> Option<SnapshotId>;
    fn get_snapshot(&self, id: &str) -> Option<LiveSnapshot>;
    fn get_blob(&self, hash: &str) -> Option<Vec<u8>>;
    /// Update the ref and publish a RefMoveEvent through the injected channel.
    fn advance_ref(&self, workspace_id: &str, snapshot_id: &str);
}

// ─── MemServerApi ─────────────────────────────────────────────────────────────

struct MemServerState {
    refs: HashMap<WorkspaceId, SnapshotId>,
    snapshots: HashMap<SnapshotId, LiveSnapshot>,
    blobs: HashMap<BlobHash, Vec<u8>>,
}

pub struct MemServerApi {
    state: Mutex<MemServerState>,
    channel: Arc<dyn NotifyChannel>,
}

impl MemServerApi {
    pub fn new(channel: Arc<dyn NotifyChannel>) -> Self {
        Self {
            state: Mutex::new(MemServerState {
                refs: HashMap::new(),
                snapshots: HashMap::new(),
                blobs: HashMap::new(),
            }),
            channel,
        }
    }

    pub fn seed_snapshot(&self, snap: LiveSnapshot) {
        assert!(!snap.id.is_empty(), "seed_snapshot: id must not be empty");
        let mut st = self.state.lock().expect("MemServerApi seed_snapshot lock");
        st.snapshots.insert(snap.id.clone(), snap);
    }

    pub fn seed_blob(&self, hash: &str, data: Vec<u8>) {
        assert!(!hash.is_empty(), "seed_blob: hash must not be empty");
        let mut st = self.state.lock().expect("MemServerApi seed_blob lock");
        st.blobs.insert(hash.to_string(), data);
    }
}

impl ServerApi for MemServerApi {
    fn get_ref(&self, workspace_id: &str) -> Option<SnapshotId> {
        assert!(!workspace_id.is_empty(), "get_ref: workspace_id must not be empty");
        self.state.lock().expect("MemServerApi get_ref lock").refs.get(workspace_id).cloned()
    }

    fn get_snapshot(&self, id: &str) -> Option<LiveSnapshot> {
        assert!(!id.is_empty(), "get_snapshot: id must not be empty");
        self.state.lock().expect("MemServerApi get_snapshot lock").snapshots.get(id).cloned()
    }

    fn get_blob(&self, hash: &str) -> Option<Vec<u8>> {
        assert!(!hash.is_empty(), "get_blob: hash must not be empty");
        self.state.lock().expect("MemServerApi get_blob lock").blobs.get(hash).cloned()
    }

    fn advance_ref(&self, workspace_id: &str, snapshot_id: &str) {
        assert!(!workspace_id.is_empty(), "advance_ref: workspace_id must not be empty");
        assert!(!snapshot_id.is_empty(), "advance_ref: snapshot_id must not be empty");
        {
            let mut st = self.state.lock().expect("MemServerApi advance_ref lock");
            st.refs.insert(workspace_id.to_string(), snapshot_id.to_string());
        }
        // Publish AFTER releasing the state lock so handlers can call get_ref/get_snapshot.
        self.channel.publish(RefMoveEvent {
            workspace_id: workspace_id.to_string(),
            snapshot_id: snapshot_id.to_string(),
        });
    }
}

// ─── ReconcileError ───────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum ReconcileError {
    SnapshotNotFound(SnapshotId),
    BlobFetchFailed(BlobHash),
}

impl std::fmt::Display for ReconcileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReconcileError::SnapshotNotFound(id) => {
                write!(f, "reconcile: snapshot not found: {}", id)
            }
            ReconcileError::BlobFetchFailed(h) => {
                write!(f, "reconcile: blob fetch failed: {}", h)
            }
        }
    }
}

impl std::error::Error for ReconcileError {}

// ─── ClientReconciler ─────────────────────────────────────────────────────────

struct ClientState {
    mount: ClientMount,
    blob_cache: HashSet<BlobHash>,
}

pub struct ClientReconciler {
    server: Arc<dyn ServerApi>,
    state: Mutex<ClientState>,
}

impl ClientReconciler {
    pub fn new(server: Arc<dyn ServerApi>) -> Self {
        Self {
            server,
            state: Mutex::new(ClientState {
                mount: ClientMount { snapshot_id: None, entries: HashMap::new() },
                blob_cache: HashSet::new(),
            }),
        }
    }

    /// Reconcile to `snapshot_id`. Idempotent: reconciling the current id is a no-op.
    /// Fetches ONLY blobs missing from the local cache. On error, degrades gracefully
    /// (caller logs and falls back to polling) rather than unwinding.
    pub fn reconcile(
        &self,
        workspace_id: &str,
        snapshot_id: &str,
    ) -> Result<ReconcileResult, ReconcileError> {
        assert!(!workspace_id.is_empty(), "reconcile: workspace_id must not be empty");
        assert!(!snapshot_id.is_empty(), "reconcile: snapshot_id must not be empty");

        // Idempotent: already at this snapshot.
        {
            let st = self.state.lock().expect("ClientReconciler reconcile lock (idempotent)");
            if st.mount.snapshot_id.as_deref() == Some(snapshot_id) {
                let skipped: Vec<BlobHash> = st.mount.entries.values().cloned().collect();
                return Ok(ReconcileResult {
                    snapshot_id: snapshot_id.to_string(),
                    fetched_blobs: Vec::new(),
                    skipped_blobs: skipped,
                });
            }
        }

        let snapshot = self.server
            .get_snapshot(snapshot_id)
            .ok_or_else(|| ReconcileError::SnapshotNotFound(snapshot_id.to_string()))?;

        // Partition hashes into missing vs cached (under lock, brief hold).
        let (missing, cached): (Vec<BlobHash>, Vec<BlobHash>) = {
            let st = self.state.lock().expect("ClientReconciler reconcile partition lock");
            let mut m = Vec::new();
            let mut c = Vec::new();
            for hash in snapshot.entries.values() {
                if st.blob_cache.contains(hash) {
                    c.push(hash.clone());
                } else {
                    m.push(hash.clone());
                }
            }
            (m, c)
        };

        // Fetch missing blobs outside the state lock (server calls are safe).
        let mut fetched: Vec<BlobHash> = Vec::new();
        for hash in &missing {
            self.server
                .get_blob(hash)
                .ok_or_else(|| ReconcileError::BlobFetchFailed(hash.clone()))?;
            fetched.push(hash.clone());
        }

        // Commit: add newly fetched blobs to cache, swap mount.
        {
            let mut st = self.state.lock().expect("ClientReconciler reconcile commit lock");
            for hash in &fetched {
                st.blob_cache.insert(hash.clone());
            }
            st.mount.snapshot_id = Some(snapshot_id.to_string());
            st.mount.entries = snapshot.entries;
        }

        Ok(ReconcileResult { snapshot_id: snapshot_id.to_string(), fetched_blobs: fetched, skipped_blobs: cached })
    }

    /// Subscribe `this` to the live channel. Calling `reconcile` on each RefMoveEvent.
    /// Errors inside the handler are logged to stderr and the next event retried.
    pub fn subscribe_live(
        this: Arc<Self>,
        channel: &dyn NotifyChannel,
        workspace_id: &str,
    ) -> SubscriptionGuard {
        assert!(!workspace_id.is_empty(), "subscribe_live: workspace_id must not be empty");
        let ws = workspace_id.to_string();
        channel.subscribe(
            workspace_id,
            Box::new(move |event| {
                if let Err(e) = this.reconcile(&ws, &event.snapshot_id) {
                    eprintln!(
                        "live reconcile error for workspace {}: {}",
                        event.workspace_id, e
                    );
                }
            }),
        )
    }

    pub fn current_mount(&self) -> ClientMount {
        self.state.lock().expect("ClientReconciler current_mount lock").mount.clone()
    }
}

// ─── PollingFallback ──────────────────────────────────────────────────────────

/// Polling-based fallback driver. `tick()` IS the injectable clock seam: tests
/// call it directly; no background timer or wall-clock is needed.
pub struct PollingFallback {
    reconciler: Arc<ClientReconciler>,
    workspace_id: String,
    server: Arc<dyn ServerApi>,
    last_seen: Mutex<Option<SnapshotId>>,
}

impl PollingFallback {
    pub fn new(
        reconciler: Arc<ClientReconciler>,
        server: Arc<dyn ServerApi>,
        workspace_id: impl Into<String>,
    ) -> Self {
        let workspace_id = workspace_id.into();
        assert!(!workspace_id.is_empty(), "PollingFallback: workspace_id must not be empty");
        Self { reconciler, workspace_id, server, last_seen: Mutex::new(None) }
    }

    /// Drive one poll cycle. Returns Some(result) when a reconcile was performed.
    /// On error, logs and returns None (degrades gracefully, retries on next tick).
    pub fn tick(&self) -> Option<ReconcileResult> {
        assert!(!self.workspace_id.is_empty(), "tick: workspace_id must not be empty");
        let current = self.server.get_ref(&self.workspace_id)?;

        let changed = {
            let last = self.last_seen.lock().expect("PollingFallback last_seen lock");
            last.as_deref() != Some(current.as_str())
        };
        if !changed {
            return None;
        }

        match self.reconciler.reconcile(&self.workspace_id, &current) {
            Ok(result) => {
                *self.last_seen.lock().expect("PollingFallback last_seen update") = Some(current);
                Some(result)
            }
            Err(e) => {
                eprintln!("polling fallback error for {}: {}", self.workspace_id, e);
                None
            }
        }
    }
}

// ─── SocketNotifyChannel ──────────────────────────────────────────────────────

/// TCP-backed NotifyChannel. Only constructed in LUNAR_SMOKE=1 tests; never
/// imported or opened during the normal test run.
///
/// Server mode (`::server`): listens for TCP connections; `publish` fans out to
/// all connected clients as newline-delimited JSON.
/// Client mode (`::connect`): connects to a server; `subscribe` registers a handler
/// that fires when the background reader receives a JSON event line.
pub struct SocketNotifyChannel {
    mode: SocketMode,
    handlers: Arc<Mutex<HashMap<String, HandlerVec>>>,
    next_id: AtomicUsize,
    stop: Arc<AtomicBool>,
}

enum SocketMode {
    Server {
        connections: Arc<Mutex<Vec<TcpStream>>>,
        local_addr: std::net::SocketAddr,
    },
    Client,
}

impl SocketNotifyChannel {
    const MAX_CLIENTS: usize = 64;

    /// Start a TCP server on `addr`. The accept loop runs in a background thread.
    pub fn server(addr: &str) -> std::io::Result<Self> {
        assert!(!addr.is_empty(), "server addr must not be empty");
        let listener = TcpListener::bind(addr)?;
        let local_addr = listener.local_addr()?;
        let connections: Arc<Mutex<Vec<TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let conns = Arc::clone(&connections);
        let stop_flag = Arc::clone(&stop);
        std::thread::spawn(move || {
            for _ in 0..Self::MAX_CLIENTS {
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        let mut c = conns.lock().expect("socket server connections lock");
                        if c.len() < Self::MAX_CLIENTS {
                            c.push(stream);
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            mode: SocketMode::Server { connections, local_addr },
            handlers: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicUsize::new(0),
            stop,
        })
    }

    /// Connect to a server at `addr`. A background reader thread dispatches events.
    pub fn connect(addr: &str) -> std::io::Result<Self> {
        assert!(!addr.is_empty(), "connect addr must not be empty");
        let stream = TcpStream::connect(addr)?;
        let handlers: Arc<Mutex<HashMap<String, HandlerVec>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let handlers_ref = Arc::clone(&handlers);
        let stop_flag = Arc::clone(&stop);
        let reader = BufReader::new(stream);

        std::thread::spawn(move || {
            for line in reader.lines() {
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                let event: RefMoveEvent = match serde_json::from_str(&line) {
                    Ok(e) => e,
                    Err(e) => {
                        eprintln!("socket client: bad event JSON: {}", e);
                        continue;
                    }
                };
                let to_call: Vec<HandlerFn> = {
                    let h = handlers_ref.lock().expect("socket client dispatch lock");
                    h.get(&event.workspace_id)
                        .map(|subs| subs.iter().map(|(_, f)| Arc::clone(f)).collect())
                        .unwrap_or_default()
                };
                for f in to_call {
                    f(event.clone());
                }
            }
        });

        Ok(Self {
            mode: SocketMode::Client,
            handlers,
            next_id: AtomicUsize::new(0),
            stop,
        })
    }

    pub fn local_addr(&self) -> Option<std::net::SocketAddr> {
        match &self.mode {
            SocketMode::Server { local_addr, .. } => Some(*local_addr),
            SocketMode::Client => None,
        }
    }
}

impl NotifyChannel for SocketNotifyChannel {
    fn subscribe(
        &self,
        workspace_id: &str,
        handler: Box<dyn Fn(RefMoveEvent) + Send + Sync>,
    ) -> SubscriptionGuard {
        assert!(!workspace_id.is_empty(), "subscribe: workspace_id must not be empty");
        let sub_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let h: HandlerFn = Arc::from(handler);
        {
            let mut hs = self.handlers.lock().expect("SocketNotifyChannel subscribe lock");
            hs.entry(workspace_id.to_string()).or_default().push((sub_id, h));
        }
        let hs_ref = Arc::clone(&self.handlers);
        let ws = workspace_id.to_string();
        SubscriptionGuard::new(move || {
            let mut hs = hs_ref.lock().expect("SocketNotifyChannel unsubscribe lock");
            if let Some(subs) = hs.get_mut(&ws) {
                subs.retain(|(id, _)| *id != sub_id);
            }
        })
    }

    fn publish(&self, event: RefMoveEvent) {
        assert!(!event.workspace_id.is_empty(), "publish: workspace_id must not be empty");
        assert!(!event.snapshot_id.is_empty(), "publish: snapshot_id must not be empty");
        match &self.mode {
            SocketMode::Server { connections, .. } => {
                let mut json =
                    serde_json::to_string(&event).expect("RefMoveEvent must serialize");
                json.push('\n');
                let bytes = json.as_bytes();
                let mut conns = connections.lock().expect("socket server publish lock");
                let mut dead: Vec<usize> = Vec::new();
                for (i, stream) in conns.iter_mut().enumerate() {
                    if stream.write_all(bytes).is_err() {
                        dead.push(i);
                    }
                }
                // Remove dead connections in reverse order to preserve indices.
                for i in dead.into_iter().rev() {
                    conns.remove(i);
                }
            }
            SocketMode::Client => {
                // Client-side publish is a no-op; clients only subscribe.
            }
        }
    }

    fn close(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.handlers.lock().expect("SocketNotifyChannel close lock").clear();
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── CountingServerApi: spy for blob fetch counts ──────────────────────────

    struct CountingServerApi {
        inner: Arc<dyn ServerApi>,
        counts: Mutex<HashMap<BlobHash, usize>>,
    }

    impl CountingServerApi {
        fn new(inner: Arc<dyn ServerApi>) -> Self {
            Self { inner, counts: Mutex::new(HashMap::new()) }
        }

        fn get_blob_count(&self, hash: &str) -> usize {
            *self.counts.lock().expect("CountingServerApi counts lock").get(hash).unwrap_or(&0)
        }
    }

    impl ServerApi for CountingServerApi {
        fn get_ref(&self, workspace_id: &str) -> Option<SnapshotId> {
            self.inner.get_ref(workspace_id)
        }
        fn get_snapshot(&self, id: &str) -> Option<LiveSnapshot> {
            self.inner.get_snapshot(id)
        }
        fn get_blob(&self, hash: &str) -> Option<Vec<u8>> {
            let result = self.inner.get_blob(hash);
            if result.is_some() {
                *self.counts
                    .lock()
                    .expect("CountingServerApi get_blob lock")
                    .entry(hash.to_string())
                    .or_insert(0) += 1;
            }
            result
        }
        fn advance_ref(&self, workspace_id: &str, snapshot_id: &str) {
            self.inner.advance_ref(workspace_id, snapshot_id);
        }
    }

    // ── Helper: build a LiveSnapshot ──────────────────────────────────────────

    fn snap(id: &str, entries: &[(&str, &str)]) -> LiveSnapshot {
        LiveSnapshot {
            id: id.to_string(),
            entries: entries.iter().map(|(p, h)| (p.to_string(), h.to_string())).collect(),
        }
    }

    // ── Test 1: Live path ─────────────────────────────────────────────────────
    //
    // Seed S1 {a.txt->h1, b.txt->h2}. Subscribe client. advance_ref to S1.
    // Assert client reconciled, both blobs fetched.
    // advance_ref to S2 {a.txt->h1, b.txt->h3, c.txt->h4}.
    // Assert fetchedBlobs==[h3,h4], skippedBlobs includes h1, h1 NOT refetched.

    #[test]
    fn live_path_fetches_only_missing_blobs() {
        let channel = Arc::new(InProcessNotifyChannel::new());
        let api = Arc::new(MemServerApi::new(channel.clone() as Arc<dyn NotifyChannel>));

        api.seed_blob("h1", b"content-a".to_vec());
        api.seed_blob("h2", b"content-b".to_vec());
        api.seed_blob("h3", b"content-c".to_vec());
        api.seed_blob("h4", b"content-d".to_vec());

        let s1 = snap("s1", &[("a.txt", "h1"), ("b.txt", "h2")]);
        let s2 = snap("s2", &[("a.txt", "h1"), ("b.txt", "h3"), ("c.txt", "h4")]);
        api.seed_snapshot(s1);
        api.seed_snapshot(s2.clone());

        let spy = Arc::new(CountingServerApi::new(api.clone() as Arc<dyn ServerApi>));
        let reconciler = Arc::new(ClientReconciler::new(spy.clone() as Arc<dyn ServerApi>));

        // Capture results from the live subscription handler.
        let results: Arc<Mutex<Vec<ReconcileResult>>> = Arc::new(Mutex::new(Vec::new()));
        let results_ref = Arc::clone(&results);
        let rec_ref = Arc::clone(&reconciler);
        let _guard = channel.subscribe(
            "ws1",
            Box::new(move |event| {
                if let Ok(r) = rec_ref.reconcile("ws1", &event.snapshot_id) {
                    results_ref.lock().expect("results lock").push(r);
                }
            }),
        );

        // advance_ref to S1; InProcessNotifyChannel is synchronous so reconcile
        // completes before advance_ref returns.
        api.advance_ref("ws1", "s1");

        {
            let rs = results.lock().expect("results lock after s1");
            assert_eq!(rs.len(), 1, "must have one reconcile result for S1");
            let r = &rs[0];
            assert_eq!(r.snapshot_id, "s1");
            let mut fetched = r.fetched_blobs.clone();
            fetched.sort();
            assert_eq!(fetched, vec!["h1", "h2"], "S1 must fetch h1 and h2");
            assert!(r.skipped_blobs.is_empty(), "S1 must skip no blobs");
        }

        assert_eq!(spy.get_blob_count("h1"), 1, "h1 fetched once during S1");
        assert_eq!(spy.get_blob_count("h2"), 1, "h2 fetched once during S1");

        // advance_ref to S2; only h3 and h4 are missing (h1 is cached).
        api.advance_ref("ws1", "s2");

        {
            let rs = results.lock().expect("results lock after s2");
            assert_eq!(rs.len(), 2, "must have two reconcile results total");
            let r = &rs[1];
            assert_eq!(r.snapshot_id, "s2");
            let mut fetched = r.fetched_blobs.clone();
            fetched.sort();
            assert_eq!(fetched, vec!["h3", "h4"], "S2 must fetch only h3 and h4");
            assert!(r.skipped_blobs.contains(&"h1".to_string()), "h1 must be skipped in S2");
        }

        // h1 must NOT have been refetched.
        assert_eq!(spy.get_blob_count("h1"), 1, "h1 must NOT be refetched for S2");
        assert_eq!(spy.get_blob_count("h3"), 1, "h3 fetched once during S2");
        assert_eq!(spy.get_blob_count("h4"), 1, "h4 fetched once during S2");

        // Final mount matches S2 entries.
        let mount = reconciler.current_mount();
        assert_eq!(mount.snapshot_id.as_deref(), Some("s2"));
        assert_eq!(mount.entries, s2.entries);
    }

    // ── Test 2: Polling fallback reaches the identical reconciled state ────────
    //
    // No live subscription. A second client with a PollingFallback. Tick once.
    // Assert it reaches same snapshotId, entries, only-missing fetch behavior.

    #[test]
    fn polling_fallback_reaches_same_state_as_live() {
        let channel = Arc::new(InProcessNotifyChannel::new());
        let api = Arc::new(MemServerApi::new(channel.clone() as Arc<dyn NotifyChannel>));

        api.seed_blob("h1", b"blob-h1".to_vec());
        api.seed_blob("h3", b"blob-h3".to_vec());
        api.seed_blob("h4", b"blob-h4".to_vec());

        let s2 = snap("s2", &[("a.txt", "h1"), ("b.txt", "h3"), ("c.txt", "h4")]);
        api.seed_snapshot(s2.clone());
        api.advance_ref("ws1", "s2");

        let spy2 = Arc::new(CountingServerApi::new(api.clone() as Arc<dyn ServerApi>));
        let poll_reconciler = Arc::new(ClientReconciler::new(spy2.clone() as Arc<dyn ServerApi>));
        let fallback = PollingFallback::new(
            poll_reconciler.clone(),
            api.clone() as Arc<dyn ServerApi>,
            "ws1",
        );

        // First tick: ref has advanced to s2; client must reconcile.
        let result = fallback.tick();
        assert!(result.is_some(), "first tick must reconcile");
        let result = result.unwrap();
        assert_eq!(result.snapshot_id, "s2");

        let mut fetched = result.fetched_blobs.clone();
        fetched.sort();
        assert_eq!(fetched, vec!["h1", "h3", "h4"], "poll must fetch all S2 blobs (cache was empty)");

        // Second tick: same ref, no change, no reconcile.
        assert!(fallback.tick().is_none(), "second tick with same ref must be no-op");

        // Mount matches S2.
        let mount = poll_reconciler.current_mount();
        assert_eq!(mount.snapshot_id.as_deref(), Some("s2"));
        assert_eq!(mount.entries, s2.entries);

        // Each blob fetched exactly once.
        assert_eq!(spy2.get_blob_count("h1"), 1);
        assert_eq!(spy2.get_blob_count("h3"), 1);
        assert_eq!(spy2.get_blob_count("h4"), 1);
    }

    // ── Test 3: Idempotent reconcile (same snapshotId, no re-fetch) ───────────

    #[test]
    fn reconcile_to_same_snapshot_is_noop() {
        let channel = Arc::new(InProcessNotifyChannel::new());
        let api = Arc::new(MemServerApi::new(channel.clone() as Arc<dyn NotifyChannel>));
        api.seed_blob("h1", b"data".to_vec());
        api.seed_snapshot(snap("s1", &[("a.txt", "h1")]));

        let spy = Arc::new(CountingServerApi::new(api.clone() as Arc<dyn ServerApi>));
        let rec = Arc::new(ClientReconciler::new(spy.clone() as Arc<dyn ServerApi>));

        rec.reconcile("ws1", "s1").expect("first reconcile must succeed");
        assert_eq!(spy.get_blob_count("h1"), 1, "h1 fetched once on first reconcile");

        let r = rec.reconcile("ws1", "s1").expect("second reconcile must succeed");
        assert!(r.fetched_blobs.is_empty(), "second reconcile must fetch nothing");
        assert_eq!(spy.get_blob_count("h1"), 1, "h1 must NOT be refetched");
    }

    // ── Test 4: Unsubscribe stops further reconciles ───────────────────────────

    #[test]
    fn unsubscribe_stops_reconciles() {
        let channel = Arc::new(InProcessNotifyChannel::new());
        let api = Arc::new(MemServerApi::new(channel.clone() as Arc<dyn NotifyChannel>));
        api.seed_blob("h1", b"a".to_vec());
        api.seed_blob("h2", b"b".to_vec());
        api.seed_snapshot(snap("s1", &[("a.txt", "h1")]));
        api.seed_snapshot(snap("s2", &[("a.txt", "h2")]));

        let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cc = Arc::clone(&call_count);
        let guard = channel.subscribe(
            "ws1",
            Box::new(move |_| {
                cc.fetch_add(1, Ordering::Relaxed);
            }),
        );

        api.advance_ref("ws1", "s1");
        assert_eq!(call_count.load(Ordering::Relaxed), 1, "handler must fire once");

        drop(guard); // unsubscribe

        api.advance_ref("ws1", "s2");
        assert_eq!(call_count.load(Ordering::Relaxed), 1, "handler must not fire after unsubscribe");
    }

    // ── Test 5: Unknown snapshotId degrades, does not panic ───────────────────

    #[test]
    fn reconcile_unknown_snapshot_returns_error() {
        let channel = Arc::new(InProcessNotifyChannel::new());
        let api = Arc::new(MemServerApi::new(channel.clone() as Arc<dyn NotifyChannel>));
        let rec = ClientReconciler::new(api.clone() as Arc<dyn ServerApi>);
        let err = rec.reconcile("ws1", "nonexistent").unwrap_err();
        assert!(
            matches!(err, ReconcileError::SnapshotNotFound(_)),
            "unknown snapshot must return SnapshotNotFound"
        );
    }

    // ── Test 6: Polling fallback with repeated advance ─────────────────────────

    #[test]
    fn polling_fallback_follows_multiple_advances() {
        let channel = Arc::new(InProcessNotifyChannel::new());
        let api = Arc::new(MemServerApi::new(channel.clone() as Arc<dyn NotifyChannel>));

        api.seed_blob("h1", b"a".to_vec());
        api.seed_blob("h2", b"b".to_vec());
        api.seed_snapshot(snap("s1", &[("a.txt", "h1")]));
        api.seed_snapshot(snap("s2", &[("b.txt", "h2")]));

        let rec = Arc::new(ClientReconciler::new(api.clone() as Arc<dyn ServerApi>));
        let fb = PollingFallback::new(rec.clone(), api.clone() as Arc<dyn ServerApi>, "ws1");

        api.advance_ref("ws1", "s1");
        assert!(fb.tick().is_some(), "must reconcile to s1");
        assert_eq!(rec.current_mount().snapshot_id.as_deref(), Some("s1"));

        api.advance_ref("ws1", "s2");
        assert!(fb.tick().is_some(), "must reconcile to s2");
        assert_eq!(rec.current_mount().snapshot_id.as_deref(), Some("s2"));
    }

    // ── Smoke test: LUNAR_SMOKE=1 only ───────────────────────────────────────
    //
    // Constructs SocketNotifyChannel server + client. Publishes an advance->reconcile
    // round trip over real TCP. Skipped when LUNAR_SMOKE != "1".
    //
    // Run with: LUNAR_SMOKE=1 cargo test socket_smoke -- --nocapture

    #[test]
    fn socket_smoke() {
        if std::env::var("LUNAR_SMOKE").as_deref() != Ok("1") {
            return;
        }

        // Server-side API with a socket channel.
        let server_channel = SocketNotifyChannel::server("127.0.0.1:0")
            .expect("socket server must bind");
        let addr = server_channel.local_addr().expect("server must have local addr");
        let server_channel = Arc::new(server_channel);

        let api = Arc::new(MemServerApi::new(server_channel.clone() as Arc<dyn NotifyChannel>));
        api.seed_blob("h1", b"blob-content".to_vec());
        api.seed_snapshot(snap("ws1", &[("file.txt", "h1")]));
        api.seed_snapshot(LiveSnapshot {
            id: "s1".to_string(),
            entries: [("file.txt".to_string(), "h1".to_string())].into_iter().collect(),
        });

        // Client connects to server.
        let client_channel = SocketNotifyChannel::connect(&addr.to_string())
            .expect("socket client must connect");

        // Allow the accept loop to process the connection.
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Set up reconciler on the client side.
        let client_api = api.clone() as Arc<dyn ServerApi>;
        let reconciler = Arc::new(ClientReconciler::new(client_api));
        let received = Arc::new(AtomicBool::new(false));
        let received_flag = Arc::clone(&received);
        let rec_ref = Arc::clone(&reconciler);

        let _guard = client_channel.subscribe(
            "ws1",
            Box::new(move |event| {
                if rec_ref.reconcile("ws1", &event.snapshot_id).is_ok() {
                    received_flag.store(true, Ordering::Relaxed);
                }
            }),
        );

        // Advance ref; server publishes over TCP.
        api.advance_ref("ws1", "s1");

        // Wait up to 1s for the client to receive and reconcile.
        const MAX_WAIT: usize = 100;
        for _ in 0..MAX_WAIT {
            if received.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(received.load(Ordering::Relaxed), "client must reconcile within 1s over TCP");
        let mount = reconciler.current_mount();
        assert_eq!(mount.snapshot_id.as_deref(), Some("s1"));
        assert!(mount.entries.contains_key("file.txt"));

        server_channel.close();
        client_channel.close();
    }
}
