//! Pen-test-style end-to-end adversarial smoke for LunarFS.
//!
//! Skips the entire test unless LUNAR_SMOKE=1 is set in the environment.
//! When LUNAR_SMOKE=1, the test starts the real lunar binary on an ephemeral
//! TCP port, pre-populates the identity DB with two principals and their
//! workspaces, then drives the four adversarial scenarios over real HTTP.
//!
//! Scenarios: authz-bypass, path-traversal, hash-spoof, acl-escape.
//! Each scenario includes at least one positive-control assertion so no
//! scenario can pass vacuously.
//!
//! Run: LUNAR_SMOKE=1 cargo test --test pen_smoke

use std::time::Duration;

use devdropbox::auth;
use devdropbox::auth::acl::{self, Permission, PrincipalKind};
use devdropbox::auth::repo;
use devdropbox::auth::token::{self, Principal, SystemClock};
use devdropbox::auth::OwnerKind;
use devdropbox::cas::{hash_bytes, hash_to_hex};
use tempfile::TempDir;

// Kill and reap the server process on every exit path, including panics.
struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// State shared across all four scenario checks within one test run.
struct PenEnv {
    base_url: String,
    client: reqwest::Client,
    tok_a_write: String, // write grant on ws-a
    tok_b_write: String, // write grant on ws-b
    tok_a_read: String,  // read-only grant on ws-a
    ro_grant_id: i64,    // rowid of the read-only grant (used for revocation)
    _guard: ChildGuard,
    _dir: TempDir,
}

// Pre-populate the identity DB before the binary starts.
// Returns (tok_a_write, tok_b_write, tok_a_read, ro_grant_id).
fn bootstrap_db(db_path: &std::path::Path) -> (String, String, String, i64) {
    let conn = auth::open(db_path).expect("pen_smoke: auth::open");
    let clock = SystemClock;

    // Principal A owns ws-a with write access.
    let uid_a = repo::create_user(&conn, None, 0).expect("create user A");
    let ws_a_id =
        repo::create_workspace(&conn, "ws-a", OwnerKind::User, uid_a, 0).expect("create ws-a");
    acl::grant(
        &conn,
        PrincipalKind::User,
        &uid_a.to_string(),
        ws_a_id,
        "/",
        Permission::Write,
        0,
    )
    .expect("grant write A on ws-a");
    let tok_a = token::mint(
        &conn,
        &Principal {
            kind: OwnerKind::User,
            id: uid_a.to_string(),
        },
        None,
        None,
        &clock,
    )
    .expect("mint tok_a")
    .plaintext;

    // Principal B owns ws-b with write access.
    let uid_b = repo::create_user(&conn, None, 0).expect("create user B");
    let ws_b_id =
        repo::create_workspace(&conn, "ws-b", OwnerKind::User, uid_b, 0).expect("create ws-b");
    acl::grant(
        &conn,
        PrincipalKind::User,
        &uid_b.to_string(),
        ws_b_id,
        "/",
        Permission::Write,
        0,
    )
    .expect("grant write B on ws-b");
    let tok_b = token::mint(
        &conn,
        &Principal {
            kind: OwnerKind::User,
            id: uid_b.to_string(),
        },
        None,
        None,
        &clock,
    )
    .expect("mint tok_b")
    .plaintext;

    // Read-only principal: separate user with only a read grant on ws-a so that
    // revoking the grant in acl_escape does not affect principal A.
    let uid_ro = repo::create_user(&conn, None, 0).expect("create read-only user");
    let ro_grant_id = acl::grant(
        &conn,
        PrincipalKind::User,
        &uid_ro.to_string(),
        ws_a_id,
        "/",
        Permission::Read,
        0,
    )
    .expect("read grant on ws-a");
    let tok_ro = token::mint(
        &conn,
        &Principal {
            kind: OwnerKind::User,
            id: uid_ro.to_string(),
        },
        None,
        None,
        &clock,
    )
    .expect("mint tok_ro")
    .plaintext;

    (tok_a, tok_b, tok_ro, ro_grant_id)
}

// Probe the server with a bounded retry loop.
// Returns true once any HTTP response is received (even 4xx).
// Bounded to 40 attempts x 150 ms = 6 s maximum wait.
async fn await_ready(client: &reqwest::Client, base: &str) -> bool {
    let probe = format!("{}/v1/ref/smoke-probe", base);
    for _ in 0..40usize {
        tokio::time::sleep(Duration::from_millis(150)).await;
        if client.get(&probe).send().await.is_ok() {
            return true;
        }
    }
    false
}

async fn start_pen_env() -> PenEnv {
    let dir = TempDir::new().expect("pen_smoke: tempdir");
    let store_dir = dir.path().join("objects");
    std::fs::create_dir_all(&store_dir).expect("pen_smoke: create store dir");
    let db_path = dir.path().join("pen.db");

    let (tok_a_write, tok_b_write, tok_a_read, ro_grant_id) = bootstrap_db(&db_path);

    // Bind an ephemeral port, record it, then drop so the child binary can rebind it.
    // There is a small TOCTOU window but it is acceptable for a local smoke test.
    let ephemeral = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("pen_smoke: bind ephemeral");
    let port = ephemeral.local_addr().expect("local_addr").port();
    drop(ephemeral);

    let addr = format!("127.0.0.1:{}", port);
    let base_url = format!("http://{}", addr);

    let bin = std::env::var("CARGO_BIN_EXE_lunar")
        .expect("pen_smoke: CARGO_BIN_EXE_lunar must be set by the cargo test harness");

    let child = std::process::Command::new(&bin)
        .args([
            "serve",
            "--store",
            &format!("local:{}", store_dir.display()),
            "--addr",
            &addr,
            "--db",
            &db_path.display().to_string(),
        ])
        .spawn()
        .expect("pen_smoke: spawn lunar binary");
    let guard = ChildGuard(child);

    let client = reqwest::Client::new();
    assert!(
        await_ready(&client, &base_url).await,
        "pen_smoke: server not ready within 6 s on {}",
        addr,
    );

    PenEnv {
        base_url,
        client,
        tok_a_write,
        tok_b_write,
        tok_a_read,
        ro_grant_id,
        _guard: guard,
        _dir: dir,
    }
}

// Scenario 1: authz-bypass
// Invariant: principal A cannot read or write principal B's workspace (no ACL grant).
// Invariant: a fabricated bearer token is rejected with 401.
async fn authz_bypass(env: &PenEnv) {
    let c = &env.client;
    let base = &env.base_url;
    let data = b"authz-bypass-probe-payload";
    let hex = hash_to_hex(&hash_bytes(data));

    // Positive control: A can PUT into ws-a.
    let own_put = c
        .put(format!("{}/v1/blob/{}?workspace=ws-a", base, hex))
        .bearer_auth(&env.tok_a_write)
        .header("content-type", "application/octet-stream")
        .body(data.to_vec())
        .send()
        .await
        .expect("own PUT");
    assert_eq!(
        own_put.status().as_u16(),
        201u16,
        "authz-bypass: A must PUT into ws-a (positive control)"
    );

    // Cross-principal write: A has no grant on ws-b.
    let cross_put = c
        .put(format!("{}/v1/blob/{}?workspace=ws-b", base, hex))
        .bearer_auth(&env.tok_a_write)
        .header("content-type", "application/octet-stream")
        .body(data.to_vec())
        .send()
        .await
        .expect("cross-principal PUT");
    assert_eq!(
        cross_put.status().as_u16(),
        403u16,
        "authz-bypass: A must not PUT into ws-b (no write grant)"
    );

    // Cross-principal read: A has no grant on ws-b.
    let cross_ref = c
        .get(format!("{}/v1/ref/ws-b", base))
        .bearer_auth(&env.tok_a_write)
        .send()
        .await
        .expect("cross-principal ref GET");
    assert_eq!(
        cross_ref.status().as_u16(),
        403u16,
        "authz-bypass: A must not read ws-b ref (no read grant)"
    );

    // Positive control: B can PUT into ws-b (confirms the workspace exists and B's grant is live).
    let b_put = c
        .put(format!("{}/v1/blob/{}?workspace=ws-b", base, hex))
        .bearer_auth(&env.tok_b_write)
        .header("content-type", "application/octet-stream")
        .body(data.to_vec())
        .send()
        .await
        .expect("B own PUT");
    assert_eq!(
        b_put.status().as_u16(),
        201u16,
        "authz-bypass: B must PUT into ws-b (positive control)"
    );

    // Under-scoped token: well-formed prefix but not present in the DB.
    let fake_tok = format!("ddb_{}", "0".repeat(64));
    let fake_get = c
        .get(format!("{}/v1/ref/ws-a", base))
        .bearer_auth(&fake_tok)
        .send()
        .await
        .expect("fabricated token GET");
    assert_eq!(
        fake_get.status().as_u16(),
        401u16,
        "authz-bypass: fabricated token must be 401"
    );
}

// Scenario 2: path-traversal
// Invariant: non-hex, too-short, and NUL-containing hash params are rejected (400).
// Invariant: traversal-pattern workspace names do not resolve to real workspaces.
async fn path_traversal(env: &PenEnv) {
    let c = &env.client;
    let base = &env.base_url;
    let valid_hex = "a".repeat(64);

    // Positive control: a valid 64-char hex hash is not rejected for format reasons.
    // The blob may be absent (404) but must not fail the format check (400).
    let valid_get = c
        .get(format!("{}/v1/blob/{}?workspace=ws-a", base, &valid_hex))
        .bearer_auth(&env.tok_a_write)
        .send()
        .await
        .expect("valid hex GET");
    assert_ne!(
        valid_get.status().as_u16(),
        400u16,
        "path-traversal: valid 64-char hex must not be 400 (positive control)"
    );

    // Hash shorter than 64 chars: length check in hex_to_hash must reject it.
    let short_hash = "a".repeat(16);
    let short_get = c
        .get(format!("{}/v1/blob/{}?workspace=ws-a", base, &short_hash))
        .bearer_auth(&env.tok_a_write)
        .send()
        .await
        .expect("short hash GET");
    assert_eq!(
        short_get.status().as_u16(),
        400u16,
        "path-traversal: hash shorter than 64 chars must be 400"
    );

    // Hash with non-hex characters: 'z' is not in [0-9a-fA-F].
    let non_hex = "z".repeat(64);
    let nonhex_get = c
        .get(format!("{}/v1/blob/{}?workspace=ws-a", base, &non_hex))
        .bearer_auth(&env.tok_a_write)
        .send()
        .await
        .expect("non-hex hash GET");
    assert_eq!(
        nonhex_get.status().as_u16(),
        400u16,
        "path-traversal: non-hex chars in hash param must be 400"
    );

    // NUL byte in hash: %00 is preserved by the URL layer and decoded by Axum to
    // a NUL byte in the path parameter string, which hex_to_hash rejects.
    // After decoding: 32 + NUL(1) + 31 = 64 chars, but NUL is not a hex digit.
    let nul_hash = format!("{}%00{}", "a".repeat(32), "a".repeat(31));
    let nul_get = c
        .get(format!("{}/v1/blob/{}?workspace=ws-a", base, nul_hash))
        .bearer_auth(&env.tok_a_write)
        .send()
        .await
        .expect("NUL hash GET");
    assert_eq!(
        nul_get.status().as_u16(),
        400u16,
        "path-traversal: NUL byte in hash param must be 400"
    );

    // Traversal workspace via query param: the name "../ws-b" is not in the DB.
    // Workspace lookup is an exact match; relative components cannot escape the namespace.
    let trav_ws = c
        .get(format!("{}/v1/blob/{}", base, &valid_hex))
        .query(&[("workspace", "../ws-b")])
        .bearer_auth(&env.tok_a_write)
        .send()
        .await
        .expect("traversal workspace query GET");
    let trav_status = trav_ws.status().as_u16();
    assert!(
        trav_status == 404 || trav_status == 400,
        "path-traversal: traversal workspace query must not resolve (got {})",
        trav_status,
    );

    // Absolute path as workspace query param: "/etc/passwd" is not in the DB.
    let abs_get = c
        .get(format!("{}/v1/blob/{}", base, &valid_hex))
        .query(&[("workspace", "/etc/passwd")])
        .bearer_auth(&env.tok_a_write)
        .send()
        .await
        .expect("absolute workspace query GET");
    let abs_status = abs_get.status().as_u16();
    assert!(
        abs_status == 404 || abs_status == 400,
        "path-traversal: absolute workspace must not grant access (got {})",
        abs_status,
    );
}

// Scenario 3: hash-spoof
// Invariant: PUT where body bytes do not match the claimed BLAKE3 hash returns 422.
// Invariant: a correctly-hashed body is accepted (201).
async fn hash_spoof(env: &PenEnv) {
    let c = &env.client;
    let base = &env.base_url;

    // Positive control: correct hash matched to the correct body.
    let good_data = b"hash-spoof-positive-payload";
    let good_hex = hash_to_hex(&hash_bytes(good_data));
    let good_put = c
        .put(format!("{}/v1/blob/{}?workspace=ws-a", base, good_hex))
        .bearer_auth(&env.tok_a_write)
        .header("content-type", "application/octet-stream")
        .body(good_data.to_vec())
        .send()
        .await
        .expect("good blob PUT");
    assert_eq!(
        good_put.status().as_u16(),
        201u16,
        "hash-spoof: correctly-hashed blob must be 201 (positive control)"
    );

    // Spoof: claim good_hex but send different bytes.
    let bad_data = b"this-is-not-the-hashed-content";
    let spoof_put = c
        .put(format!("{}/v1/blob/{}?workspace=ws-a", base, good_hex))
        .bearer_auth(&env.tok_a_write)
        .header("content-type", "application/octet-stream")
        .body(bad_data.to_vec())
        .send()
        .await
        .expect("spoofed blob PUT");
    assert_eq!(
        spoof_put.status().as_u16(),
        422u16,
        "hash-spoof: claimed hash vs mismatched body must be 422"
    );

    // Reverse spoof: send good_data but claim the hash of bad_data.
    let bad_hex = hash_to_hex(&hash_bytes(bad_data));
    let rev_put = c
        .put(format!("{}/v1/blob/{}?workspace=ws-a", base, bad_hex))
        .bearer_auth(&env.tok_a_write)
        .header("content-type", "application/octet-stream")
        .body(good_data.to_vec())
        .send()
        .await
        .expect("reverse spoof PUT");
    assert_eq!(
        rev_put.status().as_u16(),
        422u16,
        "hash-spoof: correct body with wrong claimed hash must be 422"
    );
}

// Scenario 4: acl-escape
// Invariant: a read-only grant cannot write (403 on PUT).
// Invariant: after revocation via the ACL admin API, GET is also denied (403).
async fn acl_escape(env: &PenEnv) {
    let c = &env.client;
    let base = &env.base_url;

    // Put a blob as the write owner so there is something for the read-only token to GET.
    let data = b"acl-escape-blob-content";
    let hex = hash_to_hex(&hash_bytes(data));
    let owner_put = c
        .put(format!("{}/v1/blob/{}?workspace=ws-a", base, hex))
        .bearer_auth(&env.tok_a_write)
        .header("content-type", "application/octet-stream")
        .body(data.to_vec())
        .send()
        .await
        .expect("owner PUT");
    assert_eq!(
        owner_put.status().as_u16(),
        201u16,
        "acl-escape: owner must PUT into ws-a (positive control)"
    );

    // Positive control: read-only token can GET the blob.
    let ro_get = c
        .get(format!("{}/v1/blob/{}?workspace=ws-a", base, hex))
        .bearer_auth(&env.tok_a_read)
        .send()
        .await
        .expect("read-only GET");
    assert_eq!(
        ro_get.status().as_u16(),
        200u16,
        "acl-escape: read-only grant must allow GET (positive control)"
    );

    // Escape attempt: read-only token tries to PUT. Must be 403.
    let escape_data = b"read-only-escape-attempt-data";
    let escape_hex = hash_to_hex(&hash_bytes(escape_data));
    let ro_put = c
        .put(format!("{}/v1/blob/{}?workspace=ws-a", base, escape_hex))
        .bearer_auth(&env.tok_a_read)
        .header("content-type", "application/octet-stream")
        .body(escape_data.to_vec())
        .send()
        .await
        .expect("read-only PUT attempt");
    assert_eq!(
        ro_put.status().as_u16(),
        403u16,
        "acl-escape: read-only grant must not allow PUT (403)"
    );

    // Revoke the read-only grant via the ACL admin endpoint as workspace owner.
    let del_url = format!("{}/v1/workspaces/ws-a/acl/{}", base, env.ro_grant_id);
    let revoke = c
        .delete(&del_url)
        .bearer_auth(&env.tok_a_write)
        .send()
        .await
        .expect("revoke grant DELETE");
    assert_eq!(
        revoke.status().as_u16(),
        200u16,
        "acl-escape: workspace owner must revoke the read grant (200)"
    );

    // After revocation, the same GET must now be denied.
    let post_revoke = c
        .get(format!("{}/v1/blob/{}?workspace=ws-a", base, hex))
        .bearer_auth(&env.tok_a_read)
        .send()
        .await
        .expect("GET after revoke");
    assert_eq!(
        post_revoke.status().as_u16(),
        403u16,
        "acl-escape: revoked grant must deny GET (403)"
    );
}

// Gate: skips cleanly when LUNAR_SMOKE is unset or not "1".
// When LUNAR_SMOKE=1: starts the real binary, runs all four scenarios, tears down.
#[tokio::test]
async fn pen_smoke_adversarial() {
    if std::env::var("LUNAR_SMOKE").as_deref() != Ok("1") {
        return;
    }

    let env = start_pen_env().await;
    authz_bypass(&env).await;
    path_traversal(&env).await;
    hash_spoof(&env).await;
    acl_escape(&env).await;

    eprintln!("pen_smoke: all four adversarial scenarios passed");
}
