use crate::workspace::{Workspace, WsId};
use anyhow::Result;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Persistence interface for Workspace records. Both impls use interior mutability.
pub trait WorkspaceStore: Send + Sync {
    /// Upsert a workspace record (insert or replace).
    fn save(&self, ws: &Workspace) -> Result<()>;
    /// Return the workspace with the given id, or None.
    fn get(&self, id: &WsId) -> Result<Option<Workspace>>;
    /// Remove a workspace record. No-op when the id is not present.
    fn remove(&self, id: &WsId) -> Result<()>;
    /// Return all stored workspace records in insertion order (or stable order).
    fn list_all(&self) -> Result<Vec<Workspace>>;
}

// --- In-memory implementation (for tests) ---

pub struct InMemoryWorkspaceStore {
    records: Mutex<HashMap<String, Workspace>>,
    order: Mutex<Vec<String>>,
}

impl InMemoryWorkspaceStore {
    pub fn new() -> Self {
        Self {
            records: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
        }
    }
}

impl Default for InMemoryWorkspaceStore {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceStore for InMemoryWorkspaceStore {
    fn save(&self, ws: &Workspace) -> Result<()> {
        assert!(!ws.id.0.is_empty(), "WsId must not be empty in save");
        let mut records = self.records.lock().expect("records lock poisoned");
        let mut order = self.order.lock().expect("order lock poisoned");
        if !records.contains_key(&ws.id.0) {
            order.push(ws.id.0.clone());
        }
        records.insert(ws.id.0.clone(), ws.clone());
        Ok(())
    }

    fn get(&self, id: &WsId) -> Result<Option<Workspace>> {
        assert!(!id.0.is_empty(), "WsId must not be empty in get");
        let records = self.records.lock().expect("records lock poisoned");
        Ok(records.get(&id.0).cloned())
    }

    fn remove(&self, id: &WsId) -> Result<()> {
        assert!(!id.0.is_empty(), "WsId must not be empty in remove");
        let mut records = self.records.lock().expect("records lock poisoned");
        let mut order = self.order.lock().expect("order lock poisoned");
        records.remove(&id.0);
        order.retain(|k| k != &id.0);
        Ok(())
    }

    fn list_all(&self) -> Result<Vec<Workspace>> {
        let records = self.records.lock().expect("records lock poisoned");
        let order = self.order.lock().expect("order lock poisoned");
        assert!(order.len() <= 1_000_000, "workspace list exceeds sanity cap");
        let result = order.iter().filter_map(|k| records.get(k).cloned()).collect();
        Ok(result)
    }
}

// --- SQLite implementation ---

/// Convert SystemTime to seconds since UNIX_EPOCH for SQLite storage.
fn to_unix(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Convert seconds-since-UNIX_EPOCH back to SystemTime.
fn from_unix(secs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH
    }
}

/// Serialize metadata BTreeMap to JSON string.
fn metadata_to_json(m: &std::collections::BTreeMap<String, String>) -> Result<String> {
    Ok(serde_json::to_string(m)?)
}

/// Deserialize metadata from JSON string.
fn metadata_from_json(s: &str) -> Result<std::collections::BTreeMap<String, String>> {
    Ok(serde_json::from_str(s)?)
}

fn row_to_workspace(row: &rusqlite::Row) -> rusqlite::Result<Workspace> {
    let id: String = row.get(0)?;
    let label: Option<String> = row.get(1)?;
    let base_ref: String = row.get(2)?;
    let ttl_secs: Option<i64> = row.get(3)?;
    let created_at_unix: i64 = row.get(4)?;
    let ephemeral: bool = row.get::<_, i64>(5).map(|v| v != 0)?;
    let metadata_json: String = row.get(6)?;

    let ttl = ttl_secs.map(|s| Duration::from_secs(s as u64));
    let created_at = from_unix(created_at_unix);
    let metadata = metadata_from_json(&metadata_json)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(
            std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
        )))?;

    Ok(Workspace { id: WsId(id), label, base_ref, ttl, created_at, ephemeral, metadata })
}

pub struct SqliteWorkspaceStore {
    conn: Mutex<Connection>,
}

impl SqliteWorkspaceStore {
    /// Open (or create) the workspace store. Applies the local_workspaces schema migration.
    pub fn open(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS local_workspaces (
                 id           TEXT    PRIMARY KEY,
                 label        TEXT,
                 base_ref     TEXT    NOT NULL,
                 ttl_secs     INTEGER,
                 created_at   INTEGER NOT NULL,
                 ephemeral    INTEGER NOT NULL DEFAULT 0,
                 metadata     TEXT    NOT NULL DEFAULT '{}'
             );",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }
}

impl WorkspaceStore for SqliteWorkspaceStore {
    fn save(&self, ws: &Workspace) -> Result<()> {
        assert!(!ws.id.0.is_empty(), "WsId must not be empty in save");
        assert!(!ws.base_ref.is_empty(), "base_ref must not be empty in save");
        let meta = metadata_to_json(&ws.metadata)?;
        let conn = self.conn.lock().expect("store conn lock poisoned");
        conn.execute(
            "INSERT INTO local_workspaces (id, label, base_ref, ttl_secs, created_at, ephemeral, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
               label      = excluded.label,
               base_ref   = excluded.base_ref,
               ttl_secs   = excluded.ttl_secs,
               created_at = excluded.created_at,
               ephemeral  = excluded.ephemeral,
               metadata   = excluded.metadata",
            params![
                ws.id.0,
                ws.label,
                ws.base_ref,
                ws.ttl.map(|d| d.as_secs() as i64),
                to_unix(ws.created_at),
                ws.ephemeral as i64,
                meta,
            ],
        )?;
        Ok(())
    }

    fn get(&self, id: &WsId) -> Result<Option<Workspace>> {
        assert!(!id.0.is_empty(), "WsId must not be empty in get");
        let conn = self.conn.lock().expect("store conn lock poisoned");
        let result: rusqlite::Result<Workspace> = conn.query_row(
            "SELECT id, label, base_ref, ttl_secs, created_at, ephemeral, metadata
             FROM local_workspaces WHERE id = ?1",
            params![id.0],
            row_to_workspace,
        );
        match result {
            Ok(ws) => Ok(Some(ws)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn remove(&self, id: &WsId) -> Result<()> {
        assert!(!id.0.is_empty(), "WsId must not be empty in remove");
        let conn = self.conn.lock().expect("store conn lock poisoned");
        conn.execute("DELETE FROM local_workspaces WHERE id = ?1", params![id.0])?;
        Ok(())
    }

    fn list_all(&self) -> Result<Vec<Workspace>> {
        let conn = self.conn.lock().expect("store conn lock poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, label, base_ref, ttl_secs, created_at, ephemeral, metadata
             FROM local_workspaces ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], row_to_workspace)?;
        let mut out = Vec::new();
        for row in rows {
            assert!(out.len() < 1_000_000, "workspace list exceeds sanity cap");
            out.push(row?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspace;
    use std::collections::BTreeMap;
    use std::time::{Duration, UNIX_EPOCH};

    fn make_ws(id: &str, ephemeral: bool) -> Workspace {
        Workspace {
            id: WsId(id.to_string()),
            label: Some("test-label".to_string()),
            metadata: BTreeMap::new(),
            base_ref: "base-hash-abc".to_string(),
            ttl: if ephemeral { Some(Duration::from_secs(3600)) } else { None },
            created_at: UNIX_EPOCH,
            ephemeral,
        }
    }

    #[test]
    fn inmemory_store_roundtrip() {
        let store = InMemoryWorkspaceStore::new();
        let ws = make_ws("ws-001", false);
        store.save(&ws).expect("save");
        let got = store.get(&WsId("ws-001".to_string())).expect("get").expect("Some");
        assert_eq!(got.id.0, "ws-001");
        assert_eq!(got.base_ref, "base-hash-abc");
    }

    #[test]
    fn inmemory_store_remove() {
        let store = InMemoryWorkspaceStore::new();
        let ws = make_ws("ws-002", false);
        store.save(&ws).expect("save");
        store.remove(&WsId("ws-002".to_string())).expect("remove");
        assert!(store.get(&WsId("ws-002".to_string())).expect("get").is_none());
    }

    #[test]
    fn inmemory_store_list() {
        let store = InMemoryWorkspaceStore::new();
        store.save(&make_ws("ws-A", false)).expect("save A");
        store.save(&make_ws("ws-B", true)).expect("save B");
        let list = store.list_all().expect("list");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id.0, "ws-A");
        assert_eq!(list[1].id.0, "ws-B");
    }

    #[test]
    fn sqlite_store_roundtrip() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        let store = SqliteWorkspaceStore::open(conn).expect("open store");
        let ws = make_ws("ws-sqlite-001", true);
        store.save(&ws).expect("save");
        let got = store.get(&WsId("ws-sqlite-001".to_string())).expect("get").expect("Some");
        assert_eq!(got.id.0, "ws-sqlite-001");
        assert!(got.ephemeral);
        assert_eq!(got.ttl, Some(Duration::from_secs(3600)));
    }

    #[test]
    fn sqlite_store_remove_and_list() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        let store = SqliteWorkspaceStore::open(conn).expect("open store");
        store.save(&make_ws("ws-1", false)).expect("save 1");
        store.save(&make_ws("ws-2", true)).expect("save 2");
        store.remove(&WsId("ws-1".to_string())).expect("remove");
        let list = store.list_all().expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id.0, "ws-2");
    }

    #[test]
    fn sqlite_store_upsert() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        let store = SqliteWorkspaceStore::open(conn).expect("open store");
        let ws = make_ws("ws-up", false);
        store.save(&ws).expect("first save");
        // Save again with updated label.
        let ws2 = Workspace { label: Some("updated".to_string()), ..ws };
        store.save(&ws2).expect("second save (upsert)");
        let got = store.get(&WsId("ws-up".to_string())).expect("get").expect("Some");
        assert_eq!(got.label.as_deref(), Some("updated"));
    }
}
