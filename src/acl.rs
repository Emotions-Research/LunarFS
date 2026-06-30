use anyhow::Result;
use rusqlite::{params, Connection};
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
    Read,
    Write,
    Deny,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AclEntry {
    pub path_prefix: String,
    pub principal: String,
    pub permission: Permission,
    /// None = never expires; Some(ts) = live while now < ts (unix seconds).
    pub expires_at: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AclDecision {
    Allow,
    Deny,
}

// Match on path boundary so "src" does not match "srcother".
fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    if path == prefix {
        return true;
    }
    // Require a slash after the prefix so /foo != /foobar.
    path.starts_with(&format!("{}/", prefix))
}

fn principal_matches(entry_principal: &str, principal: &str) -> bool {
    entry_principal == "*" || entry_principal == principal
}

fn is_live(entry: &AclEntry, now: i64) -> bool {
    match entry.expires_at {
        None => true,
        Some(exp) => now < exp,
    }
}

/// Pure, side-effect-free ACL decision. `now` is unix seconds (caller supplies it;
/// this fn never reads the system clock).
///
/// Semantics:
///   - If no live entry matches the path+principal: Allow (open by default, ACL is opt-in).
///   - Longest matching path_prefix wins.
///   - Among entries at the winning prefix: Deny beats Allow.
///   - If winning prefix entries do not grant `want`: Deny.
pub fn decide(
    entries: &[AclEntry],
    path: &str,
    principal: &str,
    now: i64,
    want: Permission,
) -> AclDecision {
    assert!(
        entries.len() <= 1_000_000,
        "entries slice exceeds safe cap of 1M"
    );
    assert!(path.len() <= 4096, "path must not exceed 4096 bytes");

    // Find the longest prefix among live, path+principal matching entries.
    let mut best_len: Option<usize> = None;
    for entry in entries {
        if !is_live(entry, now) {
            continue;
        }
        if !path_matches_prefix(path, &entry.path_prefix) {
            continue;
        }
        if !principal_matches(&entry.principal, principal) {
            continue;
        }
        let len = entry.path_prefix.len();
        if best_len.is_none_or(|b| len > b) {
            best_len = Some(len);
        }
    }

    // No live entry matches this path: open by default.
    let best_len = match best_len {
        None => return AclDecision::Allow,
        Some(l) => l,
    };

    // Among entries at the best prefix: Deny beats Allow; no Allow => Deny.
    let mut has_allow = false;
    for entry in entries {
        if !is_live(entry, now) {
            continue;
        }
        if !path_matches_prefix(path, &entry.path_prefix) {
            continue;
        }
        if !principal_matches(&entry.principal, principal) {
            continue;
        }
        if entry.path_prefix.len() != best_len {
            continue;
        }
        match entry.permission {
            Permission::Deny => return AclDecision::Deny,
            Permission::Write => has_allow = true, // Write grants Read
            Permission::Read if want == Permission::Read => has_allow = true,
            Permission::Read => {} // Read does not grant Write
        }
    }

    if has_allow {
        AclDecision::Allow
    } else {
        AclDecision::Deny
    }
}

pub struct AclStore {
    conn: Mutex<Connection>,
}

impl AclStore {
    pub fn new(conn: Connection) -> Self {
        Self {
            conn: Mutex::new(conn),
        }
    }

    /// Create the acl table if absent. Idempotent; safe on every startup.
    pub fn init_schema(&self) -> Result<()> {
        let conn = self.conn.lock().expect("acl conn lock poisoned");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS acl (
                 path_prefix TEXT NOT NULL,
                 principal   TEXT NOT NULL,
                 permission  TEXT NOT NULL,
                 expires_at  INTEGER
             );",
        )?;
        let exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='acl'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(exists, 1, "acl table must exist after init_schema");
        Ok(())
    }

    pub fn insert(&self, entry: &AclEntry) -> Result<()> {
        assert!(
            entry.path_prefix.len() <= 4096,
            "path_prefix must not exceed 4096 bytes"
        );
        let perm_str = permission_to_str(entry.permission);
        let conn = self.conn.lock().expect("acl conn lock poisoned");
        conn.execute(
            "INSERT INTO acl (path_prefix, principal, permission, expires_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                entry.path_prefix,
                entry.principal,
                perm_str,
                entry.expires_at
            ],
        )?;
        Ok(())
    }

    pub fn load_all(&self) -> Result<Vec<AclEntry>> {
        let conn = self.conn.lock().expect("acl conn lock poisoned");
        let mut stmt =
            conn.prepare("SELECT path_prefix, principal, permission, expires_at FROM acl")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        })?;
        let mut entries = Vec::new();
        for row in rows {
            let (path_prefix, principal, perm_str, expires_at) = row?;
            let permission = str_to_permission(&perm_str);
            entries.push(AclEntry {
                path_prefix,
                principal,
                permission,
                expires_at,
            });
        }
        Ok(entries)
    }
}

fn permission_to_str(p: Permission) -> &'static str {
    match p {
        Permission::Read => "read",
        Permission::Write => "write",
        Permission::Deny => "deny",
    }
}

fn str_to_permission(s: &str) -> Permission {
    match s {
        "write" => Permission::Write,
        "deny" => Permission::Deny,
        _ => Permission::Read,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn entry(prefix: &str, principal: &str, perm: Permission, exp: Option<i64>) -> AclEntry {
        AclEntry {
            path_prefix: prefix.to_string(),
            principal: principal.to_string(),
            permission: perm,
            expires_at: exp,
        }
    }

    // Empty ACL: open by default.
    #[test]
    fn empty_acl_is_open() {
        assert_eq!(
            decide(&[], "any/path", "user1", 1000, Permission::Read),
            AclDecision::Allow
        );
    }

    // Live Allow entry grants access.
    #[test]
    fn live_allow_grants() {
        let entries = vec![entry("docs", "*", Permission::Read, None)];
        assert_eq!(
            decide(&entries, "docs/readme.md", "*", 0, Permission::Read),
            AclDecision::Allow
        );
    }

    // Explicit Deny entry blocks access.
    #[test]
    fn explicit_deny_blocks() {
        let entries = vec![entry("secret", "*", Permission::Deny, None)];
        assert_eq!(
            decide(&entries, "secret/key.pem", "*", 0, Permission::Read),
            AclDecision::Deny
        );
    }

    // Expired Deny entry: no live entry => open by default.
    #[test]
    fn expired_deny_reverts_to_open_default() {
        let entries = vec![entry("docs", "*", Permission::Deny, Some(100))];
        // now=99: entry live => Deny
        assert_eq!(
            decide(&entries, "docs/readme.md", "*", 99, Permission::Read),
            AclDecision::Deny
        );
        // now=101: entry expired, no live match => Allow (open by default)
        assert_eq!(
            decide(&entries, "docs/readme.md", "*", 101, Permission::Read),
            AclDecision::Allow
        );
    }

    // Expiry boundary: live while now < expires_at; dead at now == expires_at.
    #[test]
    fn expiry_boundary() {
        let exp = 500i64;
        let entries = vec![entry("gated", "*", Permission::Deny, Some(exp))];

        // now = 499: live, Deny
        assert_eq!(
            decide(&entries, "gated/file.txt", "*", 499, Permission::Read),
            AclDecision::Deny
        );
        // now = 500: expired (not now < 500), open default => Allow
        assert_eq!(
            decide(&entries, "gated/file.txt", "*", 500, Permission::Read),
            AclDecision::Allow
        );
        // now = 501: also expired
        assert_eq!(
            decide(&entries, "gated/file.txt", "*", 501, Permission::Read),
            AclDecision::Allow
        );
    }

    // Wildcard principal matches any caller.
    #[test]
    fn wildcard_principal_matches_any() {
        let entries = vec![entry("pub", "*", Permission::Read, None)];
        assert_eq!(
            decide(&entries, "pub/file.txt", "user-42", 0, Permission::Read),
            AclDecision::Allow
        );
        assert_eq!(
            decide(&entries, "pub/file.txt", "bot-99", 0, Permission::Read),
            AclDecision::Allow
        );
    }

    // Specific principal; caller with no matching entry sees open default.
    #[test]
    fn specific_principal_does_not_gate_others() {
        let entries = vec![entry("private", "alice", Permission::Deny, None)];
        assert_eq!(
            decide(&entries, "private/data.txt", "alice", 0, Permission::Read),
            AclDecision::Deny
        );
        // bob has no matching entry => Allow
        assert_eq!(
            decide(&entries, "private/data.txt", "bob", 0, Permission::Read),
            AclDecision::Allow
        );
    }

    // Longest prefix wins over shorter overlapping ones.
    #[test]
    fn longest_prefix_wins() {
        let entries = vec![
            entry("docs", "*", Permission::Deny, None),        // len=4
            entry("docs/public", "*", Permission::Read, None), // len=11 (longer)
        ];
        // Longer prefix (docs/public) grants Read, overrides the shorter Deny.
        assert_eq!(
            decide(&entries, "docs/public/readme.md", "*", 0, Permission::Read),
            AclDecision::Allow
        );
        // Under docs/ but not docs/public/ => shorter Deny applies.
        assert_eq!(
            decide(
                &entries,
                "docs/internal/design.md",
                "*",
                0,
                Permission::Read
            ),
            AclDecision::Deny
        );
    }

    // At the same prefix level: Deny beats Allow.
    #[test]
    fn deny_beats_allow_at_same_prefix() {
        let entries = vec![
            entry("shared", "*", Permission::Read, None),
            entry("shared", "evil", Permission::Deny, None),
        ];
        assert_eq!(
            decide(&entries, "shared/file.txt", "evil", 0, Permission::Read),
            AclDecision::Deny
        );
        assert_eq!(
            decide(&entries, "shared/file.txt", "good", 0, Permission::Read),
            AclDecision::Allow
        );
    }

    // Path boundary: "src" prefix must not match "srcother".
    #[test]
    fn prefix_boundary_no_false_match() {
        let entries = vec![entry("src", "*", Permission::Deny, None)];
        assert_eq!(
            decide(&entries, "src/main.rs", "*", 0, Permission::Read),
            AclDecision::Deny
        );
        // No slash boundary: "srcother" does not match prefix "src".
        assert_eq!(
            decide(&entries, "srcother/file.txt", "*", 0, Permission::Read),
            AclDecision::Allow
        );
        // Exact match.
        assert_eq!(
            decide(&entries, "src", "*", 0, Permission::Read),
            AclDecision::Deny
        );
    }

    // Write permission grants Read access (Write implies Read).
    #[test]
    fn write_permission_grants_read() {
        let entries = vec![entry("writable", "editor", Permission::Write, None)];
        assert_eq!(
            decide(&entries, "writable/file.txt", "editor", 0, Permission::Read),
            AclDecision::Allow
        );
    }

    // AclStore: init_schema idempotent, insert + load_all round-trip.
    #[test]
    fn acl_store_roundtrip() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        let store = AclStore::new(conn);
        store.init_schema().expect("first init_schema");
        store.init_schema().expect("second init_schema idempotent");

        let e1 = entry("docs", "*", Permission::Read, None);
        let e2 = entry("secret", "admin", Permission::Deny, Some(9999));
        store.insert(&e1).expect("insert e1");
        store.insert(&e2).expect("insert e2");

        let loaded = store.load_all().expect("load_all");
        assert_eq!(loaded.len(), 2);

        let doc_entry = loaded
            .iter()
            .find(|e| e.path_prefix == "docs")
            .expect("docs entry");
        assert_eq!(doc_entry.permission, Permission::Read);
        assert_eq!(doc_entry.expires_at, None);

        let sec_entry = loaded
            .iter()
            .find(|e| e.path_prefix == "secret")
            .expect("secret entry");
        assert_eq!(sec_entry.permission, Permission::Deny);
        assert_eq!(sec_entry.expires_at, Some(9999));
    }

    // Timed reveal: Deny entry expires, path reverts to open default.
    #[test]
    fn timed_reveal_via_expires_at() {
        let exp = 1000i64;
        let entries = vec![entry("staged", "*", Permission::Deny, Some(exp))];
        // Before expiry: denied.
        assert_eq!(
            decide(&entries, "staged/release.bin", "*", 999, Permission::Read),
            AclDecision::Deny
        );
        // At expiry: entry dead, open default takes over.
        assert_eq!(
            decide(&entries, "staged/release.bin", "*", 1000, Permission::Read),
            AclDecision::Allow
        );
    }
}
