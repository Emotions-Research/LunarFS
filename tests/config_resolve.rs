use devdropbox::config::{
    config_path_with_env, load_config_from_path, save_config_to_path, Config, DEFAULT_HOST,
};
use devdropbox::resolve::{resolve_workspace, ENV_ORG, ENV_SERVER, ENV_TOKEN};
use std::collections::HashMap;
use tempfile::TempDir;

// ---- helpers ----------------------------------------------------------------

fn make_temp_config() -> (TempDir, Config) {
    let dir = tempfile::tempdir().expect("tempdir must be creatable");
    let path = dir.path().join("config");
    let cfg = Config {
        server: Some("https://my.server.example.com".to_string()),
        token: Some("tok_abc123".to_string()),
        org: Some("myorg".to_string()),
    };
    save_config_to_path(&path, &cfg).expect("save_config_to_path must succeed");
    let loaded = load_config_from_path(&path).expect("load_config_from_path must succeed");
    // Verify round-trip before handing to callers.
    assert_eq!(loaded.server.as_deref(), Some("https://my.server.example.com"));
    assert_eq!(loaded.token.as_deref(), Some("tok_abc123"));
    assert_eq!(loaded.org.as_deref(), Some("myorg"));
    (dir, loaded)
}

// ---- (a) bare name resolves from config -------------------------------------

#[test]
fn bare_name_resolves_server_org_workspace_token_from_config() {
    let (_dir, cfg) = make_temp_config();
    let env = HashMap::new();
    let target = resolve_workspace("ws", &cfg, &env)
        .expect("bare workspace name must resolve when org is in config");
    assert_eq!(target.server, "https://my.server.example.com");
    assert_eq!(target.org, "myorg");
    assert_eq!(target.workspace, "ws");
    assert_eq!(target.token, "tok_abc123");
}

// ---- (b) org/workspace resolves org from name, server from config -----------

#[test]
fn org_slash_workspace_resolves() {
    let (_dir, cfg) = make_temp_config();
    let env = HashMap::new();
    let target = resolve_workspace("myorg/ws", &cfg, &env)
        .expect("org/workspace must resolve");
    assert_eq!(target.org, "myorg", "org must come from name");
    assert_eq!(target.workspace, "ws");
    assert_eq!(target.server, "https://my.server.example.com", "server from config");
    assert_eq!(target.token, "tok_abc123");
}

// ---- (c) 3-segment name: host prepended with https:// when scheme absent ---

#[test]
fn three_seg_host_scheme_prepended_when_absent() {
    let (_dir, cfg) = make_temp_config();
    let env = HashMap::new();
    let target = resolve_workspace("self.example.com/myorg/ws", &cfg, &env)
        .expect("3-segment name must resolve");
    assert_eq!(target.server, "https://self.example.com", "scheme must be prepended");
    assert_eq!(target.org, "myorg");
    assert_eq!(target.workspace, "ws");
}

#[test]
fn three_seg_host_existing_scheme_preserved() {
    let (_dir, cfg) = make_temp_config();
    let env = HashMap::new();
    let target = resolve_workspace("http://self.example.com/myorg/ws", &cfg, &env)
        .expect("3-segment name with existing scheme must resolve");
    assert_eq!(target.server, "http://self.example.com", "existing scheme must not be doubled");
}

// ---- (d) env override beats config ------------------------------------------

#[test]
fn env_server_and_token_override_config() {
    let (_dir, cfg) = make_temp_config();
    let mut env = HashMap::new();
    env.insert(ENV_SERVER.to_string(), "https://override.server.com".to_string());
    env.insert(ENV_TOKEN.to_string(), "env_tok_xyz".to_string());
    let target = resolve_workspace("myorg/ws", &cfg, &env)
        .expect("must resolve with env overrides");
    assert_eq!(target.server, "https://override.server.com", "env server must beat config");
    assert_eq!(target.token, "env_tok_xyz", "env token must beat config");
}

// ---- (d2) 3-segment host beats env server override -------------------------

#[test]
fn three_seg_host_beats_env_server_override() {
    let (_dir, cfg) = make_temp_config();
    let mut env = HashMap::new();
    env.insert(ENV_SERVER.to_string(), "https://override.server.com".to_string());
    env.insert(ENV_TOKEN.to_string(), "tok".to_string());
    let target = resolve_workspace("self.example.com/myorg/ws", &cfg, &env)
        .expect("3-segment name must resolve");
    assert_eq!(
        target.server,
        "https://self.example.com",
        "host segment in name must beat env server override"
    );
}

// ---- (e) DEFAULT fallback when config.server is None and no env override ----

#[test]
fn default_host_when_config_server_absent_and_no_env_override() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config");
    let cfg_no_server =
        Config { server: None, token: Some("tok".to_string()), org: Some("myorg".to_string()) };
    save_config_to_path(&path, &cfg_no_server).expect("save");
    let loaded = load_config_from_path(&path).expect("load");
    let env = HashMap::new();
    let target = resolve_workspace("ws", &loaded, &env)
        .expect("bare name must resolve to DEFAULT_HOST when no server in config or env");
    assert_eq!(target.server, DEFAULT_HOST, "must fall through to compiled DEFAULT_HOST");
}

// ---- (f) missing token -> clear error mentioning login / env var ------------

#[test]
fn missing_token_yields_error_with_login_hint() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config");
    let cfg_no_token =
        Config { server: None, token: None, org: Some("myorg".to_string()) };
    save_config_to_path(&path, &cfg_no_token).expect("save");
    let loaded = load_config_from_path(&path).expect("load");
    let env = HashMap::new();
    let err = resolve_workspace("ws", &loaded, &env)
        .expect_err("missing token must yield an error");
    let msg = err.to_string();
    assert!(
        msg.contains("login") || msg.contains(ENV_TOKEN),
        "error message must mention 'login' or '{}', got: {:?}",
        ENV_TOKEN,
        msg
    );
}

// ---- (g) missing config file returns empty Config, not error ----------------

#[test]
fn missing_config_file_returns_empty_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("does_not_exist");
    let cfg = load_config_from_path(&path)
        .expect("missing config file must not return an error");
    assert!(cfg.server.is_none(), "empty config: server must be None");
    assert!(cfg.token.is_none(), "empty config: token must be None");
    assert!(cfg.org.is_none(), "empty config: org must be None");
}

// ---- (h) config path seam: LUNAR_CONFIG_HOME resolves to temp dir ------

#[test]
fn config_path_seam_resolves_inside_temp_dir_not_home() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut env = HashMap::new();
    env.insert(
        "LUNAR_CONFIG_HOME".to_string(),
        dir.path().to_str().expect("temp dir path is valid UTF-8").to_string(),
    );
    let path = config_path_with_env(&env).expect("config_path_with_env must succeed");
    assert!(
        path.starts_with(dir.path()),
        "config path must be inside temp dir, not $HOME: got {:?}",
        path
    );
    assert_eq!(
        path.file_name().and_then(|n| n.to_str()),
        Some("config"),
        "config file must be named 'config'"
    );
}

// ---- (i) empty-string env values treated as unset, fall through to config ---

#[test]
fn empty_string_env_values_treated_as_unset() {
    let (_dir, cfg) = make_temp_config();
    let mut env = HashMap::new();
    env.insert(ENV_SERVER.to_string(), String::new());
    env.insert(ENV_TOKEN.to_string(), String::new());
    let target = resolve_workspace("myorg/ws", &cfg, &env)
        .expect("must resolve via config when env values are empty strings");
    assert_eq!(
        target.server, "https://my.server.example.com",
        "empty env server must fall through to config"
    );
    assert_eq!(
        target.token, "tok_abc123",
        "empty env token must fall through to config"
    );
}

// ---- (j) env ORG override for bare workspace names --------------------------

#[test]
fn env_org_override_used_for_bare_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config");
    let cfg_no_org =
        Config { server: Some("https://s.example.com".to_string()), token: Some("tok".to_string()), org: None };
    save_config_to_path(&path, &cfg_no_org).expect("save");
    let loaded = load_config_from_path(&path).expect("load");
    let mut env = HashMap::new();
    env.insert(ENV_ORG.to_string(), "env_org".to_string());
    let target = resolve_workspace("ws", &loaded, &env)
        .expect("bare name must resolve when LUNAR_ORG is set in env");
    assert_eq!(target.org, "env_org");
    assert_eq!(target.workspace, "ws");
}

// ---- (k) invalid names return errors ----------------------------------------

#[test]
fn invalid_names_return_descriptive_errors() {
    let cfg = Config {
        server: None,
        token: Some("tok".to_string()),
        org: Some("myorg".to_string()),
    };
    let env = HashMap::new();

    // Empty name
    assert!(
        resolve_workspace("", &cfg, &env).is_err(),
        "empty name must return an error"
    );
    // Leading slash produces an empty first segment
    assert!(
        resolve_workspace("/ws", &cfg, &env).is_err(),
        "leading slash must return an error"
    );
    // Trailing slash produces an empty last segment
    assert!(
        resolve_workspace("ws/", &cfg, &env).is_err(),
        "trailing slash must return an error"
    );
    // Four segments exceeds the max of 3
    assert!(
        resolve_workspace("a/b/c/d", &cfg, &env).is_err(),
        "four segments must return an error"
    );
}

// ---- (l) bare name with no org anywhere -> error naming the input -----------

#[test]
fn bare_name_with_no_org_yields_error() {
    let cfg = Config { server: None, token: Some("tok".to_string()), org: None };
    let env = HashMap::new();
    let err = resolve_workspace("ws", &cfg, &env)
        .expect_err("bare name with no org in config or env must error");
    let msg = err.to_string();
    assert!(
        msg.contains("org"),
        "error message must mention 'org', got: {:?}",
        msg
    );
}

// ---- (m) config round-trip with empty/whitespace token treated as missing ---

#[test]
fn empty_token_in_config_treated_as_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config");
    // Store an empty token string (should be treated as missing).
    let cfg_empty_tok =
        Config { server: None, token: Some(String::new()), org: Some("myorg".to_string()) };
    save_config_to_path(&path, &cfg_empty_tok).expect("save");
    let loaded = load_config_from_path(&path).expect("load");
    let env = HashMap::new();
    let result = resolve_workspace("ws", &loaded, &env);
    // An empty token string in config is treated as absent; no env token either -> error.
    assert!(
        result.is_err(),
        "empty token in config with no env token must yield an error"
    );
}
