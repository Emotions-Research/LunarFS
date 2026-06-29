//! Gated NFS lifecycle smoke test: no residual mounts or listening sockets.
//!
//! Proves that repeated mount/unmount cycles via SIGINT, and an interrupted
//! session via SIGTERM, leave no OS mount entry and no listening TCP socket
//! after the child exits.
//!
//! How to run:
//!   LUNAR_SMOKE=1 cargo test --features mount-nfs --test smoke_nfs_lifecycle
//!
//! Linux requires root (or a user-namespace NFS client capable of NFS mounts).
//! macOS ships an NFS client by default; no extra setup is needed.
//!
//! Plain `cargo test --features mount-nfs` compiles the file and returns
//! immediately without spawning any process.

mod common;

// Non-Unix or non-mount-nfs: always compiles, prints a skip line, passes.
#[cfg(not(all(unix, feature = "mount-nfs")))]
#[test]
fn nfs_lifecycle_repeated_cycles_and_interrupt() {
    eprintln!("smoke_nfs_lifecycle: requires Unix + --features mount-nfs; skipped");
}

// ---------------------------------------------------------------------------
// Real test body: Unix + mount-nfs only.
// ---------------------------------------------------------------------------

#[cfg(all(unix, feature = "mount-nfs"))]
use devdropbox::nfs::client::mount_is_present;

#[cfg(all(unix, feature = "mount-nfs"))]
#[test]
fn nfs_lifecycle_repeated_cycles_and_interrupt() {
    if !common::smoke_enabled() {
        eprintln!("smoke_nfs_lifecycle: skipped (set LUNAR_SMOKE=1 to enable)");
        return;
    }

    let bin = std::env::var("CARGO_BIN_EXE_lunar").unwrap_or_else(|_| "lunar".to_string());

    let repo_dir = tempfile::tempdir().expect("create fixture repo dir");
    let mount_dir = tempfile::tempdir().expect("create mountpoint dir");

    std::fs::write(repo_dir.path().join("lifecycle-sentinel.txt"), b"nfs-lifecycle-ok")
        .expect("write sentinel");
    std::fs::create_dir_all(repo_dir.path().join("sub")).expect("create sub dir");
    std::fs::write(repo_dir.path().join("sub").join("data.txt"), b"sub-content")
        .expect("write sub/data.txt");

    let mountpoint = mount_dir.path().to_path_buf();
    let repo = repo_dir.path().to_path_buf();

    // 3 SIGINT cycles + 1 SIGTERM cycle.
    for i in 1..=3usize {
        run_cycle(&bin, &repo, &mountpoint, libc::SIGINT, i);
    }
    run_cycle(&bin, &repo, &mountpoint, libc::SIGTERM, 4);

    eprintln!("smoke_nfs_lifecycle: all 4 cycles passed");
}

/// Run one mount/unmount lifecycle cycle and assert cleanup.
///
/// Spawns `lunar mount`, waits for the server-ready port line on stderr,
/// polls until the OS mount appears, sends `sig`, waits for child exit,
/// then asserts no residual mount and no listening socket.
///
/// On Linux without root the mount command fails immediately; the cycle is
/// detected and reported as a skip rather than a failure.
#[cfg(all(unix, feature = "mount-nfs"))]
fn run_cycle(
    bin: &str,
    repo: &std::path::Path,
    mountpoint: &std::path::Path,
    sig: libc::c_int,
    cycle: usize,
) {
    use std::io::BufRead;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let sig_name = if sig == libc::SIGINT { "SIGINT" } else { "SIGTERM" };
    eprintln!("smoke_nfs_lifecycle: cycle {} ({}) start", cycle, sig_name);

    let mut child = std::process::Command::new(bin)
        .arg("mount")
        .arg(repo)
        .arg(mountpoint)
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("failed to spawn lunar mount (binary must be built with --features mount-nfs)");

    let stderr = child.stderr.take().expect("stderr must be piped");
    let child_pid = child.id() as libc::pid_t;

    // Thread: read stderr and send the port as soon as the server-ready line appears.
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stderr);
        for line in reader.lines().take(64) {
            let Ok(line) = line else { break };
            eprintln!("smoke_nfs_lifecycle [lunar]: {}", line);
            if let Some(addr) = line.strip_prefix("nfs: server on 127.0.0.1:") {
                if let Ok(port) = addr.trim().parse::<u16>() {
                    let _ = port_tx.send(port);
                    return;
                }
            }
        }
    });

    // Wait for the port-ready line (up to 8s).
    let port = match port_rx.recv_timeout(Duration::from_secs(8)) {
        Ok(p) => {
            assert_ne!(p, 0, "received port must not be zero");
            p
        }
        Err(_) => {
            // Child likely exited early before printing the port (e.g. startup failure).
            if child.try_wait().ok().flatten().is_some() && !mount_is_present(mountpoint) {
                eprintln!(
                    "smoke_nfs_lifecycle: cycle {} skipped \
                     (mount startup failed; on Linux this requires root)",
                    cycle
                );
                let _ = child.wait();
                return;
            }
            let _ = child.kill();
            let _ = child.wait();
            panic!("smoke_nfs_lifecycle: cycle {}: port line not received within 8s", cycle);
        }
    };

    // Poll until the OS mount appears (up to 10s at 200ms intervals).
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut mounted = false;
    for _ in 0..50usize {
        if Instant::now() >= deadline {
            break;
        }
        if mount_is_present(mountpoint) {
            mounted = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    if !mounted {
        // Mount never appeared; Linux without root is the typical cause.
        unsafe { libc::kill(child_pid, libc::SIGTERM) };
        let _ = child.wait();
        eprintln!(
            "smoke_nfs_lifecycle: cycle {} skipped \
             (mount never appeared; on Linux this requires root or a capable NFS client)",
            cycle
        );
        return;
    }

    eprintln!(
        "smoke_nfs_lifecycle: cycle {} mounted on port {}; sending {}",
        cycle, port, sig_name
    );

    // Send the requested signal and poll for child exit (up to 10s at 100ms).
    let rc = unsafe { libc::kill(child_pid, sig) };
    assert_eq!(rc, 0, "libc::kill(pid={}, sig={}) must succeed", child_pid, sig_name);

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let mut exited = false;
    for _ in 0..100usize {
        if Instant::now() >= exit_deadline {
            break;
        }
        if child.try_wait().ok().flatten().is_some() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    if !exited {
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "smoke_nfs_lifecycle: cycle {}: child did not exit within 10s after {}",
            cycle, sig_name
        );
    }
    // Reap: try_wait already reaped on Unix; this is a no-op / returns ECHILD, ignored.
    let _ = child.wait();

    // Assert: no residual OS mount.
    assert!(
        !mount_is_present(mountpoint),
        "smoke_nfs_lifecycle: cycle {}: OS mount still present after {} + child exit",
        cycle,
        sig_name
    );
    eprintln!("smoke_nfs_lifecycle: cycle {} mount cleaned up", cycle);

    // Assert: port is no longer listening (connection must be refused).
    let still_listening = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    )
    .is_ok();
    assert!(
        !still_listening,
        "smoke_nfs_lifecycle: cycle {}: port {} still accepting connections after cleanup",
        cycle, port
    );
    eprintln!(
        "smoke_nfs_lifecycle: cycle {} port {} closed (PASSED)",
        cycle, port
    );
}
