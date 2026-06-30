# Agents and the `run_in_workspace` API

This guide covers the high-level `run_in_workspace` entry point, its explicit state
machine, and how a NYX-style worker-spawner migrates from `git worktree add` to
lunar fork-and-mount. The adapter example in Section 4 is illustrative: it shows
the pattern in code but does not modify any live orchestrator repository.

---

## 1. Overview

`run_in_workspace` encodes the NYX worker-spawner lifecycle as an explicit state
machine with five states:

```
Forked -> Mounted -> Running -> (Promoted | Discarded) -> Destroyed
```

The central guarantee: **the ephemeral overlay is always destroyed**, regardless of
what the caller closure does. If the closure returns `Disposition::Discard`, if it
returns an error, or if it panics, the overlay is removed and no orphan workspace
record is left behind. The only thing that survives a successful `Keep` path is the
explicitly promoted artifact (a `PromotedRef`) that was extracted from the overlay
before the overlay was torn down.

State transitions:

| State | Action | Next state |
|-------|--------|------------|
| (entry) | `backend.fork(base)` | Forked |
| Forked | `backend.mount(id)` | Mounted |
| Mounted | caller closure runs | Running |
| Running | closure returns `Keep(value)` | Promoted, then Destroyed |
| Running | closure returns `Discard` | Discarded, then Destroyed |
| Running | closure panics or errors | error path, then Destroyed |
| any failure | `backend.destroy(id)` | Destroyed |

On the `Keep` path, `promote()` is called first so the chosen artifact exists
outside the overlay, and then `destroy()` removes the overlay. On every other path,
`destroy()` is called directly and nothing is promoted. An RAII guard (or
`catch_unwind`) ensures `destroy` fires even during stack unwinding.

---

## 2. API Surface

These are the exact public shapes. Do not invent signatures that contradict them.

```rust
/// Caller closure's verdict, carrying the artifact to keep on the Keep path.
pub enum Disposition<T> {
    Keep(T),
    Discard,
}

/// Stable handle to one ephemeral overlay.
pub struct WorkspaceId(String);

/// Durable reference handed back after a successful promote (overlay-independent).
pub struct PromotedRef(String);

/// Backend abstraction.
/// Production impl uses APFS clonefile / FUSE; tests use an in-memory impl.
pub trait WorkspaceBackend {
    type Mount;
    type Error: std::error::Error + Send + Sync + 'static;

    fn fork(&self, base: &std::path::Path) -> Result<WorkspaceId, Self::Error>;
    fn mount(&self, id: &WorkspaceId) -> Result<Self::Mount, Self::Error>;
    fn promote(&self, id: &WorkspaceId) -> Result<PromotedRef, Self::Error>;
    fn destroy(&self, id: &WorkspaceId) -> Result<(), Self::Error>;
}

/// What run_in_workspace returns on the success paths.
pub enum Outcome<T> {
    Kept { value: T, promoted: PromotedRef },
    Discarded,
}

/// Library error covering each state transition,
/// plus a panic-in-closure variant when catch_unwind is used.
pub enum RunError<E> {
    Fork(E),
    Mount(E),
    Promote(E),
    Destroy(E),
    ClosurePanicked,
}

/// The high-level entry point.
/// The closure receives the mounted overlay and returns a Disposition.
pub fn run_in_workspace<B, F, T>(
    backend: &B,
    base: &std::path::Path,
    f: F,
) -> Result<Outcome<T>, RunError<B::Error>>
where
    B: WorkspaceBackend,
    F: FnOnce(&B::Mount) -> Disposition<T>;
```

### Relationship to the existing `OverlayBackend`

The crate already exposes `OverlayBackend` in `workspace.rs` with `fork / write /
read / destroy / exists`. `WorkspaceBackend` is a distinct, higher-level trait: it
adds `mount` (which returns a handle the closure works against) and `promote` (the
"bless and extract" step). An adapter can implement `WorkspaceBackend` by wrapping
`OverlayBackend` and delegating the CoW fork and destroy calls.

---

## 3. Before: `git worktree add`

A typical NYX worker-spawner today creates an isolated branch checkout with git
worktrees, runs work inside it, and must remember to clean up on every exit path,
including failures.

```rust
use std::path::{Path, PathBuf};
use std::process::Command;

fn run_worker(base_branch: &str, work_root: &Path) -> anyhow::Result<String> {
    // Pick a unique path for this worker's worktree.
    let worktree_path: PathBuf = work_root.join(format!("worker-{}", rand_suffix()));

    // Create the isolated checkout. Pays a full filesystem copy.
    Command::new("git")
        .args(["worktree", "add", "--detach", worktree_path.to_str().unwrap()])
        .status()?;

    // node_modules must be reinstalled or symlinked manually.
    // This can take 30-120 s on a cold cache.
    Command::new("npm")
        .arg("install")
        .current_dir(&worktree_path)
        .status()?;

    let result = do_work(&worktree_path);

    // Manual cleanup on every exit path. A panic or early return leaks the worktree.
    Command::new("git")
        .args(["worktree", "remove", "--force", worktree_path.to_str().unwrap()])
        .status()?;

    // Also have to run `git worktree prune` after failures to remove stale refs.
    result
}
```

Pain points:

- `git worktree add` copies tracked files, paying wall-clock time proportional to
  the tree size.
- `node_modules` is not part of the worktree and requires a reinstall or a fragile
  symlink that must point to the right location.
- Manual cleanup on every return path (success, error, early return). A panic skips
  the cleanup entirely and leaves a half-built worktree with no automatic recovery.
- Orphan worktrees accumulate silently; the only recovery is `git worktree prune`,
  which is a separate step that must be called explicitly.

---

## 4. After: lunar `run_in_workspace`

The same spawner rewritten to use `run_in_workspace`. Teardown is automatic and
panic-safe. No cleanup code appears on any return path.

```rust
use std::path::Path;
use devdropbox::run_in_workspace::{
    run_in_workspace, Disposition, Outcome, WorkspaceBackend,
};

fn run_worker<B>(backend: &B, base_dir: &Path) -> Result<Option<String>, Box<dyn std::error::Error>>
where
    B: WorkspaceBackend,
    B::Mount: AsRef<Path>,
{
    let outcome = run_in_workspace(backend, base_dir, |mount| {
        let worktree_path: &Path = mount.as_ref();

        // node_modules is already present in the CoW overlay: no reinstall.
        // Do the actual work.
        match do_work(worktree_path) {
            Ok(artifact) => Disposition::Keep(artifact),
            Err(_) => Disposition::Discard,
        }
    });

    match outcome {
        Ok(Outcome::Kept { value, .. }) => Ok(Some(value)),
        Ok(Outcome::Discarded) => Ok(None),
        Err(e) => Err(Box::from(format!("workspace error: {:?}", e))),
    }
}
```

What changed:

- No `git worktree add` call. `run_in_workspace` forks via APFS `clonefile` and
  mounts the overlay; both steps are O(1) on macOS.
- No `npm install`. `node_modules` is present in the overlay from the moment of
  fork; it inherits the base snapshot via copy-on-write without any data being
  copied upfront.
- No manual cleanup. The RAII guard (or `catch_unwind` inside `run_in_workspace`)
  guarantees `backend.destroy` fires on every terminal path, including panics.
- No orphan records. If the worker binary is killed mid-run, the workspace record is
  held by the guard; a subsequent process startup can sweep any records without a
  mounted overlay.

---

## 5. Why This Wins

### APFS copy-on-write fork

On macOS, APFS `clonefile(2)` creates a reflink: the new tree shares all blocks
with the base until a block is written. A fork of a 2 GB project tree completes in
milliseconds and consumes zero additional disk until pages are dirtied. Compare
this to `git worktree add`, which copies every tracked file, or a plain `cp -r`,
which copies all bytes unconditionally.

### `node_modules` for free

Because the APFS fork shares blocks with the base, `node_modules` (which lives
inside the repository tree on most Node.js projects) is present in the overlay at
mount time. The worker starts immediately. No `npm install`, no symlink to a shared
cache, no risk of a version mismatch from a concurrent install in a sibling worker.

Under the `--features fuse` build, the FUSE mount serves the overlay as a merged
view of the base CAS layer and the worker's private writes. Reads against unchanged
paths go to the base layer at zero disk cost; writes are recorded in the overlay
only, leaving the base intact for every other worker.

### Teardown is a single destroy, not a sequence

`git worktree remove` must: unregister the worktree ref, delete the directory tree,
and optionally call `git worktree prune` to clean stale admin files. Each step can
fail independently.

`backend.destroy` on an APFS fork drops the reflink tree in one atomic unlink pass.
On the FUSE path it unmounts and removes the overlay directory. Neither requires
coordination with git's ref database.

### The RAII guard prevents leaks

`run_in_workspace` owns a `WorkspaceId` and a `promoted` flag internally. The
guard's `Drop` impl calls `backend.destroy` whenever the promoted flag is not set.
This means:

- Closure panics: Rust unwinds, the guard drops, the overlay is destroyed.
- Closure returns `Discard`: the guard destroys on drop after the closure returns.
- Backend error during `promote`: the guard destroys before propagating the error.
- Only a successful promote-then-explicit-drop sequence skips the destroy call.

A process that is `SIGKILL`-ed cannot run `Drop`, but the workspace record is
persisted with an `ephemeral` flag; a TTL sweeper or a startup reconciler can find
and destroy records that have no associated mounted overlay.

### The `Keep` path promotes exactly one artifact

`Outcome::Kept { value, promoted }` carries both the caller's return value and a
`PromotedRef` that was written outside the overlay before it was destroyed. The
caller can store this ref (a commit hash, a tarball key, a content-addressed blob
identifier) durably and the promoted data survives the overlay teardown. No other
state from the ephemeral workspace is retained.

---

## 6. Important Note

This file is an illustrative adapter pattern. It documents the `run_in_workspace`
API and shows how a worker-spawner can migrate from git worktrees to lunar
fork-and-mount. It does not modify the agent-orchestrator repository or any other
live codebase. The code snippets are examples intended for reading, not for
running as-is.

The production implementation of every type and function described above lives in
[`src/run_in_workspace.rs`](../src/run_in_workspace.rs). That file contains the
`WorkspaceBackend` trait definition, the `run_in_workspace` entry point, the RAII
`CleanupGuard`, and a complete in-memory test backend that demonstrates how to
implement the trait for deterministic unit tests. An agent author adapting their
worker-spawner should read that file alongside this guide: the guide explains the
why, and the source file is the authoritative, runnable contract.

---

## 7. See Also

- [`src/run_in_workspace.rs`](../src/run_in_workspace.rs): The worker-spawner
  implementation. Shows `WorkspaceBackend`, `run_in_workspace`, and an in-memory
  test double. Use this when you need to implement or test a custom backend.
- [`clients/mcp/README.md`](../clients/mcp/README.md): One-line install and
  config blocks for Claude Code, Cursor, and Cline using the `npx lunarfs-mcp`
  stdio MCP server. Use this to expose lunar workspaces as MCP tools to any
  agent that speaks the Model Context Protocol.
