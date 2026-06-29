//! Smoke test: SocketNotifyChannel (TCP-backed notify) with real reconcile.
//! Gated behind LUNAR_SMOKE=1. Without that variable the test returns
//! immediately and is a trivial pass in the deterministic gate.
//!
//! Run via:
//!   LUNAR_SMOKE=1 cargo test --test smoke_ws_notify -- --nocapture

mod common;

use devdropbox::live_sync::{
    ClientReconciler, LiveSnapshot, MemServerApi, NotifyChannel, ServerApi,
    SocketNotifyChannel,
};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

const MAX_WAIT_ITERS: usize = 100;
const WAIT_STEP_MS: u64 = 10;

fn make_snapshot(id: &str, entries: &[(&str, &str)]) -> LiveSnapshot {
    LiveSnapshot {
        id: id.to_string(),
        entries: entries.iter().map(|(p, h)| (p.to_string(), h.to_string())).collect(),
    }
}

/// Verify the server publishes a RefMoveEvent over TCP and the client
/// reconciler receives it, fetches the expected blobs, and reaches the
/// correct mount state. Runs a second advance to confirm sequential delivery.
#[test]
fn smoke_ws_notify_socket_roundtrip() {
    if !common::smoke_enabled() {
        eprintln!("smoke_ws_notify: skip (LUNAR_SMOKE is not set to 1)");
        return;
    }

    // Server: bind on an ephemeral port so OS assigns a free one.
    let server_channel =
        SocketNotifyChannel::server("127.0.0.1:0").expect("socket server must bind");
    let addr = server_channel.local_addr().expect("server must have a local addr");
    eprintln!("smoke_ws_notify: server bound on {}", addr);

    let server_channel = Arc::new(server_channel);
    let api = Arc::new(MemServerApi::new(server_channel.clone() as Arc<dyn NotifyChannel>));

    // Seed S1: two blobs.
    api.seed_blob("ws-h1", b"notify-smoke-blob-h1".to_vec());
    api.seed_blob("ws-h2", b"notify-smoke-blob-h2".to_vec());
    api.seed_snapshot(make_snapshot("s1", &[("alpha.txt", "ws-h1"), ("beta.txt", "ws-h2")]));

    // Seed S2: one new blob, one carried from S1.
    api.seed_blob("ws-h3", b"notify-smoke-blob-h3".to_vec());
    let s2_entries: HashMap<String, String> = [
        ("alpha.txt".to_string(), "ws-h1".to_string()),
        ("gamma.txt".to_string(), "ws-h3".to_string()),
    ]
    .into_iter()
    .collect();
    api.seed_snapshot(LiveSnapshot { id: "s2".to_string(), entries: s2_entries.clone() });

    // Client: connect to the server.
    let client_channel =
        SocketNotifyChannel::connect(&addr.to_string()).expect("socket client must connect");

    // Give the server accept loop time to register the connection (bounded, not blocking).
    std::thread::sleep(std::time::Duration::from_millis(150));

    // Reconciler on the client side uses the authoritative MemServerApi.
    let reconciler =
        Arc::new(ClientReconciler::new(api.clone() as Arc<dyn ServerApi>));

    let received_s1 = Arc::new(AtomicBool::new(false));
    let received_s2 = Arc::new(AtomicBool::new(false));
    let flag_s1 = Arc::clone(&received_s1);
    let flag_s2 = Arc::clone(&received_s2);
    let rec_ref = Arc::clone(&reconciler);

    let _guard = client_channel.subscribe(
        "ws-smoke",
        Box::new(move |event| {
            if rec_ref.reconcile("ws-smoke", &event.snapshot_id).is_ok() {
                if event.snapshot_id == "s1" {
                    flag_s1.store(true, Ordering::Release);
                } else if event.snapshot_id == "s2" {
                    flag_s2.store(true, Ordering::Release);
                }
            }
        }),
    );

    // --- Advance to S1 ---
    api.advance_ref("ws-smoke", "s1");

    // Bounded wait: up to MAX_WAIT_ITERS * WAIT_STEP_MS = 1 000ms.
    for i in 0..MAX_WAIT_ITERS {
        if received_s1.load(Ordering::Acquire) {
            break;
        }
        assert!(i < MAX_WAIT_ITERS - 1, "client must reconcile S1 within 1s over TCP");
        std::thread::sleep(std::time::Duration::from_millis(WAIT_STEP_MS));
    }
    assert!(received_s1.load(Ordering::Acquire), "client must have received S1 event");

    let mount1 = reconciler.current_mount();
    assert_eq!(mount1.snapshot_id.as_deref(), Some("s1"), "mount must be at S1");
    assert!(mount1.entries.contains_key("alpha.txt"), "S1 mount must contain alpha.txt");
    assert!(mount1.entries.contains_key("beta.txt"), "S1 mount must contain beta.txt");
    assert_eq!(mount1.entries.get("alpha.txt").map(|s| s.as_str()), Some("ws-h1"));
    eprintln!("smoke_ws_notify: S1 reconcile verified");

    // --- Advance to S2 ---
    api.advance_ref("ws-smoke", "s2");

    for i in 0..MAX_WAIT_ITERS {
        if received_s2.load(Ordering::Acquire) {
            break;
        }
        assert!(i < MAX_WAIT_ITERS - 1, "client must reconcile S2 within 1s over TCP");
        std::thread::sleep(std::time::Duration::from_millis(WAIT_STEP_MS));
    }
    assert!(received_s2.load(Ordering::Acquire), "client must have received S2 event");

    let mount2 = reconciler.current_mount();
    assert_eq!(mount2.snapshot_id.as_deref(), Some("s2"), "mount must be at S2");
    assert_eq!(mount2.entries, s2_entries, "S2 mount entries must match seeded snapshot");
    assert!(!mount2.entries.contains_key("beta.txt"), "beta.txt must not appear in S2");
    eprintln!("smoke_ws_notify: S2 reconcile verified");

    server_channel.close();
    client_channel.close();
    eprintln!("smoke_ws_notify: all assertions passed");
}
