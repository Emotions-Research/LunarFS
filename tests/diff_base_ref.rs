//! Integration test: base_ref resolution for `lunar diff <workspace>`.
//!
//! Binding confirmed against src/main.rs do_diff (one-arg path, lines 872-893):
//!   1. resolve_arg(&a, &store) -> Resolved::Ws(ws)  (label or id lookup)
//!   2. ws.base_ref is read directly from the workspace record
//!   3. resolve_arg(&ws.base_ref, &store) re-resolves the ref to a hash
//!
//! resolve_arg is private to main.rs, so tests bind to the public store +
//! workspace API that it consults. Every assertion maps to a step in that chain.
//!
//! All tests pass under `cargo test` and `cargo test --features hosted`
//! because workspace.rs and store.rs carry no cfg(feature = "hosted") gates.

use devdropbox::store::{InMemoryWorkspaceStore, SqliteWorkspaceStore, WorkspaceStore};
use devdropbox::workspace::{create_workspace, FakeClock, InMemoryBackend, WorkspaceSpec, WsId};
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::time::UNIX_EPOCH;

fn spec(base_ref: &str) -> WorkspaceSpec {
    WorkspaceSpec {
        base_ref: base_ref.to_string(),
        label: None,
        metadata: BTreeMap::new(),
        ttl: None,
        root: None,
    }
}

fn clock() -> FakeClock {
    FakeClock::new(UNIX_EPOCH)
}

fn open_sqlite() -> SqliteWorkspaceStore {
    let conn = Connection::open_in_memory().expect("in-memory SQLite must open");
    SqliteWorkspaceStore::open(conn).expect("SqliteWorkspaceStore::open must succeed")
}

// ── (a) Fork-time fidelity ──────────────────────────────────────────────────
//
// The workspace record in the store must carry the exact base_ref string that
// was supplied at fork time. This is what do_diff reads as the "old" side.

#[test]
fn base_ref_stored_at_fork_time_in_memory() {
    let backend = InMemoryBackend::new();
    let store = InMemoryWorkspaceStore::new();
    let id = WsId("ws-fidelity-mem".to_string());
    let expected = "base-v1-exact-ref";

    let ws = create_workspace(&backend, &store, &clock(), id.clone(), spec(expected))
        .expect("create_workspace must succeed");

    assert_eq!(
        ws.base_ref, expected,
        "returned Workspace.base_ref must equal the fork-time base"
    );

    let stored = store
        .get(&id)
        .expect("store.get must not error")
        .expect("workspace must be present");

    assert_eq!(
        stored.base_ref, expected,
        "store.get must return the same base_ref as at fork time"
    );
}

#[test]
fn base_ref_stored_at_fork_time_sqlite() {
    let backend = InMemoryBackend::new();
    let store = open_sqlite();
    let id = WsId("ws-fidelity-sql".to_string());
    let expected = "base-v1-exact-ref";

    let ws = create_workspace(&backend, &store, &clock(), id.clone(), spec(expected))
        .expect("create_workspace must succeed");

    assert_eq!(
        ws.base_ref, expected,
        "returned Workspace.base_ref must equal the fork-time base (SQLite)"
    );

    let stored = store
        .get(&id)
        .expect("store.get must not error")
        .expect("workspace must be present");

    assert_eq!(
        stored.base_ref, expected,
        "SQLite store.get must return the same base_ref as at fork time"
    );
}

// ── (b) Fork-time immutability ──────────────────────────────────────────────
//
// After a newer base is created (a second workspace forked from a later ref),
// the original workspace still resolves to the base it was forked from, not
// the newer tip. base_ref is stored once and never auto-updated by the library.

#[test]
fn base_ref_immutable_after_newer_base_in_memory() {
    let backend = InMemoryBackend::new();
    let store = InMemoryWorkspaceStore::new();
    let id_old = WsId("ws-old-mem".to_string());
    let id_new = WsId("ws-new-mem".to_string());
    let base_old = "base-v1";
    let base_new = "base-v2";

    create_workspace(&backend, &store, &clock(), id_old.clone(), spec(base_old))
        .expect("fork from base-v1 must succeed");
    create_workspace(&backend, &store, &clock(), id_new.clone(), spec(base_new))
        .expect("fork from base-v2 must succeed");

    let ws_old = store
        .get(&id_old)
        .expect("store.get ws-old must not error")
        .expect("ws-old must be present");

    assert_eq!(
        ws_old.base_ref, base_old,
        "original workspace must still carry base-v1 after base-v2 was created"
    );
    assert_ne!(
        ws_old.base_ref, base_new,
        "original workspace must NOT be updated to the newer base"
    );
}

#[test]
fn base_ref_immutable_after_newer_base_sqlite() {
    let backend = InMemoryBackend::new();
    let store = open_sqlite();
    let id_old = WsId("ws-old-sql".to_string());
    let id_new = WsId("ws-new-sql".to_string());
    let base_old = "base-v1";
    let base_new = "base-v2";

    create_workspace(&backend, &store, &clock(), id_old.clone(), spec(base_old))
        .expect("fork from base-v1 must succeed");
    create_workspace(&backend, &store, &clock(), id_new.clone(), spec(base_new))
        .expect("fork from base-v2 must succeed");

    let ws_old = store
        .get(&id_old)
        .expect("store.get ws-old must not error")
        .expect("ws-old must be present");
    let ws_new = store
        .get(&id_new)
        .expect("store.get ws-new must not error")
        .expect("ws-new must be present");

    assert_eq!(
        ws_old.base_ref, base_old,
        "original workspace must retain its fork-time base_ref (SQLite)"
    );
    assert_eq!(
        ws_new.base_ref, base_new,
        "newer workspace must carry its own fork-time base_ref"
    );
    assert_ne!(
        ws_old.base_ref, ws_new.base_ref,
        "the two workspaces must have distinct base_refs"
    );
}

// ── (c) Documented fallback: "HEAD" ─────────────────────────────────────────
//
// The CLI default_value for `--from` is "HEAD" (src/main.rs:158).
// At the library level there is no implicit default: WorkspaceSpec.base_ref must
// be non-empty (asserted in create_workspace). The fallback is the literal string
// "HEAD" injected by the CLI before calling create_workspace.
// This test proves the library stores "HEAD" verbatim (no silent remapping).

#[test]
fn default_cli_base_ref_head_stored_verbatim() {
    let backend = InMemoryBackend::new();
    let store = InMemoryWorkspaceStore::new();
    let id = WsId("ws-head-base".to_string());

    let ws = create_workspace(&backend, &store, &clock(), id.clone(), spec("HEAD"))
        .expect("create_workspace with base_ref=HEAD must succeed");

    assert_eq!(
        ws.base_ref, "HEAD",
        "CLI default base_ref HEAD must be stored verbatim by the library"
    );

    let stored = store
        .get(&id)
        .expect("store.get must not error")
        .expect("workspace must be present");

    assert_eq!(
        stored.base_ref, "HEAD",
        "store must round-trip the HEAD base_ref verbatim"
    );
}

// ── Edge case: fresh fork with no root ──────────────────────────────────────
//
// do_diff (one-arg path) requires ws.root to be set; it returns an error if not.
// This test confirms the library correctly records base_ref even before any root
// is committed, and that root is None on a fresh fork.

#[test]
fn freshly_forked_workspace_has_base_ref_no_root() {
    let backend = InMemoryBackend::new();
    let store = InMemoryWorkspaceStore::new();
    let id = WsId("ws-no-root".to_string());
    let base = "base-fresh-no-root";

    let ws = create_workspace(&backend, &store, &clock(), id.clone(), spec(base))
        .expect("create_workspace must succeed");

    assert_eq!(ws.base_ref, base, "base_ref must be set on fresh fork");
    assert!(
        ws.root.is_none(),
        "root must be None on fresh fork (no commits yet)"
    );
}

// ── Edge case: unknown workspace id returns None ─────────────────────────────
//
// In do_diff, resolve_arg returns an error when the arg can't be matched.
// The underlying mechanism is store.list_all() returning no match -- not a panic.
// Tested at both store implementations.

#[test]
fn unknown_workspace_id_returns_none_in_memory() {
    let store = InMemoryWorkspaceStore::new();
    let result = store
        .get(&WsId("nonexistent".to_string()))
        .expect("store.get must not error for unknown id");
    assert!(result.is_none(), "unknown id must return None, not panic");
}

#[test]
fn unknown_workspace_id_returns_none_sqlite() {
    let store = open_sqlite();
    let result = store
        .get(&WsId("nonexistent".to_string()))
        .expect("store.get must not error for unknown id (SQLite)");
    assert!(result.is_none(), "unknown id must return None in SQLite store");
}

// ── list_all round-trips base_refs for multiple workspaces ──────────────────
//
// do_ws_diff calls list_workspaces (-> list_all) to build the grouped diff view.
// Each returned workspace must carry the base_ref it was forked with.

#[test]
fn list_all_returns_correct_base_refs_sqlite() {
    let backend = InMemoryBackend::new();
    let store = open_sqlite();

    let pairs: &[(&str, &str)] = &[
        ("ws-list-a", "base-alpha"),
        ("ws-list-b", "base-beta"),
        ("ws-list-c", "base-alpha"),
    ];

    for (id, base) in pairs {
        create_workspace(
            &backend,
            &store,
            &clock(),
            WsId((*id).to_string()),
            spec(base),
        )
        .expect("create_workspace must succeed");
    }

    let all = store.list_all().expect("list_all must not error");
    assert_eq!(
        all.len(),
        pairs.len(),
        "list_all must return all created workspaces"
    );

    for (id, expected_base) in pairs {
        let ws = all.iter().find(|w| w.id.0 == *id);
        assert!(ws.is_some(), "workspace {} must appear in list_all", id);
        assert_eq!(
            ws.expect("just checked").base_ref,
            *expected_base,
            "list_all entry for {} must carry the correct base_ref",
            id
        );
    }
}
