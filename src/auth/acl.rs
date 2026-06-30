use anyhow::Result;
use rusqlite::{params, Connection};

use super::repo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrincipalKind {
    User,
    Org,
    Token,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub kind: PrincipalKind,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub id: i64,
    pub principal_kind: PrincipalKind,
    pub principal_id: String,
    pub workspace_id: i64,
    pub path_prefix: String,
    pub permission: Permission,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
}

fn principal_kind_to_str(k: PrincipalKind) -> &'static str {
    match k {
        PrincipalKind::User => "user",
        PrincipalKind::Org => "org",
        PrincipalKind::Token => "token",
    }
}

fn str_to_principal_kind(s: &str) -> Option<PrincipalKind> {
    match s {
        "user" => Some(PrincipalKind::User),
        "org" => Some(PrincipalKind::Org),
        "token" => Some(PrincipalKind::Token),
        _ => None,
    }
}

fn permission_to_str(p: Permission) -> &'static str {
    match p {
        Permission::Read => "read",
        Permission::Write => "write",
    }
}

fn str_to_permission(s: &str) -> Option<Permission> {
    match s {
        "read" => Some(Permission::Read),
        "write" => Some(Permission::Write),
        _ => None,
    }
}

// Mirrors the overlay acl.rs boundary check: exact match OR path starts with
// "{prefix}/" so that /a does not match /ab.
// Trailing slashes on the stored prefix are stripped first so that "/a/" and
// "/a" both match "/a/b" and both avoid matching "/ab".
fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return true;
    }
    if path == prefix {
        return true;
    }
    path.len() > prefix.len() && path.starts_with(prefix) && path.as_bytes()[prefix.len()] == b'/'
}

// Write implies Read: a write grant satisfies a read need; read satisfies only read.
fn permission_satisfies(granted: Permission, needed: Permission) -> bool {
    match (granted, needed) {
        (Permission::Write, _) => true,
        (Permission::Read, Permission::Read) => true,
        (Permission::Read, Permission::Write) => false,
    }
}

// Returns true if `principal` is directly named by `grant` or, for Org grants,
// if the principal is a member of the org. Only users can hold org memberships.
fn principal_matches(conn: &Connection, principal: &Principal, grant: &Grant) -> Result<bool> {
    match grant.principal_kind {
        PrincipalKind::User => {
            Ok(principal.kind == PrincipalKind::User && grant.principal_id == principal.id)
        }
        PrincipalKind::Token => {
            Ok(principal.kind == PrincipalKind::Token && grant.principal_id == principal.id)
        }
        PrincipalKind::Org => {
            if principal.kind != PrincipalKind::User {
                return Ok(false);
            }
            let user_id: i64 = match principal.id.parse() {
                Ok(id) => id,
                Err(_) => return Ok(false),
            };
            let org_id: i64 = match grant.principal_id.parse() {
                Ok(id) => id,
                Err(_) => return Ok(false),
            };
            let role = repo::role_of(conn, user_id, org_id)?;
            Ok(role.is_some())
        }
    }
}

/// Insert a new ACL grant. Returns the rowid of the new row.
pub fn grant(
    conn: &Connection,
    principal_kind: PrincipalKind,
    principal_id: &str,
    workspace_id: i64,
    path_prefix: &str,
    permission: Permission,
    created_at: i64,
) -> Result<i64> {
    assert!(!principal_id.is_empty(), "principal_id must not be empty");
    assert!(workspace_id > 0, "workspace_id must be a positive rowid");
    assert!(
        path_prefix.len() <= 4096,
        "path_prefix must not exceed 4096 bytes"
    );
    assert!(
        created_at >= 0,
        "created_at must be a non-negative unix timestamp"
    );

    conn.execute(
        "INSERT INTO acl_grants
             (principal_kind, principal_id, workspace_id, path_prefix, permission, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            principal_kind_to_str(principal_kind),
            principal_id,
            workspace_id,
            path_prefix,
            permission_to_str(permission),
            created_at,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Stamp revoked_at on the grant identified by `grant_id`.
/// Returns an error if the grant does not exist or is already revoked.
pub fn revoke(conn: &Connection, grant_id: i64, revoked_at: i64) -> Result<()> {
    assert!(grant_id > 0, "grant_id must be a positive rowid");
    assert!(
        revoked_at >= 0,
        "revoked_at must be a non-negative unix timestamp"
    );

    let rows = conn.execute(
        "UPDATE acl_grants SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
        params![revoked_at, grant_id],
    )?;
    if rows == 0 {
        return Err(anyhow::anyhow!(
            "grant {} not found or already revoked",
            grant_id
        ));
    }
    Ok(())
}

/// Returns all active (non-revoked) grants for the given workspace.
pub fn list_for_workspace(conn: &Connection, workspace_id: i64) -> Result<Vec<Grant>> {
    assert!(workspace_id > 0, "workspace_id must be a positive rowid");

    let mut stmt = conn.prepare(
        "SELECT id, principal_kind, principal_id, workspace_id, path_prefix,
                permission, created_at, revoked_at
         FROM acl_grants
         WHERE workspace_id = ?1 AND revoked_at IS NULL",
    )?;

    let rows = stmt.query_map(params![workspace_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, Option<i64>>(7)?,
        ))
    })?;

    let mut grants = Vec::new();
    for row in rows {
        let (id, kind_str, principal_id, ws_id, path_prefix, perm_str, created_at, revoked_at) =
            row?;
        let principal_kind = str_to_principal_kind(&kind_str)
            .ok_or_else(|| anyhow::anyhow!("unknown principal_kind in acl_grants: {kind_str}"))?;
        let permission = str_to_permission(&perm_str)
            .ok_or_else(|| anyhow::anyhow!("unknown permission in acl_grants: {perm_str}"))?;
        grants.push(Grant {
            id,
            principal_kind,
            principal_id,
            workspace_id: ws_id,
            path_prefix,
            permission,
            created_at,
            revoked_at,
        });
    }
    Ok(grants)
}

/// Decide whether `principal` may perform `needed` on `path` in `workspace_id`.
///
/// Rules (in order):
///   1. Only active (revoked_at IS NULL) grants in the given workspace are considered.
///   2. A grant applies only when its path_prefix covers the requested path (see path_matches_prefix).
///   3. write satisfies read or write; read satisfies only read.
///   4. An org grant covers any user who is a member of that org (any role).
///   5. Default: Deny if no matching grant exists.
pub fn authorize(
    conn: &Connection,
    principal: &Principal,
    workspace_id: i64,
    path: &str,
    needed: Permission,
) -> Result<Decision> {
    assert!(workspace_id > 0, "workspace_id must be a positive rowid");
    assert!(path.len() <= 4096, "path must not exceed 4096 bytes");

    let grants = list_for_workspace(conn, workspace_id)?;
    assert!(
        grants.len() <= 1_000_000,
        "grant count exceeds safe cap of 1M"
    );

    for g in &grants {
        if !path_matches_prefix(path, &g.path_prefix) {
            continue;
        }
        if !permission_satisfies(g.permission, needed) {
            continue;
        }
        if principal_matches(conn, principal, g)? {
            return Ok(Decision::Allow);
        }
    }

    Ok(Decision::Deny)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{open, repo, Role};
    use tempfile::tempdir;

    fn setup() -> (tempfile::TempDir, Connection) {
        let dir = tempdir().unwrap();
        let conn = open(&dir.path().join("test.db")).unwrap();
        (dir, conn)
    }

    // (a) A read-only grant does not allow write.
    #[test]
    fn read_grant_blocks_write() {
        let (_dir, conn) = setup();
        let ws = repo::create_workspace(&conn, "ws", crate::auth::OwnerKind::User, 1, 1).unwrap();

        // Create a user to satisfy any FK, even though workspace owner doesn't FK-check here.
        let uid = repo::create_user(&conn, None, 1).unwrap();
        let gid = grant(
            &conn,
            PrincipalKind::User,
            &uid.to_string(),
            ws,
            "/data/",
            Permission::Read,
            1,
        )
        .unwrap();
        assert!(gid > 0);

        let principal = Principal {
            kind: PrincipalKind::User,
            id: uid.to_string(),
        };

        // Read allowed.
        assert_eq!(
            authorize(&conn, &principal, ws, "/data/file.txt", Permission::Read).unwrap(),
            Decision::Allow
        );
        // Write denied: grant is read-only.
        assert_eq!(
            authorize(&conn, &principal, ws, "/data/file.txt", Permission::Write).unwrap(),
            Decision::Deny
        );
    }

    // (b) A write grant allows both read and write (write-implies-read).
    #[test]
    fn write_grant_allows_read_and_write() {
        let (_dir, conn) = setup();
        let ws = repo::create_workspace(&conn, "ws", crate::auth::OwnerKind::User, 1, 1).unwrap();
        let uid = repo::create_user(&conn, None, 1).unwrap();
        grant(
            &conn,
            PrincipalKind::User,
            &uid.to_string(),
            ws,
            "/repo/",
            Permission::Write,
            1,
        )
        .unwrap();

        let principal = Principal {
            kind: PrincipalKind::User,
            id: uid.to_string(),
        };

        assert_eq!(
            authorize(&conn, &principal, ws, "/repo/src/main.rs", Permission::Read).unwrap(),
            Decision::Allow,
            "write grant must satisfy read"
        );
        assert_eq!(
            authorize(
                &conn,
                &principal,
                ws,
                "/repo/src/main.rs",
                Permission::Write
            )
            .unwrap(),
            Decision::Allow,
            "write grant must satisfy write"
        );
    }

    // (c) A grant in workspace A does not authorize access in workspace B.
    #[test]
    fn cross_workspace_denied() {
        let (_dir, conn) = setup();
        let uid = repo::create_user(&conn, None, 1).unwrap();
        let ws_a =
            repo::create_workspace(&conn, "ws-a", crate::auth::OwnerKind::User, uid, 1).unwrap();
        let ws_b =
            repo::create_workspace(&conn, "ws-b", crate::auth::OwnerKind::User, uid, 2).unwrap();

        grant(
            &conn,
            PrincipalKind::User,
            &uid.to_string(),
            ws_a,
            "/shared/",
            Permission::Read,
            1,
        )
        .unwrap();

        let principal = Principal {
            kind: PrincipalKind::User,
            id: uid.to_string(),
        };

        // Allowed in ws_a.
        assert_eq!(
            authorize(&conn, &principal, ws_a, "/shared/doc.txt", Permission::Read).unwrap(),
            Decision::Allow
        );
        // Denied in ws_b: grant lives in ws_a.
        assert_eq!(
            authorize(&conn, &principal, ws_b, "/shared/doc.txt", Permission::Read).unwrap(),
            Decision::Deny
        );
    }

    // (d) Path outside the granted prefix is denied; /a must not cover /ab (boundary check).
    #[test]
    fn path_prefix_boundary() {
        let (_dir, conn) = setup();
        let uid = repo::create_user(&conn, None, 1).unwrap();
        let ws = repo::create_workspace(&conn, "ws", crate::auth::OwnerKind::User, uid, 1).unwrap();

        // Grant on /a only.
        grant(
            &conn,
            PrincipalKind::User,
            &uid.to_string(),
            ws,
            "/a",
            Permission::Read,
            1,
        )
        .unwrap();

        let principal = Principal {
            kind: PrincipalKind::User,
            id: uid.to_string(),
        };

        // Exact prefix match.
        assert_eq!(
            authorize(&conn, &principal, ws, "/a", Permission::Read).unwrap(),
            Decision::Allow
        );
        // Covered by /a (slash boundary).
        assert_eq!(
            authorize(&conn, &principal, ws, "/a/b", Permission::Read).unwrap(),
            Decision::Allow
        );
        // /b is outside /a entirely.
        assert_eq!(
            authorize(&conn, &principal, ws, "/b", Permission::Read).unwrap(),
            Decision::Deny
        );
        // /ab must NOT match /a (no slash boundary).
        assert_eq!(
            authorize(&conn, &principal, ws, "/ab", Permission::Read).unwrap(),
            Decision::Deny
        );
    }

    // (e) An org member is allowed via an org grant.
    #[test]
    fn org_member_allowed_via_org_grant() {
        let (_dir, conn) = setup();
        let uid = repo::create_user(&conn, None, 1).unwrap();
        let oid = repo::create_org(&conn, "acme", 1).unwrap();
        repo::add_membership(&conn, uid, oid, Role::Member).unwrap();

        let ws = repo::create_workspace(&conn, "ws", crate::auth::OwnerKind::Org, oid, 1).unwrap();

        // Grant to the org, not the user directly.
        grant(
            &conn,
            PrincipalKind::Org,
            &oid.to_string(),
            ws,
            "/shared/",
            Permission::Read,
            1,
        )
        .unwrap();

        let member = Principal {
            kind: PrincipalKind::User,
            id: uid.to_string(),
        };
        assert_eq!(
            authorize(&conn, &member, ws, "/shared/file.txt", Permission::Read).unwrap(),
            Decision::Allow
        );

        // A user who is NOT a member of the org is denied.
        let uid2 = repo::create_user(&conn, None, 2).unwrap();
        let outsider = Principal {
            kind: PrincipalKind::User,
            id: uid2.to_string(),
        };
        assert_eq!(
            authorize(&conn, &outsider, ws, "/shared/file.txt", Permission::Read).unwrap(),
            Decision::Deny
        );
    }

    // (f) A revoked grant is denied.
    #[test]
    fn revoked_grant_denied() {
        let (_dir, conn) = setup();
        let uid = repo::create_user(&conn, None, 1).unwrap();
        let ws = repo::create_workspace(&conn, "ws", crate::auth::OwnerKind::User, uid, 1).unwrap();

        let gid = grant(
            &conn,
            PrincipalKind::User,
            &uid.to_string(),
            ws,
            "/secret/",
            Permission::Read,
            1,
        )
        .unwrap();

        let principal = Principal {
            kind: PrincipalKind::User,
            id: uid.to_string(),
        };

        // Before revocation: allowed.
        assert_eq!(
            authorize(&conn, &principal, ws, "/secret/key.pem", Permission::Read).unwrap(),
            Decision::Allow
        );

        revoke(&conn, gid, 9999).unwrap();

        // After revocation: denied.
        assert_eq!(
            authorize(&conn, &principal, ws, "/secret/key.pem", Permission::Read).unwrap(),
            Decision::Deny
        );
    }

    // (g) No grant at all: denied.
    #[test]
    fn missing_grant_denied() {
        let (_dir, conn) = setup();
        let uid = repo::create_user(&conn, None, 1).unwrap();
        let ws = repo::create_workspace(&conn, "ws", crate::auth::OwnerKind::User, uid, 1).unwrap();

        let principal = Principal {
            kind: PrincipalKind::User,
            id: uid.to_string(),
        };
        assert_eq!(
            authorize(&conn, &principal, ws, "/anything/", Permission::Read).unwrap(),
            Decision::Deny
        );
    }
}
