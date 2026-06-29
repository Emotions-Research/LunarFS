use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

use super::{owner_kind_to_str, role_to_str, str_to_owner_kind, str_to_role, OwnerKind, Role};

pub fn create_user(
    conn: &Connection,
    external_clerk_id: Option<&str>,
    created_at: i64,
) -> Result<i64> {
    assert!(created_at >= 0, "created_at must be a non-negative unix timestamp");
    conn.execute(
        "INSERT INTO users (external_clerk_id, created_at) VALUES (?1, ?2)",
        params![external_clerk_id, created_at],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn create_org(conn: &Connection, slug: &str, created_at: i64) -> Result<i64> {
    assert!(!slug.is_empty(), "org slug must not be empty");
    assert!(created_at >= 0, "created_at must be a non-negative unix timestamp");
    conn.execute(
        "INSERT INTO organizations (slug, created_at) VALUES (?1, ?2)",
        params![slug, created_at],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn add_membership(
    conn: &Connection,
    user_id: i64,
    org_id: i64,
    role: Role,
) -> Result<()> {
    assert!(user_id > 0, "user_id must be a positive rowid");
    assert!(org_id > 0, "org_id must be a positive rowid");
    conn.execute(
        "INSERT INTO memberships (user_id, org_id, role) VALUES (?1, ?2, ?3)",
        params![user_id, org_id, role_to_str(role)],
    )?;
    Ok(())
}

/// Add a membership only if the org's seat cap allows it.
/// Counts existing memberships (all human per schema), then in hosted builds calls
/// check_seat_available; in OSS builds no seat limit is applied. Returns Err if
/// the seat cap would be exceeded (hosted only).
pub fn add_membership_checked(
    conn: &Connection,
    user_id: i64,
    org_id: i64,
    role: Role,
) -> Result<()> {
    assert!(user_id > 0, "user_id must be a positive rowid");
    assert!(org_id > 0, "org_id must be a positive rowid");

    let count: u32 = conn.query_row(
        "SELECT COUNT(DISTINCT user_id) FROM memberships WHERE org_id = ?1",
        rusqlite::params![org_id],
        |r| r.get(0),
    )?;
    assert!(count <= 1_000_000, "membership count exceeds sanity cap");

    #[cfg(feature = "hosted")]
    {
        use crate::billing::entitlement::{check_seat_available, DbPlanSource, PlanSource};
        let plan = DbPlanSource::new(conn).plan_for_org(&org_id.to_string());
        check_seat_available(plan, count)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    }

    add_membership(conn, user_id, org_id, role)
}

pub fn role_of(conn: &Connection, user_id: i64, org_id: i64) -> Result<Option<Role>> {
    assert!(user_id > 0, "user_id must be a positive rowid");
    assert!(org_id > 0, "org_id must be a positive rowid");
    let result = conn
        .query_row(
            "SELECT role FROM memberships WHERE user_id = ?1 AND org_id = ?2",
            params![user_id, org_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    match result {
        None => Ok(None),
        Some(s) => str_to_role(&s)
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("unknown role value in db: {}", s)),
    }
}

pub fn create_workspace(
    conn: &Connection,
    name: &str,
    owner_kind: OwnerKind,
    owner_id: i64,
    created_at: i64,
) -> Result<i64> {
    assert!(!name.is_empty(), "workspace name must not be empty");
    assert!(created_at >= 0, "created_at must be a non-negative unix timestamp");
    conn.execute(
        "INSERT INTO workspaces (name, owner_kind, owner_id, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![name, owner_kind_to_str(owner_kind), owner_id, created_at],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn workspace_owner(conn: &Connection, workspace_id: i64) -> Result<Option<(OwnerKind, i64)>> {
    assert!(workspace_id > 0, "workspace_id must be a positive rowid");
    let result = conn
        .query_row(
            "SELECT owner_kind, owner_id FROM workspaces WHERE id = ?1",
            params![workspace_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?;
    match result {
        None => Ok(None),
        Some((kind_str, owner_id)) => {
            let kind = str_to_owner_kind(&kind_str)
                .ok_or_else(|| anyhow::anyhow!("unknown owner_kind in db: {}", kind_str))?;
            Ok(Some((kind, owner_id)))
        }
    }
}

/// Return the id of the workspace whose name matches, choosing the lowest id when
/// multiple rows share the same name. Returns None (not an error) when no row matches.
pub fn workspace_by_name(conn: &Connection, name: &str) -> Result<Option<i64>> {
    assert!(!name.is_empty(), "workspace name must not be empty");
    let result = conn
        .query_row(
            "SELECT id FROM workspaces WHERE name = ?1 ORDER BY id LIMIT 1",
            params![name],
            |r| r.get::<_, i64>(0),
        )
        .optional()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use crate::auth::{open, OwnerKind, Role};
    use rusqlite::params;
    use super::{
        add_membership, create_org, create_user, create_workspace, role_of, workspace_by_name,
        workspace_owner,
    };
    use tempfile::tempdir;

    #[test]
    fn create_user_roundtrip_with_and_without_clerk_id() {
        let dir = tempdir().unwrap();
        let conn = open(&dir.path().join("id.db")).unwrap();

        let id1 = create_user(&conn, Some("clerk_abc"), 1000).unwrap();
        let id2 = create_user(&conn, None, 2000).unwrap();
        assert!(id1 > 0);
        assert!(id2 > 0);
        assert_ne!(id1, id2);

        let (ext1, ts1): (Option<String>, i64) = conn
            .query_row(
                "SELECT external_clerk_id, created_at FROM users WHERE id = ?1",
                params![id1],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(ext1.as_deref(), Some("clerk_abc"));
        assert_eq!(ts1, 1000);

        let (ext2, ts2): (Option<String>, i64) = conn
            .query_row(
                "SELECT external_clerk_id, created_at FROM users WHERE id = ?1",
                params![id2],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(ext2.is_none());
        assert_eq!(ts2, 2000);
    }

    #[test]
    fn create_org_roundtrip_and_slug_uniqueness() {
        let dir = tempdir().unwrap();
        let conn = open(&dir.path().join("id.db")).unwrap();

        let id = create_org(&conn, "acme-corp", 5000).unwrap();
        assert!(id > 0);

        let (slug, ts): (String, i64) = conn
            .query_row(
                "SELECT slug, created_at FROM organizations WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(slug, "acme-corp");
        assert_eq!(ts, 5000);

        // Duplicate slug must error.
        let dup = create_org(&conn, "acme-corp", 6000);
        assert!(dup.is_err(), "duplicate slug must return an error");
    }

    #[test]
    fn membership_role_of_and_no_member() {
        let dir = tempdir().unwrap();
        let conn = open(&dir.path().join("id.db")).unwrap();

        let uid = create_user(&conn, None, 1).unwrap();
        let oid = create_org(&conn, "test-org", 1).unwrap();

        add_membership(&conn, uid, oid, Role::Admin).unwrap();

        let role = role_of(&conn, uid, oid).unwrap();
        assert_eq!(role, Some(Role::Admin));

        // A user with no membership returns None.
        let uid2 = create_user(&conn, None, 2).unwrap();
        let none_role = role_of(&conn, uid2, oid).unwrap();
        assert_eq!(none_role, None);
    }

    #[test]
    fn duplicate_membership_errors() {
        let dir = tempdir().unwrap();
        let conn = open(&dir.path().join("id.db")).unwrap();

        let uid = create_user(&conn, None, 1).unwrap();
        let oid = create_org(&conn, "dup-org", 1).unwrap();

        add_membership(&conn, uid, oid, Role::Member).unwrap();
        let dup = add_membership(&conn, uid, oid, Role::Owner);
        assert!(dup.is_err(), "duplicate (user_id, org_id) pair must error");
    }

    #[test]
    fn fk_enforcement_rejects_unknown_ids() {
        let dir = tempdir().unwrap();
        let conn = open(&dir.path().join("id.db")).unwrap();

        let oid = create_org(&conn, "fk-org", 1).unwrap();

        // Non-existent user_id must error because PRAGMA foreign_keys is ON.
        let err = add_membership(&conn, 9999, oid, Role::Member);
        assert!(err.is_err(), "FK constraint must reject non-existent user_id");

        let uid = create_user(&conn, None, 1).unwrap();
        // Non-existent org_id must error because PRAGMA foreign_keys is ON.
        let err2 = add_membership(&conn, uid, 9999, Role::Member);
        assert!(err2.is_err(), "FK constraint must reject non-existent org_id");
    }

    #[test]
    fn workspace_owner_user_and_org_and_unknown() {
        let dir = tempdir().unwrap();
        let conn = open(&dir.path().join("id.db")).unwrap();

        let uid = create_user(&conn, None, 1).unwrap();
        let oid = create_org(&conn, "ws-org", 1).unwrap();

        let wid_user =
            create_workspace(&conn, "my-workspace", OwnerKind::User, uid, 10).unwrap();
        let wid_org =
            create_workspace(&conn, "org-workspace", OwnerKind::Org, oid, 20).unwrap();

        let owner_user = workspace_owner(&conn, wid_user).unwrap();
        assert_eq!(owner_user, Some((OwnerKind::User, uid)));

        let owner_org = workspace_owner(&conn, wid_org).unwrap();
        assert_eq!(owner_org, Some((OwnerKind::Org, oid)));

        // id=9999 does not exist; must return None, not an error.
        let unknown = workspace_owner(&conn, 9999).unwrap();
        assert_eq!(unknown, None);
    }

    #[test]
    fn workspace_by_name_found_and_not_found() {
        let dir = tempdir().unwrap();
        let conn = open(&dir.path().join("id.db")).unwrap();

        let uid = create_user(&conn, None, 1).unwrap();
        let ws_id = create_workspace(&conn, "my-workspace", OwnerKind::User, uid, 10).unwrap();

        let found = workspace_by_name(&conn, "my-workspace").unwrap();
        assert_eq!(found, Some(ws_id), "should find the created workspace");

        let absent = workspace_by_name(&conn, "nonexistent").unwrap();
        assert_eq!(absent, None, "missing name must return None, not an error");
    }

    #[test]
    fn workspace_by_name_returns_lowest_id_on_duplicate_name() {
        let dir = tempdir().unwrap();
        let conn = open(&dir.path().join("id.db")).unwrap();

        let uid = create_user(&conn, None, 1).unwrap();
        let id1 = create_workspace(&conn, "shared", OwnerKind::User, uid, 10).unwrap();
        let id2 = create_workspace(&conn, "shared", OwnerKind::User, uid, 20).unwrap();
        assert!(id2 > id1, "second insert should have a higher rowid");

        let result = workspace_by_name(&conn, "shared").unwrap();
        assert_eq!(result, Some(id1), "must return the lowest id among duplicates");
    }

    #[test]
    fn migration_idempotency() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("id.db");

        {
            let conn = open(&db_path).unwrap();
            create_user(&conn, None, 1).unwrap();
            // conn dropped here, connection closed
        }

        // Second open on the same file must succeed without re-running migrations.
        let conn2 = open(&db_path).unwrap();
        let uid = create_user(&conn2, None, 2).unwrap();
        assert!(uid > 0);
    }

    #[test]
    fn migration_v4_plan_column_defaults_free() {
        let dir = tempdir().unwrap();
        let conn = open(&dir.path().join("id.db")).unwrap();

        let ver: i32 =
            conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(ver, 5, "user_version must be 5 after open");

        let oid = create_org(&conn, "v4-org", 1).unwrap();
        let plan: String = conn
            .query_row(
                "SELECT plan FROM organizations WHERE id = ?1",
                params![oid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(plan, "free", "new org must default to free plan");
    }
}
