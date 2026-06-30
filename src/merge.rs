// Pure file-level 3-way merge with keep-both conflict copy semantics.
// No I/O; all inputs and outputs are owned in-memory data.

use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileState {
    Present(String),
    Deleted,
}

/// Flat snapshot: repo-relative path -> file state.
pub type Snapshot = HashMap<String, FileState>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeAction {
    TakeWinner,
    TakeLocal,
    Unchanged,
    /// Original path kept for winner; loser's version at `copy_path`.
    ConflictCopy {
        copy_path: String,
    },
}

#[derive(Debug, Clone)]
pub struct PathOutcome {
    pub path: String,
    pub action: MergeAction,
}

pub struct MergeResult {
    pub merged: Snapshot,
    pub outcomes: Vec<PathOutcome>,
    /// Paths that produced a conflict copy (edit-vs-edit or edit-vs-delete).
    pub conflicts: Vec<String>,
}

/// Dropbox-style conflict copy naming.
///
/// 'foo.txt'     + 'abc' -> 'foo (conflicted copy abc).txt'
/// 'dir/bar.md'  + 'abc' -> 'dir/bar (conflicted copy abc).md'
/// 'no-ext'      + 'abc' -> 'no-ext (conflicted copy abc)'
/// '.hidden'     + 'abc' -> '.hidden (conflicted copy abc)'
pub fn conflict_copy_path(path: &str, suffix: &str) -> String {
    assert!(!path.is_empty(), "path must not be empty");
    assert!(!suffix.is_empty(), "conflict suffix must not be empty");
    // Split on last '/' to isolate the filename segment.
    let (dir, name) = match path.rfind('/') {
        Some(i) => (&path[..=i], &path[i + 1..]),
        None => ("", path),
    };
    // Within the filename, find the extension (last '.', not at position 0 of the filename).
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    format!("{}{} (conflicted copy {}){}", dir, stem, suffix, ext)
}

/// Pure 3-way file-level merge.
///
/// `base`   - the last common snapshot both writers started from.
/// `winner` - the server's current snapshot (CAS winner's committed view).
/// `local`  - the loser's full local snapshot (base + local edits applied).
/// `suffix` - deterministic label injected into conflict copy names (no random, no clock).
///
/// Rules:
/// - Only one side changed  -> take that side (auto-merge, no conflict copy).
/// - Both sides identical   -> take winner, no conflict.
/// - edit vs edit (different content) -> winner at original path, local as conflict copy.
/// - winner edits, local deletes -> winner's edit survives; delete ignored.
/// - winner deletes, local edits -> local edit becomes a conflict copy; delete wins at original.
pub fn merge_snapshots(
    base: &Snapshot,
    winner: &Snapshot,
    local: &Snapshot,
    suffix: &str,
) -> MergeResult {
    assert!(
        !suffix.is_empty(),
        "conflict suffix must not be empty for merge"
    );
    // nyx: linear scan; upgrade path: streaming merge for very large trees
    assert!(
        base.len() + winner.len() + local.len() <= 3_000_000,
        "combined path count exceeds merge cap"
    );

    let absent = &FileState::Deleted;
    let mut all_paths: HashSet<&String> = HashSet::new();
    for p in base.keys() {
        all_paths.insert(p);
    }
    for p in winner.keys() {
        all_paths.insert(p);
    }
    for p in local.keys() {
        all_paths.insert(p);
    }

    let mut merged: Snapshot = Snapshot::with_capacity(all_paths.len());
    let mut outcomes: Vec<PathOutcome> = Vec::with_capacity(all_paths.len());
    let mut conflicts: Vec<String> = Vec::new();

    for path in all_paths {
        let base_s = base.get(path).unwrap_or(absent);
        let win_s = winner.get(path).unwrap_or(absent);
        let loc_s = local.get(path).unwrap_or(absent);

        let winner_changed = win_s != base_s;
        let local_changed = loc_s != base_s;

        let action = if !winner_changed && !local_changed {
            if win_s != absent {
                merged.insert(path.clone(), win_s.clone());
            }
            MergeAction::Unchanged
        } else if winner_changed && !local_changed {
            if win_s != absent {
                merged.insert(path.clone(), win_s.clone());
            }
            MergeAction::TakeWinner
        } else if !winner_changed && local_changed {
            if loc_s != absent {
                merged.insert(path.clone(), loc_s.clone());
            }
            MergeAction::TakeLocal
        } else {
            apply_conflict(path, win_s, loc_s, suffix, &mut merged, &mut conflicts)
        };

        outcomes.push(PathOutcome {
            path: path.clone(),
            action,
        });
    }

    MergeResult {
        merged,
        outcomes,
        conflicts,
    }
}

/// Resolve a path where both winner and local changed it relative to base.
fn apply_conflict(
    path: &str,
    win_s: &FileState,
    loc_s: &FileState,
    suffix: &str,
    merged: &mut Snapshot,
    conflicts: &mut Vec<String>,
) -> MergeAction {
    assert!(
        !path.is_empty(),
        "path must not be empty in conflict resolution"
    );

    if win_s == loc_s {
        // Both sides converged to the same content: no conflict.
        if win_s != &FileState::Deleted {
            merged.insert(path.to_owned(), win_s.clone());
        }
        return MergeAction::TakeWinner;
    }

    match (win_s, loc_s) {
        // Winner kept/edited, local deleted: edit survives (delete never silently wins).
        (FileState::Present(_), FileState::Deleted) => {
            merged.insert(path.to_owned(), win_s.clone());
            MergeAction::TakeWinner
        }
        // Winner deleted, local edited: local edit survives as conflict copy.
        // The original path stays absent (winner's deletion is authoritative).
        (FileState::Deleted, FileState::Present(content)) => {
            let copy = conflict_copy_path(path, suffix);
            merged.insert(copy.clone(), FileState::Present(content.clone()));
            conflicts.push(path.to_owned());
            MergeAction::ConflictCopy { copy_path: copy }
        }
        // edit-vs-edit with different content: winner at original path, local as conflict copy.
        (FileState::Present(_), FileState::Present(loc_content)) => {
            merged.insert(path.to_owned(), win_s.clone());
            let copy = conflict_copy_path(path, suffix);
            merged.insert(copy.clone(), FileState::Present(loc_content.clone()));
            conflicts.push(path.to_owned());
            MergeAction::ConflictCopy { copy_path: copy }
        }
        // Deleted-vs-Deleted: both agree; covered by the equality check above.
        (FileState::Deleted, FileState::Deleted) => {
            unreachable!("Deleted==Deleted should have been caught by equality check")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(entries: &[(&str, &str)]) -> Snapshot {
        entries
            .iter()
            .map(|(p, c)| (p.to_string(), FileState::Present(c.to_string())))
            .collect()
    }

    fn snap_with_del(entries: &[(&str, Option<&str>)]) -> Snapshot {
        entries
            .iter()
            .map(|(p, c)| {
                (
                    p.to_string(),
                    match c {
                        Some(s) => FileState::Present(s.to_string()),
                        None => FileState::Deleted,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn conflict_copy_path_with_extension() {
        assert_eq!(
            conflict_copy_path("foo.txt", "x"),
            "foo (conflicted copy x).txt"
        );
    }

    #[test]
    fn conflict_copy_path_no_extension() {
        assert_eq!(
            conflict_copy_path("Makefile", "y"),
            "Makefile (conflicted copy y)"
        );
    }

    #[test]
    fn conflict_copy_path_hidden_file() {
        assert_eq!(
            conflict_copy_path(".hidden", "z"),
            ".hidden (conflicted copy z)"
        );
    }

    #[test]
    fn conflict_copy_path_with_dir() {
        assert_eq!(
            conflict_copy_path("dir/foo.txt", "s"),
            "dir/foo (conflicted copy s).txt"
        );
    }

    #[test]
    fn conflict_copy_path_deep_dir() {
        assert_eq!(
            conflict_copy_path("a/b/c.md", "s"),
            "a/b/c (conflicted copy s).md"
        );
    }

    #[test]
    fn unchanged_when_neither_side_changed() {
        let base = snap(&[("a.txt", "base")]);
        let result = merge_snapshots(&base, &base, &base, "s");
        assert!(result.conflicts.is_empty());
        assert_eq!(result.merged["a.txt"], FileState::Present("base".into()));
    }

    #[test]
    fn disjoint_changes_auto_merge() {
        let base = snap(&[("a.txt", "a"), ("b.txt", "b")]);
        let winner = snap(&[("a.txt", "A"), ("b.txt", "b")]);
        let local = snap(&[("a.txt", "a"), ("b.txt", "B")]);
        let result = merge_snapshots(&base, &winner, &local, "s");
        assert!(
            result.conflicts.is_empty(),
            "disjoint changes must not conflict"
        );
        assert_eq!(result.merged["a.txt"], FileState::Present("A".into()));
        assert_eq!(result.merged["b.txt"], FileState::Present("B".into()));
    }

    #[test]
    fn same_content_both_sides_no_conflict() {
        let base = snap(&[("f.txt", "base")]);
        let winner = snap(&[("f.txt", "new")]);
        let local = snap(&[("f.txt", "new")]);
        let result = merge_snapshots(&base, &winner, &local, "s");
        assert!(
            result.conflicts.is_empty(),
            "identical edits must not conflict"
        );
        assert_eq!(result.merged["f.txt"], FileState::Present("new".into()));
    }

    #[test]
    fn edit_vs_edit_yields_conflict_copy() {
        let base = snap(&[("foo.txt", "base")]);
        let winner = snap(&[("foo.txt", "W")]);
        let local = snap(&[("foo.txt", "L")]);
        let result = merge_snapshots(&base, &winner, &local, "seed");
        assert_eq!(result.conflicts, vec!["foo.txt".to_string()]);
        assert_eq!(result.merged["foo.txt"], FileState::Present("W".into()));
        let copy = conflict_copy_path("foo.txt", "seed");
        assert_eq!(result.merged[&copy], FileState::Present("L".into()));
    }

    #[test]
    fn winner_deletes_local_edits_gives_conflict_copy() {
        let base = snap(&[("bar.txt", "base")]);
        let winner: Snapshot = HashMap::new(); // winner deleted bar.txt
        let local = snap(&[("bar.txt", "local-edit")]);
        let result = merge_snapshots(&base, &winner, &local, "s1");
        assert_eq!(result.conflicts, vec!["bar.txt".to_string()]);
        assert!(
            !result.merged.contains_key("bar.txt"),
            "original must be absent"
        );
        let copy = conflict_copy_path("bar.txt", "s1");
        assert_eq!(
            result.merged[&copy],
            FileState::Present("local-edit".into())
        );
    }

    #[test]
    fn winner_edits_local_deletes_edit_survives() {
        let base = snap(&[("bar.txt", "base")]);
        let winner = snap(&[("bar.txt", "winner-edit")]);
        let local: Snapshot = HashMap::new(); // local deleted bar.txt
        let result = merge_snapshots(&base, &winner, &local, "s2");
        assert!(
            result.conflicts.is_empty(),
            "no conflict copy when edit wins over delete"
        );
        assert_eq!(
            result.merged["bar.txt"],
            FileState::Present("winner-edit".into())
        );
    }

    #[test]
    fn new_file_added_by_winner_only() {
        let base: Snapshot = HashMap::new();
        let winner = snap(&[("new.txt", "content")]);
        let local: Snapshot = HashMap::new();
        let result = merge_snapshots(&base, &winner, &local, "s");
        assert!(result.conflicts.is_empty());
        assert_eq!(
            result.merged["new.txt"],
            FileState::Present("content".into())
        );
    }

    #[test]
    fn new_file_added_by_local_only() {
        let base: Snapshot = HashMap::new();
        let winner: Snapshot = HashMap::new();
        let local = snap(&[("new.txt", "local")]);
        let result = merge_snapshots(&base, &winner, &local, "s");
        assert!(result.conflicts.is_empty());
        assert_eq!(result.merged["new.txt"], FileState::Present("local".into()));
    }

    #[test]
    fn deleted_state_not_inserted_into_merged() {
        let base = snap_with_del(&[("f.txt", Some("x"))]);
        let winner = snap_with_del(&[("f.txt", None)]); // winner deleted
        let local = snap_with_del(&[("f.txt", None)]); // local also deleted (same as winner)
        let result = merge_snapshots(&base, &winner, &local, "s");
        assert!(result.conflicts.is_empty());
        assert!(
            !result.merged.contains_key("f.txt"),
            "both deleted: path must be absent"
        );
    }
}
