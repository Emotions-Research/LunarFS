//! High-level run-in-workspace API encoding the Nyx worker-spawner lifecycle
//! as an explicit state machine.
//!
//! States: Forked -> Mounted -> Running -> (Promoted | Discarded) -> Destroyed.
//!
//! The ephemeral overlay is destroyed on every terminal path: discard, closure
//! panic, backend error in mount or promote. On the keep path, `promote` persists
//! the artifact to durable storage before the overlay is explicitly destroyed.

use std::fmt;
use std::path::Path;

/// Caller closure verdict: keep the produced value, or discard the workspace.
pub enum Disposition<T> {
    /// Promote this value; the workspace artifact is persisted durably.
    Keep(T),
    /// Discard the workspace; nothing is promoted.
    Discard,
}

/// Stable handle to one live ephemeral overlay.
///
/// Created by `WorkspaceBackend::fork` and passed to `mount`, `promote`, and
/// `destroy`. Invalid after `destroy` completes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceId(pub String);

/// Durable reference to a promoted artifact. Independent of the overlay:
/// survives `destroy` and outlives the workspace lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotedRef(pub String);

/// Backend abstraction over workspace fork, mount, promote, and destroy.
///
/// The production implementation uses APFS clonefile (CoW) or FUSE overlays.
/// Tests supply an in-memory implementation with no filesystem access.
pub trait WorkspaceBackend {
    /// Token returned by `mount` and passed to the caller closure.
    type Mount;
    /// Error type for all backend operations.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Fork a new ephemeral overlay from `base`. O(1) in CoW implementations.
    fn fork(&self, base: &Path) -> Result<WorkspaceId, Self::Error>;
    /// Mount the overlay identified by `id`, returning a token for the caller.
    fn mount(&self, id: &WorkspaceId) -> Result<Self::Mount, Self::Error>;
    /// Promote the overlay artifact to durable storage. Called only on the keep path.
    fn promote(&self, id: &WorkspaceId) -> Result<PromotedRef, Self::Error>;
    /// Destroy the ephemeral overlay, releasing all associated resources.
    fn destroy(&self, id: &WorkspaceId) -> Result<(), Self::Error>;
}

/// Successful outcome of `run_in_workspace`.
pub enum Outcome<T> {
    /// The closure chose Keep; artifact is at `promoted`, overlay is destroyed.
    Kept {
        /// Value the closure returned inside `Disposition::Keep`.
        value: T,
        /// Durable reference to the promoted artifact.
        promoted: PromotedRef,
    },
    /// The closure chose Discard; overlay is destroyed, nothing promoted.
    Discarded,
}

/// Error covering each state transition in `run_in_workspace`.
///
/// In all error cases except `Destroy`, the ephemeral overlay has been
/// destroyed before this error is returned.
pub enum RunError<E> {
    /// `fork` failed; no overlay was created.
    Fork(E),
    /// `mount` failed; the forked overlay has been destroyed.
    Mount(E),
    /// `promote` failed; the overlay has been destroyed.
    Promote(E),
    /// `destroy` failed after a successful promote; the promoted artifact is durable.
    Destroy(E),
    /// The caller closure panicked; the overlay has been destroyed.
    ClosurePanicked,
}

impl<E: fmt::Display> fmt::Display for RunError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fork(e) => write!(f, "workspace fork failed: {}", e),
            Self::Mount(e) => write!(f, "workspace mount failed: {}", e),
            Self::Promote(e) => write!(f, "workspace promote failed: {}", e),
            Self::Destroy(e) => write!(f, "workspace destroy failed: {}", e),
            Self::ClosurePanicked => write!(f, "caller closure panicked"),
        }
    }
}

impl<E: fmt::Debug> fmt::Debug for RunError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fork(e) => write!(f, "RunError::Fork({:?})", e),
            Self::Mount(e) => write!(f, "RunError::Mount({:?})", e),
            Self::Promote(e) => write!(f, "RunError::Promote({:?})", e),
            Self::Destroy(e) => write!(f, "RunError::Destroy({:?})", e),
            Self::ClosurePanicked => write!(f, "RunError::ClosurePanicked"),
        }
    }
}

impl<E: std::error::Error + Send + Sync + 'static> std::error::Error for RunError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Fork(e) | Self::Mount(e) | Self::Promote(e) | Self::Destroy(e) => Some(e),
            Self::ClosurePanicked => None,
        }
    }
}

/// RAII guard ensuring `destroy` fires on every exit path except an explicit
/// successful promote+destroy cycle.
///
/// When `deactivated` is false (the default), `Drop` calls `backend.destroy`.
/// The keep path sets `deactivated = true` before calling `destroy` manually,
/// preventing a double-destroy attempt.
struct CleanupGuard<'b, B: WorkspaceBackend> {
    backend: &'b B,
    id: WorkspaceId,
    deactivated: bool,
}

impl<B: WorkspaceBackend> Drop for CleanupGuard<'_, B> {
    fn drop(&mut self) {
        if !self.deactivated {
            // Ignore errors: we are already on an error or discard path.
            let _ = self.backend.destroy(&self.id);
        }
    }
}

/// Run a caller-supplied closure inside an ephemeral isolated workspace.
///
/// The state machine proceeds: Fork -> Mount -> Run -> (Keep | Discard) -> Destroy.
/// The overlay is destroyed on every terminal path, including closure panics
/// and backend errors in `mount` or `promote`.
///
/// On the keep path: `promote` persists the artifact durably, then `destroy`
/// removes the ephemeral overlay. `Outcome::Kept` carries both the caller's
/// value and the durable `PromotedRef`.
///
/// On the discard path and all error paths: the overlay is destroyed and
/// nothing is promoted.
pub fn run_in_workspace<B, F, T>(
    backend: &B,
    base: &Path,
    f: F,
) -> Result<Outcome<T>, RunError<B::Error>>
where
    B: WorkspaceBackend,
    F: FnOnce(&B::Mount) -> Disposition<T>,
{
    assert!(!base.as_os_str().is_empty(), "base path must not be empty");

    // State: Forked. Guard not yet live; a fork error leaves nothing to clean.
    let id = backend.fork(base).map_err(RunError::Fork)?;

    // Guard is live from here. Drop fires destroy on any early return.
    let mut guard = CleanupGuard {
        backend,
        id,
        deactivated: false,
    };

    // State: Mounted (or error -> Destroyed via guard Drop).
    let mount_token = backend.mount(&guard.id).map_err(RunError::Mount)?;

    // State: Running. Wrap in AssertUnwindSafe: cleanup is guaranteed by the
    // guard's Drop, so crossing the unwind boundary here is safe.
    let run_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&mount_token)));

    let disposition = run_result.map_err(|_| RunError::ClosurePanicked)?;

    match disposition {
        // State: Discarded -> Destroyed (guard fires on function return).
        Disposition::Discard => Ok(Outcome::Discarded),

        // State: Promoted -> Destroyed (explicit destroy after successful promote).
        Disposition::Keep(value) => {
            let promoted = backend.promote(&guard.id).map_err(RunError::Promote)?;

            // Deactivate guard before explicit destroy to prevent a double-destroy
            // attempt if the explicit destroy call returns an error.
            guard.deactivated = true;
            backend.destroy(&guard.id).map_err(RunError::Destroy)?;

            Ok(Outcome::Kept { value, promoted })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        run_in_workspace, Disposition, Outcome, PromotedRef, RunError, WorkspaceBackend,
        WorkspaceId,
    };
    use std::collections::HashMap;
    use std::fmt;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    // -------------------------------------------------------------------------
    // In-memory backend
    // -------------------------------------------------------------------------

    /// Which backend method should fail on the next call.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum FailStage {
        Fork,
        Mount,
        Promote,
        Destroy,
    }

    /// Minimal error type for the in-memory backend.
    #[derive(Debug, Clone)]
    struct InMemError(String);

    impl fmt::Display for InMemError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "in-memory backend error: {}", self.0)
        }
    }

    impl std::error::Error for InMemError {}

    /// Trivial mount token for the in-memory backend.
    struct InMemMount;

    /// Deterministic, filesystem-free backend for state-machine tests.
    ///
    /// Uses a monotonic `AtomicUsize` counter for ids (no randomness, no clock).
    /// `set_fail_on` injects a single failure into the chosen stage; the failure
    /// is consumed on first use so subsequent calls succeed.
    struct InMemBackend {
        /// Live overlay records. Keyed by the string id from `WorkspaceId`.
        overlays: Mutex<HashMap<String, ()>>,
        /// Durable promoted records.
        promoted: Mutex<HashMap<String, ()>>,
        counter: AtomicUsize,
        fail_on: Mutex<Option<FailStage>>,
    }

    impl InMemBackend {
        fn new() -> Self {
            Self {
                overlays: Mutex::new(HashMap::new()),
                promoted: Mutex::new(HashMap::new()),
                counter: AtomicUsize::new(1),
                fail_on: Mutex::new(None),
            }
        }

        fn set_fail_on(&self, stage: FailStage) {
            *self.fail_on.lock().expect("fail_on lock poisoned") = Some(stage);
        }

        /// Count of currently live overlays (must be 0 after every terminal state).
        fn live_overlays(&self) -> usize {
            self.overlays.lock().expect("overlays lock poisoned").len()
        }

        /// Count of durably promoted refs.
        fn promoted_count(&self) -> usize {
            self.promoted.lock().expect("promoted lock poisoned").len()
        }

        /// Consume and return true if `stage` matches the pending failure injection.
        fn should_fail(&self, stage: FailStage) -> bool {
            let mut guard = self.fail_on.lock().expect("fail_on lock poisoned");
            if *guard == Some(stage) {
                *guard = None;
                true
            } else {
                false
            }
        }
    }

    impl WorkspaceBackend for InMemBackend {
        type Mount = InMemMount;
        type Error = InMemError;

        fn fork(&self, _base: &Path) -> Result<WorkspaceId, Self::Error> {
            if self.should_fail(FailStage::Fork) {
                return Err(InMemError("injected fork failure".to_string()));
            }
            let n = self.counter.fetch_add(1, Ordering::SeqCst);
            let id = WorkspaceId(format!("ws-{}", n));
            let mut overlays = self.overlays.lock().expect("overlays lock poisoned");
            overlays.insert(id.0.clone(), ());
            assert!(overlays.len() <= 10_000, "overlay count exceeds sanity cap");
            Ok(id)
        }

        fn mount(&self, id: &WorkspaceId) -> Result<Self::Mount, Self::Error> {
            if self.should_fail(FailStage::Mount) {
                return Err(InMemError("injected mount failure".to_string()));
            }
            assert!(!id.0.is_empty(), "WorkspaceId must not be empty in mount");
            let overlays = self.overlays.lock().expect("overlays lock poisoned");
            if !overlays.contains_key(&id.0) {
                return Err(InMemError(format!("overlay {} not found", id.0)));
            }
            Ok(InMemMount)
        }

        fn promote(&self, id: &WorkspaceId) -> Result<PromotedRef, Self::Error> {
            if self.should_fail(FailStage::Promote) {
                return Err(InMemError("injected promote failure".to_string()));
            }
            assert!(!id.0.is_empty(), "WorkspaceId must not be empty in promote");
            let overlays = self.overlays.lock().expect("overlays lock poisoned");
            if !overlays.contains_key(&id.0) {
                return Err(InMemError(format!("overlay {} not found in promote", id.0)));
            }
            drop(overlays);
            let mut promoted = self.promoted.lock().expect("promoted lock poisoned");
            promoted.insert(id.0.clone(), ());
            assert!(
                promoted.len() <= 10_000,
                "promoted count exceeds sanity cap"
            );
            Ok(PromotedRef(format!("ref:{}", id.0)))
        }

        fn destroy(&self, id: &WorkspaceId) -> Result<(), Self::Error> {
            if self.should_fail(FailStage::Destroy) {
                return Err(InMemError("injected destroy failure".to_string()));
            }
            assert!(!id.0.is_empty(), "WorkspaceId must not be empty in destroy");
            self.overlays
                .lock()
                .expect("overlays lock poisoned")
                .remove(&id.0);
            Ok(())
        }
    }

    // -------------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------------

    fn noop_base() -> &'static Path {
        Path::new("/noop-base")
    }

    // -------------------------------------------------------------------------
    // Tests
    // -------------------------------------------------------------------------

    // (a) Keep path: overlay gone, promoted_count == 1, value passed through.
    #[test]
    fn keep_path_promotes_and_cleans_overlay() {
        let backend = InMemBackend::new();
        let result = run_in_workspace(&backend, noop_base(), |_| Disposition::Keep(42u32));
        match result.expect("keep path must succeed") {
            Outcome::Kept { value, promoted } => {
                assert_eq!(value, 42u32, "value must be passed through");
                assert!(
                    promoted.0.starts_with("ref:"),
                    "promoted ref must carry the ref prefix"
                );
            }
            Outcome::Discarded => panic!("expected Kept, got Discarded"),
        }
        assert_eq!(backend.live_overlays(), 0, "no orphan overlays after Keep");
        assert_eq!(
            backend.promoted_count(),
            1,
            "exactly one promoted ref after Keep"
        );
    }

    // (b) Discard path: overlay gone, nothing promoted.
    #[test]
    fn discard_path_destroys_overlay_only() {
        let backend = InMemBackend::new();
        let result = run_in_workspace::<_, _, ()>(&backend, noop_base(), |_| Disposition::Discard);
        assert!(
            matches!(result, Ok(Outcome::Discarded)),
            "discard must return Ok(Discarded)"
        );
        assert_eq!(
            backend.live_overlays(),
            0,
            "no orphan overlays after Discard"
        );
        assert_eq!(
            backend.promoted_count(),
            0,
            "no promoted refs after Discard"
        );
    }

    // (c) Closure panics: RunError::ClosurePanicked returned, overlay gone.
    #[test]
    fn panic_path_destroys_overlay_and_returns_error() {
        let backend = InMemBackend::new();
        let result = run_in_workspace::<_, _, ()>(&backend, noop_base(), |_| {
            panic!("deliberate test panic");
        });
        assert!(
            matches!(result, Err(RunError::ClosurePanicked)),
            "panicking closure must yield RunError::ClosurePanicked"
        );
        assert_eq!(
            backend.live_overlays(),
            0,
            "no orphan overlays after closure panic"
        );
        assert_eq!(
            backend.promoted_count(),
            0,
            "no promoted refs after closure panic"
        );
    }

    // (d1) Mount fails: RunError::Mount, forked overlay destroyed, no orphan.
    #[test]
    fn mount_failure_destroys_forked_overlay() {
        let backend = InMemBackend::new();
        backend.set_fail_on(FailStage::Mount);
        let result = run_in_workspace::<_, _, ()>(&backend, noop_base(), |_| Disposition::Discard);
        assert!(
            matches!(result, Err(RunError::Mount(_))),
            "mount failure must yield RunError::Mount"
        );
        assert_eq!(
            backend.live_overlays(),
            0,
            "no orphan overlays after mount failure"
        );
        assert_eq!(
            backend.promoted_count(),
            0,
            "no promoted refs after mount failure"
        );
    }

    // (d2) Promote fails: RunError::Promote, overlay destroyed, no orphan.
    #[test]
    fn promote_failure_destroys_overlay() {
        let backend = InMemBackend::new();
        backend.set_fail_on(FailStage::Promote);
        let result = run_in_workspace(&backend, noop_base(), |_| Disposition::Keep(99u32));
        assert!(
            matches!(result, Err(RunError::Promote(_))),
            "promote failure must yield RunError::Promote"
        );
        assert_eq!(
            backend.live_overlays(),
            0,
            "no orphan overlays after promote failure"
        );
        assert_eq!(
            backend.promoted_count(),
            0,
            "no promoted refs after promote failure"
        );
    }

    // (e) No-orphan invariant: live_overlays == 0 after every sequential run.
    #[test]
    fn no_orphan_invariant_across_multiple_runs() {
        let backend = InMemBackend::new();
        for i in 0..5u32 {
            let r = run_in_workspace(&backend, noop_base(), |_| Disposition::Keep(i));
            assert!(r.is_ok(), "run {} must succeed", i);
            assert_eq!(backend.live_overlays(), 0, "no orphans after run {}", i);
        }
        assert_eq!(
            backend.promoted_count(),
            5,
            "five promoted refs after five keep runs"
        );
    }

    // Deterministic ids: counter starts at 1 and increments monotonically.
    #[test]
    fn fork_generates_deterministic_ids() {
        let backend = InMemBackend::new();
        let id1 = backend.fork(noop_base()).expect("first fork");
        let id2 = backend.fork(noop_base()).expect("second fork");
        // Clean up so live_overlays does not leak across other tests.
        backend.destroy(&id1).expect("destroy id1");
        backend.destroy(&id2).expect("destroy id2");
        assert_eq!(
            id1,
            WorkspaceId("ws-1".to_string()),
            "first id must be ws-1"
        );
        assert_eq!(
            id2,
            WorkspaceId("ws-2".to_string()),
            "second id must be ws-2"
        );
        assert_eq!(
            backend.live_overlays(),
            0,
            "no orphans after direct fork+destroy test"
        );
    }

    // Fork failure: RunError::Fork returned, no overlay created.
    #[test]
    fn fork_failure_returns_error_and_no_orphan() {
        let backend = InMemBackend::new();
        backend.set_fail_on(FailStage::Fork);
        let result = run_in_workspace::<_, _, ()>(&backend, noop_base(), |_| Disposition::Discard);
        assert!(
            matches!(result, Err(RunError::Fork(_))),
            "fork failure must yield RunError::Fork"
        );
        assert_eq!(
            backend.live_overlays(),
            0,
            "no orphan overlay after fork failure"
        );
    }
}
