//! Concurrent multi-agent isolation smoke test.
//!
//! Gated behind LUNAR_SMOKE=1. Without that variable the test returns
//! immediately and adds nothing to the default unit gate.

use devdropbox::overlay::{AgentId, OverlayStore, Resolution, WorkspaceId};
use rusqlite::Connection;
use std::sync::Arc;

const WS: WorkspaceId = 1;
const DEFAULT_AGENTS: usize = 16;
const DEFAULT_ITERS: usize = 50;
const SHARED_PATH: &str = "shared/base.txt";
const MAX_AGENTS: usize = 256;
const MAX_ITERS: usize = 1_000;

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn make_store(db_path: &std::path::Path) -> Arc<OverlayStore> {
    let conn = Connection::open(db_path).expect("sqlite open must succeed");
    let store = Arc::new(OverlayStore::new(conn));
    store.init_schema().expect("init_schema must succeed");
    store
}

fn agent_private_path(agent: AgentId) -> String {
    format!("agent-{}/private.txt", agent)
}

fn agent_unique_hash(agent: AgentId) -> String {
    format!("hash-agent-{}-unique", agent)
}

fn run_agent_thread(
    store: Arc<OverlayStore>,
    agent: AgentId,
    n_iters: usize,
    deletes_shared: bool,
) {
    assert!(n_iters <= MAX_ITERS, "n_iters must not exceed cap");
    let private_path = agent_private_path(agent);
    let private_hash = agent_unique_hash(agent);

    for iter in 0..n_iters {
        store
            .capture_write(agent, WS, &private_path, &private_hash)
            .unwrap_or_else(|e| {
                panic!(
                    "agent {} capture_write private at iter {}: {}",
                    agent, iter, e
                )
            });

        let iter_path = format!("agent-{}/iter-{}.txt", agent, iter % 10);
        let iter_hash = format!("hash-{}-iter-{}", agent, iter % 10);
        store
            .capture_write(agent, WS, &iter_path, &iter_hash)
            .unwrap_or_else(|e| {
                panic!("agent {} capture_write iter at iter {}: {}", agent, iter, e)
            });

        if deletes_shared {
            store
                .capture_delete(agent, WS, SHARED_PATH)
                .unwrap_or_else(|e| {
                    panic!("agent {} capture_delete at iter {}: {}", agent, iter, e)
                });
        }

        // Assert own write is immediately visible within this agent's view.
        let res = store
            .resolve(agent, &private_path)
            .unwrap_or_else(|e| panic!("agent {} resolve private at iter {}: {}", agent, iter, e));
        assert!(
            matches!(res, Resolution::Overlay(ref h) if h == &private_hash),
            "agent {} must see its own write at iter {}",
            agent,
            iter
        );
    }
}

fn assert_post_run_isolation(store: &OverlayStore, agents: &[(AgentId, bool)]) {
    assert!(
        agents.len() <= MAX_AGENTS,
        "agent count must not exceed cap"
    );

    for &(agent_a, deletes_a) in agents {
        let path_a = agent_private_path(agent_a);
        let hash_a = agent_unique_hash(agent_a);

        // Each agent must see its own hash.
        let own = store.resolve(agent_a, &path_a).expect("resolve own path");
        assert!(
            matches!(own, Resolution::Overlay(ref h) if *h == hash_a),
            "agent {} must see its own hash after all threads joined",
            agent_a
        );

        // Shared path: Tombstone if the agent deleted it, Base otherwise.
        let shared = store
            .resolve(agent_a, SHARED_PATH)
            .expect("resolve shared path");
        if deletes_a {
            assert!(
                matches!(shared, Resolution::Tombstone),
                "agent {} deleted shared path, expected Tombstone",
                agent_a
            );
        } else {
            assert!(
                matches!(shared, Resolution::Base),
                "agent {} did not delete shared path, expected Base",
                agent_a
            );
        }

        // Cross-agent isolation: A must NOT see any other agent's private path.
        for &(agent_b, _) in agents {
            if agent_b == agent_a {
                continue;
            }
            let cross = store
                .resolve(agent_a, &agent_private_path(agent_b))
                .expect("resolve cross-agent path");
            assert!(
                matches!(cross, Resolution::Base),
                "isolation violation: agent {} can see agent {}'s private path",
                agent_a,
                agent_b
            );
        }
    }
}

#[test]
fn concurrent_agent_isolation() {
    if std::env::var("LUNAR_SMOKE").as_deref() != Ok("1") {
        eprintln!("smoke_concurrency: skipped (set LUNAR_SMOKE=1 to run)");
        return;
    }

    let n_agents = env_usize("LUNAR_SMOKE_AGENTS", DEFAULT_AGENTS);
    let n_iters = env_usize("LUNAR_SMOKE_ITERS", DEFAULT_ITERS);
    assert!(
        n_agents > 0 && n_agents <= MAX_AGENTS,
        "LUNAR_SMOKE_AGENTS must be in 1..={}",
        MAX_AGENTS
    );
    assert!(
        n_iters > 0 && n_iters <= MAX_ITERS,
        "LUNAR_SMOKE_ITERS must be in 1..={}",
        MAX_ITERS
    );

    let dir = tempfile::tempdir().expect("tempdir must create");
    let store = make_store(&dir.path().join("smoke.db"));

    // Fork all agents before spawning threads so IDs are assigned sequentially.
    let agents: Vec<(AgentId, bool)> = (0..n_agents)
        .map(|i| {
            let id = store.fork(WS).expect("fork must succeed");
            (id, i % 3 == 0)
        })
        .collect();

    let handles: Vec<_> = agents
        .iter()
        .map(|&(agent, deletes)| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || run_agent_thread(store, agent, n_iters, deletes))
        })
        .collect();

    for (handle, &(agent, _)) in handles.into_iter().zip(agents.iter()) {
        handle
            .join()
            .unwrap_or_else(|_| panic!("thread for agent {} panicked", agent));
    }

    assert_post_run_isolation(&store, &agents);
    eprintln!(
        "smoke_concurrency: PASSED ({} agents x {} iters)",
        n_agents, n_iters
    );
}
