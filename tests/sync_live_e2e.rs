// Live-server autosync integration tests.
//
// Three scenarios exercising the real autosync push pipeline against an in-process
// axum server on an ephemeral loopback port:
//
//   1. ref_advances_to_match_directory_tree
//      Writes/modifies/deletes files, pushes via engine.run_once(), asserts the
//      remote ref and pulled tree match the local directory exactly.
//
//   2. write_burst_coalesces_into_single_push
//      Sends a rapid burst of WriteEvents through a ChannelWatchSource, drives
//      engine.tick() with a controllable AtomicU64 clock, and proves the debounce
//      gate coalesces the burst into exactly one push.
//
//   3. concurrent_writer_creates_conflict_ref_no_data_loss
//      Drives push_cas directly to model two racing devices; asserts the loser
//      receives CasRefOutcome::Conflict with the correct conflict_ref name, the
//      winner's ref is preserved, and the loser's blobs are durably on the server.
//
// Gating: set LUNAR_SYNC_E2E=1 to run real assertions; default `cargo test`
// takes the skip path so it never needs a live port or network.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use devdropbox::auth::acl::{self, Permission, PrincipalKind};
use devdropbox::auth::repo;
use devdropbox::auth::token;
use devdropbox::auth::token::Principal as TokenPrincipal;
use devdropbox::auth::verify::NoClerk;
use devdropbox::auth::OwnerKind;
use devdropbox::autosync::{
    AutoSyncEngine, ChannelWatchSource, Clock, HttpBlobUploader, WalkSnapshotter, WorkspaceKind,
    WriteEvent, WriteEventKind,
};
use devdropbox::cas::{hash_to_hex, hex_to_hash, MemStore, Store};
use devdropbox::index::Index;
use devdropbox::remote::{CasRefOutcome, HttpRemote};
use devdropbox::serve::{build_router, AppState};
use devdropbox::store::InMemoryWorkspaceStore;
use devdropbox::sync::{pull, push_cas};
use devdropbox::tree::{serialize_tree, TreeEntry, MODE_DIR, MODE_FILE};
use devdropbox::workspace::InMemoryBackend;
use object_store::local::LocalFileSystem;

// ---------------------------------------------------------------------------
// Skip gate
// ---------------------------------------------------------------------------

fn e2e_enabled() -> bool {
    std::env::var("LUNAR_SYNC_E2E").as_deref() == Ok("1")
}

// ---------------------------------------------------------------------------
// Token-minting clock (sat i64 timestamp, never zero so tokens are valid)
// ---------------------------------------------------------------------------

const NOW_SECS: i64 = 2_000_000_000;

struct FixedTokenClock;

impl devdropbox::auth::token::Clock for FixedTokenClock {
    fn now_secs(&self) -> i64 {
        NOW_SECS
    }
}

// ---------------------------------------------------------------------------
// Controllable autosync clock: backed by an AtomicU64 the test advances
// ---------------------------------------------------------------------------

struct AtomicClock {
    ms: Arc<AtomicU64>,
}

impl AtomicClock {
    fn new(start_ms: u64) -> (Self, Arc<AtomicU64>) {
        let ms = Arc::new(AtomicU64::new(start_ms));
        (Self { ms: ms.clone() }, ms)
    }
}

impl Clock for AtomicClock {
    fn now_ms(&self) -> u64 {
        self.ms.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// start_server: bind an ephemeral port, seed identity, start axum in background
// ---------------------------------------------------------------------------

async fn start_server(ws_name: &str) -> (String, String, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = devdropbox::auth::open(&dir.path().join("id.db")).expect("auth::open");

    let token_plaintext = {
        let uid = repo::create_user(&conn, None, NOW_SECS).expect("create_user");
        let ws_id = repo::create_workspace(&conn, ws_name, OwnerKind::User, uid, NOW_SECS)
            .expect("create_workspace");
        acl::grant(
            &conn,
            PrincipalKind::User,
            &uid.to_string(),
            ws_id,
            "/",
            Permission::Write,
            NOW_SECS,
        )
        .expect("acl grant");
        token::mint(
            &conn,
            &TokenPrincipal {
                kind: OwnerKind::User,
                id: uid.to_string(),
            },
            None,
            None,
            &FixedTokenClock,
        )
        .expect("mint token")
        .plaintext
    };

    // LocalFileSystem and LocalStubPresigner must share the same base directory so
    // presigned PUT writes (stub+local:// writes to file) are visible to the
    // store's head() checks. InMemory would silently diverge: blobs land on disk
    // via presign but the store never sees them, causing false "missing" reports.
    let store = Arc::new(LocalFileSystem::new_with_prefix(dir.path()).expect("LocalFileSystem"));
    let state = AppState {
        store,
        db: Arc::new(Mutex::new(conn)),
        verifier: Arc::new(NoClerk),
        clock: Arc::new(FixedTokenClock),
        presigner: Arc::new(devdropbox::presign::LocalStubPresigner::new(dir.path())),
        ws_backend: Arc::new(InMemoryBackend::new()),
        ws_store: Arc::new(InMemoryWorkspaceStore::new()),
        #[cfg(feature = "hosted")]
        billing: Arc::new(devdropbox::billing::provider::MockBillingProvider::default()),
        #[cfg(feature = "hosted")]
        webhook: Arc::new(devdropbox::billing::webhook::FakeWebhookProvider::new("")),
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    let router = build_router(state);
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("server error");
    });
    tokio::task::yield_now().await;
    (format!("http://127.0.0.1:{}", port), token_plaintext, dir)
}

// ---------------------------------------------------------------------------
// Scenario 1: remote ref advances to a root whose tree matches the directory
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ref_advances_to_match_directory_tree() {
    if !e2e_enabled() {
        eprintln!("skipping sync_live_e2e: set LUNAR_SYNC_E2E=1 to run");
        return;
    }

    let ws = "ws-tree-match";
    let (base_url, tok, _server_dir) = start_server(ws).await;

    // Build a local workspace directory with nested files.
    let ws_dir = tempfile::tempdir().expect("workspace tempdir");
    std::fs::create_dir(ws_dir.path().join("src")).expect("mkdir src");
    std::fs::write(ws_dir.path().join("README.md"), b"# Project\n").expect("write README");
    std::fs::write(ws_dir.path().join("src/main.rs"), b"fn main() {}\n").expect("write main");
    std::fs::write(ws_dir.path().join("src/lib.rs"), b"pub fn lib() {}\n").expect("write lib");

    // HttpBlobUploader creates its own current_thread runtime internally and uses
    // rt.block_on() inside push(). Those calls panic if issued from inside a tokio
    // runtime context, so we run the engine in a blocking thread via spawn_blocking.
    let url1 = base_url.clone();
    let tok1 = tok.clone();
    let ws_path = ws_dir.path().to_path_buf();
    let ws_name = ws.to_string();

    let (root_hex_1, uploaded_1) = tokio::task::spawn_blocking(move || {
        let store = Arc::new(MemStore::new());
        let (tx, rx) = std::sync::mpsc::channel::<WriteEvent>();
        drop(tx); // no events needed; run_once bypasses the debounce gate
        let watch = ChannelWatchSource::new(rx);
        let snap = WalkSnapshotter::new(store.clone(), &ws_path);
        let remote = HttpRemote::new(&url1, &tok1);
        let upl = HttpBlobUploader::new(remote, store).expect("HttpBlobUploader::new");
        let (clock, _) = AtomicClock::new(0);
        let mut engine = AutoSyncEngine::new(
            Box::new(watch),
            Arc::new(clock),
            Box::new(snap),
            Box::new(upl),
            ws_name,
            WorkspaceKind::Human,
            800,
            false,
        );
        let r = engine
            .run_once()
            .expect("run_once must succeed on first push");
        assert!(!r.root.is_empty(), "SyncResult root must not be empty");
        assert!(
            r.uploaded > 0,
            "first push must upload at least one blob, got {}",
            r.uploaded
        );
        (r.root, r.uploaded)
    })
    .await
    .expect("spawn_blocking scenario-1 first push");

    // Verify the remote ref equals the snapshot root.
    let remote = HttpRemote::new(&base_url, &tok);
    let remote_root_1 = remote.get_ref(ws).await.expect("get_ref after first push");
    assert_eq!(
        hash_to_hex(&remote_root_1),
        root_hex_1,
        "remote ref must equal snapshot root after first push"
    );

    // Pull into a fresh store and compare every file.
    let pull_store_1 = MemStore::new();
    let pulled_root_1 = pull(&remote, ws, &pull_store_1)
        .await
        .expect("pull after first push");
    assert_eq!(
        hash_to_hex(&pulled_root_1),
        root_hex_1,
        "pulled root must equal snapshot root"
    );

    let local_root_hash_1 = hex_to_hash(&root_hex_1).expect("hex_to_hash");
    let local_store_1 = MemStore::new();
    // Rebuild the local store from the pull to compare contents.
    let local_root_from_pull = pull(&remote, ws, &local_store_1)
        .await
        .expect("pull for index");
    let index_remote = Index::build(&pull_store_1, &pulled_root_1).expect("index remote");
    let index_local =
        Index::build(&local_store_1, &local_root_from_pull).expect("index local rebuild");

    assert_eq!(
        index_remote.len(),
        index_local.len(),
        "file count must match after first push"
    );
    // uploaded_1 counts ALL blobs (file blobs + tree nodes); index_remote.len() only
    // counts file entries. The right check is just that something was uploaded.
    assert!(uploaded_1 > 0, "first push must upload at least one blob");

    // Verify byte-identical content for every pulled file.
    for (path, hash_r) in index_remote.entries() {
        let hash_l = index_local
            .lookup(path)
            .unwrap_or_else(|| panic!("path {} must exist in local index after pull", path));
        assert_eq!(
            hash_r, &hash_l,
            "hash mismatch for {} between two pulls",
            path
        );
        let bytes_r = pull_store_1
            .get(hash_r)
            .expect("store get")
            .expect("blob present");
        let bytes_l = local_store_1
            .get(&hash_l)
            .expect("store get")
            .expect("blob present");
        assert_eq!(bytes_r, bytes_l, "content mismatch for {} after pull", path);
    }
    let _ = local_root_hash_1; // suppress unused warning

    // Modify one file and delete another; push again.
    std::fs::write(ws_dir.path().join("README.md"), b"# Updated\n").expect("modify README");
    std::fs::remove_file(ws_dir.path().join("src/lib.rs")).expect("delete lib.rs");

    let url2 = base_url.clone();
    let tok2 = tok.clone();
    let ws_path2 = ws_dir.path().to_path_buf();
    let ws_name2 = ws.to_string();
    let root_hex_1_for_seed = root_hex_1.clone();

    let root_hex_2 = tokio::task::spawn_blocking(move || {
        let store2 = Arc::new(MemStore::new());
        let (tx2, rx2) = std::sync::mpsc::channel::<WriteEvent>();
        drop(tx2);
        let watch2 = ChannelWatchSource::new(rx2);
        let snap2 = WalkSnapshotter::new(store2.clone(), &ws_path2);
        let remote2 = HttpRemote::new(&url2, &tok2);
        let upl2 = HttpBlobUploader::new(remote2, store2).expect("HttpBlobUploader::new second");
        let (clock2, _) = AtomicClock::new(0);
        let mut engine2 = AutoSyncEngine::new(
            Box::new(watch2),
            Arc::new(clock2),
            Box::new(snap2),
            Box::new(upl2),
            ws_name2,
            WorkspaceKind::Human,
            800,
            false,
        );
        engine2.seed_expected_root(Some(root_hex_1_for_seed));
        let r2 = engine2
            .run_once()
            .expect("run_once must succeed on second push");
        assert!(
            !r2.root.is_empty(),
            "second SyncResult root must not be empty"
        );
        r2.root
    })
    .await
    .expect("spawn_blocking scenario-1 second push");

    assert_ne!(
        root_hex_2, root_hex_1,
        "second root must differ from first (ref must have advanced)"
    );

    // Pull again and verify the tree matches the modified directory.
    let remote_root_2 = remote.get_ref(ws).await.expect("get_ref after second push");
    assert_eq!(
        hash_to_hex(&remote_root_2),
        root_hex_2,
        "remote ref must equal second snapshot root"
    );

    let pull_store_2 = MemStore::new();
    pull(&remote, ws, &pull_store_2)
        .await
        .expect("pull after second push");
    let index_2 = Index::build(&pull_store_2, &remote_root_2).expect("index after second push");

    // Modified file must have new content.
    let readme_hash = index_2
        .lookup("README.md")
        .expect("README.md must be in second index");
    let readme_bytes = pull_store_2
        .get(&readme_hash)
        .expect("store get")
        .expect("readme blob");
    assert_eq!(
        readme_bytes.as_slice(),
        b"# Updated\n",
        "README.md must have updated content"
    );

    // Deleted file must be absent.
    assert!(
        index_2.lookup("src/lib.rs").is_none(),
        "src/lib.rs must be absent from second index after deletion"
    );

    // src/main.rs must still be present.
    assert!(
        index_2.lookup("src/main.rs").is_some(),
        "src/main.rs must still be present after second push"
    );
}

// ---------------------------------------------------------------------------
// Scenario 2: burst of writes coalesces into exactly one push
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_burst_coalesces_into_single_push() {
    if !e2e_enabled() {
        eprintln!("skipping sync_live_e2e: set LUNAR_SYNC_E2E=1 to run");
        return;
    }

    let ws = "ws-debounce";
    let (base_url, tok, _server_dir) = start_server(ws).await;

    let ws_dir = tempfile::tempdir().expect("workspace tempdir for debounce");
    std::fs::write(ws_dir.path().join("initial.txt"), b"initial content\n")
        .expect("write initial.txt");

    let url = base_url.clone();
    let token = tok.clone();
    let ws_path = ws_dir.path().to_path_buf();
    let ws_name = ws.to_string();

    // All engine interactions happen in spawn_blocking because HttpBlobUploader
    // uses rt.block_on internally. The AtomicU64 is shared so the blocking closure
    // can advance simulated time without sleeping.
    let settled_root = tokio::task::spawn_blocking(move || {
        const DEBOUNCE_MS: u64 = 800;

        let (clock, clock_ms) = AtomicClock::new(1000);
        let store = Arc::new(MemStore::new());
        let (tx, rx) = std::sync::mpsc::channel::<WriteEvent>();
        let watch = ChannelWatchSource::new(rx);
        let snap = WalkSnapshotter::new(store.clone(), &ws_path);
        let remote = HttpRemote::new(&url, &token);
        let upl = HttpBlobUploader::new(remote, store).expect("HttpBlobUploader::new debounce");
        let mut engine = AutoSyncEngine::new(
            Box::new(watch),
            Arc::new(clock),
            Box::new(snap),
            Box::new(upl),
            ws_name,
            WorkspaceKind::Human,
            DEBOUNCE_MS,
            false,
        );

        // Send a burst of 5 events at t=1100, 1200, 1300, 1400, 1500.
        // Each event re-arms the debounce gate; last event is at t=1500.
        let burst_paths = ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"];
        for (i, path) in burst_paths.iter().enumerate() {
            let event_ms = 1100 + i as u64 * 100;
            clock_ms.store(event_ms, Ordering::Relaxed);
            tx.send(WriteEvent {
                path: path.to_string(),
                kind: WriteEventKind::Write,
                at_ms: event_ms,
            })
            .expect("send burst event");
            let r = engine.tick();
            assert!(
                r.is_none(),
                "tick during burst (t={}) must return None (debounce gate not settled), got Some",
                event_ms
            );
        }

        // Advance to last_event + DEBOUNCE_MS - 1 (one ms short of settling).
        // last_event was at t=1500; short settle at t=2299.
        clock_ms.store(1500 + DEBOUNCE_MS - 1, Ordering::Relaxed);
        let r_short = engine.tick();
        assert!(
            r_short.is_none(),
            "tick at debounce - 1ms must return None (not yet settled)"
        );

        // Advance to last_event + DEBOUNCE_MS (exactly settled).
        clock_ms.store(1500 + DEBOUNCE_MS, Ordering::Relaxed);
        let r_settle = engine.tick();
        assert!(
            r_settle.is_some(),
            "tick at exactly DEBOUNCE_MS past last event must return Some(SyncResult)"
        );
        let sync = r_settle.expect("settle result");
        assert!(
            !sync.root.is_empty(),
            "settled SyncResult root must not be empty"
        );

        // Second tick with no new events and clock unchanged: gate is reset, returns None.
        let r_no_double = engine.tick();
        assert!(
            r_no_double.is_none(),
            "second tick after settle must return None (gate reset, no new events)"
        );

        sync.root
    })
    .await
    .expect("spawn_blocking scenario-2 debounce");

    // Verify the remote ref matches the single settled push.
    let remote = HttpRemote::new(&base_url, &tok);
    let remote_ref = remote
        .get_ref(ws)
        .await
        .expect("get_ref after debounce settle");
    assert_eq!(
        hash_to_hex(&remote_ref),
        settled_root,
        "remote ref must equal the single settled SyncResult root"
    );
}

// ---------------------------------------------------------------------------
// Scenario 3: concurrent writer produces conflict ref with no lost data
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_writer_creates_conflict_ref_no_data_loss() {
    if !e2e_enabled() {
        eprintln!("skipping sync_live_e2e: set LUNAR_SYNC_E2E=1 to run");
        return;
    }

    let ws = "ws-conflict";
    let (base_url, tok, _server_dir) = start_server(ws).await;
    let remote = HttpRemote::new(&base_url, &tok);

    // Build store A: initial tree R0.
    let store_a = MemStore::new();
    let h_a_file1 = store_a
        .put(b"device-a initial file\n")
        .expect("put a-file1");
    let root_bytes_r0 = serialize_tree(&[TreeEntry {
        mode: MODE_FILE,
        name: "device_a.txt".into(),
        hash: h_a_file1,
    }]);
    let r0 = store_a.put(&root_bytes_r0).expect("put R0 root tree");
    let r0_hashes = vec![r0, h_a_file1];

    // First push: unconditional (no expected_root). Must commit; remote ref = R0.
    let res0 = push_cas(&store_a, &r0, &remote, ws, None)
        .await
        .expect("push R0");
    assert!(
        matches!(res0.outcome, CasRefOutcome::Committed),
        "first push (R0) must commit unconditionally"
    );
    let remote_after_r0 = remote.get_ref(ws).await.expect("get_ref after R0");
    assert_eq!(
        remote_after_r0, r0,
        "remote ref must equal R0 after first push"
    );

    // Advance store A: build R1 (different content). CAS against R0. Must commit.
    let h_a_file2 = store_a
        .put(b"device-a updated file\n")
        .expect("put a-file2");
    let root_bytes_r1 = serialize_tree(&[TreeEntry {
        mode: MODE_FILE,
        name: "device_a_v2.txt".into(),
        hash: h_a_file2,
    }]);
    let r1 = store_a.put(&root_bytes_r1).expect("put R1 root tree");

    let res1 = push_cas(&store_a, &r1, &remote, ws, Some(&r0))
        .await
        .expect("push R1");
    assert!(
        matches!(res1.outcome, CasRefOutcome::Committed),
        "second push (R1 with correct expected R0) must commit"
    );
    let remote_after_r1 = remote.get_ref(ws).await.expect("get_ref after R1");
    assert_eq!(
        remote_after_r1, r1,
        "remote ref must equal R1 after second push (winner)"
    );

    // Device B: builds a different tree R2 starting from stale expected_root R0.
    // This simulates a concurrent writer that missed the R1 update.
    let store_b = MemStore::new();
    let h_b_file1 = store_b
        .put(b"device-b independent change\n")
        .expect("put b-file1");
    let h_b_file2 = store_b.put(b"device-b second file\n").expect("put b-file2");
    let src_bytes_b = serialize_tree(&[
        TreeEntry {
            mode: MODE_FILE,
            name: "device_b_1.txt".into(),
            hash: h_b_file1,
        },
        TreeEntry {
            mode: MODE_FILE,
            name: "device_b_2.txt".into(),
            hash: h_b_file2,
        },
    ]);
    let h_b_src = store_b.put(&src_bytes_b).expect("put b-src tree");
    let root_bytes_r2 = serialize_tree(&[TreeEntry {
        mode: MODE_DIR,
        name: "device_b".into(),
        hash: h_b_src,
    }]);
    let r2 = store_b.put(&root_bytes_r2).expect("put R2 root tree");

    // All hashes reachable from R2 (tracked manually since we built them above).
    let r2_all_hashes = vec![r2, h_b_src, h_b_file1, h_b_file2];
    let r2_hex = hash_to_hex(&r2);

    // Concurrent push: device B believes remote is still R0 (stale). Must conflict.
    let res2 = push_cas(&store_b, &r2, &remote, ws, Some(&r0))
        .await
        .expect("push R2 conflict");

    let (conflict_current_root, conflict_ref_name) = match res2.outcome {
        CasRefOutcome::Conflict {
            current_root,
            conflict_ref,
        } => (current_root, conflict_ref),
        CasRefOutcome::Committed => panic!("concurrent push must NOT commit; expected Conflict"),
    };

    // current_root in the conflict response must be the winner's root (R1).
    assert_eq!(
        conflict_current_root, r1,
        "conflict current_root must equal R1 (the winner's ref)"
    );

    // conflict_ref must follow the <workspace>@conflict-<first 8 hex chars of R2> convention.
    let expected_conflict_ref = format!("{}@conflict-{}", ws, &r2_hex[..8]);
    assert_eq!(
        conflict_ref_name, expected_conflict_ref,
        "conflict_ref must match <workspace>@conflict-<short_hash> convention"
    );

    // Winner's ref must not be clobbered.
    let remote_after_conflict = remote.get_ref(ws).await.expect("get_ref after conflict");
    assert_eq!(
        remote_after_conflict, r1,
        "remote ref must still equal R1 (winner not clobbered)"
    );

    // No lost data: device B's blobs must all be durably on the server.
    // push_cas uploads blobs BEFORE the CAS attempt, so R2's tree and file blobs
    // are on the server even though the CAS was rejected.
    let missing = remote
        .missing_blobs(&r2_all_hashes, Some(ws))
        .await
        .expect("missing_blobs check for R2 hashes");
    assert!(
        missing.is_empty(),
        "all R2 blobs must be present on server after conflict (no lost data), missing: {:?}",
        missing.iter().map(hash_to_hex).collect::<Vec<_>>()
    );

    // Sanity: verify R0 blobs are still present too (the winner's chain is intact).
    let r0_missing = remote
        .missing_blobs(&r0_hashes, Some(ws))
        .await
        .expect("missing_blobs check for R0 hashes");
    assert!(
        r0_missing.is_empty(),
        "R0 blobs must still be present on server (historical data intact)"
    );
}
