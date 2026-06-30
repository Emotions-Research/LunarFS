// Client-side reconcile loop: when a CAS advance is rejected, fetch the winning
// snapshot, merge per-path against the local overlay, commit the merged result,
// and re-advance via CAS. Bounded retry so pathological races terminate.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::merge::{merge_snapshots, Snapshot};
use crate::ref_advance::{CasResult, RefStore};

// ---------------------------------------------------------------------------
// SnapshotStore seam
// ---------------------------------------------------------------------------

/// Read and commit (persist) flat snapshots by an opaque root identifier.
///
/// Implementations must be deterministic: the same content may receive
/// different identifiers on separate calls, but a stored identifier must
/// always resolve to exactly the snapshot that was committed under it.
pub trait SnapshotStore: Send + Sync {
    /// Retrieve the snapshot for `root`, or None if not found.
    fn fetch(&self, root: &str) -> Option<Snapshot>;
    /// Persist `snapshot` and return a stable identifier for it.
    fn commit(&self, snapshot: Snapshot) -> String;
}

/// In-memory snapshot store for testing.
///
/// Uses a monotonically increasing counter as the root identifier so
/// no wall-clock or random source is needed.
pub struct MemSnapshotStore {
    snaps: Mutex<HashMap<String, Snapshot>>,
    counter: Mutex<u64>,
}

impl MemSnapshotStore {
    pub fn new() -> Self {
        Self {
            snaps: Mutex::new(HashMap::new()),
            counter: Mutex::new(0),
        }
    }

    /// Pre-populate the store with a known snapshot under a specific root id.
    /// Useful for seeding base/winner snapshots in tests.
    pub fn seed(&self, root: &str, snap: Snapshot) {
        assert!(!root.is_empty(), "root must not be empty");
        self.snaps
            .lock()
            .expect("snaps lock poisoned")
            .insert(root.to_string(), snap);
    }
}

impl Default for MemSnapshotStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SnapshotStore for MemSnapshotStore {
    fn fetch(&self, root: &str) -> Option<Snapshot> {
        assert!(!root.is_empty(), "root must not be empty");
        self.snaps
            .lock()
            .expect("snaps lock poisoned")
            .get(root)
            .cloned()
    }

    fn commit(&self, snapshot: Snapshot) -> String {
        // Increment counter first, then store under the new id.
        let id = {
            let mut c = self.counter.lock().expect("counter lock poisoned");
            *c += 1;
            format!("snap-{:016x}", *c)
        };
        self.snaps
            .lock()
            .expect("snaps lock poisoned")
            .insert(id.clone(), snapshot);
        id
    }
}

// ---------------------------------------------------------------------------
// Reconcile error
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ReconcileError {
    RefNotFound,
    SnapshotNotFound(String),
    MaxRetriesExceeded { attempts: usize },
}

impl std::fmt::Display for ReconcileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReconcileError::RefNotFound => write!(f, "reconcile: ref not found in store"),
            ReconcileError::SnapshotNotFound(r) => {
                write!(f, "reconcile: snapshot not found for root {}", r)
            }
            ReconcileError::MaxRetriesExceeded { attempts } => {
                write!(
                    f,
                    "reconcile: max retries ({}) exceeded without convergence",
                    attempts
                )
            }
        }
    }
}

impl std::error::Error for ReconcileError {}

// ---------------------------------------------------------------------------
// Reconcile loop
// ---------------------------------------------------------------------------

/// Reconcile a CAS rejection by merging and re-advancing until convergence.
///
/// Called after a CAS advance from `base_root` to some new root was rejected.
/// `local_overlay` is the FULL local snapshot (base + all local edits applied):
/// the merge computes per-path deltas against `base_root` to identify what
/// each side changed.
///
/// Loop (bounded by `max_retries`):
///   1. Read the current winning root from `ref_store`.
///   2. Merge: base + winner + local -> merged snapshot.
///   3. Commit merged snapshot -> merged_root.
///   4. CAS advance ref from winner_root to merged_root.
///      - On success: return merged_root.
///      - On mismatch: update winner_root to the new current, repeat.
///
/// Only SHARED workspaces ever call this function; isolated single-writer
/// workspaces never get a CAS mismatch.
pub fn reconcile(
    ref_store: &dyn RefStore,
    snapshot_store: &dyn SnapshotStore,
    ref_name: &str,
    base_root: &str,
    local_overlay: &Snapshot,
    conflict_suffix: &str,
    max_retries: usize,
) -> Result<String, ReconcileError> {
    assert!(!ref_name.is_empty(), "ref_name must not be empty");
    assert!(!base_root.is_empty(), "base_root must not be empty");
    assert!(
        !conflict_suffix.is_empty(),
        "conflict_suffix must not be empty"
    );
    assert!(max_retries >= 1, "max_retries must be at least 1");
    assert!(max_retries <= 100, "max_retries must not exceed 100");

    let base_snap = snapshot_store
        .fetch(base_root)
        .ok_or_else(|| ReconcileError::SnapshotNotFound(base_root.to_string()))?;

    let mut winner_root = ref_store
        .read(ref_name)
        .ok_or(ReconcileError::RefNotFound)?;

    for _ in 0..max_retries {
        let winner_snap = snapshot_store
            .fetch(&winner_root)
            .ok_or_else(|| ReconcileError::SnapshotNotFound(winner_root.clone()))?;

        let merge_result =
            merge_snapshots(&base_snap, &winner_snap, local_overlay, conflict_suffix);
        let merged_root = snapshot_store.commit(merge_result.merged);

        match ref_store.advance(ref_name, Some(&winner_root), &merged_root) {
            CasResult::Committed { value } => return Ok(value),
            CasResult::Mismatch { current } => {
                // A concurrent writer advanced again while we were merging.
                // Use the new current as the next winner and retry.
                winner_root = current;
            }
        }
    }

    Err(ReconcileError::MaxRetriesExceeded {
        attempts: max_retries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::FileState;
    use crate::ref_advance::MemRefStore;

    fn present(s: &str) -> FileState {
        FileState::Present(s.to_string())
    }

    fn snap(entries: &[(&str, &str)]) -> Snapshot {
        entries
            .iter()
            .map(|(p, c)| (p.to_string(), present(c)))
            .collect()
    }

    #[test]
    fn reconcile_merges_disjoint_changes() {
        let ref_store = MemRefStore::new();
        let snap_store = MemSnapshotStore::new();

        let base = snap(&[("shared.txt", "base")]);
        snap_store.seed("base", base.clone());
        ref_store.advance("ws", None, "base");

        // Writer A commits a.txt
        let winner = snap(&[("shared.txt", "base"), ("a.txt", "from-A")]);
        snap_store.seed("winner", winner);
        ref_store.advance("ws", Some("base"), "winner");

        // Writer B's local view includes b.txt
        let local = snap(&[("shared.txt", "base"), ("b.txt", "from-B")]);

        let final_root = reconcile(&ref_store, &snap_store, "ws", "base", &local, "s", 5)
            .expect("reconcile must succeed");

        let final_snap = snap_store
            .fetch(&final_root)
            .expect("final snapshot must exist");
        assert_eq!(final_snap.get("a.txt"), Some(&present("from-A")));
        assert_eq!(final_snap.get("b.txt"), Some(&present("from-B")));
        assert_eq!(final_snap.get("shared.txt"), Some(&present("base")));
        assert_eq!(ref_store.read("ws").as_deref(), Some(&final_root as &str));
    }

    #[test]
    fn reconcile_fails_on_too_many_retries() {
        use std::sync::Arc;

        // A ref store that always reports mismatch by advancing the ref after each reconcile.
        let ref_store = Arc::new(MemRefStore::new());
        let snap_store = MemSnapshotStore::new();

        let base = snap(&[("f.txt", "base")]);
        snap_store.seed("base", base.clone());
        ref_store.advance("ws", None, "base");

        // Pre-seed "adversarial-v*" snapshots so fetch never fails.
        for i in 1u64..=5 {
            snap_store.seed(&format!("adversarial-v{}", i), base.clone());
        }

        // After each reconcile attempt sets the ref to a new merged root,
        // a background writer immediately advances it again. We simulate this
        // by wrapping the store in a shim that always advances on read.
        // Instead, use max_retries=1 and make the winner always the current ref
        // so that commit + CAS gets a mismatch on every try.

        // Simpler: run 1 retry with a winner that keeps changing each time we
        // call advance. We do this by having two callers race in sequence:
        // after the first reconcile commits snap-X and tries CAS(winner -> snap-X),
        // manually advance the ref to something else first.

        // Actually, just verify the error variant is returned.
        // We'll use a store that refuses all advances.
        struct NeverCasStore;
        impl RefStore for NeverCasStore {
            fn advance(&self, _: &str, _: Option<&str>, _: &str) -> CasResult {
                CasResult::Mismatch {
                    current: "adversarial-v1".to_string(),
                }
            }
            fn read(&self, _: &str) -> Option<String> {
                Some("adversarial-v1".to_string())
            }
        }

        let result = reconcile(&NeverCasStore, &snap_store, "ws", "base", &base, "s", 3);
        assert!(
            matches!(
                result,
                Err(ReconcileError::MaxRetriesExceeded { attempts: 3 })
            ),
            "must error after max retries"
        );
    }
}
