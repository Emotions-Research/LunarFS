//! Pure, FFI-free seam types for the Windows ProjFS backend.
//!
//! This module is compiled on ALL targets (macOS, Linux, Windows) so it can be
//! unit-tested anywhere without pulling in windows-rs.  The windows backend
//! consumes `PlaceholderMeta` and converts it to the real `PRJ_PLACEHOLDER_INFO`.

use crate::acl::{decide, AclDecision, AclEntry, Permission};
use crate::cas::Store;
use crate::fuse::resolve::{resolve_attr, resolve_read, resolve_readdir, resolve_size};
use crate::fuse::translate::{Attributes, FsError, NodeKind};
use crate::fuse::{route_delete, route_write};
use crate::index::Index;
use crate::overlay::{AgentId, OverlayStore, WorkspaceId};

/// Windows FILE_ATTRIBUTE_DIRECTORY bitmask, reproduced as a plain u32.
///
/// Matches the Win32 constant; no windows-rs import required.
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// Windows FILE_ATTRIBUTE_NORMAL bitmask, reproduced as a plain u32.
///
/// Set on regular files that carry no other special attributes.
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

/// Pure, FFI-free description of a ProjFS placeholder assembled from CAS node
/// attributes.
///
/// Mirrors the fields `PrjWritePlaceholderInfo` ultimately needs, expressed as
/// plain Rust types with no windows-rs imports.  The windows backend converts
/// this struct into a real `PRJ_PLACEHOLDER_INFO` at call time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceholderMeta {
    /// True when the placeholder represents a directory rather than a file.
    pub is_directory: bool,
    /// Logical byte size of the file; zero for directories.
    pub file_size: u64,
    /// Windows `FILE_ATTRIBUTE_*` bitmask computed from node kind.
    ///
    /// Directories carry `FILE_ATTRIBUTE_DIRECTORY`; plain files carry
    /// `FILE_ATTRIBUTE_NORMAL`.
    pub file_attributes: u32,
}

/// Translate CAS node attributes into ProjFS placeholder metadata.
///
/// Invariant: `attr.size` must fit in a `LARGE_INTEGER` (signed i64).
/// Maps `NodeKind::Dir` to `is_directory = true` with `FILE_ATTRIBUTE_DIRECTORY`.
/// Maps `NodeKind::File` to `is_directory = false` with `FILE_ATTRIBUTE_NORMAL`
/// and `file_size` preserved verbatim from the input.
pub fn placeholder_from_attr(attr: &Attributes) -> PlaceholderMeta {
    assert!(
        attr.size <= i64::MAX as u64,
        "file size must fit in a LARGE_INTEGER (Windows i64): size={}", attr.size
    );
    match attr.kind {
        NodeKind::Dir => PlaceholderMeta {
            is_directory: true,
            file_size: 0,
            file_attributes: FILE_ATTRIBUTE_DIRECTORY,
        },
        NodeKind::File => PlaceholderMeta {
            is_directory: false,
            file_size: attr.size,
            file_attributes: FILE_ATTRIBUTE_NORMAL,
        },
    }
}

// ---------------------------------------------------------------------------
// Read-path translation: pure functions for the three ProjFS read callbacks.
//
// No ProjFS or windows-rs imports. All three compile and are tested on macOS.
// The cfg(windows) callback shim that calls into real ProjFS is in
// src/backend/windows.rs (gated, not compiled here).
// ---------------------------------------------------------------------------

/// A directory entry for ProjFS GetDirectoryEnumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjFsDirEntry {
    pub name: String,
    pub is_dir: bool,
    /// Logical byte size via resolve_size; zero for directories.
    pub size: u64,
}

/// File/directory attributes for ProjFS GetPlaceholderInfo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjFsAttrs {
    pub is_dir: bool,
    /// Logical byte size via resolve_size; zero for directories.
    pub size: u64,
}

/// Enumerate the direct children of `path` for ProjFS GetDirectoryEnumeration.
///
/// Returns entries in case-insensitive ascending order (ProjFS convention).
/// "." and ".." are excluded; ProjFS handles dot entries internally. File
/// entries carry the overlay-aware size from resolve_size. Directory entries
/// carry size zero. If resolve_size fails for an individual child (e.g. an
/// in-flight tombstone) the entry is listed with size zero rather than
/// aborting the whole enumeration, mirroring FUSE readdir behaviour.
///
/// Missing or not-a-directory path => Err(FsError::NotFound) or
/// Err(FsError::NotADir). ACL deny on the directory => Err(FsError::AccessDenied).
#[allow(clippy::too_many_arguments)]
pub fn enumerate_dir(
    index: &Index,
    store: &dyn Store,
    overlay: Option<&OverlayStore>,
    acl: &[AclEntry],
    agent: AgentId,
    principal: &str,
    now: i64,
    path: &str,
) -> Result<Vec<ProjFsDirEntry>, FsError> {
    assert!(path.len() <= 4096, "path must not exceed 4096 bytes");
    assert!(principal.len() <= 256, "principal must not exceed 256 bytes");

    let raw = resolve_readdir(index, overlay, acl, agent, principal, now, path)?;

    let mut out: Vec<ProjFsDirEntry> = Vec::with_capacity(raw.len());
    for entry in raw {
        if entry.name == "." || entry.name == ".." {
            continue;
        }
        let (is_dir, size) = if entry.kind == NodeKind::Dir {
            (true, 0u64)
        } else {
            let child_path = if path.is_empty() {
                entry.name.clone()
            } else {
                format!("{}/{}", path, entry.name)
            };
            // Use the overlay-aware size; fall back to zero on any resolution error
            // (consistent with FUSE readdir which does not check per-child ACL).
            let sz = resolve_size(index, store, overlay, acl, agent, principal, now, &child_path)
                .unwrap_or(0);
            (false, sz)
        };
        out.push(ProjFsDirEntry { name: entry.name, is_dir, size });
    }

    // ProjFS requires case-insensitive ascending order (Windows string comparison).
    out.sort_by_key(|e| e.name.to_lowercase());
    Ok(out)
}

/// Resolve attributes for ProjFS GetPlaceholderInfo.
///
/// The size is the REAL overlay-aware value from resolve_size: never a stale
/// cached value, never zero for non-empty files. This mirrors the FUSE-T
/// getattr fix where resolve_size replaced the stale index size.
///
/// Missing path => Err(FsError::NotFound); ACL deny => Err(FsError::AccessDenied).
#[allow(clippy::too_many_arguments)]
pub fn placeholder_info(
    index: &Index,
    store: &dyn Store,
    overlay: Option<&OverlayStore>,
    acl: &[AclEntry],
    agent: AgentId,
    principal: &str,
    now: i64,
    path: &str,
) -> Result<ProjFsAttrs, FsError> {
    assert!(path.len() <= 4096, "path must not exceed 4096 bytes");
    assert!(principal.len() <= 256, "principal must not exceed 256 bytes");

    let attrs = resolve_attr(index, overlay, acl, agent, principal, now, path)?;
    let size = resolve_size(index, store, overlay, acl, agent, principal, now, path)?;
    Ok(ProjFsAttrs { is_dir: attrs.kind == NodeKind::Dir, size })
}

/// Read a byte window from a file for ProjFS GetFileData.
///
/// The (offset, length) window is clamped to the actual file length:
/// - offset >= file length => empty Vec (not an error)
/// - offset + length > file length => available tail returned
/// - length == 0 => empty Vec
/// - never panics on out-of-range offset or length
///
/// Lazy hydration is triggered through the Store exactly as the FUSE read path
/// does. Missing path => Err(FsError::NotFound).
///
/// nyx: length is capped at 128 MiB per call to match route_read's assert;
/// ProjFS splits larger requests into multiple GetFileData callbacks anyway.
#[allow(clippy::too_many_arguments)]
pub fn read_file_data(
    index: &Index,
    store: &dyn Store,
    overlay: Option<&OverlayStore>,
    acl: &[AclEntry],
    agent: AgentId,
    principal: &str,
    now: i64,
    path: &str,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>, FsError> {
    assert!(path.len() <= 4096, "path must not exceed 4096 bytes");
    assert!(principal.len() <= 256, "principal must not exceed 256 bytes");

    if length == 0 {
        return Ok(Vec::new());
    }
    const MAX_READ: u64 = 128 * 1024 * 1024;
    let size_u32 = length.min(MAX_READ) as u32;
    resolve_read(index, store, overlay, acl, agent, principal, now, path, offset, size_u32)
}

// ---------------------------------------------------------------------------
// Write-path translation: pure functions for the ProjFS notification callback.
//
// No ProjFS or windows-rs imports. Both compile and are tested on macOS.
// The cfg(windows) notification callback shim is in src/backend/windows.rs.
// ---------------------------------------------------------------------------

/// Which overlay mutation a ProjFS notification maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjFsWriteOp {
    /// File created or modified: capture bytes in the overlay CAS.
    Write,
    /// File deleted: record a tombstone in the overlay.
    Delete,
}

/// Target-agnostic write-path core for the PRJ notification callback.
///
/// ACL is checked first via Permission::Write. A deny returns
/// Err(FsError::AccessDenied) and NO overlay mutation occurs. On allow:
/// Write calls route_write with the provided bytes; Delete calls route_delete
/// to record a tombstone. Reuses the shared fuse::route_write / route_delete
/// seams so behavior is identical across FUSE, macOS, and Windows transports.
#[allow(clippy::too_many_arguments)]
pub fn apply_write_notification(
    store: &dyn Store,
    overlay: &OverlayStore,
    acl: &[AclEntry],
    agent: AgentId,
    workspace: WorkspaceId,
    principal: &str,
    now: i64,
    path: &str,
    op: ProjFsWriteOp,
    bytes: Option<&[u8]>,
) -> Result<(), FsError> {
    assert!(path.len() <= 4096, "path must not exceed 4096 bytes: path={}", path);
    assert!(principal.len() <= 256, "principal must not exceed 256 bytes");

    if decide(acl, path, principal, now, Permission::Write) == AclDecision::Deny {
        return Err(FsError::AccessDenied);
    }

    match op {
        ProjFsWriteOp::Write => {
            let data = bytes.ok_or_else(|| {
                FsError::IoError("write notification missing bytes".into())
            })?;
            route_write(store, overlay, agent, workspace, path, data)
                .map(|_| ())
                .map_err(|e| FsError::IoError(e.to_string()))
        }
        ProjFsWriteOp::Delete => {
            route_delete(overlay, agent, workspace, path)
                .map_err(|e| FsError::IoError(e.to_string()))
        }
    }
}

/// Pure FsError to raw Win32 HRESULT bits. FFI-free and testable on macOS.
///
/// Values match the constants previously hard-coded in windows.rs
/// fs_err_to_hresult; that function now delegates here so both code paths
/// produce identical error codes.
pub fn fs_error_to_win32(e: &FsError) -> u32 {
    match e {
        FsError::NotFound     => 0x80070002, // ERROR_FILE_NOT_FOUND
        FsError::NotADir      => 0x80070003, // ERROR_PATH_NOT_FOUND
        FsError::IsADir       => 0x80070015, // ERROR_NOT_SUPPORTED (close mapping)
        FsError::AccessDenied => 0x80070005, // ERROR_ACCESS_DENIED
        FsError::IoError(_)   => 0x8007001F, // ERROR_GEN_FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuse::translate::{Attributes, NodeKind};

    #[test]
    fn placeholder_file_preserves_size_and_sets_normal_bit() {
        let attr = Attributes { kind: NodeKind::File, size: 1234, mode: 0o100644 };
        let meta = placeholder_from_attr(&attr);
        assert_eq!(meta.file_size, 1234, "file size must be preserved from input");
        assert!(!meta.is_directory, "file node must not be marked as directory");
        assert_ne!(
            meta.file_attributes & FILE_ATTRIBUTE_NORMAL,
            0,
            "file node must have FILE_ATTRIBUTE_NORMAL set"
        );
        assert_eq!(
            meta.file_attributes & FILE_ATTRIBUTE_DIRECTORY,
            0,
            "file node must not have FILE_ATTRIBUTE_DIRECTORY set"
        );
    }

    #[test]
    fn placeholder_dir_sets_directory_bit_and_zero_size() {
        let attr = Attributes { kind: NodeKind::Dir, size: 0, mode: 0o040755 };
        let meta = placeholder_from_attr(&attr);
        assert!(meta.is_directory, "directory node must be marked as directory");
        assert_eq!(meta.file_size, 0, "directory placeholder size must be zero");
        assert_ne!(
            meta.file_attributes & FILE_ATTRIBUTE_DIRECTORY,
            0,
            "directory node must have FILE_ATTRIBUTE_DIRECTORY set"
        );
        assert_eq!(
            meta.file_attributes & FILE_ATTRIBUTE_NORMAL,
            0,
            "directory node must not have FILE_ATTRIBUTE_NORMAL set"
        );
    }

    // -------------------------------------------------------------------------
    // Tests for the three pure read-path functions
    // -------------------------------------------------------------------------

    mod read_path_tests {
        use super::super::{enumerate_dir, placeholder_info, read_file_data};
        use crate::acl::AclEntry;
        use crate::cas::{MemStore, Store};
        use crate::fuse::translate::FsError;
        use crate::overlay::{OverlayStore, WorkspaceId};
        use crate::tree::{serialize_tree, TreeEntry, MODE_FILE};
        use rusqlite::Connection;

        const WS: WorkspaceId = 1;
        const PRINCIPAL: &str = "alice";
        const NOW: i64 = 1_000_000_000;

        fn no_acl() -> Vec<AclEntry> {
            Vec::new()
        }

        fn make_base(name: &str, content: &[u8]) -> (MemStore, crate::index::Index) {
            let store = MemStore::new();
            let file_hash = store.put(content).unwrap();
            let tree_bytes = serialize_tree(&[TreeEntry {
                mode: MODE_FILE,
                name: name.to_string(),
                hash: file_hash,
            }]);
            let root_hash = store.put(&tree_bytes).unwrap();
            let index = crate::index::Index::build(&store, &root_hash).unwrap();
            (store, index)
        }

        fn make_overlay() -> OverlayStore {
            let conn = Connection::open_in_memory().unwrap();
            let ov = OverlayStore::new(conn);
            ov.init_schema().unwrap();
            ov
        }

        fn empty_index() -> (MemStore, crate::index::Index) {
            let store = MemStore::new();
            let tree_bytes = serialize_tree(&[]);
            let root_hash = store.put(&tree_bytes).unwrap();
            let index = crate::index::Index::build(&store, &root_hash).unwrap();
            (store, index)
        }

        // --- placeholder_info -------------------------------------------------

        #[test]
        fn placeholder_info_returns_real_size_not_zero() {
            let content = b"hello world";
            let (store, index) = make_base("greet.txt", content);
            let overlay = make_overlay();
            let agent = overlay.fork(WS).unwrap();

            let attrs = placeholder_info(
                &index, &store, Some(&overlay), &no_acl(), agent, PRINCIPAL, NOW, "greet.txt",
            )
            .unwrap();
            assert!(!attrs.is_dir, "file must not be marked as directory");
            assert_eq!(attrs.size, content.len() as u64, "size must equal blob length");
            assert_ne!(attrs.size, 0, "size must not be zero for a non-empty file");
        }

        #[test]
        fn placeholder_info_overlay_size_wins_over_base() {
            let (store, index) = make_base("f.txt", b"hi");
            let overlay = make_overlay();
            let agent = overlay.fork(WS).unwrap();
            crate::fuse::route_write(&store, &overlay, agent, WS, "f.txt", b"longer content")
                .unwrap();

            let attrs = placeholder_info(
                &index, &store, Some(&overlay), &no_acl(), agent, PRINCIPAL, NOW, "f.txt",
            )
            .unwrap();
            assert_eq!(attrs.size, b"longer content".len() as u64, "overlay size must win");
            assert_ne!(attrs.size, b"hi".len() as u64, "must not return stale base size");
        }

        #[test]
        fn placeholder_info_directory_is_zero_size() {
            let (store, index) = make_base("sub/file.txt", b"data");
            let overlay = make_overlay();
            let agent = overlay.fork(WS).unwrap();

            let attrs = placeholder_info(
                &index, &store, Some(&overlay), &no_acl(), agent, PRINCIPAL, NOW, "sub",
            )
            .unwrap();
            assert!(attrs.is_dir, "sub must be recognized as a directory");
            assert_eq!(attrs.size, 0, "directory size must be zero");
        }

        #[test]
        fn placeholder_info_missing_is_not_found() {
            let (store, index) = empty_index();
            let overlay = make_overlay();
            let agent = overlay.fork(WS).unwrap();

            let err = placeholder_info(
                &index, &store, Some(&overlay), &no_acl(), agent, PRINCIPAL, NOW, "ghost.txt",
            )
            .unwrap_err();
            assert!(matches!(err, FsError::NotFound), "missing path must return NotFound");
        }

        // --- read_file_data ---------------------------------------------------

        #[test]
        fn read_file_data_full_read() {
            let content = b"abcdefgh";
            let (store, index) = make_base("file.dat", content);
            let overlay = make_overlay();
            let agent = overlay.fork(WS).unwrap();

            let got = read_file_data(
                &index, &store, Some(&overlay), &no_acl(), agent, PRINCIPAL, NOW,
                "file.dat", 0, content.len() as u64,
            )
            .unwrap();
            assert_eq!(got, content, "full read must return entire content");
        }

        #[test]
        fn read_file_data_mid_file_window() {
            let content = b"abcdefgh";
            let (store, index) = make_base("file.dat", content);
            let overlay = make_overlay();
            let agent = overlay.fork(WS).unwrap();

            let got = read_file_data(
                &index, &store, Some(&overlay), &no_acl(), agent, PRINCIPAL, NOW,
                "file.dat", 2, 3,
            )
            .unwrap();
            assert_eq!(got, b"cde", "mid-file window must return correct slice");
        }

        #[test]
        fn read_file_data_offset_plus_length_past_eof() {
            let content = b"abcde";
            let (store, index) = make_base("file.dat", content);
            let overlay = make_overlay();
            let agent = overlay.fork(WS).unwrap();

            let got = read_file_data(
                &index, &store, Some(&overlay), &no_acl(), agent, PRINCIPAL, NOW,
                "file.dat", 3, 100,
            )
            .unwrap();
            assert_eq!(got, b"de", "past-EOF window must be clamped to available tail");
        }

        #[test]
        fn read_file_data_offset_past_eof_returns_empty() {
            let content = b"abc";
            let (store, index) = make_base("file.dat", content);
            let overlay = make_overlay();
            let agent = overlay.fork(WS).unwrap();

            let got = read_file_data(
                &index, &store, Some(&overlay), &no_acl(), agent, PRINCIPAL, NOW,
                "file.dat", 10, 5,
            )
            .unwrap();
            assert!(got.is_empty(), "offset past EOF must return empty slice");
        }

        #[test]
        fn read_file_data_zero_length_returns_empty() {
            let content = b"abc";
            let (store, index) = make_base("file.dat", content);
            let overlay = make_overlay();
            let agent = overlay.fork(WS).unwrap();

            let got = read_file_data(
                &index, &store, Some(&overlay), &no_acl(), agent, PRINCIPAL, NOW,
                "file.dat", 0, 0,
            )
            .unwrap();
            assert!(got.is_empty(), "zero-length request must return empty slice");
        }

        #[test]
        fn read_file_data_missing_is_not_found() {
            let (store, index) = empty_index();
            let overlay = make_overlay();
            let agent = overlay.fork(WS).unwrap();

            let err = read_file_data(
                &index, &store, Some(&overlay), &no_acl(), agent, PRINCIPAL, NOW,
                "ghost.txt", 0, 10,
            )
            .unwrap_err();
            assert!(matches!(err, FsError::NotFound), "missing path must return NotFound");
        }

        // --- enumerate_dir ----------------------------------------------------

        #[test]
        fn enumerate_dir_returns_sorted_entries_with_real_sizes() {
            let store = MemStore::new();
            let h_b = store.put(b"beta").unwrap();
            let h_a = store.put(b"alpha").unwrap();
            let h_g = store.put(b"gamma").unwrap();
            let tree_bytes = serialize_tree(&[
                TreeEntry { mode: MODE_FILE, name: "beta.txt".to_string(), hash: h_b },
                TreeEntry { mode: MODE_FILE, name: "alpha.txt".to_string(), hash: h_a },
                TreeEntry { mode: MODE_FILE, name: "gamma.txt".to_string(), hash: h_g },
            ]);
            let root_hash = store.put(&tree_bytes).unwrap();
            let index = crate::index::Index::build(&store, &root_hash).unwrap();
            let overlay = make_overlay();
            let agent = overlay.fork(WS).unwrap();

            let entries = enumerate_dir(
                &index, &store, Some(&overlay), &no_acl(), agent, PRINCIPAL, NOW, "",
            )
            .unwrap();

            assert!(!entries.iter().any(|e| e.name == "." || e.name == ".."),
                "dot entries must be excluded");

            let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, vec!["alpha.txt", "beta.txt", "gamma.txt"],
                "entries must be in case-insensitive ascending order");

            // Sizes come from resolve_size (real blob lengths).
            assert_eq!(entries[0].size, b"alpha".len() as u64, "alpha size must equal blob length");
            assert_eq!(entries[1].size, b"beta".len() as u64, "beta size must equal blob length");
            assert_eq!(entries[2].size, b"gamma".len() as u64, "gamma size must equal blob length");
        }

        #[test]
        fn enumerate_dir_file_sizes_reflect_overlay() {
            let (store, index) = make_base("f.txt", b"base content");
            let overlay = make_overlay();
            let agent = overlay.fork(WS).unwrap();
            crate::fuse::route_write(&store, &overlay, agent, WS, "f.txt", b"different length")
                .unwrap();

            let entries = enumerate_dir(
                &index, &store, Some(&overlay), &no_acl(), agent, PRINCIPAL, NOW, "",
            )
            .unwrap();

            let f = entries.iter().find(|e| e.name == "f.txt").unwrap();
            assert_eq!(f.size, b"different length".len() as u64,
                "enumerate_dir must report overlay size, not stale base size");
            assert_ne!(f.size, b"base content".len() as u64);
        }

        #[test]
        fn enumerate_dir_missing_path_is_not_found() {
            let (store, index) = make_base("a.txt", b"data");
            let overlay = make_overlay();
            let agent = overlay.fork(WS).unwrap();

            let err = enumerate_dir(
                &index, &store, Some(&overlay), &no_acl(), agent, PRINCIPAL, NOW, "no_such_dir",
            )
            .unwrap_err();
            assert!(matches!(err, FsError::NotFound), "missing dir must return NotFound");
        }

        #[test]
        fn enumerate_dir_on_file_is_not_a_dir() {
            let (store, index) = make_base("file.rs", b"code");
            let overlay = make_overlay();
            let agent = overlay.fork(WS).unwrap();

            let err = enumerate_dir(
                &index, &store, Some(&overlay), &no_acl(), agent, PRINCIPAL, NOW, "file.rs",
            )
            .unwrap_err();
            assert!(matches!(err, FsError::NotADir), "calling on a file must return NotADir");
        }

        #[test]
        fn enumerate_dir_subdirectory() {
            use crate::tree::{MODE_DIR, TreeEntry as TE};
            let store2 = MemStore::new();
            let h2 = store2.put(b"content").unwrap();
            let sub_tree = {
                let bytes = serialize_tree(&[TE {
                    mode: MODE_FILE,
                    name: "a.txt".to_string(),
                    hash: h2,
                }]);
                store2.put(&bytes).unwrap()
            };
            let root_tree = {
                let bytes = serialize_tree(&[TE {
                    mode: MODE_DIR,
                    name: "sub".to_string(),
                    hash: sub_tree,
                }]);
                store2.put(&bytes).unwrap()
            };
            let index = crate::index::Index::build(&store2, &root_tree).unwrap();
            let overlay = make_overlay();
            let agent = overlay.fork(WS).unwrap();

            let entries = enumerate_dir(
                &index, &store2, Some(&overlay), &no_acl(), agent, PRINCIPAL, NOW, "sub",
            )
            .unwrap();

            assert_eq!(entries.len(), 1, "sub dir must have exactly one child");
            assert_eq!(entries[0].name, "a.txt");
            assert!(!entries[0].is_dir);
            assert_eq!(entries[0].size, b"content".len() as u64);
        }
    }

    mod write_path_tests {
        use super::super::{
            apply_write_notification, fs_error_to_win32, placeholder_info, read_file_data,
            ProjFsWriteOp,
        };
        use crate::acl::{AclEntry, Permission};
        use crate::cas::{MemStore, Store};
        use crate::fuse::translate::FsError;
        use crate::overlay::{OverlayStore, Resolution, WorkspaceId};
        use crate::tree::{serialize_tree, TreeEntry, MODE_FILE};
        use rusqlite::Connection;

        const WS: WorkspaceId = 1;
        const PRINCIPAL: &str = "alice";
        const NOW: i64 = 1_000_000_000;

        fn no_acl() -> Vec<AclEntry> {
            Vec::new()
        }

        fn deny_acl(prefix: &str) -> Vec<AclEntry> {
            vec![AclEntry {
                path_prefix: prefix.to_string(),
                principal: "*".to_string(),
                permission: Permission::Deny,
                expires_at: None,
            }]
        }

        fn make_base(name: &str, content: &[u8]) -> (MemStore, crate::index::Index) {
            let store = MemStore::new();
            let file_hash = store.put(content).unwrap();
            let tree_bytes = serialize_tree(&[TreeEntry {
                mode: MODE_FILE,
                name: name.to_string(),
                hash: file_hash,
            }]);
            let root_hash = store.put(&tree_bytes).unwrap();
            let index = crate::index::Index::build(&store, &root_hash).unwrap();
            (store, index)
        }

        fn make_overlay() -> OverlayStore {
            let conn = Connection::open_in_memory().unwrap();
            let ov = OverlayStore::new(conn);
            ov.init_schema().unwrap();
            ov
        }

        // (a) write capture then read-back: captured bytes replace base content.
        #[test]
        fn write_capture_then_readback() {
            let (store, index) = make_base("f.txt", b"base");
            let overlay = make_overlay();
            let agent = overlay.fork(WS).unwrap();

            let result = apply_write_notification(
                &store, &overlay, &no_acl(), agent, WS, PRINCIPAL, NOW,
                "f.txt", ProjFsWriteOp::Write, Some(b"captured-bytes"),
            );
            assert!(result.is_ok(), "write capture must succeed with no ACL restriction");

            let got = read_file_data(
                &index, &store, Some(&overlay), &no_acl(), agent, PRINCIPAL, NOW,
                "f.txt", 0, 1024,
            )
            .unwrap();
            assert_eq!(got, b"captured-bytes",
                "read-back must return captured bytes, not stale base content");
            assert_ne!(got, b"base",
                "stale base content must be shadowed by the overlay write");

            let attrs = placeholder_info(
                &index, &store, Some(&overlay), &no_acl(), agent, PRINCIPAL, NOW, "f.txt",
            )
            .unwrap();
            assert_eq!(
                attrs.size,
                b"captured-bytes".len() as u64,
                "placeholder_info size must equal the captured byte length, not base size"
            );
        }

        // (b) tombstone hides the base file for both read and stat.
        #[test]
        fn delete_tombstone_hides_base() {
            let (store, index) = make_base("del.txt", b"bye");
            let overlay = make_overlay();
            let agent = overlay.fork(WS).unwrap();

            let result = apply_write_notification(
                &store, &overlay, &no_acl(), agent, WS, PRINCIPAL, NOW,
                "del.txt", ProjFsWriteOp::Delete, None,
            );
            assert!(result.is_ok(), "tombstone delete must succeed with no ACL restriction");

            let read_err = read_file_data(
                &index, &store, Some(&overlay), &no_acl(), agent, PRINCIPAL, NOW,
                "del.txt", 0, 10,
            )
            .unwrap_err();
            assert!(
                matches!(read_err, FsError::NotFound),
                "tombstoned file must return NotFound on read; got {:?}", read_err
            );

            let attr_err = placeholder_info(
                &index, &store, Some(&overlay), &no_acl(), agent, PRINCIPAL, NOW, "del.txt",
            )
            .unwrap_err();
            assert!(
                matches!(attr_err, FsError::NotFound),
                "tombstoned file must return NotFound on placeholder_info; got {:?}", attr_err
            );
        }

        // (c) ACL deny blocks the write and leaves the overlay row absent.
        #[test]
        fn acl_deny_blocks_write() {
            let (store, _index) = make_base("secret.txt", b"private");
            let overlay = make_overlay();
            let agent = overlay.fork(WS).unwrap();

            let result = apply_write_notification(
                &store, &overlay, &deny_acl("secret.txt"), agent, WS, PRINCIPAL, NOW,
                "secret.txt", ProjFsWriteOp::Write, Some(b"x"),
            );
            assert!(
                matches!(result, Err(FsError::AccessDenied)),
                "ACL deny must return AccessDenied; got {:?}", result
            );

            let resolution = overlay.resolve(agent, "secret.txt").unwrap();
            assert!(
                matches!(resolution, Resolution::Base),
                "denied write must not mutate the overlay: resolve must be Base"
            );

            assert_eq!(
                fs_error_to_win32(&FsError::AccessDenied),
                0x80070005,
                "AccessDenied must map to Win32 ERROR_ACCESS_DENIED 0x80070005"
            );
        }

        // (d) empty ACL allows the write and bytes are readable immediately.
        #[test]
        fn acl_allow_passes_through() {
            let (store, index) = make_base("data.txt", b"old");
            let overlay = make_overlay();
            let agent = overlay.fork(WS).unwrap();

            let result = apply_write_notification(
                &store, &overlay, &no_acl(), agent, WS, PRINCIPAL, NOW,
                "data.txt", ProjFsWriteOp::Write, Some(b"ok"),
            );
            assert!(result.is_ok(), "empty ACL must allow the write through");

            let got = read_file_data(
                &index, &store, Some(&overlay), &no_acl(), agent, PRINCIPAL, NOW,
                "data.txt", 0, 1024,
            )
            .unwrap();
            assert_eq!(got, b"ok",
                "captured bytes must be returned on read with no ACL restriction");
        }

        // Win32 error-code mapping: all five variants must match the documented codes.
        #[test]
        fn fs_error_to_win32_mappings() {
            assert_eq!(fs_error_to_win32(&FsError::NotFound), 0x80070002,
                "NotFound must map to ERROR_FILE_NOT_FOUND 0x80070002");
            assert_eq!(fs_error_to_win32(&FsError::NotADir), 0x80070003,
                "NotADir must map to ERROR_PATH_NOT_FOUND 0x80070003");
            assert_eq!(fs_error_to_win32(&FsError::IsADir), 0x80070015,
                "IsADir must map to ERROR_NOT_SUPPORTED 0x80070015");
            assert_eq!(fs_error_to_win32(&FsError::AccessDenied), 0x80070005,
                "AccessDenied must map to ERROR_ACCESS_DENIED 0x80070005");
            assert_eq!(fs_error_to_win32(&FsError::IoError("x".into())), 0x8007001F,
                "IoError must map to ERROR_GEN_FAILURE 0x8007001F");
        }
    }
}
