use getrandom::getrandom;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::OwnerKind;

// Re-export OwnerKind under the token-centric name for external callers.
pub use super::OwnerKind as PrincipalKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub kind: OwnerKind,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Minted {
    pub token_id: i64,
    /// Returned exactly once. Never stored. Prefix "ddb_" makes tokens self-identifying.
    pub plaintext: String,
}

pub trait Clock {
    fn now_secs(&self) -> i64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_secs(&self) -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before the Unix epoch")
            .as_secs() as i64
    }
}

#[derive(Debug)]
pub enum AuthError {
    NotFound,
    Revoked,
    Expired,
    Rng(String),
    Db(rusqlite::Error),
    Internal(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::NotFound => write!(f, "token not found"),
            AuthError::Revoked => write!(f, "token has been revoked"),
            AuthError::Expired => write!(f, "token has expired"),
            AuthError::Rng(e) => write!(f, "RNG failure: {e}"),
            AuthError::Db(e) => write!(f, "database error: {e}"),
            AuthError::Internal(e) => write!(f, "internal error: {e}"),
        }
    }
}

impl std::error::Error for AuthError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        if let AuthError::Db(e) = self {
            Some(e)
        } else {
            None
        }
    }
}

impl From<rusqlite::Error> for AuthError {
    fn from(e: rusqlite::Error) -> Self {
        AuthError::Db(e)
    }
}

fn sha256_of(input: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    h.finalize().into()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn kind_to_str(k: OwnerKind) -> &'static str {
    match k {
        OwnerKind::User => "user",
        OwnerKind::Org => "org",
    }
}

fn str_to_kind(s: &str) -> Option<OwnerKind> {
    match s {
        "user" => Some(OwnerKind::User),
        "org" => Some(OwnerKind::Org),
        _ => None,
    }
}

/// Mint a new bearer token for `principal`. Returns the plaintext exactly once;
/// only its SHA-256 hash is written to the database.
///
/// The plaintext format is "ddb_<64 hex chars>" (256 bits of OS entropy).
/// The full string (prefix included) is hashed on both mint and validate.
pub fn mint(
    conn: &Connection,
    principal: &Principal,
    scope: Option<&str>,
    expires_at: Option<i64>,
    clock: &impl Clock,
) -> Result<Minted, AuthError> {
    assert!(!principal.id.is_empty(), "principal id must not be empty");
    if let Some(exp) = expires_at {
        assert!(exp > 0, "expires_at must be a positive unix timestamp");
    }

    let mut raw = [0u8; 32];
    getrandom(&mut raw).map_err(|e| AuthError::Rng(e.to_string()))?;

    let plaintext = format!("ddb_{}", hex_encode(&raw));
    let hash = sha256_of(&plaintext);

    let created_at = clock.now_secs();
    assert!(
        created_at >= 0,
        "clock must return a non-negative unix timestamp"
    );

    conn.execute(
        "INSERT INTO api_tokens
             (principal_kind, principal_id, token_hash, scope, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            kind_to_str(principal.kind),
            principal.id,
            hash.as_slice(),
            scope,
            created_at,
            expires_at,
        ],
    )?;

    Ok(Minted {
        token_id: conn.last_insert_rowid(),
        plaintext,
    })
}

/// Validate a bearer token. Returns the resolved Principal on success.
///
/// Rejects tokens where revoked_at is set (AuthError::Revoked) or where
/// expires_at is in the past per the injected clock (AuthError::Expired).
/// Unknown or garbage tokens return AuthError::NotFound and never panic.
pub fn validate(
    conn: &Connection,
    bearer: &str,
    clock: &impl Clock,
) -> Result<Principal, AuthError> {
    assert!(!bearer.is_empty(), "bearer must not be empty");

    let computed = sha256_of(bearer);

    let row = conn
        .query_row(
            "SELECT principal_kind, principal_id, token_hash, expires_at, revoked_at
             FROM api_tokens WHERE token_hash = ?1",
            params![computed.as_slice()],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                ))
            },
        )
        .optional()?;

    let (kind_str, principal_id, stored_hash, expires_at, revoked_at) = match row {
        None => return Err(AuthError::NotFound),
        Some(r) => r,
    };

    // Constant-time compare on raw hash bytes: defense-in-depth against timing leaks
    // even though the lookup itself is by hash.
    if stored_hash.len() != 32 {
        return Err(AuthError::Internal(
            "stored token_hash has unexpected length".to_string(),
        ));
    }
    if !bool::from(computed.as_slice().ct_eq(stored_hash.as_slice())) {
        return Err(AuthError::NotFound);
    }

    if revoked_at.is_some() {
        return Err(AuthError::Revoked);
    }

    if let Some(exp) = expires_at {
        if clock.now_secs() >= exp {
            return Err(AuthError::Expired);
        }
    }

    let kind = str_to_kind(&kind_str)
        .ok_or_else(|| AuthError::Internal(format!("unknown principal_kind in db: {kind_str}")))?;

    Ok(Principal {
        kind,
        id: principal_id,
    })
}

/// Stamp revoked_at on the token identified by `token_id`.
/// Returns AuthError::NotFound if no such token exists.
pub fn revoke(conn: &Connection, token_id: i64, clock: &impl Clock) -> Result<(), AuthError> {
    assert!(token_id > 0, "token_id must be a positive rowid");

    let revoked_at = clock.now_secs();
    assert!(
        revoked_at >= 0,
        "clock must return a non-negative unix timestamp"
    );

    let rows = conn.execute(
        "UPDATE api_tokens SET revoked_at = ?1 WHERE id = ?2",
        params![revoked_at, token_id],
    )?;

    if rows == 0 {
        return Err(AuthError::NotFound);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::open;
    use rusqlite::params;
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    struct FakeClock {
        pub now: i64,
    }

    impl Clock for FakeClock {
        fn now_secs(&self) -> i64 {
            self.now
        }
    }

    fn setup() -> (tempfile::TempDir, Connection) {
        let dir = tempdir().unwrap();
        let conn = open(&dir.path().join("test.db")).unwrap();
        (dir, conn)
    }

    // (a) round-trip: mint then validate succeeds and yields the right principal.
    #[test]
    fn roundtrip_mint_validate() {
        let (_dir, conn) = setup();
        let clock = FakeClock { now: 1_000_000 };
        let principal = Principal {
            kind: PrincipalKind::User,
            id: "42".to_string(),
        };

        let minted = mint(&conn, &principal, None, None, &clock).unwrap();
        assert!(
            minted.plaintext.starts_with("ddb_"),
            "plaintext must carry the ddb_ prefix"
        );
        assert!(minted.token_id > 0, "token_id must be a positive rowid");

        let resolved = validate(&conn, &minted.plaintext, &clock).unwrap();
        assert_eq!(resolved, principal);
    }

    // (b) wrong and malformed tokens are rejected without panicking.
    #[test]
    fn wrong_tokens_rejected() {
        let (_dir, conn) = setup();
        let clock = FakeClock { now: 1_000_000 };

        let err1 = validate(&conn, "ddb_notarealtoken", &clock);
        assert!(
            matches!(err1, Err(AuthError::NotFound)),
            "wrong token must be NotFound"
        );

        let err2 = validate(&conn, "completelymalformedstring", &clock);
        assert!(
            matches!(err2, Err(AuthError::NotFound)),
            "malformed token must be NotFound"
        );
    }

    // (c) mint then revoke then validate returns Revoked.
    #[test]
    fn revoked_token_rejected() {
        let (_dir, conn) = setup();
        let clock = FakeClock { now: 1_000_000 };
        let principal = Principal {
            kind: PrincipalKind::Org,
            id: "7".to_string(),
        };

        let minted = mint(&conn, &principal, None, None, &clock).unwrap();
        revoke(&conn, minted.token_id, &clock).unwrap();

        let err = validate(&conn, &minted.plaintext, &clock);
        assert!(
            matches!(err, Err(AuthError::Revoked)),
            "revoked token must return Revoked"
        );
    }

    // (d) token with expires_at in the past returns Expired.
    #[test]
    fn expired_token_rejected() {
        let (_dir, conn) = setup();
        let clock = FakeClock { now: 1_000_000 };
        let principal = Principal {
            kind: PrincipalKind::User,
            id: "99".to_string(),
        };

        let minted = mint(&conn, &principal, None, Some(1_001_000), &clock).unwrap();

        let future_clock = FakeClock { now: 1_002_000 };
        let err = validate(&conn, &minted.plaintext, &future_clock);
        assert!(
            matches!(err, Err(AuthError::Expired)),
            "expired token must return Expired"
        );
    }

    // (e) plaintext is never persisted: stored token_hash differs from the plaintext
    //     and equals the expected SHA-256 of the plaintext.
    #[test]
    fn plaintext_not_persisted() {
        let (_dir, conn) = setup();
        let clock = FakeClock { now: 1_000_000 };
        let principal = Principal {
            kind: PrincipalKind::User,
            id: "1".to_string(),
        };

        let minted = mint(&conn, &principal, None, None, &clock).unwrap();

        let stored_hash: Vec<u8> = conn
            .query_row(
                "SELECT token_hash FROM api_tokens WHERE id = ?1",
                params![minted.token_id],
                |r| r.get(0),
            )
            .unwrap();

        assert_ne!(
            stored_hash,
            minted.plaintext.as_bytes(),
            "stored token_hash must not equal the plaintext bytes"
        );

        let expected: [u8; 32] = {
            let mut h = Sha256::new();
            h.update(minted.plaintext.as_bytes());
            h.finalize().into()
        };
        assert_eq!(
            stored_hash,
            expected.as_slice(),
            "stored token_hash must equal SHA-256 of the plaintext"
        );
    }
}
