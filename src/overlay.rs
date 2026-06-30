use anyhow::Result;
use rusqlite::{params, Connection};
use std::sync::Mutex;

pub type WorkspaceId = i64;
pub type AgentId = i64;

/// One overlay record for (agent_id, path).
/// blob_hash == None means a tombstone (the agent deleted this path).
pub struct OverlayEntry {
    pub workspace_id: WorkspaceId,
    pub agent_id: AgentId,
    pub path: String,
    pub blob_hash: Option<String>,
}

/// Result of resolving one path for one agent.
pub enum Resolution {
    /// Agent overlay has this path; read the CAS blob with this hash.
    Overlay(String),
    /// No overlay entry; fall through to the CAS base layer.
    Base,
    /// Agent overlay has a tombstone; path is deleted for this agent (FUSE returns ENOENT).
    Tombstone,
}

pub struct OverlayStore {
    conn: Mutex<Connection>,
}

impl OverlayStore {
    pub fn new(conn: Connection) -> Self {
        Self {
            conn: Mutex::new(conn),
        }
    }

    /// Create the agents, agent_overlay, and workspace_roots tables if absent.
    /// Idempotent; safe on every startup.
    pub fn init_schema(&self) -> Result<()> {
        let conn = self.conn.lock().expect("overlay conn lock poisoned");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS agents (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 workspace_id INTEGER NOT NULL,
                 created_at   INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS agent_overlay (
                 workspace_id INTEGER NOT NULL,
                 agent_id     INTEGER NOT NULL,
                 path         TEXT    NOT NULL,
                 blob_hash    TEXT,
                 PRIMARY KEY (agent_id, path)
             );
             CREATE INDEX IF NOT EXISTS idx_agent_overlay_agent
                 ON agent_overlay(agent_id);
             CREATE TABLE IF NOT EXISTS workspace_roots (
                 workspace_id INTEGER PRIMARY KEY,
                 root_hash    TEXT    NOT NULL,
                 created_at   INTEGER NOT NULL
             );",
        )?;
        let agents_exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='agents'",
            [],
            |r| r.get(0),
        )?;
        let overlay_exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='agent_overlay'",
            [],
            |r| r.get(0),
        )?;
        let roots_exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='workspace_roots'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(
            agents_exists, 1,
            "agents table must exist after init_schema"
        );
        assert_eq!(
            overlay_exists, 1,
            "agent_overlay table must exist after init_schema"
        );
        assert_eq!(
            roots_exists, 1,
            "workspace_roots table must exist after init_schema"
        );
        Ok(())
    }

    /// Register the root tree hash for `ws_id`. Called once when creating or forking a workspace.
    /// Errors with a unique-constraint violation if `ws_id` already has a registered root.
    pub fn create_workspace_root(&self, ws_id: WorkspaceId, root_hash: &str) -> Result<()> {
        assert!(ws_id > 0, "workspace_id must be a positive rowid");
        assert!(!root_hash.is_empty(), "root_hash must not be empty");
        let conn = self.conn.lock().expect("overlay conn lock poisoned");
        conn.execute(
            "INSERT INTO workspace_roots (workspace_id, root_hash, created_at)
             VALUES (?1, ?2, unixepoch())",
            params![ws_id, root_hash],
        )?;
        Ok(())
    }

    /// Return the registered root tree hash for `ws_id`, or None if no root is registered.
    pub fn workspace_root(&self, ws_id: WorkspaceId) -> Result<Option<String>> {
        assert!(ws_id > 0, "workspace_id must be a positive rowid");
        let conn = self.conn.lock().expect("overlay conn lock poisoned");
        let result: rusqlite::Result<String> = conn.query_row(
            "SELECT root_hash FROM workspace_roots WHERE workspace_id = ?1",
            params![ws_id],
            |r| r.get(0),
        );
        match result {
            Ok(hash) => Ok(Some(hash)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Count agents allocated for `ws_id`. Used in tests to verify fork isolation.
    #[cfg(test)]
    pub fn agent_count_for_workspace(&self, ws_id: WorkspaceId) -> Result<i64> {
        assert!(ws_id > 0, "workspace_id must be a positive rowid");
        let conn = self.conn.lock().expect("overlay conn lock poisoned");
        let count = conn.query_row(
            "SELECT COUNT(*) FROM agents WHERE workspace_id = ?1",
            params![ws_id],
            |r| r.get(0),
        )?;
        Ok(count)
    }

    /// Create a fresh isolated agent in `workspace` via a SINGLE INSERT.
    ///
    /// Copies no file data. The new agent starts with an empty overlay and sees the entire
    /// CAS base layer via fall-through in resolve(). Sub-millisecond: isolation comes from
    /// overlay resolution, not data duplication.
    pub fn fork(&self, workspace: WorkspaceId) -> Result<AgentId> {
        assert!(workspace > 0, "workspace_id must be a positive rowid");
        let conn = self.conn.lock().expect("overlay conn lock poisoned");
        conn.execute(
            "INSERT INTO agents (workspace_id, created_at) VALUES (?1, unixepoch())",
            params![workspace],
        )?;
        let id = conn.last_insert_rowid();
        assert!(id > 0, "fork must return a positive AgentId rowid");
        Ok(id)
    }

    /// Resolve `path` for `agent`: Overlay(hash) > Tombstone > Base.
    ///
    /// Returns Base for any agent_id that has no overlay rows (including unknown agents).
    pub fn resolve(&self, agent: AgentId, path: &str) -> Result<Resolution> {
        assert!(agent >= 0, "agent_id must be non-negative");
        assert!(path.len() <= 4096, "path length must not exceed 4096 bytes");
        let conn = self.conn.lock().expect("overlay conn lock poisoned");
        let result: rusqlite::Result<Option<String>> = conn.query_row(
            "SELECT blob_hash FROM agent_overlay WHERE agent_id = ?1 AND path = ?2",
            params![agent, path],
            |row| row.get(0),
        );
        match result {
            Ok(Some(hash)) => Ok(Resolution::Overlay(hash)),
            Ok(None) => Ok(Resolution::Tombstone),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(Resolution::Base),
            Err(e) => Err(e.into()),
        }
    }

    /// Upsert (agent, path) -> blob_hash into the overlay, clearing any prior tombstone.
    pub fn capture_write(
        &self,
        agent: AgentId,
        workspace: WorkspaceId,
        path: &str,
        blob_hash: &str,
    ) -> Result<()> {
        assert!(agent > 0, "agent_id must be a positive rowid");
        assert!(
            !blob_hash.is_empty(),
            "blob_hash must not be empty for a write capture"
        );
        let conn = self.conn.lock().expect("overlay conn lock poisoned");
        conn.execute(
            "INSERT INTO agent_overlay (workspace_id, agent_id, path, blob_hash)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(agent_id, path) DO UPDATE SET
               blob_hash    = excluded.blob_hash,
               workspace_id = excluded.workspace_id",
            params![workspace, agent, path, blob_hash],
        )?;
        Ok(())
    }

    /// Upsert (agent, path) as a tombstone (NULL blob_hash), recording a delete in the overlay.
    pub fn capture_delete(&self, agent: AgentId, workspace: WorkspaceId, path: &str) -> Result<()> {
        assert!(agent > 0, "agent_id must be a positive rowid");
        assert!(workspace > 0, "workspace_id must be a positive rowid");
        let conn = self.conn.lock().expect("overlay conn lock poisoned");
        conn.execute(
            "INSERT INTO agent_overlay (workspace_id, agent_id, path, blob_hash)
             VALUES (?1, ?2, ?3, NULL)
             ON CONFLICT(agent_id, path) DO UPDATE SET
               blob_hash    = NULL,
               workspace_id = excluded.workspace_id",
            params![workspace, agent, path],
        )?;
        Ok(())
    }

    // nyx: hard cap; upgrade path: paginated streaming for very large overlays
    const MAX_OVERLAY_ENTRIES: usize = 1_000_000;

    /// All overlay entries for `agent`, ordered by path (deterministic).
    pub fn entries_for_agent(&self, agent: AgentId) -> Result<Vec<OverlayEntry>> {
        assert!(agent >= 0, "agent_id must be non-negative");
        let conn = self.conn.lock().expect("overlay conn lock poisoned");
        let mut stmt = conn.prepare(
            "SELECT workspace_id, agent_id, path, blob_hash
             FROM agent_overlay
             WHERE agent_id = ?1
             ORDER BY path",
        )?;
        let rows = stmt.query_map(params![agent], |row| {
            Ok(OverlayEntry {
                workspace_id: row.get(0)?,
                agent_id: row.get(1)?,
                path: row.get(2)?,
                blob_hash: row.get(3)?,
            })
        })?;
        let mut entries = Vec::new();
        for row in rows {
            assert!(
                entries.len() < Self::MAX_OVERLAY_ENTRIES,
                "overlay entries for agent {} exceed cap of {}",
                agent,
                Self::MAX_OVERLAY_ENTRIES
            );
            entries.push(row?);
        }
        Ok(entries)
    }
}

/// Result of a successful `fork_workspace` call.
#[derive(Debug)]
pub struct WorkspaceHandle {
    pub workspace_id: WorkspaceId,
    pub agent_id: AgentId,
    pub root_hash: String,
}

/// O(1) workspace fork: ACL-gated, allocates no blobs.
///
/// Authorization uses the Epic 2 deny-by-default model: `principal` must hold an
/// active Write (or Read) grant on `base_ws_id` in the auth DB (`conn`). If denied,
/// the function returns an error and allocates nothing (no workspace_roots row, no
/// agent row, no ACL grant).
///
/// `new_ws_id` must already exist in the auth DB (created by the caller via
/// `repo::create_workspace`) so that the ACL grant written here references a valid
/// workspace rowid.
pub fn fork_workspace(
    overlay: &OverlayStore,
    conn: &rusqlite::Connection,
    base_ws_id: WorkspaceId,
    new_ws_id: WorkspaceId,
    principal: &crate::auth::acl::Principal,
    now: i64,
) -> Result<WorkspaceHandle> {
    use crate::auth::acl::{authorize, grant, Decision, Permission};
    assert!(base_ws_id > 0, "base_ws_id must be a positive rowid");
    assert!(new_ws_id > 0, "new_ws_id must be a positive rowid");
    assert!(
        base_ws_id != new_ws_id,
        "base_ws_id and new_ws_id must differ"
    );
    assert!(now >= 0, "now must be a non-negative unix timestamp");

    // Step 1: ACL -- principal must be authorized to read the base workspace.
    let decision = authorize(conn, principal, base_ws_id, "/", Permission::Read)?;
    if decision != Decision::Allow {
        return Err(anyhow::anyhow!(
            "forbidden: principal denied read on workspace {}",
            base_ws_id
        ));
    }

    // Step 2: Resolve base root hash (one SELECT, no blob iteration).
    let base_root = overlay
        .workspace_root(base_ws_id)?
        .ok_or_else(|| anyhow::anyhow!("workspace {} has no registered root", base_ws_id))?;

    // Step 3: Alias the same root hash to the new workspace (one INSERT, zero blob copies).
    overlay.create_workspace_root(new_ws_id, &base_root)?;

    // Step 4: Allocate a fresh overlay namespace for the new workspace (one INSERT).
    let agent_id = overlay.fork(new_ws_id)?;

    // Step 5: Grant the principal Write access to the new workspace.
    grant(
        conn,
        principal.kind,
        &principal.id,
        new_ws_id,
        "/",
        Permission::Write,
        now,
    )?;

    Ok(WorkspaceHandle {
        workspace_id: new_ws_id,
        agent_id,
        root_hash: base_root,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn make_store() -> OverlayStore {
        let conn = Connection::open_in_memory().expect("in-memory db must open");
        let store = OverlayStore::new(conn);
        store
            .init_schema()
            .expect("init_schema must succeed on in-memory db");
        store
    }

    // Fake workspace rowid; no workspaces table exists in this subtask's schema.
    const WS: WorkspaceId = 1;

    // (a) A freshly forked agent with no overlay rows resolves any path to Base.
    #[test]
    fn read_fall_through() {
        let store = make_store();
        let agent = store.fork(WS).expect("fork must succeed");
        let res = store
            .resolve(agent, "src/main.rs")
            .expect("resolve must not error");
        assert!(
            matches!(res, Resolution::Base),
            "fresh agent must fall through to Base"
        );

        // Also verify an empty path (opaque string) resolves to Base.
        let res2 = store
            .resolve(agent, "")
            .expect("empty path must resolve without panic");
        assert!(
            matches!(res2, Resolution::Base),
            "empty path on fresh agent must be Base"
        );
    }

    // (b) After capture_write, resolve returns Overlay(hash); re-capture overwrites to new hash.
    #[test]
    fn write_capture() {
        let store = make_store();
        let agent = store.fork(WS).expect("fork");
        store
            .capture_write(agent, WS, "src/lib.rs", "hashA")
            .expect("first capture_write");

        let res = store
            .resolve(agent, "src/lib.rs")
            .expect("resolve after write");
        match res {
            Resolution::Overlay(h) => assert_eq!(h, "hashA", "must return first hash"),
            other => panic!(
                "expected Overlay(hashA), got unexpected variant: {:?}",
                matches!(other, Resolution::Base)
            ),
        }

        // Overwrite with a new hash.
        store
            .capture_write(agent, WS, "src/lib.rs", "hashB")
            .expect("second capture_write");
        let res2 = store
            .resolve(agent, "src/lib.rs")
            .expect("resolve after overwrite");
        match res2 {
            Resolution::Overlay(h) => assert_eq!(h, "hashB", "must return overwritten hash"),
            _ => panic!("expected Overlay(hashB) after overwrite"),
        }
    }

    // (c) After capture_delete, resolve returns Tombstone. A later capture_write on the same
    //     path clears the tombstone back to Overlay.
    #[test]
    fn delete_tombstone_and_restore() {
        let store = make_store();
        let agent = store.fork(WS).expect("fork");

        store
            .capture_delete(agent, WS, "docs/README.md")
            .expect("capture_delete");
        let res = store
            .resolve(agent, "docs/README.md")
            .expect("resolve after delete");
        assert!(
            matches!(res, Resolution::Tombstone),
            "must return Tombstone after delete"
        );

        // Write on a tombstoned path must clear the tombstone.
        store
            .capture_write(agent, WS, "docs/README.md", "hashC")
            .expect("capture_write after tombstone");
        let res2 = store
            .resolve(agent, "docs/README.md")
            .expect("resolve after restore");
        match res2 {
            Resolution::Overlay(h) => assert_eq!(h, "hashC", "tombstone must be cleared by write"),
            _ => panic!("expected Overlay(hashC) after write over tombstone"),
        }
    }

    // (d) Isolation: two agents in the same workspace do not see each other's overlay.
    #[test]
    fn agent_isolation() {
        let store = make_store();
        let agent_a = store.fork(WS).expect("fork A");
        let agent_b = store.fork(WS).expect("fork B");
        assert_ne!(agent_a, agent_b, "fork must return distinct AgentIds");

        // Write on A must not be visible on B.
        store
            .capture_write(agent_a, WS, "shared/config.toml", "hashX")
            .expect("write A");
        let res_b = store
            .resolve(agent_b, "shared/config.toml")
            .expect("resolve B after A write");
        assert!(
            matches!(res_b, Resolution::Base),
            "write on A must not appear in B"
        );

        // Delete on A must not tombstone B.
        store
            .capture_delete(agent_a, WS, "another/file.rs")
            .expect("delete A");
        let res_b2 = store
            .resolve(agent_b, "another/file.rs")
            .expect("resolve B after A delete");
        assert!(
            matches!(res_b2, Resolution::Base),
            "delete on A must not tombstone B"
        );

        // B's view of A's written path is still Base.
        let res_b3 = store
            .resolve(agent_b, "shared/config.toml")
            .expect("resolve B unchanged");
        assert!(
            matches!(res_b3, Resolution::Base),
            "B must still see Base for A's write path"
        );
    }

    // (e) fork returns distinct, strictly increasing AgentIds.
    #[test]
    fn fork_returns_distinct_increasing_ids() {
        let store = make_store();
        let id1 = store.fork(WS).expect("fork 1");
        let id2 = store.fork(WS).expect("fork 2");
        let id3 = store.fork(WS).expect("fork 3");
        assert!(id2 > id1, "second fork must have a larger id than first");
        assert!(id3 > id2, "third fork must have a larger id than second");
        assert!(
            id1 > 0 && id2 > 0 && id3 > 0,
            "all fork ids must be positive"
        );
    }

    // init_schema is idempotent (calling it twice must not error).
    #[test]
    fn init_schema_idempotent() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        let store = OverlayStore::new(conn);
        store.init_schema().expect("first init_schema must succeed");
        store
            .init_schema()
            .expect("second init_schema must also succeed (idempotent)");
    }

    // resolve on a completely unknown agent_id returns Base (no panic).
    #[test]
    fn resolve_unknown_agent_returns_base() {
        let store = make_store();
        let res = store
            .resolve(9999, "any/path")
            .expect("resolve on unknown agent must not panic");
        assert!(
            matches!(res, Resolution::Base),
            "unknown agent must return Base"
        );
    }

    // entries_for_agent returns an empty vec for an agent with no overlay rows.
    #[test]
    fn entries_for_agent_empty() {
        let store = make_store();
        let agent = store.fork(WS).expect("fork");
        let entries = store
            .entries_for_agent(agent)
            .expect("entries_for_agent must not error");
        assert!(
            entries.is_empty(),
            "fresh agent must have zero overlay entries"
        );
    }

    // entries_for_agent returns entries sorted by path, deterministically.
    #[test]
    fn entries_for_agent_ordered_by_path() {
        let store = make_store();
        let agent = store.fork(WS).expect("fork");

        store
            .capture_write(agent, WS, "z/last.rs", "h3")
            .expect("write z");
        store
            .capture_write(agent, WS, "a/first.rs", "h1")
            .expect("write a");
        store
            .capture_delete(agent, WS, "m/middle.rs")
            .expect("delete m");

        let entries = store.entries_for_agent(agent).expect("entries_for_agent");
        assert_eq!(entries.len(), 3, "must have 3 overlay entries");
        assert_eq!(entries[0].path, "a/first.rs");
        assert_eq!(entries[1].path, "m/middle.rs");
        assert!(entries[1].blob_hash.is_none(), "middle must be a tombstone");
        assert_eq!(entries[2].path, "z/last.rs");
        assert_eq!(entries[2].blob_hash.as_deref(), Some("h3"));
    }

    // Nested paths are treated as opaque strings (no special path normalization).
    #[test]
    fn nested_paths_opaque() {
        let store = make_store();
        let agent = store.fork(WS).expect("fork");
        store
            .capture_write(agent, WS, "a/b/c/d.txt", "h")
            .expect("nested write");
        let res = store.resolve(agent, "a/b/c/d.txt").expect("resolve nested");
        assert!(
            matches!(res, Resolution::Overlay(_)),
            "nested path must resolve to Overlay"
        );
        // Parent paths are distinct from child paths.
        let res_parent = store.resolve(agent, "a/b/c").expect("resolve parent");
        assert!(
            matches!(res_parent, Resolution::Base),
            "parent must be Base unless explicitly written"
        );
    }

    // workspace_root returns None for an unknown workspace_id.
    #[test]
    fn workspace_root_absent_returns_none() {
        let store = make_store();
        let root = store
            .workspace_root(9999)
            .expect("workspace_root must not error for absent id");
        assert!(root.is_none(), "absent workspace_id must return None");
    }

    // create_workspace_root and workspace_root round-trip.
    #[test]
    fn workspace_root_roundtrip() {
        let store = make_store();
        store
            .create_workspace_root(WS, "abc123")
            .expect("create_workspace_root must succeed");
        let got = store
            .workspace_root(WS)
            .expect("workspace_root")
            .expect("must be Some");
        assert_eq!(got, "abc123", "workspace_root must return what was stored");
    }

    // Duplicate registration on the same workspace_id must fail.
    #[test]
    fn workspace_root_duplicate_errors() {
        let store = make_store();
        store
            .create_workspace_root(WS, "hash1")
            .expect("first registration must succeed");
        let err = store.create_workspace_root(WS, "hash2");
        assert!(
            err.is_err(),
            "second registration on same workspace_id must fail"
        );
    }
}

#[cfg(test)]
mod fork_tests {
    use super::*;
    use crate::auth::{self, acl as auth_acl, repo, OwnerKind};
    use rusqlite::Connection;
    use tempfile::tempdir;

    fn make_overlay() -> OverlayStore {
        let conn = Connection::open_in_memory().expect("in-memory overlay db");
        let store = OverlayStore::new(conn);
        store.init_schema().expect("init_schema");
        store
    }

    // Covers invariants (a)-(e) from the spec: fork is O(1), shares base root, CoW isolation,
    // and unauthorized actors are rejected with no allocation.
    #[test]
    fn fork_workspace_cow_invariants() {
        let dir = tempdir().expect("tempdir for auth db");
        let conn = auth::open(&dir.path().join("auth.db")).expect("auth::open");
        let overlay = make_overlay();

        // (a) Set up base workspace with a known root hash.
        let uid = repo::create_user(&conn, None, 1).expect("create_user");
        let base_ws_id = repo::create_workspace(&conn, "base", OwnerKind::User, uid, 1)
            .expect("create base workspace");
        let base_root = "aabb000000000000000000000000000000000000000000000000000000000000";
        overlay
            .create_workspace_root(base_ws_id, base_root)
            .expect("register base root");

        // Grant the authorized actor Write on the base workspace (Write implies Read in authorize).
        let principal = auth_acl::Principal {
            kind: auth_acl::PrincipalKind::User,
            id: uid.to_string(),
        };
        auth_acl::grant(
            &conn,
            principal.kind,
            &principal.id,
            base_ws_id,
            "/",
            auth_acl::Permission::Write,
            1,
        )
        .expect("grant actor access to base workspace");

        // (b) Fork as the authorized actor.
        let fork_ws_id = repo::create_workspace(&conn, "fork", OwnerKind::User, uid, 2)
            .expect("create fork workspace");
        let handle = fork_workspace(&overlay, &conn, base_ws_id, fork_ws_id, &principal, 2)
            .expect("fork must succeed for authorized actor");

        // Assert: fork root == base root (root pointer is shared, not copied).
        assert_eq!(
            handle.root_hash, base_root,
            "fork root must equal base root"
        );
        assert_eq!(
            handle.workspace_id, fork_ws_id,
            "handle workspace_id must match fork_ws_id"
        );
        assert!(handle.agent_id > 0, "agent_id must be a positive rowid");

        // Assert: zero overlay entries allocated by fork itself (no blobs, no writes).
        let overlay_entries = overlay
            .entries_for_agent(handle.agent_id)
            .expect("entries_for_agent");
        assert!(
            overlay_entries.is_empty(),
            "fork must allocate zero overlay entries"
        );

        // Assert: fork is a distinct workspace from base.
        assert_ne!(
            handle.workspace_id, base_ws_id,
            "fork must have distinct workspace_id"
        );

        // (c) Read from fork resolves to Base (falls through to shared CAS).
        let res = overlay
            .resolve(handle.agent_id, "known/file.txt")
            .expect("resolve");
        assert!(
            matches!(res, Resolution::Base),
            "read from fork must fall through to Base"
        );

        // (d) Write a new file to the fork: exactly one overlay entry appears in fork namespace.
        overlay
            .capture_write(
                handle.agent_id,
                fork_ws_id,
                "new/file.txt",
                "newhash0000000000000",
            )
            .expect("capture_write on fork");

        let fork_entries = overlay
            .entries_for_agent(handle.agent_id)
            .expect("entries after fork write");
        assert_eq!(
            fork_entries.len(),
            1,
            "fork must have exactly one overlay entry after write"
        );

        // Assert base root is unchanged after the fork write.
        let base_root_now = overlay
            .workspace_root(base_ws_id)
            .expect("base workspace_root query")
            .expect("some");
        assert_eq!(
            base_root_now, base_root,
            "base root must be unchanged after fork write"
        );

        // Assert fork workspace_root still aliases the original base root.
        let fork_root = overlay
            .workspace_root(fork_ws_id)
            .expect("fork workspace_root query")
            .expect("some");
        assert_eq!(
            fork_root, base_root,
            "fork workspace_root must still alias base root"
        );

        // (e) Unauthorized fork: a user with no ACL grant on base_ws_id must be refused.
        let other_uid = repo::create_user(&conn, None, 3).expect("create other user");
        let other_principal = auth_acl::Principal {
            kind: auth_acl::PrincipalKind::User,
            id: other_uid.to_string(),
        };
        let rejected_ws_id =
            repo::create_workspace(&conn, "rejected", OwnerKind::User, other_uid, 3)
                .expect("create rejected workspace");

        let err = fork_workspace(
            &overlay,
            &conn,
            base_ws_id,
            rejected_ws_id,
            &other_principal,
            3,
        )
        .expect_err("unauthorized fork must fail");
        assert!(
            err.to_string().contains("forbidden"),
            "error must mention forbidden, got: {}",
            err
        );

        // Assert: no workspace root allocated for the rejected fork.
        let no_root = overlay
            .workspace_root(rejected_ws_id)
            .expect("workspace_root for rejected");
        assert!(
            no_root.is_none(),
            "rejected fork must not register a workspace root"
        );

        // Assert: no overlay agent allocated for the rejected fork.
        let agent_count = overlay
            .agent_count_for_workspace(rejected_ws_id)
            .expect("agent count for rejected");
        assert_eq!(
            agent_count, 0,
            "rejected fork must not allocate an agent namespace"
        );
    }
}
