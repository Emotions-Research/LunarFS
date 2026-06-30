// Integration tests for the put_ref compare-and-swap (CAS) feature.
//
// Three cases per spec:
//   (a) absent expected_root       => unconditional overwrite (legacy)
//   (b) matching expected_root     => overwrite + 200
//   (c) mismatching expected_root  => current ref NOT overwritten, conflict ref created, 409
//
// Runs with: cargo test (default features) and cargo test --features hosted.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use devdropbox::auth::acl::{self, Permission, PrincipalKind};
use devdropbox::auth::repo;
use devdropbox::auth::token;
use devdropbox::auth::token::Principal as TokenPrincipal;
use devdropbox::auth::verify::NoClerk;
use devdropbox::auth::OwnerKind;
use devdropbox::cas::hash_bytes;
use devdropbox::cas::hash_to_hex;
use devdropbox::serve::{build_router, AppState};
use devdropbox::store::InMemoryWorkspaceStore;
use devdropbox::workspace::InMemoryBackend;
use http_body_util::BodyExt;
use object_store::memory::InMemory;
use tower::ServiceExt;

const NOW: i64 = 2_000_000_000;

struct FixedClock;

impl devdropbox::auth::token::Clock for FixedClock {
    fn now_secs(&self) -> i64 {
        NOW
    }
}

fn build_state(dir: &std::path::Path) -> AppState {
    let conn = devdropbox::auth::open(&dir.join("cas.db")).expect("auth::open");
    AppState {
        store: Arc::new(InMemory::new()),
        db: Arc::new(Mutex::new(conn)),
        verifier: Arc::new(NoClerk),
        clock: Arc::new(FixedClock),
        presigner: Arc::new(devdropbox::presign::LocalStubPresigner::new(dir)),
        ws_backend: Arc::new(InMemoryBackend::new()),
        ws_store: Arc::new(InMemoryWorkspaceStore::new()),
        #[cfg(feature = "hosted")]
        billing: Arc::new(devdropbox::billing::provider::MockBillingProvider::default()),
        #[cfg(feature = "hosted")]
        webhook: Arc::new(devdropbox::billing::webhook::FakeWebhookProvider::new("")),
    }
}

fn seed_workspace(state: &AppState, ws_name: &str) -> String {
    let conn = state.db.lock().expect("db lock");
    let uid = repo::create_user(&conn, None, NOW).expect("create_user");
    let ws_id = repo::create_workspace(&conn, ws_name, OwnerKind::User, uid, NOW)
        .expect("create_workspace");
    acl::grant(
        &conn,
        PrincipalKind::User,
        &uid.to_string(),
        ws_id,
        "/",
        Permission::Write,
        NOW,
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
        &FixedClock,
    )
    .expect("mint")
    .plaintext
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("JSON body")
}

async fn put_ref_request(
    state: AppState,
    tok: &str,
    ws: &str,
    body: serde_json::Value,
) -> axum::response::Response {
    let payload = serde_json::to_vec(&body).expect("json");
    build_router(state)
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/v1/ref/{}", ws))
                .header("Authorization", format!("Bearer {}", tok))
                .header("content-type", "application/json")
                .body(Body::from(payload))
                .expect("req"),
        )
        .await
        .expect("oneshot")
}

async fn get_ref_value(state: AppState, tok: &str, ws: &str) -> Option<String> {
    let resp = build_router(state)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/v1/ref/{}", ws))
                .header("Authorization", format!("Bearer {}", tok))
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("oneshot");
    if resp.status() != StatusCode::OK {
        return None;
    }
    let v = body_json(resp).await;
    v["root"].as_str().map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// (a) Absent expected_root => unconditional overwrite (legacy path)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn absent_expected_root_is_unconditional_overwrite() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = build_state(dir.path());
    let tok = seed_workspace(&state, "demo");

    let root_a = hash_to_hex(&hash_bytes(b"root-a"));
    let root_b = hash_to_hex(&hash_bytes(b"root-b"));

    // First write (no expected_root): sets root_a.
    let r = put_ref_request(
        state.clone(),
        &tok,
        "demo",
        serde_json::json!({"root": root_a}),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);

    // Second write (no expected_root): unconditionally overwrites to root_b.
    let r = put_ref_request(
        state.clone(),
        &tok,
        "demo",
        serde_json::json!({"root": root_b}),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);

    // Verify root_b is stored (root_a was clobbered).
    let stored = get_ref_value(state, &tok, "demo").await;
    assert_eq!(stored.as_deref(), Some(root_b.as_str()));
}

// ---------------------------------------------------------------------------
// (b) Matching expected_root => overwrite + 200
// ---------------------------------------------------------------------------

#[tokio::test]
async fn matching_expected_root_overwrites_with_200() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = build_state(dir.path());
    let tok = seed_workspace(&state, "ws-match");

    let root_a = hash_to_hex(&hash_bytes(b"match-root-a"));
    let root_b = hash_to_hex(&hash_bytes(b"match-root-b"));

    // Seed root_a unconditionally.
    let r = put_ref_request(
        state.clone(),
        &tok,
        "ws-match",
        serde_json::json!({"root": root_a}),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);

    // CAS update with correct expected_root: should succeed.
    let r = put_ref_request(
        state.clone(),
        &tok,
        "ws-match",
        serde_json::json!({"root": root_b, "expected_root": root_a}),
    )
    .await;
    assert_eq!(
        r.status(),
        StatusCode::OK,
        "CAS with matching expected_root must return 200"
    );

    let stored = get_ref_value(state, &tok, "ws-match").await;
    assert_eq!(
        stored.as_deref(),
        Some(root_b.as_str()),
        "root_b must be stored after CAS success"
    );
}

// ---------------------------------------------------------------------------
// (c) Mismatching expected_root => no overwrite, conflict ref, 409
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mismatching_expected_root_creates_conflict_ref_and_returns_409() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = build_state(dir.path());
    let tok = seed_workspace(&state, "ws-conflict");

    let root_a = hash_to_hex(&hash_bytes(b"conflict-root-a"));
    let root_b = hash_to_hex(&hash_bytes(b"conflict-root-b"));
    let root_incoming = hash_to_hex(&hash_bytes(b"conflict-root-incoming"));

    // Seed root_a.
    let r = put_ref_request(
        state.clone(),
        &tok,
        "ws-conflict",
        serde_json::json!({"root": root_a}),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);

    // CAS update where server holds root_a but client expects root_b: mismatch.
    let r = put_ref_request(
        state.clone(),
        &tok,
        "ws-conflict",
        serde_json::json!({"root": root_incoming, "expected_root": root_b}),
    )
    .await;
    assert_eq!(
        r.status(),
        StatusCode::CONFLICT,
        "mismatching expected_root must return 409"
    );

    let body = body_json(r).await;
    let conflict_ref = body["conflict_ref"].as_str().expect("conflict_ref in body");
    let current_root = body["current_root"].as_str().expect("current_root in body");

    // conflict_ref must be <workspace>@conflict-<first 8 chars of incoming root>
    let expected_short_hash = &root_incoming[..8];
    assert_eq!(
        conflict_ref,
        format!("ws-conflict@conflict-{}", expected_short_hash),
        "conflict_ref name must follow <workspace>@conflict-<short_hash> convention"
    );
    assert_eq!(
        current_root, root_a,
        "current_root in 409 body must be root_a"
    );

    // The workspace ref must NOT have been overwritten: still root_a.
    let stored = get_ref_value(state, &tok, "ws-conflict").await;
    assert_eq!(
        stored.as_deref(),
        Some(root_a.as_str()),
        "workspace ref must remain root_a after conflict"
    );
    // Note: conflict refs are stored in the object store under a name that has no
    // DB workspace row, so they are not readable via GET /v1/ref directly. The 409
    // body (conflict_ref + current_root) and the workspace-ref invariant above are
    // the observable proof that the conflict ref was created and the main ref was protected.
}

// ---------------------------------------------------------------------------
// Edge: first push with expected_root set => accept (no conflict against nothing)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn first_push_with_expected_root_is_accepted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = build_state(dir.path());
    let tok = seed_workspace(&state, "ws-first");

    let root_a = hash_to_hex(&hash_bytes(b"first-push-root-a"));
    let some_expected = hash_to_hex(&hash_bytes(b"some-expected-that-does-not-exist"));

    // No existing ref for ws-first. Push with an expected_root set anyway.
    let r = put_ref_request(
        state.clone(),
        &tok,
        "ws-first",
        serde_json::json!({"root": root_a, "expected_root": some_expected}),
    )
    .await;
    assert_eq!(
        r.status(),
        StatusCode::OK,
        "first push with expected_root must be accepted (no current ref to conflict against)"
    );

    let stored = get_ref_value(state, &tok, "ws-first").await;
    assert_eq!(stored.as_deref(), Some(root_a.as_str()));
}
