//! Pure overlay-routing helpers for the FUSE layer.
//! These functions operate on plain data types and can be unit-tested without
//! a live FUSE mount.

#[cfg(all(target_os = "macos", feature = "fuse"))]
pub mod ops;
pub mod resolve;
pub mod translate;

use crate::cas::{hash_to_hex, hex_to_hash, Hash, Store};
use crate::index::Index;
use crate::overlay::{AgentId, OverlayStore, Resolution, WorkspaceId};
use std::io;

/// Resolve a read for (agent, path) through the overlay, then the CAS base.
///
/// Returns Ok(Some(bytes)) for a hit, Ok(None) when the path is tombstoned or
/// absent from both layers (caller must return ENOENT), Err on I/O failure.
///
/// Isolation invariant: the resolution is scoped to `agent`. Other agents'
/// writes are invisible regardless of their overlay state.
pub fn route_read(
    store: &dyn Store,
    index: &Index,
    overlay: &OverlayStore,
    agent: AgentId,
    path: &str,
    offset: usize,
    size: usize,
) -> io::Result<Option<Vec<u8>>> {
    assert!(path.len() <= 4096, "path must not exceed 4096 bytes");
    assert!(
        size <= 128 * 1024 * 1024,
        "requested size must be <= 128 MiB"
    );

    let resolution = overlay
        .resolve(agent, path)
        .map_err(|e| io::Error::other(e.to_string()))?;

    let blob: Vec<u8> = match resolution {
        Resolution::Tombstone => return Ok(None),
        Resolution::Overlay(hex) => {
            let hash = hex_to_hash(&hex)?;
            store.get(&hash)?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "overlay blob missing from CAS")
            })?
        }
        Resolution::Base => match index.lookup(path) {
            None => return Ok(None),
            Some(hash) => store.get(&hash)?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "base blob missing from CAS")
            })?,
        },
    };

    let start = offset.min(blob.len());
    let end = (offset + size).min(blob.len());
    Ok(Some(blob[start..end].to_vec()))
}

/// Write `data` into the CAS and record the write for `agent` in the overlay.
///
/// The CAS write is content-addressed (immutable by definition), so it never
/// mutates existing blobs or other agents' overlay entries. Only the overlay
/// row for (agent, path) changes.
///
/// Returns the raw BLAKE3 hash of the stored blob.
pub fn route_write(
    store: &dyn Store,
    overlay: &OverlayStore,
    agent: AgentId,
    workspace: WorkspaceId,
    path: &str,
    data: &[u8],
) -> io::Result<Hash> {
    assert!(agent > 0, "agent must be a positive rowid");
    assert!(!path.is_empty(), "path must not be empty for a write");

    let hash = store.put(data)?;
    let hex = hash_to_hex(&hash);
    overlay
        .capture_write(agent, workspace, path, &hex)
        .map_err(|e| io::Error::other(e.to_string()))?;
    Ok(hash)
}

/// Record a delete for `agent` as a tombstone in the overlay.
///
/// The base layer and every other agent's overlay are unaffected.
pub fn route_delete(
    overlay: &OverlayStore,
    agent: AgentId,
    workspace: WorkspaceId,
    path: &str,
) -> io::Result<()> {
    assert!(agent > 0, "agent must be a positive rowid");
    assert!(!path.is_empty(), "path must not be empty for a delete");

    overlay
        .capture_delete(agent, workspace, path)
        .map_err(|e| io::Error::other(e.to_string()))
}

/// Returns true if `path` is tombstoned (deleted) for `agent`.
/// On any overlay error, returns false conservatively (do not hide the file).
pub fn is_tombstoned(overlay: &OverlayStore, agent: AgentId, path: &str) -> bool {
    matches!(overlay.resolve(agent, path), Ok(Resolution::Tombstone))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::MemStore;
    use crate::overlay::OverlayStore;
    use crate::tree::{serialize_tree, TreeEntry, MODE_FILE};
    use rusqlite::Connection;

    const WS: WorkspaceId = 1;

    /// Build a MemStore + Index with one flat file.
    fn make_base(name: &str, content: &[u8]) -> (MemStore, Index) {
        let store = MemStore::new();
        let file_hash = store.put(content).unwrap();
        let tree_bytes = serialize_tree(&[TreeEntry {
            mode: MODE_FILE,
            name: name.to_string(),
            hash: file_hash,
        }]);
        let root_hash = store.put(&tree_bytes).unwrap();
        let index = Index::build(&store, &root_hash).unwrap();
        (store, index)
    }

    fn make_overlay() -> OverlayStore {
        let conn = Connection::open_in_memory().unwrap();
        let ov = OverlayStore::new(conn);
        ov.init_schema().unwrap();
        ov
    }

    // (1) Read fall-through: fresh agent with no overlay rows sees the base blob.
    #[test]
    fn read_fall_through() {
        let (store, index) = make_base("file.txt", b"base content");
        let overlay = make_overlay();
        let agent = overlay.fork(WS).unwrap();

        let data = route_read(&store, &index, &overlay, agent, "file.txt", 0, 1024)
            .unwrap()
            .expect("base path must return data");
        assert_eq!(data, b"base content");
    }

    // (2) Write capture: agent sees its own write; sibling agent still sees base.
    #[test]
    fn write_capture() {
        let (store, index) = make_base("file.txt", b"original");
        let overlay = make_overlay();
        let agent_a = overlay.fork(WS).unwrap();
        let agent_b = overlay.fork(WS).unwrap();

        route_write(&store, &overlay, agent_a, WS, "file.txt", b"modified").unwrap();

        let a_view = route_read(&store, &index, &overlay, agent_a, "file.txt", 0, 1024)
            .unwrap()
            .expect("A must see its own write");
        assert_eq!(a_view, b"modified");

        let b_view = route_read(&store, &index, &overlay, agent_b, "file.txt", 0, 1024)
            .unwrap()
            .expect("B must still see base");
        assert_eq!(b_view, b"original", "B must not see A's write");
    }

    // (3) Delete tombstone: read returns None for deleting agent; sibling sees base.
    #[test]
    fn delete_tombstone() {
        let (store, index) = make_base("del.txt", b"bye");
        let overlay = make_overlay();
        let agent_a = overlay.fork(WS).unwrap();
        let agent_b = overlay.fork(WS).unwrap();

        route_delete(&overlay, agent_a, WS, "del.txt").unwrap();

        let result = route_read(&store, &index, &overlay, agent_a, "del.txt", 0, 1024).unwrap();
        assert!(
            result.is_none(),
            "tombstoned path must return None for deleting agent"
        );
        assert!(is_tombstoned(&overlay, agent_a, "del.txt"));

        let b_view = route_read(&store, &index, &overlay, agent_b, "del.txt", 0, 1024)
            .unwrap()
            .expect("B must still see base");
        assert_eq!(b_view, b"bye");
    }

    // (4) Cross-agent isolation: write by A is invisible to B.
    #[test]
    fn cross_agent_isolation() {
        let (store, index) = make_base("shared.rs", b"base");
        let overlay = make_overlay();
        let agent_a = overlay.fork(WS).unwrap();
        let agent_b = overlay.fork(WS).unwrap();

        route_write(&store, &overlay, agent_a, WS, "shared.rs", b"a-only").unwrap();

        let b_view = route_read(&store, &index, &overlay, agent_b, "shared.rs", 0, 1024)
            .unwrap()
            .expect("B must see base");
        assert_eq!(b_view, b"base", "B must be isolated from A's write");
    }

    // (5) Write on a tombstoned path clears the tombstone.
    #[test]
    fn write_clears_tombstone() {
        let (store, index) = make_base("t.txt", b"original");
        let overlay = make_overlay();
        let agent = overlay.fork(WS).unwrap();

        route_delete(&overlay, agent, WS, "t.txt").unwrap();
        assert!(is_tombstoned(&overlay, agent, "t.txt"));

        route_write(&store, &overlay, agent, WS, "t.txt", b"restored").unwrap();
        assert!(
            !is_tombstoned(&overlay, agent, "t.txt"),
            "tombstone must be cleared by write"
        );

        let data = route_read(&store, &index, &overlay, agent, "t.txt", 0, 1024)
            .unwrap()
            .expect("write must clear tombstone");
        assert_eq!(data, b"restored");
    }

    // (6) Absent path returns None (not an error) for all agents.
    #[test]
    fn absent_path_returns_none() {
        let (store, index) = make_base("other.txt", b"content");
        let overlay = make_overlay();
        let agent = overlay.fork(WS).unwrap();

        let result = route_read(&store, &index, &overlay, agent, "no_such.txt", 0, 1024).unwrap();
        assert!(result.is_none(), "absent path must return None");
    }

    // Concurrency smoke: N agents write and read in parallel without cross-contamination.
    // Only runs when LUNAR_SMOKE=1 is set; skipped in the default gate.
    #[test]
    fn smoke_concurrent_agents() {
        if std::env::var("LUNAR_SMOKE").as_deref() != Ok("1") {
            return;
        }

        use crate::index::Index;
        use crate::tree::{serialize_tree, TreeEntry, MODE_FILE};
        use std::sync::Arc;
        use std::thread;

        const N: usize = 8;
        const WS_SMOKE: WorkspaceId = 42;

        let store = Arc::new(MemStore::new());
        let file_hash = store.put(b"base").unwrap();
        let tree_bytes = serialize_tree(&[TreeEntry {
            mode: MODE_FILE,
            name: "shared.txt".to_string(),
            hash: file_hash,
        }]);
        let root_hash = store.put(&tree_bytes).unwrap();
        let index = Arc::new(Index::build(store.as_ref(), &root_hash).unwrap());

        let conn = Connection::open_in_memory().unwrap();
        let overlay = Arc::new(OverlayStore::new(conn));
        overlay.init_schema().unwrap();

        let agents: Vec<AgentId> = (0..N).map(|_| overlay.fork(WS_SMOKE).unwrap()).collect();

        // nyx: bounded loop (N threads, N <= 8); no unbounded growth
        let handles: Vec<_> = agents
            .iter()
            .enumerate()
            .map(|(i, &agent)| {
                let store_c = Arc::clone(&store);
                let overlay_c = Arc::clone(&overlay);
                let index_c = Arc::clone(&index);
                let payload = format!("agent-{}-data", i).into_bytes();
                thread::spawn(move || {
                    route_write(
                        store_c.as_ref(),
                        &overlay_c,
                        agent,
                        WS_SMOKE,
                        "shared.txt",
                        &payload,
                    )
                    .unwrap();
                    let view = route_read(
                        store_c.as_ref(),
                        &index_c,
                        &overlay_c,
                        agent,
                        "shared.txt",
                        0,
                        1024,
                    )
                    .unwrap()
                    .expect("each agent must read back its own write");
                    assert_eq!(view, payload, "agent {} must see its own write", i);
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread must not panic");
        }

        // Post-join: each agent's overlay must still contain only its own write.
        for (i, &agent) in agents.iter().enumerate() {
            let expected = format!("agent-{}-data", i).into_bytes();
            let got = route_read(
                store.as_ref(),
                &index,
                &overlay,
                agent,
                "shared.txt",
                0,
                1024,
            )
            .unwrap()
            .expect("post-join read must succeed");
            assert_eq!(got, expected, "agent {} isolation violated after join", i);
        }
    }
}
