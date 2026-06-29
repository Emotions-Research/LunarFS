//! Gated FUSE-T real-mount smoke test.
//!
//! Requires macOS + --features fuse AND LUNAR_SMOKE=1 to run.
//! Plain `cargo test` (no features): the no-op variant compiles and passes.
//!
//! Run via: scripts/smoke-fuse.sh
//!
//! Optional env knobs:
//!   LUNAR_SMOKE_READERS       concurrent readers (default 16, cap 256)
//!   LUNAR_SMOKE_DURATION_SECS stress duration in seconds (default 60, cap 600)

// Non-macOS-fuse path: always compiles and passes without mounting.
#[cfg(not(all(target_os = "macos", feature = "fuse")))]
#[test]
fn fuse_t_mount_hydration_and_concurrent_read_stress() {
    eprintln!("smoke_fuse_mount: requires macOS + --features fuse; skipped");
}

// ---------------------------------------------------------------------------
// macOS + --features fuse: real mount, hydration check, concurrent-read stress.
// ---------------------------------------------------------------------------

#[cfg(all(target_os = "macos", feature = "fuse"))]
fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// RAII teardown guard.
///
/// Owns the spawned lunar mount child process and both TempDirs.
/// Drop unmounts (umount, then diskutil unmount force fallback), kills
/// and waits the child, then defensively reaps any go-nfsv4 process
/// FUSE-T may have spawned as a grandchild for this mountpoint.
///
/// Never panics inside Drop: all errors are swallowed so an in-progress
/// panic unwind is not aborted (a panic-in-Drop during unwind aborts).
#[cfg(all(target_os = "macos", feature = "fuse"))]
struct MountGuard {
    mountpoint: std::path::PathBuf,
    /// take()-d in Drop for idempotency; None after first Drop.
    child: Option<std::process::Child>,
    /// Kept alive until after Drop runs (field drop order: mountpoint,
    /// child, _repo_dir, _mount_dir). TempDir ignores removal errors.
    _repo_dir: tempfile::TempDir,
    _mount_dir: tempfile::TempDir,
}

#[cfg(all(target_os = "macos", feature = "fuse"))]
impl Drop for MountGuard {
    fn drop(&mut self) {
        let mp_str = self.mountpoint.to_string_lossy().into_owned();

        // Step 1: unmount. Plain umount first; diskutil unmount force as fallback.
        // Ignore non-fatal errors -- mount may already be gone on the failure path.
        let plain_ok = std::process::Command::new("umount")
            .arg(&self.mountpoint)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !plain_ok {
            let _ = std::process::Command::new("diskutil")
                .args(["unmount", "force", &mp_str])
                .status();
        }

        // Step 2: kill + wait the owned lunar child.
        // take() makes this idempotent under double-drop (child is None after first Drop).
        // Always wait() after kill() to prevent zombies (clippy::zombie_processes).
        // Ignore kill() errors: child may have already exited after the unmount.
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }

        // Step 3: defensive reap of go-nfsv4 scoped to this mountpoint.
        // FUSE-T's libfuse spawns go-nfsv4 internally; it may outlive the lunar
        // child if FUSE-T daemonizes it. Scope by mountpoint path so we never kill
        // an unrelated go-nfsv4 process running on the same machine.
        let _ = std::process::Command::new("pkill")
            .args(["-f", &format!("go-nfsv4.*{}", mp_str)])
            .status();

        // Fields drop after this fn returns in declaration order:
        // PathBuf (trivial), Option<Child> (already None), _repo_dir, _mount_dir.
    }
}

#[cfg(all(target_os = "macos", feature = "fuse"))]
#[test]
fn fuse_t_mount_hydration_and_concurrent_read_stress() {
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    };

    // Run-time gate: skip unless LUNAR_SMOKE=1.
    if std::env::var("LUNAR_SMOKE").as_deref() != Ok("1") {
        eprintln!("smoke_fuse_mount: skipped (set LUNAR_SMOKE=1 to run)");
        return;
    }

    let readers = env_usize("LUNAR_SMOKE_READERS", 16);
    let duration_secs = env_usize("LUNAR_SMOKE_DURATION_SECS", 60);
    assert!(
        (1..=256).contains(&readers),
        "LUNAR_SMOKE_READERS must be in 1..=256, got {}",
        readers
    );
    assert!(
        (1..=600).contains(&duration_secs),
        "LUNAR_SMOKE_DURATION_SECS must be in 1..=600, got {}",
        duration_secs
    );

    // --- Build fixture repo ---
    let repo_dir = tempfile::tempdir().expect("create fixture repo dir");
    let mount_dir = tempfile::tempdir().expect("create mountpoint dir");
    let repo = repo_dir.path().to_path_buf();
    let mountpoint = mount_dir.path().to_path_buf();

    std::fs::create_dir_all(repo.join("a")).expect("create subdir a");
    std::fs::create_dir_all(repo.join("b").join("c")).expect("create subdir b/c");

    // Relative path -> expected bytes (source/CAS truth).
    let mut fixture: Vec<(String, Vec<u8>)> = Vec::new();

    // Sentinel: polled to detect mount readiness.
    std::fs::write(repo.join("sentinel.txt"), b"ready").expect("write sentinel");
    fixture.push(("sentinel.txt".to_string(), b"ready".to_vec()));

    // Small files in a/.
    for i in 0u8..5 {
        let name = format!("a/file{}.txt", i);
        let content: Vec<u8> = (0u8..64).map(|j| i.wrapping_add(j) % 251).collect();
        std::fs::write(repo.join(&name), &content).expect("write small fixture file");
        fixture.push((name, content));
    }

    // Medium files in b/c/ (~8 KB each).
    for i in 0u8..2 {
        let name = format!("b/c/medium{}.bin", i);
        let content: Vec<u8> = (0u16..8192).map(|j| (j as u8).wrapping_add(i) % 251).collect();
        std::fs::write(repo.join(&name), &content).expect("write medium fixture file");
        fixture.push((name, content));
    }

    // One large file (~256 KB) to exercise multi-read hydration.
    {
        let name = "b/large.bin";
        let content: Vec<u8> = (0usize..262_144).map(|j| (j % 251) as u8).collect();
        std::fs::write(repo.join(name), &content).expect("write large fixture file");
        fixture.push((name.to_string(), content));
    }

    assert!(
        fixture.len() >= 8,
        "fixture must contain at least 8 files, got {}",
        fixture.len()
    );

    // --- Spawn mount as a child process; guard owns all teardown ---
    // CARGO_BIN_EXE_lunar is set by cargo for integration tests and points
    // to the binary compiled with the same feature flags as this test.
    let bin = std::env::var("CARGO_BIN_EXE_lunar")
        .unwrap_or_else(|_| "lunar".to_string());
    let child = std::process::Command::new(&bin)
        .arg("mount")
        .arg(&repo)
        .arg(&mountpoint)
        .spawn()
        .expect("failed to spawn lunar mount -- ensure the binary is built with --features fuse");

    // Guard takes ownership of the child and both TempDirs.
    // Drop runs unconditionally on every exit path: normal return, assertion
    // failure, and panic unwind. Never call child.wait() in the test body.
    let _guard = MountGuard {
        mountpoint: mountpoint.clone(),
        child: Some(child),
        _repo_dir: repo_dir,
        _mount_dir: mount_dir,
    };

    // --- Poll for readiness (bounded: up to ~30s at 100ms intervals) ---
    // A failing assert here triggers _guard.drop() which unmounts and reaps.
    let sentinel_path = mountpoint.join("sentinel.txt");
    let mut ready = false;
    for _ in 0..300 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if std::fs::read(&sentinel_path)
            .map(|b| b == b"ready")
            .unwrap_or(false)
        {
            ready = true;
            break;
        }
    }
    assert!(ready, "smoke_fuse_mount: mount never became ready within 30s");
    eprintln!("smoke_fuse_mount: mount ready");

    // --- Hydration correctness: each fixture file read through the mount ---
    let mut hydration_errors: Vec<String> = Vec::new();
    for (rel, expected) in &fixture {
        match std::fs::read(mountpoint.join(rel)) {
            Ok(ref actual) if actual == expected => {}
            Ok(actual) => hydration_errors.push(format!(
                "{}: bytes mismatch ({} bytes got, {} expected)",
                rel,
                actual.len(),
                expected.len()
            )),
            Err(e) => hydration_errors.push(format!("{}: read error: {}", rel, e)),
        }
    }
    // A failing assert here triggers _guard.drop() (unmount + reap).
    assert!(
        hydration_errors.is_empty(),
        "smoke_fuse_mount: hydration errors:\n{}",
        hydration_errors.join("\n")
    );
    eprintln!("smoke_fuse_mount: {} files hydrated correctly", fixture.len());

    // --- Concurrent-read stress ---
    // Each entry carries the mounted path and the expected source bytes so
    // threads can do a full byte comparison instead of only checking Ok/Err.
    let targets: Arc<Vec<(std::path::PathBuf, Vec<u8>)>> = Arc::new(
        fixture
            .iter()
            .map(|(rel, bytes)| (mountpoint.join(rel), bytes.clone()))
            .collect(),
    );
    let n_targets = targets.len();
    assert!(n_targets > 0, "targets must not be empty before stress run");

    let total_reads = Arc::new(AtomicU64::new(0));
    let total_errors = Arc::new(AtomicU64::new(0));
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(duration_secs as u64);

    eprintln!(
        "smoke_fuse_mount: starting stress ({} readers x {}s)",
        readers, duration_secs
    );
    let stress_start = std::time::Instant::now();

    let reader_handles: Vec<_> = (0..readers)
        .map(|i| {
            let targets = Arc::clone(&targets);
            let reads = Arc::clone(&total_reads);
            let errs = Arc::clone(&total_errors);
            std::thread::spawn(move || {
                let mut local_reads: u64 = 0;
                let mut local_errors: u64 = 0;
                let mut idx: usize = i;
                while std::time::Instant::now() < deadline {
                    let (path, expected) = &targets[idx % n_targets];
                    match std::fs::read(path) {
                        Ok(actual) if actual == *expected => local_reads += 1,
                        Ok(_) => local_errors += 1, // wrong length or wrong content, includes 0-byte empty read and truncation
                        Err(_) => local_errors += 1,
                    }
                    idx = idx.wrapping_add(1);
                }
                reads.fetch_add(local_reads, Ordering::Relaxed);
                errs.fetch_add(local_errors, Ordering::Relaxed);
            })
        })
        .collect();

    // Collect thread outcomes; panic after the guard drops if any panicked.
    let mut panicked_threads: Vec<usize> = Vec::new();
    for (i, h) in reader_handles.into_iter().enumerate() {
        if h.join().is_err() {
            panicked_threads.push(i);
        }
    }
    let elapsed_secs = stress_start.elapsed().as_secs_f64();

    // --- Mount survival: check sentinel while the mount is still up ---
    let survived = std::fs::read(&sentinel_path)
        .map(|b| b == b"ready")
        .unwrap_or(false);

    // --- Report ---
    let reads = total_reads.load(Ordering::Relaxed);
    let errors = total_errors.load(Ordering::Relaxed);
    let reads_per_sec = if elapsed_secs > 0.0 {
        reads as f64 / elapsed_secs
    } else {
        0.0
    };
    eprintln!(
        "smoke_fuse_mount: reads={} errors={} reads/sec={:.1} survival={} ({} readers x {}s)",
        reads, errors, reads_per_sec, survived, readers, duration_secs
    );

    if !panicked_threads.is_empty() {
        panic!(
            "smoke_fuse_mount: reader threads panicked: {:?}",
            panicked_threads
        );
    }

    eprintln!(
        "smoke_fuse_mount: PASSED ({} readers x {}s)",
        readers, duration_secs
    );

    assert_eq!(errors, 0, "concurrent-read stress must have zero read errors");
    assert!(survived, "mount must still serve reads after the stress run");
    // _guard drops here: unmount + kill lunar child + defensive go-nfsv4 reap.
}
