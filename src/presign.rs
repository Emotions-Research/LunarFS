// ---------------------------------------------------------------------------
// presign -- short-TTL URL seam so blob bytes bypass the axum handler
// ---------------------------------------------------------------------------
// Stub URL format (carried entirely in the URL string, no external state):
//   stub+local://BASE_DIR_HEX/EXPIRES_AT/OP/SIG_HEX/PCT_OBJECT_PATH
// where:
//   BASE_DIR_HEX = hex of the UTF-8 bytes of the LocalFileSystem root path
//   EXPIRES_AT   = decimal i64 unix seconds
//   OP           = "get" | "put"
//   SIG_HEX      = 64-char hex of blake3::keyed_hash(STUB_KEY, "{op}|{object_path}|{expires_at}")
//   PCT_OBJECT_PATH = object_path with '/' -> "%2F" and '%' -> "%25"
// base_dir is NOT signed; it is client-local state the test already trusts.
// ---------------------------------------------------------------------------

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use subtle::ConstantTimeEq as _;

// Fixed 32-byte signing key for stub URLs -- deterministic across processes.
// nyx: upgrade path is env-var override; for tests determinism is mandatory
const STUB_KEY: [u8; 32] = *b"lunarfs--stub-presign-key-v1!!!!";

pub const DEFAULT_PRESIGN_TTL_SECS: u64 = 300;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresignOp {
    Get,
    Put,
}

#[derive(Clone, Debug)]
pub struct PresignedUrl {
    pub url: String,
    pub method: String,
    pub expires_at: i64,
}

#[derive(Debug)]
pub enum PresignError {
    Expired,
    InvalidSignature,
    Malformed(String),
    Io(String),
}

impl std::fmt::Display for PresignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PresignError::Expired => write!(f, "presigned URL has expired"),
            PresignError::InvalidSignature => write!(f, "presigned URL signature is invalid"),
            PresignError::Malformed(s) => write!(f, "presigned URL malformed: {}", s),
            PresignError::Io(s) => write!(f, "presign I/O error: {}", s),
        }
    }
}

impl std::error::Error for PresignError {}

// ---------------------------------------------------------------------------
// Presigner trait
// ---------------------------------------------------------------------------

pub trait Presigner: Send + Sync {
    /// Mint a short-TTL presigned URL for `op` on the blob at `object_path`
    /// (e.g. "blobs/aa/<rest>"). `now_secs` is the current clock; expiry = now + ttl.
    fn presign(
        &self,
        op: PresignOp,
        object_path: &str,
        ttl_secs: u64,
        now_secs: i64,
    ) -> anyhow::Result<PresignedUrl>;
}

// ---------------------------------------------------------------------------
// Internal encoding helpers
// ---------------------------------------------------------------------------

fn op_str(op: PresignOp) -> &'static str {
    match op {
        PresignOp::Get => "get",
        PresignOp::Put => "put",
    }
}

fn op_from_str(s: &str) -> Option<PresignOp> {
    match s {
        "get" => Some(PresignOp::Get),
        "put" => Some(PresignOp::Put),
        _ => None,
    }
}

fn bytes_to_hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        s.push_str(&format!("{:02x}", byte));
    }
    s
}

fn hex_to_bytes(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for i in (0..s.len()).step_by(2) {
        let hi = u8::from_str_radix(&s[i..i + 1], 16).ok()?;
        let lo = u8::from_str_radix(&s[i + 1..i + 2], 16).ok()?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

// Encode only '/' and '%' so object_path survives as a single URL path component.
// Object paths are always `blobs/<2hex>/<62hex>` so only '/' needs encoding in practice.
fn pct_encode(s: &str) -> String {
    s.replace('%', "%25").replace('/', "%2F")
}

// Decode in reverse order of encoding to avoid double-processing.
fn pct_decode(s: &str) -> String {
    s.replace("%2F", "/").replace("%25", "%")
}

fn compute_stub_sig(op: PresignOp, object_path: &str, expires_at: i64) -> [u8; 32] {
    let msg = format!("{}|{}|{}", op_str(op), object_path, expires_at);
    *blake3::keyed_hash(&STUB_KEY, msg.as_bytes()).as_bytes()
}

fn build_stub_url(
    base_dir: &std::path::Path,
    op: PresignOp,
    object_path: &str,
    expires_at: i64,
) -> String {
    let sig = compute_stub_sig(op, object_path, expires_at);
    let base_hex = bytes_to_hex(base_dir.to_string_lossy().as_bytes());
    let sig_hex = bytes_to_hex(&sig);
    let pct = pct_encode(object_path);
    format!(
        "stub+local://{}/{}/{}/{}/{}",
        base_hex,
        expires_at,
        op_str(op),
        sig_hex,
        pct
    )
}

// ---------------------------------------------------------------------------
// LocalStubPresigner
// ---------------------------------------------------------------------------

pub struct LocalStubPresigner {
    base_dir: PathBuf,
}

impl LocalStubPresigner {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }
}

impl Presigner for LocalStubPresigner {
    fn presign(
        &self,
        op: PresignOp,
        object_path: &str,
        ttl_secs: u64,
        now_secs: i64,
    ) -> anyhow::Result<PresignedUrl> {
        assert!(!object_path.is_empty(), "object_path must not be empty");
        assert!(ttl_secs > 0, "ttl_secs must be positive");
        let expires_at = now_secs + ttl_secs as i64;
        let url = build_stub_url(&self.base_dir, op, object_path, expires_at);
        let method = match op {
            PresignOp::Get => "GET",
            PresignOp::Put => "PUT",
        };
        Ok(PresignedUrl {
            url,
            method: method.to_string(),
            expires_at,
        })
    }
}

// ---------------------------------------------------------------------------
// ResolvedStub + resolve_stub
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ResolvedStub {
    pub fs_path: PathBuf,
    pub op: PresignOp,
    pub expires_at: i64,
}

/// Validate a stub URL's signature and expiry; return the resolved local FS path.
/// Returns Expired if now_secs > expires_at, InvalidSignature on mismatch.
pub fn resolve_stub(url: &str, now_secs: i64) -> Result<ResolvedStub, PresignError> {
    assert!(!url.is_empty(), "url must not be empty");
    let rest = url
        .strip_prefix("stub+local://")
        .ok_or_else(|| PresignError::Malformed("missing stub+local:// scheme".to_string()))?;

    // Five segments split on '/' from the left: base_hex, expires, op, sig, pct_path.
    let mut parts = rest.splitn(5, '/');
    let base_hex = parts
        .next()
        .ok_or_else(|| PresignError::Malformed("missing base_dir segment".to_string()))?;
    let expires_str = parts
        .next()
        .ok_or_else(|| PresignError::Malformed("missing expires_at segment".to_string()))?;
    let op_s = parts
        .next()
        .ok_or_else(|| PresignError::Malformed("missing op segment".to_string()))?;
    let sig_hex = parts
        .next()
        .ok_or_else(|| PresignError::Malformed("missing sig segment".to_string()))?;
    let pct_path = parts
        .next()
        .ok_or_else(|| PresignError::Malformed("missing object_path segment".to_string()))?;

    let base_bytes = hex_to_bytes(base_hex)
        .ok_or_else(|| PresignError::Malformed("base_dir_hex is not valid hex".to_string()))?;
    let base_str = String::from_utf8(base_bytes)
        .map_err(|_| PresignError::Malformed("base_dir is not valid UTF-8".to_string()))?;

    let expires_at: i64 = expires_str
        .parse()
        .map_err(|_| PresignError::Malformed("expires_at is not a valid i64".to_string()))?;

    let op = op_from_str(op_s)
        .ok_or_else(|| PresignError::Malformed(format!("unknown op: {}", op_s)))?;

    let object_path = pct_decode(pct_path);

    // Verify signature in constant time before revealing expiry information.
    let expected = compute_stub_sig(op, &object_path, expires_at);
    let provided = hex_to_bytes(sig_hex)
        .ok_or_else(|| PresignError::Malformed("sig is not valid hex".to_string()))?;
    if provided.len() != 32 {
        return Err(PresignError::InvalidSignature);
    }
    let sig_ok = expected[..].ct_eq(provided.as_slice());
    if sig_ok.unwrap_u8() == 0 {
        return Err(PresignError::InvalidSignature);
    }

    // Check expiry after the signature to prevent timing oracles on the expiry field.
    if now_secs > expires_at {
        return Err(PresignError::Expired);
    }

    let fs_path = PathBuf::from(&base_str).join(&object_path);
    Ok(ResolvedStub {
        fs_path,
        op,
        expires_at,
    })
}

// ---------------------------------------------------------------------------
// R2Presigner (real S3/R2; only exercised by the gated smoke test)
// ---------------------------------------------------------------------------

pub struct R2Presigner {
    signer: Arc<dyn object_store::signer::Signer>,
}

impl R2Presigner {
    pub fn new(signer: Arc<dyn object_store::signer::Signer>) -> Self {
        Self { signer }
    }
}

impl Presigner for R2Presigner {
    fn presign(
        &self,
        op: PresignOp,
        object_path: &str,
        ttl_secs: u64,
        now_secs: i64,
    ) -> anyhow::Result<PresignedUrl> {
        assert!(!object_path.is_empty(), "object_path must not be empty");
        assert!(ttl_secs > 0, "ttl_secs must be positive");
        let method = match op {
            PresignOp::Get => reqwest::Method::GET,
            PresignOp::Put => reqwest::Method::PUT,
        };
        let path = object_store::path::Path::from(object_path);
        let expires_in = Duration::from_secs(ttl_secs);
        let signer = Arc::clone(&self.signer);
        // block_in_place moves this tokio worker thread out of the async context
        // so Handle::current().block_on() can drive the async signed_url call.
        // Requires rt-multi-thread (already enabled) and a live runtime handle.
        let signed = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(signer.signed_url(method, &path, expires_in))
        })
        .map_err(|e| anyhow::anyhow!("R2 presign failed: {}", e))?;
        let method_str = match op {
            PresignOp::Get => "GET",
            PresignOp::Put => "PUT",
        };
        Ok(PresignedUrl {
            url: signed.to_string(),
            method: method_str.to_string(),
            expires_at: now_secs + ttl_secs as i64,
        })
    }
}

// ---------------------------------------------------------------------------
// Client byte movers
// ---------------------------------------------------------------------------

/// Fetch bytes from a presigned URL. For stub+local:// URLs the bytes are read
/// directly from the local FS without transiting the axum handler.
pub async fn fetch_presigned(
    client: &reqwest::Client,
    url: &str,
    now_secs: i64,
) -> anyhow::Result<Vec<u8>> {
    assert!(!url.is_empty(), "url must not be empty");
    if url.starts_with("stub+local://") {
        let r = resolve_stub(url, now_secs).map_err(|e| anyhow::anyhow!("{}", e))?;
        let bytes = tokio::fs::read(&r.fs_path)
            .await
            .map_err(|e| anyhow::anyhow!("stub read {}: {}", r.fs_path.display(), e))?;
        return Ok(bytes);
    }
    let resp = client.get(url).send().await?;
    let status = resp.status();
    anyhow::ensure!(status.is_success(), "GET presigned URL returned {}", status);
    Ok(resp.bytes().await?.to_vec())
}

/// Write bytes to a presigned URL. For stub+local:// URLs the bytes are written
/// directly to the local FS, creating parent directories as needed.
pub async fn put_presigned(
    client: &reqwest::Client,
    url: &str,
    bytes: Vec<u8>,
    now_secs: i64,
) -> anyhow::Result<()> {
    assert!(!url.is_empty(), "url must not be empty");
    if url.starts_with("stub+local://") {
        let r = resolve_stub(url, now_secs).map_err(|e| anyhow::anyhow!("{}", e))?;
        if let Some(parent) = r.fs_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| anyhow::anyhow!("create_dir_all {}: {}", parent.display(), e))?;
        }
        tokio::fs::write(&r.fs_path, &bytes)
            .await
            .map_err(|e| anyhow::anyhow!("stub write {}: {}", r.fs_path.display(), e))?;
        return Ok(());
    }
    let resp = client.put(url).body(bytes).send().await?;
    let status = resp.status();
    anyhow::ensure!(status.is_success(), "PUT presigned URL returned {}", status);
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const OBJECT_PATH: &str =
        "blobs/aa/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn stub_presign_resolve_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = LocalStubPresigner::new(dir.path());
        let signed = p
            .presign(PresignOp::Put, OBJECT_PATH, 300, 1000)
            .expect("presign");
        assert_eq!(signed.method, "PUT");
        assert_eq!(signed.expires_at, 1300);

        let resolved = resolve_stub(&signed.url, 1100).expect("resolve_stub");
        assert_eq!(resolved.op, PresignOp::Put);
        assert_eq!(resolved.expires_at, 1300);
        assert_eq!(resolved.fs_path, dir.path().join(OBJECT_PATH));
    }

    #[tokio::test]
    async fn stub_put_then_fetch_byte_correct() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = LocalStubPresigner::new(dir.path());
        let now: i64 = 5000;
        let data = b"hello presign";

        let put_url = p
            .presign(PresignOp::Put, OBJECT_PATH, 300, now)
            .expect("presign put");
        let get_url = p
            .presign(PresignOp::Get, OBJECT_PATH, 300, now)
            .expect("presign get");

        let client = reqwest::Client::new();
        put_presigned(&client, &put_url.url, data.to_vec(), now)
            .await
            .expect("put_presigned");

        // Verify the file actually exists at the resolved FS path.
        let resolved = resolve_stub(&put_url.url, now).expect("resolve");
        assert!(
            resolved.fs_path.exists(),
            "blob must exist on disk at resolved path"
        );

        let got = fetch_presigned(&client, &get_url.url, now)
            .await
            .expect("fetch_presigned");
        assert_eq!(got, data, "bytes must round-trip byte-correct");
    }

    #[test]
    fn expired_stub_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = LocalStubPresigner::new(dir.path());
        let signed = p
            .presign(PresignOp::Get, OBJECT_PATH, 10, 1000)
            .expect("presign");
        // expires_at = 1010; resolve at now=2000 > 1010 must return Expired.
        let err = resolve_stub(&signed.url, 2000).expect_err("must be Expired");
        assert!(
            matches!(err, PresignError::Expired),
            "expected Expired, got: {}",
            err
        );
    }

    #[test]
    fn tampered_sig_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = LocalStubPresigner::new(dir.path());
        let signed = p
            .presign(PresignOp::Get, OBJECT_PATH, 300, 1000)
            .expect("presign");

        // Full URL split on '/': ["stub+local:", "", BASEHEX, EXPIRES, OP, SIG, PCT_PATH]
        // SIG is at index 5.
        let mut parts: Vec<String> = signed.url.split('/').map(|s| s.to_string()).collect();
        assert!(
            parts.len() >= 7,
            "URL must have at least 7 slash-segments, got {}",
            parts.len()
        );
        let last = parts[5].pop().unwrap_or('0');
        parts[5].push(if last == '0' { 'f' } else { '0' });
        let tampered = parts.join("/");

        let err = resolve_stub(&tampered, 1100).expect_err("must be InvalidSignature");
        assert!(
            matches!(err, PresignError::InvalidSignature),
            "expected InvalidSignature, got: {}",
            err
        );
    }

    #[test]
    fn tampered_object_path_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = LocalStubPresigner::new(dir.path());
        let signed = p
            .presign(PresignOp::Get, OBJECT_PATH, 300, 1000)
            .expect("presign");

        // PCT_PATH is at index 6 when splitting the full URL on '/'.
        let mut parts: Vec<String> = signed.url.split('/').map(|s| s.to_string()).collect();
        assert!(
            parts.len() >= 7,
            "URL must have at least 7 slash-segments, got {}",
            parts.len()
        );
        let last = parts[6].pop().unwrap_or('0');
        parts[6].push(if last == 'a' { 'b' } else { 'a' });
        let tampered = parts.join("/");

        let err = resolve_stub(&tampered, 1100).expect_err("must be InvalidSignature");
        assert!(
            matches!(err, PresignError::InvalidSignature),
            "expected InvalidSignature on tampered path, got: {}",
            err
        );
    }

    #[test]
    fn malformed_url_is_rejected() {
        let err = resolve_stub("stub+local://abc/123/get/sig", 0).expect_err("must be Malformed");
        assert!(
            matches!(err, PresignError::Malformed(_)),
            "expected Malformed, got: {}",
            err
        );
    }
}
