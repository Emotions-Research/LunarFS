use std::io;
use std::path::Path as StdPath;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use object_store::{
    path::Path as ObjPath, ObjectStore, PutMode, PutOptions, PutPayload, UpdateVersion,
};

use crate::cas::{hash_bytes, hash_to_hex, hex_to_hash, Hash, Store};
use crate::presign::{fetch_presigned, put_presigned, PresignOp, PresignedUrl};

// ---------------------------------------------------------------------------
// HttpRemote: v1 HTTP API client
// ---------------------------------------------------------------------------

pub struct HttpRemote {
    base_url: String,
    token: String,
    client: reqwest::Client,
}

impl HttpRemote {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            token: token.into(),
            client: reqwest::Client::new(),
        }
    }

    /// POST /v1/blobs/missing: returns the subset of `hashes` absent on the server.
    /// When `workspace` is Some, includes it as `?workspace=<name>` for ACL resolution.
    pub async fn missing_blobs(
        &self,
        hashes: &[Hash],
        workspace: Option<&str>,
    ) -> anyhow::Result<Vec<Hash>> {
        assert!(hashes.len() <= 65_536, "hash list exceeds 65536 entries");
        let hex_list: Vec<String> = hashes.iter().map(hash_to_hex).collect();
        let url = match workspace {
            Some(ws) => format!("{}/v1/blobs/missing?workspace={}", self.base_url, ws),
            None => format!("{}/v1/blobs/missing", self.base_url),
        };
        let resp = self
            .client
            .post(url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "hashes": hex_list }))
            .send()
            .await?;
        let status = resp.status();
        anyhow::ensure!(status.is_success(), "POST /v1/blobs/missing returned {}", status);
        let body: serde_json::Value = resp.json().await?;
        let arr = body["missing"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("missing field is not an array"))?;
        let mut result = Vec::with_capacity(arr.len());
        for v in arr {
            let hex =
                v.as_str().ok_or_else(|| anyhow::anyhow!("hash entry is not a string"))?;
            result.push(
                hex_to_hash(hex)
                    .map_err(|e| anyhow::anyhow!("bad hash in response: {}", e))?,
            );
        }
        Ok(result)
    }

    /// POST /v1/blob/:hash/presign?op=put|get: request a short-TTL presigned URL
    /// for off-server blob byte transfer. Non-2xx from the server returns Err so
    /// put_blob / get_blob can fall back to the direct endpoint.
    pub async fn presign(
        &self,
        hash: &Hash,
        op: PresignOp,
        workspace: Option<&str>,
    ) -> anyhow::Result<PresignedUrl> {
        assert_eq!(hash_to_hex(hash).len(), 64, "hash_to_hex must produce 64 chars");
        let url = presign_url(&self.base_url, &hash_to_hex(hash), op, workspace);
        let resp = self.client.post(&url).bearer_auth(&self.token).send().await?;
        let status = resp.status();
        anyhow::ensure!(status.is_success(), "POST presign {} returned {}", url, status);
        let body: serde_json::Value = resp.json().await?;
        let purl = body["url"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("presign response missing 'url'"))?
            .to_string();
        let method = body["method"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("presign response missing 'method'"))?
            .to_string();
        let expires_at = body["expires_at"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("presign response missing 'expires_at'"))?;
        Ok(PresignedUrl { url: purl, method, expires_at })
    }

    /// PUT /v1/blob/:hash: upload raw bytes for a known hash.
    /// Attempts off-server transfer via a presigned PUT URL first; falls back to
    /// transiting bytes through the server handler if presign is unavailable.
    pub async fn put_blob(
        &self,
        hash: &Hash,
        bytes: Vec<u8>,
        workspace: Option<&str>,
    ) -> anyhow::Result<()> {
        let hex = hash_to_hex(hash);
        assert_eq!(hex.len(), 64, "hash_to_hex must produce 64 chars");
        let now = unix_now_secs();
        // Clone before moving bytes into the presign attempt so fallback has them.
        let bytes_fallback = bytes.clone();
        let presign_ok = async {
            let p = self.presign(hash, PresignOp::Put, workspace).await?;
            put_presigned(&self.client, &p.url, bytes, now).await
        }
        .await;
        if presign_ok.is_ok() {
            return Ok(());
        }
        // Fallback: direct PUT through the server handler.
        let url = blob_url(&self.base_url, &hex, workspace);
        let resp = self
            .client
            .put(url)
            .bearer_auth(&self.token)
            .header("content-type", "application/octet-stream")
            .body(bytes_fallback)
            .send()
            .await?;
        let status = resp.status();
        anyhow::ensure!(status.is_success(), "PUT /v1/blob/{} returned {}", hex, status);
        Ok(())
    }

    /// GET /v1/blob/:hash: fetch raw bytes for a known hash.
    /// Attempts off-server transfer via a presigned GET URL first; falls back to
    /// transiting bytes through the server handler if presign is unavailable.
    pub async fn get_blob(&self, hash: &Hash, workspace: Option<&str>) -> anyhow::Result<Vec<u8>> {
        let hex = hash_to_hex(hash);
        assert_eq!(hex.len(), 64, "hash_to_hex must produce 64 chars");
        let now = unix_now_secs();
        let presign_result = async {
            let p = self.presign(hash, PresignOp::Get, workspace).await?;
            fetch_presigned(&self.client, &p.url, now).await
        }
        .await;
        if let Ok(bytes) = presign_result {
            return Ok(bytes);
        }
        // Fallback: direct GET through the server handler.
        let url = blob_url(&self.base_url, &hex, workspace);
        let resp = self.client.get(url).bearer_auth(&self.token).send().await?;
        let status = resp.status();
        anyhow::ensure!(status.is_success(), "GET /v1/blob/{} returned {}", hex, status);
        Ok(resp.bytes().await?.to_vec())
    }

    /// PUT /v1/ref/:workspace: publish root hash for a workspace.
    /// Legacy unconditional path (no expected_root). Use `put_ref_cas` from `lunar push`.
    pub async fn put_ref(&self, workspace: &str, root: &Hash) -> anyhow::Result<()> {
        assert!(!workspace.is_empty(), "workspace must not be empty");
        let resp = self
            .client
            .put(format!("{}/v1/ref/{}", self.base_url, workspace))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "root": hash_to_hex(root) }))
            .send()
            .await?;
        let status = resp.status();
        anyhow::ensure!(status.is_success(), "PUT /v1/ref/{} returned {}", workspace, status);
        Ok(())
    }

    /// PUT /v1/ref/:workspace with optional compare-and-swap.
    ///
    /// When `expected_root` is Some, the server only commits if its current root
    /// matches. On mismatch the server returns 409 with `{conflict_ref, current_root}`;
    /// this method surfaces a human-readable error message and exits non-zero (via
    /// the returned Err). When `expected_root` is None, behavior is identical to
    /// `put_ref` (unconditional last-writer-wins).
    pub async fn put_ref_cas(
        &self,
        workspace: &str,
        root: &Hash,
        expected_root: Option<&Hash>,
    ) -> anyhow::Result<()> {
        assert!(!workspace.is_empty(), "workspace must not be empty");
        let body = match expected_root {
            Some(exp) => serde_json::json!({
                "root": hash_to_hex(root),
                "expected_root": hash_to_hex(exp),
            }),
            None => serde_json::json!({ "root": hash_to_hex(root) }),
        };
        let resp = self
            .client
            .put(format!("{}/v1/ref/{}", self.base_url, workspace))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if status == reqwest::StatusCode::CONFLICT {
            let payload: serde_json::Value = resp.json().await.unwrap_or_default();
            let conflict_ref =
                payload["conflict_ref"].as_str().unwrap_or("<unknown>").to_string();
            let current_root =
                payload["current_root"].as_str().unwrap_or("<unknown>").to_string();
            anyhow::bail!(
                "push rejected: the server's root for workspace '{}' has changed since your last sync.\n\
                 Your push was saved as conflict ref '{}'.\n\
                 The server's current root is '{}'.\n\
                 Pull/merge and push again.",
                workspace,
                conflict_ref,
                current_root
            );
        }
        anyhow::ensure!(status.is_success(), "PUT /v1/ref/{} returned {}", workspace, status);
        Ok(())
    }

    /// GET /v1/ref/:workspace: fetch the current root hash for a workspace.
    pub async fn get_ref(&self, workspace: &str) -> anyhow::Result<Hash> {
        assert!(!workspace.is_empty(), "workspace must not be empty");
        let resp = self
            .client
            .get(format!("{}/v1/ref/{}", self.base_url, workspace))
            .bearer_auth(&self.token)
            .send()
            .await?;
        let status = resp.status();
        anyhow::ensure!(status.is_success(), "GET /v1/ref/{} returned {}", workspace, status);
        let body: serde_json::Value = resp.json().await?;
        let hex = body["root"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("root field missing or not a string"))?;
        hex_to_hash(hex).map_err(|e| anyhow::anyhow!("bad root hash from server: {}", e))
    }
}

// ---------------------------------------------------------------------------
// Pure URL-building helpers (no I/O; testable without a live server)
// ---------------------------------------------------------------------------

/// Build the presign endpoint URL for a blob hash and operation.
pub fn presign_url(base: &str, hex: &str, op: PresignOp, workspace: Option<&str>) -> String {
    assert_eq!(hex.len(), 64, "hex must be 64 chars");
    let op_str = match op {
        PresignOp::Get => "get",
        PresignOp::Put => "put",
    };
    match workspace {
        Some(ws) => format!("{}/v1/blob/{}/presign?op={}&workspace={}", base, hex, op_str, ws),
        None => format!("{}/v1/blob/{}/presign?op={}", base, hex, op_str),
    }
}

/// Build the direct blob endpoint URL (used as the fallback).
fn blob_url(base: &str, hex: &str, workspace: Option<&str>) -> String {
    assert_eq!(hex.len(), 64, "hex must be 64 chars");
    match workspace {
        Some(ws) => format!("{}/v1/blob/{}?workspace={}", base, hex, ws),
        None => format!("{}/v1/blob/{}", base, hex),
    }
}

/// Current unix time in seconds. Stub-minted URLs are always fresh so expiry
/// checks never trip on a just-minted URL.
fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[derive(Debug)]
pub enum RemoteError {
    Conflict,
    NotFound,
    Backend(String),
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RemoteError::Conflict => write!(f, "remote conflict: precondition failed"),
            RemoteError::NotFound => write!(f, "remote: object not found"),
            RemoteError::Backend(msg) => write!(f, "remote backend error: {}", msg),
        }
    }
}

impl std::error::Error for RemoteError {}

/// A resolved HEAD together with the version token needed for a later compare-and-swap.
#[derive(Debug)]
pub struct HeadPointer {
    pub root: Hash,
    pub version: UpdateVersion,
}

pub struct Remote {
    store: Arc<dyn ObjectStore>,
    rt: tokio::runtime::Runtime,
    workspace: String,
}

impl Remote {
    pub fn from_store(
        store: Arc<dyn ObjectStore>,
        workspace: impl Into<String>,
    ) -> io::Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(io::Error::other)?;
        Ok(Self {
            store,
            rt,
            workspace: workspace.into(),
        })
    }

    pub fn in_memory(workspace: &str) -> Self {
        let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        Self::from_store(store, workspace).expect("in_memory Remote creation must not fail")
    }

    pub fn local(dir: &StdPath, workspace: &str) -> io::Result<Self> {
        let store: Arc<dyn ObjectStore> = Arc::new(
            object_store::local::LocalFileSystem::new_with_prefix(dir)
                .map_err(|e| io::Error::other(e.to_string()))?,
        );
        Self::from_store(store, workspace)
    }

    fn blob_path(hash: &Hash) -> ObjPath {
        ObjPath::from(format!("cas/{}", hash_to_hex(hash)))
    }

    fn head_path(&self) -> ObjPath {
        ObjPath::from(format!("workspaces/{}/HEAD", self.workspace))
    }

    pub fn put_blob(&self, data: &[u8]) -> Result<Hash, RemoteError> {
        let hash = hash_bytes(data);
        let path = Self::blob_path(&hash);
        let payload = PutPayload::from(data.to_vec());
        let result = self.rt.block_on(self.store.put_opts(
            &path,
            payload,
            PutOptions { mode: PutMode::Create, ..Default::default() },
        ));
        match result {
            Ok(_) => Ok(hash),
            Err(object_store::Error::AlreadyExists { .. }) => Ok(hash), // idempotent
            Err(e) => Err(RemoteError::Backend(e.to_string())),
        }
    }

    pub fn get_blob(&self, hash: &Hash) -> Result<Option<Vec<u8>>, RemoteError> {
        let path = Self::blob_path(hash);
        self.rt.block_on(async {
            match self.store.get(&path).await {
                Ok(result) => {
                    let bytes = result
                        .bytes()
                        .await
                        .map_err(|e| RemoteError::Backend(e.to_string()))?;
                    Ok(Some(bytes.to_vec()))
                }
                Err(object_store::Error::NotFound { .. }) => Ok(None),
                Err(e) => Err(RemoteError::Backend(e.to_string())),
            }
        })
    }

    pub fn read_head(&self) -> Result<Option<HeadPointer>, RemoteError> {
        let path = self.head_path();
        self.rt.block_on(async {
            match self.store.get(&path).await {
                Err(object_store::Error::NotFound { .. }) => Ok(None),
                Err(e) => Err(RemoteError::Backend(e.to_string())),
                Ok(result) => {
                    let meta = result.meta.clone();
                    let bytes = result
                        .bytes()
                        .await
                        .map_err(|e| RemoteError::Backend(e.to_string()))?;
                    let hex = std::str::from_utf8(&bytes)
                        .map_err(|e| RemoteError::Backend(format!("HEAD not UTF-8: {}", e)))?;
                    let root = hex_to_hash(hex.trim())
                        .map_err(|e| RemoteError::Backend(format!("HEAD malformed: {}", e)))?;
                    let version = UpdateVersion {
                        e_tag: meta.e_tag,
                        version: meta.version,
                    };
                    Ok(Some(HeadPointer { root, version }))
                }
            }
        })
    }

    /// Write the root hash as HEAD for the first time (fails with Conflict if HEAD exists).
    pub fn init_head(&self, root: &Hash) -> Result<HeadPointer, RemoteError> {
        let path = self.head_path();
        let payload = PutPayload::from(hash_to_hex(root).into_bytes());
        let result = self.rt.block_on(self.store.put_opts(
            &path,
            payload,
            PutOptions { mode: PutMode::Create, ..Default::default() },
        ));
        match result {
            Ok(put_result) => Ok(HeadPointer {
                root: *root,
                version: UpdateVersion {
                    e_tag: put_result.e_tag,
                    version: put_result.version,
                },
            }),
            Err(object_store::Error::AlreadyExists { .. }) => Err(RemoteError::Conflict),
            Err(e) => Err(RemoteError::Backend(e.to_string())),
        }
    }

    /// Compare-and-swap HEAD; fails with Conflict if the stored version no longer matches expected.
    pub fn update_head(
        &self,
        root: &Hash,
        expected: &UpdateVersion,
    ) -> Result<HeadPointer, RemoteError> {
        let path = self.head_path();
        let payload = PutPayload::from(hash_to_hex(root).into_bytes());
        let result = self.rt.block_on(self.store.put_opts(
            &path,
            payload,
            PutOptions {
                mode: PutMode::Update(expected.clone()),
                ..Default::default()
            },
        ));
        match result {
            Ok(put_result) => Ok(HeadPointer {
                root: *root,
                version: UpdateVersion {
                    e_tag: put_result.e_tag,
                    version: put_result.version,
                },
            }),
            Err(object_store::Error::AlreadyExists { .. }) => Err(RemoteError::Conflict),
            Err(object_store::Error::Precondition { .. }) => Err(RemoteError::Conflict),
            Err(e) => Err(RemoteError::Backend(e.to_string())),
        }
    }
}

impl Store for Remote {
    fn put(&self, data: &[u8]) -> io::Result<Hash> {
        self.put_blob(data)
            .map_err(|e| io::Error::other(e.to_string()))
    }

    fn get(&self, hash: &Hash) -> io::Result<Option<Vec<u8>>> {
        self.get_blob(hash)
            .map_err(|e| io::Error::other(e.to_string()))
    }

    fn has(&self, hash: &Hash) -> bool {
        self.get_blob(hash).map(|r| r.is_some()).unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// RemoteBlobSource -- BlobSource backed by the in-process Remote object store
// ---------------------------------------------------------------------------

/// Fetches blob bytes from a Remote (in-memory or on-disk object_store) with
/// no real network involved. Used with HydratingStore to give local CAS stores
/// on-demand access to blobs held by the shared server.
pub struct RemoteBlobSource {
    remote: Arc<Remote>,
}

impl RemoteBlobSource {
    pub fn new(remote: Arc<Remote>) -> Self {
        assert!(Arc::strong_count(&remote) >= 1, "remote must be live");
        Self { remote }
    }
}

impl crate::cas::BlobSource for RemoteBlobSource {
    fn fetch_blob(&self, hash: &Hash) -> io::Result<Option<Vec<u8>>> {
        assert_eq!(hash.len(), 32, "hash must be 32 bytes");
        // Remote::get_blob returns Ok(None) when the blob is absent (not an error).
        self.remote.get_blob(hash).map_err(|e| io::Error::other(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::hash_bytes;
    use crate::index::Index;
    use crate::tree::{serialize_tree, TreeEntry, MODE_DIR, MODE_FILE};

    fn put_nested_tree(remote: &Remote) -> (Hash, Hash, Hash) {
        let h_main = remote.put(b"fn main() {}").unwrap();
        let h_lib = remote.put(b"pub fn hello() {}").unwrap();

        let src_bytes = serialize_tree(&[
            TreeEntry { mode: MODE_FILE, name: "main.rs".into(), hash: h_main },
            TreeEntry { mode: MODE_FILE, name: "lib.rs".into(), hash: h_lib },
        ]);
        let h_src_tree = remote.put(&src_bytes).unwrap();

        let h_readme = remote.put(b"# Dev Dropbox").unwrap();
        let root_bytes = serialize_tree(&[
            TreeEntry { mode: MODE_DIR, name: "src".into(), hash: h_src_tree },
            TreeEntry { mode: MODE_FILE, name: "README.md".into(), hash: h_readme },
        ]);
        let root = remote.put(&root_bytes).unwrap();
        (root, h_main, h_readme)
    }

    #[test]
    fn put_blob_get_blob_roundtrip() {
        let remote = Remote::in_memory("t1");
        let data = b"hello from object store";
        let hash = remote.put_blob(data).expect("put_blob must succeed");
        let got = remote.get_blob(&hash).expect("get_blob must succeed");
        assert_eq!(got, Some(data.to_vec()), "bytes must round-trip");
    }

    #[test]
    fn get_blob_absent_returns_none() {
        let remote = Remote::in_memory("t2");
        let got = remote.get_blob(&[0u8; 32]).expect("absent must not error");
        assert!(got.is_none());
    }

    #[test]
    fn put_blob_idempotent() {
        let remote = Remote::in_memory("t3");
        let data = b"idempotent data";
        let h1 = remote.put_blob(data).unwrap();
        let h2 = remote.put_blob(data).unwrap();
        assert_eq!(h1, h2, "repeated put must return same hash");
        assert_eq!(remote.get_blob(&h1).unwrap(), Some(data.to_vec()));
    }

    #[test]
    fn remote_implements_store_for_index_build() {
        let remote = Remote::in_memory("t4");
        let (root, h_main, h_readme) = put_nested_tree(&remote);

        let index = Index::build(&remote, &root).expect("Index::build must succeed");
        assert_eq!(index.len(), 3, "3 files expected");
        assert_eq!(index.lookup("src/main.rs"), Some(h_main));
        assert_eq!(index.lookup("README.md"), Some(h_readme));
        assert!(index.lookup("src").is_none(), "dir must not appear");
    }

    #[test]
    fn read_head_fresh_workspace_is_none() {
        let remote = Remote::in_memory("t5");
        let result = remote.read_head().expect("read_head must not error on fresh workspace");
        assert!(result.is_none());
    }

    #[test]
    fn init_head_then_read_head_round_trips() {
        let remote = Remote::in_memory("t6");
        let root = hash_bytes(b"the root hash");
        let ptr = remote.init_head(&root).expect("init_head must succeed");
        assert_eq!(ptr.root, root);

        let got = remote.read_head().unwrap().expect("read_head must return Some");
        assert_eq!(got.root, root);
    }

    #[test]
    fn init_head_twice_is_conflict() {
        let remote = Remote::in_memory("t7");
        let r1 = hash_bytes(b"root 1");
        let r2 = hash_bytes(b"root 2");
        remote.init_head(&r1).expect("first init must succeed");
        let err = remote.init_head(&r2).expect_err("second init must fail");
        assert!(matches!(err, RemoteError::Conflict), "expected Conflict, got: {:?}", err);
    }

    #[test]
    fn update_head_stale_version_is_conflict() {
        let remote = Remote::in_memory("t8");
        let r1 = hash_bytes(b"root 1");
        let r2 = hash_bytes(b"root 2");
        remote.init_head(&r1).unwrap();
        let stale = UpdateVersion { e_tag: Some("wrong-etag-xyz".to_string()), version: None };
        let err = remote.update_head(&r2, &stale).expect_err("stale CAS must fail");
        assert!(matches!(err, RemoteError::Conflict), "expected Conflict, got: {:?}", err);
    }

    #[test]
    fn update_head_current_version_succeeds() {
        let remote = Remote::in_memory("t9");
        let r1 = hash_bytes(b"root 1");
        let r2 = hash_bytes(b"root 2");
        let ptr1 = remote.init_head(&r1).unwrap();
        let ptr2 = remote.update_head(&r2, &ptr1.version).expect("CAS with current version must succeed");
        assert_eq!(ptr2.root, r2);
        let got = remote.read_head().unwrap().expect("HEAD must exist");
        assert_eq!(got.root, r2);
    }

    #[test]
    fn local_backend_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let remote = Remote::local(dir.path(), "local-ws").expect("local must succeed");
        let data = b"local backend data";
        let hash = remote.put_blob(data).unwrap();
        let got = remote.get_blob(&hash).unwrap();
        assert_eq!(got, Some(data.to_vec()));
    }

    // -----------------------------------------------------------------------
    // HttpRemote unit tests (no live server)
    // -----------------------------------------------------------------------

    #[test]
    fn presign_url_get_no_workspace() {
        let hex = "a".repeat(64);
        let url = presign_url("http://localhost:9000", &hex, PresignOp::Get, None);
        assert_eq!(url, format!("http://localhost:9000/v1/blob/{}/presign?op=get", hex));
    }

    #[test]
    fn presign_url_put_with_workspace() {
        let hex = "b".repeat(64);
        let url = presign_url("http://localhost:9000", &hex, PresignOp::Put, Some("my-ws"));
        assert_eq!(
            url,
            format!("http://localhost:9000/v1/blob/{}/presign?op=put&workspace=my-ws", hex)
        );
    }

    #[test]
    fn presign_url_get_op_differs_from_put() {
        let hex = "c".repeat(64);
        let get_url = presign_url("http://h", &hex, PresignOp::Get, Some("ws"));
        let put_url = presign_url("http://h", &hex, PresignOp::Put, Some("ws"));
        assert!(get_url.contains("op=get"), "get URL must contain op=get");
        assert!(put_url.contains("op=put"), "put URL must contain op=put");
        assert_ne!(get_url, put_url, "get and put URLs must differ");
    }

    // Verifies fallback behavior: against an unreachable port both presign and
    // the direct endpoint fail, so put_blob / get_blob return Err not panic.
    #[tokio::test]
    async fn put_blob_fallback_returns_err_on_unreachable_server() {
        let client = reqwest::ClientBuilder::new()
            .timeout(std::time::Duration::from_millis(200))
            .build()
            .expect("client build");
        let remote = HttpRemote {
            base_url: "http://127.0.0.1:1".to_string(),
            token: "tok".to_string(),
            client,
        };
        let hash = crate::cas::hash_bytes(b"unreachable");
        let result = remote.put_blob(&hash, b"unreachable".to_vec(), None).await;
        assert!(result.is_err(), "put_blob against unreachable port must return Err");
    }

    #[tokio::test]
    async fn get_blob_fallback_returns_err_on_unreachable_server() {
        let client = reqwest::ClientBuilder::new()
            .timeout(std::time::Duration::from_millis(200))
            .build()
            .expect("client build");
        let remote = HttpRemote {
            base_url: "http://127.0.0.1:1".to_string(),
            token: "tok".to_string(),
            client,
        };
        let hash = crate::cas::hash_bytes(b"unreachable");
        let result = remote.get_blob(&hash, None).await;
        assert!(result.is_err(), "get_blob against unreachable port must return Err");
    }
}
