// Integration tests: presign bypass -- blob bytes routed through the local
// filesystem stub resolver, not through the axum put_blob / get_blob handlers.
//
// Setup: LocalFileSystem store rooted at <tempdir> + LocalStubPresigner rooted
// at the SAME <tempdir>. put_presigned writes to <tempdir>/blobs/<aa>/<rest>
// via the stub resolver; fetch_presigned reads from the same path. The axum
// blob handlers are never invoked for the byte transfer.
#![cfg(feature = "hosted")]

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use devdropbox::auth::acl::{self, Permission, PrincipalKind};
use devdropbox::auth::repo;
use devdropbox::auth::token;
use devdropbox::auth::token::Principal as TokenPrincipal;
use devdropbox::auth::verify::NoClerk;
use devdropbox::auth::OwnerKind;
use devdropbox::cas::{hash_bytes, hash_to_hex};
use devdropbox::presign::{
    fetch_presigned, put_presigned, LocalStubPresigner, PresignError, PresignOp, Presigner,
    resolve_stub,
};
use devdropbox::serve::{build_router, AppState};
use http_body_util::BodyExt;
use object_store::local::LocalFileSystem;
use tower::ServiceExt;

// Fixed reference time (2033-05-18) matching serve_http.rs convention.
const NOW: i64 = 2_000_000_000;

// ---------------------------------------------------------------------------
// FixedClock
// ---------------------------------------------------------------------------

struct FixedClock {
    now: i64,
}

impl devdropbox::auth::token::Clock for FixedClock {
    fn now_secs(&self) -> i64 {
        self.now
    }
}

// ---------------------------------------------------------------------------
// Test environment
// ---------------------------------------------------------------------------

struct Env {
    state: AppState,
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = devdropbox::auth::open(&dir.path().join("test.db")).expect("auth::open");
        let store = Arc::new(
            LocalFileSystem::new_with_prefix(dir.path()).expect("LocalFileSystem"),
        );
        let presigner: Arc<dyn devdropbox::presign::Presigner> =
            Arc::new(LocalStubPresigner::new(dir.path()));
        let clock: Arc<dyn devdropbox::auth::token::Clock + Send + Sync> =
            Arc::new(FixedClock { now: NOW });
        let state = AppState {
            store,
            db: Arc::new(Mutex::new(conn)),
            verifier: Arc::new(NoClerk),
            clock,
            presigner,
            ws_backend: Arc::new(devdropbox::workspace::InMemoryBackend::new()),
            ws_store: Arc::new(devdropbox::store::InMemoryWorkspaceStore::new()),
            billing: Arc::new(devdropbox::billing::provider::MockBillingProvider::default()),
            webhook: Arc::new(devdropbox::billing::webhook::FakeWebhookProvider::new("")),
        };
        Env { state, dir }
    }
}

// ---------------------------------------------------------------------------
// Setup helpers
// ---------------------------------------------------------------------------

fn setup_write_workspace(
    db: &Arc<Mutex<rusqlite::Connection>>,
    ws_name: &str,
) -> (i64, i64, String) {
    let conn = db.lock().expect("db lock");
    let uid = repo::create_user(&conn, None, NOW).expect("create_user");
    let ws_id = repo::create_workspace(&conn, ws_name, OwnerKind::User, uid, NOW)
        .expect("create_workspace");
    acl::grant(&conn, PrincipalKind::User, &uid.to_string(), ws_id, "/", Permission::Write, NOW)
        .expect("acl grant");
    let minted = token::mint(
        &conn,
        &TokenPrincipal { kind: OwnerKind::User, id: uid.to_string() },
        None,
        None,
        &FixedClock { now: NOW },
    )
    .expect("mint");
    (uid, ws_id, minted.plaintext)
}

// ---------------------------------------------------------------------------
// Response body helper
// ---------------------------------------------------------------------------

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    resp.into_body().collect().await.expect("body collect").to_bytes().to_vec()
}

// ---------------------------------------------------------------------------
// Presign request builder
// ---------------------------------------------------------------------------

fn presign_req(hex: &str, op: &str, workspace: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(format!("/v1/blob/{}/presign?op={}&workspace={}", hex, op, workspace))
        .header("Authorization", format!("Bearer {}", token))
        .body(Body::empty())
        .expect("presign request")
}

// ---------------------------------------------------------------------------
// (a) bypass_put_then_get_roundtrip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bypass_put_then_get_roundtrip() {
    let env = Env::new();
    let (_, _, tok) = setup_write_workspace(&env.state.db, "ws");

    let data: &[u8] = b"presign bypass payload";
    let hash = hash_bytes(data);
    let hex = hash_to_hex(&hash);

    // Mint a PUT presign URL via the server endpoint.
    let put_resp = build_router(env.state.clone())
        .oneshot(presign_req(&hex, "put", "ws", &tok))
        .await
        .expect("oneshot");
    assert_eq!(put_resp.status(), StatusCode::OK, "presign PUT must return 200");
    let put_body: serde_json::Value =
        serde_json::from_slice(&body_bytes(put_resp).await).expect("presign PUT json");
    let put_url = put_body["url"].as_str().expect("url field").to_string();
    assert_eq!(put_body["method"].as_str().unwrap(), "PUT");

    // Write bytes directly via the stub resolver -- no axum handler involved.
    let client = reqwest::Client::new();
    put_presigned(&client, &put_url, data.to_vec(), NOW)
        .await
        .expect("put_presigned must succeed");

    // Verify the file exists on disk at the expected object_store path BEFORE
    // any GET handler call. This is the structural proof of the bypass.
    let expected_path = env.dir.path().join(format!("blobs/{}/{}", &hex[..2], &hex[2..]));
    assert!(
        expected_path.exists(),
        "blob must exist on disk at {} after put_presigned",
        expected_path.display()
    );
    let on_disk = std::fs::read(&expected_path).expect("read on-disk blob");
    assert_eq!(on_disk, data, "on-disk bytes must be byte-identical to uploaded data");

    // Mint a GET presign URL and fetch bytes via the stub resolver.
    let get_resp = build_router(env.state.clone())
        .oneshot(presign_req(&hex, "get", "ws", &tok))
        .await
        .expect("oneshot");
    assert_eq!(get_resp.status(), StatusCode::OK, "presign GET must return 200");
    let get_body: serde_json::Value =
        serde_json::from_slice(&body_bytes(get_resp).await).expect("presign GET json");
    let get_url = get_body["url"].as_str().expect("url field").to_string();
    assert_eq!(get_body["method"].as_str().unwrap(), "GET");

    let fetched = fetch_presigned(&client, &get_url, NOW).await.expect("fetch_presigned must succeed");
    assert_eq!(fetched, data, "fetched bytes must be byte-identical to uploaded data");

    // No /v1/blob/:hash PUT or GET oneshot was issued in this test: the bytes
    // moved entirely through the filesystem stub resolver.
}

// ---------------------------------------------------------------------------
// (b) expired_presign_rejected
// ---------------------------------------------------------------------------

#[tokio::test]
async fn expired_presign_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = LocalStubPresigner::new(dir.path());
    let object_path = "blobs/aa/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    // TTL = 10s, minted at now=1000, so expires_at = 1010.
    let signed = p.presign(PresignOp::Put, object_path, 10, 1000).expect("presign");
    assert_eq!(signed.expires_at, 1010);

    // Resolve at now=2000 (well past expiry): must return Expired.
    let err = resolve_stub(&signed.url, 2000).expect_err("must be Expired");
    assert!(matches!(err, PresignError::Expired), "expected Expired, got: {}", err);

    // fetch_presigned must also return Err when the URL is expired.
    let client = reqwest::Client::new();
    let fetch_err = fetch_presigned(&client, &signed.url, 2000).await;
    assert!(fetch_err.is_err(), "fetch_presigned with expired URL must return Err");
    let msg = format!("{}", fetch_err.unwrap_err());
    assert!(msg.contains("expired"), "error message must mention expiry, got: {}", msg);
}

// ---------------------------------------------------------------------------
// (c) invalid_presign_rejected
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invalid_presign_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = LocalStubPresigner::new(dir.path());
    let object_path = "blobs/cc/dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    let signed = p.presign(PresignOp::Get, object_path, 300, 1000).expect("presign");

    // The URL is: stub+local://BASE/EXPIRES/OP/SIG/PCT_PATH
    // Split on '/', SIG is at index 5.
    let mut parts: Vec<String> = signed.url.split('/').map(|s| s.to_string()).collect();
    assert!(parts.len() >= 7, "URL must have at least 7 slash-segments, got {}", parts.len());
    let last = parts[5].pop().unwrap_or('0');
    parts[5].push(if last == '0' { 'f' } else { '0' });
    let corrupted = parts.join("/");

    let err = resolve_stub(&corrupted, 1100).expect_err("must be InvalidSignature");
    assert!(
        matches!(err, PresignError::InvalidSignature),
        "expected InvalidSignature, got: {}",
        err
    );

    // fetch_presigned must also return Err on a tampered URL.
    let client = reqwest::Client::new();
    let fetch_err = fetch_presigned(&client, &corrupted, 1100).await;
    assert!(fetch_err.is_err(), "fetch_presigned with tampered URL must return Err");
    let msg = format!("{}", fetch_err.unwrap_err());
    assert!(
        msg.contains("signature") || msg.contains("invalid"),
        "error message must mention signature invalidity, got: {}",
        msg
    );
}

// ---------------------------------------------------------------------------
// (d) full_client_roundtrip_via_tcp
//
// Spin up the in-process router on an ephemeral TCP port, drive HttpRemote
// put_blob / get_blob, and assert that the bytes land on disk via the stub
// presign path (not through the server handler).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_client_roundtrip_via_tcp() {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = devdropbox::auth::open(&dir.path().join("test.db")).expect("auth::open");
    let store = Arc::new(
        LocalFileSystem::new_with_prefix(dir.path()).expect("LocalFileSystem"),
    );
    let presigner: Arc<dyn devdropbox::presign::Presigner> =
        Arc::new(LocalStubPresigner::new(dir.path()));
    let clock: Arc<dyn devdropbox::auth::token::Clock + Send + Sync> =
        Arc::new(FixedClock { now: NOW });
    let db: Arc<Mutex<rusqlite::Connection>> = Arc::new(Mutex::new(conn));

    // Set up the workspace and mint a token before moving the DB into AppState.
    let tok = {
        let c = db.lock().expect("db lock");
        let uid = repo::create_user(&c, None, NOW).expect("create_user");
        let ws_id = repo::create_workspace(&c, "ws", OwnerKind::User, uid, NOW)
            .expect("create_workspace");
        acl::grant(&c, PrincipalKind::User, &uid.to_string(), ws_id, "/", Permission::Write, NOW)
            .expect("acl grant");
        token::mint(
            &c,
            &TokenPrincipal { kind: OwnerKind::User, id: uid.to_string() },
            None,
            None,
            &FixedClock { now: NOW },
        )
        .expect("mint")
        .plaintext
    };

    let state = AppState {
        store,
        db,
        verifier: Arc::new(NoClerk),
        clock,
        presigner,
        ws_backend: Arc::new(devdropbox::workspace::InMemoryBackend::new()),
        ws_store: Arc::new(devdropbox::store::InMemoryWorkspaceStore::new()),
        billing: Arc::new(devdropbox::billing::provider::MockBillingProvider::default()),
        webhook: Arc::new(devdropbox::billing::webhook::FakeWebhookProvider::new("")),
    };

    // Bind an ephemeral port; pass the listener directly to axum::serve so there
    // is no bind-drop-rebind race.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let base_url = format!("http://{}", addr);

    let router = build_router(state);
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("axum::serve");
    });

    // Readiness probe: loop until the server accepts a connection (any HTTP
    // response, including 404, means the port is bound).
    let http_client = reqwest::Client::new();
    let probe_url = format!("{}/v1/ref/probe-absent", base_url);
    let mut ready = false;
    for _ in 0..40usize {
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        if http_client.get(&probe_url).bearer_auth(&tok).send().await.is_ok() {
            ready = true;
            break;
        }
    }
    assert!(ready, "in-process server must become ready within 2s on {}", addr);

    // Drive HttpRemote -- put_blob and get_blob route through presign by default.
    let data: &[u8] = b"full client presign bypass roundtrip";
    let hash = hash_bytes(data);
    let hex = hash_to_hex(&hash);
    let remote = devdropbox::remote::HttpRemote::new(&base_url, &tok);

    remote.put_blob(&hash, data.to_vec(), Some("ws")).await.expect("put_blob via presign");

    // The blob must exist on disk (stub presign resolver wrote it, not the handler).
    let disk_path = dir.path().join(format!("blobs/{}/{}", &hex[..2], &hex[2..]));
    assert!(
        disk_path.exists(),
        "blob must be on disk at {} after HttpRemote::put_blob via presign",
        disk_path.display()
    );
    let on_disk = std::fs::read(&disk_path).expect("read on-disk blob");
    assert_eq!(on_disk, data, "on-disk bytes must match uploaded data");

    let fetched = remote.get_blob(&hash, Some("ws")).await.expect("get_blob via presign");
    assert_eq!(fetched, data, "fetched bytes must be byte-identical to uploaded data");

    server.abort();
    let _ = server.await;
}
