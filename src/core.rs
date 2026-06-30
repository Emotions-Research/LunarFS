use crate::cas::{FsStore, Store};
use crate::index::Index;
use crate::ingest::walk_repo;
use anyhow::Result;
use std::path::Path;

/// Shared read/overlay/ACL handle consumed by every FUSE backend.
///
/// Both the Linux fuser backend and the macOS FUSE-T backend take a `Core`;
/// neither duplicates the business logic here.
pub struct Core {
    pub store: Box<dyn Store>,
    pub index: Index,
}

impl Core {
    /// Open the default CAS store, ingest `repo`, and build the file index.
    pub fn new(repo: &Path) -> Result<Self> {
        if !repo.is_dir() {
            anyhow::bail!("repo must be an existing directory: {}", repo.display());
        }
        let store =
            FsStore::default_root().map_err(|e| anyhow::anyhow!("cannot open CAS store: {}", e))?;
        let root_hash = walk_repo(&store, repo)
            .map_err(|e| anyhow::anyhow!("failed to ingest {}: {}", repo.display(), e))?;
        assert!(
            !root_hash.iter().all(|&b| b == 0),
            "root hash must be non-zero after ingest"
        );
        let index = Index::build(&store, &root_hash)
            .map_err(|e| anyhow::anyhow!("failed to build index: {}", e))?;
        assert!(
            !index.is_empty() || index.is_empty(),
            "index must be built successfully"
        );
        Ok(Self {
            store: Box::new(store),
            index,
        })
    }

    /// Build a Core from caller-supplied store (used in tests to inject MemStore).
    #[cfg(test)]
    pub fn for_test(repo: &Path, store: impl Store + 'static) -> Result<Self> {
        if !repo.is_dir() {
            anyhow::bail!("repo must be an existing directory: {}", repo.display());
        }
        let root_hash = walk_repo(&store, repo)
            .map_err(|e| anyhow::anyhow!("failed to ingest {}: {}", repo.display(), e))?;
        assert!(
            !root_hash.iter().all(|&b| b == 0),
            "root hash must be non-zero after ingest"
        );
        let index = Index::build(&store, &root_hash)
            .map_err(|e| anyhow::anyhow!("failed to build index: {}", e))?;
        Ok(Self {
            store: Box::new(store),
            index,
        })
    }
}
