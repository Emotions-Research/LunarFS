// Integration tests for the diff_trees engine.
//
// Covers: Added/Modified/Deleted classification, empty-diff on identical trees,
// and the unchanged-subtree pruning property.
//
// Bound symbols (verified against source before writing):
//   devdropbox::patch::{diff_trees, Change, ChangeKind}
//   devdropbox::cas::{Hash, MemStore}
//   devdropbox::tree::{serialize_tree, TreeEntry, MODE_DIR, MODE_FILE}
//
// No proptest (not a dev-dependency): proptest-style coverage uses table-driven cases.
// No shared common module: seeding helpers are inlined per the independence rule.

use devdropbox::cas::{Hash, MemStore, Store};
use devdropbox::patch::{diff_trees, Change, ChangeKind};
use devdropbox::tree::{serialize_tree, TreeEntry, MODE_DIR, MODE_FILE};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Seeding helpers (inlined; do not use tests/common)
// ---------------------------------------------------------------------------

fn put_blob(cas: &MemStore, data: &[u8]) -> Hash {
    cas.put(data).expect("put_blob must succeed")
}

fn put_tree(cas: &MemStore, entries: &[TreeEntry]) -> Hash {
    let bytes = serialize_tree(entries);
    cas.put(&bytes).expect("put_tree must succeed")
}

/// Build a flat (no subdirs) tree from (name, content) pairs.
fn flat_tree(cas: &MemStore, files: &[(&str, &[u8])]) -> Hash {
    let entries: Vec<TreeEntry> = files
        .iter()
        .map(|(name, data)| {
            let hash = put_blob(cas, data);
            TreeEntry {
                mode: MODE_FILE,
                name: name.to_string(),
                hash,
            }
        })
        .collect();
    put_tree(cas, &entries)
}

fn empty_tree(cas: &MemStore) -> Hash {
    put_tree(cas, &[])
}

fn paths_with_kind(changes: &[Change], kind: ChangeKind) -> Vec<PathBuf> {
    changes
        .iter()
        .filter(|c| c.kind == kind)
        .map(|c| c.path.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// (a) Added: a file present only in the new tree is reported as Added exactly once.
// ---------------------------------------------------------------------------

#[test]
fn added_file_classified_exactly_once() {
    let cas = MemStore::new();

    let old_root = flat_tree(&cas, &[("existing.txt", b"existing")]);
    let new_root = flat_tree(&cas, &[("existing.txt", b"existing"), ("added.txt", b"brand new")]);

    let mut changes = diff_trees(old_root, new_root, &cas).expect("diff must succeed");
    changes.sort_by(|a, b| a.path.cmp(&b.path));

    let added = paths_with_kind(&changes, ChangeKind::Added);
    assert_eq!(added.len(), 1, "exactly one Added entry; got: {:?}", added);
    assert_eq!(added[0], PathBuf::from("added.txt"));

    let added_entry = changes.iter().find(|c| c.kind == ChangeKind::Added).unwrap();
    assert_eq!(added_entry.old_size, None, "Added must have no old_size");
    assert_eq!(
        added_entry.new_size,
        Some(b"brand new".len() as u64),
        "Added new_size must equal content length"
    );

    let not_added: Vec<_> = changes.iter().filter(|c| c.kind != ChangeKind::Added).collect();
    assert!(
        not_added.is_empty(),
        "unchanged file must not appear in diff; got: {:?}",
        not_added.iter().map(|c| &c.path).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// (b) Modified: a file with a changed blob is Modified, never Added or Deleted.
// ---------------------------------------------------------------------------

#[test]
fn modified_file_not_added_or_deleted() {
    let cas = MemStore::new();

    let old_root = flat_tree(&cas, &[("file.txt", b"old content")]);
    let new_root = flat_tree(&cas, &[("file.txt", b"new content")]);

    let changes = diff_trees(old_root, new_root, &cas).expect("diff must succeed");

    assert_eq!(changes.len(), 1, "exactly one change");
    assert_eq!(changes[0].kind, ChangeKind::Modified, "must be Modified");
    assert_eq!(changes[0].path, PathBuf::from("file.txt"));
    assert_eq!(
        changes[0].old_size,
        Some(b"old content".len() as u64),
        "old_size must equal old content length"
    );
    assert_eq!(
        changes[0].new_size,
        Some(b"new content".len() as u64),
        "new_size must equal new content length"
    );

    assert!(
        paths_with_kind(&changes, ChangeKind::Added).is_empty(),
        "Modified file must not appear as Added"
    );
    assert!(
        paths_with_kind(&changes, ChangeKind::Deleted).is_empty(),
        "Modified file must not appear as Deleted"
    );
}

// ---------------------------------------------------------------------------
// (c) Deleted: a file present only in the old tree is reported as Deleted exactly once.
// ---------------------------------------------------------------------------

#[test]
fn deleted_file_classified_exactly_once() {
    let cas = MemStore::new();

    let old_root = flat_tree(&cas, &[("kept.txt", b"keep me"), ("gone.txt", b"bye")]);
    let new_root = flat_tree(&cas, &[("kept.txt", b"keep me")]);

    let mut changes = diff_trees(old_root, new_root, &cas).expect("diff must succeed");
    changes.sort_by(|a, b| a.path.cmp(&b.path));

    let deleted = paths_with_kind(&changes, ChangeKind::Deleted);
    assert_eq!(deleted.len(), 1, "exactly one Deleted entry; got: {:?}", deleted);
    assert_eq!(deleted[0], PathBuf::from("gone.txt"));

    let deleted_entry = changes.iter().find(|c| c.kind == ChangeKind::Deleted).unwrap();
    assert_eq!(
        deleted_entry.old_size,
        Some(b"bye".len() as u64),
        "Deleted old_size must equal content length"
    );
    assert_eq!(deleted_entry.new_size, None, "Deleted must have no new_size");

    let not_deleted: Vec<_> = changes.iter().filter(|c| c.kind != ChangeKind::Deleted).collect();
    assert!(
        not_deleted.is_empty(),
        "unchanged file must not appear in diff; got: {:?}",
        not_deleted.iter().map(|c| &c.path).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// (d) Empty diff: identical trees yield zero changes.
// ---------------------------------------------------------------------------

#[test]
fn identical_trees_yield_empty_diff() {
    let cas = MemStore::new();

    let root = flat_tree(&cas, &[("a.txt", b"alpha"), ("b.txt", b"beta")]);
    let changes = diff_trees(root, root, &cas).expect("diff must succeed");

    assert!(
        changes.is_empty(),
        "identical tree hashes must produce empty diff; got: {:?}",
        changes.iter().map(|c| &c.path).collect::<Vec<_>>()
    );
}

// Edge case: two independently built empty trees share the same hash (CAS is
// content-addressed), so the diff is also empty.
#[test]
fn two_empty_trees_yield_empty_diff() {
    let cas = MemStore::new();

    let root1 = empty_tree(&cas);
    let root2 = empty_tree(&cas);

    assert_eq!(root1, root2, "two empty trees must share the same CAS hash");

    let changes = diff_trees(root1, root2, &cas).expect("diff must succeed");
    assert!(changes.is_empty(), "empty vs empty must produce empty diff");
}

// Table-driven: several same-content configurations all produce empty diffs.
#[test]
fn table_identical_trees_always_empty() {
    let cases: &[(&str, &[(&str, &[u8])])] = &[
        ("empty", &[]),
        ("single_file", &[("x.txt", b"hello")]),
        ("multi_file", &[("a.txt", b"a"), ("b.txt", b"b"), ("c.txt", b"c")]),
        ("large_content", &[("big.txt", &[b'z'; 4096])]),
    ];

    for (name, files) in cases {
        let cas = MemStore::new();
        let root = flat_tree(&cas, files);
        let result = diff_trees(root, root, &cas).expect("diff must succeed");
        assert!(
            result.is_empty(),
            "case '{}': identical roots must yield empty diff; got {} entries",
            name,
            result.len()
        );
    }
}

// ---------------------------------------------------------------------------
// (e) Unchanged-subtree pruning: entries inside an unchanged sibling are absent.
//
// The diff engine prunes at the entry level: when old_entry.hash == new_entry.hash
// for a subtree entry, it skips it entirely (no WorkItem::Diff is pushed for it).
// Since the public diff_trees API does not expose a traversal counter, pruning is
// proven by the absence of any changed path inside the stable subtree.
// ---------------------------------------------------------------------------

#[test]
fn unchanged_subtree_entries_not_in_diff() {
    let cas = MemStore::new();

    // "stable/" subtree: two files, hash is the same on both sides.
    let h_s1 = put_blob(&cas, b"stable file 1 content");
    let h_s2 = put_blob(&cas, b"stable file 2 content");
    let stable_subtree = put_tree(
        &cas,
        &[
            TreeEntry { mode: MODE_FILE, name: "s1.txt".into(), hash: h_s1 },
            TreeEntry { mode: MODE_FILE, name: "s2.txt".into(), hash: h_s2 },
        ],
    );

    // "changed/" subtree: one file whose blob differs between old and new.
    let h_old = put_blob(&cas, b"old value");
    let old_changed = put_tree(
        &cas,
        &[TreeEntry { mode: MODE_FILE, name: "work.txt".into(), hash: h_old }],
    );
    let h_new = put_blob(&cas, b"new value");
    let new_changed = put_tree(
        &cas,
        &[TreeEntry { mode: MODE_FILE, name: "work.txt".into(), hash: h_new }],
    );

    // Both roots reference the same stable_subtree hash.
    let old_root = put_tree(
        &cas,
        &[
            TreeEntry { mode: MODE_DIR, name: "stable".into(), hash: stable_subtree },
            TreeEntry { mode: MODE_DIR, name: "changed".into(), hash: old_changed },
        ],
    );
    let new_root = put_tree(
        &cas,
        &[
            TreeEntry { mode: MODE_DIR, name: "stable".into(), hash: stable_subtree },
            TreeEntry { mode: MODE_DIR, name: "changed".into(), hash: new_changed },
        ],
    );

    let changes = diff_trees(old_root, new_root, &cas).expect("diff must succeed");

    assert_eq!(
        changes.len(),
        1,
        "only changed/work.txt must appear; got: {:?}",
        changes.iter().map(|c| (&c.path, &c.kind)).collect::<Vec<_>>()
    );
    assert_eq!(changes[0].path, PathBuf::from("changed/work.txt"));
    assert_eq!(changes[0].kind, ChangeKind::Modified);

    // Pruning: the stable/ subtree was short-circuited; no stable/ paths in diff.
    let leaked: Vec<_> = changes
        .iter()
        .filter(|c| c.path.starts_with("stable"))
        .map(|c| &c.path)
        .collect();
    assert!(
        leaked.is_empty(),
        "stable/ subtree must be pruned from the diff; leaked: {:?}",
        leaked
    );
}

// Three sibling subtrees: two are stable, one changes. Both stable ones are pruned.
#[test]
fn two_stable_siblings_pruned_when_one_changes() {
    let cas = MemStore::new();

    let h_a = put_blob(&cas, b"alpha content");
    let tree_alpha = put_tree(
        &cas,
        &[TreeEntry { mode: MODE_FILE, name: "fa.txt".into(), hash: h_a }],
    );

    let h_b = put_blob(&cas, b"beta content");
    let tree_beta = put_tree(
        &cas,
        &[TreeEntry { mode: MODE_FILE, name: "fb.txt".into(), hash: h_b }],
    );

    let h_old_c = put_blob(&cas, b"gamma old");
    let old_gamma = put_tree(
        &cas,
        &[TreeEntry { mode: MODE_FILE, name: "fc.txt".into(), hash: h_old_c }],
    );
    let h_new_c = put_blob(&cas, b"gamma new");
    let new_gamma = put_tree(
        &cas,
        &[TreeEntry { mode: MODE_FILE, name: "fc.txt".into(), hash: h_new_c }],
    );

    let old_root = put_tree(
        &cas,
        &[
            TreeEntry { mode: MODE_DIR, name: "alpha".into(), hash: tree_alpha },
            TreeEntry { mode: MODE_DIR, name: "beta".into(), hash: tree_beta },
            TreeEntry { mode: MODE_DIR, name: "gamma".into(), hash: old_gamma },
        ],
    );
    let new_root = put_tree(
        &cas,
        &[
            TreeEntry { mode: MODE_DIR, name: "alpha".into(), hash: tree_alpha },
            TreeEntry { mode: MODE_DIR, name: "beta".into(), hash: tree_beta },
            TreeEntry { mode: MODE_DIR, name: "gamma".into(), hash: new_gamma },
        ],
    );

    let changes = diff_trees(old_root, new_root, &cas).expect("diff must succeed");

    assert_eq!(changes.len(), 1, "one change expected; got: {:?}", changes.iter().map(|c| &c.path).collect::<Vec<_>>());
    assert_eq!(changes[0].path, PathBuf::from("gamma/fc.txt"));
    assert_eq!(changes[0].kind, ChangeKind::Modified);

    let leaked: Vec<_> = changes
        .iter()
        .filter(|c| c.path.starts_with("alpha") || c.path.starts_with("beta"))
        .map(|c| &c.path)
        .collect();
    assert!(
        leaked.is_empty(),
        "alpha/ and beta/ must be pruned from the diff; leaked: {:?}",
        leaked
    );
}

// ---------------------------------------------------------------------------
// Edge case: type change (file in one tree, directory in the other).
//
// The implementation handles this as: the old kind is Deleted (or all its
// contents are Deleted) and the new kind is Added (or all its contents are
// Added), depending on which direction the type changes.
// This test asserts whatever the implementation defines (no assumption made).
// ---------------------------------------------------------------------------

#[test]
fn type_change_file_to_dir_is_classified_coherently() {
    let cas = MemStore::new();

    // Old: "thing" is a plain file.
    let h_old_blob = put_blob(&cas, b"was a file");
    let old_root = put_tree(
        &cas,
        &[TreeEntry { mode: MODE_FILE, name: "thing".into(), hash: h_old_blob }],
    );

    // New: "thing" is a directory containing one file.
    let h_inner = put_blob(&cas, b"inner file content");
    let inner_tree = put_tree(
        &cas,
        &[TreeEntry { mode: MODE_FILE, name: "inner.txt".into(), hash: h_inner }],
    );
    let new_root = put_tree(
        &cas,
        &[TreeEntry { mode: MODE_DIR, name: "thing".into(), hash: inner_tree }],
    );

    let mut changes = diff_trees(old_root, new_root, &cas).expect("diff must succeed");
    changes.sort_by(|a, b| a.path.cmp(&b.path));

    // The diff must not be empty: "thing" changed type.
    assert!(
        !changes.is_empty(),
        "a file-to-dir type change must produce at least one change"
    );

    // The old file path must appear as Deleted.
    let old_file_deleted = changes.iter().any(|c| {
        c.path == PathBuf::from("thing") && c.kind == ChangeKind::Deleted
    });
    assert!(
        old_file_deleted,
        "old file path must be Deleted on a file-to-dir type change; changes: {:?}",
        changes.iter().map(|c| (&c.path, &c.kind)).collect::<Vec<_>>()
    );

    // The new directory's contents must appear as Added.
    let inner_added = changes.iter().any(|c| {
        c.path == PathBuf::from("thing/inner.txt") && c.kind == ChangeKind::Added
    });
    assert!(
        inner_added,
        "new dir contents must be Added on a file-to-dir type change; changes: {:?}",
        changes.iter().map(|c| (&c.path, &c.kind)).collect::<Vec<_>>()
    );
}

#[test]
fn type_change_dir_to_file_is_classified_coherently() {
    let cas = MemStore::new();

    // Old: "thing" is a directory containing one file.
    let h_inner = put_blob(&cas, b"was inner content");
    let old_dir = put_tree(
        &cas,
        &[TreeEntry { mode: MODE_FILE, name: "child.txt".into(), hash: h_inner }],
    );
    let old_root = put_tree(
        &cas,
        &[TreeEntry { mode: MODE_DIR, name: "thing".into(), hash: old_dir }],
    );

    // New: "thing" is a plain file.
    let h_new_blob = put_blob(&cas, b"now a file");
    let new_root = put_tree(
        &cas,
        &[TreeEntry { mode: MODE_FILE, name: "thing".into(), hash: h_new_blob }],
    );

    let mut changes = diff_trees(old_root, new_root, &cas).expect("diff must succeed");
    changes.sort_by(|a, b| a.path.cmp(&b.path));

    assert!(
        !changes.is_empty(),
        "a dir-to-file type change must produce at least one change"
    );

    // Old directory contents must appear as Deleted.
    let inner_deleted = changes.iter().any(|c| {
        c.path == PathBuf::from("thing/child.txt") && c.kind == ChangeKind::Deleted
    });
    assert!(
        inner_deleted,
        "old dir contents must be Deleted on a dir-to-file type change; changes: {:?}",
        changes.iter().map(|c| (&c.path, &c.kind)).collect::<Vec<_>>()
    );

    // The new file path must appear as Added.
    let new_file_added = changes.iter().any(|c| {
        c.path == PathBuf::from("thing") && c.kind == ChangeKind::Added
    });
    assert!(
        new_file_added,
        "new file path must be Added on a dir-to-file type change; changes: {:?}",
        changes.iter().map(|c| (&c.path, &c.kind)).collect::<Vec<_>>()
    );
}
