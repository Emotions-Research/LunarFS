//! NFS server bootstrap: bind to loopback ephemeral port and spawn the server task.
//!
//! Caller learns the bound port before invoking the OS NFS client. Call
//! `NfsServer::shutdown` to abort the task and release the bound loopback port.

use std::io;
use std::sync::Arc;

use anyhow::Result;
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use tokio::task::JoinHandle;

use super::fs::CasNfs;

/// A running NFSv3 server bound to a loopback ephemeral port.
///
/// The server task runs in the background. Call `shutdown()` to stop it and
/// release the bound port. `task()` gives mutable access to the JoinHandle so
/// callers can race it inside `tokio::select!`.
pub struct NfsServer {
    pub port: u16,
    // Retained so dropping it (alongside aborting the task) releases the OS port binding.
    _listener: Arc<NFSTcpListener<CasNfs>>,
    handle: JoinHandle<io::Result<()>>,
}

impl NfsServer {
    /// Mutable borrow of the background task handle for use in `tokio::select!`.
    pub fn task(&mut self) -> &mut JoinHandle<io::Result<()>> {
        &mut self.handle
    }

    /// Stop the server. Aborts the task and drops the listener Arc so the loopback
    /// port is released immediately. Aborting the task alone is not sufficient: the
    /// spawned closure holds one Arc clone, but this struct holds a second; both must
    /// be dropped before the OS releases the bound address.
    pub fn shutdown(self) {
        self.handle.abort();
        // Explicit drop order: abort first, then release the listener Arc.
        drop(self._listener);
    }
}

/// Bind a loopback NFSv3 server on an OS-assigned ephemeral port and return an
/// owning handle to the running server.
pub async fn serve(fs: CasNfs) -> Result<NfsServer> {
    let listener = NFSTcpListener::bind("127.0.0.1:0", fs)
        .await
        .map_err(|e| anyhow::anyhow!("NFS server bind failed on 127.0.0.1:0: {}", e))?;

    let port = listener.get_listen_port();
    assert!(port != 0, "OS must assign a non-zero ephemeral port");

    let listener = Arc::new(listener);
    // A second Arc clone enters the task; `listener` stays in NfsServer so shutdown
    // can drop it and release the port after the task is aborted.
    let arc_task = Arc::clone(&listener);
    let handle = tokio::spawn(async move { arc_task.handle_forever().await });

    Ok(NfsServer {
        port,
        _listener: listener,
        handle,
    })
}
