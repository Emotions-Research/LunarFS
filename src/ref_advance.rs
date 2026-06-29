// Server-side CAS ref advance: atomic compare-and-swap for workspace refs.
// The MemRefStore implementation is used directly in tests; the HTTP server
// enforces the same semantics via object_store's PutMode::Update.

use std::collections::HashMap;
use std::sync::Mutex;

/// Result of a CAS ref advance attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CasResult {
    /// The advance succeeded; `value` is the now-committed ref value.
    Committed { value: String },
    /// Another writer advanced the ref first; `current` is what the ref holds now.
    Mismatch { current: String },
}

/// Atomic ref store: compare-and-swap semantics for named string refs.
///
/// Implementations must ensure the read-compare-write is atomic with respect
/// to concurrent callers: two simultaneous advances from the same expected_old
/// must result in exactly one Committed and one Mismatch.
pub trait RefStore: Send + Sync {
    /// Atomically advance ref `name` from `expected_old` to `new_value`.
    ///
    /// - `expected_old = None`: write only if the ref does not yet exist (first-write CAS).
    /// - `expected_old = Some(v)`: write only if the current value equals `v`.
    ///
    /// Returns Committed on success with the stored value, or Mismatch with
    /// the actual current value so the caller can reconcile without a second trip.
    fn advance(&self, name: &str, expected_old: Option<&str>, new_value: &str) -> CasResult;

    /// Read the current ref value, or None if the ref has not been set.
    fn read(&self, name: &str) -> Option<String>;
}

/// In-process CAS ref store backed by a single Mutex<HashMap>.
///
/// The Mutex ensures the compare and swap happen atomically: the lock is held
/// from the read through the conditional write, so no two callers can both
/// observe the same old value and both succeed.
pub struct MemRefStore {
    refs: Mutex<HashMap<String, String>>,
}

impl MemRefStore {
    pub fn new() -> Self {
        Self { refs: Mutex::new(HashMap::new()) }
    }
}

impl Default for MemRefStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RefStore for MemRefStore {
    fn advance(&self, name: &str, expected_old: Option<&str>, new_value: &str) -> CasResult {
        assert!(!name.is_empty(), "ref name must not be empty");
        assert!(!new_value.is_empty(), "new_value must not be empty");
        // Hold the lock for the entire compare-and-swap to prevent lost updates.
        let mut refs = self.refs.lock().expect("MemRefStore lock poisoned");
        let current = refs.get(name).cloned();
        match (expected_old, &current) {
            (None, None) => {
                // First-write CAS: ref absent, no expected value -> create.
                refs.insert(name.to_string(), new_value.to_string());
                CasResult::Committed { value: new_value.to_string() }
            }
            (None, Some(cur)) => {
                // First-write CAS but ref already exists -> mismatch.
                CasResult::Mismatch { current: cur.clone() }
            }
            (Some(_), None) => {
                // Update CAS but ref does not exist yet -> mismatch (use empty string as sentinel).
                CasResult::Mismatch { current: String::new() }
            }
            (Some(expected), Some(cur)) => {
                if cur.as_str() == expected {
                    refs.insert(name.to_string(), new_value.to_string());
                    CasResult::Committed { value: new_value.to_string() }
                } else {
                    CasResult::Mismatch { current: cur.clone() }
                }
            }
        }
    }

    fn read(&self, name: &str) -> Option<String> {
        assert!(!name.is_empty(), "ref name must not be empty");
        self.refs.lock().expect("MemRefStore lock poisoned").get(name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_write_cas_succeeds_when_absent() {
        let store = MemRefStore::new();
        let result = store.advance("main", None, "v1");
        assert_eq!(result, CasResult::Committed { value: "v1".to_string() });
        assert_eq!(store.read("main"), Some("v1".to_string()));
    }

    #[test]
    fn first_write_cas_fails_when_ref_exists() {
        let store = MemRefStore::new();
        store.advance("main", None, "v1");
        let result = store.advance("main", None, "v2");
        assert!(
            matches!(result, CasResult::Mismatch { current } if current == "v1"),
            "second first-write must return mismatch with current value"
        );
        assert_eq!(store.read("main"), Some("v1".to_string()), "ref must not change on mismatch");
    }

    #[test]
    fn update_cas_succeeds_when_expected_matches() {
        let store = MemRefStore::new();
        store.advance("main", None, "v1");
        let result = store.advance("main", Some("v1"), "v2");
        assert_eq!(result, CasResult::Committed { value: "v2".to_string() });
        assert_eq!(store.read("main"), Some("v2".to_string()));
    }

    #[test]
    fn update_cas_fails_when_expected_stale() {
        let store = MemRefStore::new();
        store.advance("main", None, "v1");
        let result = store.advance("main", Some("old"), "v2");
        assert!(
            matches!(result, CasResult::Mismatch { current } if current == "v1"),
            "stale expected must return mismatch with actual current"
        );
        assert_eq!(store.read("main"), Some("v1".to_string()));
    }

    #[test]
    fn racing_advances_first_writer_wins() {
        use std::sync::Arc;
        let store = Arc::new(MemRefStore::new());
        store.advance("ws", None, "base");

        // Simulate two concurrent advances from the same expected_old "base".
        let s1 = Arc::clone(&store);
        let s2 = Arc::clone(&store);

        let r1 = s1.advance("ws", Some("base"), "v-A");
        let r2 = s2.advance("ws", Some("base"), "v-B");

        // Exactly one committed, one mismatch.
        let committed_count = [&r1, &r2]
            .iter()
            .filter(|r| matches!(r, CasResult::Committed { .. }))
            .count();
        let mismatch_count = [&r1, &r2]
            .iter()
            .filter(|r| matches!(r, CasResult::Mismatch { .. }))
            .count();
        assert_eq!(committed_count, 1, "exactly one writer must win");
        assert_eq!(mismatch_count, 1, "exactly one writer must lose");
    }

    #[test]
    fn read_absent_ref_returns_none() {
        let store = MemRefStore::new();
        assert_eq!(store.read("nonexistent"), None);
    }

    #[test]
    fn update_cas_when_ref_absent_returns_mismatch() {
        let store = MemRefStore::new();
        let result = store.advance("main", Some("expected"), "new");
        assert!(
            matches!(result, CasResult::Mismatch { current } if current.is_empty()),
            "update CAS on absent ref must return mismatch with empty sentinel"
        );
    }
}
