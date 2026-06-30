// Integration tests: per-agent view grouping and overlap detection.
//
// Covers:
//   (a) group_by_base groups N seeded workspaces into the correct base_ref partition
//   (b) detect_overlaps flags a path touched by 2+ workspaces
//   (c) detect_overlaps does not flag a path touched by exactly 1 workspace
// Edge cases:
//   - single workspace: trivial group, no overlaps
//   - empty set: no groups, no panics
//   - identical content at same path by two workspaces: still flagged (path-based rule)
//   - two distinct base_refs: two groups, members partitioned correctly
//   - render_group_report: OVERLAPS section appears when overlaps exist
//
// Bound symbols (verified against source before writing):
//   devdropbox::ws_diff::{WorkspaceDiff, detect_overlaps, group_by_base, render_group_report}
//   devdropbox::workspace::{create_workspace, FakeClock, InMemoryBackend, Workspace, WorkspaceSpec, WsId}
//   devdropbox::store::InMemoryWorkspaceStore
//   devdropbox::patch::{Change, ChangeKind}
//
// Seeding mirrors the diff_base_ref recipe: create_workspace forks each workspace
// from a shared base_ref. WorkspaceDiff entries represent what each workspace wrote
// (the "blob write + commit root" step). No live CAS needed: group_by_base and
// detect_overlaps operate on the struct fields only.
//
// Independence: no tests/common module; all helpers are inlined in this file.

use devdropbox::patch::{Change, ChangeKind};
use devdropbox::store::InMemoryWorkspaceStore;
use devdropbox::workspace::{
    create_workspace, FakeClock, InMemoryBackend, Workspace, WorkspaceSpec, WsId,
};
use devdropbox::ws_diff::{detect_overlaps, group_by_base, render_group_report, WorkspaceDiff};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

// ── Seeding helpers (inlined per independence rule) ──────────────────────────

fn new_backend() -> InMemoryBackend {
    InMemoryBackend::new()
}

fn new_store() -> InMemoryWorkspaceStore {
    InMemoryWorkspaceStore::new()
}

fn spec(base_ref: &str) -> WorkspaceSpec {
    WorkspaceSpec {
        base_ref: base_ref.to_string(),
        label: None,
        metadata: BTreeMap::new(),
        ttl: None,
        root: None,
    }
}

/// Fork a workspace from base_ref and return the persisted record.
/// Mirrors the seeding recipe from diff_base_ref.rs.
fn fork_ws(
    backend: &InMemoryBackend,
    store: &InMemoryWorkspaceStore,
    id: &str,
    base_ref: &str,
) -> Workspace {
    let clk = FakeClock::new(UNIX_EPOCH);
    create_workspace(backend, store, &clk, WsId(id.to_string()), spec(base_ref))
        .expect("create_workspace must succeed")
}

fn make_change(path: &str, kind: ChangeKind) -> Change {
    Change {
        path: PathBuf::from(path),
        kind,
        old_size: None,
        new_size: None,
        old_blob: None,
        new_blob: None,
    }
}

/// Build a WorkspaceDiff from a Workspace and a list of path changes.
/// The diff id and label are taken directly from the workspace record,
/// matching how the production code builds WorkspaceDiff from stored workspaces.
fn make_diff(ws: &Workspace, changes: Vec<Change>) -> WorkspaceDiff {
    WorkspaceDiff {
        id: ws.id.0.clone(),
        label: ws.label.clone(),
        base_ref: ws.base_ref.clone(),
        changes,
    }
}

// ── (a) N-workspace grouping ─────────────────────────────────────────────────

/// group_by_base over N workspaces sharing one base_ref yields one group
/// whose membership exactly matches the seeded set.
#[test]
fn group_by_base_accounts_for_all_n_workspaces() {
    let backend = new_backend();
    let store = new_store();

    let ws_a = fork_ws(&backend, &store, "ws-view-a", "base-v1");
    let ws_b = fork_ws(&backend, &store, "ws-view-b", "base-v1");
    let ws_c = fork_ws(&backend, &store, "ws-view-c", "base-v1");

    let groups = group_by_base(&[ws_a, ws_b, ws_c]);

    assert_eq!(groups.len(), 1, "all workspaces share base-v1: exactly one group");
    assert_eq!(groups[0].base_ref, "base-v1");
    assert_eq!(
        groups[0].members.len(),
        3,
        "group must account for all 3 seeded workspaces"
    );

    let ids: Vec<&str> = groups[0].members.iter().map(|w| w.id.0.as_str()).collect();
    assert!(ids.contains(&"ws-view-a"), "ws-view-a must be present in the group");
    assert!(ids.contains(&"ws-view-b"), "ws-view-b must be present in the group");
    assert!(ids.contains(&"ws-view-c"), "ws-view-c must be present in the group");
}

// ── (b) Overlap flagging + (c) non-overlap control ──────────────────────────

/// detect_overlaps flags a path touched by 2+ workspaces and does not flag
/// a path touched by exactly 1 workspace.
#[test]
fn detect_overlaps_flags_shared_and_skips_unique() {
    let backend = new_backend();
    let store = new_store();

    let ws_a = fork_ws(&backend, &store, "ws-overlap-a", "base-v1");
    let ws_b = fork_ws(&backend, &store, "ws-overlap-b", "base-v1");

    // Shared path: both workspaces write "src/shared.rs" (the overlap).
    // Unique paths: each workspace writes one path the other does not (non-overlap controls).
    let diff_a = make_diff(
        &ws_a,
        vec![
            make_change("src/shared.rs", ChangeKind::Modified),
            make_change("src/only_a.rs", ChangeKind::Added),
        ],
    );
    let diff_b = make_diff(
        &ws_b,
        vec![
            make_change("src/shared.rs", ChangeKind::Modified),
            make_change("src/only_b.rs", ChangeKind::Added),
        ],
    );

    let overlaps = detect_overlaps(&[diff_a, diff_b]);

    // (b) shared path IS flagged
    assert!(
        overlaps.contains_key(&PathBuf::from("src/shared.rs")),
        "src/shared.rs touched by 2 workspaces must be flagged as overlapping"
    );
    let overlap_names = &overlaps[&PathBuf::from("src/shared.rs")];
    assert_eq!(
        overlap_names.len(),
        2,
        "exactly 2 workspace display names in the overlap list"
    );
    assert!(
        overlap_names.contains(&ws_a.id.0),
        "ws_a id must appear in the overlap list"
    );
    assert!(
        overlap_names.contains(&ws_b.id.0),
        "ws_b id must appear in the overlap list"
    );

    // (c) unique paths are NOT flagged
    assert!(
        !overlaps.contains_key(&PathBuf::from("src/only_a.rs")),
        "src/only_a.rs touched by 1 workspace must NOT be flagged"
    );
    assert!(
        !overlaps.contains_key(&PathBuf::from("src/only_b.rs")),
        "src/only_b.rs touched by 1 workspace must NOT be flagged"
    );
}

// ── Edge: single workspace ───────────────────────────────────────────────────

#[test]
fn single_workspace_trivial_group_no_overlaps() {
    let backend = new_backend();
    let store = new_store();

    let ws = fork_ws(&backend, &store, "ws-solo", "base-v1");
    let diff = make_diff(
        &ws,
        vec![
            make_change("a.rs", ChangeKind::Added),
            make_change("b.rs", ChangeKind::Added),
        ],
    );

    let groups = group_by_base(&[ws]);
    assert_eq!(groups.len(), 1, "single workspace yields exactly one group");
    assert_eq!(groups[0].members.len(), 1, "group has exactly one member");

    let overlaps = detect_overlaps(&[diff]);
    assert!(
        overlaps.is_empty(),
        "a single workspace cannot produce overlaps with itself"
    );
}

// ── Edge: empty set ──────────────────────────────────────────────────────────

#[test]
fn empty_workspace_set_no_groups_no_panic() {
    let groups = group_by_base(&[]);
    assert!(groups.is_empty(), "empty workspace list must yield no groups");

    let overlaps = detect_overlaps(&[]);
    assert!(overlaps.is_empty(), "empty diff list must yield no overlaps");
}

// ── Edge: identical content at same path → still flagged (path-based rule) ──

/// detect_overlaps is path-based, not content-based: two workspaces writing the
/// same bytes to the same path still constitutes an overlap.
#[test]
fn identical_content_at_same_path_still_flagged() {
    let backend = new_backend();
    let store = new_store();

    let ws_a = fork_ws(&backend, &store, "ws-same-a", "base-v1");
    let ws_b = fork_ws(&backend, &store, "ws-same-b", "base-v1");

    // Both workspaces write the same logical content to config.toml.
    // The implementation counts distinct workspace names per path, not content.
    let diff_a = make_diff(&ws_a, vec![make_change("config.toml", ChangeKind::Added)]);
    let diff_b = make_diff(&ws_b, vec![make_change("config.toml", ChangeKind::Added)]);

    let overlaps = detect_overlaps(&[diff_a, diff_b]);
    assert!(
        overlaps.contains_key(&PathBuf::from("config.toml")),
        "path-based rule: same path by 2 workspaces must be flagged even with identical content"
    );
}

// ── Edge: two distinct base_refs → two groups, members correctly partitioned ─

#[test]
fn two_base_refs_produce_two_partitioned_groups() {
    let backend = new_backend();
    let store = new_store();

    let ws_a1 = fork_ws(&backend, &store, "ws-alpha-1", "base-alpha");
    let ws_a2 = fork_ws(&backend, &store, "ws-alpha-2", "base-alpha");
    let ws_b1 = fork_ws(&backend, &store, "ws-beta-1", "base-beta");

    let groups = group_by_base(&[ws_a1, ws_a2, ws_b1]);

    assert_eq!(groups.len(), 2, "two distinct base_refs must yield two groups");

    let g_alpha = groups.iter().find(|g| g.base_ref == "base-alpha");
    let g_beta = groups.iter().find(|g| g.base_ref == "base-beta");

    assert!(g_alpha.is_some(), "base-alpha group must exist");
    assert!(g_beta.is_some(), "base-beta group must exist");

    assert_eq!(
        g_alpha.expect("base-alpha group was checked").members.len(),
        2,
        "base-alpha group must have 2 members"
    );
    assert_eq!(
        g_beta.expect("base-beta group was checked").members.len(),
        1,
        "base-beta group must have 1 member"
    );

    let beta_id = &g_beta.expect("base-beta group was checked").members[0].id.0;
    assert_eq!(beta_id, "ws-beta-1", "base-beta group must contain ws-beta-1");
}

// ── Coherence: render_group_report reflects overlaps in the full view ─────────

#[test]
fn render_group_report_includes_overlap_section_when_present() {
    let backend = new_backend();
    let store = new_store();

    let ws_a = fork_ws(&backend, &store, "ws-report-a", "base-v1");
    let ws_b = fork_ws(&backend, &store, "ws-report-b", "base-v1");

    let diffs = vec![
        make_diff(&ws_a, vec![make_change("src/shared.rs", ChangeKind::Modified)]),
        make_diff(&ws_b, vec![make_change("src/shared.rs", ChangeKind::Modified)]),
    ];

    let overlaps = detect_overlaps(&diffs);
    let report = render_group_report("base-v1", &diffs, &overlaps);

    assert!(
        report.contains("OVERLAPS (who stepped on whom)"),
        "report must contain the OVERLAPS section header"
    );
    assert!(
        report.contains("src/shared.rs"),
        "report must name the overlapping path"
    );
    assert!(
        report.contains("base-v1"),
        "report must reference the base_ref"
    );
}

#[test]
fn render_group_report_no_overlap_section_when_clean() {
    let backend = new_backend();
    let store = new_store();

    let ws_a = fork_ws(&backend, &store, "ws-clean-a", "base-v1");
    let ws_b = fork_ws(&backend, &store, "ws-clean-b", "base-v1");

    let diffs = vec![
        make_diff(&ws_a, vec![make_change("src/file_a.rs", ChangeKind::Added)]),
        make_diff(&ws_b, vec![make_change("src/file_b.rs", ChangeKind::Added)]),
    ];

    let overlaps = detect_overlaps(&diffs);
    assert!(overlaps.is_empty(), "no shared paths: overlaps must be empty");

    let report = render_group_report("base-v1", &diffs, &overlaps);
    assert!(
        report.contains("no overlapping paths in this group"),
        "clean report must say no overlapping paths"
    );
    assert!(
        !report.contains("OVERLAPS (who stepped on whom)"),
        "clean report must not contain the OVERLAPS header"
    );
}
