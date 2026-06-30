//! NFSv3 loopback mount backend (read-only).
//!
//! Feature gate: `mount-nfs`. The whole module is absent from other builds.
//!
//! Flow: Core::new -> IdTable -> CasNfs -> tokio runtime -> serve (bind) ->
//!       run_os_nfs_mount -> wait for exit or signal -> unmount + shutdown.

pub mod client;
pub mod fs;
pub mod ids;
pub mod overlay_view;
pub mod serve;

use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::core::Core;
use fs::CasNfs;

/// RAII guard that force-unmounts `mountpoint` when dropped.
///
/// Checks `mount_is_present` before unmounting so a double-drop (explicit
/// unmount followed by guard firing) is a no-op.
struct MountGuard {
    mountpoint: PathBuf,
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        if client::mount_is_present(&self.mountpoint) {
            if let Err(e) = client::run_os_nfs_unmount(&self.mountpoint, true) {
                eprintln!("nfs: cleanup unmount failed (best-effort): {}", e);
            }
        }
    }
}

/// Mount `repo`'s CAS view at `mountpoint` via a loopback NFSv3 server.
///
/// Builds the in-memory index, binds an NFS server on 127.0.0.1 on an
/// OS-assigned ephemeral port, then invokes the OS NFS client to mount it.
///
/// Blocks until a signal (SIGINT / SIGTERM / Ctrl-C) is received or the
/// server exits. On every exit path (signal, server error, mount failure,
/// or panic unwind) the OS mount is removed and the embedded server is shut
/// down, releasing the loopback port. A stale prior mount at `mountpoint`
/// is force-unmounted before starting.
pub fn mount_nfs(repo: &Path, mountpoint: &Path) -> Result<()> {
    anyhow::ensure!(
        repo.is_dir(),
        "repo must be an existing directory: {}",
        repo.display()
    );
    anyhow::ensure!(
        mountpoint.is_dir(),
        "mountpoint must be an existing directory: {}",
        mountpoint.display()
    );

    // Stale-mount recovery: clear any dangling prior mount before starting.
    if client::mount_is_present(mountpoint) {
        eprintln!(
            "nfs: stale mount detected at {}; force-unmounting before mounting",
            mountpoint.display()
        );
        if let Err(e) = client::run_os_nfs_unmount(mountpoint, true) {
            eprintln!("nfs: force-unmount warning: {}", e);
        }
        if client::mount_is_present(mountpoint) {
            anyhow::bail!(
                "stale mount at {} could not be cleared; unmount manually before retrying",
                mountpoint.display()
            );
        }
    }

    let core = Core::new(repo)?;
    let fs = CasNfs::new(core.index, core.store);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("tokio runtime build failed: {}", e))?;

    rt.block_on(async {
        let mut server = serve::serve(fs).await?;
        eprintln!("nfs: server on 127.0.0.1:{}", server.port);

        // On mount failure: release the port immediately; nothing was mounted.
        if let Err(e) = client::run_os_nfs_mount(server.port, mountpoint) {
            server.shutdown();
            return Err(e.context("OS NFS mount command failed"));
        }
        eprintln!(
            "nfs: mounted {} at {}",
            repo.display(),
            mountpoint.display()
        );

        // RAII guard: unmounts on every exit path below, including panic unwinds.
        let _guard = MountGuard {
            mountpoint: mountpoint.to_path_buf(),
        };

        // Race the server task against OS signals.
        let result = wait_for_signal_or_server(&mut server).await;

        // Abort the task and release the loopback port. The guard drops after
        // this block returns and removes the OS mount via run_os_nfs_unmount.
        server.shutdown();

        result
    })
}

/// Race the server task against SIGINT and SIGTERM.
///
/// Returns Ok(()) on a clean shutdown signal or a cancelled/clean server exit.
/// Returns Err for genuine server errors or task panics. Caller must call
/// server.shutdown() and allow the MountGuard to drop after this returns.
#[cfg(unix)]
async fn wait_for_signal_or_server(server: &mut serve::NfsServer) -> Result<()> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigint = signal(SignalKind::interrupt())
        .map_err(|e| anyhow::anyhow!("failed to install SIGINT handler: {}", e))?;
    let mut sigterm = signal(SignalKind::terminate())
        .map_err(|e| anyhow::anyhow!("failed to install SIGTERM handler: {}", e))?;

    tokio::select! {
        outcome = server.task() => map_server_outcome(outcome),
        _ = sigint.recv() => {
            eprintln!("nfs: received SIGINT; shutting down");
            Ok(())
        }
        _ = sigterm.recv() => {
            eprintln!("nfs: received SIGTERM; shutting down");
            Ok(())
        }
    }
}

/// Race the server task against Ctrl-C (non-Unix fallback).
#[cfg(not(unix))]
async fn wait_for_signal_or_server(server: &mut serve::NfsServer) -> Result<()> {
    tokio::select! {
        outcome = server.task() => map_server_outcome(outcome),
        _ = tokio::signal::ctrl_c() => {
            eprintln!("nfs: received Ctrl-C; shutting down");
            Ok(())
        }
    }
}

fn map_server_outcome(
    outcome: std::result::Result<std::io::Result<()>, tokio::task::JoinError>,
) -> Result<()> {
    match outcome {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(anyhow::anyhow!("NFS server error: {}", e)),
        Err(e) if e.is_cancelled() => Ok(()),
        Err(e) => Err(anyhow::anyhow!("NFS server task panicked: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_guard_drop_on_unmounted_path_is_noop() {
        // Path that will never appear in the mount table. mount_is_present
        // returns false, so Drop skips the unmount command entirely.
        let guard = MountGuard {
            mountpoint: PathBuf::from("/nfs_guard_test_never_mounted_9x7z"),
        };
        drop(guard);
    }
}
