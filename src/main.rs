use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Parser)]
#[command(
    name = "lunar",
    about = "LunarFS: content-addressed lazy filesystem for developer repos"
)]
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
        /// Workspace to auto-sync writes to while mounted; reads ~/.lunar/config like push/pull.
        #[arg(long)]
        watch: Option<String>,
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
    /// Watch a local directory and continuously sync changes to a remote workspace.
    /// Server and token are read from ~/.lunar/config; env vars
    /// LUNAR_BASE_URL and LUNAR_TOKEN override config when set.
    Autosync {
        /// Workspace name: bare "ws", "org/ws", or "host/org/ws".
        workspace: String,
        /// Directory to watch.
        dir: PathBuf,
        /// Quiet-period after the last fs event before pushing (milliseconds).
        #[arg(long, default_value_t = 500)]
        debounce_ms: u64,
        /// Poll interval for the engine loop (milliseconds).
        #[arg(long, default_value_t = 250)]
        poll_ms: u64,
    },
    /// Sync a local directory to a remote workspace in the foreground until Ctrl-C,
    /// or once with --once. Server and token are read from ~/.lunar/config; env vars
    /// LUNAR_BASE_URL and LUNAR_TOKEN override config when set.
    Sync {
        /// Workspace name: bare "ws", "org/ws", or "host/org/ws".
        workspace: String,
        /// Local directory to watch and sync.
        path: PathBuf,
        /// Quiet-period after the last fs event before pushing (milliseconds).
        #[arg(long, default_value_t = 800)]
        debounce_ms: u64,
        /// Poll interval for the engine loop (milliseconds).
        #[arg(long, default_value_t = 250)]
        poll_ms: u64,
        /// Sync the current state once and exit.
        #[arg(long)]
        once: bool,
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
    /// Show what changed between two tree roots (or a workspace vs its fork base).
    /// Each argument can be a 64-char root hash, a workspace label, or a workspace id.
    /// With a single workspace argument, diffs the workspace's recorded base_ref (old)
    /// against its recorded root (new), showing exactly what changed since forking.
    /// With --all, enumerates every local workspace grouped by base ref (same as `lunar ws diff`).
    Diff {
        /// First ref: root hash, workspace label, or workspace id. Omit when using --all.
        a: Option<String>,
        /// Second ref. If omitted, <a> must be a workspace (diffs base_ref vs root).
        b: Option<String>,
        /// Diff all local workspaces grouped by shared base ref (same as `lunar ws diff`).
        #[arg(long)]
        all: bool,
        /// Print only paths, one per line, no status markers or summary.
        #[arg(long)]
        name_only: bool,
        /// Print per-file byte deltas and a total summary.
        #[arg(long)]
        stat: bool,
        /// Print a machine-readable JSON array (one object per changed path).
        #[arg(long)]
        json: bool,
        /// Emit a unified git-style line diff for each changed text blob.
        #[arg(long)]
        patch: bool,
        /// Path to the workspace state database (default: ~/.lunar/state.db).
        #[arg(long)]
        db: Option<PathBuf>,
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
    /// Show what changed across all local workspaces, grouped by shared base ref.
    /// Flags paths touched by 2+ workspaces in the same base group.
    Diff {
        /// Path to the workspace state database (default: ~/.lunar/state.db).
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
        SubCmd::Mount {
            repo,
            mountpoint,
            watch,
        } => {
            do_mount(repo, mountpoint, watch)?;
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
        SubCmd::Autosync {
            workspace,
            dir,
            debounce_ms,
            poll_ms,
        } => {
            do_autosync(workspace, dir, debounce_ms, poll_ms)?;
        }
        SubCmd::Sync {
            workspace,
            path,
            debounce_ms,
            poll_ms,
            once,
        } => {
            do_sync(workspace, path, debounce_ms, poll_ms, once)?;
        }
        SubCmd::Fork { base, fork } => {
            do_fork(base, fork)?;
        }
        SubCmd::Ws { cmd } => {
            do_ws(cmd)?;
        }
        SubCmd::Diff {
            a,
            b,
            all,
            name_only,
            stat,
            json,
            patch,
            db,
        } => {
            do_diff(a, b, all, name_only, stat, json, patch, db)?;
        }
    }
    Ok(())
}

fn do_mount(repo: PathBuf, mountpoint: PathBuf, watch: Option<String>) -> Result<()> {
    if let Some(ws) = watch {
        use devdropbox::autosync::{
            format_sync_status, is_autosync_disabled, AutoSyncEngine, HttpBlobUploader,
            NotifyWatchSource, SystemClock, WalkSnapshotter, WorkspaceKind,
        };
        use std::sync::atomic::{AtomicBool, Ordering};

        let cfg = devdropbox::config::load_config()?;
        let env = env_map();
        let target = devdropbox::resolve::resolve_workspace(&ws, &cfg, &env)
            .context("failed to resolve watch workspace")?;

        let local: Arc<dyn devdropbox::cas::Store> =
            Arc::new(devdropbox::cas::FsStore::default_root().context("failed to open local CAS")?);
        let remote = devdropbox::remote::HttpRemote::new(&target.server, &target.token);

        let seed_rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build tokio runtime for seed")?;
        let seed = seed_rt
            .block_on(remote.get_ref(&target.workspace))
            .ok()
            .map(|h| devdropbox::cas::hash_to_hex(&h));

        let watch_src = NotifyWatchSource::new(&mountpoint)
            .context("failed to start fs watcher for mountpoint")?;
        let snapshotter = WalkSnapshotter::new(Arc::clone(&local), &mountpoint);
        let uploader = HttpBlobUploader::new(remote, Arc::clone(&local))
            .context("failed to build uploader")?;
        let workspace_name = target.workspace.clone();

        let stop = Arc::new(AtomicBool::new(false));
        let stop_sync = Arc::clone(&stop);
        let stop_ctrlc = Arc::clone(&stop);

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("signal handler rt");
            rt.block_on(async {
                let _ = tokio::signal::ctrl_c().await;
                stop_ctrlc.store(true, Ordering::Relaxed);
            });
        });

        let sync_thread = std::thread::spawn(move || {
            let mut engine = AutoSyncEngine::new(
                Box::new(watch_src),
                Arc::new(SystemClock),
                Box::new(snapshotter),
                Box::new(uploader),
                workspace_name,
                WorkspaceKind::Human,
                800,
                is_autosync_disabled(),
            );
            engine.seed_expected_root(seed);
            engine.run_blocking_with(250, &stop_sync, &mut |r| {
                println!("{}", format_sync_status(r))
            });
        });

        devdropbox::mount(&repo, &mountpoint).context(if cfg!(windows) {
            "ProjFS mount failed"
        } else {
            "FUSE mount failed"
        })?;

        stop.store(true, Ordering::Relaxed);
        let _ = sync_thread.join();
    } else {
        devdropbox::mount(&repo, &mountpoint).context(if cfg!(windows) {
            "ProjFS mount failed"
        } else {
            "FUSE mount failed"
        })?;
    }
    Ok(())
}

fn env_map() -> HashMap<String, String> {
    std::env::vars().collect()
}

fn do_login(server: Option<String>, token: String, org: Option<String>) -> Result<()> {
    let cfg = devdropbox::config::Config {
        server,
        token: Some(token),
        org,
    };
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
    let local = devdropbox::cas::FsStore::default_root().context("failed to open local CAS")?;
    let remote = devdropbox::remote::HttpRemote::new(&target.server, &target.token);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;
    let uploaded = rt.block_on(devdropbox::sync::push(
        &local,
        &root,
        &remote,
        &target.workspace,
    ))?;
    println!(
        "pushed {} blob(s) to workspace {}",
        uploaded, target.workspace
    );
    Ok(())
}

fn do_pull(workspace: String) -> Result<()> {
    let cfg = devdropbox::config::load_config()?;
    let env = env_map();
    let target = devdropbox::resolve::resolve_workspace(&workspace, &cfg, &env)?;
    let local = devdropbox::cas::FsStore::default_root().context("failed to open local CAS")?;
    let remote = devdropbox::remote::HttpRemote::new(&target.server, &target.token);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;
    let root = rt.block_on(devdropbox::sync::pull(&remote, &target.workspace, &local))?;
    println!(
        "pulled workspace {} root={}",
        target.workspace,
        devdropbox::cas::hash_to_hex(&root)
    );
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
        WsCmd::Fork {
            from,
            ephemeral,
            label,
            db,
        } => do_ws_fork(from, ephemeral, label, db),
        WsCmd::Ls { db } => do_ws_ls(db),
        WsCmd::Destroy { id, db } => do_ws_destroy(id, db),
        WsCmd::Diff { db } => do_ws_diff(db),
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
    std::fs::create_dir_all(&root).with_context(|| {
        format!(
            "failed to create workspace overlay root at {}",
            root.display()
        )
    })?;
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
    let ttl = if ephemeral {
        Some(Duration::from_secs(86400))
    } else {
        None
    };
    let spec = WorkspaceSpec {
        base_ref: from,
        label,
        metadata: BTreeMap::new(),
        ttl,
        root: None,
    };

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

    println!(
        "{:<20} {:<12} {:<16} {:<10} {:<12} LABEL",
        "ID", "EPHEMERAL", "BASE_REF", "TTL(s)", "CREATED_AT"
    );
    for ws in workspaces {
        let ttl_str = ws
            .ttl
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|| "-".to_string());
        let label_str = ws.label.as_deref().unwrap_or("-");
        let base_short: String = ws.base_ref.chars().take(16).collect();
        println!(
            "{:<20} {:<12} {:<16} {:<10} {:<12} {}",
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

fn do_ws_diff(db: Option<PathBuf>) -> Result<()> {
    use devdropbox::workspace::list_workspaces;
    use devdropbox::ws_diff::{detect_overlaps, group_by_base, render_group_report, WorkspaceDiff};

    let store = open_ws_store(db)?;
    let workspaces = list_workspaces(&store)?;

    if workspaces.is_empty() {
        println!("no local workspaces");
        return Ok(());
    }

    let cas = devdropbox::cas::FsStore::default_root().context("failed to open local CAS")?;
    let groups = group_by_base(&workspaces);

    for group in &groups {
        // Resolve the shared base_ref ONCE per group.
        let base_hash = match resolve_arg(&group.base_ref, &store) {
            Ok(resolved) => match resolved_to_hash(&resolved) {
                Ok(h) => h,
                Err(e) => {
                    println!(
                        "warning: cannot resolve base '{}' to a hash ({}); skipping group",
                        &group.base_ref, e
                    );
                    continue;
                }
            },
            Err(e) => {
                println!(
                    "warning: cannot resolve base '{}' ({}); skipping group",
                    &group.base_ref, e
                );
                continue;
            }
        };

        let mut group_diffs: Vec<WorkspaceDiff> = Vec::new();

        for member in &group.members {
            let root_hex = match member.root.as_deref() {
                Some(r) => r,
                None => {
                    let name = devdropbox::ws_diff::display_name(
                        &member.id.0,
                        member.label.as_deref(),
                    );
                    println!("{}: no recorded root, skipped", name);
                    continue;
                }
            };

            let member_hash = match devdropbox::cas::hex_to_hash(root_hex) {
                Ok(h) => h,
                Err(e) => {
                    let name = devdropbox::ws_diff::display_name(
                        &member.id.0,
                        member.label.as_deref(),
                    );
                    println!("{}: malformed root hash ({}), skipped", name, e);
                    continue;
                }
            };

            let mut changes = devdropbox::patch::diff_trees(base_hash, member_hash, &cas)
                .with_context(|| {
                    format!(
                        "diff_trees failed for workspace {}",
                        member.id.0
                    )
                })?;
            changes.sort_by(|x, y| x.path.cmp(&y.path));

            group_diffs.push(WorkspaceDiff {
                id: member.id.0.clone(),
                label: member.label.clone(),
                base_ref: member.base_ref.clone(),
                changes,
            });
        }

        let overlaps = detect_overlaps(&group_diffs);
        print!("{}", render_group_report(&group.base_ref, &group_diffs, &overlaps));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// diff subcommand
// ---------------------------------------------------------------------------

/// Result of resolving a user-supplied argument to an identity.
enum Resolved {
    /// A literal root hash (64-char hex, validated).
    Hash(devdropbox::cas::Hash),
    /// A workspace looked up by label or id.
    Ws(Box<devdropbox::workspace::Workspace>),
}

/// Resolve an argument to either a validated hash or a workspace record.
/// Resolution order (first match wins): (1) literal 64-char hex hash,
/// (2) workspace label, (3) workspace id.
fn resolve_arg(
    arg: &str,
    store: &dyn devdropbox::store::WorkspaceStore,
) -> Result<Resolved> {
    assert!(!arg.is_empty(), "resolve_arg: argument must not be empty");

    // (1) literal root hash
    if let Ok(h) = devdropbox::cas::hex_to_hash(arg) {
        return Ok(Resolved::Hash(h));
    }

    // Single list_all covers both (2) label and (3) id lookups.
    let all = store.list_all()?;
    assert!(all.len() <= 1_000_000, "workspace list exceeds sanity cap");

    // (2) workspace label
    if let Some(ws) = all.iter().find(|w| w.label.as_deref() == Some(arg)) {
        return Ok(Resolved::Ws(Box::new(ws.clone())));
    }

    // (3) workspace id
    if let Some(ws) = all.iter().find(|w| w.id.0 == arg) {
        return Ok(Resolved::Ws(Box::new(ws.clone())));
    }

    anyhow::bail!(
        "cannot resolve '{}' as a root hash, label, or workspace",
        arg
    );
}

/// Extract the root hash from a resolved argument.
/// For a workspace, requires `workspace.root` to be set.
fn resolved_to_hash(resolved: &Resolved) -> Result<devdropbox::cas::Hash> {
    assert!(
        matches!(resolved, Resolved::Hash(_) | Resolved::Ws(_)),
        "resolved_to_hash: unexpected variant"
    );
    match resolved {
        Resolved::Hash(h) => Ok(*h),
        Resolved::Ws(ws) => {
            let root_hex = ws.root.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "workspace '{}' has no recorded root; record a root before diffing",
                    ws.id.0
                )
            })?;
            devdropbox::cas::hex_to_hash(root_hex).map_err(|e| {
                anyhow::anyhow!(
                    "workspace '{}' root hash is malformed: {}",
                    ws.id.0,
                    e
                )
            })
        }
    }
}

/// Default git-style render: one `A/M/D path` line per change, sorted by path,
/// then a trailing `N files changed` line. Caller must pre-sort `changes`.
fn render_changeset(changes: &[devdropbox::patch::Change]) -> String {
    assert!(
        changes.len() <= 1_000_000,
        "render_changeset: changeset exceeds sanity cap"
    );
    let mut out = String::new();
    for c in changes {
        let marker = match c.kind {
            devdropbox::patch::ChangeKind::Added => 'A',
            devdropbox::patch::ChangeKind::Modified => 'M',
            devdropbox::patch::ChangeKind::Deleted => 'D',
        };
        out.push(marker);
        out.push(' ');
        out.push_str(&c.path.display().to_string());
        out.push('\n');
    }
    out.push_str(&format!("{} files changed\n", changes.len()));
    out
}

/// --name-only render: sorted paths only, one per line, no markers, no summary.
/// Caller must pre-sort `changes`.
fn render_name_only(changes: &[devdropbox::patch::Change]) -> String {
    assert!(
        changes.len() <= 1_000_000,
        "render_name_only: changeset exceeds sanity cap"
    );
    let mut out = String::new();
    for c in changes {
        out.push_str(&c.path.display().to_string());
        out.push('\n');
    }
    out
}

/// --stat render: per-file `path | +<added> -<removed>` then totals.
/// Byte delta rules: Added -> added=new_size, removed=0;
/// Deleted -> added=0, removed=old_size;
/// Modified -> added=max(0,new-old), removed=max(0,old-new).
/// Caller must pre-sort `changes`.
fn render_stat(changes: &[devdropbox::patch::Change]) -> String {
    assert!(
        changes.len() <= 1_000_000,
        "render_stat: changeset exceeds sanity cap"
    );
    let mut out = String::new();
    let mut total_added: u64 = 0;
    let mut total_removed: u64 = 0;

    for c in changes {
        let (added, removed) = match c.kind {
            devdropbox::patch::ChangeKind::Added => (c.new_size.unwrap_or(0), 0u64),
            devdropbox::patch::ChangeKind::Deleted => (0u64, c.old_size.unwrap_or(0)),
            devdropbox::patch::ChangeKind::Modified => {
                let old = c.old_size.unwrap_or(0);
                let new = c.new_size.unwrap_or(0);
                (new.saturating_sub(old), old.saturating_sub(new))
            }
        };
        total_added = total_added.saturating_add(added);
        total_removed = total_removed.saturating_add(removed);
        out.push_str(&format!(
            "{} | +{} -{}\n",
            c.path.display(),
            added,
            removed
        ));
    }
    out.push_str(&format!(
        "{} files changed (+{} -{} bytes)\n",
        changes.len(),
        total_added,
        total_removed
    ));
    out
}

/// --json render: compact JSON array sorted by path, one object per change.
/// Each object carries: path (string), status ("A"|"M"|"D"), old_size, new_size.
/// Caller must pre-sort `changes`. Uses serde_json (already a crate dependency).
fn changeset_to_json(changes: &[devdropbox::patch::Change]) -> String {
    assert!(
        changes.len() <= 1_000_000,
        "changeset_to_json: changeset exceeds sanity cap"
    );
    let arr: Vec<serde_json::Value> = changes
        .iter()
        .map(|c| {
            let status = match c.kind {
                devdropbox::patch::ChangeKind::Added => "A",
                devdropbox::patch::ChangeKind::Modified => "M",
                devdropbox::patch::ChangeKind::Deleted => "D",
            };
            serde_json::json!({
                "path": c.path.display().to_string(),
                "status": status,
                "old_size": c.old_size,
                "new_size": c.new_size,
            })
        })
        .collect();
    let mut out = serde_json::to_string_pretty(&arr).unwrap_or_else(|_| "[]".to_string());
    out.push('\n');
    out
}

fn do_diff(
    a: Option<String>,
    b: Option<String>,
    all: bool,
    name_only: bool,
    stat: bool,
    json: bool,
    patch: bool,
    db: Option<PathBuf>,
) -> Result<()> {
    if all {
        return do_ws_diff(db);
    }

    let a = a.ok_or_else(|| anyhow::anyhow!("diff requires a ref argument or --all"))?;
    assert!(!a.is_empty(), "do_diff: first argument must not be empty");

    let store = open_ws_store(db)?;

    let resolved_a = resolve_arg(&a, &store)?;

    let (old_hash, new_hash) = if let Some(b_str) = b {
        // Two-arg: resolve both sides independently.
        assert!(!b_str.is_empty(), "do_diff: second argument must not be empty");
        let old = resolved_to_hash(&resolved_a)?;
        let resolved_b = resolve_arg(&b_str, &store)?;
        let new = resolved_to_hash(&resolved_b)?;
        (old, new)
    } else {
        // One-arg: must resolve to a workspace; diffs base_ref vs recorded root.
        match &resolved_a {
            Resolved::Ws(ws) => {
                let new_hex = ws.root.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "workspace '{}' has no recorded root; push a tree root first \
                         or pass two explicit refs",
                        ws.id.0
                    )
                })?;
                let new_hash = devdropbox::cas::hex_to_hash(new_hex).map_err(|e| {
                    anyhow::anyhow!(
                        "workspace '{}' root hash is malformed: {}",
                        ws.id.0,
                        e
                    )
                })?;
                // Resolve base_ref through the same resolution order.
                let base_resolved = resolve_arg(&ws.base_ref, &store)?;
                let old_hash = resolved_to_hash(&base_resolved)?;
                (old_hash, new_hash)
            }
            Resolved::Hash(_) => {
                anyhow::bail!(
                    "'{}' resolves to a root hash with no diff base; \
                     pass two arguments or use a workspace id",
                    a
                );
            }
        }
    };

    let cas =
        devdropbox::cas::FsStore::default_root().context("failed to open local CAS")?;
    let mut changes = devdropbox::patch::diff_trees(old_hash, new_hash, &cas)
        .context("diff_trees failed")?;
    changes.sort_by(|x, y| x.path.cmp(&y.path));

    // Flag precedence: --patch > --json > --stat > --name-only. Document for callers.
    if patch {
        print!("{}", devdropbox::patch::render_patch(&changes, &cas)?);
    } else if json {
        print!("{}", changeset_to_json(&changes));
    } else if stat {
        print!("{}", render_stat(&changes));
    } else if name_only {
        print!("{}", render_name_only(&changes));
    } else {
        print!("{}", render_changeset(&changes));
    }
    Ok(())
}

fn do_autosync(workspace: String, dir: PathBuf, debounce_ms: u64, poll_ms: u64) -> Result<()> {
    use devdropbox::autosync::{
        is_autosync_disabled, AutoSyncEngine, HttpBlobUploader, NotifyWatchSource, SystemClock,
        WalkSnapshotter, WorkspaceKind,
    };
    use std::sync::atomic::{AtomicBool, Ordering};

    let cfg = devdropbox::config::load_config()?;
    let env = env_map();
    let target = devdropbox::resolve::resolve_workspace(&workspace, &cfg, &env)?;

    let local: Arc<dyn devdropbox::cas::Store> =
        Arc::new(devdropbox::cas::FsStore::default_root().context("failed to open local CAS")?);

    // Seed expected_root from the server's current ref so the first push is CAS-aware.
    // Use a dedicated remote + runtime for seeding; the uploader gets a fresh remote so
    // its reqwest connection pool is not contaminated by keep-alive connections from the
    // seed runtime (cross-runtime keep-alive reuse hangs the push pipeline).
    let seed = {
        let seed_remote = devdropbox::remote::HttpRemote::new(&target.server, &target.token);
        let seed_rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build tokio runtime for seed")?;
        seed_rt
            .block_on(seed_remote.get_ref(&target.workspace))
            .ok()
            .map(|h| devdropbox::cas::hash_to_hex(&h))
    };

    let watch = NotifyWatchSource::new(&dir).context("failed to start fs watcher")?;
    let snapshotter = WalkSnapshotter::new(Arc::clone(&local), &dir);
    let uploader = HttpBlobUploader::new(
        devdropbox::remote::HttpRemote::new(&target.server, &target.token),
        Arc::clone(&local),
    )
    .context("failed to build uploader")?;

    let mut engine = AutoSyncEngine::new(
        Box::new(watch),
        Arc::new(SystemClock),
        Box::new(snapshotter),
        Box::new(uploader),
        target.workspace.clone(),
        WorkspaceKind::Human,
        debounce_ms,
        is_autosync_disabled(),
    );
    engine.seed_expected_root(seed);

    println!(
        "autosync: watching {} -> workspace {}",
        dir.display(),
        target.workspace
    );

    // Sync the directory's current state once on startup so pre-existing files are
    // pushed immediately, then watch for subsequent changes.
    engine.run_once().context("initial sync on startup failed")?;

    let stop = Arc::new(AtomicBool::new(false));
    let stop_signal = Arc::clone(&stop);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("signal handler rt");
        rt.block_on(async {
            let _ = tokio::signal::ctrl_c().await;
            stop_signal.store(true, Ordering::Relaxed);
        });
    });

    engine.run_blocking(poll_ms, &stop);
    Ok(())
}

fn do_sync(
    workspace: String,
    path: PathBuf,
    debounce_ms: u64,
    poll_ms: u64,
    once: bool,
) -> Result<()> {
    use devdropbox::autosync::{
        format_sync_status, is_autosync_disabled, AutoSyncEngine, HttpBlobUploader,
        NotifyWatchSource, SystemClock, WalkSnapshotter, WorkspaceKind,
    };
    use std::sync::atomic::{AtomicBool, Ordering};

    if !path.is_dir() {
        anyhow::bail!(
            "sync path must be an existing directory: {}",
            path.display()
        );
    }

    let cfg = devdropbox::config::load_config()?;
    let env = env_map();
    let target = devdropbox::resolve::resolve_workspace(&workspace, &cfg, &env)?;

    let local: Arc<dyn devdropbox::cas::Store> =
        Arc::new(devdropbox::cas::FsStore::default_root().context("failed to open local CAS")?);

    // Use a dedicated short-lived remote and runtime to seed expected_root.
    // A fresh remote is created below for the uploader so the reqwest connection
    // pool is not shared across two tokio runtimes (keep-alive connections added
    // to the pool inside seed_rt would be inert in uploader's runtime, causing
    // push_cas to hang indefinitely waiting for a response that never arrives).
    let seed = {
        let seed_remote = devdropbox::remote::HttpRemote::new(&target.server, &target.token);
        let seed_rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build tokio runtime for seed")?;
        seed_rt
            .block_on(seed_remote.get_ref(&target.workspace))
            .ok()
            .map(|h| devdropbox::cas::hash_to_hex(&h))
    };

    let watch = NotifyWatchSource::new(&path).context("failed to start fs watcher")?;
    let snapshotter = WalkSnapshotter::new(Arc::clone(&local), &path);
    let uploader = HttpBlobUploader::new(
        devdropbox::remote::HttpRemote::new(&target.server, &target.token),
        Arc::clone(&local),
    )
    .context("failed to build uploader")?;

    let mut engine = AutoSyncEngine::new(
        Box::new(watch),
        Arc::new(SystemClock),
        Box::new(snapshotter),
        Box::new(uploader),
        target.workspace.clone(),
        WorkspaceKind::Human,
        debounce_ms,
        is_autosync_disabled(),
    );
    engine.seed_expected_root(seed);

    if once {
        println!(
            "sync: one-shot {} -> workspace {}",
            path.display(),
            workspace
        );
        let result = engine.run_once().context("one-shot sync failed")?;
        println!("{}", format_sync_status(&result));
        return Ok(());
    }

    println!(
        "sync: watching {} -> workspace {} (debounce {}ms)",
        path.display(),
        workspace,
        debounce_ms
    );

    // Sync the directory's current state once on startup so pre-existing files are
    // pushed immediately, then watch for subsequent changes.
    let initial = engine.run_once().context("initial sync failed")?;
    println!("{}", format_sync_status(&initial));

    let stop = Arc::new(AtomicBool::new(false));
    let stop_signal = Arc::clone(&stop);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("signal handler rt");
        rt.block_on(async {
            let _ = tokio::signal::ctrl_c().await;
            stop_signal.store(true, Ordering::Relaxed);
        });
    });

    engine.run_blocking_with(poll_ms, &stop, &mut |r| {
        println!("{}", format_sync_status(r))
    });
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
    std::fs::create_dir_all(&overlays_dir).with_context(|| {
        format!(
            "failed to create overlays dir at {}",
            overlays_dir.display()
        )
    })?;
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

#[cfg(test)]
mod tests {
    use super::{
        changeset_to_json, render_changeset, render_name_only, render_stat, resolve_arg,
        resolved_to_hash, Resolved,
    };
    use devdropbox::patch::{Change, ChangeKind};
    use devdropbox::store::{InMemoryWorkspaceStore, WorkspaceStore};
    use devdropbox::workspace::{Workspace, WsId};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::UNIX_EPOCH;

    fn hex64(fill: u8) -> String {
        format!("{:02x}", fill).repeat(32)
    }

    fn make_ws(
        id: &str,
        label: Option<&str>,
        base_ref: &str,
        root: Option<&str>,
    ) -> Workspace {
        Workspace {
            id: WsId(id.to_string()),
            label: label.map(|s| s.to_string()),
            metadata: BTreeMap::new(),
            base_ref: base_ref.to_string(),
            ttl: None,
            created_at: UNIX_EPOCH,
            ephemeral: false,
            root: root.map(|s| s.to_string()),
        }
    }

    fn save_ws(store: &InMemoryWorkspaceStore, ws: Workspace) {
        WorkspaceStore::save(store, &ws).expect("save workspace");
    }

    // (a) Argument resolution: literal 64-char hex resolves to Hash variant.
    #[test]
    fn resolve_literal_hash_succeeds() {
        let store = InMemoryWorkspaceStore::new();
        let hex = hex64(0xab);
        let result = resolve_arg(&hex, &store).expect("resolve literal hash");
        assert!(
            matches!(result, Resolved::Hash(_)),
            "64-char hex must resolve to Hash"
        );
    }

    // (a) Argument resolution: workspace label resolves to Ws variant.
    #[test]
    fn resolve_by_label_succeeds() {
        let store = InMemoryWorkspaceStore::new();
        let root_hex = hex64(0xbb);
        let ws = make_ws("ws-label-test", Some("my-label"), &hex64(0xcc), Some(&root_hex));
        save_ws(&store, ws);

        let result = resolve_arg("my-label", &store).expect("resolve by label");
        match result {
            Resolved::Ws(w) => {
                assert_eq!(w.label.as_deref(), Some("my-label"), "label must match");
                assert_eq!(w.id.0, "ws-label-test");
            }
            Resolved::Hash(_) => panic!("label lookup must return Ws, not Hash"),
        }
    }

    // (a) Argument resolution: workspace id resolves to Ws variant.
    #[test]
    fn resolve_by_workspace_id_succeeds() {
        let store = InMemoryWorkspaceStore::new();
        let root_hex = hex64(0xdd);
        let ws = make_ws("ws-id-test", None, &hex64(0xee), Some(&root_hex));
        save_ws(&store, ws);

        let result = resolve_arg("ws-id-test", &store).expect("resolve by workspace id");
        match result {
            Resolved::Ws(w) => assert_eq!(w.id.0, "ws-id-test"),
            Resolved::Hash(_) => panic!("id lookup must return Ws, not Hash"),
        }
    }

    // (a) Argument resolution: label takes priority over id with the same string.
    #[test]
    fn resolve_label_takes_priority_over_id() {
        let store = InMemoryWorkspaceStore::new();
        let ws_by_id = make_ws("shared-name", None, &hex64(0x11), Some(&hex64(0x22)));
        let ws_by_label = make_ws("other-id", Some("shared-name"), &hex64(0x33), Some(&hex64(0x44)));
        save_ws(&store, ws_by_id);
        save_ws(&store, ws_by_label);

        let result = resolve_arg("shared-name", &store).expect("resolve ambiguous name");
        match result {
            Resolved::Ws(w) => assert_eq!(
                w.id.0, "other-id",
                "label match must win over id match"
            ),
            Resolved::Hash(_) => panic!("must return Ws"),
        }
    }

    // (a) Unresolvable argument returns an error.
    #[test]
    fn resolve_unknown_arg_errors() {
        let store = InMemoryWorkspaceStore::new();
        let err = resolve_arg("not-a-hash-or-workspace", &store);
        assert!(err.is_err(), "unknown arg must produce an error");
    }

    // (b) Base-ref fallback: one-arg workspace exposes base_ref as old side
    //     and root as new side.
    #[test]
    fn base_ref_fallback_sides() {
        let store = InMemoryWorkspaceStore::new();
        let base_hex = hex64(0xff);
        let root_hex = hex64(0x01);
        let ws = make_ws("ws-agent", None, &base_hex, Some(&root_hex));
        save_ws(&store, ws);

        let resolved = resolve_arg("ws-agent", &store).expect("resolve workspace");
        match resolved {
            Resolved::Ws(w) => {
                assert_eq!(w.base_ref, base_hex, "base_ref must be old side");
                assert_eq!(
                    w.root.as_deref(),
                    Some(root_hex.as_str()),
                    "root must be new side"
                );
            }
            Resolved::Hash(_) => panic!("must resolve to Ws"),
        }
    }

    // (b) Workspace with no root errors in resolved_to_hash.
    #[test]
    fn workspace_no_root_errors_on_resolved_to_hash() {
        let store = InMemoryWorkspaceStore::new();
        let ws = make_ws("ws-no-root", None, &hex64(0xaa), None);
        save_ws(&store, ws);

        let resolved = resolve_arg("ws-no-root", &store).expect("resolve");
        let err = resolved_to_hash(&resolved);
        assert!(err.is_err(), "workspace with no root must error");
        let msg = err.unwrap_err().to_string();
        assert!(
            msg.contains("no recorded root"),
            "error must mention no recorded root, got: {}",
            msg
        );
    }

    // (c) Default render: sorted A/M/D lines plus trailing summary.
    #[test]
    fn render_changeset_default_output() {
        let mut changes = vec![
            Change {
                path: PathBuf::from("b.txt"),
                kind: ChangeKind::Modified,
                old_size: Some(10),
                new_size: Some(20),
                old_blob: None,
                new_blob: None,
            },
            Change {
                path: PathBuf::from("a.txt"),
                kind: ChangeKind::Added,
                old_size: None,
                new_size: Some(5),
                old_blob: None,
                new_blob: None,
            },
            Change {
                path: PathBuf::from("c.txt"),
                kind: ChangeKind::Deleted,
                old_size: Some(15),
                new_size: None,
                old_blob: None,
                new_blob: None,
            },
        ];
        changes.sort_by(|x, y| x.path.cmp(&y.path));
        let out = render_changeset(&changes);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "A a.txt", "first line: added a.txt");
        assert_eq!(lines[1], "M b.txt", "second line: modified b.txt");
        assert_eq!(lines[2], "D c.txt", "third line: deleted c.txt");
        assert_eq!(lines[3], "3 files changed", "trailing summary");
    }

    // (c) Empty changeset renders correctly.
    #[test]
    fn render_changeset_empty() {
        let out = render_changeset(&[]);
        assert_eq!(out, "0 files changed\n");
    }

    // (d) --name-only: sorted paths, no markers, no summary.
    #[test]
    fn render_name_only_output() {
        let changes = vec![
            Change {
                path: PathBuf::from("a.txt"),
                kind: ChangeKind::Added,
                old_size: None,
                new_size: Some(5),
                old_blob: None,
                new_blob: None,
            },
            Change {
                path: PathBuf::from("b.txt"),
                kind: ChangeKind::Modified,
                old_size: Some(10),
                new_size: Some(20),
                old_blob: None,
                new_blob: None,
            },
        ];
        let out = render_name_only(&changes);
        assert_eq!(out, "a.txt\nb.txt\n", "paths only, newline-terminated");
        assert!(
            !out.contains("A ") && !out.contains("M "),
            "must not have status markers"
        );
        assert!(!out.contains("files changed"), "must not have summary line");
    }

    // (d) --name-only empty changeset produces empty string.
    #[test]
    fn render_name_only_empty() {
        assert_eq!(render_name_only(&[]), "");
    }

    // (e) --stat: correct +/- byte deltas and totals.
    #[test]
    fn render_stat_output() {
        let changes = vec![
            // Added: added=new_size=10, removed=0
            Change {
                path: PathBuf::from("a.txt"),
                kind: ChangeKind::Added,
                old_size: None,
                new_size: Some(10),
                old_blob: None,
                new_blob: None,
            },
            // Deleted: added=0, removed=old_size=20
            Change {
                path: PathBuf::from("b.txt"),
                kind: ChangeKind::Deleted,
                old_size: Some(20),
                new_size: None,
                old_blob: None,
                new_blob: None,
            },
            // Modified (shrink): old=50, new=30 -> added=0, removed=20
            Change {
                path: PathBuf::from("c.txt"),
                kind: ChangeKind::Modified,
                old_size: Some(50),
                new_size: Some(30),
                old_blob: None,
                new_blob: None,
            },
            // Modified (grow): old=30, new=50 -> added=20, removed=0
            Change {
                path: PathBuf::from("d.txt"),
                kind: ChangeKind::Modified,
                old_size: Some(30),
                new_size: Some(50),
                old_blob: None,
                new_blob: None,
            },
        ];
        let out = render_stat(&changes);
        assert!(out.contains("a.txt | +10 -0"), "added: +new_size -0");
        assert!(out.contains("b.txt | +0 -20"), "deleted: +0 -old_size");
        assert!(out.contains("c.txt | +0 -20"), "shrinking mod: +0 -delta");
        assert!(out.contains("d.txt | +20 -0"), "growing mod: +delta -0");
        // totals: added=10+0+0+20=30, removed=0+20+20+0=40
        assert!(
            out.contains("4 files changed (+30 -40 bytes)"),
            "totals line must be correct, got: {}",
            out
        );
    }

    // (e) --stat empty changeset.
    #[test]
    fn render_stat_empty() {
        let out = render_stat(&[]);
        assert_eq!(out, "0 files changed (+0 -0 bytes)\n");
    }

    // (f) --json: parses back to array with correct fields.
    #[test]
    fn changeset_to_json_output() {
        let changes = vec![
            Change {
                path: PathBuf::from("a.txt"),
                kind: ChangeKind::Added,
                old_size: None,
                new_size: Some(5),
                old_blob: None,
                new_blob: None,
            },
            Change {
                path: PathBuf::from("b.txt"),
                kind: ChangeKind::Deleted,
                old_size: Some(12),
                new_size: None,
                old_blob: None,
                new_blob: None,
            },
        ];
        let out = changeset_to_json(&changes);
        let parsed: serde_json::Value =
            serde_json::from_str(&out).expect("changeset_to_json must produce valid JSON");
        let arr = parsed.as_array().expect("must be a JSON array");
        assert_eq!(arr.len(), 2, "must have two entries");

        assert_eq!(arr[0]["path"].as_str().unwrap(), "a.txt");
        assert_eq!(arr[0]["status"].as_str().unwrap(), "A");
        assert!(arr[0]["old_size"].is_null(), "Added: old_size must be null");
        assert_eq!(arr[0]["new_size"].as_u64().unwrap(), 5);

        assert_eq!(arr[1]["path"].as_str().unwrap(), "b.txt");
        assert_eq!(arr[1]["status"].as_str().unwrap(), "D");
        assert_eq!(arr[1]["old_size"].as_u64().unwrap(), 12);
        assert!(arr[1]["new_size"].is_null(), "Deleted: new_size must be null");
    }

    // (f) --json empty changeset produces an empty array.
    #[test]
    fn changeset_to_json_empty() {
        let out = changeset_to_json(&[]);
        let parsed: serde_json::Value =
            serde_json::from_str(&out).expect("must be valid JSON");
        assert_eq!(
            parsed.as_array().unwrap().len(),
            0,
            "empty changeset must produce []"
        );
    }
}
