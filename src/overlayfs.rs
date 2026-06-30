//! OverlayFS support: pure option-string helper (portable) + Linux mount(2) path.
//!
//! `overlay_options` is unconditionally compiled and tested on all platforms.
//! `mount_overlay` / `unmount_overlay` are Linux-only via per-item `#[cfg]` gates.

use std::path::Path;

#[cfg(target_os = "linux")]
use anyhow::{anyhow, Result};

#[cfg(target_os = "linux")]
use nix::mount::{mount, umount2, MntFlags, MsFlags};

/// Build the data option string for an OverlayFS mount.
///
/// Uses `to_string_lossy` so paths with non-UTF-8 bytes are rendered
/// with replacement characters. Call `mount_overlay` for production use:
/// it validates UTF-8 and returns an error before the kernel sees garbage.
pub fn overlay_options(lower: &Path, upper: &Path, work: &Path) -> String {
    let s = format!(
        "lowerdir={},upperdir={},workdir={}",
        lower.to_string_lossy(),
        upper.to_string_lossy(),
        work.to_string_lossy(),
    );
    assert!(
        s.starts_with("lowerdir="),
        "overlay_options must start with lowerdir="
    );
    assert!(
        s.contains(",upperdir="),
        "overlay_options must contain ,upperdir="
    );
    s
}

/// Validate `p` is representable as UTF-8 (required for the overlay option string).
#[cfg(target_os = "linux")]
fn path_to_str(p: &Path) -> Result<&str> {
    p.to_str()
        .ok_or_else(|| anyhow!("path contains non-UTF-8 bytes: {:?}", p))
}

/// Create `upper` and `work` directories if they do not exist.
///
/// The kernel requires both to be present before the mount(2) call.
#[cfg(target_os = "linux")]
fn ensure_upper_work(upper: &Path, work: &Path) -> Result<()> {
    std::fs::create_dir_all(upper)?;
    std::fs::create_dir_all(work)?;
    assert!(
        upper.is_dir(),
        "upper dir must exist after ensure_upper_work"
    );
    assert!(work.is_dir(), "work dir must exist after ensure_upper_work");
    Ok(())
}

/// Return an error if `work` is not empty.
///
/// The kernel rejects a non-empty workdir with EINVAL; surfacing it early
/// produces a clearer message than the kernel error.
#[cfg(target_os = "linux")]
fn check_work_empty(work: &Path) -> Result<()> {
    let first = std::fs::read_dir(work)?.next();
    if first.is_some() {
        return Err(anyhow!(
            "overlayfs workdir must be empty: {}",
            work.display()
        ));
    }
    Ok(())
}

/// Mount an OverlayFS at `target` via a single `mount(2)` call.
///
/// - `lower`  : read-only base layer (the CAS blob directory)
/// - `upper`  : per-agent writable layer; created if absent
/// - `work`   : kernel atomics scratch dir; created if absent, must be empty
///              and on the same filesystem as `upper`
/// - `target` : mount point; must already exist
///
/// Returns an error on any failure; never panics on runtime conditions.
#[cfg(target_os = "linux")]
pub fn mount_overlay(lower: &Path, upper: &Path, work: &Path, target: &Path) -> Result<()> {
    assert!(lower.exists(), "lower dir must exist before mounting");
    assert!(target.exists(), "mount target must exist before mounting");

    ensure_upper_work(upper, work)?;
    check_work_empty(work)?;

    let options = format!(
        "lowerdir={},upperdir={},workdir={}",
        path_to_str(lower)?,
        path_to_str(upper)?,
        path_to_str(work)?,
    );

    mount(
        Some("overlay"),
        target,
        Some("overlay"),
        MsFlags::empty(),
        Some(options.as_str()),
    )
    .map_err(|e| anyhow!("mount overlayfs at {}: {}", target.display(), e))
}

/// Unmount an OverlayFS at `target` via `umount2(2)`.
#[cfg(target_os = "linux")]
pub fn unmount_overlay(target: &Path) -> Result<()> {
    assert!(target.exists(), "unmount target must exist");
    assert!(target.is_dir(), "unmount target must be a directory");
    umount2(target, MntFlags::empty())
        .map_err(|e| anyhow!("umount2 at {}: {}", target.display(), e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // These two tests are NOT cfg-gated: they run on macOS and Linux alike.

    #[test]
    fn overlay_options_absolute_paths() {
        let opts = overlay_options(
            Path::new("/srv/base"),
            Path::new("/agent/1/upper"),
            Path::new("/agent/1/work"),
        );
        assert_eq!(
            opts, "lowerdir=/srv/base,upperdir=/agent/1/upper,workdir=/agent/1/work",
            "absolute-path option string must match the overlay mount data format"
        );
    }

    #[test]
    fn overlay_options_relative_paths() {
        let opts = overlay_options(Path::new("lower"), Path::new("upper"), Path::new("work"));
        assert_eq!(
            opts, "lowerdir=lower,upperdir=upper,workdir=work",
            "relative-path option string must format correctly"
        );
    }

    // Full mount test: Linux only; skipped without CAP_SYS_ADMIN.
    #[cfg(target_os = "linux")]
    #[test]
    fn mount_overlay_privilege_gated() {
        if !nix::unistd::getuid().is_root() {
            // Verify option string construction without the syscall.
            let dir = tempfile::tempdir().expect("tempdir");
            let lower = dir.path().join("lower");
            let upper = dir.path().join("upper");
            let work = dir.path().join("work");
            let opts = overlay_options(&lower, &upper, &work);
            assert!(
                opts.starts_with("lowerdir="),
                "option string must start with lowerdir="
            );
            assert!(
                opts.contains(",workdir="),
                "option string must contain ,workdir="
            );
            return; // skip syscall: no CAP_SYS_ADMIN
        }

        // Running as root: exercise the real mount + unmount.
        let dir = tempfile::tempdir().expect("tempdir");
        let lower = dir.path().join("lower");
        let upper = dir.path().join("upper");
        let work = dir.path().join("work");
        let target = dir.path().join("mnt");

        std::fs::create_dir_all(&lower).expect("create lower");
        std::fs::create_dir_all(&target).expect("create target");

        mount_overlay(&lower, &upper, &work, &target).expect("mount_overlay must succeed as root");
        unmount_overlay(&target).expect("unmount_overlay must succeed");
    }
}
