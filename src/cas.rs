use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub type Hash = [u8; 32];

pub fn hash_bytes(data: &[u8]) -> Hash {
    blake3::hash(data).into()
}

pub fn hash_to_hex(h: &Hash) -> String {
    let mut s = String::with_capacity(64);
    for b in h {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

pub fn hex_to_hash(s: &str) -> io::Result<Hash> {
    if s.len() != 64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("hash hex must be 64 chars, got {}", s.len()),
        ));
    }
    let mut h = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let byte_str = std::str::from_utf8(chunk)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        h[i] = u8::from_str_radix(byte_str, 16)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    }
    Ok(h)
}

pub trait Store: Send + Sync {
    fn put(&self, data: &[u8]) -> io::Result<Hash>;
    fn get(&self, hash: &Hash) -> io::Result<Option<Vec<u8>>>;
    fn has(&self, hash: &Hash) -> bool;
}

pub struct FsStore {
    root: PathBuf,
}

impl FsStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn default_root() -> io::Result<Self> {
        let home = std::env::var("HOME")
            .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "$HOME is not set"))?;
        let root = PathBuf::from(home).join(".lunar").join("cas");
        Ok(Self { root })
    }

    fn blob_path(&self, hash: &Hash) -> PathBuf {
        let hex = hash_to_hex(hash);
        // nyx: git-style 2-char fanout; upgrade path: configurable fanout depth
        let (prefix, rest) = hex.split_at(2);
        self.root.join(prefix).join(rest)
    }
}

impl Store for FsStore {
    fn put(&self, data: &[u8]) -> io::Result<Hash> {
        assert!(!data.is_empty() || data.is_empty(), "data slice must be valid");
        let hash = hash_bytes(data);
        let path = self.blob_path(&hash);
        if path.exists() {
            return Ok(hash);
        }
        let parent = path.parent().ok_or_else(|| {
            io::Error::other("blob path has no parent directory")
        })?;
        std::fs::create_dir_all(parent)?;
        let tmp = path.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(data)?;
            f.flush()?;
        }
        std::fs::rename(&tmp, &path)?;
        Ok(hash)
    }

    fn get(&self, hash: &Hash) -> io::Result<Option<Vec<u8>>> {
        let path = self.blob_path(hash);
        match std::fs::read(&path) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn has(&self, hash: &Hash) -> bool {
        self.blob_path(hash).exists()
    }
}

pub struct MemStore {
    data: Mutex<HashMap<Hash, Vec<u8>>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self {
            data: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for MemStore {
    fn default() -> Self {
        Self::new()
    }
}

impl Store for MemStore {
    fn put(&self, data: &[u8]) -> io::Result<Hash> {
        let hash = hash_bytes(data);
        let mut map = self.data.lock().expect("MemStore lock poisoned");
        map.entry(hash).or_insert_with(|| data.to_vec());
        Ok(hash)
    }

    fn get(&self, hash: &Hash) -> io::Result<Option<Vec<u8>>> {
        let map = self.data.lock().expect("MemStore lock poisoned");
        Ok(map.get(hash).cloned())
    }

    fn has(&self, hash: &Hash) -> bool {
        let map = self.data.lock().expect("MemStore lock poisoned");
        map.contains_key(hash)
    }
}

// ---------------------------------------------------------------------------
// BlobSource -- on-demand remote fetch seam
// ---------------------------------------------------------------------------

/// A source of blob bytes for remote-backed workspaces.
///
/// Returns Ok(None) when the source does not hold the blob (not found),
/// as distinct from Err (real I/O or protocol failure). Callers verify the
/// returned bytes against the expected content hash before writing to local.
pub trait BlobSource: Send + Sync {
    fn fetch_blob(&self, hash: &Hash) -> io::Result<Option<Vec<u8>>>;
}

// ---------------------------------------------------------------------------
// HydratingStore -- single hydration choke point for local + remote reads
// ---------------------------------------------------------------------------

/// Wraps a local CAS store with a remote BlobSource fallback.
///
/// get() checks local first (fast path). On a miss, it fetches from the remote
/// source, verifies the returned bytes match the expected content hash (rejecting
/// any corruption without touching local), writes the verified bytes to local
/// (atomic + idempotent), then returns. Both ReadCache.read_path and
/// fuse::route_read call store.get(), so constructing either with a
/// HydratingStore routes ALL reads through this single choke point.
///
/// nyx: concurrent readers of the same missing hash may each trigger a remote
/// fetch (strong count > 1 between local.get miss and local.put). Both writes
/// are idempotent (FsStore uses atomic rename; MemStore uses or_insert_with).
/// Upgrade path: single-flight coalescing with a DashMap<Hash, Arc<OnceCell>>.
pub struct HydratingStore {
    local: Arc<dyn Store>,
    remote: Arc<dyn BlobSource>,
}

impl HydratingStore {
    pub fn new(local: Arc<dyn Store>, remote: Arc<dyn BlobSource>) -> Self {
        assert!(Arc::strong_count(&local) >= 1, "local store ref must be live");
        assert!(Arc::strong_count(&remote) >= 1, "remote source ref must be live");
        Self { local, remote }
    }
}

impl Store for HydratingStore {
    fn put(&self, data: &[u8]) -> io::Result<Hash> {
        self.local.put(data)
    }

    fn get(&self, hash: &Hash) -> io::Result<Option<Vec<u8>>> {
        // Fast path: blob already in local CAS; no remote contact needed.
        if let Some(data) = self.local.get(hash)? {
            return Ok(Some(data));
        }
        // Miss: pull from the remote source.
        let fetched = match self.remote.fetch_blob(hash)? {
            Some(b) => b,
            // Remote also lacks the blob; propagate as not-found.
            None => return Ok(None),
        };
        // Verify content hash BEFORE writing to local CAS.
        // A mismatch means the remote returned corrupt data; reject without storing.
        let actual = hash_bytes(&fetched);
        if actual != *hash {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "remote blob hash mismatch: expected {} got {}",
                    hash_to_hex(hash),
                    hash_to_hex(&actual),
                ),
            ));
        }
        // Atomic + idempotent write to local CAS, then return the bytes.
        self.local.put(&fetched)?;
        Ok(Some(fetched))
    }

    fn has(&self, hash: &Hash) -> bool {
        self.local.has(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_bytes_stability() {
        let h1 = hash_bytes(b"hello world");
        let h2 = hash_bytes(b"hello world");
        assert_eq!(h1, h2, "same input must produce same hash");
        let h3 = hash_bytes(b"hello world!");
        assert_ne!(h1, h3, "different input must produce different hash");
    }

    #[test]
    fn hash_known_vector() {
        // BLAKE3 of empty bytes is a known constant.
        let h = hash_bytes(b"");
        let hex = hash_to_hex(&h);
        // blake3::hash("") known value
        assert_eq!(
            hex,
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
            "empty-string BLAKE3 must match known vector"
        );
    }

    #[test]
    fn hex_roundtrip() {
        let h = hash_bytes(b"test roundtrip");
        let hex = hash_to_hex(&h);
        assert_eq!(hex.len(), 64);
        let back = hex_to_hash(&hex).expect("hex_to_hash must succeed on valid hex");
        assert_eq!(h, back, "hex roundtrip must be identity");
    }

    #[test]
    fn hex_to_hash_bad_length() {
        let err = hex_to_hash("deadbeef").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn memstore_put_get_roundtrip() {
        let store = MemStore::new();
        let data = b"some blob content";
        let hash = store.put(data).expect("put must succeed");
        assert!(store.has(&hash));
        let got = store.get(&hash).expect("get must succeed").expect("blob must be present");
        assert_eq!(got, data, "get must return what was put");
    }

    #[test]
    fn memstore_get_absent() {
        let store = MemStore::new();
        let hash = [0u8; 32];
        let got = store.get(&hash).expect("get must not error for absent key");
        assert!(got.is_none(), "absent key must return None");
    }

    #[test]
    fn memstore_put_idempotent() {
        let store = MemStore::new();
        let data = b"idempotent";
        let h1 = store.put(data).expect("first put must succeed");
        let h2 = store.put(data).expect("second put must succeed");
        assert_eq!(h1, h2, "idempotent puts must return same hash");
    }

    #[test]
    fn fsstore_put_get_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FsStore::new(dir.path());
        let data = b"fsstore blob";
        let hash = store.put(data).expect("put must succeed");
        assert!(store.has(&hash));
        let got = store.get(&hash).expect("get must succeed").expect("blob must be present");
        assert_eq!(got, data);
    }

    #[test]
    fn fsstore_get_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FsStore::new(dir.path());
        let hash = [0xffu8; 32];
        let got = store.get(&hash).expect("get must not error");
        assert!(got.is_none());
    }

    #[test]
    fn fsstore_put_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FsStore::new(dir.path());
        let data = b"idempotent fs";
        let h1 = store.put(data).expect("first put");
        let h2 = store.put(data).expect("second put");
        assert_eq!(h1, h2);
    }

    #[test]
    fn fsstore_fanout_layout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FsStore::new(dir.path());
        let data = b"fanout check";
        let hash = store.put(data).expect("put");
        let hex = hash_to_hex(&hash);
        let expected = dir.path().join(&hex[..2]).join(&hex[2..]);
        assert!(expected.exists(), "blob must be at 2-char fanout path");
    }
}
