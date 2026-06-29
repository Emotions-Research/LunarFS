//! OS NFS client invocation.
//!
//! `build_nfs_mount_argv` is a pure function (no syscalls) suitable for unit
//! tests. `run_os_nfs_mount` selects the current platform and spawns the command.

use anyhow::Result;
use std::path::Path;

/// Which operating system's NFS mount command to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientOs {
    MacOs,
    Linux,
}

/// Build the argv for invoking the OS NFS client.
///
/// Both macOS and Linux pass `port` AND `mountport` to the same ephemeral port
/// because nfsserve serves the MOUNT and NFS protocols on one TCP port.
///
/// Returns `(program, args)` where `program` is the executable name and `args`
/// are the positional arguments (not including the program).
///
/// Pure function: no syscalls, no process spawning.
pub fn build_nfs_mount_argv(
    os: ClientOs,
    host: &str,
    port: u16,
    mountpoint: &Path,
) -> (String, Vec<String>) {
    assert!(!host.is_empty(), "host must not be empty");
    assert!(port != 0, "port must not be zero");

    let mountpoint_str = mountpoint.display().to_string();
    let source = format!("{}:/", host);

    match os {
        ClientOs::MacOs => {
            let opts = format!(
                "vers=3,tcp,noatime,nolocks,port={},mountport={},soft",
                port, port
            );
            (
                "mount_nfs".to_owned(),
                vec!["-o".to_owned(), opts, source, mountpoint_str],
            )
        }
        ClientOs::Linux => {
            let opts = format!(
                "vers=3,tcp,noatime,nolock,port={},mountport={},soft",
                port, port
            );
            (
                "mount".to_owned(),
                vec![
                    "-t".to_owned(),
                    "nfs".to_owned(),
                    "-o".to_owned(),
                    opts,
                    source,
                    mountpoint_str,
                ],
            )
        }
    }
}

/// Invoke the OS NFS mount command against 127.0.0.1:port at mountpoint.
///
/// Returns an error if the command exits non-zero. On Linux, NFS mount typically
/// requires root; the error message surfaces this hint.
pub fn run_os_nfs_mount(port: u16, mountpoint: &Path) -> Result<()> {
    assert!(port != 0, "port must not be zero before invoking OS mount");
    run_os_nfs_mount_inner(port, mountpoint)
}

#[cfg(target_os = "macos")]
fn run_os_nfs_mount_inner(port: u16, mountpoint: &Path) -> Result<()> {
    run_mount_command(ClientOs::MacOs, port, mountpoint)
}

#[cfg(target_os = "linux")]
fn run_os_nfs_mount_inner(port: u16, mountpoint: &Path) -> Result<()> {
    run_mount_command(ClientOs::Linux, port, mountpoint)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn run_os_nfs_mount_inner(_port: u16, _mountpoint: &Path) -> Result<()> {
    anyhow::bail!("NFS client mount is not supported on this OS (supported: macOS, Linux)")
}

fn run_mount_command(os: ClientOs, port: u16, mountpoint: &Path) -> Result<()> {
    let (program, args) = build_nfs_mount_argv(os, "127.0.0.1", port, mountpoint);
    let status = std::process::Command::new(&program)
        .args(&args)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to launch {}: {}", program, e))?;
    if !status.success() {
        anyhow::bail!(
            "{} exited with {:?}; on Linux, NFS mount requires root (try sudo)",
            program,
            status.code()
        );
    }
    Ok(())
}

/// Build the argv for invoking the OS NFS unmount command.
///
/// Pure function: no syscalls, no process spawning.
///
/// macOS: `diskutil unmount [force] <mountpoint>`
/// Linux: `umount [-f] <mountpoint>`
pub fn build_nfs_unmount_argv(
    os: ClientOs,
    mountpoint: &Path,
    force: bool,
) -> (String, Vec<String>) {
    let mp = mountpoint.display().to_string();
    assert!(!mp.is_empty(), "mountpoint must not be empty");

    match os {
        ClientOs::MacOs => {
            let args = if force {
                vec!["unmount".to_owned(), "force".to_owned(), mp]
            } else {
                vec!["unmount".to_owned(), mp]
            };
            ("diskutil".to_owned(), args)
        }
        ClientOs::Linux => {
            let args = if force {
                vec!["-f".to_owned(), mp]
            } else {
                vec![mp]
            };
            ("umount".to_owned(), args)
        }
    }
}

/// Invoke the OS NFS unmount command at mountpoint.
///
/// `force` triggers a forced unmount (diskutil unmount force / umount -f).
/// Returns an error if the command exits non-zero.
/// Never panics; on an unsupported OS returns Err with a clear message.
pub fn run_os_nfs_unmount(mountpoint: &Path, force: bool) -> Result<()> {
    run_os_nfs_unmount_inner(mountpoint, force)
}

#[cfg(target_os = "macos")]
fn run_os_nfs_unmount_inner(mountpoint: &Path, force: bool) -> Result<()> {
    run_unmount_command(ClientOs::MacOs, mountpoint, force)
}

#[cfg(target_os = "linux")]
fn run_os_nfs_unmount_inner(mountpoint: &Path, force: bool) -> Result<()> {
    run_unmount_command(ClientOs::Linux, mountpoint, force)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn run_os_nfs_unmount_inner(_mountpoint: &Path, _force: bool) -> Result<()> {
    anyhow::bail!("NFS client unmount is not supported on this OS (supported: macOS, Linux)")
}

fn run_unmount_command(os: ClientOs, mountpoint: &Path, force: bool) -> Result<()> {
    let (program, args) = build_nfs_unmount_argv(os, mountpoint, force);
    let status = std::process::Command::new(&program)
        .args(&args)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to launch {}: {}", program, e))?;
    if !status.success() {
        anyhow::bail!(
            "{} exited with {:?}",
            program,
            status.code()
        );
    }
    Ok(())
}

/// Return true if `mountpoint` appears as a mount target in `output`.
///
/// Handles two formats:
///   macOS `mount` lines:  `127.0.0.1:/ on /some/path (nfs, ...)`
///   Linux `/proc/self/mountinfo` lines: space-separated, target is field 5 (0-indexed: 4)
///
/// A path that is merely a prefix of another longer path is NOT matched
/// (e.g. `/mnt/a` does not match a line whose target is `/mnt/ab`).
///
/// Pure function: no syscalls, no process spawning.
pub fn mountpoint_in_mount_output(output: &str, mountpoint: &Path) -> bool {
    let mp = mountpoint.display().to_string();
    assert!(!mp.is_empty(), "mountpoint must not be empty");

    for line in output.lines() {
        // macOS format: "... on <path> (..."
        // The path is bounded by ' on ' on the left and ' (' on the right.
        if let Some(after_on) = line.find(" on ") {
            let rest = &line[after_on + 4..];
            // rest starts at the target path; it ends at ' (' or end-of-line.
            let target = if let Some(paren) = rest.find(" (") {
                rest[..paren].trim()
            } else {
                rest.trim()
            };
            if target == mp {
                return true;
            }
            // Do not fall through to the mountinfo check for this line format.
            continue;
        }

        // Linux /proc/self/mountinfo: fields are space-separated.
        // Field indices (0-based): 0=mount-id 1=parent-id 2=major:minor 3=root 4=mount-target ...
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 5 && fields[4] == mp {
            return true;
        }
    }
    false
}

/// Return true if `mountpoint` is currently mounted on this OS.
///
/// Reads the live mount table (macOS: `mount` stdout; Linux: /proc/self/mountinfo,
/// falling back to `mount` stdout). On any probe error returns false without panicking.
pub fn mount_is_present(mountpoint: &Path) -> bool {
    let output = read_mount_table();
    match output {
        Ok(text) => mountpoint_in_mount_output(&text, mountpoint),
        Err(_) => false,
    }
}

#[cfg(target_os = "macos")]
fn read_mount_table() -> Result<String> {
    let out = std::process::Command::new("mount")
        .output()
        .map_err(|e| anyhow::anyhow!("failed to spawn mount: {}", e))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(target_os = "linux")]
fn read_mount_table() -> Result<String> {
    // Prefer the kernel-provided table; fall back to spawning `mount`.
    match std::fs::read_to_string("/proc/self/mountinfo") {
        Ok(text) => Ok(text),
        Err(_) => {
            let out = std::process::Command::new("mount")
                .output()
                .map_err(|e| anyhow::anyhow!("failed to spawn mount: {}", e))?;
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn read_mount_table() -> Result<String> {
    anyhow::bail!("mount table probe not supported on this OS (supported: macOS, Linux)")
}

// ---------------------------------------------------------------------------
// Tests (pure: no process spawning)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn macos_argv_uses_mount_nfs_with_required_opts() {
        let mountpoint = PathBuf::from("/mnt/test");
        let (program, args) = build_nfs_mount_argv(ClientOs::MacOs, "127.0.0.1", 2049, &mountpoint);
        assert_eq!(program, "mount_nfs");
        assert!(args.contains(&"-o".to_owned()), "must have -o flag");
        let opts = args.iter().find(|a| a.contains("vers=3")).expect("opts must contain vers=3");
        assert!(opts.contains("tcp"), "opts must contain tcp");
        assert!(opts.contains("noatime"), "opts must contain noatime");
        assert!(opts.contains("port=2049"), "opts must specify port");
        assert!(opts.contains("mountport=2049"), "opts must specify mountport");
        let source = args.last().expect("last arg must be mountpoint");
        assert_eq!(source, "/mnt/test");
    }

    #[test]
    fn macos_argv_source_is_root_export() {
        let mountpoint = PathBuf::from("/mnt/foo");
        let (_program, args) = build_nfs_mount_argv(ClientOs::MacOs, "127.0.0.1", 5000, &mountpoint);
        // Source "127.0.0.1:/" must appear before the mountpoint.
        let source_pos = args.iter().position(|a| a == "127.0.0.1:/").expect("source must be present");
        let mnt_pos = args.iter().position(|a| a == "/mnt/foo").expect("mountpoint must be present");
        assert!(source_pos < mnt_pos, "source must appear before mountpoint");
    }

    #[test]
    fn linux_argv_uses_mount_t_nfs() {
        let mountpoint = PathBuf::from("/mnt/test");
        let (program, args) = build_nfs_mount_argv(ClientOs::Linux, "127.0.0.1", 5555, &mountpoint);
        assert_eq!(program, "mount");
        let t_pos = args.iter().position(|a| a == "-t").expect("-t flag must be present");
        assert_eq!(args[t_pos + 1], "nfs", "-t must be followed by nfs");
        let opts = args.iter().find(|a| a.contains("vers=3")).expect("opts must contain vers=3");
        assert!(opts.contains("port=5555"), "opts must specify port");
        assert!(opts.contains("mountport=5555"), "opts must specify mountport");
        assert!(opts.contains("nolock"), "Linux opts must use nolock (not nolocks)");
    }

    #[test]
    fn port_is_embedded_in_opts() {
        let mountpoint = PathBuf::from("/mnt/x");
        for port in [1024u16, 7777, 49152, 65535] {
            let (_prog, args) =
                build_nfs_mount_argv(ClientOs::MacOs, "127.0.0.1", port, &mountpoint);
            let opts = args.iter().find(|a| a.contains("vers=3")).unwrap();
            assert!(
                opts.contains(&format!("port={}", port)),
                "port={} must appear in opts",
                port
            );
            assert!(
                opts.contains(&format!("mountport={}", port)),
                "mountport={} must appear in opts",
                port
            );
        }
    }

    // -----------------------------------------------------------------------
    // Unmount argv tests
    // -----------------------------------------------------------------------

    #[test]
    fn macos_unmount_argv_no_force() {
        let mp = PathBuf::from("/mnt/dropbox");
        let (prog, args) = build_nfs_unmount_argv(ClientOs::MacOs, &mp, false);
        assert_eq!(prog, "diskutil");
        assert_eq!(args, vec!["unmount", "/mnt/dropbox"]);
    }

    #[test]
    fn macos_unmount_argv_force() {
        let mp = PathBuf::from("/mnt/dropbox");
        let (prog, args) = build_nfs_unmount_argv(ClientOs::MacOs, &mp, true);
        assert_eq!(prog, "diskutil");
        assert_eq!(args, vec!["unmount", "force", "/mnt/dropbox"]);
    }

    #[test]
    fn linux_unmount_argv_no_force() {
        let mp = PathBuf::from("/mnt/dropbox");
        let (prog, args) = build_nfs_unmount_argv(ClientOs::Linux, &mp, false);
        assert_eq!(prog, "umount");
        assert_eq!(args, vec!["/mnt/dropbox"]);
    }

    #[test]
    fn linux_unmount_argv_force() {
        let mp = PathBuf::from("/mnt/dropbox");
        let (prog, args) = build_nfs_unmount_argv(ClientOs::Linux, &mp, true);
        assert_eq!(prog, "umount");
        assert_eq!(args, vec!["-f", "/mnt/dropbox"]);
    }

    // -----------------------------------------------------------------------
    // mountpoint_in_mount_output tests
    // -----------------------------------------------------------------------

    #[test]
    fn macos_format_matched() {
        let output = "127.0.0.1:/ on /mnt/a (nfs, noatime, soft)";
        let mp = PathBuf::from("/mnt/a");
        assert!(mountpoint_in_mount_output(output, &mp));
    }

    #[test]
    fn macos_format_not_matched_when_absent() {
        let output = "127.0.0.1:/ on /mnt/b (nfs, noatime, soft)";
        let mp = PathBuf::from("/mnt/a");
        assert!(!mountpoint_in_mount_output(output, &mp));
    }

    #[test]
    fn mountinfo_format_matched() {
        // /proc/self/mountinfo field layout: id parent major:minor root target ...
        let output = "36 35 8:1 / /mnt/a rw,relatime shared:1 - nfs 127.0.0.1:/ rw";
        let mp = PathBuf::from("/mnt/a");
        assert!(mountpoint_in_mount_output(output, &mp));
    }

    #[test]
    fn mountinfo_format_not_matched_when_absent() {
        let output = "36 35 8:1 / /mnt/b rw,relatime shared:1 - nfs 127.0.0.1:/ rw";
        let mp = PathBuf::from("/mnt/a");
        assert!(!mountpoint_in_mount_output(output, &mp));
    }

    #[test]
    fn prefix_boundary_not_matched() {
        // /mnt/a must NOT match a line whose target is /mnt/ab.
        let macos_line = "127.0.0.1:/ on /mnt/ab (nfs, soft)";
        let mp = PathBuf::from("/mnt/a");
        assert!(!mountpoint_in_mount_output(macos_line, &mp));

        let mountinfo_line = "36 35 8:1 / /mnt/ab rw,relatime shared:1 - nfs 127.0.0.1:/ rw";
        assert!(!mountpoint_in_mount_output(mountinfo_line, &mp));
    }
}
