use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::cas::{hash_to_hex, hex_to_hash, Hash, Store};
use crate::index::Index;
use crate::overlay::{AgentId, OverlayStore};
use crate::tree::{deserialize_tree, TreeEntry, MODE_DIR};

const MAX_ENTRIES: usize = 1_000_000;
const MODE_REGULAR: &str = "100644";
const ZEROS: &str = "0000000";

fn is_binary(data: &[u8]) -> bool {
    data.contains(&0u8)
}

fn abbrev(hex: &str) -> &str {
    if hex.len() >= 7 {
        &hex[..7]
    } else {
        hex
    }
}

fn git_path(path: &str) -> String {
    if path
        .bytes()
        .any(|b| matches!(b, b' ' | b'\t' | b'"' | b'\\'))
    {
        format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        path.to_owned()
    }
}

/// Format one side of a hunk position. Git omits `,count` when count == 1.
fn hunk_pos(start: usize, count: usize) -> String {
    match count {
        0 => format!("{},0", start),
        1 => format!("{}", start),
        n => format!("{},{}", start, n),
    }
}

/// Split bytes into logical lines, stripping the per-line `\n`.
/// Returns (lines, has_trailing_newline).
fn split_lines(data: &[u8]) -> (Vec<String>, bool) {
    if data.is_empty() {
        return (vec![], true);
    }
    let has_trailing = *data.last().expect("non-empty slice has a last byte") == b'\n';
    let trimmed = if has_trailing {
        &data[..data.len() - 1]
    } else {
        data
    };
    let lines = trimmed
        .split(|&b| b == b'\n')
        .map(|l| String::from_utf8_lossy(l).into_owned())
        .collect();
    (lines, has_trailing)
}

/// Append a unified-diff section for one path to `out`.
///
/// `before_hex = None` means the path was not in the base (new file).
/// `after_hex  = None` means the agent deleted the path (tombstone).
fn append_file_diff(
    out: &mut String,
    path: &str,
    before_hex: Option<&str>,
    after_hex: Option<&str>,
    before_bytes: &[u8],
    after_bytes: &[u8],
) {
    assert!(
        before_hex.is_some() || after_hex.is_some(),
        "at least one of before_hex or after_hex must be Some"
    );

    let is_new = before_hex.is_none();
    let is_deleted = after_hex.is_none();
    let qp = git_path(path);

    out.push_str(&format!("diff --git a/{qp} b/{qp}\n"));

    if is_new {
        out.push_str(&format!("new file mode {MODE_REGULAR}\n"));
        out.push_str(&format!(
            "index {}..{}\n",
            ZEROS,
            after_hex.map(abbrev).unwrap_or(ZEROS)
        ));
    } else if is_deleted {
        out.push_str(&format!("deleted file mode {MODE_REGULAR}\n"));
        out.push_str(&format!(
            "index {}..{}\n",
            before_hex.map(abbrev).unwrap_or(ZEROS),
            ZEROS
        ));
    } else {
        out.push_str(&format!(
            "index {}..{} {MODE_REGULAR}\n",
            before_hex.map(abbrev).unwrap_or(ZEROS),
            after_hex.map(abbrev).unwrap_or(ZEROS),
        ));
    }

    if is_binary(before_bytes) || is_binary(after_bytes) {
        let from = if is_new {
            "/dev/null".to_owned()
        } else {
            format!("a/{qp}")
        };
        let to = if is_deleted {
            "/dev/null".to_owned()
        } else {
            format!("b/{qp}")
        };
        out.push_str(&format!("Binary files {from} and {to} differ\n"));
        return;
    }

    let (before_lines, before_nl) = split_lines(before_bytes);
    let (after_lines, after_nl) = split_lines(after_bytes);
    let bc = before_lines.len();
    let ac = after_lines.len();

    let from = if is_new {
        "/dev/null".to_owned()
    } else {
        format!("a/{qp}")
    };
    let to = if is_deleted {
        "/dev/null".to_owned()
    } else {
        format!("b/{qp}")
    };
    out.push_str(&format!("--- {from}\n"));
    out.push_str(&format!("+++ {to}\n"));

    if bc > 0 || ac > 0 {
        let before_start = if is_new { 0 } else { 1 };
        let after_start = if is_deleted { 0 } else { 1 };
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            hunk_pos(before_start, bc),
            hunk_pos(after_start, ac),
        ));
        for (i, line) in before_lines.iter().enumerate() {
            out.push('-');
            out.push_str(line);
            out.push('\n');
            if !before_nl && i + 1 == bc {
                out.push_str("\\ No newline at end of file\n");
            }
        }
        for (i, line) in after_lines.iter().enumerate() {
            out.push('+');
            out.push_str(line);
            out.push('\n');
            if !after_nl && i + 1 == ac {
                out.push_str("\\ No newline at end of file\n");
            }
        }
    }
}

/// Extract all of `agent`'s overlay changes as a single git-compatible unified diff string.
///
/// Entries are ordered deterministically by path (guaranteed by entries_for_agent).
/// Returns an empty string when the agent has no overlay entries.
pub fn extract_patch(
    overlay: &OverlayStore,
    cas: &dyn Store,
    base: &Index,
    agent: AgentId,
) -> Result<String> {
    assert!(agent >= 0, "agent_id must be non-negative");

    let entries = overlay.entries_for_agent(agent)?;
    assert!(
        entries.len() <= MAX_ENTRIES,
        "entries count {} exceeds cap of {}",
        entries.len(),
        MAX_ENTRIES
    );

    // nyx: out grows with diff content; bounded by entries cap * per-blob size
    let mut out = String::new();

    for entry in &entries {
        let path = &entry.path;
        let base_hash = base.lookup(path);

        // Skip: agent tombstoned a path that was never in the base (no-op).
        if entry.blob_hash.is_none() && base_hash.is_none() {
            continue;
        }

        let base_hex = base_hash.map(|h| hash_to_hex(&h));
        let before_bytes: Vec<u8> = match base_hash {
            Some(h) => cas.get(&h)?.unwrap_or_default(),
            None => vec![],
        };

        match &entry.blob_hash {
            Some(hex) => {
                let hash =
                    hex_to_hash(hex).map_err(|e| anyhow!("invalid blob_hash '{}': {}", hex, e))?;
                let after_bytes = cas.get(&hash)?.unwrap_or_default();
                append_file_diff(
                    &mut out,
                    path,
                    base_hex.as_deref(),
                    Some(hex),
                    &before_bytes,
                    &after_bytes,
                );
            }
            None => {
                append_file_diff(
                    &mut out,
                    path,
                    base_hex.as_deref(),
                    None,
                    &before_bytes,
                    &[],
                );
            }
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Content-addressed tree diff
//
// Type mapping (spec placeholders -> real symbols):
//   Hash          -> crate::cas::Hash = [u8; 32]
//   Tree          -> Vec<TreeEntry> (result of deserialize_tree)
//   TreeEntry     -> crate::tree::TreeEntry { mode: u32, name: String, hash: Hash }
//   is_subtree    -> entry.mode == MODE_DIR  (MODE_FILE / MODE_EXEC are blobs)
//   blob_size     -> loader.blob_size(&hash) which calls cas.get and returns .len() as u64
//                    (no separate metadata store; same approach as Index::build)
//   CrateError    -> anyhow::Error  (matches extract_patch's anyhow::Result)
// ---------------------------------------------------------------------------

const MAX_DIFF_ITERS: usize = 262_144;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug)]
pub struct Change {
    pub path: PathBuf,
    pub kind: ChangeKind,
    pub old_size: Option<u64>,
    pub new_size: Option<u64>,
    pub old_blob: Option<Hash>, // blob hash on the old side (None for Added)
    pub new_blob: Option<Hash>, // blob hash on the new side (None for Deleted)
}

// Testability seam: every subtree load and blob-size read routes through this
// trait. Production path uses CasLoader; test path uses CountingLoader.
trait TreeLoader {
    fn load_tree(&mut self, hash: &Hash) -> Result<Vec<TreeEntry>>;
    fn blob_size(&mut self, hash: &Hash) -> Result<u64>;
}

struct CasLoader<'a> {
    cas: &'a dyn Store,
}

impl<'a> TreeLoader for CasLoader<'a> {
    fn load_tree(&mut self, hash: &Hash) -> Result<Vec<TreeEntry>> {
        let bytes = self
            .cas
            .get(hash)?
            .ok_or_else(|| anyhow!("tree blob {} not found in CAS", hash_to_hex(hash)))?;
        deserialize_tree(&bytes).map_err(|e| anyhow!("deserialize_tree: {}", e))
    }

    fn blob_size(&mut self, hash: &Hash) -> Result<u64> {
        let bytes = self
            .cas
            .get(hash)?
            .ok_or_else(|| anyhow!("blob {} not found in CAS", hash_to_hex(hash)))?;
        Ok(bytes.len() as u64)
    }
}

enum WorkItem {
    Diff {
        prefix: PathBuf,
        old_hash: Hash,
        new_hash: Hash,
    },
    AddAll {
        prefix: PathBuf,
        hash: Hash,
    },
    DeleteAll {
        prefix: PathBuf,
        hash: Hash,
    },
}

fn diff_trees_inner(
    old_root: Hash,
    new_root: Hash,
    loader: &mut dyn TreeLoader,
) -> Result<Vec<Change>> {
    // Identical roots: zero cost, zero loads.
    if old_root == new_root {
        return Ok(vec![]);
    }

    let mut changes: Vec<Change> = Vec::new();
    let mut stack: Vec<WorkItem> = vec![WorkItem::Diff {
        prefix: PathBuf::new(),
        old_hash: old_root,
        new_hash: new_root,
    }];
    let mut iters = 0usize;

    while let Some(item) = stack.pop() {
        iters += 1;
        if iters > MAX_DIFF_ITERS {
            return Err(anyhow!(
                "diff_trees exceeded iteration cap ({}); tree may be too deep",
                MAX_DIFF_ITERS
            ));
        }

        match item {
            WorkItem::Diff {
                prefix,
                old_hash,
                new_hash,
            } => {
                // Inner prune: equal hashes at any subtree level => nothing changed.
                if old_hash == new_hash {
                    continue;
                }

                let old_entries = loader.load_tree(&old_hash)?;
                let new_entries = loader.load_tree(&new_hash)?;

                assert!(
                    old_entries.len() <= 65_536,
                    "old tree has too many entries: {}",
                    old_entries.len()
                );
                assert!(
                    new_entries.len() <= 65_536,
                    "new tree has too many entries: {}",
                    new_entries.len()
                );

                let mut old_map: HashMap<String, TreeEntry> = old_entries
                    .into_iter()
                    .map(|e| (e.name.clone(), e))
                    .collect();
                let mut new_map: HashMap<String, TreeEntry> = new_entries
                    .into_iter()
                    .map(|e| (e.name.clone(), e))
                    .collect();

                // Collect names that exist on both sides (snapshot before draining).
                let shared: Vec<String> = old_map
                    .keys()
                    .filter(|k| new_map.contains_key(*k))
                    .cloned()
                    .collect();

                for name in shared {
                    let old_e = old_map
                        .remove(&name)
                        .expect("shared key must be in old_map");
                    let new_e = new_map
                        .remove(&name)
                        .expect("shared key must be in new_map");
                    let path = prefix.join(&name);

                    // O(changed): equal hash => identical content, prune unconditionally.
                    if old_e.hash == new_e.hash {
                        continue;
                    }

                    let old_is_dir = old_e.mode == MODE_DIR;
                    let new_is_dir = new_e.mode == MODE_DIR;

                    match (old_is_dir, new_is_dir) {
                        (true, true) => {
                            stack.push(WorkItem::Diff {
                                prefix: path,
                                old_hash: old_e.hash,
                                new_hash: new_e.hash,
                            });
                        }
                        (false, false) => {
                            let old_size = loader.blob_size(&old_e.hash)?;
                            let new_size = loader.blob_size(&new_e.hash)?;
                            changes.push(Change {
                                path,
                                kind: ChangeKind::Modified,
                                old_size: Some(old_size),
                                new_size: Some(new_size),
                                old_blob: Some(old_e.hash),
                                new_blob: Some(new_e.hash),
                            });
                        }
                        // Type change (blob<->dir): Deleted old side + Added new side.
                        // nyx: rare; preserves accurate size on each half.
                        (true, false) => {
                            stack.push(WorkItem::DeleteAll {
                                prefix: path.clone(),
                                hash: old_e.hash,
                            });
                            let new_size = loader.blob_size(&new_e.hash)?;
                            changes.push(Change {
                                path,
                                kind: ChangeKind::Added,
                                old_size: None,
                                new_size: Some(new_size),
                                old_blob: None,
                                new_blob: Some(new_e.hash),
                            });
                        }
                        (false, true) => {
                            let old_size = loader.blob_size(&old_e.hash)?;
                            changes.push(Change {
                                path: path.clone(),
                                kind: ChangeKind::Deleted,
                                old_size: Some(old_size),
                                new_size: None,
                                old_blob: Some(old_e.hash),
                                new_blob: None,
                            });
                            stack.push(WorkItem::AddAll {
                                prefix: path,
                                hash: new_e.hash,
                            });
                        }
                    }
                }

                // Names only in old: Deleted.
                for (name, old_e) in old_map {
                    let path = prefix.join(&name);
                    if old_e.mode == MODE_DIR {
                        stack.push(WorkItem::DeleteAll {
                            prefix: path,
                            hash: old_e.hash,
                        });
                    } else {
                        let old_size = loader.blob_size(&old_e.hash)?;
                        changes.push(Change {
                            path,
                            kind: ChangeKind::Deleted,
                            old_size: Some(old_size),
                            new_size: None,
                            old_blob: Some(old_e.hash),
                            new_blob: None,
                        });
                    }
                }

                // Names only in new: Added.
                for (name, new_e) in new_map {
                    let path = prefix.join(&name);
                    if new_e.mode == MODE_DIR {
                        stack.push(WorkItem::AddAll {
                            prefix: path,
                            hash: new_e.hash,
                        });
                    } else {
                        let new_size = loader.blob_size(&new_e.hash)?;
                        changes.push(Change {
                            path,
                            kind: ChangeKind::Added,
                            old_size: None,
                            new_size: Some(new_size),
                            old_blob: None,
                            new_blob: Some(new_e.hash),
                        });
                    }
                }
            }

            WorkItem::AddAll { prefix, hash } => {
                let entries = loader.load_tree(&hash)?;
                for entry in entries {
                    let path = prefix.join(&entry.name);
                    if entry.mode == MODE_DIR {
                        stack.push(WorkItem::AddAll {
                            prefix: path,
                            hash: entry.hash,
                        });
                    } else {
                        let new_size = loader.blob_size(&entry.hash)?;
                        changes.push(Change {
                            path,
                            kind: ChangeKind::Added,
                            old_size: None,
                            new_size: Some(new_size),
                            old_blob: None,
                            new_blob: Some(entry.hash),
                        });
                    }
                }
            }

            WorkItem::DeleteAll { prefix, hash } => {
                let entries = loader.load_tree(&hash)?;
                for entry in entries {
                    let path = prefix.join(&entry.name);
                    if entry.mode == MODE_DIR {
                        stack.push(WorkItem::DeleteAll {
                            prefix: path,
                            hash: entry.hash,
                        });
                    } else {
                        let old_size = loader.blob_size(&entry.hash)?;
                        changes.push(Change {
                            path,
                            kind: ChangeKind::Deleted,
                            old_size: Some(old_size),
                            new_size: None,
                            old_blob: Some(entry.hash),
                            new_blob: None,
                        });
                    }
                }
            }
        }
    }

    Ok(changes)
}

/// Compute the structural changeset between two content-addressed trees.
///
/// Returns a `Vec<Change>` where each entry describes an Added, Modified, or
/// Deleted path. Subtrees whose hash is identical on both sides are pruned and
/// never descended into, making the walk O(changed) rather than O(total).
pub fn diff_trees(old_root: Hash, new_root: Hash, cas: &dyn Store) -> Result<Vec<Change>> {
    let mut loader = CasLoader { cas };
    diff_trees_inner(old_root, new_root, &mut loader)
}

/// Render the changeset as a git-style unified diff by fetching each blob's
/// bytes from the CAS. Binary blobs are annotated (handled inside
/// append_file_diff via the is_binary NUL-byte heuristic), not diffed.
/// Caller should pre-sort `changes` by path for deterministic output.
pub fn render_patch(changes: &[Change], cas: &dyn Store) -> Result<String> {
    assert!(
        changes.len() <= MAX_ENTRIES,
        "render_patch: changeset exceeds cap of {}",
        MAX_ENTRIES
    );
    // nyx: out grows with diff content; bounded by changes cap * per-blob size
    let mut out = String::new();
    for c in changes {
        let path = c.path.to_string_lossy();
        let before_hex = c.old_blob.map(|h| hash_to_hex(&h));
        let after_hex = c.new_blob.map(|h| hash_to_hex(&h));
        let before_bytes = match &c.old_blob {
            Some(h) => cas
                .get(h)
                .map_err(|e| anyhow!("cas get failed for old_blob: {}", e))?
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let after_bytes = match &c.new_blob {
            Some(h) => cas
                .get(h)
                .map_err(|e| anyhow!("cas get failed for new_blob: {}", e))?
                .unwrap_or_default(),
            None => Vec::new(),
        };
        append_file_diff(
            &mut out,
            &path,
            before_hex.as_deref(),
            after_hex.as_deref(),
            &before_bytes,
            &after_bytes,
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::{hash_to_hex, MemStore, Store};
    use crate::index::Index;
    use crate::overlay::{OverlayStore, WorkspaceId};
    use crate::tree::{serialize_tree, TreeEntry, MODE_FILE};
    use rusqlite::Connection;

    const WS: WorkspaceId = 1;

    fn make_overlay() -> OverlayStore {
        let conn = Connection::open_in_memory().expect("in-memory db");
        let s = OverlayStore::new(conn);
        s.init_schema().expect("init_schema");
        s
    }

    /// Build an Index from a flat list of (name, content) pairs, all at root level.
    fn index_with_files(cas: &MemStore, files: &[(&str, &[u8])]) -> Index {
        let entries: Vec<TreeEntry> = files
            .iter()
            .map(|(name, data)| TreeEntry {
                mode: MODE_FILE,
                name: name.to_string(),
                hash: cas.put(data).expect("put base blob"),
            })
            .collect();
        let tree_bytes = serialize_tree(&entries);
        let root = cas.put(&tree_bytes).expect("put tree");
        Index::build(cas, &root).expect("build index")
    }

    fn empty_index(cas: &MemStore) -> Index {
        let tree_bytes = serialize_tree(&[]);
        let root = cas.put(&tree_bytes).expect("put empty tree");
        Index::build(cas, &root).expect("build empty index")
    }

    // (a) Agent wrote a brand-new path: new file mode diff with /dev/null source.
    #[test]
    fn new_file_diff() {
        let cas = MemStore::new();
        let overlay = make_overlay();
        let agent = overlay.fork(WS).unwrap();

        let hash = cas.put(b"hello\nworld\n").unwrap();
        let hex = hash_to_hex(&hash);
        overlay.capture_write(agent, WS, "new.txt", &hex).unwrap();

        let base = empty_index(&cas);
        let patch = extract_patch(&overlay, &cas, &base, agent).unwrap();

        assert!(
            patch.contains("new file mode 100644"),
            "must have new file mode"
        );
        assert!(patch.contains("--- /dev/null"), "source must be /dev/null");
        assert!(patch.contains("+++ b/new.txt"), "dest must be b/new.txt");
        assert!(patch.contains("+hello\n"), "must contain +hello line");
        assert!(patch.contains("+world\n"), "must contain +world line");
        let minus_content: Vec<&str> = patch
            .lines()
            .filter(|l| l.starts_with('-') && !l.starts_with("---"))
            .collect();
        assert!(
            minus_content.is_empty(),
            "new file must have no removed lines"
        );
    }

    // (b) Agent modified an existing base path: '-' lines are base, '+' lines are overlay.
    #[test]
    fn modified_file_diff() {
        let cas = MemStore::new();
        let overlay = make_overlay();
        let agent = overlay.fork(WS).unwrap();

        let base = index_with_files(&cas, &[("foo.txt", b"old line\n")]);

        let new_hash = cas.put(b"new line\n").unwrap();
        let new_hex = hash_to_hex(&new_hash);
        overlay
            .capture_write(agent, WS, "foo.txt", &new_hex)
            .unwrap();

        let patch = extract_patch(&overlay, &cas, &base, agent).unwrap();

        assert!(
            !patch.contains("new file mode"),
            "modification must not say new file"
        );
        assert!(
            !patch.contains("deleted file mode"),
            "modification must not say deleted"
        );
        assert!(patch.contains("--- a/foo.txt"), "source must be a/foo.txt");
        assert!(patch.contains("+++ b/foo.txt"), "dest must be b/foo.txt");
        assert!(
            patch.contains("-old line\n"),
            "must contain removed base line"
        );
        assert!(
            patch.contains("+new line\n"),
            "must contain added overlay line"
        );
    }

    // (c) Tombstoned path: deleted file mode diff.
    #[test]
    fn tombstone_diff() {
        let cas = MemStore::new();
        let overlay = make_overlay();
        let agent = overlay.fork(WS).unwrap();

        let base = index_with_files(&cas, &[("bar.txt", b"to be deleted\n")]);
        overlay.capture_delete(agent, WS, "bar.txt").unwrap();

        let patch = extract_patch(&overlay, &cas, &base, agent).unwrap();

        assert!(
            patch.contains("deleted file mode 100644"),
            "must have deleted file mode"
        );
        assert!(patch.contains("--- a/bar.txt"), "source must be a/bar.txt");
        assert!(patch.contains("+++ /dev/null"), "dest must be /dev/null");
        assert!(
            patch.contains("-to be deleted\n"),
            "must contain removed line"
        );
        let plus_content: Vec<&str> = patch
            .lines()
            .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
            .collect();
        assert!(
            plus_content.is_empty(),
            "deleted file must have no added content lines"
        );
    }

    // (d) Structural fixture: inspect exact line layout of a known new-file patch.
    #[test]
    fn fixture_structure() {
        let cas = MemStore::new();
        let overlay = make_overlay();
        let agent = overlay.fork(WS).unwrap();

        let hash = cas.put(b"line one\nline two\n").unwrap();
        let hex = hash_to_hex(&hash);
        overlay.capture_write(agent, WS, "hello.txt", &hex).unwrap();

        let base = empty_index(&cas);
        let patch = extract_patch(&overlay, &cas, &base, agent).unwrap();

        let lines: Vec<&str> = patch.lines().collect();
        assert!(
            lines.len() >= 8,
            "patch must have at least 8 lines, got {}",
            lines.len()
        );
        assert_eq!(lines[0], "diff --git a/hello.txt b/hello.txt");
        assert_eq!(lines[1], "new file mode 100644");
        assert!(
            lines[2].starts_with("index 0000000.."),
            "index line must start 0000000.., got: {}",
            lines[2]
        );
        assert_eq!(lines[3], "--- /dev/null");
        assert_eq!(lines[4], "+++ b/hello.txt");
        assert_eq!(lines[5], "@@ -0,0 +1,2 @@");
        assert_eq!(lines[6], "+line one");
        assert_eq!(lines[7], "+line two");
    }

    // (e) Agent with no overlay entries yields an empty string.
    #[test]
    fn empty_agent_yields_empty_patch() {
        let cas = MemStore::new();
        let overlay = make_overlay();
        let agent = overlay.fork(WS).unwrap();
        let base = empty_index(&cas);
        let patch = extract_patch(&overlay, &cas, &base, agent).unwrap();
        assert_eq!(patch, "", "empty agent must produce empty patch");
    }

    // ---------------------------------------------------------------------------
    // diff_trees tests
    // ---------------------------------------------------------------------------

    use super::{diff_trees, diff_trees_inner, ChangeKind, TreeLoader};
    use crate::cas::{hash_bytes, Hash};
    use crate::tree::MODE_DIR;
    use std::path::PathBuf;

    // In-memory loader that counts every tree load, enabling prune assertions.
    struct CountingLoader {
        blobs: HashMap<Hash, Vec<u8>>,
        tree_loads: Vec<Hash>,
    }

    impl CountingLoader {
        fn new() -> Self {
            Self {
                blobs: HashMap::new(),
                tree_loads: vec![],
            }
        }

        fn put_blob(&mut self, data: &[u8]) -> Hash {
            let h = hash_bytes(data);
            self.blobs.insert(h, data.to_vec());
            h
        }

        fn put_tree(&mut self, entries: &[TreeEntry]) -> Hash {
            let bytes = serialize_tree(entries);
            let h = hash_bytes(&bytes);
            self.blobs.insert(h, bytes);
            h
        }

        fn loaded(&self, hash: &Hash) -> bool {
            self.tree_loads.contains(hash)
        }
    }

    impl TreeLoader for CountingLoader {
        fn load_tree(&mut self, hash: &Hash) -> anyhow::Result<Vec<TreeEntry>> {
            self.tree_loads.push(*hash);
            let bytes = self
                .blobs
                .get(hash)
                .ok_or_else(|| anyhow::anyhow!("hash not in loader"))?;
            crate::tree::deserialize_tree(bytes).map_err(|e| anyhow::anyhow!("{}", e))
        }

        fn blob_size(&mut self, hash: &Hash) -> anyhow::Result<u64> {
            let bytes = self
                .blobs
                .get(hash)
                .ok_or_else(|| anyhow::anyhow!("blob not in loader"))?;
            Ok(bytes.len() as u64)
        }
    }

    // (a) PRUNE: a shared subtree with equal hash is never loaded.
    #[test]
    fn diff_trees_prune_unchanged_subtree() {
        let mut l = CountingLoader::new();

        // Shared subtree (same hash on both sides).
        let h_a = l.put_blob(b"file a content");
        let h_b = l.put_blob(b"file b content");
        let shared_tree = l.put_tree(&[
            TreeEntry {
                mode: MODE_FILE,
                name: "a.txt".into(),
                hash: h_a,
            },
            TreeEntry {
                mode: MODE_FILE,
                name: "b.txt".into(),
                hash: h_b,
            },
        ]);

        // Changed blob only in root.
        let h_old_changed = l.put_blob(b"old value");
        let h_new_changed = l.put_blob(b"new value");

        let old_root = l.put_tree(&[
            TreeEntry {
                mode: MODE_DIR,
                name: "shared".into(),
                hash: shared_tree,
            },
            TreeEntry {
                mode: MODE_FILE,
                name: "changed.txt".into(),
                hash: h_old_changed,
            },
        ]);
        let new_root = l.put_tree(&[
            TreeEntry {
                mode: MODE_DIR,
                name: "shared".into(),
                hash: shared_tree,
            },
            TreeEntry {
                mode: MODE_FILE,
                name: "changed.txt".into(),
                hash: h_new_changed,
            },
        ]);

        let changes = diff_trees_inner(old_root, new_root, &mut l).unwrap();

        // The shared subtree's hash must never have been passed to load_tree.
        assert!(
            !l.loaded(&shared_tree),
            "prune: shared subtree hash must not be loaded"
        );

        // Only the changed file contributes a change.
        assert_eq!(changes.len(), 1, "only one change expected");
        assert_eq!(changes[0].kind, ChangeKind::Modified);
        assert_eq!(changes[0].path, PathBuf::from("changed.txt"));
        assert_eq!(changes[0].old_size, Some(b"old value".len() as u64));
        assert_eq!(changes[0].new_size, Some(b"new value".len() as u64));
    }

    // (b) Added: path only in new tree.
    #[test]
    fn diff_trees_added() {
        let mut l = CountingLoader::new();

        let h_existing = l.put_blob(b"existing");
        let h_new_file = l.put_blob(b"brand new content");

        let old_root = l.put_tree(&[TreeEntry {
            mode: MODE_FILE,
            name: "existing.txt".into(),
            hash: h_existing,
        }]);
        let new_root = l.put_tree(&[
            TreeEntry {
                mode: MODE_FILE,
                name: "existing.txt".into(),
                hash: h_existing,
            },
            TreeEntry {
                mode: MODE_FILE,
                name: "new.txt".into(),
                hash: h_new_file,
            },
        ]);

        let mut changes = diff_trees_inner(old_root, new_root, &mut l).unwrap();
        changes.sort_by(|a, b| a.path.cmp(&b.path));

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Added);
        assert_eq!(changes[0].path, PathBuf::from("new.txt"));
        assert_eq!(changes[0].old_size, None, "Added must have no old_size");
        assert_eq!(changes[0].new_size, Some(b"brand new content".len() as u64));
    }

    // (c) Deleted: path only in old tree.
    #[test]
    fn diff_trees_deleted() {
        let mut l = CountingLoader::new();

        let h_kept = l.put_blob(b"kept");
        let h_gone = l.put_blob(b"going away");

        let old_root = l.put_tree(&[
            TreeEntry {
                mode: MODE_FILE,
                name: "kept.txt".into(),
                hash: h_kept,
            },
            TreeEntry {
                mode: MODE_FILE,
                name: "gone.txt".into(),
                hash: h_gone,
            },
        ]);
        let new_root = l.put_tree(&[TreeEntry {
            mode: MODE_FILE,
            name: "kept.txt".into(),
            hash: h_kept,
        }]);

        let mut changes = diff_trees_inner(old_root, new_root, &mut l).unwrap();
        changes.sort_by(|a, b| a.path.cmp(&b.path));

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Deleted);
        assert_eq!(changes[0].path, PathBuf::from("gone.txt"));
        assert_eq!(changes[0].old_size, Some(b"going away".len() as u64));
        assert_eq!(changes[0].new_size, None, "Deleted must have no new_size");
    }

    // (d) Modified: same path, different blob hash, correct sizes.
    #[test]
    fn diff_trees_modified() {
        let mut l = CountingLoader::new();

        let h_old = l.put_blob(b"old content here");
        let h_new = l.put_blob(b"new content here - longer");

        let old_root = l.put_tree(&[TreeEntry {
            mode: MODE_FILE,
            name: "file.txt".into(),
            hash: h_old,
        }]);
        let new_root = l.put_tree(&[TreeEntry {
            mode: MODE_FILE,
            name: "file.txt".into(),
            hash: h_new,
        }]);

        let changes = diff_trees_inner(old_root, new_root, &mut l).unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Modified);
        assert_eq!(changes[0].path, PathBuf::from("file.txt"));
        assert_eq!(changes[0].old_size, Some(b"old content here".len() as u64));
        assert_eq!(
            changes[0].new_size,
            Some(b"new content here - longer".len() as u64)
        );
    }

    // (e) Identical roots: empty changeset, zero loads.
    #[test]
    fn diff_trees_identical_roots_empty_and_no_loads() {
        let mut l = CountingLoader::new();

        let h_file = l.put_blob(b"some content");
        let root = l.put_tree(&[TreeEntry {
            mode: MODE_FILE,
            name: "f.txt".into(),
            hash: h_file,
        }]);

        let changes = diff_trees_inner(root, root, &mut l).unwrap();

        assert!(
            changes.is_empty(),
            "identical roots must yield empty changeset"
        );
        assert!(
            l.tree_loads.is_empty(),
            "identical roots must trigger zero loads"
        );
    }

    // (f) diff_trees public API works via CAS store (smoke test).
    #[test]
    fn diff_trees_public_api_smoke() {
        let cas = MemStore::new();

        let h_a = cas.put(b"hello").unwrap();
        let h_b = cas.put(b"world").unwrap();
        let old_tree = serialize_tree(&[TreeEntry {
            mode: MODE_FILE,
            name: "a.txt".into(),
            hash: h_a,
        }]);
        let new_tree = serialize_tree(&[TreeEntry {
            mode: MODE_FILE,
            name: "b.txt".into(),
            hash: h_b,
        }]);
        let old_root = cas.put(&old_tree).unwrap();
        let new_root = cas.put(&new_tree).unwrap();

        let mut changes = diff_trees(old_root, new_root, &cas).unwrap();
        changes.sort_by(|a, b| a.path.cmp(&b.path));

        assert_eq!(changes.len(), 2);
        let deleted = changes
            .iter()
            .find(|c| c.kind == ChangeKind::Deleted)
            .unwrap();
        let added = changes
            .iter()
            .find(|c| c.kind == ChangeKind::Added)
            .unwrap();
        assert_eq!(deleted.path, PathBuf::from("a.txt"));
        assert_eq!(added.path, PathBuf::from("b.txt"));
    }

    // ---------------------------------------------------------------------------
    // render_patch tests
    // ---------------------------------------------------------------------------

    use super::render_patch;

    /// Build a single-file flat tree root hash in the given CAS.
    fn one_file_root(cas: &MemStore, name: &str, data: &[u8]) -> Hash {
        let blob = cas.put(data).expect("put blob");
        let tree_bytes = serialize_tree(&[TreeEntry {
            mode: MODE_FILE,
            name: name.to_string(),
            hash: blob,
        }]);
        cas.put(&tree_bytes).expect("put tree")
    }

    // (g) A text modification produces unified hunks with the changed lines.
    #[test]
    fn render_patch_text_modification_emits_unified_hunks() {
        let cas = MemStore::new();

        let old_root = one_file_root(&cas, "foo.txt", b"alpha\nbeta\n");
        let new_root = one_file_root(&cas, "foo.txt", b"alpha\ngamma\n");

        let mut changes = diff_trees(old_root, new_root, &cas).unwrap();
        changes.sort_by(|a, b| a.path.cmp(&b.path));

        let patch = render_patch(&changes, &cas).unwrap();

        assert!(
            patch.contains("diff --git a/foo.txt b/foo.txt"),
            "must have diff --git header"
        );
        assert!(patch.contains("@@"), "must have a hunk header");
        assert!(patch.contains("-beta"), "must contain removed line");
        assert!(patch.contains("+gamma"), "must contain added line");
        assert!(
            !patch.contains("Binary files"),
            "text file must not be annotated as binary"
        );
    }

    // (h) Binary blobs are annotated with "differ", never line-diffed.
    #[test]
    fn render_patch_binary_blob_is_annotated_not_diffed() {
        let cas = MemStore::new();

        let old_root = one_file_root(&cas, "bin.dat", b"\x00\x01\x02data");
        let new_root = one_file_root(&cas, "bin.dat", b"\x00\x09\x09changed");

        let mut changes = diff_trees(old_root, new_root, &cas).unwrap();
        changes.sort_by(|a, b| a.path.cmp(&b.path));

        let patch = render_patch(&changes, &cas).unwrap();

        assert!(patch.contains("differ"), "binary annotation must say differ");
        assert!(patch.contains("bin.dat"), "annotation must name the file");
        assert!(!patch.contains("@@"), "binary diff must have no hunk header");

        // No content lines: no lines starting with + (beyond +++ header) or - (beyond --- header).
        // For binary files, append_file_diff emits no --- / +++ at all, so these checks are strict.
        let plus_content: Vec<&str> = patch
            .lines()
            .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
            .collect();
        let minus_content: Vec<&str> = patch
            .lines()
            .filter(|l| l.starts_with('-') && !l.starts_with("---"))
            .collect();
        assert!(
            plus_content.is_empty(),
            "binary diff must have no added content lines, got: {:?}",
            plus_content
        );
        assert!(
            minus_content.is_empty(),
            "binary diff must have no removed content lines, got: {:?}",
            minus_content
        );
    }
}
