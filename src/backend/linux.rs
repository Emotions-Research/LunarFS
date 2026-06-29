#![cfg(all(target_os = "linux", feature = "fuser"))]

use anyhow::Result;
use std::path::Path;

/// Mount the CAS view of `repo` at `mountpoint` using the fuser-based backend.
///
/// Builds a `Core` from the repo (ingesting into the default CAS), wraps it in
/// a `LunarFs`, then hands control to fuser's blocking mount2 call.
pub fn mount_linux(repo: &Path, mountpoint: &Path) -> Result<()> {
    assert!(repo.is_dir(), "repo must be a directory before calling mount_linux");
    assert!(mountpoint.is_dir(), "mountpoint must be a directory before calling mount_linux");

    let core = crate::core::Core::new(repo)?;
    let fs = crate::fs::LunarFs::new(core.store, core.index);
    crate::fs::mount(fs, mountpoint)?;
    Ok(())
}
