use anyhow::Result;
use std::path::Path;

/// Which mount transport the current compile target uses.
///
/// Compiled on ALL targets so the selection is testable without mounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Linux,
    MacOs,
    Windows,
}

/// Return the mount backend this binary was compiled for.
///
/// Always compiled regardless of `--features`; it reflects only the target OS.
#[cfg(target_os = "linux")]
pub fn selected_backend() -> Backend {
    Backend::Linux
}

#[cfg(target_os = "macos")]
pub fn selected_backend() -> Backend {
    Backend::MacOs
}

#[cfg(target_os = "windows")]
pub fn selected_backend() -> Backend {
    Backend::Windows
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn selected_backend() -> Backend {
    Backend::Linux
}

/// Mount the CAS view of `repo` at `mountpoint`, dispatching by target OS.
///
/// On Linux (with `--features fuser`): drives the fuser-based LunarFs.
/// On macOS (with `--features fuse`): drives the FUSE-T / libfuse backend.
/// On Windows (with `--features projfs`): drives the ProjFS backend.
pub fn mount(repo: &Path, mountpoint: &Path) -> Result<()> {
    if !repo.is_dir() {
        anyhow::bail!("repo must be an existing directory: {}", repo.display());
    }
    if !mountpoint.is_dir() {
        anyhow::bail!(
            "mountpoint must be an existing directory: {}",
            mountpoint.display()
        );
    }
    dispatch_mount(repo, mountpoint)
}

// mount-nfs takes priority on unix; it cannot build on Windows (nfsserve is unix-only).
#[cfg(all(feature = "mount-nfs", unix))]
fn dispatch_mount(repo: &Path, mountpoint: &Path) -> Result<()> {
    crate::nfs::mount_nfs(repo, mountpoint)
}

// mount-nfs + Windows without projfs: degrade cleanly instead of a compile error.
#[cfg(all(feature = "mount-nfs", not(unix), not(feature = "projfs")))]
fn dispatch_mount(_repo: &Path, _mountpoint: &Path) -> Result<()> {
    anyhow::bail!("mount is not supported on this platform without --features projfs")
}

#[cfg(all(not(feature = "mount-nfs"), target_os = "linux", feature = "fuser"))]
fn dispatch_mount(repo: &Path, mountpoint: &Path) -> Result<()> {
    crate::backend::linux::mount_linux(repo, mountpoint)
}

#[cfg(all(not(feature = "mount-nfs"), target_os = "macos", feature = "fuse"))]
fn dispatch_mount(repo: &Path, mountpoint: &Path) -> Result<()> {
    crate::backend::macos::mount_macos(repo, mountpoint)
}

// ProjFS covers Windows regardless of mount-nfs (nfs is a no-op on Windows anyway).
#[cfg(all(target_os = "windows", feature = "projfs"))]
fn dispatch_mount(repo: &Path, mountpoint: &Path) -> Result<()> {
    crate::backend::windows::mount_windows(repo, mountpoint)
}

#[cfg(not(any(
    all(feature = "mount-nfs", unix),
    all(feature = "mount-nfs", not(unix), not(feature = "projfs")),
    all(not(feature = "mount-nfs"), target_os = "linux", feature = "fuser"),
    all(not(feature = "mount-nfs"), target_os = "macos", feature = "fuse"),
    all(target_os = "windows", feature = "projfs"),
)))]
fn dispatch_mount(_repo: &Path, _mountpoint: &Path) -> Result<()> {
    anyhow::bail!(
        "mount() requires --features fuse (macOS/FUSE-T), --features fuser (Linux), \
         --features projfs (Windows), or --features mount-nfs"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_backend_matches_target_os() {
        let b = selected_backend();
        #[cfg(target_os = "linux")]
        assert_eq!(b, Backend::Linux, "Linux build must return Backend::Linux");
        #[cfg(target_os = "macos")]
        assert_eq!(b, Backend::MacOs, "macOS build must return Backend::MacOs");
        #[cfg(target_os = "windows")]
        assert_eq!(
            b,
            Backend::Windows,
            "Windows build must return Backend::Windows"
        );
    }

    #[test]
    fn backend_enum_is_eq_and_clone() {
        let a = Backend::MacOs;
        let b = a;
        assert_eq!(a, b, "Backend must implement Copy+Eq");
        assert_ne!(Backend::Linux, Backend::MacOs, "variants must be distinct");
        assert_ne!(
            Backend::MacOs,
            Backend::Windows,
            "variants must be distinct"
        );
        assert_ne!(
            Backend::Linux,
            Backend::Windows,
            "variants must be distinct"
        );
    }
}
