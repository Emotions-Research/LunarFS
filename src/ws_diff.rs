use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::patch::{Change, ChangeKind};
use crate::workspace::Workspace;

pub struct WorkspaceDiff {
    pub id: String,
    pub label: Option<String>,
    pub base_ref: String,
    pub changes: Vec<Change>,
}

pub struct BaseGroup {
    pub base_ref: String,
    pub members: Vec<Workspace>,
}

/// Returns the label when present and non-empty, else the id.
pub fn display_name(ws_id: &str, label: Option<&str>) -> String {
    assert!(!ws_id.is_empty(), "display_name: ws_id must not be empty");
    match label {
        Some(l) if !l.is_empty() => l.to_string(),
        _ => ws_id.to_string(),
    }
}

/// Groups workspaces by identical base_ref. Groups sorted by base_ref ascending;
/// within each group members sorted by (label-or-empty, id) ascending.
pub fn group_by_base(workspaces: &[Workspace]) -> Vec<BaseGroup> {
    assert!(
        workspaces.len() <= 1_000_000,
        "group_by_base: workspace count {} exceeds sanity cap",
        workspaces.len()
    );

    let mut map: BTreeMap<String, Vec<Workspace>> = BTreeMap::new();
    for ws in workspaces {
        map.entry(ws.base_ref.clone()).or_default().push(ws.clone());
    }

    // BTreeMap iterates in sorted key order, so groups are already in base_ref order.
    map.into_iter()
        .map(|(base_ref, mut members)| {
            members.sort_by(|a, b| {
                let ak = (a.label.as_deref().unwrap_or(""), a.id.0.as_str());
                let bk = (b.label.as_deref().unwrap_or(""), b.id.0.as_str());
                ak.cmp(&bk)
            });
            BaseGroup { base_ref, members }
        })
        .collect()
}

/// Returns only paths touched by 2+ DISTINCT workspaces (by display name).
/// Each path maps to a sorted list of the workspace display names that touched it.
pub fn detect_overlaps(diffs: &[WorkspaceDiff]) -> BTreeMap<PathBuf, Vec<String>> {
    assert!(
        diffs.len() <= 1_000_000,
        "detect_overlaps: diff count {} exceeds sanity cap",
        diffs.len()
    );

    let mut path_names: BTreeMap<PathBuf, BTreeMap<String, ()>> = BTreeMap::new();

    for diff in diffs {
        let name = display_name(&diff.id, diff.label.as_deref());
        for change in &diff.changes {
            path_names
                .entry(change.path.clone())
                .or_default()
                .insert(name.clone(), ());
        }
    }

    path_names
        .into_iter()
        .filter(|(_, names)| names.len() >= 2)
        .map(|(path, names)| {
            let mut name_vec: Vec<String> = names.into_keys().collect();
            name_vec.sort();
            (path, name_vec)
        })
        .collect()
}

/// Renders a labeled per-workspace changeset block plus a prominent OVERLAPS section.
pub fn render_group_report(
    base_ref: &str,
    diffs: &[WorkspaceDiff],
    overlaps: &BTreeMap<PathBuf, Vec<String>>,
) -> String {
    assert!(!base_ref.is_empty(), "render_group_report: base_ref must not be empty");
    assert!(
        diffs.len() <= 1_000_000,
        "render_group_report: diff count {} exceeds sanity cap",
        diffs.len()
    );

    let mut out = String::new();

    let base_short: String = base_ref.chars().take(16).collect();
    out.push_str(&format!(
        "=== base: {} ({} workspace{})\n",
        base_short,
        diffs.len(),
        if diffs.len() == 1 { "" } else { "s" }
    ));

    for diff in diffs {
        let name = display_name(&diff.id, diff.label.as_deref());
        out.push_str(&format!(
            "\n{} (id={}, {} changed):\n",
            name,
            diff.id,
            diff.changes.len()
        ));
        for change in &diff.changes {
            let marker = match change.kind {
                ChangeKind::Added => 'A',
                ChangeKind::Modified => 'M',
                ChangeKind::Deleted => 'D',
            };
            out.push(marker);
            out.push(' ');
            out.push_str(&change.path.display().to_string());
            out.push('\n');
        }
    }

    out.push('\n');
    if overlaps.is_empty() {
        out.push_str("no overlapping paths in this group\n");
    } else {
        out.push_str("OVERLAPS (who stepped on whom)\n");
        out.push_str("------------------------------\n");
        for (path, names) in overlaps {
            out.push_str(&format!(
                "{}  <- {}\n",
                path.display(),
                names.join(", ")
            ));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::ChangeKind;
    use crate::workspace::{Workspace, WsId};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::UNIX_EPOCH;

    fn make_ws(id: &str, label: Option<&str>, base_ref: &str) -> Workspace {
        Workspace {
            id: WsId(id.to_string()),
            label: label.map(|s| s.to_string()),
            metadata: BTreeMap::new(),
            base_ref: base_ref.to_string(),
            ttl: None,
            created_at: UNIX_EPOCH,
            ephemeral: false,
            root: None,
        }
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

    #[test]
    fn group_by_base_groups_shared_base() {
        let workspaces = vec![
            make_ws("ws-a1", Some("agent-a1"), "baseA"),
            make_ws("ws-b1", Some("agent-b1"), "baseB"),
            make_ws("ws-a2", Some("agent-a2"), "baseA"),
            make_ws("ws-b2", Some("agent-b2"), "baseB"),
        ];

        let groups = group_by_base(&workspaces);

        assert_eq!(groups.len(), 2, "must produce two groups");
        assert_eq!(groups[0].base_ref, "baseA", "first group is baseA (sorted)");
        assert_eq!(groups[1].base_ref, "baseB", "second group is baseB");

        assert_eq!(groups[0].members.len(), 2, "baseA has 2 members");
        assert_eq!(groups[1].members.len(), 2, "baseB has 2 members");

        // Members within baseA sorted by (label, id): agent-a1 < agent-a2
        assert_eq!(
            groups[0].members[0].label.as_deref(),
            Some("agent-a1"),
            "first baseA member is agent-a1"
        );
        assert_eq!(
            groups[0].members[1].label.as_deref(),
            Some("agent-a2"),
            "second baseA member is agent-a2"
        );
    }

    #[test]
    fn detect_overlaps_flags_shared_path() {
        let diffs = vec![
            WorkspaceDiff {
                id: "ws-1".to_string(),
                label: Some("agent-1".to_string()),
                base_ref: "baseA".to_string(),
                changes: vec![
                    make_change("src/x.rs", ChangeKind::Added),
                    make_change("src/only-in-1.rs", ChangeKind::Added),
                ],
            },
            WorkspaceDiff {
                id: "ws-2".to_string(),
                label: Some("agent-2".to_string()),
                base_ref: "baseA".to_string(),
                changes: vec![
                    make_change("src/x.rs", ChangeKind::Modified),
                    make_change("src/only-in-2.rs", ChangeKind::Added),
                ],
            },
        ];

        let overlaps = detect_overlaps(&diffs);

        assert!(
            overlaps.contains_key(&PathBuf::from("src/x.rs")),
            "src/x.rs must be flagged as overlapping"
        );
        let names = &overlaps[&PathBuf::from("src/x.rs")];
        assert!(names.contains(&"agent-1".to_string()), "agent-1 must be in overlap list");
        assert!(names.contains(&"agent-2".to_string()), "agent-2 must be in overlap list");

        assert!(
            !overlaps.contains_key(&PathBuf::from("src/only-in-1.rs")),
            "path touched by only one workspace must not appear in overlaps"
        );
        assert!(
            !overlaps.contains_key(&PathBuf::from("src/only-in-2.rs")),
            "path touched by only one workspace must not appear in overlaps"
        );
    }

    #[test]
    fn render_group_report_contains_overlap_section() {
        let diffs = vec![
            WorkspaceDiff {
                id: "ws-1".to_string(),
                label: Some("agent-1".to_string()),
                base_ref: "base123".to_string(),
                changes: vec![make_change("src/shared.rs", ChangeKind::Modified)],
            },
            WorkspaceDiff {
                id: "ws-2".to_string(),
                label: Some("agent-2".to_string()),
                base_ref: "base123".to_string(),
                changes: vec![make_change("src/shared.rs", ChangeKind::Modified)],
            },
        ];

        let overlaps = detect_overlaps(&diffs);
        let report = render_group_report("base123", &diffs, &overlaps);

        assert!(
            report.contains("OVERLAPS (who stepped on whom)"),
            "report must contain the OVERLAPS section header"
        );
        assert!(
            report.contains("src/shared.rs"),
            "report must name the overlapping path"
        );

        // Empty overlaps case
        let no_overlaps: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
        let report_clean = render_group_report("base123", &diffs, &no_overlaps);
        assert!(
            report_clean.contains("no overlapping paths in this group"),
            "report with no overlaps must say so"
        );
    }

    #[test]
    fn detect_overlaps_dedupes_same_workspace() {
        // A single workspace listing the same path twice must NOT self-overlap.
        let diffs = vec![WorkspaceDiff {
            id: "ws-solo".to_string(),
            label: Some("solo".to_string()),
            base_ref: "base".to_string(),
            changes: vec![
                make_change("src/x.rs", ChangeKind::Added),
                make_change("src/x.rs", ChangeKind::Modified),
            ],
        }];

        let overlaps = detect_overlaps(&diffs);
        assert!(
            overlaps.is_empty(),
            "a single workspace touching a path twice must not produce a self-overlap"
        );
    }
}
