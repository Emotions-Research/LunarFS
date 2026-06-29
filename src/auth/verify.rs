// SPDX-License-Identifier: AGPL-3.0-only
// LunarFS engine (bucket B). See /LICENSE and /LICENSING.md.

use std::sync::Arc;

// ---- Principal ----------------------------------------------------------

/// A resolved human identity extracted from an auth credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub user_id: String,
    pub org_id: Option<String>,
}

// ---- VerifyError --------------------------------------------------------

#[derive(Debug)]
pub enum VerifyError {
    /// Bearer credential could not be parsed as a JWT.
    Malformed,
    /// The JWT header `kid` was not found in the JWKS key set.
    UnknownKid,
    /// The JWT signature did not verify against the public key.
    InvalidSignature,
    /// The JWT `iss` claim does not match the configured issuer.
    InvalidIssuer,
    /// The JWT `aud` claim does not match the configured audience.
    InvalidAudience,
    /// The JWT `exp` claim is in the past per the injected clock.
    Expired,
    /// The JWKS key source could not be fetched or parsed.
    KeySource(String),
    /// JWT verification is not supported in self-host mode.
    Unsupported,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::Malformed => write!(f, "malformed bearer credential"),
            VerifyError::UnknownKid => write!(f, "key id not found in JWKS"),
            VerifyError::InvalidSignature => write!(f, "JWT signature invalid"),
            VerifyError::InvalidIssuer => write!(f, "JWT issuer mismatch"),
            VerifyError::InvalidAudience => write!(f, "JWT audience mismatch"),
            VerifyError::Expired => write!(f, "JWT has expired"),
            VerifyError::KeySource(e) => write!(f, "JWKS key source error: {e}"),
            VerifyError::Unsupported => write!(f, "JWT not accepted in self-host mode"),
        }
    }
}

impl std::error::Error for VerifyError {}

// ---- Verifier trait -----------------------------------------------------

/// Turn a bearer credential into a resolved human Principal.
pub trait Verifier: Send + Sync {
    fn verify(&self, bearer: &str) -> Result<Principal, VerifyError>;
}

// ---- NoClerk (self-host mode) -------------------------------------------

/// Self-host verifier that performs no JWT validation and requires no Clerk config.
///
/// In self-host deployments, human-principal resolution comes from API tokens
/// (see `auth::token::validate`), not from Clerk session JWTs. Any JWT presented
/// to this verifier is rejected with `VerifyError::Unsupported`.
pub struct NoClerk;

impl Verifier for NoClerk {
    fn verify(&self, _bearer: &str) -> Result<Principal, VerifyError> {
        Err(VerifyError::Unsupported)
    }
}

// ---- AuthMode (config gating) -------------------------------------------

/// Authentication mode resolved from environment variables at startup.
///
/// When all three Clerk variables are present (`CLERK_ISSUER`, `CLERK_AUDIENCE`,
/// `CLERK_JWKS_URL`) and the `hosted` cargo feature is enabled, selects
/// `ClerkJwtVerifier`. Otherwise falls back to `NoClerk` so the server boots
/// without any Clerk configuration.
pub enum AuthMode {
    Clerk {
        issuer: String,
        audience: String,
        jwks_url: String,
    },
    SelfHost,
}

impl AuthMode {
    pub fn from_env() -> Self {
        let issuer = std::env::var("CLERK_ISSUER").ok();
        let audience = std::env::var("CLERK_AUDIENCE").ok();
        let jwks_url = std::env::var("CLERK_JWKS_URL").ok();
        match (issuer, audience, jwks_url) {
            (Some(i), Some(a), Some(u)) if !i.is_empty() && !a.is_empty() && !u.is_empty() => {
                AuthMode::Clerk { issuer: i, audience: a, jwks_url: u }
            }
            _ => AuthMode::SelfHost,
        }
    }

    pub fn build_verifier(self) -> Arc<dyn Verifier> {
        match self {
            #[cfg(feature = "hosted")]
            AuthMode::Clerk { issuer, audience, jwks_url } => {
                let keys: Arc<dyn crate::auth::verifier::JwksKeySource> =
                    Arc::new(crate::auth::verifier::HttpJwksSource::new(jwks_url));
                let clock: Arc<dyn crate::auth::verifier::Clock> =
                    Arc::new(crate::auth::verifier::SystemClock);
                Arc::new(crate::auth::verifier::ClerkJwtVerifier::new(
                    issuer, audience, keys, clock,
                ))
            }
            _ => Arc::new(NoClerk),
        }
    }
}

// ---- Tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_clerk_returns_unsupported() {
        let v = NoClerk;
        let err = v.verify("any-bearer").expect_err("NoClerk must reject any credential");
        assert!(matches!(err, VerifyError::Unsupported), "expected Unsupported, got {err:?}");
        let err2 = v.verify("Bearer token").expect_err("NoClerk must reject Bearer-prefixed");
        assert!(matches!(err2, VerifyError::Unsupported));
    }

    #[test]
    fn auth_mode_from_env_selects_self_host_when_vars_absent() {
        // With no CLERK_* vars set, from_env must return SelfHost.
        let mode = AuthMode::from_env();
        let verifier = mode.build_verifier();
        let err = verifier.verify("any").expect_err("self-host mode must reject JWT");
        assert!(matches!(err, VerifyError::Unsupported));
    }
}
