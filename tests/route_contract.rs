// Route-contract integration test (default features, no hosted feature gate).
//
// Verifies that every HTTP path HttpRemote uses is registered in build_router
// and returns expected status codes. Distinguishes a routing 404 (unregistered
// path: axum returns empty body) from a resource 404 (ApiError::NotFound:
// returns JSON {"error": "not found"}).
//
// Runs with: cargo test (default features).

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
    let conn = devdropbox::auth::open(&dir.join("rc.db")).expect("auth::open");
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

// Create a workspace, grant Write, mint a token. Returns the token plaintext.
fn seed_workspace(state: &AppState, ws_name: &str) -> String {
    let conn = state.db.lock().expect("db lock");
    let uid = repo::create_user(&conn, None, NOW).expect("create_user");
    let ws_id =
        repo::create_workspace(&conn, ws_name, OwnerKind::User, uid, NOW).expect("create_workspace");
    acl::grant(&conn, PrincipalKind::User, &uid.to_string(), ws_id, "/", Permission::Write, NOW)
        .expect("acl grant");
    token::mint(
        &conn,
        &TokenPrincipal { kind: OwnerKind::User, id: uid.to_string() },
        None,
        None,
        &FixedClock,
    )
    .expect("mint")
    .plaintext
}

async fn body_bytes(resp: axum::response::Response) -> axum::body::Bytes {
    resp.into_body().collect().await.expect("body collect").to_bytes()
}

// A routing 404 has an empty (or non-JSON) body.
// An app-level 404 (ApiError::NotFound) has {"error": "not found"}.
fn is_json(bytes: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(bytes).is_ok()
}

// ---------------------------------------------------------------------------
// Contract: POST /v1/blobs/missing
// ---------------------------------------------------------------------------

// The route must be registered. With a valid workspace and empty hash list it
// returns 200 and {"missing": []}. A routing 404 would never reach this assertion.
#[tokio::test]
async fn contract_blobs_missing_route_registered() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = build_state(dir.path());
    let tok = seed_workspace(&state, "demo");

    let payload = serde_json::to_vec(&serde_json::json!({"hashes": []})).expect("json");
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/blobs/missing?workspace=demo")
        .header("Authorization", format!("Bearer {}", tok))
        .header("content-type", "application/json")
        .body(Body::from(payload))
        .expect("request");

    let resp = build_router(state).oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK, "POST /v1/blobs/missing must return 200");
    let v: serde_json::Value =
        serde_json::from_slice(&body_bytes(resp).await).expect("JSON response body");
    assert_eq!(
        v["missing"].as_array().expect("missing array").len(),
        0,
        "empty hash list means zero missing"
    );
}

// ---------------------------------------------------------------------------
// Contract: PUT /v1/ref/:workspace  and  GET /v1/ref/:workspace
// ---------------------------------------------------------------------------

// Before any PUT the GET returns a resource 404 (JSON body, not empty body).
// After PUT the GET returns 200 with the root hash.
// Both failures would be routing 404s (empty body) if the route was absent.
#[tokio::test]
async fn contract_ref_routes_registered_and_roundtrip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = build_state(dir.path());
    let tok = seed_workspace(&state, "demo");
    let root_hex = hash_to_hex(&hash_bytes(b"test-root-content"));

    // GET before PUT: resource 404 with JSON body, not a routing 404 (empty body).
    let early = build_router(state.clone())
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/v1/ref/demo")
                .header("Authorization", format!("Bearer {}", tok))
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("oneshot");
    assert_eq!(
        early.status(),
        StatusCode::NOT_FOUND,
        "GET /v1/ref/:workspace before PUT must be 404"
    );
    let early_bytes = body_bytes(early).await;
    assert!(
        is_json(&early_bytes),
        "404 body must be JSON (resource not found), not empty (route not registered): {:?}",
        String::from_utf8_lossy(&early_bytes)
    );

    // PUT ref: route must be registered and return 200.
    let put_body = serde_json::to_vec(&serde_json::json!({"root": root_hex})).expect("json");
    let put_resp = build_router(state.clone())
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/v1/ref/demo")
                .header("Authorization", format!("Bearer {}", tok))
                .header("content-type", "application/json")
                .body(Body::from(put_body))
                .expect("req"),
        )
        .await
        .expect("oneshot");
    assert_eq!(put_resp.status(), StatusCode::OK, "PUT /v1/ref/:workspace must return 200");

    // GET after PUT: 200 with root hash.
    let get_resp = build_router(state)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/v1/ref/demo")
                .header("Authorization", format!("Bearer {}", tok))
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("oneshot");
    assert_eq!(get_resp.status(), StatusCode::OK, "GET /v1/ref/:workspace after PUT must return 200");
    let v: serde_json::Value =
        serde_json::from_slice(&body_bytes(get_resp).await).expect("JSON");
    assert_eq!(v["root"].as_str().unwrap(), root_hex, "root hash must round-trip");
}

// ---------------------------------------------------------------------------
// Contract: PUT /v1/blob/:hash  and  GET /v1/blob/:hash
// ---------------------------------------------------------------------------

#[tokio::test]
async fn contract_blob_routes_registered_and_roundtrip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = build_state(dir.path());
    let tok = seed_workspace(&state, "demo");

    let data = b"route-contract-blob-bytes";
    let hex = hash_to_hex(&hash_bytes(data));

    let put_resp = build_router(state.clone())
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/v1/blob/{}?workspace=demo", hex))
                .header("Authorization", format!("Bearer {}", tok))
                .header("content-type", "application/octet-stream")
                .body(Body::from(data.to_vec()))
                .expect("req"),
        )
        .await
        .expect("oneshot");
    assert_eq!(put_resp.status(), StatusCode::CREATED, "PUT /v1/blob/:hash must return 201");

    let get_resp = build_router(state)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/v1/blob/{}?workspace=demo", hex))
                .header("Authorization", format!("Bearer {}", tok))
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("oneshot");
    assert_eq!(get_resp.status(), StatusCode::OK, "GET /v1/blob/:hash must return 200");
    assert_eq!(
        body_bytes(get_resp).await.as_ref(),
        &data[..],
        "blob bytes must round-trip"
    );
}

// ---------------------------------------------------------------------------
// Contract: POST /v1/blob/:hash/presign
// ---------------------------------------------------------------------------

#[tokio::test]
async fn contract_blob_presign_route_registered() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = build_state(dir.path());
    let tok = seed_workspace(&state, "demo");
    let hex = hash_to_hex(&hash_bytes(b"presign-target-bytes"));

    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/v1/blob/{}/presign?op=put&workspace=demo", hex))
        .header("Authorization", format!("Bearer {}", tok))
        .body(Body::empty())
        .expect("req");

    let resp = build_router(state).oneshot(req).await.expect("oneshot");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "POST /v1/blob/:hash/presign must return 200"
    );
    let v: serde_json::Value =
        serde_json::from_slice(&body_bytes(resp).await).expect("JSON");
    assert!(v["url"].as_str().is_some(), "presign response must include url");
    assert_eq!(v["method"].as_str().unwrap(), "PUT", "op=put must yield method=PUT");
}
