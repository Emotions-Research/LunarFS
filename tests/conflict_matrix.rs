// Deterministic in-process conflict matrix: proves all four CAS+merge scenarios.
// No real filesystem, no timers, no network, no sleeps.
//
// Scenario 1: disjoint changes auto-merge (zero conflict copies).
// Scenario 2: same-path edit-vs-edit yields a conflict copy preserving both versions.
// Scenario 3: edit-vs-delete (both directions) keeps the edit; delete never silently wins.
// Scenario 4: two racing CAS advances; first wins, loser reconciles to merged result.
//
// Also asserts: isolated single-writer workspace CAS always succeeds with no conflicts.

use std::collections::HashMap;
use std::sync::Arc;

use devdropbox::merge::{conflict_copy_path, merge_snapshots, FileState, Snapshot};
use devdropbox::reconcile::{reconcile, MemSnapshotStore, SnapshotStore};
use devdropbox::ref_advance::{CasResult, MemRefStore, RefStore};

// ---------------------------------------------------------------------------
// Snapshot construction helpers
// ---------------------------------------------------------------------------

fn present(s: &str) -> FileState {
    FileState::Present(s.to_string())
}

fn snap(entries: &[(&str, &str)]) -> Snapshot {
    entries
        .iter()
        .map(|(p, c)| (p.to_string(), present(c)))
        .collect()
}

// ---------------------------------------------------------------------------
// Scenario 1: Disjoint changes auto-merge
// ---------------------------------------------------------------------------

#[test]
fn scenario1_disjoint_changes_auto_merge() {
    let base = snap(&[("a.txt", "base-a"), ("b.txt", "base-b")]);
    // Winner edited a.txt; local edited b.txt (completely disjoint).
    let winner = snap(&[("a.txt", "winner-a"), ("b.txt", "base-b")]);
    let local = snap(&[("a.txt", "base-a"), ("b.txt", "local-b")]);

    let result = merge_snapshots(&base, &winner, &local, "seed");

    assert!(
        result.conflicts.is_empty(),
        "disjoint changes must produce zero conflict copies"
    );
    assert_eq!(
        result.merged.get("a.txt"),
        Some(&present("winner-a")),
        "winner's edit must be taken for a.txt"
    );
    assert_eq!(
        result.merged.get("b.txt"),
        Some(&present("local-b")),
        "local's edit must be taken for b.txt"
    );
    // No spurious keys.
    assert_eq!(
        result.merged.len(),
        2,
        "merged must contain exactly 2 entries"
    );
}

// ---------------------------------------------------------------------------
// Scenario 2: Same-path edit-vs-edit
// ---------------------------------------------------------------------------

#[test]
fn scenario2_edit_vs_edit_keeps_both_versions() {
    let base = snap(&[("foo.txt", "base-content")]);
    let winner = snap(&[("foo.txt", "winner-content")]);
    let local = snap(&[("foo.txt", "local-content")]);

    let result = merge_snapshots(&base, &winner, &local, "abc");

    // Exactly one conflict on foo.txt.
    assert_eq!(result.conflicts, vec!["foo.txt".to_string()]);

    // Winner's version stays at the original path.
    assert_eq!(
        result.merged.get("foo.txt"),
        Some(&present("winner-content")),
        "winner must hold the original path"
    );

    // Local's version appears at the conflict copy path.
    let copy = conflict_copy_path("foo.txt", "abc");
    assert_eq!(
        result.merged.get(&copy),
        Some(&present("local-content")),
        "local's content must be preserved at the conflict copy path"
    );

    // Both versions present: nothing was dropped.
    assert_eq!(
        result.merged.len(),
        2,
        "merged must contain original + conflict copy"
    );
}

// ---------------------------------------------------------------------------
// Scenario 3: Edit-vs-delete (both directions)
// ---------------------------------------------------------------------------

#[test]
fn scenario3_winner_deletes_local_edit_survives() {
    // Direction A: winner deleted bar.txt; local edited it.
    let base = snap(&[("bar.txt", "base-content")]);
    let winner: Snapshot = HashMap::new(); // winner deleted bar.txt
    let local = snap(&[("bar.txt", "local-edit")]);

    let result = merge_snapshots(&base, &winner, &local, "s1");

    // The original path must be absent (winner's delete is authoritative at original path).
    assert!(
        !result.merged.contains_key("bar.txt"),
        "original path must be absent when winner deleted it"
    );

    // Local's edit must survive as a conflict copy.
    let copy = conflict_copy_path("bar.txt", "s1");
    assert_eq!(
        result.merged.get(&copy),
        Some(&present("local-edit")),
        "local's edit must survive as conflict copy"
    );

    assert_eq!(result.conflicts, vec!["bar.txt".to_string()]);
}

#[test]
fn scenario3_winner_edits_local_delete_ignored() {
    // Direction B: winner edited bar.txt; local deleted it.
    let base = snap(&[("bar.txt", "base-content")]);
    let winner = snap(&[("bar.txt", "winner-edit")]);
    let local: Snapshot = HashMap::new(); // local deleted bar.txt

    let result = merge_snapshots(&base, &winner, &local, "s2");

    // Winner's edit survives at the original path; delete never silently wins.
    assert_eq!(
        result.merged.get("bar.txt"),
        Some(&present("winner-edit")),
        "winner's edit must survive; local delete must not win"
    );

    // No conflict copy needed (winner's view is taken).
    assert!(
        result.conflicts.is_empty(),
        "no conflict copy when edit takes priority over delete"
    );
}

// ---------------------------------------------------------------------------
// Scenario 4: Racing CAS advances + reconcile loop
// ---------------------------------------------------------------------------

#[test]
fn scenario4_racing_cas_advances_converge() {
    let ref_store = Arc::new(MemRefStore::new());
    let snap_store = Arc::new(MemSnapshotStore::new());

    // Both writers start from the same base snapshot.
    let base = snap(&[("shared.txt", "base-value")]);
    snap_store.seed("base-root", base.clone());

    // Initialize the ref to base-root.
    let init = ref_store.advance("workspace", None, "base-root");
    assert!(
        matches!(init, CasResult::Committed { .. }),
        "first-write CAS must succeed for an empty ref"
    );

    // Writer A commits their snapshot and advances the ref.
    let winner_snap = snap(&[("shared.txt", "base-value"), ("a.txt", "from-A")]);
    snap_store.seed("winner-root", winner_snap);
    let result_a = ref_store.advance("workspace", Some("base-root"), "winner-root");
    assert!(
        matches!(result_a, CasResult::Committed { .. }),
        "first racer must win the CAS"
    );

    // Writer B tries to advance from the same base-root (now stale).
    // B's local view has b.txt added.
    let b_local = snap(&[("shared.txt", "base-value"), ("b.txt", "from-B")]);

    let result_b = ref_store.advance("workspace", Some("base-root"), "b-attempted-root");
    assert!(
        matches!(&result_b, CasResult::Mismatch { current } if current == "winner-root"),
        "second racer must get a mismatch carrying the winner's root"
    );

    // B reconciles: merge against winner, re-advance until CAS succeeds.
    let final_root = reconcile(
        &*ref_store,
        &*snap_store,
        "workspace",
        "base-root",
        &b_local,
        "suffix",
        5,
    )
    .expect("reconcile must converge");

    // The final snapshot must contain both A's and B's changes.
    let final_snap = snap_store
        .fetch(&final_root)
        .expect("final snapshot must be in store");
    assert_eq!(
        final_snap.get("a.txt"),
        Some(&present("from-A")),
        "A's change must be present after reconcile"
    );
    assert_eq!(
        final_snap.get("b.txt"),
        Some(&present("from-B")),
        "B's change must be present after reconcile"
    );
    assert_eq!(
        final_snap.get("shared.txt"),
        Some(&present("base-value")),
        "shared file must be unchanged"
    );

    // The ref must point to the merged result.
    assert_eq!(
        ref_store.read("workspace").as_deref(),
        Some(final_root.as_str()),
        "ref must point to the final merged root"
    );
}

// ---------------------------------------------------------------------------
// Isolated single-writer workspace: CAS always succeeds, no conflicts produced
// ---------------------------------------------------------------------------

#[test]
fn isolated_workspace_cas_always_succeeds_no_conflicts() {
    let ref_store = MemRefStore::new();
    let snap_store = MemSnapshotStore::new();

    let base: Snapshot = HashMap::new();
    snap_store.seed("empty", base.clone());

    // First write.
    let r1 = ref_store.advance("isolated", None, "v1");
    assert!(
        matches!(r1, CasResult::Committed { .. }),
        "isolated: first write must succeed"
    );

    // Sequential advances (single writer): always provide the correct expected_old.
    let r2 = ref_store.advance("isolated", Some("v1"), "v2");
    assert!(
        matches!(r2, CasResult::Committed { .. }),
        "isolated: second advance must succeed"
    );

    let r3 = ref_store.advance("isolated", Some("v2"), "v3");
    assert!(
        matches!(r3, CasResult::Committed { .. }),
        "isolated: third advance must succeed"
    );

    assert_eq!(
        ref_store.read("isolated").as_deref(),
        Some("v3"),
        "isolated: ref must point to latest committed value"
    );

    // Single writer never reconciles: verify merge produces zero conflicts for a clean advance.
    let prev = snap(&[("file.txt", "v2-content")]);
    let current_local = snap(&[("file.txt", "v3-content")]);
    snap_store.seed("v2-snap", prev.clone());
    snap_store.seed("v3-snap", current_local.clone());

    let merge_result = merge_snapshots(&prev, &current_local, &current_local, "s");
    assert!(
        merge_result.conflicts.is_empty(),
        "single writer: identical winner and local must produce zero conflicts"
    );
}
