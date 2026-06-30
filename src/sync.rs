use std::collections::HashSet;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use futures::stream::{self, StreamExt, TryStreamExt};

use crate::cas::{hash_bytes, hash_to_hex, Hash, Store};
use crate::index::Index;
use crate::remote::{HttpRemote, Remote};
use crate::tree::{deserialize_tree, MODE_DIR};

pub enum SyncOutcome {
    Empty,
    Unchanged,
    Updated { root: Hash, files: usize },
}

pub struct SyncDaemon {
    remote: Arc<Remote>,
    last_seen: Mutex<Option<Hash>>,
    state: Arc<RwLock<Option<Index>>>,
}

impl SyncDaemon {
    pub fn new(remote: Arc<Remote>) -> Self {
        Self {
            remote,
            last_seen: Mutex::new(None),
            state: Arc::new(RwLock::new(None)),
        }
    }

    pub fn poll_once(&self) -> io::Result<SyncOutcome> {
        let head = self
            .remote
            .read_head()
            .map_err(|e| io::Error::other(e.to_string()))?;

        let ptr = match head {
            None => return Ok(SyncOutcome::Empty),
            Some(p) => p,
        };

        let mut last = self.last_seen.lock().expect("last_seen lock poisoned");
        if *last == Some(ptr.root) {
            return Ok(SyncOutcome::Unchanged);
        }

        let index = Index::build(&*self.remote, &ptr.root)?;
        let files = index.len();
        let root = ptr.root;
        *last = Some(root);
        *self.state.write().expect("state lock poisoned") = Some(index);
        Ok(SyncOutcome::Updated { root, files })
    }

    pub fn current_root(&self) -> Option<Hash> {
        *self.last_seen.lock().expect("last_seen lock poisoned")
    }

    pub fn with_index<R>(&self, f: impl FnOnce(Option<&Index>) -> R) -> R {
        let guard = self.state.read().expect("state lock poisoned");
        f(guard.as_ref())
    }

    /// Loop: poll HEAD, sleep `interval`, break when `stop` becomes true.
    /// Per-iteration errors are logged to stderr rather than propagated.
    pub fn run(&self, interval: Duration, stop: &AtomicBool) {
        const MAX_POLLS: usize = 10_000_000;
        for _ in 0..MAX_POLLS {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            if let Err(e) = self.poll_once() {
                eprintln!("sync daemon: poll error: {}", e);
            }
            std::thread::sleep(interval);
            if stop.load(Ordering::Relaxed) {
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Concurrency knob: LUNAR_TRANSFER_CONCURRENCY env var, default 24.
// 0 or unparseable falls back to 24 (buffer_unordered(0) would stall).
// ---------------------------------------------------------------------------

fn transfer_concurrency() -> usize {
    std::env::var("LUNAR_TRANSFER_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(24)
}

// ---------------------------------------------------------------------------
// Push: upload reachable blobs and publish the workspace ref.
// ---------------------------------------------------------------------------

/// Result from a CAS-aware push that the caller can inspect before acting on.
pub struct PushResult {
    pub uploaded: usize,
    pub outcome: crate::remote::CasRefOutcome,
}

/// Upload missing blobs for `root` and CAS-advance the workspace ref against
/// `expected_root`. Returns `PushResult` so the caller can inspect the outcome
/// and decide whether to retry (on `Conflict`) or accept (on `Committed`).
pub async fn push_cas(
    local: &dyn Store,
    root: &Hash,
    remote: &HttpRemote,
    workspace: &str,
    expected_root: Option<&Hash>,
) -> anyhow::Result<PushResult> {
    assert!(!workspace.is_empty(), "workspace must not be empty");

    let all_hashes = collect_all_hashes(local, root)?;
    let missing = remote.missing_blobs(&all_hashes, Some(workspace)).await?;
    let upload_count = missing.len();
    let concurrency = transfer_concurrency();

    stream::iter(missing.into_iter())
        .map(|hash| async move {
            let data = local
                .get(&hash)
                .map_err(|e| anyhow::anyhow!("read local store for {}: {}", hash_to_hex(&hash), e))?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "blob {} listed in tree but absent from local store",
                        hash_to_hex(&hash)
                    )
                })?;
            remote.put_blob(&hash, data, Some(workspace)).await?;
            Ok::<_, anyhow::Error>(())
        })
        .buffer_unordered(concurrency)
        .try_collect::<()>()
        .await?;

    let outcome = remote
        .put_ref_cas_outcome(workspace, root, expected_root)
        .await?;
    Ok(PushResult {
        uploaded: upload_count,
        outcome,
    })
}

/// Upload reachable blobs and publish the workspace ref.
/// Returns the number of blobs uploaded (0 on a full dedup hit).
///
/// Fetches expected_root from the server at push time (closest approximation to
/// last-synced root without a local state file). Delegates to `push_cas` and maps
/// `Conflict` to the same human-readable bail as before so `lunar push` is unchanged.
pub async fn push(
    local: &dyn Store,
    root: &Hash,
    remote: &HttpRemote,
    workspace: &str,
) -> anyhow::Result<usize> {
    assert!(!workspace.is_empty(), "workspace must not be empty");
    let expected_root: Option<Hash> = remote.get_ref(workspace).await.ok();
    let result = push_cas(local, root, remote, workspace, expected_root.as_ref()).await?;
    match result.outcome {
        crate::remote::CasRefOutcome::Committed => Ok(result.uploaded),
        crate::remote::CasRefOutcome::Conflict {
            conflict_ref,
            current_root,
        } => {
            anyhow::bail!(
                "push rejected: the server's root for workspace '{}' has changed since your last sync.\n\
                 Your push was saved as conflict ref '{}'.\n\
                 The server's current root is '{}'.\n\
                 Pull/merge and push again.",
                workspace,
                conflict_ref,
                hash_to_hex(&current_root)
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Pull: fetch all reachable blobs into the local store and return the root.
// ---------------------------------------------------------------------------

pub async fn pull(remote: &HttpRemote, workspace: &str, local: &dyn Store) -> anyhow::Result<Hash> {
    assert!(!workspace.is_empty(), "workspace must not be empty");

    let root = remote.get_ref(workspace).await?;
    fetch_tree_blobs(remote, local, &root, workspace).await?;
    Ok(root)
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Collect every blob hash reachable from `root` (the root tree itself, all
/// subtree blobs, and all file blobs). Iterative; bounded by MAX_TREE_NODES.
fn collect_all_hashes(store: &dyn Store, root: &Hash) -> anyhow::Result<Vec<Hash>> {
    const MAX_TREE_NODES: usize = 65_536;
    let mut all: Vec<Hash> = Vec::new();
    let mut visited: HashSet<Hash> = HashSet::new();
    // Stack entries: (hash, is_tree). Root is always a tree.
    let mut stack: Vec<(Hash, bool)> = vec![(*root, true)];
    let mut node_count = 0usize;

    while let Some((hash, is_tree)) = stack.pop() {
        if visited.contains(&hash) {
            continue;
        }
        node_count += 1;
        anyhow::ensure!(
            node_count <= MAX_TREE_NODES,
            "tree DAG exceeds {} nodes",
            MAX_TREE_NODES
        );
        visited.insert(hash);
        all.push(hash);

        if !is_tree {
            continue;
        }

        let blob = store
            .get(&hash)
            .map_err(|e| anyhow::anyhow!("read tree blob {}: {}", hash_to_hex(&hash), e))?
            .ok_or_else(|| {
                anyhow::anyhow!("tree blob {} not found in local store", hash_to_hex(&hash))
            })?;

        let entries = deserialize_tree(&blob)
            .map_err(|e| anyhow::anyhow!("deserialize tree {}: {}", hash_to_hex(&hash), e))?;
        for entry in entries {
            let child_is_tree = entry.mode == MODE_DIR;
            stack.push((entry.hash, child_is_tree));
        }
    }

    Ok(all)
}

/// Fetch every blob reachable from `root` from the remote into `local`.
/// Skips blobs already present in `local`. Verifies content integrity on fetch.
///
/// Phase 1: DFS tree walk, fetching tree blobs serially (each tree must be
/// parsed to discover its children). File blobs absent from local are queued.
/// Phase 2: All queued file blobs are fetched concurrently via buffer_unordered.
async fn fetch_tree_blobs(
    remote: &HttpRemote,
    local: &dyn Store,
    root: &Hash,
    workspace: &str,
) -> anyhow::Result<()> {
    const MAX_TREE_NODES: usize = 65_536;
    let concurrency = transfer_concurrency();
    let mut visited: HashSet<Hash> = HashSet::new();
    let mut stack: Vec<(Hash, bool)> = vec![(*root, true)];
    let mut node_count = 0usize;
    let mut missing_files: Vec<Hash> = Vec::new();

    while let Some((hash, is_tree)) = stack.pop() {
        if visited.contains(&hash) {
            continue;
        }
        node_count += 1;
        anyhow::ensure!(
            node_count <= MAX_TREE_NODES,
            "remote tree DAG exceeds {} nodes",
            MAX_TREE_NODES
        );
        visited.insert(hash);

        let data = if local.has(&hash) {
            local
                .get(&hash)
                .map_err(|e| anyhow::anyhow!("read local blob {}: {}", hash_to_hex(&hash), e))?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "has() true but get() returned None for {}",
                        hash_to_hex(&hash)
                    )
                })?
        } else if is_tree {
            // Tree blob: fetch serially so we can parse children immediately.
            let fetched = remote.get_blob(&hash, Some(workspace)).await?;
            let computed = hash_bytes(&fetched);
            anyhow::ensure!(
                computed == hash,
                "hash mismatch fetching {}: server returned data hashing to {}",
                hash_to_hex(&hash),
                hash_to_hex(&computed)
            );
            local
                .put(&fetched)
                .map_err(|e| anyhow::anyhow!("store blob {}: {}", hash_to_hex(&hash), e))?;
            fetched
        } else {
            // File blob not in local: queue for parallel fetch in phase 2.
            missing_files.push(hash);
            continue;
        };

        if is_tree {
            let entries = deserialize_tree(&data)
                .map_err(|e| anyhow::anyhow!("deserialize tree {}: {}", hash_to_hex(&hash), e))?;
            for entry in entries {
                stack.push((entry.hash, entry.mode == MODE_DIR));
            }
        }
    }

    // Phase 2: fetch missing file blobs concurrently; write each to local as it arrives.
    stream::iter(missing_files.into_iter())
        .map(|hash| async move {
            let fetched = remote.get_blob(&hash, Some(workspace)).await?;
            let computed = hash_bytes(&fetched);
            anyhow::ensure!(
                computed == hash,
                "hash mismatch fetching {}: server returned data hashing to {}",
                hash_to_hex(&hash),
                hash_to_hex(&computed)
            );
            local
                .put(&fetched)
                .map_err(|e| anyhow::anyhow!("store blob {}: {}", hash_to_hex(&hash), e))?;
            Ok::<_, anyhow::Error>(())
        })
        .buffer_unordered(concurrency)
        .try_collect::<()>()
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::Store;
    use crate::remote::Remote;
    use crate::tree::{serialize_tree, TreeEntry, MODE_FILE};

    fn remote_with_flat_tree(workspace: &str, blobs: &[(&str, &[u8])]) -> (Arc<Remote>, Hash) {
        let remote = Arc::new(Remote::in_memory(workspace));
        let entries: Vec<TreeEntry> = blobs
            .iter()
            .map(|(name, data)| {
                let hash = remote.put(data).unwrap();
                TreeEntry {
                    mode: MODE_FILE,
                    name: name.to_string(),
                    hash,
                }
            })
            .collect();
        let tree_bytes = serialize_tree(&entries);
        let root = remote.put(&tree_bytes).unwrap();
        remote.init_head(&root).unwrap();
        (remote, root)
    }

    #[test]
    fn poll_once_empty_when_no_head() {
        let remote = Arc::new(Remote::in_memory("s1"));
        let daemon = SyncDaemon::new(remote);
        assert!(matches!(daemon.poll_once().unwrap(), SyncOutcome::Empty));
    }

    #[test]
    fn poll_once_updated_on_first_head() {
        let (remote, root) = remote_with_flat_tree("s2", &[("a.txt", b"a"), ("b.txt", b"b")]);
        let daemon = SyncDaemon::new(remote);

        let out = daemon.poll_once().unwrap();
        assert!(
            matches!(&out, SyncOutcome::Updated { root: r, files: 2 } if *r == root),
            "first poll must be Updated with 2 files"
        );
        assert_eq!(daemon.current_root(), Some(root));
        daemon.with_index(|idx| {
            let idx = idx.expect("index must be present");
            assert!(idx.lookup("a.txt").is_some());
            assert!(idx.lookup("b.txt").is_some());
        });
    }

    #[test]
    fn poll_once_unchanged_when_head_stable() {
        let (remote, _) = remote_with_flat_tree("s3", &[("x.txt", b"x")]);
        let daemon = SyncDaemon::new(remote);
        daemon.poll_once().unwrap();
        assert!(matches!(
            daemon.poll_once().unwrap(),
            SyncOutcome::Unchanged
        ));
    }

    #[test]
    fn poll_once_updated_after_head_advances() {
        let (remote, _root1) = remote_with_flat_tree("s4", &[("a.txt", b"a"), ("b.txt", b"b")]);
        let daemon = SyncDaemon::new(Arc::clone(&remote));

        daemon.poll_once().unwrap();

        // Advance HEAD to a new tree
        let h_c = remote.put(b"c").unwrap();
        let h_d = remote.put(b"d").unwrap();
        let tree2 = serialize_tree(&[
            TreeEntry {
                mode: MODE_FILE,
                name: "c.txt".into(),
                hash: h_c,
            },
            TreeEntry {
                mode: MODE_FILE,
                name: "d.txt".into(),
                hash: h_d,
            },
        ]);
        let root2 = remote.put(&tree2).unwrap();
        let ptr = remote.read_head().unwrap().expect("HEAD must exist");
        remote.update_head(&root2, &ptr.version).unwrap();

        let out = daemon.poll_once().unwrap();
        assert!(
            matches!(&out, SyncOutcome::Updated { files: 2, .. }),
            "poll after head advance must be Updated"
        );
        daemon.with_index(|idx| {
            let idx = idx.expect("index must be present");
            assert!(idx.lookup("c.txt").is_some());
            assert!(idx.lookup("d.txt").is_some());
            assert!(idx.lookup("a.txt").is_none(), "old files must be gone");
        });
    }

    #[test]
    fn poll_once_empty_tree_yields_updated_zero_files() {
        let remote = Arc::new(Remote::in_memory("s5"));
        let empty_tree = serialize_tree(&[]);
        let root = remote.put(&empty_tree).unwrap();
        remote.init_head(&root).unwrap();

        let daemon = SyncDaemon::new(remote);
        let out = daemon.poll_once().unwrap();
        assert!(
            matches!(&out, SyncOutcome::Updated { files: 0, .. }),
            "empty tree must be Updated with 0 files"
        );
        daemon.with_index(|idx| assert!(idx.expect("index present").is_empty()));
    }

    #[test]
    fn run_stops_on_flag() {
        let remote = Arc::new(Remote::in_memory("s6"));
        let daemon = Arc::new(SyncDaemon::new(Arc::clone(&remote)));
        let stop = Arc::new(AtomicBool::new(false));

        let daemon_t = Arc::clone(&daemon);
        let stop_t = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            daemon_t.run(Duration::from_millis(5), &stop_t);
        });

        std::thread::sleep(Duration::from_millis(50));
        stop.store(true, Ordering::Relaxed);
        handle.join().expect("daemon thread must join");
    }
}
