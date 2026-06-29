use crate::cas::{hash_to_hex, Hash, Store};
use crate::tree::{deserialize_tree, MODE_DIR};
use std::collections::HashMap;
use std::io;

#[derive(Debug)]
pub struct Index {
    map: HashMap<String, (Hash, u64)>,
}

impl Index {
    pub fn build(store: &dyn Store, root: &Hash) -> io::Result<Self> {
        const MAX_ITERS: usize = 65_536;
        let mut map = HashMap::new();

        // Iterative tree walk: stack items are (tree_hash, path_prefix).
        // Empty prefix means root tree.
        let mut stack: Vec<(Hash, String)> = vec![(*root, String::new())];
        let mut iters = 0usize;

        while let Some((tree_hash, prefix)) = stack.pop() {
            iters += 1;
            if iters > MAX_ITERS {
                return Err(io::Error::other(
                    "tree DAG exceeds maximum walk depth (65536 nodes)",
                ));
            }

            let blob = store.get(&tree_hash)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("tree blob {} not found in store", hash_to_hex(&tree_hash)),
                )
            })?;

            let entries = deserialize_tree(&blob)?;

            for entry in entries {
                let full_path = if prefix.is_empty() {
                    entry.name.clone()
                } else {
                    format!("{}/{}", prefix, entry.name)
                };

                if entry.mode == MODE_DIR {
                    stack.push((entry.hash, full_path));
                } else {
                    let file_blob = store.get(&entry.hash)?.ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("file blob {} not found in store", hash_to_hex(&entry.hash)),
                        )
                    })?;
                    let file_len = file_blob.len() as u64;
                    map.insert(full_path, (entry.hash, file_len));
                }
            }
        }

        Ok(Self { map })
    }

    pub fn lookup(&self, path: &str) -> Option<Hash> {
        self.map.get(path).map(|(h, _)| *h)
    }

    /// Returns the cached byte length of the base blob at `path`, or None if
    /// the path is not a base file. O(1) map read with no CAS fetch.
    pub fn lookup_size(&self, path: &str) -> Option<u64> {
        self.map.get(path).map(|(_, sz)| *sz)
    }

    /// Returns both the hash and cached byte length for `path` in one map read.
    pub fn lookup_entry(&self, path: &str) -> Option<(Hash, u64)> {
        self.map.get(path).map(|(h, sz)| (*h, *sz))
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &Hash)> {
        self.map.iter().map(|(k, (h, _))| (k.as_str(), h))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::{hash_bytes, MemStore};
    use crate::tree::{serialize_tree, TreeEntry, MODE_DIR, MODE_FILE};

    fn put_blob(store: &MemStore, data: &[u8]) -> Hash {
        store.put(data).unwrap()
    }

    fn put_tree(store: &MemStore, entries: Vec<TreeEntry>) -> Hash {
        let bytes = serialize_tree(&entries);
        store.put(&bytes).unwrap()
    }

    #[test]
    fn build_flat_tree() {
        let store = MemStore::new();
        let h_a = put_blob(&store, b"content of a");
        let h_b = put_blob(&store, b"content of b");
        let root = put_tree(
            &store,
            vec![
                TreeEntry { mode: MODE_FILE, name: "a.txt".into(), hash: h_a },
                TreeEntry { mode: MODE_FILE, name: "b.txt".into(), hash: h_b },
            ],
        );

        let index = Index::build(&store, &root).expect("build must succeed");
        assert_eq!(index.len(), 2);
        assert_eq!(index.lookup("a.txt"), Some(h_a));
        assert_eq!(index.lookup("b.txt"), Some(h_b));
        assert_eq!(index.lookup("missing.txt"), None);
    }

    #[test]
    fn build_nested_tree() {
        let store = MemStore::new();

        let h_main = put_blob(&store, b"fn main() {}");
        let h_lib = put_blob(&store, b"pub fn hello() {}");

        let src_tree = put_tree(
            &store,
            vec![
                TreeEntry { mode: MODE_FILE, name: "main.rs".into(), hash: h_main },
                TreeEntry { mode: MODE_FILE, name: "lib.rs".into(), hash: h_lib },
            ],
        );

        let h_readme = put_blob(&store, b"# Dev Dropbox");

        let root = put_tree(
            &store,
            vec![
                TreeEntry { mode: MODE_DIR, name: "src".into(), hash: src_tree },
                TreeEntry { mode: MODE_FILE, name: "README.md".into(), hash: h_readme },
            ],
        );

        let index = Index::build(&store, &root).expect("build must succeed");
        assert_eq!(index.len(), 3, "3 files: src/main.rs, src/lib.rs, README.md");
        assert_eq!(index.lookup("src/main.rs"), Some(h_main));
        assert_eq!(index.lookup("src/lib.rs"), Some(h_lib));
        assert_eq!(index.lookup("README.md"), Some(h_readme));
        assert_eq!(index.lookup("src"), None, "dir paths must not be in index");
    }

    #[test]
    fn build_empty_root() {
        let store = MemStore::new();
        let root = put_tree(&store, vec![]);
        let index = Index::build(&store, &root).expect("empty root must succeed");
        assert!(index.is_empty());
    }

    #[test]
    fn build_missing_blob_errors() {
        let store = MemStore::new();
        let missing = hash_bytes(b"not in store");
        let err = Index::build(&store, &missing).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
