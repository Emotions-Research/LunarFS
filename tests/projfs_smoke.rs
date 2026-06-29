#![cfg(target_os = "windows")]
//! ProjFS integration smoke: lazy-hydration byte-correctness and concurrent-read stress.
//!
//! Compile-gated: the top-level inner attribute `#![cfg(target_os = "windows")]`
//! makes this file produce zero compiled output on macOS or Linux. The macOS CI
//! gate stays green and executes no code from this file.
//!
//! Runtime-gated: LUNAR_SMOKE=1 must be set in the environment. Without it,
//! every test function returns immediately without touching ProjFS.
//!
//! Prerequisites (Windows CI only):
//!   - The Windows "Projected File System" optional feature must be enabled.
//!   - The binary must be built with: cargo build --features projfs
//!
//! Run command (Windows CI only, with LUNAR_SMOKE=1 set):
//!   cargo test --features projfs --test projfs_smoke -- --nocapture

use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn smoke_enabled() -> bool {
    std::env::var("LUNAR_SMOKE").as_deref() == Ok("1")
}

fn lunar_bin() -> String {
    std::env::var("CARGO_BIN_EXE_lunar")
        .unwrap_or_else(|_| "lunar".to_string())
}

/// Deterministic bytes for fixture file with the given `salt` and `len`.
///
/// Two files with distinct salts produce non-overlapping byte sequences even
/// when the same `j` index is used, so cross-file byte confusion is detectable.
fn make_fixture(salt: u8, len: usize) -> Vec<u8> {
    assert!(len <= 16 * 1024 * 1024, "fixture len must not exceed 16 MiB");
    (0..len).map(|j| ((j % 251) as u8) ^ salt).collect()
}

/// Probes whether a real ProjFS mount works on this host.
///
/// Spawns `lunar mount` into fresh temp dirs via the same live entry point
/// the smoke tests use (`spawn_mount` helper above), waits for mount readiness,
/// then tears down immediately. Returns true only on a clean mount + unmount
/// round trip. Returns false for any error: binary not found, spawn failure,
/// mount timeout. Never panics. The verdict derives from the actual mount
/// attempt, not from reading Windows optional-feature flags (which report
/// enabled-pending-reboot and can lie).
fn projfs_mount_probe() -> bool {
    let sentinel_bytes: &[u8] = b"projfs-probe-ready";
    assert!(!sentinel_bytes.is_empty(), "probe sentinel must not be empty");

    let repo_dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(_) => return false,
    };
    let mount_dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(_) => return false,
    };

    if std::fs::write(repo_dir.path().join("sentinel.txt"), sentinel_bytes).is_err() {
        return false;
    }

    let bin = lunar_bin();
    let mut child = match std::process::Command::new(&bin)
        .arg("mount")
        .arg(repo_dir.path())
        .arg(mount_dir.path())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    // Poll until the sentinel appears in the virtual root (bounded; no panic on timeout).
    let sentinel_path = mount_dir.path().join("sentinel.txt");
    const MAX_POLLS: usize = 100;
    const POLL_MS: u64 = 100;
    let mut mounted = false;
    for _ in 0..MAX_POLLS {
        std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
        if std::fs::read(&sentinel_path)
            .map(|b| b == sentinel_bytes)
            .unwrap_or(false)
        {
            mounted = true;
            break;
        }
    }

    // Kill before temp dirs drop: ProjFS must not be virtualizing a removed dir.
    let _ = child.kill();
    let _ = child.wait();

    mounted
}

// ---------------------------------------------------------------------------
// RAII guard: owns the child process and temp directories
// ---------------------------------------------------------------------------

/// Kills the lunar mount child process and cleans up temp directories on drop.
///
/// Fields drop in declaration order after `fn drop` returns: child (already None),
/// then _repo_dir and _mount_dir. The child must be killed before the directories
/// are removed so ProjFS is no longer virtualizing the (now-gone) mount dir.
struct ProjFsGuard {
    mountpoint: PathBuf,
    child: Option<std::process::Child>,
    _repo_dir: TempDir,
    _mount_dir: TempDir,
}

impl Drop for ProjFsGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // kill() then wait() prevents zombie processes (clippy::zombie_processes).
            // Ignore errors: the child may have already exited on the failure path.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ---------------------------------------------------------------------------
// Mount setup and readiness polling
// ---------------------------------------------------------------------------

/// Spawn `lunar mount <repo_dir> <mount_dir>` and return a guard owning it.
///
/// The guard takes ownership of both TempDirs so they live until after the child
/// is killed in Drop. Panics immediately if the spawn fails.
fn spawn_mount(repo_dir: TempDir, mount_dir: TempDir) -> ProjFsGuard {
    let bin = lunar_bin();
    let child = std::process::Command::new(&bin)
        .arg("mount")
        .arg(repo_dir.path())
        .arg(mount_dir.path())
        .spawn()
        .expect(
            "failed to spawn lunar mount: ensure the binary is built with \
             --features projfs and the Windows Projected File System optional feature \
             is enabled on this machine",
        );
    ProjFsGuard {
        mountpoint: mount_dir.path().to_path_buf(),
        child: Some(child),
        _repo_dir: repo_dir,
        _mount_dir: mount_dir,
    }
}

/// Poll until `sentinel_path` is readable and contains `expected_bytes`.
///
/// Tries up to 300 times at 100ms intervals (30 seconds total). Panics on timeout
/// so Drop on the guard runs and kills the child before the test binary exits.
fn wait_for_ready(sentinel_path: &std::path::Path, expected_bytes: &[u8]) {
    assert!(!expected_bytes.is_empty(), "sentinel content must not be empty");
    const MAX_POLLS: usize = 300;
    const POLL_MS: u64 = 100;
    let mut ready = false;
    for _ in 0..MAX_POLLS {
        std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
        if std::fs::read(sentinel_path)
            .map(|b| b == expected_bytes)
            .unwrap_or(false)
        {
            ready = true;
            break;
        }
    }
    assert!(
        ready,
        "projfs_smoke: ProjFS mount never became ready within {}ms \
         (sentinel not readable at {:?})",
        MAX_POLLS as u64 * POLL_MS,
        sentinel_path
    );
}

// ---------------------------------------------------------------------------
// ProjFS availability detection seam
// ---------------------------------------------------------------------------
// Gated on the `projfs` feature because PrjMarkDirectoryAsPlaceholder (and the
// windows crate types it needs) are only compiled into the build when projfs is
// active. The seam probes availability before the live child-process spawn so
// the test can skip cleanly on hosts where ProjFS is not installed, without
// waiting for the 30-second poll timeout.

#[cfg(feature = "projfs")]
mod detect {
    use windows::Win32::Storage::ProjectedFileSystem::PrjMarkDirectoryAsPlaceholder;
    use windows::core::{GUID, PCWSTR};

    // Feature-not-present HRESULT allowlist. Each constant is matched by numeric
    // code against the HRESULT returned by PrjMarkDirectoryAsPlaceholder. A future
    // reviewer can audit this list to confirm no legitimate mount errors are silenced.

    // HRESULT_FROM_WIN32(ERROR_MOD_NOT_FOUND) = 0x8007007E
    // ProjectedFSLib.dll is absent: the optional component Client-ProjFS is not installed.
    const HR_FNP_MOD_NOT_FOUND: i32 = 0x8007007Eu32 as i32;

    // HRESULT_FROM_WIN32(ERROR_PROC_NOT_FOUND) = 0x8007007F
    // DLL is present but the called entry point is unresolvable (version mismatch or
    // partial installation of the optional component).
    const HR_FNP_PROC_NOT_FOUND: i32 = 0x8007007Fu32 as i32;

    // HRESULT_FROM_WIN32(ERROR_NOT_SUPPORTED) = 0x80070032
    // ProjFS is installed but the kernel driver is not yet active (reboot pending
    // after optional-feature enablement).
    const HR_FNP_NOT_SUPPORTED: i32 = 0x80070032u32 as i32;

    // HRESULT_FROM_WIN32(ERROR_INVALID_FUNCTION) = 0x80070001
    // Seen on some Windows editions when the ProjFS driver has not been started after
    // initial installation (driver start deferred to first reboot).
    const HR_FNP_INVALID_FUNCTION: i32 = 0x80070001u32 as i32;

    /// Returns true iff `e` carries a feature-not-present HRESULT code.
    ///
    /// Matches only the exact numeric codes in the allowlist above; does NOT match
    /// on error message text. Errors not in the allowlist (access denied, path not
    /// found, etc.) return false and continue to fail loudly so regressions on
    /// ProjFS-enabled hosts are not swallowed.
    pub fn is_feature_not_present(e: &windows::core::Error) -> bool {
        let c = e.code().0;
        c == HR_FNP_MOD_NOT_FOUND
            || c == HR_FNP_PROC_NOT_FOUND
            || c == HR_FNP_NOT_SUPPORTED
            || c == HR_FNP_INVALID_FUNCTION
    }

    /// Classification of ProjFS start availability on this host.
    #[derive(Debug)]
    pub enum ProjfsAvailability {
        /// ProjFS optional feature is present and the API entry point resolved.
        Available,
        /// ProjFS is not installed, not yet started, or waiting for a reboot.
        Unavailable,
    }

    /// Probe ProjFS availability without starting a live provider.
    ///
    /// Calls PrjMarkDirectoryAsPlaceholder on a fresh temp directory. If
    /// ProjectedFSLib.dll or its entry point is absent, or if the feature needs
    /// a reboot, the call returns a feature-not-present HRESULT and this function
    /// returns Ok(Unavailable). If the call succeeds, the probe directory is
    /// cleaned up and Ok(Available) is returned. Any other error propagates as
    /// Err (unexpected, treated as a genuine failure by callers).
    ///
    /// An empty placeholder root is still removable by TempDir::drop when no
    /// provider is running, so cleanup is safe after a successful probe.
    pub fn probe_projfs_available() -> std::io::Result<ProjfsAvailability> {
        let probe_dir = tempfile::tempdir()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        let wide: Vec<u16> = {
            use std::os::windows::ffi::OsStrExt;
            let mut w: Vec<u16> = probe_dir.path().as_os_str().encode_wide().collect();
            w.push(0);
            w
        };

        let mut guid_bytes = [0u8; 16];
        getrandom::getrandom(&mut guid_bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        let probe_guid = GUID {
            data1: u32::from_ne_bytes(guid_bytes[0..4].try_into().unwrap()),
            data2: u16::from_ne_bytes(guid_bytes[4..6].try_into().unwrap()),
            data3: u16::from_ne_bytes(guid_bytes[6..8].try_into().unwrap()),
            data4: guid_bytes[8..16].try_into().unwrap(),
        };

        let result = unsafe {
            PrjMarkDirectoryAsPlaceholder(
                PCWSTR(wide.as_ptr()),
                PCWSTR(std::ptr::null::<u16>()),
                None,
                &probe_guid,
            )
        };
        // wide must remain alive until PrjMarkDirectoryAsPlaceholder returns.
        drop(wide);
        // probe_dir drops here: TempDir::drop deletes the empty directory.
        drop(probe_dir);

        match result {
            Ok(()) => Ok(ProjfsAvailability::Available),
            Err(e) if is_feature_not_present(&e) => Ok(ProjfsAvailability::Unavailable),
            Err(e) => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("ProjFS availability probe failed unexpectedly: {}", e),
            )),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn make_err(code: i32) -> windows::core::Error {
            windows::core::Error::from(windows::core::HRESULT(code))
        }

        #[test]
        fn feature_not_present_dll_absent() {
            assert!(is_feature_not_present(&make_err(HR_FNP_MOD_NOT_FOUND)));
        }

        #[test]
        fn feature_not_present_proc_absent() {
            assert!(is_feature_not_present(&make_err(HR_FNP_PROC_NOT_FOUND)));
        }

        #[test]
        fn feature_not_present_not_supported() {
            assert!(is_feature_not_present(&make_err(HR_FNP_NOT_SUPPORTED)));
        }

        #[test]
        fn feature_not_present_invalid_function() {
            assert!(is_feature_not_present(&make_err(HR_FNP_INVALID_FUNCTION)));
        }

        #[test]
        fn feature_not_present_access_denied() {
            // HRESULT_FROM_WIN32(ERROR_ACCESS_DENIED) = 0x80070005: real error, must NOT skip.
            assert!(!is_feature_not_present(&make_err(0x80070005u32 as i32)));
        }

        #[test]
        fn feature_not_present_e_fail() {
            // E_FAIL = 0x80004005: generic failure, must NOT trigger the skip path.
            assert!(!is_feature_not_present(&make_err(0x80004005u32 as i32)));
        }
    }
}

// ---------------------------------------------------------------------------
// Test A: lazy-hydration byte-correctness
// ---------------------------------------------------------------------------

/// Prove that the ProjFS provider serves correct placeholder metadata before a
/// file's data is read, then hydrates the exact source bytes on first access.
///
/// Sequence: stat (triggers GetPlaceholderInfo, no data read) then read
/// (triggers GetFileData, lazy hydration). Both must return byte-exact results.
#[test]
fn projfs_lazy_hydration_byte_correctness() -> Result<(), Box<dyn std::error::Error>> {
    if !smoke_enabled() {
        eprintln!(
            "projfs_smoke: lazy_hydration_byte_correctness skipped \
             (set LUNAR_SMOKE=1 to enable)"
        );
        return Ok(());
    }

    if !projfs_mount_probe() {
        eprintln!("ProjFS not active on this runner (feature likely needs reboot after enable); skipping live-mount smoke");
        return Ok(());
    }

    const FILE_LEN: usize = 4096;
    let fixture_bytes: Vec<u8> = make_fixture(0xA5, FILE_LEN);
    let sentinel_bytes: &[u8] = b"projfs-smoke-ready";

    let repo_dir = tempfile::tempdir().expect("create fixture repo dir");
    let mount_dir = tempfile::tempdir().expect("create mountpoint dir");

    std::fs::write(repo_dir.path().join("sentinel.txt"), sentinel_bytes)
        .expect("write sentinel to repo");
    std::fs::write(repo_dir.path().join("fixture.bin"), &fixture_bytes)
        .expect("write fixture to repo");

    let guard = spawn_mount(repo_dir, mount_dir);
    wait_for_ready(&guard.mountpoint.join("sentinel.txt"), sentinel_bytes);
    eprintln!("projfs_smoke: mount ready (lazy_hydration test)");

    let fixture_path = guard.mountpoint.join("fixture.bin");

    // Stat the virtual file: this triggers GetPlaceholderInfo, not GetFileData.
    // The placeholder must report the exact source size before any data is read.
    let meta = std::fs::metadata(&fixture_path).expect(
        "projfs_smoke: stat of fixture.bin must succeed: placeholder must be visible \
         in the virtual root before data is read",
    );
    assert_eq!(
        meta.len(),
        FILE_LEN as u64,
        "projfs_smoke: placeholder size ({}) must equal source size ({}) before any data read",
        meta.len(),
        FILE_LEN
    );
    assert!(!meta.is_dir(), "projfs_smoke: fixture.bin must not be reported as a directory");
    eprintln!("projfs_smoke: placeholder metadata correct (size={})", meta.len());

    // Read the file through the virtual root: this triggers GetFileData (lazy hydration).
    // The hydrated bytes must be exactly equal to the source fixture bytes.
    let got = std::fs::read(&fixture_path)
        .expect("projfs_smoke: read of fixture.bin through virtual root must succeed");

    assert_eq!(
        got.len(),
        fixture_bytes.len(),
        "projfs_smoke: hydrated byte count ({}) must equal source length ({})",
        got.len(),
        fixture_bytes.len()
    );
    assert_eq!(
        got,
        fixture_bytes,
        "projfs_smoke: every hydrated byte must exactly equal the corresponding source byte"
    );
    eprintln!("projfs_smoke: lazy_hydration_byte_correctness PASSED");
    // guard drops here: kills the lunar child process; TempDirs are removed.
    Ok(())
}

// ---------------------------------------------------------------------------
// Test B: concurrent-read stress
// ---------------------------------------------------------------------------

/// Prove that concurrent GetFileData callbacks across multiple files and threads
/// return byte-exact results with no data races, torn reads, or panics.
///
/// K files, T threads, each thread reads all K files R times and checks every
/// byte. Any thread-level mismatch or panic is collected and reported after all
/// threads join.
#[test]
fn projfs_concurrent_read_stress() -> Result<(), Box<dyn std::error::Error>> {
    if !smoke_enabled() {
        eprintln!(
            "projfs_smoke: concurrent_read_stress skipped \
             (set LUNAR_SMOKE=1 to enable)"
        );
        return Ok(());
    }

    if !projfs_mount_probe() {
        eprintln!("ProjFS not active on this runner (feature likely needs reboot after enable); skipping live-mount smoke");
        return Ok(());
    }

    const K: usize = 4; // fixture files
    const T: usize = 4; // reader threads
    const R: usize = 8; // rounds per thread (each round reads all K files once)

    let sentinel_bytes: &[u8] = b"projfs-concurrent-ready";
    let repo_dir = tempfile::tempdir().expect("create fixture repo dir");
    let mount_dir = tempfile::tempdir().expect("create mountpoint dir");

    std::fs::write(repo_dir.path().join("sentinel.txt"), sentinel_bytes)
        .expect("write sentinel");

    // K files with distinct salts and varying sizes to maximize hydration diversity.
    let file_sizes: [usize; K] = [512, 1024, 2048, 8192];
    let mut fixtures: Vec<(String, Vec<u8>)> = Vec::with_capacity(K);
    for (i, &len) in file_sizes.iter().enumerate() {
        let name = format!("file{}.bin", i);
        let bytes = make_fixture(i as u8, len);
        std::fs::write(repo_dir.path().join(&name), &bytes).expect("write fixture file");
        fixtures.push((name, bytes));
    }
    assert_eq!(fixtures.len(), K, "must have exactly K fixture files before mounting");

    let guard = spawn_mount(repo_dir, mount_dir);
    wait_for_ready(&guard.mountpoint.join("sentinel.txt"), sentinel_bytes);
    eprintln!("projfs_smoke: mount ready (concurrent_read_stress test)");

    let mountpoint: Arc<PathBuf> = Arc::new(guard.mountpoint.clone());
    let fixtures_arc: Arc<Vec<(String, Vec<u8>)>> = Arc::new(fixtures);

    // T threads, each reading all K files for R rounds.
    let handles: Vec<_> = (0..T)
        .map(|t| {
            let mp = Arc::clone(&mountpoint);
            let fx = Arc::clone(&fixtures_arc);
            std::thread::spawn(move || {
                let mut errors: Vec<String> = Vec::new();
                for r in 0..R {
                    for (name, expected) in fx.as_ref() {
                        match std::fs::read(mp.join(name)) {
                            Ok(actual) if actual == *expected => {}
                            Ok(actual) => errors.push(format!(
                                "thread={} round={} file={}: byte mismatch \
                                 (got {} bytes, expected {})",
                                t, r, name, actual.len(), expected.len()
                            )),
                            Err(e) => errors.push(format!(
                                "thread={} round={} file={}: read error: {}",
                                t, r, name, e
                            )),
                        }
                    }
                }
                errors
            })
        })
        .collect();

    let mut all_errors: Vec<String> = Vec::new();
    let mut panicked: Vec<usize> = Vec::new();
    for (i, h) in handles.into_iter().enumerate() {
        match h.join() {
            Ok(errs) => all_errors.extend(errs),
            Err(_) => panicked.push(i),
        }
    }

    if !panicked.is_empty() {
        panic!("projfs_smoke: reader threads panicked: {:?}", panicked);
    }
    assert!(
        all_errors.is_empty(),
        "projfs_smoke: concurrent-read errors ({} total):\n{}",
        all_errors.len(),
        all_errors.join("\n")
    );
    let total_reads = T * R * K;
    eprintln!(
        "projfs_smoke: concurrent_read_stress PASSED \
         ({} threads x {} rounds x {} files = {} reads)",
        T, R, fixtures_arc.len(), total_reads
    );
    // guard drops here: kills the lunar child process; TempDirs are removed.
    Ok(())
}
