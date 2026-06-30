use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Hosted cloud base URL. Override via config `server` field or LUNAR_BASE_URL env var.
pub const DEFAULT_HOST: &str = "https://cloud.lunarfs.com";

/// Config persisted to ~/.lunar/config. All fields are optional; absent fields
/// fall through to env overrides and then the compiled DEFAULT_HOST constant.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
}

/// Resolve the config file path from an explicit env map.
/// Precedence: LUNAR_CONFIG_HOME (a directory) -> $HOME/.lunar/config.
/// Tests inject a HashMap with LUNAR_CONFIG_HOME set to a temp dir so the
/// real $HOME is never touched.
pub fn config_path_with_env(env: &HashMap<String, String>) -> Result<PathBuf> {
    if let Some(dir) = env.get("LUNAR_CONFIG_HOME").filter(|s| !s.is_empty()) {
        let path = PathBuf::from(dir).join("config");
        assert!(
            !path.as_os_str().is_empty(),
            "resolved config path must not be empty"
        );
        return Ok(path);
    }
    let home = home_dir_from_env(env)?;
    assert!(
        !home.as_os_str().is_empty(),
        "home directory path must not be empty"
    );
    Ok(home.join(".lunar").join("config"))
}

/// Resolve the config file path using the real process environment.
pub fn config_path() -> Result<PathBuf> {
    let env: HashMap<String, String> = std::env::vars().collect();
    config_path_with_env(&env)
}

fn home_dir_from_env(env: &HashMap<String, String>) -> Result<PathBuf> {
    if let Some(h) = env.get("HOME").filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(h));
    }
    if let Some(h) = env.get("USERPROFILE").filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(h));
    }
    anyhow::bail!("HOME is not set; cannot determine config path")
}

/// Load config from `path`. Returns an empty Config when the file does not exist.
/// A missing file is not an error: the caller falls through to env overrides and DEFAULT_HOST.
pub fn load_config_from_path(path: &Path) -> Result<Config> {
    assert!(
        !path.as_os_str().is_empty(),
        "config path must not be empty"
    );
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<Config>(&text)
            .with_context(|| format!("failed to parse config at {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(anyhow::Error::from(e))
            .with_context(|| format!("failed to read config at {}", path.display())),
    }
}

/// Load config from the seam-resolved path (reads real process env).
pub fn load_config() -> Result<Config> {
    let path = config_path()?;
    load_config_from_path(&path)
}

/// Write config to `path`. Creates the parent directory (mode 0700 on unix) and the
/// file (mode 0600 on unix) so the token is not world-readable.
pub fn save_config_to_path(path: &Path, cfg: &Config) -> Result<()> {
    assert!(
        !path.as_os_str().is_empty(),
        "config path must not be empty"
    );
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create config dir {}", dir.display()))?;
            set_dir_perms(dir)?;
        }
    }
    let text = serde_json::to_string_pretty(cfg).context("failed to serialize config")?;
    assert!(!text.is_empty(), "serialized config JSON must not be empty");
    std::fs::write(path, text.as_bytes())
        .with_context(|| format!("failed to write config to {}", path.display()))?;
    set_file_perms(path)?;
    Ok(())
}

/// Write config to the seam-resolved path (reads real process env).
pub fn save_config(cfg: &Config) -> Result<()> {
    let path = config_path()?;
    save_config_to_path(&path, cfg)
}

#[cfg(unix)]
fn set_dir_perms(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to chmod 700 {}", path.display()))
}

#[cfg(not(unix))]
fn set_dir_perms(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_perms(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to chmod 600 {}", path.display()))
}

#[cfg(not(unix))]
fn set_file_perms(_path: &Path) -> Result<()> {
    Ok(())
}

/// Run mode for the workspace engine.
///
/// Local: all CoW operations stay on local disk; no network access occurs.
/// Cloud: a remote server and auth token are configured; blob transfers go off-device.
///
/// Mode is determined by `resolve_mode`; callers must NOT hardcode a mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Local,
    Cloud,
}

/// Resolve the run mode from a loaded Config.
///
/// Cloud mode activates when a non-empty `server` OR `token` is present.
/// An absent config file, an all-None config, or a config with only an `org`
/// field each resolve to Local: no remote is reachable without at least one
/// of server/token.
pub fn resolve_mode(cfg: &Config) -> Mode {
    assert!(
        cfg.server.as_deref().is_none_or(|s| s.len() < 4096),
        "server field must be a reasonable length"
    );
    let has_server = cfg.server.as_deref().filter(|s| !s.is_empty()).is_some();
    let has_token = cfg.token.as_deref().filter(|s| !s.is_empty()).is_some();
    if has_server || has_token {
        Mode::Cloud
    } else {
        Mode::Local
    }
}
