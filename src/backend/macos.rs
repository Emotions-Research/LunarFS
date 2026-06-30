#![cfg(all(target_os = "macos", feature = "fuse"))]

use anyhow::Result;
use std::path::Path;
use std::sync::Arc;

use crate::acl::AclEntry;
use crate::core::Core;
use crate::fuse::ops::{init_state, make_read_ops, FsData};
use crate::fuse_t::FuseOperations;
use crate::overlay::{AgentId, OverlayStore, WorkspaceId};

/// Prepared FUSE-T session: callbacks wired, mountpoint stored, loop NOT entered.
///
/// Use `run_with` to inject the event loop (or a no-op in tests).
/// Use `with_overlay` / `with_acl` to configure overlay and ACL before running.
pub struct MacFuseSession {
    mountpoint: std::path::PathBuf,
    ops: FuseOperations,
    fs_data: Arc<FsData>,
}

impl std::fmt::Debug for MacFuseSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacFuseSession")
            .field("mountpoint", &self.mountpoint)
            .finish_non_exhaustive()
    }
}

impl MacFuseSession {
    /// Wire the read callbacks and store the session config.
    ///
    /// Does NOT call fuse_mount, fuse_new, or fuse_loop; the loop is reached
    /// only through the `run_with` seam so tests never perform a real mount.
    ///
    /// Default: base-only (no overlay), empty ACL, principal from process uid.
    pub fn build(core: Core, mountpoint: &Path) -> Result<Self> {
        // Invariant 1: mountpoint must be an existing directory.
        if !mountpoint.is_dir() {
            anyhow::bail!(
                "mountpoint must be an existing directory: {}",
                mountpoint.display()
            );
        }
        // SAFETY: getuid() is a simple POSIX syscall with no preconditions.
        let principal = unsafe { libc::getuid() }.to_string();
        let fs_data = Arc::new(FsData {
            index: core.index,
            store: core.store,
            overlay: None,
            agent: 0,
            workspace: 0,
            acl: Vec::new(),
            principal,
        });
        let ops = make_read_ops();
        // Invariant 2: getattr callback must be wired (make_read_ops always sets it).
        assert!(
            ops.getattr.is_some(),
            "getattr callback must be wired in ops"
        );
        Ok(Self {
            mountpoint: mountpoint.to_path_buf(),
            ops,
            fs_data,
        })
    }

    /// Attach an overlay store and agent context.
    ///
    /// Consumes and returns Self so callers can chain: `session.with_overlay(...)`.
    /// Must be called before `run_with`; panics if the Arc has been shared already.
    pub fn with_overlay(
        self,
        overlay: Arc<OverlayStore>,
        agent: AgentId,
        workspace: WorkspaceId,
    ) -> Self {
        let ops = self.ops;
        let mountpoint = self.mountpoint;
        let mut data = Arc::try_unwrap(self.fs_data).unwrap_or_else(|_| {
            panic!("MacFuseSession must be the sole Arc owner when calling with_overlay")
        });
        data.overlay = Some(overlay);
        data.agent = agent;
        data.workspace = workspace;
        Self {
            mountpoint,
            ops,
            fs_data: Arc::new(data),
        }
    }

    /// Set the ACL entries used for per-path permission checks.
    ///
    /// Consumes and returns Self; chain after `build` and before `run_with`.
    /// Panics if the Arc has been shared already.
    pub fn with_acl(self, entries: Vec<AclEntry>) -> Self {
        let ops = self.ops;
        let mountpoint = self.mountpoint;
        let mut data = Arc::try_unwrap(self.fs_data).unwrap_or_else(|_| {
            panic!("MacFuseSession must be the sole Arc owner when calling with_acl")
        });
        data.acl = entries;
        Self {
            mountpoint,
            ops,
            fs_data: Arc::new(data),
        }
    }

    /// Drive the session with a caller-supplied event loop.
    ///
    /// Production: pass a closure that calls `fuse_main_real` (or fuse_loop).
    /// Tests: pass `|_| Ok(())` to verify construction without mounting.
    pub fn run_with<F: FnOnce(&mut Self) -> Result<()>>(mut self, enter_loop: F) -> Result<()> {
        // Invariant 1: mountpoint must not be empty.
        assert!(
            !self.mountpoint.as_os_str().is_empty(),
            "mountpoint path must not be empty at run time"
        );
        // Invariant 2: getattr must still be wired (only build() constructs Self).
        assert!(
            self.ops.getattr.is_some(),
            "ops must have getattr wired before running"
        );
        enter_loop(&mut self)
    }
}

/// Mount `repo` at `mountpoint` via the FUSE-T high-level API.
pub fn mount_macos(repo: &Path, mountpoint: &Path) -> Result<()> {
    assert!(
        repo.is_dir(),
        "repo must be a directory before calling mount_macos"
    );
    assert!(
        mountpoint.is_dir(),
        "mountpoint must be a directory before calling mount_macos"
    );

    let core = Core::new(repo)?;
    let session = MacFuseSession::build(core, mountpoint)?;

    session.run_with(|s| {
        // Initialize the global index+store that the callbacks read.
        if !init_state(Arc::clone(&s.fs_data)) {
            anyhow::bail!("FUSE filesystem state already initialized in this process");
        }

        // Build argv: ["lunar", "<mountpoint>"].
        let prog = std::ffi::CString::new("lunar")?;
        let mnt_str = s
            .mountpoint
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("mountpoint path is not valid UTF-8"))?;
        let mnt = std::ffi::CString::new(mnt_str)?;

        // SAFETY: argv elements point into prog/mnt CStrings that live until this
        // closure returns. fuse_main_real only reads argv (never writes to it).
        let mut argv: [*mut libc::c_char; 2] = [
            prog.as_ptr() as *mut libc::c_char,
            mnt.as_ptr() as *mut libc::c_char,
        ];

        let ret = unsafe {
            crate::fuse_t::fuse_main_real(
                2,
                argv.as_mut_ptr(),
                &s.ops,
                std::mem::size_of::<FuseOperations>(),
                std::ptr::null_mut(),
            )
        };
        if ret != 0 {
            anyhow::bail!("fuse_main_real returned non-zero exit status: {}", ret);
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::MemStore;
    use crate::index::Index;
    use crate::ingest::walk_repo;

    fn make_test_core(repo: &Path) -> Core {
        let store = MemStore::new();
        let hash = walk_repo(&store, repo).expect("walk_repo must succeed on fixture repo");
        let index = Index::build(&store, &hash).expect("Index::build must succeed");
        Core {
            store: Box::new(store),
            index,
        }
    }

    #[test]
    fn macos_session_builds_and_no_op_loop_succeeds() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("create fixture repo dir");
        std::fs::write(repo.join("hello.txt"), b"hello world").expect("write fixture file");
        let mnt = tmp.path().join("mnt");
        std::fs::create_dir_all(&mnt).expect("create mountpoint dir");

        let core = make_test_core(&repo);
        // Verify the fixture file landed in the index before moving core.
        assert!(
            core.index.lookup("hello.txt").is_some(),
            "fixture file must be present in the index"
        );

        let session =
            MacFuseSession::build(core, &mnt).expect("MacFuseSession::build must succeed");

        // No-op closure: fuse_loop is never entered, no real mount occurs.
        session
            .run_with(|_s| Ok(()))
            .expect("no-op run_with must succeed");
    }

    #[test]
    fn macos_session_build_rejects_nonexistent_mountpoint() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("create fixture repo dir");
        std::fs::write(repo.join("f.txt"), b"data").expect("write fixture");
        let core = make_test_core(&repo);

        let bad_mnt = tmp.path().join("does_not_exist");
        let err = MacFuseSession::build(core, &bad_mnt)
            .expect_err("build must reject a nonexistent mountpoint");
        let msg = err.to_string();
        assert!(
            msg.contains("mountpoint must be an existing directory"),
            "error message must name the constraint; got: {msg}"
        );
    }
}
