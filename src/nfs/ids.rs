//! Deterministic fileid<->path bijection for the NFS layer.
//!
//! Base IDs (1..=base_len) are built once from the Index: deterministic,
//! zero blob fetches, lexicographically ordered so root ("") is always 1.
//! Dynamic IDs (base_len+1 and above) are allocated at runtime by intern() as
//! paths are created/written/renamed. IDs are never reused for different paths.

use crate::fuse::translate::NodeKind;
use crate::index::Index;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

const MAX_NODES: usize = 2_000_000;

// nyx: in-process only; upgrade path: on-disk id journal for persistence across remounts.
const MAX_DYNAMIC_NODES: usize = 1_000_000;

struct DynamicMaps {
    next_id: u64,
    id_to_node: HashMap<u64, (String, NodeKind)>,
    path_to_id: HashMap<String, u64>,
    tombstones: HashSet<String>,
}

/// Two-way mapping between NFS fileids and filesystem paths.
///
/// The base maps are immutable and built at mount time. The dynamic maps
/// grow as paths are created, written, or renamed during the mount session.
pub struct IdTable {
    base_id_to_node: Vec<(String, NodeKind)>,
    base_path_to_id: HashMap<String, u64>,
    dyn_maps: Mutex<DynamicMaps>,
}

impl IdTable {
    /// Build from an Index. Walks all file paths, derives ancestor dirs,
    /// sorts lexicographically, assigns fileids starting at 1 (root="" = 1).
    pub fn build(index: &Index) -> Self {
        let mut node_set: HashMap<String, NodeKind> = HashMap::new();
        node_set.insert(String::new(), NodeKind::Dir);

        let mut file_count = 0usize;
        for (file_path, _hash) in index.entries() {
            file_count += 1;
            assert!(
                file_count <= 65_536,
                "index exceeds the 65536-entry cap from Index::build"
            );
            assert!(file_path.len() <= 4096, "path exceeds 4096-byte limit");

            for (i, byte) in file_path.as_bytes().iter().enumerate() {
                if *byte == b'/' {
                    let dir_prefix = &file_path[..i];
                    node_set.entry(dir_prefix.to_owned()).or_insert(NodeKind::Dir);
                    assert!(
                        node_set.len() <= MAX_NODES,
                        "node set exceeded {} entries (ancestor expansion)",
                        MAX_NODES
                    );
                }
            }

            node_set.entry(file_path.to_owned()).or_insert(NodeKind::File);
            assert!(
                node_set.len() <= MAX_NODES,
                "node set exceeded {} entries (file insertion)",
                MAX_NODES
            );
        }

        let mut sorted: Vec<(String, NodeKind)> = node_set.into_iter().collect();
        sorted.sort_by(|(a, _), (b, _)| a.cmp(b));

        let capacity = sorted.len();
        let mut base_path_to_id: HashMap<String, u64> = HashMap::with_capacity(capacity);
        for (idx, (path, _kind)) in sorted.iter().enumerate() {
            let id = (idx + 1) as u64;
            base_path_to_id.insert(path.clone(), id);
        }

        let base_len = sorted.len() as u64;
        Self {
            base_id_to_node: sorted,
            base_path_to_id,
            dyn_maps: Mutex::new(DynamicMaps {
                next_id: base_len + 1,
                id_to_node: HashMap::new(),
                path_to_id: HashMap::new(),
                tombstones: HashSet::new(),
            }),
        }
    }

    /// Root directory fileid (always 1).
    pub fn root() -> u64 {
        1
    }

    /// Returns (path, NodeKind) for the given fileid, or None if unknown or tombstoned.
    pub fn path_of(&self, id: u64) -> Option<(String, NodeKind)> {
        if id == 0 {
            return None;
        }
        let lock = self.dyn_maps.lock().expect("dyn_maps lock poisoned");
        // Dynamic range first
        if let Some((path, kind)) = lock.id_to_node.get(&id) {
            if lock.tombstones.contains(path.as_str()) {
                return None;
            }
            return Some((path.clone(), *kind));
        }
        // Base range
        let idx = (id as usize).checked_sub(1)?;
        let (path, kind) = self.base_id_to_node.get(idx)?;
        if lock.tombstones.contains(path.as_str()) {
            return None;
        }
        Some((path.clone(), *kind))
    }

    /// Returns the fileid for `path`, or None if unknown or tombstoned.
    pub fn id_of(&self, path: &str) -> Option<u64> {
        let lock = self.dyn_maps.lock().expect("dyn_maps lock poisoned");
        if lock.tombstones.contains(path) {
            return None;
        }
        if let Some(&id) = self.base_path_to_id.get(path) {
            return Some(id);
        }
        lock.path_to_id.get(path).copied()
    }

    /// Idempotent fileid allocation. Clears any existing tombstone, then
    /// returns the existing id or allocates a fresh one above the base range.
    pub fn intern(&self, path: &str, kind: NodeKind) -> u64 {
        assert!(path.len() <= 4096, "path must not exceed 4096 bytes");
        let mut lock = self.dyn_maps.lock().expect("dyn_maps lock poisoned");
        lock.tombstones.remove(path);
        if let Some(&id) = self.base_path_to_id.get(path) {
            return id;
        }
        if let Some(&id) = lock.path_to_id.get(path) {
            return id;
        }
        assert!(
            lock.id_to_node.len() < MAX_DYNAMIC_NODES,
            "dynamic node table exceeded {} entries",
            MAX_DYNAMIC_NODES
        );
        let id = lock.next_id;
        assert!(id > 0, "dynamic fileid must be positive and strictly increasing");
        lock.next_id = id + 1;
        lock.id_to_node.insert(id, (path.to_owned(), kind));
        lock.path_to_id.insert(path.to_owned(), id);
        id
    }

    /// Tombstone `path`. Subsequent id_of/path_of return None for it.
    /// The id slot remains reserved (never reassigned to a different path).
    pub fn forget(&self, path: &str) {
        assert!(path.len() <= 4096, "path must not exceed 4096 bytes");
        let mut lock = self.dyn_maps.lock().expect("dyn_maps lock poisoned");
        lock.tombstones.insert(path.to_owned());
    }

    /// Returns true if `path` is tombstoned.
    pub fn is_hidden(&self, path: &str) -> bool {
        let lock = self.dyn_maps.lock().expect("dyn_maps lock poisoned");
        lock.tombstones.contains(path)
    }

    /// Tombstone `old` and intern `new_path` with `kind`.
    pub fn rename_path(&self, old: &str, new_path: &str, kind: NodeKind) {
        assert!(old.len() <= 4096, "old path must not exceed 4096 bytes");
        assert!(new_path.len() <= 4096, "new path must not exceed 4096 bytes");
        let mut lock = self.dyn_maps.lock().expect("dyn_maps lock poisoned");
        lock.tombstones.insert(old.to_owned());
        intern_in_lock(&self.base_path_to_id, &mut lock, new_path, kind);
    }

    /// Re-key every path equal to `old_prefix` or starting with `old_prefix/`
    /// to the corresponding `new_prefix` path. Tombstones old paths; interns
    /// new ones. Covers both base and dynamic paths.
    pub fn rename_subtree(&self, old_prefix: &str, new_prefix: &str) {
        assert!(old_prefix.len() <= 4096, "old_prefix must not exceed 4096 bytes");
        assert!(new_prefix.len() <= 4096, "new_prefix must not exceed 4096 bytes");
        let old_slash = format!("{}/", old_prefix);

        let mut renames: Vec<(String, String, NodeKind)> = Vec::new();

        // Collect base paths under old_prefix
        for (path, &id) in &self.base_path_to_id {
            if path != old_prefix && !path.starts_with(&old_slash) {
                continue;
            }
            let suffix = &path[old_prefix.len()..];
            let new_path = format!("{}{}", new_prefix, suffix);
            if new_path.len() > 4096 {
                continue;
            }
            let (_, kind) = &self.base_id_to_node[(id as usize) - 1];
            renames.push((path.clone(), new_path, *kind));
        }

        // Collect dynamic paths under old_prefix (snapshot under lock)
        let dyn_snapshot: Vec<(String, NodeKind)> = {
            let lock = self.dyn_maps.lock().expect("dyn_maps lock poisoned");
            lock.path_to_id
                .keys()
                .filter(|p| *p == old_prefix || p.starts_with(&old_slash))
                .filter_map(|p| {
                    let (_, kind) = lock.id_to_node.values().find(|(path, _)| path == p)?;
                    Some((p.clone(), *kind))
                })
                .collect()
        };
        for (path, kind) in dyn_snapshot {
            if !renames.iter().any(|(op, _, _)| op == &path) {
                let suffix = &path[old_prefix.len()..];
                let new_path = format!("{}{}", new_prefix, suffix);
                if new_path.len() <= 4096 {
                    renames.push((path, new_path, kind));
                }
            }
        }

        // Apply tombstones and interns under a single lock acquisition
        let mut lock = self.dyn_maps.lock().expect("dyn_maps lock poisoned");
        for (old_path, new_path, kind) in renames {
            lock.tombstones.insert(old_path);
            intern_in_lock(&self.base_path_to_id, &mut lock, &new_path, kind);
        }
    }

    /// Return all dynamically interned NodeKind::Dir entries that are direct
    /// children of `parent`. Used by readdir to surface empty dirs from mkdir.
    pub fn dynamic_dir_children_of(&self, parent: &str) -> Vec<(String, NodeKind)> {
        let prefix = if parent.is_empty() { String::new() } else { format!("{}/", parent) };
        let lock = self.dyn_maps.lock().expect("dyn_maps lock poisoned");
        let mut result = Vec::new();
        for (path, kind) in lock.id_to_node.values() {
            if *kind != NodeKind::Dir || lock.tombstones.contains(path.as_str()) {
                continue;
            }
            let rest: &str = if prefix.is_empty() {
                path.as_str()
            } else {
                match path.strip_prefix(&prefix) {
                    Some(r) => r,
                    None => continue,
                }
            };
            if !rest.is_empty() && !rest.contains('/') {
                result.push((path.clone(), *kind));
            }
        }
        result
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        let lock = self.dyn_maps.lock().expect("dyn_maps lock poisoned");
        self.base_id_to_node.len() + lock.id_to_node.len()
    }
}

/// Intern `path` into `lock`, reusing an existing id (base or dynamic) or
/// allocating a fresh one. Clears any tombstone. Called with lock already held.
fn intern_in_lock(
    base: &HashMap<String, u64>,
    lock: &mut DynamicMaps,
    path: &str,
    kind: NodeKind,
) {
    assert!(path.len() <= 4096, "path must not exceed 4096 bytes");
    lock.tombstones.remove(path);
    if base.contains_key(path) || lock.path_to_id.contains_key(path) {
        return;
    }
    assert!(
        lock.id_to_node.len() < MAX_DYNAMIC_NODES,
        "dynamic node table exceeded {} entries",
        MAX_DYNAMIC_NODES
    );
    let id = lock.next_id;
    assert!(id > 0, "dynamic fileid must be positive");
    lock.next_id = id + 1;
    lock.id_to_node.insert(id, (path.to_owned(), kind));
    lock.path_to_id.insert(path.to_owned(), id);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::{MemStore, Store};
    use crate::tree::{serialize_tree, TreeEntry, MODE_DIR, MODE_FILE};

    fn make_fixture_index() -> Index {
        let store = MemStore::new();

        let h_readme = store.put(b"hello").unwrap();
        let h_main = store.put(b"fn main() {}").unwrap();
        let h_lib = store.put(b"pub fn f() {}").unwrap();

        let sub_tree = store
            .put(&serialize_tree(&[TreeEntry {
                mode: MODE_FILE,
                name: "lib.rs".into(),
                hash: h_lib,
            }]))
            .unwrap();

        let src_tree = store
            .put(&serialize_tree(&[
                TreeEntry { mode: MODE_FILE, name: "main.rs".into(), hash: h_main },
                TreeEntry { mode: MODE_DIR, name: "sub".into(), hash: sub_tree },
            ]))
            .unwrap();

        let root_tree = store
            .put(&serialize_tree(&[
                TreeEntry { mode: MODE_FILE, name: "README.md".into(), hash: h_readme },
                TreeEntry { mode: MODE_DIR, name: "src".into(), hash: src_tree },
            ]))
            .unwrap();

        Index::build(&store, &root_tree).expect("index build must succeed")
    }

    #[test]
    fn root_is_fileid_1() {
        let index = make_fixture_index();
        let table = IdTable::build(&index);
        assert_eq!(IdTable::root(), 1);
        let (path, kind) = table.path_of(1).expect("root must exist");
        assert_eq!(path, "");
        assert_eq!(kind, NodeKind::Dir);
    }

    #[test]
    fn fixture_yields_six_nodes() {
        let index = make_fixture_index();
        let table = IdTable::build(&index);
        assert_eq!(table.len(), 6);
    }

    #[test]
    fn inferred_dirs_have_ids() {
        let index = make_fixture_index();
        let table = IdTable::build(&index);
        assert!(table.id_of("src").is_some(), "src/ must have an id");
        assert!(table.id_of("src/sub").is_some(), "src/sub/ must have an id");
        let (_, src_kind) = table.path_of(table.id_of("src").unwrap()).unwrap();
        assert_eq!(src_kind, NodeKind::Dir);
    }

    #[test]
    fn files_are_file_kind() {
        let index = make_fixture_index();
        let table = IdTable::build(&index);
        let (_, kind) = table.path_of(table.id_of("README.md").unwrap()).unwrap();
        assert_eq!(kind, NodeKind::File);
        let (_, kind2) = table.path_of(table.id_of("src/main.rs").unwrap()).unwrap();
        assert_eq!(kind2, NodeKind::File);
    }

    #[test]
    fn round_trip_all_paths() {
        let index = make_fixture_index();
        let table = IdTable::build(&index);
        for id in 1..=(table.len() as u64) {
            let (path, _kind) =
                table.path_of(id).unwrap_or_else(|| panic!("id {} must exist", id));
            let back = table
                .id_of(&path)
                .unwrap_or_else(|| panic!("path {:?} must have an id", path));
            assert_eq!(back, id, "round-trip failed for path {:?}", path);
        }
    }

    #[test]
    fn ids_are_stable_across_builds() {
        let index = make_fixture_index();
        let t1 = IdTable::build(&index);
        let t2 = IdTable::build(&index);
        let id1 = t1.id_of("src/main.rs").unwrap();
        let id2 = t2.id_of("src/main.rs").unwrap();
        assert_eq!(id1, id2, "IDs must be deterministic across builds");
    }

    #[test]
    fn sorted_order_matches_expected() {
        let index = make_fixture_index();
        let table = IdTable::build(&index);
        assert_eq!(table.id_of(""), Some(1));
        assert_eq!(table.id_of("README.md"), Some(2));
        assert_eq!(table.id_of("src"), Some(3));
        assert_eq!(table.id_of("src/main.rs"), Some(4));
        assert_eq!(table.id_of("src/sub"), Some(5));
        assert_eq!(table.id_of("src/sub/lib.rs"), Some(6));
    }

    #[test]
    fn unknown_id_returns_none() {
        let index = make_fixture_index();
        let table = IdTable::build(&index);
        assert!(table.path_of(0).is_none(), "fileid 0 is reserved");
        assert!(table.path_of(9999).is_none(), "out-of-range id must return None");
    }

    #[test]
    fn unknown_path_returns_none() {
        let index = make_fixture_index();
        let table = IdTable::build(&index);
        assert!(table.id_of("does_not_exist.txt").is_none());
    }

    #[test]
    fn empty_index_yields_root_only() {
        let store = crate::cas::MemStore::new();
        let root_tree = store.put(&serialize_tree(&[])).unwrap();
        let index = Index::build(&store, &root_tree).unwrap();
        let table = IdTable::build(&index);
        assert_eq!(table.len(), 1);
        assert_eq!(table.id_of(""), Some(1));
    }

    // --- dynamic allocation tests -------------------------------------------

    #[test]
    fn intern_is_idempotent() {
        let index = make_fixture_index();
        let table = IdTable::build(&index);
        let id1 = table.intern("newfile.txt", NodeKind::File);
        let id2 = table.intern("newfile.txt", NodeKind::File);
        assert_eq!(id1, id2, "intern must return the same id on repeated calls");
        assert!(id1 > 6, "dynamic id must be above base range");
    }

    #[test]
    fn intern_base_path_returns_base_id() {
        let index = make_fixture_index();
        let table = IdTable::build(&index);
        let base_id = table.id_of("README.md").unwrap();
        let interned_id = table.intern("README.md", NodeKind::File);
        assert_eq!(
            base_id, interned_id,
            "intern on a base path must return the existing base id"
        );
    }

    #[test]
    fn intern_allocates_above_base_ids() {
        let index = make_fixture_index();
        let table = IdTable::build(&index);
        let base_max = 6u64; // fixture has 6 base nodes
        let new_id = table.intern("dynamic.rs", NodeKind::File);
        assert!(
            new_id > base_max,
            "dynamic id ({}) must be above base max ({})",
            new_id,
            base_max
        );
    }

    #[test]
    fn forget_hides_path() {
        let index = make_fixture_index();
        let table = IdTable::build(&index);
        assert!(table.id_of("README.md").is_some(), "must be visible before forget");
        table.forget("README.md");
        assert!(table.id_of("README.md").is_none(), "must be hidden after forget");
        assert!(table.path_of(2).is_none(), "path_of must return None for tombstoned path");
    }

    #[test]
    fn intern_clears_tombstone() {
        let index = make_fixture_index();
        let table = IdTable::build(&index);
        table.forget("README.md");
        assert!(table.id_of("README.md").is_none(), "must be hidden after forget");
        table.intern("README.md", NodeKind::File);
        assert!(table.id_of("README.md").is_some(), "must be visible after intern clears tombstone");
    }

    #[test]
    fn rename_subtree_rekeyes_nested_paths() {
        let index = make_fixture_index();
        let table = IdTable::build(&index);
        // Intern a dynamic path under src/
        let _id_main = table.intern("src/main.rs", NodeKind::File); // already base, returns base id
        let _ = table.intern("src/new.rs", NodeKind::File);

        table.rename_subtree("src", "lib");

        // Old paths must be hidden
        assert!(table.id_of("src").is_none(), "src must be tombstoned");
        assert!(table.id_of("src/main.rs").is_none(), "src/main.rs must be tombstoned");
        assert!(table.id_of("src/sub").is_none(), "src/sub must be tombstoned");
        assert!(table.id_of("src/sub/lib.rs").is_none(), "src/sub/lib.rs must be tombstoned");
        assert!(table.id_of("src/new.rs").is_none(), "src/new.rs must be tombstoned");

        // New paths must be visible
        assert!(table.id_of("lib").is_some(), "lib must be interned");
        assert!(table.id_of("lib/main.rs").is_some(), "lib/main.rs must be interned");
        assert!(table.id_of("lib/sub").is_some(), "lib/sub must be interned");
        assert!(table.id_of("lib/sub/lib.rs").is_some(), "lib/sub/lib.rs must be interned");
        assert!(table.id_of("lib/new.rs").is_some(), "lib/new.rs must be interned");
    }

    #[test]
    fn dynamic_ids_never_collide_with_base_ids() {
        let index = make_fixture_index();
        let table = IdTable::build(&index);
        let base_ids: std::collections::HashSet<u64> =
            (1..=6).collect();
        let dyn_id1 = table.intern("a.txt", NodeKind::File);
        let dyn_id2 = table.intern("b.txt", NodeKind::File);
        let dyn_id3 = table.intern("c.txt", NodeKind::File);
        assert!(!base_ids.contains(&dyn_id1), "dynamic id must not collide with base");
        assert!(!base_ids.contains(&dyn_id2), "dynamic id must not collide with base");
        assert!(!base_ids.contains(&dyn_id3), "dynamic id must not collide with base");
        assert_ne!(dyn_id1, dyn_id2);
        assert_ne!(dyn_id2, dyn_id3);
    }
}
