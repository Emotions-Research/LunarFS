// In-process adversarial test suite.
// Drives devdropbox through tower::ServiceExt::oneshot against build_router(state).
// No network, no spawned processes, no sleeps, no wall-clock reads.
// All tests are deterministic and run in the default cargo test gate.
// LUNAR_SMOKE is not read here; gated end-to-end tests live in tests/pen_smoke.rs.
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
use devdropbox::serve::{build_router, AppState};
use http_body_util::BodyExt;
use object_store::memory::InMemory;
use tower::ServiceExt;

// Fixed unix timestamp used throughout; avoids any wall-clock dependence.
const T: i64 = 2_000_000_000;

// ---------------------------------------------------------------------------
// FixedClock
// ---------------------------------------------------------------------------

struct FixedClock;

impl devdropbox::auth::token::Clock for FixedClock {
    fn now_secs(&self) -> i64 {
        T
    }
}

// ---------------------------------------------------------------------------
// Env: holds AppState + temp directory for the lifetime of each test.
// ---------------------------------------------------------------------------

struct Env {
    state: AppState,
    _dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = devdropbox::auth::open(&dir.path().join("adv.db")).expect("auth::open");
        let clock: Arc<dyn devdropbox::auth::token::Clock + Send + Sync> = Arc::new(FixedClock);
        let state = AppState {
            store: Arc::new(InMemory::new()),
            db: Arc::new(Mutex::new(conn)),
            verifier: Arc::new(NoClerk),
            clock,
            presigner: Arc::new(devdropbox::presign::LocalStubPresigner::new(dir.path())),
            ws_backend: Arc::new(devdropbox::workspace::InMemoryBackend::new()),
            ws_store: Arc::new(devdropbox::store::InMemoryWorkspaceStore::new()),
            billing: Arc::new(devdropbox::billing::provider::MockBillingProvider::default()),
            webhook: Arc::new(devdropbox::billing::webhook::FakeWebhookProvider::new("")),
        };
        Env { state, _dir: dir }
    }
}

// ---------------------------------------------------------------------------
// DB setup helpers (lock-release before returning; never cross await points)
// ---------------------------------------------------------------------------

// Create a user, a workspace, and a grant. Returns (user_id, workspace_id, bearer_token).
fn make_workspace(
    db: &Arc<Mutex<rusqlite::Connection>>,
    ws_name: &str,
    perm: Permission,
) -> (i64, i64, String) {
    let conn = db.lock().expect("db lock");
    let uid = repo::create_user(&conn, None, T).expect("create_user");
    let ws_id =
        repo::create_workspace(&conn, ws_name, OwnerKind::User, uid, T).expect("create_workspace");
    acl::grant(
        &conn,
        PrincipalKind::User,
        &uid.to_string(),
        ws_id,
        "/",
        perm,
        T,
    )
    .expect("acl grant");
    let tok = token::mint(
        &conn,
        &TokenPrincipal {
            kind: OwnerKind::User,
            id: uid.to_string(),
        },
        None,
        None,
        &FixedClock,
    )
    .expect("mint")
    .plaintext;
    (uid, ws_id, tok)
}

// Add a second user with a grant on an already-existing workspace (looked up by name).
// Returns (user_id, grant_id, bearer_token).
fn add_user_to_workspace(
    db: &Arc<Mutex<rusqlite::Connection>>,
    ws_name: &str,
    perm: Permission,
) -> (i64, i64, String) {
    let conn = db.lock().expect("db lock");
    let ws_id = repo::workspace_by_name(&conn, ws_name)
        .expect("ws lookup")
        .expect("workspace must exist");
    let uid = repo::create_user(&conn, None, T).expect("create_user");
    let grant_id = acl::grant(
        &conn,
        PrincipalKind::User,
        &uid.to_string(),
        ws_id,
        "/",
        perm,
        T,
    )
    .expect("acl grant");
    let tok = token::mint(
        &conn,
        &TokenPrincipal {
            kind: OwnerKind::User,
            id: uid.to_string(),
        },
        None,
        None,
        &FixedClock,
    )
    .expect("mint")
    .plaintext;
    (uid, grant_id, tok)
}

// Revoke a grant identified by grant_id.
fn revoke_grant(db: &Arc<Mutex<rusqlite::Connection>>, grant_id: i64) {
    let conn = db.lock().expect("db lock");
    acl::revoke(&conn, grant_id, T).expect("revoke");
}

// ---------------------------------------------------------------------------
// HTTP request builders
// ---------------------------------------------------------------------------

fn put_blob_req(hex: &str, body: Vec<u8>, tok: &str, workspace: &str) -> Request<Body> {
    Request::builder()
        .method(Method::PUT)
        .uri(format!("/v1/blob/{}?workspace={}", hex, workspace))
        .header("Authorization", format!("Bearer {}", tok))
        .header("content-type", "application/octet-stream")
        .body(Body::from(body))
        .expect("put_blob request")
}

fn get_blob_req(hex: &str, tok: &str, workspace: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(format!("/v1/blob/{}?workspace={}", hex, workspace))
        .header("Authorization", format!("Bearer {}", tok))
        .body(Body::empty())
        .expect("get_blob request")
}

fn get_ref_req(workspace: &str, tok: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(format!("/v1/ref/{}", workspace))
        .header("Authorization", format!("Bearer {}", tok))
        .body(Body::empty())
        .expect("get_ref request")
}

async fn collect_body(resp: axum::response::Response) -> axum::body::Bytes {
    resp.into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes()
}

// ===========================================================================
// Section 1: authz-bypass
//
// Invariants:
//   (a) Principal A cannot read or write principal B's workspace.
//   (b) An under-scoped (read-only) token is denied on write operations.
// ===========================================================================

// Invariant (a): cross-principal read denied; A's token must not see B's data.
#[tokio::test]
async fn authz_bypass_cross_workspace_read_denied() {
    let env = Env::new();
    let (_, _, tok_a) = make_workspace(&env.state.db, "ws-a", Permission::Write);
    let (_, _, tok_b) = make_workspace(&env.state.db, "ws-b", Permission::Write);

    // B stores a blob in ws-b.
    let data = b"b-private-blob";
    let hex = hash_to_hex(&hash_bytes(data));
    let put = build_router(env.state.clone())
        .oneshot(put_blob_req(&hex, data.to_vec(), &tok_b, "ws-b"))
        .await
        .expect("oneshot");
    assert_eq!(
        put.status(),
        StatusCode::CREATED,
        "B writing own workspace must succeed"
    );

    // A's token tries to read from ws-b: must be denied.
    let get = build_router(env.state.clone())
        .oneshot(get_blob_req(&hex, &tok_a, "ws-b"))
        .await
        .expect("oneshot");
    assert_eq!(
        get.status(),
        StatusCode::FORBIDDEN,
        "A must not read B's workspace"
    );
    let body = collect_body(get).await;
    // Confirm no blob bytes were leaked (body is error JSON, not the secret data).
    assert!(
        !body.as_ref().windows(data.len()).any(|w| w == data),
        "leaked data must not appear"
    );

    // Positive control: A can read from A's own workspace after writing to it.
    let data_a = b"a-own-blob";
    let hex_a = hash_to_hex(&hash_bytes(data_a));
    build_router(env.state.clone())
        .oneshot(put_blob_req(&hex_a, data_a.to_vec(), &tok_a, "ws-a"))
        .await
        .expect("oneshot");
    let get_own = build_router(env.state.clone())
        .oneshot(get_blob_req(&hex_a, &tok_a, "ws-a"))
        .await
        .expect("oneshot");
    assert_eq!(
        get_own.status(),
        StatusCode::OK,
        "A must read from A's own workspace"
    );
}

// Invariant (a): cross-principal write denied; A's token must not write to B's workspace.
#[tokio::test]
async fn authz_bypass_cross_workspace_write_denied() {
    let env = Env::new();
    let (_, _, tok_a) = make_workspace(&env.state.db, "ws-a", Permission::Write);
    let (_, _, _) = make_workspace(&env.state.db, "ws-b", Permission::Write);

    let data = b"write-attempt-on-b";
    let hex = hash_to_hex(&hash_bytes(data));

    // A tries to PUT a blob into ws-b.
    let resp = build_router(env.state.clone())
        .oneshot(put_blob_req(&hex, data.to_vec(), &tok_a, "ws-b"))
        .await
        .expect("oneshot");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "A must not write to B's workspace"
    );

    // Positive control: A's token can write to ws-a.
    let resp_own = build_router(env.state.clone())
        .oneshot(put_blob_req(&hex, data.to_vec(), &tok_a, "ws-a"))
        .await
        .expect("oneshot");
    assert_eq!(
        resp_own.status(),
        StatusCode::CREATED,
        "A must write to A's own workspace"
    );
}

// Invariant (b): a read-only token is denied on write operations.
#[tokio::test]
async fn authz_bypass_under_scoped_token_denied_on_write() {
    let env = Env::new();
    // User A: write grant (populates ws with a blob for the read positive-control).
    let (_, _, tok_write) = make_workspace(&env.state.db, "ws", Permission::Write);
    // User B: read-only grant on the same workspace.
    let (_, _, tok_read) = add_user_to_workspace(&env.state.db, "ws", Permission::Read);

    let data = b"scope-test-data";
    let hex = hash_to_hex(&hash_bytes(data));

    // Write the blob using the write token first.
    build_router(env.state.clone())
        .oneshot(put_blob_req(&hex, data.to_vec(), &tok_write, "ws"))
        .await
        .expect("oneshot");

    // Under-scoped (read-only) token must be denied on PUT.
    let put_resp = build_router(env.state.clone())
        .oneshot(put_blob_req(&hex, data.to_vec(), &tok_read, "ws"))
        .await
        .expect("oneshot");
    assert_eq!(
        put_resp.status(),
        StatusCode::FORBIDDEN,
        "read-only token must be denied on PUT"
    );

    // Positive control: same read-only token CAN GET the blob.
    let get_resp = build_router(env.state.clone())
        .oneshot(get_blob_req(&hex, &tok_read, "ws"))
        .await
        .expect("oneshot");
    assert_eq!(
        get_resp.status(),
        StatusCode::OK,
        "read-only token must be allowed on GET"
    );
}

// ===========================================================================
// Section 2: path-traversal
//
// Invariants:
//   (a) Traversal sequences (../) in the blob hash URL path param are rejected
//       at the hex-validation layer (400) before the object store is reached.
//   (b) Absolute paths and NUL bytes in the hash param are similarly rejected (400).
//   (c) Traversal sequences, absolute paths, and NUL bytes in the workspace param
//       (query string and URL path segment) are denied (404) and never resolve
//       outside the workspace namespace.
// ===========================================================================

// Invariant (a): ../ traversal in the blob hash URL path param is rejected 400.
// The traversal value is percent-encoded (%2F for slash) so it arrives as a single
// URL path segment; hex_to_hash then rejects it because it is not 64 hex chars.
#[tokio::test]
async fn path_traversal_dotdot_in_hash_param_is_bad_request() {
    let env = Env::new();
    let (_, _, tok) = make_workspace(&env.state.db, "ws", Permission::Write);

    // %2E%2E%2F encodes "../"; the whole segment decodes to "../../etc/passwd".
    // Length is 16, not 64: hex_to_hash rejects it with BadHash (400).
    let req = Request::builder()
        .method(Method::GET)
        .uri("/v1/blob/%2E%2E%2F..%2Fetc%2Fpasswd?workspace=ws")
        .header("Authorization", format!("Bearer {}", tok))
        .body(Body::empty())
        .expect("request");
    let resp = build_router(env.state.clone())
        .oneshot(req)
        .await
        .expect("oneshot");
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "../ in hash must be 400 BadRequest"
    );

    // Positive control: a well-formed 64-char hex hash does not trigger 400.
    let data = b"control-blob";
    let hex = hash_to_hex(&hash_bytes(data));
    build_router(env.state.clone())
        .oneshot(put_blob_req(&hex, data.to_vec(), &tok, "ws"))
        .await
        .expect("oneshot");
    let get = build_router(env.state.clone())
        .oneshot(get_blob_req(&hex, &tok, "ws"))
        .await
        .expect("oneshot");
    assert_eq!(get.status(), StatusCode::OK, "valid hex hash must succeed");
}

// Invariant (b): absolute path in the blob hash URL path param is rejected 400.
#[tokio::test]
async fn path_traversal_absolute_path_in_hash_param_is_bad_request() {
    let env = Env::new();
    let (_, _, tok) = make_workspace(&env.state.db, "ws", Permission::Write);

    // An absolute path is not a 64-char hex string; rejected by hex_to_hash.
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/v1/blob/%2Fetc%2Fpasswd?workspace=ws")
        .header("Authorization", format!("Bearer {}", tok))
        .header("content-type", "application/octet-stream")
        .body(Body::from(b"x".to_vec()))
        .expect("request");
    let resp = build_router(env.state.clone())
        .oneshot(req)
        .await
        .expect("oneshot");
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "absolute path in hash must be 400 BadRequest"
    );
}

// Invariant (b): NUL byte in the blob hash URL path param is rejected 400.
// A percent-encoded NUL (%00) is a non-hex character; hex_to_hash rejects it.
#[tokio::test]
async fn path_traversal_nul_in_hash_param_is_bad_request() {
    let env = Env::new();
    let (_, _, tok) = make_workspace(&env.state.db, "ws", Permission::Write);

    // %00 decodes to the NUL byte, which is not a valid hex digit.
    // The full string is also not 64 chars, so length check fails first.
    let req = Request::builder()
        .method(Method::GET)
        .uri("/v1/blob/%00evilhash?workspace=ws")
        .header("Authorization", format!("Bearer {}", tok))
        .body(Body::empty())
        .expect("request");
    let resp = build_router(env.state.clone())
        .oneshot(req)
        .await
        .expect("oneshot");
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "NUL in hash must be 400 BadRequest"
    );
}

// Invariant (c): ../ traversal in the workspace query param is denied (404).
// The traversal value does not exist in the workspace DB, so it resolves to nothing.
// No data from any other workspace is returned or leaked.
#[tokio::test]
async fn path_traversal_dotdot_in_workspace_query_param_denied() {
    let env = Env::new();
    let (_, _, tok_a) = make_workspace(&env.state.db, "ws-a", Permission::Write);

    // Store a blob in ws-a so there is data to potentially leak.
    let data = b"a-sensitive-data";
    let hex = hash_to_hex(&hash_bytes(data));
    build_router(env.state.clone())
        .oneshot(put_blob_req(&hex, data.to_vec(), &tok_a, "ws-a"))
        .await
        .expect("oneshot");

    // Attempt to reach ws-a via a traversal workspace name.
    let req = Request::builder()
        .method(Method::GET)
        .uri(format!("/v1/blob/{}?workspace=..%2Fws-a", hex))
        .header("Authorization", format!("Bearer {}", tok_a))
        .body(Body::empty())
        .expect("request");
    let resp = build_router(env.state.clone())
        .oneshot(req)
        .await
        .expect("oneshot");
    // Traversal workspace name does not exist in DB: 404, never resolves outside namespace.
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "../ws-a traversal must be denied (workspace not in DB)"
    );

    // Positive control: direct access with the real name succeeds.
    let direct = build_router(env.state.clone())
        .oneshot(get_blob_req(&hex, &tok_a, "ws-a"))
        .await
        .expect("oneshot");
    assert_eq!(
        direct.status(),
        StatusCode::OK,
        "direct workspace name must succeed"
    );
}

// Invariant (c): absolute path in the workspace query param is denied (404).
#[tokio::test]
async fn path_traversal_absolute_path_in_workspace_query_param_denied() {
    let env = Env::new();
    let (_, _, tok) = make_workspace(&env.state.db, "ws", Permission::Write);

    let data = b"data";
    let hex = hash_to_hex(&hash_bytes(data));

    // Absolute path as workspace name: no such workspace in DB.
    let req = Request::builder()
        .method(Method::GET)
        .uri(format!("/v1/blob/{}?workspace=%2Fetc%2Fpasswd", hex))
        .header("Authorization", format!("Bearer {}", tok))
        .body(Body::empty())
        .expect("request");
    let resp = build_router(env.state.clone())
        .oneshot(req)
        .await
        .expect("oneshot");
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "absolute path as workspace name must be denied (404)"
    );
}

// Invariant (c): NUL byte in the workspace query param is denied.
// The NUL-containing name does not match any workspace in the DB.
#[tokio::test]
async fn path_traversal_nul_in_workspace_query_param_denied() {
    let env = Env::new();
    let (_, _, tok) = make_workspace(&env.state.db, "ws", Permission::Write);

    let data = b"data";
    let hex = hash_to_hex(&hash_bytes(data));

    // ws%00evil decodes to a string with an embedded NUL byte.
    let req = Request::builder()
        .method(Method::GET)
        .uri(format!("/v1/blob/{}?workspace=ws%00evil", hex))
        .header("Authorization", format!("Bearer {}", tok))
        .body(Body::empty())
        .expect("request");
    let resp = build_router(env.state.clone())
        .oneshot(req)
        .await
        .expect("oneshot");
    // A NUL-embedded name does not exist in the DB; denied.
    let status = resp.status();
    assert!(
        status == StatusCode::NOT_FOUND || status == StatusCode::BAD_REQUEST,
        "NUL in workspace must be denied; got {}",
        status
    );
}

// Invariant (c): traversal in the workspace URL path param is denied.
// The encoded segment "%2E%2E" decodes to "..", which does not exist in the DB.
// The object-store key ref/.. is never constructed.
#[tokio::test]
async fn path_traversal_dotdot_in_workspace_url_path_param_denied() {
    let env = Env::new();
    let (_, _, tok) = make_workspace(&env.state.db, "ws", Permission::Write);

    // %2E%2E decodes to ".." as the workspace name; that name is not in the DB.
    let resp = build_router(env.state.clone())
        .oneshot(get_ref_req("%2E%2E", &tok))
        .await
        .expect("oneshot");
    // ".." is not a workspace in the DB; request is denied.
    assert!(
        resp.status().is_client_error(),
        ".. as workspace URL segment must be denied; got {}",
        resp.status()
    );
}

// Invariant (c): NUL byte in the workspace URL path param is denied.
// %00 decodes to NUL; the resulting workspace name does not exist in the DB.
#[tokio::test]
async fn path_traversal_nul_in_workspace_url_path_param_denied() {
    let env = Env::new();
    let (_, _, tok) = make_workspace(&env.state.db, "ws", Permission::Write);

    // ws%00name decodes to "ws\x00name"; no workspace with that name exists.
    let resp = build_router(env.state.clone())
        .oneshot(get_ref_req("ws%00name", &tok))
        .await
        .expect("oneshot");
    assert!(
        resp.status().is_client_error(),
        "NUL byte in workspace URL param must be denied; got {}",
        resp.status()
    );
}

// ===========================================================================
// Section 3: hash-spoof
//
// Invariant: a blob write whose body bytes do not match the claimed BLAKE3 hash
// is rejected (Epic-1 content-address invariant: claimed_hash == blake3(bytes)).
// ===========================================================================

// Invariant: mismatched BLAKE3 hash is rejected with 422 Unprocessable Entity.
#[tokio::test]
async fn hash_spoof_mismatched_blake3_rejected() {
    let env = Env::new();
    let (_, _, tok) = make_workspace(&env.state.db, "ws", Permission::Write);

    // Claim the hash of "real content" but send "tampered content" as the body.
    let real_data = b"real content";
    let tampered_data = b"tampered content";
    let claimed_hex = hash_to_hex(&hash_bytes(real_data));

    let put = build_router(env.state.clone())
        .oneshot(put_blob_req(
            &claimed_hex,
            tampered_data.to_vec(),
            &tok,
            "ws",
        ))
        .await
        .expect("oneshot");
    assert_eq!(
        put.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "hash mismatch must be rejected 422 (Epic-1 BLAKE3 invariant)"
    );

    // Confirm the spoofed blob was NOT stored: fetching the claimed hash returns 404.
    let get = build_router(env.state.clone())
        .oneshot(get_blob_req(&claimed_hex, &tok, "ws"))
        .await
        .expect("oneshot");
    assert_eq!(
        get.status(),
        StatusCode::NOT_FOUND,
        "spoofed blob must not be stored after hash mismatch"
    );

    // Positive control: a correctly-hashed blob IS accepted.
    let correct_hex = hash_to_hex(&hash_bytes(real_data));
    let put_ok = build_router(env.state.clone())
        .oneshot(put_blob_req(&correct_hex, real_data.to_vec(), &tok, "ws"))
        .await
        .expect("oneshot");
    assert_eq!(
        put_ok.status(),
        StatusCode::CREATED,
        "correctly-hashed blob must be accepted"
    );
    let get_ok = build_router(env.state.clone())
        .oneshot(get_blob_req(&correct_hex, &tok, "ws"))
        .await
        .expect("oneshot");
    assert_eq!(
        get_ok.status(),
        StatusCode::OK,
        "stored blob must be retrievable"
    );
    let got = collect_body(get_ok).await;
    assert_eq!(
        got.as_ref(),
        real_data,
        "retrieved bytes must be byte-identical to the original"
    );
}

// Invariant: all-zeros hash claimed for non-empty content is rejected.
// A zero hash is only valid for the specific content whose BLAKE3 equals zero (none in practice).
#[tokio::test]
async fn hash_spoof_zero_hash_with_nonempty_body_rejected() {
    let env = Env::new();
    let (_, _, tok) = make_workspace(&env.state.db, "ws", Permission::Write);

    let zero_hex = "0".repeat(64);
    let data = b"some data that does not hash to zero";

    let put = build_router(env.state.clone())
        .oneshot(put_blob_req(&zero_hex, data.to_vec(), &tok, "ws"))
        .await
        .expect("oneshot");
    assert_eq!(
        put.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "zero hash with non-matching body must be rejected"
    );
}

// ===========================================================================
// Section 4: ACL-escape
//
// Invariants:
//   (a) A read-only grant permits read but not write (permission boundary holds).
//   (b) A revoked grant is denied on both read and write (revocation is immediate).
// ===========================================================================

// Invariant (a): read-only grant allows GET, denies PUT.
#[tokio::test]
async fn acl_escape_read_only_grant_permits_read_denies_write() {
    let env = Env::new();
    // User A: write access (populates the workspace with a blob).
    let (_, _, tok_a) = make_workspace(&env.state.db, "shared-ws", Permission::Write);
    // User B: read-only grant on the same workspace.
    let (_, _, tok_b) = {
        let (uid, gid, tok) = add_user_to_workspace(&env.state.db, "shared-ws", Permission::Read);
        (uid, gid, tok)
    };

    let data = b"shared-workspace-blob";
    let hex = hash_to_hex(&hash_bytes(data));

    // A writes the blob.
    build_router(env.state.clone())
        .oneshot(put_blob_req(&hex, data.to_vec(), &tok_a, "shared-ws"))
        .await
        .expect("oneshot");

    // Positive control: B can GET the blob (read grant allows read).
    let get_b = build_router(env.state.clone())
        .oneshot(get_blob_req(&hex, &tok_b, "shared-ws"))
        .await
        .expect("oneshot");
    assert_eq!(
        get_b.status(),
        StatusCode::OK,
        "read-only grant must permit GET (positive control)"
    );
    let got = collect_body(get_b).await;
    assert_eq!(
        got.as_ref(),
        data,
        "B's GET body must be byte-identical to the original"
    );

    // ACL-escape: B tries to overwrite with different content via PUT.
    let escalation_data = b"escalation-attempt-write";
    let escalation_hex = hash_to_hex(&hash_bytes(escalation_data));
    let put_b = build_router(env.state.clone())
        .oneshot(put_blob_req(
            &escalation_hex,
            escalation_data.to_vec(),
            &tok_b,
            "shared-ws",
        ))
        .await
        .expect("oneshot");
    assert_eq!(
        put_b.status(),
        StatusCode::FORBIDDEN,
        "read-only grant must deny PUT (ACL-escape invariant)"
    );

    // Confirm the escalation blob was not stored.
    let check = build_router(env.state.clone())
        .oneshot(get_blob_req(&escalation_hex, &tok_a, "shared-ws"))
        .await
        .expect("oneshot");
    assert_eq!(
        check.status(),
        StatusCode::NOT_FOUND,
        "escalation blob must not be stored after write denial"
    );
}

// Invariant (b): a revoked grant is denied immediately on subsequent operations.
#[tokio::test]
async fn acl_escape_revoked_grant_denied() {
    let env = Env::new();
    let (_, _, tok_a) = make_workspace(&env.state.db, "rev-ws", Permission::Write);
    // User B: read-only grant; capture grant_id for revocation.
    let (_, grant_id_b, tok_b) = add_user_to_workspace(&env.state.db, "rev-ws", Permission::Read);

    let data = b"revocable-blob";
    let hex = hash_to_hex(&hash_bytes(data));

    // A writes the blob.
    build_router(env.state.clone())
        .oneshot(put_blob_req(&hex, data.to_vec(), &tok_a, "rev-ws"))
        .await
        .expect("oneshot");

    // B can read before revocation (positive control: grant is active).
    let pre_get = build_router(env.state.clone())
        .oneshot(get_blob_req(&hex, &tok_b, "rev-ws"))
        .await
        .expect("oneshot");
    assert_eq!(
        pre_get.status(),
        StatusCode::OK,
        "B must read before revocation (positive control)"
    );

    // Revoke B's grant.
    revoke_grant(&env.state.db, grant_id_b);

    // After revocation: B's read is denied.
    let post_get = build_router(env.state.clone())
        .oneshot(get_blob_req(&hex, &tok_b, "rev-ws"))
        .await
        .expect("oneshot");
    assert_eq!(
        post_get.status(),
        StatusCode::FORBIDDEN,
        "B must be denied after grant revocation (ACL-escape invariant)"
    );

    // After revocation: B's write attempt is also denied.
    let new_data = b"post-revocation-write-attempt";
    let new_hex = hash_to_hex(&hash_bytes(new_data));
    let post_put = build_router(env.state.clone())
        .oneshot(put_blob_req(&new_hex, new_data.to_vec(), &tok_b, "rev-ws"))
        .await
        .expect("oneshot");
    assert_eq!(
        post_put.status(),
        StatusCode::FORBIDDEN,
        "B's write must also be denied after grant revocation"
    );
}
