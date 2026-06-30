// Deterministic integration tests for the workspace lifecycle HTTP endpoints.
// All tests use tower::ServiceExt::oneshot against build_router(state) with
// InMemoryBackend and InMemoryWorkspaceStore -- network-free and mount-free.
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
use devdropbox::serve::{build_router, AppState};
use devdropbox::store::InMemoryWorkspaceStore;
use devdropbox::workspace::InMemoryBackend;
use http_body_util::BodyExt;
use object_store::memory::InMemory;
use tower::ServiceExt;

const NOW: i64 = 2_000_000_000;

struct FixedClock {
    now: i64,
}

impl devdropbox::auth::token::Clock for FixedClock {
    fn now_secs(&self) -> i64 {
        self.now
    }
}

struct Env {
    state: AppState,
    _dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = devdropbox::auth::open(&dir.path().join("test.db")).expect("auth::open");
        let clock: Arc<dyn devdropbox::auth::token::Clock + Send + Sync> =
            Arc::new(FixedClock { now: NOW });
        let state = AppState {
            store: Arc::new(InMemory::new()),
            db: Arc::new(Mutex::new(conn)),
            verifier: Arc::new(NoClerk),
            clock,
            presigner: Arc::new(devdropbox::presign::LocalStubPresigner::new(dir.path())),
            ws_backend: Arc::new(InMemoryBackend::new()),
            ws_store: Arc::new(InMemoryWorkspaceStore::new()),
            billing: Arc::new(devdropbox::billing::provider::MockBillingProvider::default()),
            webhook: Arc::new(devdropbox::billing::webhook::FakeWebhookProvider::new("")),
        };
        Env { state, _dir: dir }
    }
}

// Create an Epic-2 workspace `ws_name` owned by a fresh user, grant that user
// Write at "/", and mint an API token. Returns (uid, token_plaintext).
fn setup_user_write_on(db: &Arc<Mutex<rusqlite::Connection>>, ws_name: &str) -> (i64, String) {
    let conn = db.lock().expect("db lock");
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
    let minted = token::mint(
        &conn,
        &TokenPrincipal {
            kind: OwnerKind::User,
            id: uid.to_string(),
        },
        None,
        None,
        &FixedClock { now: NOW },
    )
    .expect("mint");
    (uid, minted.plaintext)
}

// Create a user with no ACL grants; return their API token.
fn setup_user_no_grant(db: &Arc<Mutex<rusqlite::Connection>>) -> String {
    let conn = db.lock().expect("db lock");
    let uid = repo::create_user(&conn, None, NOW).expect("create_user no-grant");
    let minted = token::mint(
        &conn,
        &TokenPrincipal {
            kind: OwnerKind::User,
            id: uid.to_string(),
        },
        None,
        None,
        &FixedClock { now: NOW },
    )
    .expect("mint no-grant");
    minted.plaintext
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("body collect")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("json body")
}

fn fork_req(body: serde_json::Value, tok: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri("/v1/workspace/fork")
        .header("Authorization", format!("Bearer {}", tok))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).expect("json encode")))
        .expect("fork request")
}

fn list_req(tok: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri("/v1/workspaces")
        .header("Authorization", format!("Bearer {}", tok))
        .body(Body::empty())
        .expect("list request")
}

fn delete_req(id: &str, tok: &str) -> Request<Body> {
    Request::builder()
        .method(Method::DELETE)
        .uri(format!("/v1/workspace/{}", id))
        .header("Authorization", format!("Bearer {}", tok))
        .body(Body::empty())
        .expect("delete request")
}

// (a) Authorized fork: 201 with non-empty id; subsequent list includes that id with
// the correct label.
#[tokio::test]
async fn authorized_fork_appears_in_list() {
    let env = Env::new();
    let (_, tok) = setup_user_write_on(&env.state.db, "main");

    let fork_resp = build_router(env.state.clone())
        .oneshot(fork_req(
            serde_json::json!({"label": "agent-1", "from_base": "main", "ttl_secs": 3600}),
            &tok,
        ))
        .await
        .expect("oneshot fork");
    assert_eq!(
        fork_resp.status(),
        StatusCode::CREATED,
        "fork must return 201"
    );
    let fork_body = body_json(fork_resp).await;
    let ws_id = fork_body["id"]
        .as_str()
        .expect("id field must be a string")
        .to_string();
    assert!(!ws_id.is_empty(), "returned id must not be empty");

    let list_resp = build_router(env.state.clone())
        .oneshot(list_req(&tok))
        .await
        .expect("oneshot list");
    assert_eq!(list_resp.status(), StatusCode::OK, "list must return 200");
    let list_body = body_json(list_resp).await;
    let workspaces = list_body["workspaces"]
        .as_array()
        .expect("workspaces must be array");
    assert_eq!(
        workspaces.len(),
        1,
        "list must contain exactly one workspace"
    );
    assert_eq!(
        workspaces[0]["id"].as_str().unwrap(),
        ws_id,
        "listed id must match forked id"
    );
    assert_eq!(
        workspaces[0]["label"].as_str().unwrap(),
        "agent-1",
        "label must round-trip"
    );
}

// (b) Unauthorized path: caller with no ACL grant on "main" gets 403 on fork;
// their list returns an empty array.
#[tokio::test]
async fn unauthorized_path_rejected() {
    let env = Env::new();
    // Owner creates "main" and has Write; caller_tok has no grant on "main".
    let (_owner_uid, _owner_tok) = setup_user_write_on(&env.state.db, "main");
    let caller_tok = setup_user_no_grant(&env.state.db);

    let fork_resp = build_router(env.state.clone())
        .oneshot(fork_req(
            serde_json::json!({"from_base": "main"}),
            &caller_tok,
        ))
        .await
        .expect("oneshot fork");
    assert_eq!(
        fork_resp.status(),
        StatusCode::FORBIDDEN,
        "unauthorized fork must be 403"
    );

    let list_resp = build_router(env.state.clone())
        .oneshot(list_req(&caller_tok))
        .await
        .expect("oneshot list");
    assert_eq!(list_resp.status(), StatusCode::OK, "list must return 200");
    let list_body = body_json(list_resp).await;
    let workspaces = list_body["workspaces"]
        .as_array()
        .expect("workspaces must be array");
    assert!(
        workspaces.is_empty(),
        "unauthorized caller must see an empty list"
    );
}

// (c) Delete removes the workspace: 200, no longer in list, second delete is 404.
#[tokio::test]
async fn delete_removes_and_destroys() {
    let env = Env::new();
    let (_, tok) = setup_user_write_on(&env.state.db, "main");

    // Fork to create a lifecycle workspace.
    let fork_resp = build_router(env.state.clone())
        .oneshot(fork_req(serde_json::json!({"from_base": "main"}), &tok))
        .await
        .expect("oneshot fork");
    assert_eq!(
        fork_resp.status(),
        StatusCode::CREATED,
        "fork must return 201"
    );
    let ws_id = body_json(fork_resp).await["id"]
        .as_str()
        .expect("id field")
        .to_string();

    // Delete it.
    let del_resp = build_router(env.state.clone())
        .oneshot(delete_req(&ws_id, &tok))
        .await
        .expect("oneshot delete");
    assert_eq!(
        del_resp.status(),
        StatusCode::OK,
        "first delete must return 200"
    );

    // List no longer includes the workspace.
    let list_resp = build_router(env.state.clone())
        .oneshot(list_req(&tok))
        .await
        .expect("oneshot list after delete");
    assert_eq!(list_resp.status(), StatusCode::OK);
    let workspaces = body_json(list_resp).await["workspaces"]
        .as_array()
        .expect("workspaces array")
        .clone();
    assert!(
        !workspaces.iter().any(|w| w["id"].as_str() == Some(&ws_id)),
        "deleted workspace must not appear in list"
    );

    // Second delete: 404 because the record is gone from the store.
    let del2_resp = build_router(env.state.clone())
        .oneshot(delete_req(&ws_id, &tok))
        .await
        .expect("oneshot second delete");
    assert_eq!(
        del2_resp.status(),
        StatusCode::NOT_FOUND,
        "second delete must return 404"
    );
}

// (d) TTL round-trips: ephemeral workspace has ttl_secs and state="ephemeral";
// persistent workspace has ttl_secs=null and state="persistent".
#[tokio::test]
async fn ttl_round_trips() {
    let env = Env::new();
    let (_, tok) = setup_user_write_on(&env.state.db, "main");

    // Fork with TTL.
    let ephemeral_resp = build_router(env.state.clone())
        .oneshot(fork_req(
            serde_json::json!({"from_base": "main", "ttl_secs": 7200}),
            &tok,
        ))
        .await
        .expect("oneshot ephemeral fork");
    assert_eq!(
        ephemeral_resp.status(),
        StatusCode::CREATED,
        "ephemeral fork must return 201"
    );
    let ephemeral_id = body_json(ephemeral_resp).await["id"]
        .as_str()
        .expect("id field")
        .to_string();

    // Fork without TTL.
    let persistent_resp = build_router(env.state.clone())
        .oneshot(fork_req(serde_json::json!({"from_base": "main"}), &tok))
        .await
        .expect("oneshot persistent fork");
    assert_eq!(
        persistent_resp.status(),
        StatusCode::CREATED,
        "persistent fork must return 201"
    );
    let persistent_id = body_json(persistent_resp).await["id"]
        .as_str()
        .expect("id field")
        .to_string();

    // List and verify both records.
    let list_resp = build_router(env.state.clone())
        .oneshot(list_req(&tok))
        .await
        .expect("oneshot list");
    assert_eq!(list_resp.status(), StatusCode::OK);
    let workspaces = body_json(list_resp).await["workspaces"]
        .as_array()
        .expect("workspaces array")
        .clone();
    assert_eq!(
        workspaces.len(),
        2,
        "list must contain exactly two workspaces"
    );

    let ephemeral_ws = workspaces
        .iter()
        .find(|w| w["id"].as_str() == Some(&ephemeral_id))
        .expect("ephemeral workspace must appear in list");
    assert_eq!(
        ephemeral_ws["ttl_secs"]
            .as_u64()
            .expect("ttl_secs must be u64"),
        7200,
        "ttl_secs must round-trip as 7200"
    );
    assert_eq!(
        ephemeral_ws["state"].as_str().unwrap(),
        "ephemeral",
        "state must be ephemeral"
    );

    let persistent_ws = workspaces
        .iter()
        .find(|w| w["id"].as_str() == Some(&persistent_id))
        .expect("persistent workspace must appear in list");
    assert!(
        persistent_ws["ttl_secs"].is_null(),
        "ttl_secs must be null for persistent workspace"
    );
    assert_eq!(
        persistent_ws["state"].as_str().unwrap(),
        "persistent",
        "state must be persistent"
    );
}
