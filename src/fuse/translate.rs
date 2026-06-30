//! FFI-free translation layer between the VFS path space and the CAS-backed core.
//!
//! All three public functions are parameterised over IndexSeam / StoreSeam traits
//! so deterministic in-memory fakes can be injected in tests without any mount,
//! FFI, or network.

use crate::cas::Hash;
use std::collections::HashSet;

// ---------------------------------------------------------------------------
// Seam traits
// ---------------------------------------------------------------------------

/// Resolves virtual paths to node metadata. Only files live in the real Index;
/// directories are inferred by prefix scanning via `file_paths`.
pub trait IndexSeam {
    fn lookup(&self, path: &str) -> Option<NodeMeta>;
    /// All file paths present (used to infer directory existence + children).
    fn file_paths(&self) -> Vec<String>;
}

/// Lazily hydrates content bytes for a CAS hash. Callers track call counts
/// (via a wrapping fake) to assert exactly-once hydration.
pub trait StoreSeam {
    fn hydrate(&self, hash: &Hash) -> Option<Vec<u8>>;
}

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Dir,
}

#[derive(Debug, Clone)]
pub struct NodeMeta {
    pub kind: NodeKind,
    pub size: u64,
    pub mode: u32,
    pub hash: Option<Hash>,
}

#[derive(Debug, Clone)]
pub struct Attributes {
    pub kind: NodeKind,
    pub size: u64,
    pub mode: u32,
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub kind: NodeKind,
}

#[derive(Debug)]
pub enum FsError {
    NotFound,
    NotADir,
    IsADir,
    IoError(String),
    AccessDenied,
}

// ---------------------------------------------------------------------------
// errno mapping (safe; libc constants are plain integers)
// ---------------------------------------------------------------------------

pub fn errno_for(e: &FsError) -> i32 {
    match e {
        FsError::NotFound => -libc::ENOENT,
        FsError::NotADir => -libc::ENOTDIR,
        FsError::IsADir => -libc::EISDIR,
        FsError::IoError(_) => -libc::EIO,
        FsError::AccessDenied => -libc::EACCES,
    }
}

// ---------------------------------------------------------------------------
// Internal helper: resolve a path to a NodeMeta (file OR inferred dir).
// ---------------------------------------------------------------------------

fn resolve_path(index: &dyn IndexSeam, path: &str) -> Result<NodeMeta, FsError> {
    assert!(path.len() <= 4096, "path must not exceed 4096 bytes");

    // Fast path: direct file lookup.
    if let Some(meta) = index.lookup(path) {
        return Ok(meta);
    }

    // Root is always a directory.
    if path.is_empty() {
        return Ok(NodeMeta {
            kind: NodeKind::Dir,
            size: 0,
            mode: 0o040755,
            hash: None,
        });
    }

    // Check whether any file lives under path/.
    let prefix = format!("{}/", path);
    let exists = index.file_paths().iter().any(|p| p.starts_with(&prefix));
    if exists {
        Ok(NodeMeta {
            kind: NodeKind::Dir,
            size: 0,
            mode: 0o040755,
            hash: None,
        })
    } else {
        Err(FsError::NotFound)
    }
}

// ---------------------------------------------------------------------------
// Translation functions (public API, 100% safe Rust)
// ---------------------------------------------------------------------------

/// Returns stat-level attributes for the node at `path`.
/// Missing path => Err(FsError::NotFound).
pub fn attr_for(index: &dyn IndexSeam, path: &str) -> Result<Attributes, FsError> {
    let meta = resolve_path(index, path)?;
    Ok(Attributes {
        kind: meta.kind,
        size: meta.size,
        mode: meta.mode,
    })
}

/// Returns a clamped byte slice from the file at `path`.
///
/// - offset >= file length => empty Vec (not an error).
/// - offset+size > file length => clamped to EOF.
/// - path is a directory => Err(FsError::IsADir).
/// - hash missing from store => Err(FsError::IoError).
/// - hydrate() is called exactly once per invocation.
pub fn read_slice(
    index: &dyn IndexSeam,
    store: &dyn StoreSeam,
    path: &str,
    offset: u64,
    size: u32,
) -> Result<Vec<u8>, FsError> {
    assert!(path.len() <= 4096, "path must not exceed 4096 bytes");
    assert!(
        size as u64 <= 128 * 1024 * 1024,
        "read size must be <= 128 MiB"
    );

    let meta = resolve_path(index, path)?;
    if meta.kind == NodeKind::Dir {
        return Err(FsError::IsADir);
    }
    let hash = meta
        .hash
        .ok_or_else(|| FsError::IoError(format!("no hash for {}", path)))?;
    let bytes = store
        .hydrate(&hash)
        .ok_or_else(|| FsError::IoError(format!("blob missing from store: {}", path)))?;

    let len = bytes.len() as u64;
    if offset >= len || size == 0 {
        return Ok(Vec::new());
    }
    let start = offset as usize;
    let end = ((offset + u64::from(size)).min(len)) as usize;
    Ok(bytes[start..end].to_vec())
}

/// Returns the direct children of the directory at `path` (plus `.` and `..`).
///
/// - path is a file => Err(FsError::NotADir).
/// - path not found => Err(FsError::NotFound).
pub fn read_dir(index: &dyn IndexSeam, path: &str) -> Result<Vec<DirEntry>, FsError> {
    assert!(path.len() <= 4096, "path must not exceed 4096 bytes");

    // Validate: must be a directory, not a file or missing.
    if !path.is_empty() {
        if index.lookup(path).is_some() {
            return Err(FsError::NotADir);
        }
        let prefix = format!("{}/", path);
        let has_children = index.file_paths().iter().any(|p| p.starts_with(&prefix));
        if !has_children {
            return Err(FsError::NotFound);
        }
    }

    let prefix: String = if path.is_empty() {
        String::new()
    } else {
        format!("{}/", path)
    };

    let mut seen: HashSet<String> = HashSet::new();
    let mut entries: Vec<DirEntry> = vec![
        DirEntry {
            name: ".".to_owned(),
            kind: NodeKind::Dir,
        },
        DirEntry {
            name: "..".to_owned(),
            kind: NodeKind::Dir,
        },
    ];
    seen.insert(".".to_owned());
    seen.insert("..".to_owned());

    for file_path in index.file_paths() {
        // Strip the directory prefix; skip entries that don't belong here.
        let rest: &str = if prefix.is_empty() {
            file_path.as_str()
        } else {
            match file_path.strip_prefix(&prefix) {
                Some(r) => r,
                None => continue,
            }
        };

        // The immediate child name is everything up to the first '/'.
        let component: &str = match rest.split('/').next() {
            Some(c) if !c.is_empty() => c,
            _ => continue,
        };

        if seen.contains(component) {
            continue;
        }
        seen.insert(component.to_owned());

        // If rest has a '/', the child is a subdirectory; otherwise it is a file.
        let kind = if rest.contains('/') {
            NodeKind::Dir
        } else {
            NodeKind::File
        };
        entries.push(DirEntry {
            name: component.to_owned(),
            kind,
        });
    }

    Ok(entries)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    // --- Fakes ---------------------------------------------------------------

    struct FakeIndex {
        // path -> (size, mode, hash)
        files: HashMap<String, (u64, u32, Hash)>,
    }

    impl FakeIndex {
        fn new() -> Self {
            Self {
                files: HashMap::new(),
            }
        }
        fn add(&mut self, path: &str, content: &[u8], mode: u32) -> Hash {
            // Deterministic fake hash: first 32 bytes of content, zero-padded.
            let mut h = [0u8; 32];
            let n = content.len().min(32);
            h[..n].copy_from_slice(&content[..n]);
            self.files
                .insert(path.to_owned(), (content.len() as u64, mode, h));
            h
        }
    }

    impl IndexSeam for FakeIndex {
        fn lookup(&self, path: &str) -> Option<NodeMeta> {
            self.files.get(path).map(|(size, mode, hash)| NodeMeta {
                kind: NodeKind::File,
                size: *size,
                mode: *mode,
                hash: Some(*hash),
            })
        }
        fn file_paths(&self) -> Vec<String> {
            self.files.keys().cloned().collect()
        }
    }

    struct FakeStore {
        blobs: HashMap<Hash, Vec<u8>>,
        hydrate_count: Arc<AtomicU32>,
    }

    impl FakeStore {
        fn new(counter: Arc<AtomicU32>) -> Self {
            Self {
                blobs: HashMap::new(),
                hydrate_count: counter,
            }
        }
        fn insert(&mut self, hash: Hash, data: Vec<u8>) {
            self.blobs.insert(hash, data);
        }
    }

    impl StoreSeam for FakeStore {
        fn hydrate(&self, hash: &Hash) -> Option<Vec<u8>> {
            self.hydrate_count.fetch_add(1, Ordering::Relaxed);
            self.blobs.get(hash).cloned()
        }
    }

    fn make_index_store(
        path: &str,
        content: &[u8],
        mode: u32,
    ) -> (FakeIndex, FakeStore, Arc<AtomicU32>) {
        let counter = Arc::new(AtomicU32::new(0));
        let mut idx = FakeIndex::new();
        let hash = idx.add(path, content, mode);
        let mut store = FakeStore::new(Arc::clone(&counter));
        store.insert(hash, content.to_vec());
        (idx, store, counter)
    }

    // --- attr_for tests ------------------------------------------------------

    #[test]
    fn attr_for_known_file() {
        let (idx, _, _) = make_index_store("README.md", b"hello world", 0o100644);
        let attrs = attr_for(&idx, "README.md").unwrap();
        assert_eq!(attrs.kind, NodeKind::File);
        assert_eq!(attrs.size, 11);
        assert_eq!(attrs.mode, 0o100644);
    }

    #[test]
    fn attr_for_known_dir() {
        let (idx, _, _) = make_index_store("src/main.rs", b"fn main(){}", 0o100644);
        let attrs = attr_for(&idx, "src").unwrap();
        assert_eq!(attrs.kind, NodeKind::Dir);
        assert_eq!(attrs.mode, 0o040755);
    }

    #[test]
    fn attr_for_root() {
        let (idx, _, _) = make_index_store("src/main.rs", b"fn main(){}", 0o100644);
        let attrs = attr_for(&idx, "").unwrap();
        assert_eq!(attrs.kind, NodeKind::Dir);
    }

    #[test]
    fn attr_for_missing() {
        let idx = FakeIndex::new();
        let err = attr_for(&idx, "no_such").unwrap_err();
        assert!(matches!(err, FsError::NotFound));
    }

    // --- read_slice tests ----------------------------------------------------

    #[test]
    fn read_slice_full() {
        let content = b"abcdefgh";
        let (idx, store, _) = make_index_store("f.txt", content, 0o100644);
        let got = read_slice(&idx, &store, "f.txt", 0, 100).unwrap();
        assert_eq!(got, content);
    }

    #[test]
    fn read_slice_offset_and_size() {
        let content = b"abcdefgh";
        let (idx, store, _) = make_index_store("f.txt", content, 0o100644);
        let got = read_slice(&idx, &store, "f.txt", 2, 3).unwrap();
        assert_eq!(got, b"cde");
    }

    #[test]
    fn read_slice_past_eof_offset() {
        let content = b"abc";
        let (idx, store, _) = make_index_store("f.txt", content, 0o100644);
        let got = read_slice(&idx, &store, "f.txt", 10, 5).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn read_slice_offset_plus_size_past_eof() {
        let content = b"abcde";
        let (idx, store, _) = make_index_store("f.txt", content, 0o100644);
        let got = read_slice(&idx, &store, "f.txt", 3, 100).unwrap();
        assert_eq!(got, b"de");
    }

    #[test]
    fn read_slice_zero_size() {
        let (idx, store, _) = make_index_store("f.txt", b"abc", 0o100644);
        let got = read_slice(&idx, &store, "f.txt", 0, 0).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn read_slice_missing() {
        let counter = Arc::new(AtomicU32::new(0));
        let idx = FakeIndex::new();
        let store = FakeStore::new(Arc::clone(&counter));
        let err = read_slice(&idx, &store, "ghost.txt", 0, 10).unwrap_err();
        assert!(matches!(err, FsError::NotFound));
    }

    #[test]
    fn read_slice_on_dir_is_eisdir() {
        let (idx, store, _) = make_index_store("src/lib.rs", b"pub fn f(){}", 0o100644);
        let err = read_slice(&idx, &store, "src", 0, 100).unwrap_err();
        assert!(matches!(err, FsError::IsADir));
    }

    #[test]
    fn read_slice_hydrate_count_exactly_one() {
        let content = b"payload bytes";
        let (idx, store, counter) = make_index_store("data.bin", content, 0o100644);
        let got = read_slice(&idx, &store, "data.bin", 0, 1024).unwrap();
        assert_eq!(got, content);
        assert_eq!(
            counter.load(Ordering::Relaxed),
            1,
            "hydrate must be called exactly once"
        );
    }

    #[test]
    fn read_slice_empty_file() {
        let counter = Arc::new(AtomicU32::new(0));
        let mut idx = FakeIndex::new();
        let hash = idx.add("empty.txt", b"", 0o100644);
        let mut store = FakeStore::new(Arc::clone(&counter));
        store.insert(hash, vec![]);
        let got = read_slice(&idx, &store, "empty.txt", 0, 100).unwrap();
        assert!(got.is_empty());
    }

    // --- read_dir tests ------------------------------------------------------

    fn entry_names(entries: &[DirEntry]) -> Vec<String> {
        entries.iter().map(|e| e.name.clone()).collect()
    }

    fn find_entry<'a>(entries: &'a [DirEntry], name: &str) -> Option<&'a DirEntry> {
        entries.iter().find(|e| e.name == name)
    }

    #[test]
    fn read_dir_root() {
        let (idx, _, _) = make_index_store("README.md", b"hello", 0o100644);
        let entries = read_dir(&idx, "").unwrap();
        let names = entry_names(&entries);
        assert!(names.contains(&"README.md".to_owned()));
        assert!(names.contains(&".".to_owned()));
        assert!(names.contains(&"..".to_owned()));
        let e = find_entry(&entries, "README.md").unwrap();
        assert_eq!(e.kind, NodeKind::File);
    }

    #[test]
    fn read_dir_nested() {
        let mut idx = FakeIndex::new();
        let h = idx.add("src/main.rs", b"fn main(){}", 0o100644);
        let counter = Arc::new(AtomicU32::new(0));
        let mut store = FakeStore::new(Arc::clone(&counter));
        store.insert(h, b"fn main(){}".to_vec());
        idx.add("src/sub/lib.rs", b"pub fn g(){}", 0o100644);

        let entries = read_dir(&idx, "src").unwrap();
        let names = entry_names(&entries);
        assert!(
            names.contains(&"main.rs".to_owned()),
            "direct file child must appear"
        );
        assert!(names.contains(&"sub".to_owned()), "subdir must appear");

        let sub_entry = find_entry(&entries, "sub").unwrap();
        assert_eq!(sub_entry.kind, NodeKind::Dir);
        let file_entry = find_entry(&entries, "main.rs").unwrap();
        assert_eq!(file_entry.kind, NodeKind::File);
    }

    #[test]
    fn read_dir_missing() {
        let idx = FakeIndex::new();
        let err = read_dir(&idx, "no_dir").unwrap_err();
        assert!(matches!(err, FsError::NotFound));
    }

    #[test]
    fn read_dir_on_file_is_enotdir() {
        let (idx, _, _) = make_index_store("file.rs", b"code", 0o100644);
        let err = read_dir(&idx, "file.rs").unwrap_err();
        assert!(matches!(err, FsError::NotADir));
    }

    #[test]
    fn read_dir_dedup() {
        let mut idx = FakeIndex::new();
        idx.add("src/a.rs", b"a", 0o100644);
        idx.add("src/b.rs", b"b", 0o100644);
        let entries = read_dir(&idx, "src").unwrap();
        let names = entry_names(&entries);
        // Should contain exactly . .. a.rs b.rs (no duplicates).
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "no duplicate entries");
    }

    // --- errno_for tests ------------------------------------------------------

    #[test]
    fn errno_mapping() {
        assert_eq!(errno_for(&FsError::NotFound), -libc::ENOENT);
        assert_eq!(errno_for(&FsError::NotADir), -libc::ENOTDIR);
        assert_eq!(errno_for(&FsError::IsADir), -libc::EISDIR);
        assert_eq!(errno_for(&FsError::IoError("x".into())), -libc::EIO);
        assert_eq!(errno_for(&FsError::AccessDenied), -libc::EACCES);
    }
}
