#![cfg(feature = "hosted")]

use std::sync::{Arc, Mutex};

use devdropbox::auth::acl::{self, Permission, PrincipalKind};
use devdropbox::auth::repo;
use devdropbox::auth::token;
use devdropbox::auth::token::Principal as TokenPrincipal;
use devdropbox::auth::verify::NoClerk;
use devdropbox::auth::OwnerKind;
use devdropbox::cas::{hash_to_hex, MemStore, Store};
use devdropbox::index::Index;
use devdropbox::remote::HttpRemote;
use devdropbox::serve::{build_router, AppState};
use devdropbox::sync::{pull, push};
use devdropbox::tree::{serialize_tree, TreeEntry, MODE_DIR, MODE_FILE};
use object_store::local::LocalFileSystem;

/// Bind an ephemeral port, set up the identity DB, start the axum server in
/// the background. Returns (base_url, api_token_plaintext, _tempdir).
/// The `_tempdir` must be kept alive for the test duration.
async fn start_server() -> (String, String, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = devdropbox::auth::open(&dir.path().join("id.db")).expect("auth::open");

    // Create a user, workspace, write grant, and API token all before binding the port.
    let token_plaintext = {
        let uid = repo::create_user(&conn, None, 0).expect("create_user");
        let ws_id = repo::create_workspace(&conn, "ws-roundtrip", OwnerKind::User, uid, 0)
            .expect("create_workspace");
        acl::grant(
            &conn,
            PrincipalKind::User,
            &uid.to_string(),
            ws_id,
            "/",
            Permission::Write,
            0,
        )
        .expect("acl grant");
        let minted = token::mint(
            &conn,
            &TokenPrincipal {
                kind: OwnerKind::User,
                id: uid.to_string(),
            },
            None,
            None,
            &devdropbox::auth::token::SystemClock,
        )
        .expect("mint token");
        minted.plaintext
    };

    let clock: Arc<dyn devdropbox::auth::token::Clock + Send + Sync> =
        Arc::new(devdropbox::auth::token::SystemClock);
    // LocalFileSystem + LocalStubPresigner share the same dir so blobs written
    // via the presign path are visible to missing_blobs (which reads the store).
    let store = Arc::new(LocalFileSystem::new_with_prefix(dir.path()).expect("LocalFileSystem"));
    let state = AppState {
        store,
        db: Arc::new(Mutex::new(conn)),
        verifier: Arc::new(NoClerk),
        clock,
        presigner: Arc::new(devdropbox::presign::LocalStubPresigner::new(dir.path())),
        ws_backend: Arc::new(devdropbox::workspace::InMemoryBackend::new()),
        ws_store: Arc::new(devdropbox::store::InMemoryWorkspaceStore::new()),
        billing: Arc::new(devdropbox::billing::provider::MockBillingProvider::default()),
        webhook: Arc::new(devdropbox::billing::webhook::FakeWebhookProvider::new("")),
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind to ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    let router = build_router(state);
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("server error");
    });
    // Yield so the spawned accept loop starts before we send requests.
    tokio::task::yield_now().await;
    (format!("http://127.0.0.1:{}", port), token_plaintext, dir)
}

/// Build a fixture workspace in `store`:
///   README.md
///   src/main.rs
///   src/lib.rs
/// Returns the root tree hash.
fn build_fixture(store: &MemStore) -> devdropbox::cas::Hash {
    let h_readme = store.put(b"# Dev Dropbox\n").expect("put README");
    let h_main = store
        .put(b"fn main() { println!(\"hello\"); }\n")
        .expect("put main.rs");
    let h_lib = store
        .put(b"pub fn greet() -> &'static str { \"hello\" }\n")
        .expect("put lib.rs");

    let src_bytes = serialize_tree(&[
        TreeEntry {
            mode: MODE_FILE,
            name: "lib.rs".into(),
            hash: h_lib,
        },
        TreeEntry {
            mode: MODE_FILE,
            name: "main.rs".into(),
            hash: h_main,
        },
    ]);
    let h_src = store.put(&src_bytes).expect("put src tree");

    let root_bytes = serialize_tree(&[
        TreeEntry {
            mode: MODE_FILE,
            name: "README.md".into(),
            hash: h_readme,
        },
        TreeEntry {
            mode: MODE_DIR,
            name: "src".into(),
            hash: h_src,
        },
    ]);
    store.put(&root_bytes).expect("put root tree")
}

#[tokio::test]
async fn roundtrip_push_pull_and_dedup() {
    let (base_url, token, _dir) = start_server().await;

    // Build fixture in store A.
    let store_a = MemStore::new();
    let root_a = build_fixture(&store_a);

    let remote = HttpRemote::new(&base_url, token);

    // First push: must upload all blobs (5: root tree, src tree, README, main.rs, lib.rs).
    let uploaded_first = push(&store_a, &root_a, &remote, "ws-roundtrip")
        .await
        .expect("first push must succeed");
    assert!(
        uploaded_first > 0,
        "first push must upload at least one blob, got {}",
        uploaded_first
    );

    // Pull into a fresh empty store B.
    let store_b = MemStore::new();
    let root_b = pull(&remote, "ws-roundtrip", &store_b)
        .await
        .expect("pull must succeed");

    // Root hashes must match.
    assert_eq!(
        root_a,
        root_b,
        "pulled root {} must equal pushed root {}",
        hash_to_hex(&root_b),
        hash_to_hex(&root_a)
    );

    // Every file reachable from A must be present in B with byte-identical content.
    let index_a = Index::build(&store_a, &root_a).expect("index A must build");
    let index_b = Index::build(&store_b, &root_b).expect("index B must build");

    assert_eq!(
        index_a.len(),
        index_b.len(),
        "file count must match after roundtrip"
    );

    for (path, hash_a) in index_a.entries() {
        let hash_b = index_b
            .lookup(path)
            .unwrap_or_else(|| panic!("path {} missing from B", path));
        assert_eq!(
            *hash_a, hash_b,
            "hash mismatch for {} after roundtrip",
            path
        );
        let content_a = store_a.get(hash_a).unwrap().unwrap();
        let content_b = store_b.get(&hash_b).unwrap().unwrap();
        assert_eq!(content_a, content_b, "content mismatch for {}", path);
    }

    // Second push: all blobs already on server, must upload zero.
    let uploaded_second = push(&store_a, &root_a, &remote, "ws-roundtrip")
        .await
        .expect("second push must succeed");
    assert_eq!(
        uploaded_second, 0,
        "second push must be a full dedup hit (0 uploads)"
    );
}
