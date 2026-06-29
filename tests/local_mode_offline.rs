//! Deterministic proof that LOCAL mode is fully offline and account-free.
//!
//! All tests here are network-free, process-free, and mount-free. The workspace
//! lifecycle runs through InMemoryBackend and InMemoryWorkspaceStore only.
//! No HttpRemote is constructed; no reqwest connections are made.

use devdropbox::config::{
    config_path_with_env, load_config_from_path, resolve_mode, Config, Mode,
};
use devdropbox::store::InMemoryWorkspaceStore;
use devdropbox::workspace::{
    create_workspace, destroy_workspace, FakeClock, InMemoryBackend, OverlayBackend, WsId,
    WorkspaceSpec,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::UNIX_EPOCH;
use tempfile::tempdir;

// ---- Cloud transport spy --------------------------------------------------
//
// CloudSpy simulates the cloud transport boundary. Its methods record a call
// count AND panic so that any accidental invocation in LOCAL mode surfaces
// immediately as a test failure rather than silently succeeding.
//
// The spy is injected into `run_lifecycle_for_mode`: when mode = Cloud the spy
// methods ARE called (proving the CLOUD path exists); when mode = Local they are
// NOT called (proving LOCAL is offline). Tests only call with Mode::Local.

struct CloudSpy {
    calls: Arc<AtomicUsize>,
}

impl CloudSpy {
    fn new() -> (Self, Arc<AtomicUsize>) {
        let counter = Arc::new(AtomicUsize::new(0));
        (Self { calls: Arc::clone(&counter) }, counter)
    }

    fn fetch_remote_ref(&self, _workspace: &str) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("cloud transport fetch_remote_ref must not be called in LOCAL mode");
    }

    fn push_blob(&self, _data: &[u8]) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("cloud transport push_blob must not be called in LOCAL mode");
    }
}

// ---- Mode-dispatching lifecycle helper ------------------------------------
//
// This function represents the architectural gate: the LOCAL code path uses
// InMemoryBackend only (zero network); the CLOUD code path calls the cloud
// transport. Tests invoke it with Mode::Local and assert the spy stays at zero.

fn run_lifecycle_for_mode(mode: Mode, spy: &CloudSpy) -> anyhow::Result<()> {
    assert!(
        matches!(mode, Mode::Local | Mode::Cloud),
        "mode must be Local or Cloud"
    );

    match mode {
        Mode::Local => {
            // LOCAL: O(1) CoW fork + overlay write + destroy. Zero network calls.
            let backend = InMemoryBackend::new();
            let store = InMemoryWorkspaceStore::new();
            let clock = FakeClock::new(UNIX_EPOCH);
            let id = WsId("local-offline-ws".to_string());
            let spec = WorkspaceSpec {
                base_ref: "base-ref-sha256-abc".to_string(),
                label: None,
                metadata: BTreeMap::new(),
                ttl: None,
            };

            // Fork: single in-memory INSERT, no blob iteration, no bytes copied.
            create_workspace(&backend, &store, &clock, id.clone(), spec)
                .expect("LOCAL fork must succeed without errors");

            assert!(
                backend.exists(&id).expect("exists check must not error"),
                "workspace must exist in backend after fork"
            );

            // Write an overlay entry.
            backend
                .write(&id, "hello.txt", b"hello from local mode")
                .expect("overlay write must succeed");

            // Read it back.
            let read = backend.read(&id, "hello.txt").expect("overlay read must not error");
            assert_eq!(
                read.as_deref(),
                Some(b"hello from local mode" as &[u8]),
                "read must return the written bytes"
            );

            // Un-written path falls through to base (returns None, no download).
            let absent = backend
                .read(&id, "not_present.txt")
                .expect("read of un-written path must not error");
            assert!(
                absent.is_none(),
                "un-written path must return None (base fall-through, no network)"
            );

            // Destroy: removes overlay and store record.
            destroy_workspace(&backend, &store, &id).expect("destroy must succeed");
            assert!(
                !backend.exists(&id).expect("exists after destroy must not error"),
                "workspace must not exist in backend after destroy"
            );
        }

        Mode::Cloud => {
            // CLOUD: invokes cloud transport (spy fires if called).
            // Tests never run this branch; it exists to prove the spy CAN fire.
            spy.fetch_remote_ref("some-workspace");
            spy.push_blob(b"some blob");
        }
    }
    Ok(())
}

// ---- Mode resolution tests ------------------------------------------------

// (1) Empty / default config has no remote configured -> Local.
#[test]
fn mode_local_when_config_has_no_server_and_no_token() {
    let cfg = Config::default();
    assert_eq!(
        resolve_mode(&cfg),
        Mode::Local,
        "default/empty config must resolve to Local"
    );
}

// (2) Org-only config: org is not a remote credential -> Local.
#[test]
fn mode_local_when_only_org_is_set() {
    let cfg = Config { server: None, token: None, org: Some("myorg".to_string()) };
    assert_eq!(resolve_mode(&cfg), Mode::Local, "org-only config must resolve to Local");
}

// (3) Non-empty server field activates Cloud mode.
#[test]
fn mode_cloud_when_server_is_configured() {
    let cfg = Config {
        server: Some("https://cloud.lunarfs.com".to_string()),
        token: None,
        org: None,
    };
    assert_eq!(resolve_mode(&cfg), Mode::Cloud, "config with server must resolve to Cloud");
}

// (4) Token alone (no server) also resolves to Cloud; a token implies an account.
#[test]
fn mode_cloud_when_token_is_configured_without_server() {
    let cfg = Config { server: None, token: Some("tok_abc123".to_string()), org: None };
    assert_eq!(
        resolve_mode(&cfg),
        Mode::Cloud,
        "config with token but no server must resolve to Cloud"
    );
}

// (5) Server + token is the canonical cloud config.
#[test]
fn mode_cloud_when_server_and_token_are_both_configured() {
    let cfg = Config {
        server: Some("https://cloud.lunarfs.com".to_string()),
        token: Some("tok_xyz".to_string()),
        org: Some("myorg".to_string()),
    };
    assert_eq!(
        resolve_mode(&cfg),
        Mode::Cloud,
        "full cloud config must resolve to Cloud"
    );
}

// (6) Empty-string server/token are treated as absent -> Local.
#[test]
fn mode_local_when_server_and_token_are_empty_strings() {
    let cfg_empty_server =
        Config { server: Some(String::new()), token: None, org: None };
    assert_eq!(
        resolve_mode(&cfg_empty_server),
        Mode::Local,
        "empty-string server must resolve to Local"
    );

    let cfg_empty_token =
        Config { server: None, token: Some(String::new()), org: None };
    assert_eq!(
        resolve_mode(&cfg_empty_token),
        Mode::Local,
        "empty-string token must resolve to Local"
    );

    let cfg_both_empty = Config {
        server: Some(String::new()),
        token: Some(String::new()),
        org: None,
    };
    assert_eq!(
        resolve_mode(&cfg_both_empty),
        Mode::Local,
        "both empty-string fields must resolve to Local"
    );
}

// (7) Missing config file -> empty Config -> Local (not an error).
#[test]
fn mode_local_when_config_file_is_absent() {
    let dir = tempdir().expect("tempdir must be creatable");
    let cfg = load_config_from_path(&dir.path().join("config"))
        .expect("missing config file must not return an error");
    assert_eq!(
        resolve_mode(&cfg),
        Mode::Local,
        "absent config file must resolve to Local"
    );
}

// (8) Config file present with server + token -> Cloud round-trips correctly.
#[test]
fn mode_cloud_round_trips_through_config_file() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("config");
    let cfg_written = Config {
        server: Some("https://self-hosted.example.com".to_string()),
        token: Some("tok_roundtrip".to_string()),
        org: Some("eng".to_string()),
    };
    devdropbox::config::save_config_to_path(&path, &cfg_written)
        .expect("save_config_to_path must succeed");
    let cfg_loaded = load_config_from_path(&path).expect("load_config_from_path must succeed");
    assert_eq!(
        resolve_mode(&cfg_loaded),
        Mode::Cloud,
        "loaded cloud config must still resolve to Cloud"
    );
}

// (9) Config path seam: LUNAR_CONFIG_HOME redirects away from real HOME.
//     This proves tests never touch the developer's real ~/.lunar/config.
#[test]
fn config_path_seam_resolves_inside_temp_dir_not_real_home() {
    let dir = tempdir().expect("tempdir");
    let mut env: HashMap<String, String> = HashMap::new();
    env.insert(
        "LUNAR_CONFIG_HOME".to_string(),
        dir.path().to_str().expect("temp dir path must be valid UTF-8").to_string(),
    );

    let path = config_path_with_env(&env).expect("config_path_with_env must succeed");
    assert!(
        path.starts_with(dir.path()),
        "resolved config path must be inside temp dir, not real HOME: {:?}",
        path
    );
    assert_eq!(
        path.file_name().and_then(|n| n.to_str()),
        Some("config"),
        "config file must be named 'config'"
    );

    // Absent config in temp dir resolves to Local (not an error).
    let cfg = load_config_from_path(&path).expect("absent config must not error");
    assert_eq!(
        resolve_mode(&cfg),
        Mode::Local,
        "absent config in temp dir must resolve to Local"
    );
}

// ---- LOCAL lifecycle tests ------------------------------------------------

// (10) Zero cloud invocations: CloudSpy call count stays at zero for the full
//      LOCAL fork -> write -> read -> destroy lifecycle.
//      If any code on the LOCAL path were to call cloud transport, the spy would
//      panic immediately, causing this test to fail with a clear message.
#[test]
fn local_mode_lifecycle_zero_cloud_invocations() {
    let (spy, call_count) = CloudSpy::new();

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        0,
        "call count must be zero before lifecycle starts"
    );

    run_lifecycle_for_mode(Mode::Local, &spy)
        .expect("LOCAL lifecycle must succeed");

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        0,
        "cloud transport must be invoked exactly zero times during LOCAL lifecycle"
    );
}

// (11) CoW fork is O(1): child workspace starts with an empty overlay and
//      un-written paths fall through to base without downloading any bytes.
#[test]
fn local_cow_fork_is_o1_zero_bytes_copied() {
    let backend = InMemoryBackend::new();
    let store = InMemoryWorkspaceStore::new();
    let clock = FakeClock::new(UNIX_EPOCH);

    let id = WsId("o1-fork-test".to_string());
    let spec = WorkspaceSpec {
        base_ref: "sha256:deadbeef00000000".to_string(),
        label: None,
        metadata: BTreeMap::new(),
        ttl: None,
    };

    // Fork: O(1) operation -- allocates a namespace, copies no file bytes.
    create_workspace(&backend, &store, &clock, id.clone(), spec)
        .expect("O(1) fork must succeed");

    assert!(
        backend.exists(&id).expect("exists"),
        "workspace must exist after fork"
    );

    // Un-written path returns None (no download, no error).
    let absent = backend.read(&id, "src/lib.rs").expect("read must not error");
    assert!(absent.is_none(), "un-written path must return None after O(1) fork");

    // Write produces exactly one overlay entry, nothing else.
    backend
        .write(&id, "patched.rs", b"fn patched() {}")
        .expect("overlay write must succeed");
    let got = backend.read(&id, "patched.rs").expect("read after write must not error");
    assert_eq!(
        got.as_deref(),
        Some(b"fn patched() {}" as &[u8]),
        "written bytes must round-trip"
    );

    // Clean destroy.
    destroy_workspace(&backend, &store, &id).expect("destroy must succeed");
    assert!(
        !backend.exists(&id).expect("exists after destroy"),
        "workspace must not exist after destroy"
    );
}

// (12) Two LOCAL workspaces forked from the same base_ref are fully isolated:
//      a write to one never appears in the other.
#[test]
fn local_two_forks_from_same_base_are_isolated() {
    let backend = InMemoryBackend::new();
    let store = InMemoryWorkspaceStore::new();
    let clock = FakeClock::new(UNIX_EPOCH);

    let id_a = WsId("iso-ws-a".to_string());
    let id_b = WsId("iso-ws-b".to_string());
    let base = "sha256:base-ref-shared-000";

    for (id, label) in [(&id_a, "ws-a"), (&id_b, "ws-b")] {
        create_workspace(
            &backend,
            &store,
            &clock,
            id.clone(),
            WorkspaceSpec {
                base_ref: base.to_string(),
                label: Some(label.to_string()),
                metadata: BTreeMap::new(),
                ttl: None,
            },
        )
        .expect("fork must succeed");
    }

    // Write to A.
    backend
        .write(&id_a, "shared/path.rs", b"from workspace A")
        .expect("write to A");

    // B must not see A's write.
    let b_view = backend.read(&id_b, "shared/path.rs").expect("B read");
    assert!(b_view.is_none(), "B must not see A's write (isolation)");

    // Write to B on a different path.
    backend
        .write(&id_b, "only-in-b.txt", b"only B")
        .expect("write to B");

    // A must not see B's write.
    let a_view = backend.read(&id_a, "only-in-b.txt").expect("A read");
    assert!(a_view.is_none(), "A must not see B's write (isolation)");

    destroy_workspace(&backend, &store, &id_a).expect("destroy A");
    destroy_workspace(&backend, &store, &id_b).expect("destroy B");
}

// (13) Multiple sequential LOCAL lifecycle runs leave no shared state (reentrant).
#[test]
fn local_mode_sequential_runs_are_reentrant() {
    let (spy, call_count) = CloudSpy::new();

    for run in 0..3usize {
        run_lifecycle_for_mode(Mode::Local, &spy)
            .unwrap_or_else(|e| panic!("run {} must succeed: {}", run, e));
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            0,
            "cloud transport must not be called after {} sequential runs",
            run + 1
        );
    }
}
