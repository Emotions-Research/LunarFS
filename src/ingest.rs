use crate::cas::{hash_to_hex, FsStore, Hash, Store};
use crate::tree::{serialize_tree, TreeEntry, MODE_DIR, MODE_EXEC, MODE_FILE};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

const MAX_WALK_ENTRIES: usize = 65_536;

pub fn walk_repo(store: &dyn Store, repo_path: &Path) -> io::Result<Hash> {
    // BFS to collect all directories, then process leaves-first (reverse BFS order).
    let repo_path = repo_path.canonicalize()?;

    // Phase 1: BFS to collect directories in breadth-first order.
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut bfs_queue: Vec<PathBuf> = vec![repo_path.clone()];
    let mut bfs_pos = 0usize;

    while bfs_pos < bfs_queue.len() {
        if bfs_queue.len() > MAX_WALK_ENTRIES {
            return Err(io::Error::other(
                "repo has too many directories (max 65536)",
            ));
        }
        let dir = bfs_queue[bfs_pos].clone();
        bfs_pos += 1;
        dirs.push(dir.clone());

        let mut rd: Vec<_> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() != ".git")
            .collect();
        rd.sort_by_key(|e| e.file_name());

        for entry in rd {
            let path = entry.path();
            if path.is_dir() && !path.is_symlink() {
                bfs_queue.push(path);
            }
        }
    }

    // Phase 2: Process dirs in reverse BFS order (deepest first).
    let mut dir_hashes: HashMap<PathBuf, Hash> = HashMap::new();

    for dir in dirs.iter().rev() {
        let mut rd: Vec<_> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() != ".git")
            .collect();
        rd.sort_by_key(|e| e.file_name());

        let mut tree_entries: Vec<TreeEntry> = Vec::new();

        for entry in rd {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let meta = std::fs::symlink_metadata(&path)?;

            if meta.is_dir() {
                let child_hash = dir_hashes.get(&path).ok_or_else(|| {
                    io::Error::other(format!("child dir hash not found for {}", path.display()))
                })?;
                tree_entries.push(TreeEntry {
                    mode: MODE_DIR,
                    name,
                    hash: *child_hash,
                });
            } else if meta.is_file() {
                let data = std::fs::read(&path)?;
                let hash = store.put(&data)?;
                tree_entries.push(TreeEntry {
                    mode: file_mode(&meta),
                    name,
                    hash,
                });
            }
            // symlinks skipped intentionally
        }

        let tree_bytes = serialize_tree(&tree_entries);
        let tree_hash = store.put(&tree_bytes)?;
        dir_hashes.insert(dir.clone(), tree_hash);
    }

    dir_hashes
        .get(&repo_path)
        .copied()
        .ok_or_else(|| io::Error::other("root tree hash not computed"))
}

pub fn ingest_repo(repo_path: &Path) -> io::Result<String> {
    let store = FsStore::default_root()?;
    let root_hash = walk_repo(&store, repo_path)?;
    Ok(hash_to_hex(&root_hash))
}

#[cfg(unix)]
fn file_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    if meta.permissions().mode() & 0o111 != 0 {
        MODE_EXEC
    } else {
        MODE_FILE
    }
}

#[cfg(not(unix))]
fn file_mode(_meta: &std::fs::Metadata) -> u32 {
    MODE_FILE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::MemStore;
    use crate::index::Index;
    use std::fs;

    #[test]
    fn walk_flat_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        fs::write(dir.path().join("b.txt"), b"world").unwrap();

        let store = MemStore::new();
        let root = walk_repo(&store, dir.path()).expect("walk must succeed");
        let index = Index::build(&store, &root).expect("index build");

        assert_eq!(index.len(), 2);

        let h_a = index.lookup("a.txt").expect("a.txt must be indexed");
        let blob_a = store.get(&h_a).unwrap().unwrap();
        assert_eq!(blob_a, b"hello");
    }

    #[test]
    fn walk_nested_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), b"fn main() {}").unwrap();
        fs::write(dir.path().join("README.md"), b"# readme").unwrap();

        let store = MemStore::new();
        let root = walk_repo(&store, dir.path()).expect("walk must succeed");
        let index = Index::build(&store, &root).expect("index build");

        assert_eq!(index.len(), 2);
        assert!(index.lookup("src/main.rs").is_some());
        assert!(index.lookup("README.md").is_some());
    }

    #[test]
    fn walk_skips_git_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git/HEAD"), b"ref: refs/heads/main").unwrap();
        fs::write(dir.path().join("real.txt"), b"real").unwrap();

        let store = MemStore::new();
        let root = walk_repo(&store, dir.path()).expect("walk must succeed");
        let index = Index::build(&store, &root).expect("index build");

        assert_eq!(index.len(), 1, ".git must be excluded");
        assert!(index.lookup("real.txt").is_some());
        assert!(index.lookup(".git/HEAD").is_none());
    }

    #[test]
    fn walk_empty_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = MemStore::new();
        let root = walk_repo(&store, dir.path()).expect("empty dir must succeed");
        let index = Index::build(&store, &root).expect("index build");
        assert!(index.is_empty());
    }

    #[test]
    fn walk_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("x.rs"), b"pub fn x() {}").unwrap();

        let store = MemStore::new();
        let r1 = walk_repo(&store, dir.path()).expect("first walk");
        let r2 = walk_repo(&store, dir.path()).expect("second walk");
        assert_eq!(r1, r2, "same repo must produce same root hash");
    }
}
