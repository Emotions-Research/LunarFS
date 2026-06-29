use anyhow::{anyhow, Result};

use crate::cas::{hash_to_hex, hex_to_hash, Store};
use crate::index::Index;
use crate::overlay::{AgentId, OverlayStore};

const MAX_ENTRIES: usize = 1_000_000;
const MODE_REGULAR: &str = "100644";
const ZEROS: &str = "0000000";

fn is_binary(data: &[u8]) -> bool {
    data.contains(&0u8)
}

fn abbrev(hex: &str) -> &str {
    if hex.len() >= 7 { &hex[..7] } else { hex }
}

fn git_path(path: &str) -> String {
    if path.bytes().any(|b| matches!(b, b' ' | b'\t' | b'"' | b'\\')) {
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
    let trimmed = if has_trailing { &data[..data.len() - 1] } else { data };
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
        out.push_str(&format!("index {}..{}\n", ZEROS, after_hex.map(abbrev).unwrap_or(ZEROS)));
    } else if is_deleted {
        out.push_str(&format!("deleted file mode {MODE_REGULAR}\n"));
        out.push_str(&format!("index {}..{}\n", before_hex.map(abbrev).unwrap_or(ZEROS), ZEROS));
    } else {
        out.push_str(&format!(
            "index {}..{} {MODE_REGULAR}\n",
            before_hex.map(abbrev).unwrap_or(ZEROS),
            after_hex.map(abbrev).unwrap_or(ZEROS),
        ));
    }

    if is_binary(before_bytes) || is_binary(after_bytes) {
        let from = if is_new { "/dev/null".to_owned() } else { format!("a/{qp}") };
        let to = if is_deleted { "/dev/null".to_owned() } else { format!("b/{qp}") };
        out.push_str(&format!("Binary files {from} and {to} differ\n"));
        return;
    }

    let (before_lines, before_nl) = split_lines(before_bytes);
    let (after_lines, after_nl) = split_lines(after_bytes);
    let bc = before_lines.len();
    let ac = after_lines.len();

    let from = if is_new { "/dev/null".to_owned() } else { format!("a/{qp}") };
    let to = if is_deleted { "/dev/null".to_owned() } else { format!("b/{qp}") };
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
                let hash = hex_to_hash(hex)
                    .map_err(|e| anyhow!("invalid blob_hash '{}': {}", hex, e))?;
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
                append_file_diff(&mut out, path, base_hex.as_deref(), None, &before_bytes, &[]);
            }
        }
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

        assert!(patch.contains("new file mode 100644"), "must have new file mode");
        assert!(patch.contains("--- /dev/null"), "source must be /dev/null");
        assert!(patch.contains("+++ b/new.txt"), "dest must be b/new.txt");
        assert!(patch.contains("+hello\n"), "must contain +hello line");
        assert!(patch.contains("+world\n"), "must contain +world line");
        let minus_content: Vec<&str> = patch
            .lines()
            .filter(|l| l.starts_with('-') && !l.starts_with("---"))
            .collect();
        assert!(minus_content.is_empty(), "new file must have no removed lines");
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
        overlay.capture_write(agent, WS, "foo.txt", &new_hex).unwrap();

        let patch = extract_patch(&overlay, &cas, &base, agent).unwrap();

        assert!(!patch.contains("new file mode"), "modification must not say new file");
        assert!(!patch.contains("deleted file mode"), "modification must not say deleted");
        assert!(patch.contains("--- a/foo.txt"), "source must be a/foo.txt");
        assert!(patch.contains("+++ b/foo.txt"), "dest must be b/foo.txt");
        assert!(patch.contains("-old line\n"), "must contain removed base line");
        assert!(patch.contains("+new line\n"), "must contain added overlay line");
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

        assert!(patch.contains("deleted file mode 100644"), "must have deleted file mode");
        assert!(patch.contains("--- a/bar.txt"), "source must be a/bar.txt");
        assert!(patch.contains("+++ /dev/null"), "dest must be /dev/null");
        assert!(patch.contains("-to be deleted\n"), "must contain removed line");
        let plus_content: Vec<&str> = patch
            .lines()
            .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
            .collect();
        assert!(plus_content.is_empty(), "deleted file must have no added content lines");
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
        assert!(lines.len() >= 8, "patch must have at least 8 lines, got {}", lines.len());
        assert_eq!(lines[0], "diff --git a/hello.txt b/hello.txt");
        assert_eq!(lines[1], "new file mode 100644");
        assert!(lines[2].starts_with("index 0000000.."), "index line must start 0000000.., got: {}", lines[2]);
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
}
