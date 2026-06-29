use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Parser)]
#[command(name = "lunar", about = "LunarFS: content-addressed lazy filesystem for developer repos")]
struct Cli {
    #[command(subcommand)]
    cmd: SubCmd,
}

#[derive(Subcommand)]
enum SubCmd {
    /// Walk <repo> into the CAS and print the root tree hash.
    Ingest {
        /// Path to the repository to ingest.
        repo: PathBuf,
    },
    /// Mount the CAS view of <repo> at <mountpoint> (requires --features fuse).
    Mount {
        /// Path to the repository to mount.
        repo: PathBuf,
        /// Directory to use as the FUSE mount point.
        mountpoint: PathBuf,
    },
    /// Boot the HTTP blob service. Default address: 127.0.0.1:8787.
    Serve {
        /// Object store spec: local:<path> or s3://<bucket>.
        #[arg(long)]
        store: String,
        /// Address to bind (host:port).
        #[arg(long, default_value = "127.0.0.1:8787")]
        addr: String,
        /// Path to the identity SQLite database.
        #[arg(long, default_value = "lunar.db")]
        db: PathBuf,
    },
    /// Log in to a LunarFS server. Persists server URL and token to
    /// ~/.lunar/config so subsequent commands work without env vars.
    Login {
        /// Server base URL (defaults to the hosted cloud when omitted).
        #[arg(long)]
        server: Option<String>,
        /// Auth token to store.
        #[arg(long)]
        token: String,
        /// Default org used for bare workspace names (optional).
        #[arg(long)]
        org: Option<String>,
    },
    /// Push local CAS tree to a remote workspace.
    /// Server and token are read from ~/.lunar/config; env vars
    /// LUNAR_BASE_URL and LUNAR_TOKEN override config when set.
    Push {
        /// Workspace name: bare "ws", "org/ws", or "host/org/ws".
        workspace: String,
        /// Root tree hash to push (64-char hex from `lunar ingest`).
        root: String,
    },
    /// Pull a remote workspace into the local CAS.
    /// Server and token are read from ~/.lunar/config; env vars
    /// LUNAR_BASE_URL and LUNAR_TOKEN override config when set.
    Pull {
        /// Workspace name: bare "ws", "org/ws", or "host/org/ws".
        workspace: String,
    },
    /// Fork a remote workspace into a new workspace (O(1): copies only the root ref).
    /// Server and token are read from ~/.lunar/config; env vars
    /// LUNAR_BASE_URL and LUNAR_TOKEN override config when set.
    Fork {
        /// Name of the workspace to fork from (bare, org/ws, or host/org/ws).
        base: String,
        /// Name for the new forked workspace (bare, org/ws, or host/org/ws).
        fork: String,
    },
    /// Manage local ephemeral agent workspaces. Subcommands: fork, ls, destroy.
    Ws {
        #[command(subcommand)]
        cmd: WsCmd,
    },
}

#[derive(Subcommand)]
enum WsCmd {
    /// Fork a local workspace from a base ref (O(1) CoW, no data copy).
    Fork {
        /// Base ref to fork from (a root hash, label, or workspace id). Defaults to "HEAD".
        #[arg(long, default_value = "HEAD")]
        from: String,
        /// Mark the workspace ephemeral with a 24-hour TTL.
        #[arg(long)]
        ephemeral: bool,
        /// Optional human-readable label for the workspace.
        #[arg(long)]
        label: Option<String>,
        /// Path to the state database (created if absent).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// List all local workspaces.
    Ls {
        /// Path to the state database.
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Destroy a local workspace by id (drops overlay, ref, and record).
    Destroy {
        /// Workspace id to destroy.
        id: String,
        /// Path to the state database.
        #[arg(long)]
        db: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        SubCmd::Ingest { repo } => {
            let hex = devdropbox::ingest::ingest_repo(&repo)
                .with_context(|| format!("failed to ingest {}", repo.display()))?;
            println!("{}", hex);
        }
        SubCmd::Mount { repo, mountpoint } => {
            do_mount(repo, mountpoint)?;
        }
        SubCmd::Serve { store, addr, db } => {
            do_serve(store, addr, db)?;
        }
        SubCmd::Login { server, token, org } => {
            do_login(server, token, org)?;
        }
        SubCmd::Push { workspace, root } => {
            do_push(workspace, root)?;
        }
        SubCmd::Pull { workspace } => {
            do_pull(workspace)?;
        }
        SubCmd::Fork { base, fork } => {
            do_fork(base, fork)?;
        }
        SubCmd::Ws { cmd } => {
            do_ws(cmd)?;
        }
    }
    Ok(())
}

fn do_mount(repo: PathBuf, mountpoint: PathBuf) -> Result<()> {
    devdropbox::mount(&repo, &mountpoint).context(if cfg!(windows) {
        "ProjFS mount failed"
    } else {
        "FUSE mount failed"
    })
}

fn env_map() -> HashMap<String, String> {
    std::env::vars().collect()
}

fn do_login(server: Option<String>, token: String, org: Option<String>) -> Result<()> {
    let cfg = devdropbox::config::Config { server, token: Some(token), org };
    let path = devdropbox::config::config_path()?;
    devdropbox::config::save_config(&cfg)?;
    println!("logged in; config written to {}", path.display());
    Ok(())
}

fn do_push(workspace: String, root_hex: String) -> Result<()> {
    let cfg = devdropbox::config::load_config()?;
    let env = env_map();
    let target = devdropbox::resolve::resolve_workspace(&workspace, &cfg, &env)?;
    let root = devdropbox::cas::hex_to_hash(&root_hex)
        .context("invalid root hash: expected 64-char hex")?;
    let local = devdropbox::cas::FsStore::default_root()
        .context("failed to open local CAS")?;
    let remote = devdropbox::remote::HttpRemote::new(&target.server, &target.token);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;
    let uploaded =
        rt.block_on(devdropbox::sync::push(&local, &root, &remote, &target.workspace))?;
    println!("pushed {} blob(s) to workspace {}", uploaded, target.workspace);
    Ok(())
}

fn do_pull(workspace: String) -> Result<()> {
    let cfg = devdropbox::config::load_config()?;
    let env = env_map();
    let target = devdropbox::resolve::resolve_workspace(&workspace, &cfg, &env)?;
    let local = devdropbox::cas::FsStore::default_root()
        .context("failed to open local CAS")?;
    let remote = devdropbox::remote::HttpRemote::new(&target.server, &target.token);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;
    let root = rt.block_on(devdropbox::sync::pull(&remote, &target.workspace, &local))?;
    println!("pulled workspace {} root={}", target.workspace, devdropbox::cas::hash_to_hex(&root));
    Ok(())
}

fn do_fork(base: String, fork: String) -> Result<()> {
    let cfg = devdropbox::config::load_config()?;
    let env = env_map();
    let base_target = devdropbox::resolve::resolve_workspace(&base, &cfg, &env)?;
    let fork_name = devdropbox::resolve::parse_workspace_segment(&fork)?;
    let url = format!(
        "{}/v1/workspaces/{}/fork",
        base_target.server.trim_end_matches('/'),
        base_target.workspace
    );
    let body = serde_json::json!({"new_workspace": fork_name});
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(&url)
        .bearer_auth(&base_target.token)
        .json(&body)
        .send()
        .context("fork request failed")?;
    let status = resp.status();
    let text = resp.text().context("failed to read fork response body")?;
    if !status.is_success() {
        anyhow::bail!("fork failed ({}): {}", status, text);
    }
    println!("forked {} into {}", base_target.workspace, fork_name);
    println!("{}", text);
    Ok(())
}

fn do_ws(cmd: WsCmd) -> Result<()> {
    match cmd {
        WsCmd::Fork { from, ephemeral, label, db } => {
            do_ws_fork(from, ephemeral, label, db)
        }
        WsCmd::Ls { db } => do_ws_ls(db),
        WsCmd::Destroy { id, db } => do_ws_destroy(id, db),
    }
}

fn lunar_home_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .context("HOME is not set; cannot determine lunar directory")?;
    Ok(PathBuf::from(home).join(".lunar"))
}

fn ws_db_path(db: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = db {
        return Ok(p);
    }
    let dir = lunar_home_dir()?;
    std::fs::create_dir_all(&dir).context("failed to create ~/.lunar directory")?;
    Ok(dir.join("state.db"))
}

fn open_ws_store(db: Option<PathBuf>) -> Result<devdropbox::store::SqliteWorkspaceStore> {
    let path = ws_db_path(db)?;
    let conn = rusqlite::Connection::open(&path)
        .with_context(|| format!("failed to open workspace db at {}", path.display()))?;
    devdropbox::store::SqliteWorkspaceStore::open(conn)
        .context("failed to initialize workspace store schema")
}

fn ws_overlay_backend(db: Option<&PathBuf>) -> Result<devdropbox::workspace::LocalFsBackend> {
    let root = if let Some(p) = db {
        p.parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf()
            .join("workspaces")
    } else {
        lunar_home_dir()?.join("workspaces")
    };
    std::fs::create_dir_all(&root)
        .with_context(|| format!("failed to create workspace overlay root at {}", root.display()))?;
    Ok(devdropbox::workspace::LocalFsBackend::new(root))
}

fn do_ws_fork(
    from: String,
    ephemeral: bool,
    label: Option<String>,
    db: Option<PathBuf>,
) -> Result<()> {
    use devdropbox::workspace::{create_workspace, new_ws_id, SystemWsClock, WorkspaceSpec};
    use std::collections::BTreeMap;
    use std::time::Duration;

    let store = open_ws_store(db.clone())?;
    let backend = ws_overlay_backend(db.as_ref())?;
    let clock = SystemWsClock;
    let id = new_ws_id();
    let ttl = if ephemeral { Some(Duration::from_secs(86400)) } else { None };
    let spec = WorkspaceSpec { base_ref: from, label, metadata: BTreeMap::new(), ttl };

    let ws = create_workspace(&backend, &store, &clock, id, spec)?;

    println!("workspace created");
    println!("id:        {}", ws.id.0);
    println!("base_ref:  {}", ws.base_ref);
    println!("ephemeral: {}", ws.ephemeral);
    if let Some(ttl) = ws.ttl {
        println!("ttl:       {}s", ttl.as_secs());
    }
    Ok(())
}

fn do_ws_ls(db: Option<PathBuf>) -> Result<()> {
    use devdropbox::workspace::{list_workspaces, secs_since_epoch};

    let store = open_ws_store(db)?;
    let workspaces = list_workspaces(&store)?;

    if workspaces.is_empty() {
        println!("no local workspaces");
        return Ok(());
    }

    println!("{:<20} {:<12} {:<16} {:<10} {:<12} LABEL",
        "ID", "EPHEMERAL", "BASE_REF", "TTL(s)", "CREATED_AT");
    for ws in workspaces {
        let ttl_str = ws.ttl.map(|d| d.as_secs().to_string()).unwrap_or_else(|| "-".to_string());
        let label_str = ws.label.as_deref().unwrap_or("-");
        let base_short: String = ws.base_ref.chars().take(16).collect();
        println!("{:<20} {:<12} {:<16} {:<10} {:<12} {}",
            ws.id.0,
            ws.ephemeral,
            base_short,
            ttl_str,
            secs_since_epoch(ws.created_at),
            label_str,
        );
    }
    Ok(())
}

fn do_ws_destroy(id: String, db: Option<PathBuf>) -> Result<()> {
    use devdropbox::workspace::{destroy_workspace, WsId};

    let store = open_ws_store(db.clone())?;
    let backend = ws_overlay_backend(db.as_ref())?;
    let ws_id = WsId(id.clone());

    destroy_workspace(&backend, &store, &ws_id)
        .with_context(|| format!("failed to destroy workspace {}", id))?;

    println!("workspace {} destroyed", id);
    Ok(())
}

fn do_serve(store: String, addr: String, db_path: PathBuf) -> Result<()> {
    let obj_store = devdropbox::serve::build_object_store(&store)
        .with_context(|| format!("failed to build object store from {:?}", store))?;
    let presigner = devdropbox::serve::build_presigner(&store)
        .with_context(|| format!("failed to build presigner from {:?}", store))?;
    let conn = devdropbox::auth::open(&db_path)
        .with_context(|| format!("failed to open identity db at {}", db_path.display()))?;
    let verifier = devdropbox::auth::verify::AuthMode::from_env().build_verifier();
    let clock = Arc::new(devdropbox::auth::token::SystemClock);
    let overlays_dir = db_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("overlays");
    std::fs::create_dir_all(&overlays_dir)
        .with_context(|| format!("failed to create overlays dir at {}", overlays_dir.display()))?;
    let ws_conn = rusqlite::Connection::open(&db_path)
        .with_context(|| format!("failed to open workspace db at {}", db_path.display()))?;
    let ws_store = devdropbox::store::SqliteWorkspaceStore::open(ws_conn)
        .context("failed to initialize workspace store schema")?;
    let ws_backend = devdropbox::workspace::LocalFsBackend::new(overlays_dir);
    #[cfg(feature = "hosted")]
    let state = devdropbox::serve::AppState {
        store: obj_store,
        db: Arc::new(Mutex::new(conn)),
        verifier,
        clock,
        presigner,
        ws_backend: Arc::new(ws_backend),
        ws_store: Arc::new(ws_store),
        billing: Arc::new(devdropbox::billing::stripe::StripeBillingProvider::with_env()),
        webhook: Arc::new(devdropbox::billing::webhook::StripeWebhookProvider::from_env()),
    };
    #[cfg(not(feature = "hosted"))]
    let state = devdropbox::serve::AppState {
        store: obj_store,
        db: Arc::new(Mutex::new(conn)),
        verifier,
        clock,
        presigner,
        ws_backend: Arc::new(ws_backend),
        ws_store: Arc::new(ws_store),
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;
    rt.block_on(devdropbox::serve::run(state, &addr))
}
