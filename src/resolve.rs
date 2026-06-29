use anyhow::Result;
use std::collections::HashMap;

use crate::config::{Config, DEFAULT_HOST};

/// Existing env var for the server base URL override.
pub const ENV_SERVER: &str = "LUNAR_BASE_URL";
/// Existing env var for the auth token override.
pub const ENV_TOKEN: &str = "LUNAR_TOKEN";
/// Env var for the default org override (new; scopes bare workspace names).
pub const ENV_ORG: &str = "LUNAR_ORG";

/// Fully resolved dispatch target produced by resolve_workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    pub server: String,
    pub org: String,
    pub workspace: String,
    pub token: String,
}

/// Resolve a (possibly host-qualified) workspace name to a concrete dispatch target.
///
/// Name grammar (Docker-image-ref style, split on '/'):
///   "workspace"              -> 1 seg: workspace=seg, org from env/config (error if neither)
///   "org/workspace"          -> 2 segs: org=seg0, workspace=seg1
///   "host/org/workspace"     -> 3 segs: server=normalize(seg0), org=seg1, workspace=seg2
///   "https://h/org/ws"       -> scheme-prefixed 3-seg: server=https://h, org, workspace
///   0, >3, or any empty seg  -> validation error naming the bad input
///
/// Precedence: name host > LUNAR_BASE_URL env > config.server > DEFAULT_HOST.
/// Token: LUNAR_TOKEN env > config.token; if neither, clear error with login hint.
/// Empty-string env values are treated as unset and fall through to config.
pub fn resolve_workspace(
    name: &str,
    cfg: &Config,
    env: &HashMap<String, String>,
) -> Result<ResolvedTarget> {
    if name.is_empty() {
        anyhow::bail!("workspace name must not be empty");
    }

    // Strip a leading http(s):// scheme so "https://host/org/ws" parses cleanly.
    // Without stripping, the "://" produces empty segments when split on '/'.
    let (scheme_prefix, bare) = extract_scheme(name);

    let segments: Vec<&str> = bare.split('/').collect();
    assert!(!segments.is_empty(), "split('/') always yields at least one segment");

    for seg in &segments {
        if seg.is_empty() {
            anyhow::bail!(
                "invalid workspace name {:?}: empty segment (check for leading, trailing, or consecutive slashes)",
                name
            );
        }
    }

    // A scheme-prefixed name must have exactly 3 bare segments: host/org/workspace.
    if scheme_prefix.is_some() && segments.len() != 3 {
        anyhow::bail!(
            "invalid workspace name {:?}: scheme-prefixed names require 3 segments (host/org/ws), got {}",
            name, segments.len()
        );
    }

    let (server, org, workspace) = match segments.len() {
        1 => {
            let ws = segments[0].to_string();
            let org = env_nonempty(env, ENV_ORG)
                .or_else(|| cfg.org.as_deref().filter(|s| !s.is_empty()).map(str::to_string))
                .ok_or_else(|| anyhow::anyhow!(
                    "org could not be determined for bare workspace name {:?}: \
                     set {} or add 'org' to ~/.lunar/config",
                    name, ENV_ORG
                ))?;
            (server_precedence(cfg, env), org, ws)
        }
        2 => {
            let org = segments[0].to_string();
            let ws = segments[1].to_string();
            (server_precedence(cfg, env), org, ws)
        }
        3 => {
            // If a scheme was extracted, prepend it back; otherwise normalize_host adds one.
            let server = match scheme_prefix {
                Some(p) => format!("{}{}", p, segments[0]),
                None => normalize_host(segments[0]),
            };
            assert!(
                server.starts_with("http://") || server.starts_with("https://"),
                "resolved server must have an http(s):// scheme"
            );
            let org = segments[1].to_string();
            let ws = segments[2].to_string();
            (server, org, ws)
        }
        n => anyhow::bail!(
            "invalid workspace name {:?}: expected 1-3 slash-delimited segments, got {}",
            name,
            n
        ),
    };

    let token = env_nonempty(env, ENV_TOKEN)
        .or_else(|| cfg.token.as_deref().filter(|s| !s.is_empty()).map(str::to_string))
        .ok_or_else(|| anyhow::anyhow!(
            "no auth token: run 'lunar login' or set {}",
            ENV_TOKEN
        ))?;

    assert!(!server.is_empty(), "resolved server must not be empty");
    assert!(!workspace.is_empty(), "resolved workspace must not be empty");

    Ok(ResolvedTarget { server, org, workspace, token })
}

/// Extract the workspace name segment from a possibly-qualified name without full resolution.
/// Used for the fork destination, which is a new name and does not require org context.
///
/// "ws" -> "ws", "org/ws" -> "ws", "host/org/ws" -> "ws"
pub fn parse_workspace_segment(name: &str) -> Result<String> {
    if name.is_empty() {
        anyhow::bail!("workspace name must not be empty");
    }
    let segments: Vec<&str> = name.split('/').collect();
    assert!(!segments.is_empty(), "split('/') always yields at least one segment");
    if segments.len() > 3 {
        anyhow::bail!(
            "invalid workspace name {:?}: too many segments (max 3), got {}",
            name,
            segments.len()
        );
    }
    let last = *segments.last().expect("non-empty segments vec has a last element");
    if last.is_empty() {
        anyhow::bail!("invalid workspace name {:?}: trailing slash or empty last segment", name);
    }
    Ok(last.to_string())
}

/// Server URL when no host segment is present in the name.
/// Precedence: LUNAR_BASE_URL env > config.server > DEFAULT_HOST constant.
fn server_precedence(cfg: &Config, env: &HashMap<String, String>) -> String {
    if let Some(s) = env_nonempty(env, ENV_SERVER) {
        return s;
    }
    if let Some(s) = cfg.server.as_deref().filter(|s| !s.is_empty()) {
        return s.to_string();
    }
    DEFAULT_HOST.to_string()
}

/// Strip an http(s):// scheme prefix from `name`, returning (Some(prefix), rest) or (None, name).
/// This lets "https://host/org/ws" parse cleanly: the "://" would otherwise produce
/// empty segments when split on '/'.
fn extract_scheme(name: &str) -> (Option<&str>, &str) {
    if let Some(rest) = name.strip_prefix("https://") {
        (Some("https://"), rest)
    } else if let Some(rest) = name.strip_prefix("http://") {
        (Some("http://"), rest)
    } else {
        (None, name)
    }
}

/// Normalize a bare host segment to a URL by prepending https:// when no scheme is present.
fn normalize_host(host: &str) -> String {
    assert!(!host.is_empty(), "host segment must not be empty");
    // extract_scheme is called first in resolve_workspace so a scheme can never appear here.
    format!("https://{}", host)
}

/// Return the env var value only if it is present AND non-empty.
/// An empty-string value is treated as unset so it falls through to config.
fn env_nonempty(env: &HashMap<String, String>, key: &str) -> Option<String> {
    env.get(key).and_then(|v| if v.is_empty() { None } else { Some(v.clone()) })
}
