//! Overlay-aware IndexSeam for the NFS layer.
//!
//! Merges overlay-written files and tombstoned (deleted) paths with the
//! immutable base Index so translate::read_dir reflects current overlay state.
//! Only files are surfaced here. Empty directories created via mkdir are
//! tracked in IdTable and surfaced separately in CasNfs::readdir.

use std::collections::{HashMap, HashSet};

use crate::fuse::translate::{IndexSeam, NodeKind, NodeMeta};
use crate::index::Index;

pub struct OverlayView<'a> {
    index: &'a Index,
    /// path -> byte size for files written by this agent.
    written: HashMap<String, u64>,
    tombstoned: HashSet<String>,
}

impl<'a> OverlayView<'a> {
    pub fn new(
        index: &'a Index,
        written: HashMap<String, u64>,
        tombstoned: HashSet<String>,
    ) -> Self {
        assert!(written.len() < 1_000_000, "overlay written map must not exceed cap");
        Self { index, written, tombstoned }
    }
}

impl IndexSeam for OverlayView<'_> {
    /// Returns NodeMeta for a file path, or None for tombstoned or absent paths.
    /// Never returns Some for directory paths (dirs are inferred by translate).
    fn lookup(&self, path: &str) -> Option<NodeMeta> {
        assert!(path.len() <= 4096, "path must not exceed 4096 bytes");
        if self.tombstoned.contains(path) {
            return None;
        }
        if let Some(&size) = self.written.get(path) {
            return Some(NodeMeta { kind: NodeKind::File, size, mode: 0o100644, hash: None });
        }
        let (hash, size) = self.index.lookup_entry(path)?;
        Some(NodeMeta { kind: NodeKind::File, size, mode: 0o100644, hash: Some(hash) })
    }

    /// Returns all file paths visible to this agent: (base - tombstoned) union (overlay written).
    fn file_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self
            .index
            .entries()
            .map(|(p, _)| p.to_owned())
            .filter(|p| !self.tombstoned.contains(p))
            .collect();
        for p in self.written.keys() {
            if !self.tombstoned.contains(p) && self.index.lookup(p).is_none() {
                paths.push(p.clone());
            }
        }
        paths
    }
}
