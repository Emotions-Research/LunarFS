use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

pub mod acl;
pub mod repo;
pub mod token;
#[cfg(feature = "hosted")]
pub mod verifier;
pub mod verify;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Owner,
    Admin,
    Member,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerKind {
    User,
    Org,
}

fn role_to_str(r: Role) -> &'static str {
    match r {
        Role::Owner => "owner",
        Role::Admin => "admin",
        Role::Member => "member",
    }
}

fn str_to_role(s: &str) -> Option<Role> {
    match s {
        "owner" => Some(Role::Owner),
        "admin" => Some(Role::Admin),
        "member" => Some(Role::Member),
        _ => None,
    }
}

fn owner_kind_to_str(k: OwnerKind) -> &'static str {
    match k {
        OwnerKind::User => "user",
        OwnerKind::Org => "org",
    }
}

fn str_to_owner_kind(s: &str) -> Option<OwnerKind> {
    match s {
        "user" => Some(OwnerKind::User),
        "org" => Some(OwnerKind::Org),
        _ => None,
    }
}

// Migration v1: creates all four identity tables.
// Uses CREATE TABLE IF NOT EXISTS so that the batch is safe to re-run if a
// prior attempt applied the DDL but failed before bumping user_version.
const MIGRATION_V1: &str = "
    CREATE TABLE IF NOT EXISTS users (
        id                INTEGER PRIMARY KEY,
        external_clerk_id TEXT,
        created_at        INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS organizations (
        id         INTEGER PRIMARY KEY,
        slug       TEXT NOT NULL UNIQUE,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS memberships (
        user_id INTEGER NOT NULL REFERENCES users(id),
        org_id  INTEGER NOT NULL REFERENCES organizations(id),
        role    TEXT NOT NULL CHECK (role IN ('owner', 'admin', 'member')),
        PRIMARY KEY (user_id, org_id)
    );
    CREATE TABLE IF NOT EXISTS workspaces (
        id         INTEGER PRIMARY KEY,
        name       TEXT NOT NULL,
        owner_kind TEXT NOT NULL CHECK (owner_kind IN ('user', 'org')),
        owner_id   INTEGER NOT NULL,
        created_at INTEGER NOT NULL
    );
";
// Note: workspaces.owner_id is a polymorphic reference -- users(id) when
// owner_kind='user', organizations(id) when owner_kind='org'. SQLite cannot
// express a cross-table FK, so this constraint is enforced in application code.

// Migration v2: machine tokens for the CLI and agent fleets.
// token_hash stores only the SHA-256 of the bearer plaintext; the plaintext is
// never written to the database. The index on token_hash makes validate() O(log n).
const MIGRATION_V2: &str = "
    CREATE TABLE IF NOT EXISTS api_tokens (
        id             INTEGER PRIMARY KEY,
        principal_kind TEXT    NOT NULL CHECK (principal_kind IN ('user', 'org')),
        principal_id   TEXT    NOT NULL,
        token_hash     BLOB    NOT NULL,
        scope          TEXT,
        created_at     INTEGER NOT NULL,
        expires_at     INTEGER,
        revoked_at     INTEGER
    );
    CREATE INDEX IF NOT EXISTS api_tokens_hash_idx ON api_tokens (token_hash);
";

// Migration v3: per-path ACL grants.
// Stores positive grants only; the absence of a matching grant means Deny.
// revoked_at nullable: NULL = active, non-NULL = revoked timestamp (unix seconds).
const MIGRATION_V3: &str = "
    CREATE TABLE IF NOT EXISTS acl_grants (
        id             INTEGER PRIMARY KEY,
        principal_kind TEXT    NOT NULL CHECK (principal_kind IN ('user', 'org', 'token')),
        principal_id   TEXT    NOT NULL,
        workspace_id   INTEGER NOT NULL,
        path_prefix    TEXT    NOT NULL,
        permission     TEXT    NOT NULL CHECK (permission IN ('read', 'write')),
        created_at     INTEGER NOT NULL,
        revoked_at     INTEGER
    );
";

// Migration v4: plan column on organizations.
// SQLite allows ADD COLUMN with a constant DEFAULT, so existing rows get 'free'.
const MIGRATION_V4: &str =
    "ALTER TABLE organizations ADD COLUMN plan TEXT NOT NULL DEFAULT 'free';";

// Migration v5: Stripe webhook fields on organizations.
// stripe_customer_id: nullable, set when org subscribes via Stripe.
// past_due: 0=current, 1=payment failed; reset to 0 on invoice.paid.
const MIGRATION_V5: &str = "
    ALTER TABLE organizations ADD COLUMN stripe_customer_id TEXT;
    ALTER TABLE organizations ADD COLUMN past_due INTEGER NOT NULL DEFAULT 0;
    CREATE INDEX IF NOT EXISTS org_stripe_customer_idx
        ON organizations (stripe_customer_id);
";

fn run_migrations(conn: &Connection) -> Result<()> {
    let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    assert!(version >= 0, "user_version must be non-negative");
    if version < 1 {
        conn.execute_batch(MIGRATION_V1)?;
        conn.execute_batch("PRAGMA user_version = 1;")?;
    }
    if version < 2 {
        conn.execute_batch(MIGRATION_V2)?;
        conn.execute_batch("PRAGMA user_version = 2;")?;
    }
    if version < 3 {
        conn.execute_batch(MIGRATION_V3)?;
        conn.execute_batch("PRAGMA user_version = 3;")?;
    }
    if version < 4 {
        conn.execute_batch(MIGRATION_V4)?;
        conn.execute_batch("PRAGMA user_version = 4;")?;
    }
    if version < 5 {
        conn.execute_batch(MIGRATION_V5)?;
        conn.execute_batch("PRAGMA user_version = 5;")?;
    }
    Ok(())
}

/// Open (or create) the identity database at `path`.
/// Sets PRAGMA foreign_keys = ON and applies all pending migrations.
/// Re-opening an already-migrated database is a no-op.
pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    run_migrations(&conn)?;
    Ok(conn)
}
