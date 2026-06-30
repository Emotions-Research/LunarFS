use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::bail;
use axum::{
    body::Bytes,
    extract::{Extension, Path, Query, State},
    http::{header, HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, head, post, put},
    Json, Router,
};
use object_store::{
    path::Path as ObjPath, ObjectStore, PutMode, PutOptions, PutPayload, UpdateVersion,
};
use serde_json::json;

use crate::auth::acl::Principal as AclPrincipal;
use crate::auth::{
    acl::{self, Decision, Permission, PrincipalKind},
    repo, token, verify, OwnerKind, Role,
};
use crate::cas::{hash_bytes, hash_to_hex, hex_to_hash};

// ---------------------------------------------------------------------------
// AppState
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn ObjectStore>,
    pub db: Arc<std::sync::Mutex<rusqlite::Connection>>,
    pub verifier: Arc<dyn verify::Verifier>,
    pub clock: Arc<dyn token::Clock + Send + Sync>,
    pub presigner: Arc<dyn crate::presign::Presigner>,
    pub ws_backend: Arc<dyn crate::workspace::OverlayBackend>,
    pub ws_store: Arc<dyn crate::store::WorkspaceStore>,
    #[cfg(feature = "hosted")]
    pub billing: Arc<dyn crate::billing::provider::BillingProvider>,
    #[cfg(feature = "hosted")]
    pub webhook: Arc<dyn crate::billing::webhook::WebhookProvider>,
}

// ---------------------------------------------------------------------------
// Caller principal carrier (set by resolve_principal, read by handlers)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct CallerPrincipal(pub AclPrincipal);

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

pub enum ApiError {
    Unauthorized,
    Forbidden,
    BadRequest(String),
    BadHash,
    HashMismatch,
    NotFound,
    /// CAS mismatch: another writer advanced the ref first.
    /// The inner string is the current (winning) ref value.
    CasMismatch(String),
    Backend(String),
    /// Quota exceeded: storage cap, seat cap, or metered billing block (402).
    QuotaExceeded(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::CasMismatch(current) => (
                StatusCode::CONFLICT,
                Json(json!({"error": "cas_mismatch", "current": current})),
            )
                .into_response(),
            other => {
                let (status, msg) = match other {
                    ApiError::Unauthorized => {
                        (StatusCode::UNAUTHORIZED, "unauthorized".to_string())
                    }
                    ApiError::Forbidden => (StatusCode::FORBIDDEN, "forbidden".to_string()),
                    ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
                    ApiError::BadHash => {
                        (StatusCode::BAD_REQUEST, "invalid hash format".to_string())
                    }
                    ApiError::HashMismatch => (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "body does not match claimed hash".to_string(),
                    ),
                    ApiError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
                    ApiError::Backend(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
                    ApiError::QuotaExceeded(msg) => (StatusCode::PAYMENT_REQUIRED, msg),
                    ApiError::CasMismatch(_) => unreachable!("handled above"),
                };
                (status, Json(json!({"error": msg}))).into_response()
            }
        }
    }
}

#[cfg(feature = "hosted")]
impl From<crate::billing::entitlement::EntitlementError> for ApiError {
    fn from(e: crate::billing::entitlement::EntitlementError) -> Self {
        use crate::billing::entitlement::EntitlementError;
        match e {
            EntitlementError::StorageHardCapExceeded {
                limit_gb,
                attempted_gb,
            } => ApiError::QuotaExceeded(format!(
                "storage limit {}GB exceeded: write would reach {}GB",
                limit_gb, attempted_gb
            )),
            EntitlementError::SeatCapExceeded { limit, current } => {
                ApiError::QuotaExceeded(format!(
                    "seat cap of {} exceeded: org already has {} seat(s)",
                    limit, current
                ))
            }
            EntitlementError::FeatureNotInPlan { .. } => ApiError::Forbidden,
        }
    }
}

#[cfg(feature = "hosted")]
impl From<crate::billing::provider::BillingError> for ApiError {
    fn from(e: crate::billing::provider::BillingError) -> Self {
        use crate::billing::provider::BillingError;
        match e {
            BillingError::TierNotSelfServe(plan) => ApiError::BadRequest(format!(
                "{} tier has no self-serve checkout; contact sales",
                plan.as_str()
            )),
            BillingError::MissingApiKey => ApiError::Backend(
                "STRIPE_CONTEXT_FLAG: STRIPE_API_KEY not configured for this org".to_string(),
            ),
            BillingError::Unauthorized { account_context } => {
                let ctx = account_context.as_deref().unwrap_or("<none>");
                ApiError::Backend(format!(
                    "STRIPE_CONTEXT_FLAG: stripe 401 for account context {}; confirm STRIPE_API_KEY/account",
                    ctx
                ))
            }
            BillingError::Upstream(msg) => ApiError::Backend(msg),
        }
    }
}

// ---------------------------------------------------------------------------
// Object-store path helpers (byte-for-byte identical to Epic-1 layout)
// ---------------------------------------------------------------------------

fn blob_path(hex: &str) -> ObjPath {
    ObjPath::from(format!("blobs/{}/{}", &hex[..2], &hex[2..]))
}

fn ref_path(workspace: &str) -> ObjPath {
    ObjPath::from(format!("ref/{}", workspace))
}

// ---------------------------------------------------------------------------
// build_object_store -- the single resolution seam (preserved unchanged)
// ---------------------------------------------------------------------------

pub fn build_object_store(spec: &str) -> anyhow::Result<Arc<dyn ObjectStore>> {
    if let Some(path) = spec.strip_prefix("local:") {
        std::fs::create_dir_all(path)?;
        let store = object_store::local::LocalFileSystem::new_with_prefix(path)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        return Ok(Arc::new(store));
    }

    if let Some(bucket) = spec.strip_prefix("s3://") {
        let access_key = std::env::var("AWS_ACCESS_KEY_ID")
            .or_else(|_| std::env::var("S3_ACCESS_KEY_ID"))
            .unwrap_or_default();
        let secret = std::env::var("AWS_SECRET_ACCESS_KEY")
            .or_else(|_| std::env::var("S3_SECRET_ACCESS_KEY"))
            .unwrap_or_default();
        let endpoint = std::env::var("AWS_ENDPOINT")
            .or_else(|_| std::env::var("S3_ENDPOINT"))
            .or_else(|_| std::env::var("R2_ENDPOINT"))
            .ok();
        let region = std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("S3_REGION"))
            .unwrap_or_else(|_| "auto".to_string());

        let has_custom_endpoint = endpoint.is_some();
        let mut builder = object_store::aws::AmazonS3Builder::new()
            .with_bucket_name(bucket)
            .with_access_key_id(access_key)
            .with_secret_access_key(secret)
            .with_region(region);
        if let Some(ep) = endpoint {
            builder = builder.with_endpoint(ep);
        }
        if has_custom_endpoint {
            builder = builder.with_allow_http(true);
        }
        let store = builder
            .build()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        return Ok(Arc::new(store));
    }

    bail!(
        "unknown store spec {:?}: use local:<path> or s3://<bucket>",
        spec
    )
}

// ---------------------------------------------------------------------------
// build_presigner -- resolution seam mirroring build_object_store
// ---------------------------------------------------------------------------

pub fn build_presigner(spec: &str) -> anyhow::Result<Arc<dyn crate::presign::Presigner>> {
    if let Some(path) = spec.strip_prefix("local:") {
        return Ok(Arc::new(crate::presign::LocalStubPresigner::new(path)));
    }

    if let Some(bucket) = spec.strip_prefix("s3://") {
        let access_key = std::env::var("AWS_ACCESS_KEY_ID")
            .or_else(|_| std::env::var("S3_ACCESS_KEY_ID"))
            .unwrap_or_default();
        let secret = std::env::var("AWS_SECRET_ACCESS_KEY")
            .or_else(|_| std::env::var("S3_SECRET_ACCESS_KEY"))
            .unwrap_or_default();
        let endpoint = std::env::var("AWS_ENDPOINT")
            .or_else(|_| std::env::var("S3_ENDPOINT"))
            .or_else(|_| std::env::var("R2_ENDPOINT"))
            .ok();
        let region = std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("S3_REGION"))
            .unwrap_or_else(|_| "auto".to_string());

        let has_custom_endpoint = endpoint.is_some();
        let mut builder = object_store::aws::AmazonS3Builder::new()
            .with_bucket_name(bucket)
            .with_access_key_id(access_key)
            .with_secret_access_key(secret)
            .with_region(region);
        if let Some(ep) = endpoint {
            builder = builder.with_endpoint(ep);
        }
        if has_custom_endpoint {
            builder = builder.with_allow_http(true);
        }
        let store = builder
            .build()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        return Ok(Arc::new(crate::presign::R2Presigner::new(Arc::new(store))));
    }

    bail!(
        "unknown store spec {:?}: use local:<path> or s3://<bucket>",
        spec
    )
}

// ---------------------------------------------------------------------------
// Clock wrapper: satisfies &impl token::Clock for a dyn Clock trait object.
// token::Clock does not require Send+Sync so we cannot call validate with
// &*state.clock directly (unsized). This wrapper holds a reference and
// forwards now_secs, letting validate receive a Sized implementor.
// ---------------------------------------------------------------------------

struct ClockRef<'a>(&'a (dyn token::Clock + Send + Sync));

impl token::Clock for ClockRef<'_> {
    fn now_secs(&self) -> i64 {
        self.0.now_secs()
    }
}

// Bridges token::Clock (now_secs -> i64) to workspace::WsClock (now -> SystemTime).
// Constructed per-handler as a stack value; lifetime is bounded to the handler future.
struct WsClockFromToken<'a>(&'a (dyn token::Clock + Send + Sync));

impl crate::workspace::WsClock for WsClockFromToken<'_> {
    fn now(&self) -> std::time::SystemTime {
        std::time::UNIX_EPOCH + Duration::from_secs(self.0.now_secs().max(0) as u64)
    }
}

// ---------------------------------------------------------------------------
// Workspace extraction helpers for blob routes
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct WorkspaceQuery {
    workspace: Option<String>,
}

#[derive(serde::Deserialize)]
struct PresignQuery {
    workspace: Option<String>,
    op: Option<String>,
}

fn workspace_from_query_and_headers(
    q: &WorkspaceQuery,
    headers: &HeaderMap,
) -> Result<String, ApiError> {
    if let Some(ws) = &q.workspace {
        if !ws.is_empty() {
            return Ok(ws.clone());
        }
    }
    if let Some(val) = headers.get("x-workspace") {
        if let Ok(s) = val.to_str() {
            if !s.is_empty() {
                return Ok(s.to_string());
            }
        }
    }
    Err(ApiError::BadRequest(
        "workspace required: use ?workspace=<name> or X-Workspace header".to_string(),
    ))
}

// ---------------------------------------------------------------------------
// Authorization helpers
// ---------------------------------------------------------------------------

// Resolve workspace by name and enforce the ACL. Returns the workspace id on
// success; maps unknown-name to 404 and Deny to 403. Locks and releases the
// db mutex internally so the guard never crosses an await point.
fn authorize_op(
    state: &AppState,
    principal: &AclPrincipal,
    workspace_name: &str,
    needed: Permission,
) -> Result<i64, ApiError> {
    assert!(
        !workspace_name.is_empty(),
        "workspace_name must not be empty"
    );
    let conn = state
        .db
        .lock()
        .map_err(|_| ApiError::Backend("db mutex poisoned".to_string()))?;
    let ws_id = repo::workspace_by_name(&conn, workspace_name)
        .map_err(|e| ApiError::Backend(e.to_string()))?
        .ok_or(ApiError::NotFound)?;
    match acl::authorize(&conn, principal, ws_id, "/", needed)
        .map_err(|e| ApiError::Backend(e.to_string()))?
    {
        Decision::Allow => Ok(ws_id),
        Decision::Deny => Err(ApiError::Forbidden),
    }
}

// Return true if principal is the owner or an admin-role member of the
// workspace owner. False for all other cases (wrong principal, non-User
// principal, unknown workspace). Caller holds the conn lock.
fn is_workspace_admin(
    conn: &rusqlite::Connection,
    principal: &AclPrincipal,
    ws_id: i64,
) -> anyhow::Result<bool> {
    assert!(ws_id > 0, "ws_id must be a positive rowid");
    if principal.kind != PrincipalKind::User {
        return Ok(false);
    }
    match repo::workspace_owner(conn, ws_id)? {
        None => Ok(false),
        Some((OwnerKind::User, owner_id)) => Ok(principal.id == owner_id.to_string()),
        Some((OwnerKind::Org, org_id)) => {
            let user_id: i64 = match principal.id.parse() {
                Ok(id) => id,
                Err(_) => return Ok(false),
            };
            match repo::role_of(conn, user_id, org_id)? {
                Some(Role::Owner) | Some(Role::Admin) => Ok(true),
                _ => Ok(false),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// String conversions for principal kind and permission (ACL admin endpoints)
// ---------------------------------------------------------------------------

fn parse_principal_kind_str(s: &str) -> Option<PrincipalKind> {
    match s {
        "user" => Some(PrincipalKind::User),
        "org" => Some(PrincipalKind::Org),
        "token" => Some(PrincipalKind::Token),
        _ => None,
    }
}

fn parse_permission_str(s: &str) -> Option<Permission> {
    match s {
        "read" => Some(Permission::Read),
        "write" => Some(Permission::Write),
        _ => None,
    }
}

fn principal_kind_to_str(k: PrincipalKind) -> &'static str {
    match k {
        PrincipalKind::User => "user",
        PrincipalKind::Org => "org",
        PrincipalKind::Token => "token",
    }
}

fn permission_to_str(p: Permission) -> &'static str {
    match p {
        Permission::Read => "read",
        Permission::Write => "write",
    }
}

// ---------------------------------------------------------------------------
// Authentication middleware
// ---------------------------------------------------------------------------

async fn resolve_principal(
    State(state): State<AppState>,
    mut req: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, ApiError> {
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    // Guard empty bearer BEFORE calling token::validate (which asserts non-empty).
    if bearer.is_empty() {
        return Err(ApiError::Unauthorized);
    }

    // Try API-token path first.
    let token_result = {
        let conn = state
            .db
            .lock()
            .map_err(|_| ApiError::Backend("db mutex poisoned".to_string()))?;
        token::validate(&conn, bearer, &ClockRef(&*state.clock)).map(|p| {
            let kind = match p.kind {
                OwnerKind::User => PrincipalKind::User,
                OwnerKind::Org => PrincipalKind::Org,
            };
            AclPrincipal { kind, id: p.id }
        })
    };

    let principal = match token_result {
        Ok(p) => p,
        Err(_) => {
            // Fall through to JWT path; map any error to 401.
            match state.verifier.verify(bearer) {
                Ok(p) => AclPrincipal {
                    kind: PrincipalKind::User,
                    id: p.user_id,
                },
                Err(_) => return Err(ApiError::Unauthorized),
            }
        }
    };

    req.extensions_mut().insert(CallerPrincipal(principal));
    Ok(next.run(req).await)
}

// ---------------------------------------------------------------------------
// Request / response shapes
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct MissingRequest {
    hashes: Vec<String>,
}

#[derive(serde::Serialize)]
struct MissingResponse {
    missing: Vec<String>,
}

#[derive(serde::Deserialize)]
struct ForkRequest {
    new_workspace: String,
}

#[derive(serde::Deserialize)]
struct PutRefRequest {
    root: String,
    /// CAS expected value. None = first-write (create-if-absent); Some = conditional update.
    expected_root: Option<String>,
}

#[derive(serde::Deserialize)]
struct GrantRequest {
    principal_kind: String,
    principal_id: String,
    path_prefix: String,
    permission: String,
}

// ---------------------------------------------------------------------------
// Blob handlers
// ---------------------------------------------------------------------------

async fn put_blob(
    State(state): State<AppState>,
    Extension(caller): Extension<CallerPrincipal>,
    Query(ws_query): Query<WorkspaceQuery>,
    headers: HeaderMap,
    Path(hex): Path<String>,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let ws_name = workspace_from_query_and_headers(&ws_query, &headers)?;
    authorize_op(&state, &caller.0, &ws_name, Permission::Write)?;

    hex_to_hash(&hex).map_err(|_| ApiError::BadHash)?;
    let computed = hash_to_hex(&hash_bytes(&body));
    if computed != hex {
        return Err(ApiError::HashMismatch);
    }

    let path = blob_path(&hex);
    let payload = PutPayload::from(body.to_vec());
    let result = state
        .store
        .put_opts(
            &path,
            payload,
            PutOptions {
                mode: PutMode::Create,
                ..Default::default()
            },
        )
        .await;
    match result {
        Ok(_) | Err(object_store::Error::AlreadyExists { .. }) => {}
        Err(e) => return Err(ApiError::Backend(e.to_string())),
    }
    Ok((StatusCode::CREATED, Json(json!({"hash": hex}))))
}

async fn get_blob(
    State(state): State<AppState>,
    Extension(caller): Extension<CallerPrincipal>,
    Query(ws_query): Query<WorkspaceQuery>,
    headers: HeaderMap,
    Path(hex): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let ws_name = workspace_from_query_and_headers(&ws_query, &headers)?;
    authorize_op(&state, &caller.0, &ws_name, Permission::Read)?;

    hex_to_hash(&hex).map_err(|_| ApiError::BadHash)?;
    let path = blob_path(&hex);
    match state.store.get(&path).await {
        Ok(result) => {
            let bytes = result
                .bytes()
                .await
                .map_err(|e| ApiError::Backend(e.to_string()))?;
            let mut hdrs = HeaderMap::new();
            hdrs.insert(
                header::CONTENT_TYPE,
                "application/octet-stream"
                    .parse()
                    .expect("static header is valid"),
            );
            Ok((StatusCode::OK, hdrs, bytes.to_vec()).into_response())
        }
        Err(object_store::Error::NotFound { .. }) => Err(ApiError::NotFound),
        Err(e) => Err(ApiError::Backend(e.to_string())),
    }
}

async fn head_blob(
    State(state): State<AppState>,
    Extension(caller): Extension<CallerPrincipal>,
    Query(ws_query): Query<WorkspaceQuery>,
    headers: HeaderMap,
    Path(hex): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let ws_name = workspace_from_query_and_headers(&ws_query, &headers)?;
    authorize_op(&state, &caller.0, &ws_name, Permission::Read)?;

    hex_to_hash(&hex).map_err(|_| ApiError::BadHash)?;
    let path = blob_path(&hex);
    match state.store.head(&path).await {
        Ok(_) => Ok(StatusCode::OK.into_response()),
        Err(object_store::Error::NotFound { .. }) => Err(ApiError::NotFound),
        Err(e) => Err(ApiError::Backend(e.to_string())),
    }
}

async fn presign_blob(
    State(state): State<AppState>,
    Extension(caller): Extension<CallerPrincipal>,
    headers: HeaderMap,
    Path(hex): Path<String>,
    Query(q): Query<PresignQuery>,
) -> Result<impl IntoResponse, ApiError> {
    assert!(!hex.is_empty(), "hex path param must not be empty");
    let ws_name = {
        let ws_q = WorkspaceQuery {
            workspace: q.workspace.clone(),
        };
        workspace_from_query_and_headers(&ws_q, &headers)?
    };
    let op = match q.op.as_deref() {
        Some("get") => crate::presign::PresignOp::Get,
        Some("put") => crate::presign::PresignOp::Put,
        _ => {
            return Err(ApiError::BadRequest(
                "op must be 'get' or 'put'".to_string(),
            ))
        }
    };
    let needed = match op {
        crate::presign::PresignOp::Get => Permission::Read,
        crate::presign::PresignOp::Put => Permission::Write,
    };
    authorize_op(&state, &caller.0, &ws_name, needed)?;
    hex_to_hash(&hex).map_err(|_| ApiError::BadHash)?;
    let object_path = format!("blobs/{}/{}", &hex[..2], &hex[2..]);
    let p = state
        .presigner
        .presign(
            op,
            &object_path,
            crate::presign::DEFAULT_PRESIGN_TTL_SECS,
            state.clock.now_secs(),
        )
        .map_err(|e| ApiError::Backend(e.to_string()))?;
    Ok(Json(
        json!({"url": p.url, "method": p.method, "expires_at": p.expires_at}),
    ))
}

async fn post_blobs_missing(
    State(state): State<AppState>,
    Extension(caller): Extension<CallerPrincipal>,
    Query(ws_query): Query<WorkspaceQuery>,
    headers: HeaderMap,
    Json(req): Json<MissingRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let ws_name = workspace_from_query_and_headers(&ws_query, &headers)?;
    authorize_op(&state, &caller.0, &ws_name, Permission::Read)?;

    let mut missing = Vec::new();
    for hex in req.hashes {
        if hex.len() < 2 {
            missing.push(hex);
            continue;
        }
        let path = blob_path(&hex);
        match state.store.head(&path).await {
            Ok(_) => {}
            Err(object_store::Error::NotFound { .. }) => missing.push(hex),
            Err(e) => return Err(ApiError::Backend(e.to_string())),
        }
    }
    Ok(Json(MissingResponse { missing }))
}

// ---------------------------------------------------------------------------
// CAS decision helper (pure, no I/O)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum PutRefDecision {
    /// Proceed with the write: no expectation supplied, expectation matched, or
    /// no existing ref to conflict against (first push).
    Overwrite,
    /// CAS mismatch: save a conflict ref and return 409.
    /// `short_hash` is the first 8 hex chars of the incoming root.
    /// `current_root` is the server's current stored value.
    Conflict {
        short_hash: String,
        current_root: String,
    },
}

/// Pure CAS decision: no I/O, safe to unit-test without a running server.
///
/// `current`:  the server's stored root for the workspace (None = no ref yet).
/// `expected`: what the client believes is current (None = legacy, no expectation).
/// `incoming`: the root the client wants to install; must not be empty.
fn decide_put_ref(current: Option<&str>, expected: Option<&str>, incoming: &str) -> PutRefDecision {
    assert!(!incoming.is_empty(), "incoming root must not be empty");
    let Some(exp) = expected else {
        return PutRefDecision::Overwrite;
    };
    let Some(cur) = current else {
        // No existing ref: nothing to conflict against. Accept unconditionally so
        // that a first push with expected_root is not hostilely rejected.
        return PutRefDecision::Overwrite;
    };
    if cur == exp {
        PutRefDecision::Overwrite
    } else {
        // Roots are 64-char hex strings; 8 chars = 32-bit identity prefix.
        let short_hash = incoming[..8].to_string();
        PutRefDecision::Conflict {
            short_hash,
            current_root: cur.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Ref handlers
// ---------------------------------------------------------------------------

async fn put_ref(
    State(state): State<AppState>,
    Extension(caller): Extension<CallerPrincipal>,
    Path(workspace): Path<String>,
    Json(req): Json<PutRefRequest>,
) -> Result<Response, ApiError> {
    assert!(
        !workspace.is_empty(),
        "workspace path param must not be empty"
    );
    authorize_op(&state, &caller.0, &workspace, Permission::Write)?;

    if req.root.is_empty() {
        return Err(ApiError::BadRequest("root must not be empty".to_string()));
    }

    let path = ref_path(&workspace);

    match req.expected_root {
        None => {
            // Unconditional write (backward-compatible path: old clients omit expected_root).
            let bytes = serde_json::to_vec(&json!({"root": req.root}))
                .map_err(|e| ApiError::Backend(e.to_string()))?;
            state
                .store
                .put(&path, PutPayload::from(bytes))
                .await
                .map_err(|e| ApiError::Backend(e.to_string()))?;
            Ok(StatusCode::OK.into_response())
        }
        Some(ref expected) => {
            // CAS update: read current ref + etag, decide, then write or conflict.
            let current_read = state.store.get(&path).await;
            let (current_root_opt, etag, version) = match current_read {
                Err(object_store::Error::NotFound { .. }) => (None, None, None),
                Err(e) => return Err(ApiError::Backend(e.to_string())),
                Ok(get_result) => {
                    let etag = get_result.meta.e_tag.clone();
                    let version = get_result.meta.version.clone();
                    let bytes = get_result
                        .bytes()
                        .await
                        .map_err(|e| ApiError::Backend(e.to_string()))?;
                    let val: serde_json::Value = serde_json::from_slice(&bytes)
                        .map_err(|e| ApiError::Backend(e.to_string()))?;
                    let root = val["root"]
                        .as_str()
                        .ok_or_else(|| {
                            ApiError::Backend("current ref missing root field".to_string())
                        })?
                        .to_string();
                    (Some(root), etag, version)
                }
            };

            match decide_put_ref(current_root_opt.as_deref(), Some(expected), &req.root) {
                PutRefDecision::Overwrite => {
                    let new_bytes = serde_json::to_vec(&json!({"root": req.root}))
                        .map_err(|e| ApiError::Backend(e.to_string()))?;
                    if current_root_opt.is_none() {
                        // First push for this workspace: unconditional create.
                        state
                            .store
                            .put(&path, PutPayload::from(new_bytes))
                            .await
                            .map_err(|e| ApiError::Backend(e.to_string()))?;
                    } else if etag.is_some() || version.is_some() {
                        // CAS matched: use a conditional update guarded by the etag/version
                        // so a concurrent writer cannot silently overwrite between GET and PUT.
                        // Clone bytes so the NotImplemented fallback below can reuse them.
                        let upd_version = UpdateVersion {
                            e_tag: etag,
                            version,
                        };
                        let result = state
                            .store
                            .put_opts(
                                &path,
                                PutPayload::from(new_bytes.clone()),
                                PutOptions {
                                    mode: PutMode::Update(upd_version),
                                    ..Default::default()
                                },
                            )
                            .await;
                        match result {
                            Ok(_) => {}
                            Err(object_store::Error::AlreadyExists { .. })
                            | Err(object_store::Error::Precondition { .. }) => {
                                // Race: concurrent writer landed between our GET and PUT.
                                // Record the incoming root as a conflict ref and return 409.
                                let current = read_ref_root(&state.store, &path).await?;
                                return Ok(write_conflict_ref_response(
                                    &state.store,
                                    &workspace,
                                    &req.root,
                                    &req.root[..8],
                                    &current,
                                )
                                .await?);
                            }
                            Err(object_store::Error::NotImplemented) => {
                                // Backend (e.g. LocalFileSystem) does not support conditional
                                // writes. Content match already verified above; fall back to
                                // an unconditional write.
                                state
                                    .store
                                    .put(&path, PutPayload::from(new_bytes))
                                    .await
                                    .map_err(|e| ApiError::Backend(e.to_string()))?;
                            }
                            Err(e) => return Err(ApiError::Backend(e.to_string())),
                        }
                    } else {
                        // Backend provided no ETag or version.
                        // Content was already verified above; fall back to unconditional write.
                        state
                            .store
                            .put(&path, PutPayload::from(new_bytes))
                            .await
                            .map_err(|e| ApiError::Backend(e.to_string()))?;
                    }
                    Ok(StatusCode::OK.into_response())
                }
                PutRefDecision::Conflict {
                    short_hash,
                    current_root,
                } => Ok(write_conflict_ref_response(
                    &state.store,
                    &workspace,
                    &req.root,
                    &short_hash,
                    &current_root,
                )
                .await?),
            }
        }
    }
}

/// Store the incoming root under `<workspace>@conflict-<short_hash>` and return
/// a 409 response with `{conflict_ref, current_root}`.
async fn write_conflict_ref_response(
    store: &dyn ObjectStore,
    workspace: &str,
    incoming_root: &str,
    short_hash: &str,
    current_root: &str,
) -> Result<Response, ApiError> {
    assert!(!workspace.is_empty(), "workspace must not be empty");
    assert!(!short_hash.is_empty(), "short_hash must not be empty");
    let conflict_name = format!("{}@conflict-{}", workspace, short_hash);
    let conflict_path = ref_path(&conflict_name);
    let conflict_bytes = serde_json::to_vec(&json!({"root": incoming_root}))
        .map_err(|e| ApiError::Backend(e.to_string()))?;
    store
        .put(&conflict_path, PutPayload::from(conflict_bytes))
        .await
        .map_err(|e| ApiError::Backend(e.to_string()))?;
    Ok((
        StatusCode::CONFLICT,
        Json(json!({"conflict_ref": conflict_name, "current_root": current_root})),
    )
        .into_response())
}

/// Helper: read the "root" field from a stored ref object, or return an empty string on error.
async fn read_ref_root(store: &dyn ObjectStore, path: &ObjPath) -> Result<String, ApiError> {
    let result = store
        .get(path)
        .await
        .map_err(|e| ApiError::Backend(e.to_string()))?;
    let bytes = result
        .bytes()
        .await
        .map_err(|e| ApiError::Backend(e.to_string()))?;
    let val: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| ApiError::Backend(e.to_string()))?;
    Ok(val["root"].as_str().unwrap_or("").to_string())
}

async fn get_ref(
    State(state): State<AppState>,
    Extension(caller): Extension<CallerPrincipal>,
    Path(workspace): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    authorize_op(&state, &caller.0, &workspace, Permission::Read)?;

    let path = ref_path(&workspace);
    match state.store.get(&path).await {
        Ok(result) => {
            let bytes = result
                .bytes()
                .await
                .map_err(|e| ApiError::Backend(e.to_string()))?;
            let mut hdrs = HeaderMap::new();
            hdrs.insert(
                header::CONTENT_TYPE,
                "application/json".parse().expect("static header is valid"),
            );
            Ok((StatusCode::OK, hdrs, bytes.to_vec()).into_response())
        }
        Err(object_store::Error::NotFound { .. }) => Err(ApiError::NotFound),
        Err(e) => Err(ApiError::Backend(e.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Admin ACL endpoints
// ---------------------------------------------------------------------------

async fn post_workspace_acl(
    State(state): State<AppState>,
    Extension(caller): Extension<CallerPrincipal>,
    Path(workspace): Path<String>,
    Json(req): Json<GrantRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let principal_kind = parse_principal_kind_str(&req.principal_kind).ok_or_else(|| {
        ApiError::BadRequest(format!("invalid principal_kind: {}", req.principal_kind))
    })?;
    let permission = parse_permission_str(&req.permission)
        .ok_or_else(|| ApiError::BadRequest(format!("invalid permission: {}", req.permission)))?;
    if req.principal_id.is_empty() {
        return Err(ApiError::BadRequest(
            "principal_id must not be empty".to_string(),
        ));
    }

    let grant_id = {
        let conn = state
            .db
            .lock()
            .map_err(|_| ApiError::Backend("db mutex poisoned".to_string()))?;
        let ws_id = repo::workspace_by_name(&conn, &workspace)
            .map_err(|e| ApiError::Backend(e.to_string()))?
            .ok_or(ApiError::NotFound)?;
        if !is_workspace_admin(&conn, &caller.0, ws_id)
            .map_err(|e| ApiError::Backend(e.to_string()))?
        {
            return Err(ApiError::Forbidden);
        }
        let created_at = state.clock.now_secs();
        acl::grant(
            &conn,
            principal_kind,
            &req.principal_id,
            ws_id,
            &req.path_prefix,
            permission,
            created_at,
        )
        .map_err(|e| ApiError::Backend(e.to_string()))?
    };

    Ok((StatusCode::CREATED, Json(json!({"grant_id": grant_id}))))
}

async fn delete_workspace_acl(
    State(state): State<AppState>,
    Extension(caller): Extension<CallerPrincipal>,
    Path((workspace, grant_id)): Path<(String, i64)>,
) -> Result<impl IntoResponse, ApiError> {
    let conn = state
        .db
        .lock()
        .map_err(|_| ApiError::Backend("db mutex poisoned".to_string()))?;
    let ws_id = repo::workspace_by_name(&conn, &workspace)
        .map_err(|e| ApiError::Backend(e.to_string()))?
        .ok_or(ApiError::NotFound)?;
    if !is_workspace_admin(&conn, &caller.0, ws_id).map_err(|e| ApiError::Backend(e.to_string()))? {
        return Err(ApiError::Forbidden);
    }
    let revoked_at = state.clock.now_secs();
    acl::revoke(&conn, grant_id, revoked_at).map_err(|_| ApiError::NotFound)?;
    Ok(StatusCode::OK)
}

async fn get_workspace_acl(
    State(state): State<AppState>,
    Extension(caller): Extension<CallerPrincipal>,
    Path(workspace): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let grants = {
        let conn = state
            .db
            .lock()
            .map_err(|_| ApiError::Backend("db mutex poisoned".to_string()))?;
        let ws_id = repo::workspace_by_name(&conn, &workspace)
            .map_err(|e| ApiError::Backend(e.to_string()))?
            .ok_or(ApiError::NotFound)?;
        if !is_workspace_admin(&conn, &caller.0, ws_id)
            .map_err(|e| ApiError::Backend(e.to_string()))?
        {
            return Err(ApiError::Forbidden);
        }
        acl::list_for_workspace(&conn, ws_id).map_err(|e| ApiError::Backend(e.to_string()))?
    };

    let grant_list: Vec<serde_json::Value> = grants
        .iter()
        .map(|g| {
            json!({
                "id": g.id,
                "principal_kind": principal_kind_to_str(g.principal_kind),
                "principal_id": g.principal_id,
                "path_prefix": g.path_prefix,
                "permission": permission_to_str(g.permission),
                "created_at": g.created_at,
            })
        })
        .collect();
    Ok(Json(json!({"grants": grant_list})))
}

// ---------------------------------------------------------------------------
// Workspace fork handler
// ---------------------------------------------------------------------------

/// POST /v1/workspaces/:workspace/fork
///
/// O(1) fork: copies only the workspace-root ref (same root tree hash), creates a new
/// workspace DB record, and grants the caller Write access to the new workspace.
/// Zero blobs are copied; the new workspace shares the immutable base CAS.
async fn post_workspace_fork(
    State(state): State<AppState>,
    Extension(caller): Extension<CallerPrincipal>,
    Path(workspace): Path<String>,
    Json(req): Json<ForkRequest>,
) -> Result<impl IntoResponse, ApiError> {
    assert!(
        !workspace.is_empty(),
        "workspace path param must not be empty"
    );
    if req.new_workspace.is_empty() {
        return Err(ApiError::BadRequest(
            "new_workspace must not be empty".to_string(),
        ));
    }

    // Step 1: ACL -- caller must have at least Read on the base workspace.
    authorize_op(&state, &caller.0, &workspace, Permission::Read)?;

    // Step 2: Read base root hash from the object store (one GET, no blob iteration).
    let base_root = {
        let path = ref_path(&workspace);
        let result = state.store.get(&path).await.map_err(|e| match e {
            object_store::Error::NotFound { .. } => ApiError::NotFound,
            e => ApiError::Backend(e.to_string()),
        })?;
        let bytes = result
            .bytes()
            .await
            .map_err(|e| ApiError::Backend(e.to_string()))?;
        let val: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|e| ApiError::Backend(e.to_string()))?;
        val["root"]
            .as_str()
            .ok_or_else(|| ApiError::Backend("base ref missing root field".to_string()))?
            .to_string()
    };
    assert!(
        !base_root.is_empty(),
        "resolved base root hash must not be empty"
    );

    // Step 3: Write new workspace ref aliasing the same root hash (O(1): one PUT, zero blobs).
    {
        let new_path = ref_path(&req.new_workspace);
        let ref_bytes = serde_json::to_vec(&json!({"root": base_root}))
            .map_err(|e| ApiError::Backend(e.to_string()))?;
        state
            .store
            .put(&new_path, PutPayload::from(ref_bytes))
            .await
            .map_err(|e| ApiError::Backend(e.to_string()))?;
    }

    // Step 4: Create the new workspace DB record and grant the caller Write access (two INSERTs).
    {
        let conn = state
            .db
            .lock()
            .map_err(|_| ApiError::Backend("db mutex poisoned".to_string()))?;
        let owner_kind = match caller.0.kind {
            PrincipalKind::Org => OwnerKind::Org,
            _ => OwnerKind::User,
        };
        let owner_id: i64 = caller.0.id.parse().unwrap_or(0);
        let fork_ws_id = repo::create_workspace(
            &conn,
            &req.new_workspace,
            owner_kind,
            owner_id,
            state.clock.now_secs(),
        )
        .map_err(|e| ApiError::Backend(e.to_string()))?;
        acl::grant(
            &conn,
            caller.0.kind,
            &caller.0.id,
            fork_ws_id,
            "/",
            Permission::Write,
            state.clock.now_secs(),
        )
        .map_err(|e| ApiError::Backend(e.to_string()))?;
    }

    Ok((
        StatusCode::CREATED,
        Json(json!({"workspace": req.new_workspace, "root": base_root})),
    ))
}

// ---------------------------------------------------------------------------
// Workspace lifecycle endpoints (fork / list / destroy)
// ---------------------------------------------------------------------------

const DEFAULT_BASE_REF: &str = "main";

fn workspace_json(ws: &crate::workspace::Workspace) -> serde_json::Value {
    assert!(
        !ws.id.0.is_empty(),
        "workspace id must not be empty in workspace_json"
    );
    json!({
        "id": ws.id.0,
        "label": ws.label,
        "base_ref": ws.base_ref,
        "ttl_secs": ws.ttl.map(|d| d.as_secs()),
        "ephemeral": ws.ephemeral,
        "state": if ws.ephemeral { "ephemeral" } else { "persistent" },
        "created_at": crate::workspace::secs_since_epoch(ws.created_at),
        "metadata": ws.metadata,
    })
}

#[derive(serde::Deserialize)]
struct LifecycleForkRequest {
    label: Option<String>,
    from_base: Option<String>,
    ttl_secs: Option<u64>,
}

async fn post_workspace_lifecycle_fork(
    State(state): State<AppState>,
    Extension(caller): Extension<CallerPrincipal>,
    Json(req): Json<LifecycleForkRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let base_name = req
        .from_base
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_REF.to_string());
    assert!(!base_name.is_empty(), "base_name must not be empty");
    authorize_op(&state, &caller.0, &base_name, Permission::Read)?;
    let ws_clock = WsClockFromToken(&*state.clock);
    let spec = crate::workspace::WorkspaceSpec {
        base_ref: base_name,
        label: req.label,
        metadata: BTreeMap::new(),
        ttl: req.ttl_secs.map(Duration::from_secs),
        root: None,
    };
    let ws = crate::workspace::create_workspace(
        &*state.ws_backend,
        &*state.ws_store,
        &ws_clock,
        crate::workspace::new_ws_id(),
        spec,
    )
    .map_err(|e| ApiError::Backend(e.to_string()))?;
    Ok((StatusCode::CREATED, Json(workspace_json(&ws))))
}

async fn get_workspaces_list(
    State(state): State<AppState>,
    Extension(caller): Extension<CallerPrincipal>,
) -> Result<impl IntoResponse, ApiError> {
    let all = crate::workspace::list_workspaces(&*state.ws_store)
        .map_err(|e| ApiError::Backend(e.to_string()))?;
    assert!(all.len() <= 1_000_000, "workspace list exceeds sanity cap");
    let mut visible: Vec<serde_json::Value> = Vec::new();
    for ws in all {
        match authorize_op(&state, &caller.0, &ws.base_ref, Permission::Read) {
            Ok(_) => visible.push(workspace_json(&ws)),
            Err(ApiError::Forbidden) | Err(ApiError::NotFound) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(Json(json!({ "workspaces": visible })))
}

async fn delete_workspace_lifecycle(
    State(state): State<AppState>,
    Extension(caller): Extension<CallerPrincipal>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    assert!(!id.is_empty(), "workspace id path param must not be empty");
    let ws = state
        .ws_store
        .get(&crate::workspace::WsId(id.clone()))
        .map_err(|e| ApiError::Backend(e.to_string()))?
        .ok_or(ApiError::NotFound)?;
    authorize_op(&state, &caller.0, &ws.base_ref, Permission::Write)?;
    crate::workspace::destroy_workspace(
        &*state.ws_backend,
        &*state.ws_store,
        &crate::workspace::WsId(id),
    )
    .map_err(|e| ApiError::Backend(e.to_string()))?;
    Ok(StatusCode::OK)
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn build_router(state: AppState) -> Router {
    // All authenticated routes share the resolve_principal middleware layer.
    let authed = Router::new()
        .route("/v1/blob/:hash", put(put_blob))
        .route("/v1/blob/:hash", get(get_blob))
        .route("/v1/blob/:hash", head(head_blob))
        .route("/v1/blob/:hash/presign", post(presign_blob))
        .route("/v1/blobs/missing", post(post_blobs_missing))
        .route("/v1/ref/:workspace", put(put_ref))
        .route("/v1/ref/:workspace", get(get_ref))
        .route("/v1/workspaces/:workspace/acl", post(post_workspace_acl))
        .route("/v1/workspaces/:workspace/acl", get(get_workspace_acl))
        .route(
            "/v1/workspaces/:workspace/acl/:grant_id",
            delete(delete_workspace_acl),
        )
        .route("/v1/workspaces/:workspace/fork", post(post_workspace_fork))
        .route("/v1/workspace/fork", post(post_workspace_lifecycle_fork))
        .route("/v1/workspaces", get(get_workspaces_list))
        .route("/v1/workspace/:id", delete(delete_workspace_lifecycle));

    #[cfg(feature = "hosted")]
    let authed = authed
        .route(
            "/v1/billing/usage",
            get(crate::billing::metering::get_billing_usage),
        )
        .route(
            "/v1/billing/checkout",
            post(crate::billing::provider::post_checkout),
        )
        .route(
            "/v1/billing/portal",
            post(crate::billing::provider::post_portal),
        );

    let authed = authed.layer(middleware::from_fn_with_state(
        state.clone(),
        resolve_principal,
    ));

    // Webhook route is unauthenticated: Stripe authenticates via the signed payload.
    #[cfg(feature = "hosted")]
    let public = Router::new().route(
        "/v1/billing/webhook",
        post(crate::billing::webhook::post_billing_webhook),
    );
    #[cfg(not(feature = "hosted"))]
    let public = Router::new();

    authed.merge(public).with_state(state)
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

pub async fn run(state: AppState, addr: &str) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let router = build_router(state);
    axum::serve(listener, router).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests for the pure CAS decision helper
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{decide_put_ref, PutRefDecision};

    const ROOT_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const ROOT_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn no_expected_always_overwrites() {
        assert_eq!(
            decide_put_ref(Some(ROOT_A), None, ROOT_B),
            PutRefDecision::Overwrite
        );
        assert_eq!(
            decide_put_ref(None, None, ROOT_B),
            PutRefDecision::Overwrite
        );
    }

    #[test]
    fn first_push_with_expected_overwrites() {
        // No current ref: accept regardless of expected_root.
        assert_eq!(
            decide_put_ref(None, Some(ROOT_A), ROOT_B),
            PutRefDecision::Overwrite
        );
    }

    #[test]
    fn matching_expected_overwrites() {
        assert_eq!(
            decide_put_ref(Some(ROOT_A), Some(ROOT_A), ROOT_B),
            PutRefDecision::Overwrite
        );
    }

    #[test]
    fn mismatching_expected_produces_conflict() {
        let decision = decide_put_ref(Some(ROOT_A), Some(ROOT_B), ROOT_B);
        assert_eq!(
            decision,
            PutRefDecision::Conflict {
                short_hash: "bbbbbbbb".to_string(),
                current_root: ROOT_A.to_string(),
            }
        );
    }

    #[test]
    fn conflict_short_hash_is_first_8_chars_of_incoming() {
        let incoming = "1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";
        let decision = decide_put_ref(Some(ROOT_A), Some(ROOT_B), incoming);
        match decision {
            PutRefDecision::Conflict { short_hash, .. } => {
                assert_eq!(short_hash, "12345678");
            }
            PutRefDecision::Overwrite => panic!("expected Conflict"),
        }
    }
}
