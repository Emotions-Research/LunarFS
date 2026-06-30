//! Resolution helpers composing ACL and overlay logic for the FUSE layer.
//!
//! Ungated: compiles under plain `cargo test` so the deterministic scenario
//! tests run without the `fuse` feature or a real mount.
//!
//! The macOS C shims (fuse/ops.rs) delegate entirely to these functions.
//! The Linux fuser backend (fs.rs) already uses the same underlying modules
//! (acl::decide, fuse::route_read, fuse::is_tombstoned) directly, so this
//! module enforces identical ACL-before-tombstone ordering.

use crate::acl::{decide, AclDecision, AclEntry, Permission};
use crate::cas::{hex_to_hash, Store};
use crate::fuse::translate::{
    attr_for, read_dir, Attributes, DirEntry, FsError, IndexSeam, NodeKind, NodeMeta,
};
use crate::index::Index;
use crate::overlay::{AgentId, OverlayStore, Resolution};

// ---------------------------------------------------------------------------
// IndexSeam impl for Index
//
// Bridges the concrete Index type into the seam trait so ungated tests (and
// resolve_attr / resolve_readdir callers) can pass &Index as &dyn IndexSeam
// without depending on the gated FsData defined in ops.rs.
// ---------------------------------------------------------------------------

impl IndexSeam for Index {
    fn lookup(&self, path: &str) -> Option<NodeMeta> {
        self.lookup_entry(path).map(|(hash, size)| NodeMeta {
            kind: NodeKind::File,
            size,
            mode: 0o100644,
            hash: Some(hash),
        })
    }

    fn file_paths(&self) -> Vec<String> {
        self.entries().map(|(k, _)| k.to_owned()).collect()
    }
}

// ---------------------------------------------------------------------------
// Shared ACL guard (ACL-before-tombstone ordering is the invariant)
// ---------------------------------------------------------------------------

fn check_acl(acl: &[AclEntry], path: &str, principal: &str, now: i64) -> Result<(), FsError> {
    assert!(
        acl.len() <= 1_000_000,
        "acl slice exceeds safe cap of 1M entries"
    );
    if decide(acl, path, principal, now, Permission::Read) == AclDecision::Deny {
        return Err(FsError::AccessDenied);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Resolution helpers (public; all 100% safe Rust, no FFI)
// ---------------------------------------------------------------------------

/// Resolve attributes for `path` with overlay tombstone and ACL checks.
///
/// Order: (1) ACL Deny => EACCES; (2) tombstone => ENOENT; (3) attr_for.
/// This mirrors the ordering in src/fs.rs LunarFs::getattr.
pub fn resolve_attr(
    index: &dyn IndexSeam,
    overlay: Option<&OverlayStore>,
    acl: &[AclEntry],
    agent: AgentId,
    principal: &str,
    now: i64,
    path: &str,
) -> Result<Attributes, FsError> {
    assert!(path.len() <= 4096, "path must not exceed 4096 bytes");
    assert!(
        principal.len() <= 256,
        "principal must not exceed 256 bytes"
    );
    check_acl(acl, path, principal, now)?;
    if let Some(ov) = overlay {
        if super::is_tombstoned(ov, agent, path) {
            return Err(FsError::NotFound);
        }
    }
    attr_for(index, path)
}

/// Like resolve_attr but additionally requires the resolved node is a file.
///
/// Returns Err(FsError::IsADir) for directories, matching fuse_open semantics.
pub fn resolve_open(
    index: &dyn IndexSeam,
    overlay: Option<&OverlayStore>,
    acl: &[AclEntry],
    agent: AgentId,
    principal: &str,
    now: i64,
    path: &str,
) -> Result<Attributes, FsError> {
    let attrs = resolve_attr(index, overlay, acl, agent, principal, now, path)?;
    if attrs.kind != NodeKind::File {
        return Err(FsError::IsADir);
    }
    Ok(attrs)
}

/// Resolve a byte-range read for `path`: ACL check, then overlay routing.
///
/// When overlay is Some, delegates to route_read (handles Tombstone and
/// Overlay(hash) > Base priority).  When overlay is None, reads from the
/// CAS base directly (production base-only mount with no overlay configured).
///
/// Takes a concrete &Index (not &dyn IndexSeam) because route_read requires it.
#[allow(clippy::too_many_arguments)]
pub fn resolve_read(
    index: &Index,
    store: &dyn Store,
    overlay: Option<&OverlayStore>,
    acl: &[AclEntry],
    agent: AgentId,
    principal: &str,
    now: i64,
    path: &str,
    offset: u64,
    size: u32,
) -> Result<Vec<u8>, FsError> {
    assert!(path.len() <= 4096, "path must not exceed 4096 bytes");
    assert!(
        principal.len() <= 256,
        "principal must not exceed 256 bytes"
    );
    check_acl(acl, path, principal, now)?;
    if let Some(ov) = overlay {
        return super::route_read(
            store,
            index,
            ov,
            agent,
            path,
            offset as usize,
            size as usize,
        )
        .map_err(|e| FsError::IoError(e.to_string()))
        .and_then(|opt| opt.ok_or(FsError::NotFound));
    }
    // Base-only path: no overlay configured, read directly from index + CAS.
    let hash = index.lookup(path).ok_or(FsError::NotFound)?;
    let data = store
        .get(&hash)
        .map_err(|e| FsError::IoError(e.to_string()))?
        .ok_or_else(|| FsError::IoError(format!("blob missing from store: {}", path)))?;
    let len = data.len() as u64;
    if offset >= len || size == 0 {
        return Ok(Vec::new());
    }
    let start = offset as usize;
    let end = ((offset + u64::from(size)).min(len)) as usize;
    Ok(data[start..end].to_vec())
}

/// List direct children of the directory at `path`, with ACL check.
///
/// Tombstoned children are NOT filtered (matching Linux backend behaviour:
/// LunarFs::readdir does not suppress tombstoned entries).
pub fn resolve_readdir(
    index: &dyn IndexSeam,
    _overlay: Option<&OverlayStore>,
    acl: &[AclEntry],
    _agent: AgentId,
    principal: &str,
    now: i64,
    path: &str,
) -> Result<Vec<DirEntry>, FsError> {
    assert!(path.len() <= 4096, "path must not exceed 4096 bytes");
    assert!(
        principal.len() <= 256,
        "principal must not exceed 256 bytes"
    );
    check_acl(acl, path, principal, now)?;
    read_dir(index, path)
}

/// Resolve the exact byte length a full read of `path` would produce.
///
/// Order mirrors resolve_attr: (1) ACL Deny => EACCES; (2) tombstone => ENOENT;
/// (3) overlay write => overlay blob length (on-demand CAS fetch, covered by
/// the existing local cache); (4) directory => 0; (5) base file => cached
/// length from Index (O(1) map read, no CAS fetch).
#[allow(clippy::too_many_arguments)]
pub fn resolve_size(
    index: &Index,
    store: &dyn Store,
    overlay: Option<&OverlayStore>,
    acl: &[AclEntry],
    agent: AgentId,
    principal: &str,
    now: i64,
    path: &str,
) -> Result<u64, FsError> {
    assert!(path.len() <= 4096, "path must not exceed 4096 bytes");
    assert!(
        principal.len() <= 256,
        "principal must not exceed 256 bytes"
    );

    check_acl(acl, path, principal, now)?;

    if let Some(ov) = overlay {
        match ov
            .resolve(agent, path)
            .map_err(|e| FsError::IoError(e.to_string()))?
        {
            Resolution::Tombstone => return Err(FsError::NotFound),
            Resolution::Overlay(hex) => {
                let hash = hex_to_hash(&hex).map_err(|e| FsError::IoError(e.to_string()))?;
                let blob = store
                    .get(&hash)
                    .map_err(|e| FsError::IoError(e.to_string()))?
                    .ok_or_else(|| FsError::IoError(format!("overlay blob missing: {}", path)))?;
                return Ok(blob.len() as u64);
            }
            Resolution::Base => {}
        }
    }

    // Base path (no overlay write) or no overlay configured.
    // attr_for infers directories from base file prefixes; directories return size 0.
    let attrs = attr_for(index, path)?;
    if attrs.kind == NodeKind::Dir {
        return Ok(0);
    }
    // File in base: O(1) size lookup from Index, no CAS fetch.
    index.lookup_size(path).ok_or(FsError::NotFound)
}

// ---------------------------------------------------------------------------
// Deterministic scenario tests (ungated, no mount, no FFI, no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acl::Permission;
    use crate::cas::MemStore;
    use crate::fuse::route_write;
    use crate::fuse::translate::errno_for;
    use crate::overlay::{OverlayStore, WorkspaceId};
    use crate::tree::{serialize_tree, TreeEntry, MODE_FILE};
    use rusqlite::Connection;

    const WS: WorkspaceId = 1;
    const PRINCIPAL: &str = "alice";
    const NOW: i64 = 1_000_000_000;

    fn make_base(name: &str, content: &[u8]) -> (MemStore, Index) {
        let store = MemStore::new();
        let file_hash = store.put(content).unwrap();
        let tree_bytes = serialize_tree(&[TreeEntry {
            mode: MODE_FILE,
            name: name.to_string(),
            hash: file_hash,
        }]);
        let root_hash = store.put(&tree_bytes).unwrap();
        let index = Index::build(&store, &root_hash).unwrap();
        (store, index)
    }

    fn make_overlay() -> OverlayStore {
        let conn = Connection::open_in_memory().unwrap();
        let ov = OverlayStore::new(conn);
        ov.init_schema().unwrap();
        ov
    }

    fn no_acl() -> Vec<AclEntry> {
        Vec::new()
    }

    fn deny_acl(path_prefix: &str) -> Vec<AclEntry> {
        vec![AclEntry {
            path_prefix: path_prefix.to_owned(),
            principal: PRINCIPAL.to_owned(),
            permission: Permission::Deny,
            expires_at: None,
        }]
    }

    // errno_for(AccessDenied) maps to -EACCES.
    #[test]
    fn access_denied_errno_maps_to_eacces() {
        assert_eq!(errno_for(&FsError::AccessDenied), -libc::EACCES);
        assert_ne!(errno_for(&FsError::AccessDenied), -libc::ENOENT);
    }

    // (a) Path present only in lower layer: fresh agent, no overlay row.
    //     resolve_read must fall through to CAS base and return the original bytes.
    #[test]
    fn resolve_read_lower_layer_only() {
        let (store, index) = make_base("f.txt", b"base-content");
        let overlay = make_overlay();
        let agent = overlay.fork(WS).unwrap();

        let bytes = resolve_read(
            &index,
            &store,
            Some(&overlay),
            &no_acl(),
            agent,
            PRINCIPAL,
            NOW,
            "f.txt",
            0,
            1024,
        )
        .unwrap();
        assert_eq!(bytes, b"base-content", "fresh agent must see base bytes");
        assert!(!bytes.is_empty());
    }

    // (b) Same path shadowed in upper overlay: resolve_read must return upper bytes.
    #[test]
    fn resolve_read_upper_shadows_lower() {
        let (store, index) = make_base("f.txt", b"base-bytes");
        let overlay = make_overlay();
        let agent = overlay.fork(WS).unwrap();

        route_write(&store, &overlay, agent, WS, "f.txt", b"upper-bytes").unwrap();

        let bytes = resolve_read(
            &index,
            &store,
            Some(&overlay),
            &no_acl(),
            agent,
            PRINCIPAL,
            NOW,
            "f.txt",
            0,
            1024,
        )
        .unwrap();
        assert_eq!(bytes, b"upper-bytes", "upper overlay must shadow lower");
        assert_ne!(bytes.as_slice(), b"base-bytes");
    }

    // (c) Tombstoned path: resolve_read => NotFound/ENOENT; resolve_attr same.
    #[test]
    fn resolve_tombstone_is_enoent() {
        let (store, index) = make_base("del.txt", b"bye");
        let overlay = make_overlay();
        let agent = overlay.fork(WS).unwrap();
        overlay.capture_delete(agent, WS, "del.txt").unwrap();

        let err_read = resolve_read(
            &index,
            &store,
            Some(&overlay),
            &no_acl(),
            agent,
            PRINCIPAL,
            NOW,
            "del.txt",
            0,
            1024,
        )
        .unwrap_err();
        assert!(
            matches!(err_read, FsError::NotFound),
            "tombstone read must be NotFound"
        );
        assert_eq!(errno_for(&err_read), -libc::ENOENT);

        let err_attr = resolve_attr(
            &index,
            Some(&overlay),
            &no_acl(),
            agent,
            PRINCIPAL,
            NOW,
            "del.txt",
        )
        .unwrap_err();
        assert!(
            matches!(err_attr, FsError::NotFound),
            "tombstone attr must be NotFound"
        );
        assert_eq!(errno_for(&err_attr), -libc::ENOENT);
    }

    // (d) ACL-denied path: all four helpers return AccessDenied / EACCES.
    #[test]
    fn resolve_acl_denied_is_eacces() {
        let (store, index) = make_base("dir/secret.txt", b"private");
        let overlay = make_overlay();
        let agent = overlay.fork(WS).unwrap();

        let acl_file = deny_acl("dir/secret.txt");

        let e_attr = resolve_attr(
            &index,
            Some(&overlay),
            &acl_file,
            agent,
            PRINCIPAL,
            NOW,
            "dir/secret.txt",
        )
        .unwrap_err();
        assert!(matches!(e_attr, FsError::AccessDenied));
        assert_eq!(errno_for(&e_attr), -libc::EACCES);

        let e_open = resolve_open(
            &index,
            Some(&overlay),
            &acl_file,
            agent,
            PRINCIPAL,
            NOW,
            "dir/secret.txt",
        )
        .unwrap_err();
        assert!(matches!(e_open, FsError::AccessDenied));
        assert_eq!(errno_for(&e_open), -libc::EACCES);

        let e_read = resolve_read(
            &index,
            &store,
            Some(&overlay),
            &acl_file,
            agent,
            PRINCIPAL,
            NOW,
            "dir/secret.txt",
            0,
            1024,
        )
        .unwrap_err();
        assert!(matches!(e_read, FsError::AccessDenied));
        assert_eq!(errno_for(&e_read), -libc::EACCES);

        let acl_dir = deny_acl("dir");
        let e_readdir = resolve_readdir(
            &index,
            Some(&overlay),
            &acl_dir,
            agent,
            PRINCIPAL,
            NOW,
            "dir",
        )
        .unwrap_err();
        assert!(matches!(e_readdir, FsError::AccessDenied));
        assert_eq!(errno_for(&e_readdir), -libc::EACCES);
    }

    // (e) Allowed path (empty ACL = open by default): normal bytes and attrs.
    #[test]
    fn resolve_allowed_path() {
        let (store, index) = make_base("pub.txt", b"hello");
        let overlay = make_overlay();
        let agent = overlay.fork(WS).unwrap();

        let bytes = resolve_read(
            &index,
            &store,
            Some(&overlay),
            &no_acl(),
            agent,
            PRINCIPAL,
            NOW,
            "pub.txt",
            0,
            1024,
        )
        .unwrap();
        assert_eq!(bytes, b"hello");

        let attrs = resolve_attr(
            &index,
            Some(&overlay),
            &no_acl(),
            agent,
            PRINCIPAL,
            NOW,
            "pub.txt",
        )
        .unwrap();
        assert_eq!(attrs.kind, NodeKind::File);
    }

    // --- resolve_size tests --------------------------------------------------

    // (a) Base file: resolve_size returns the base blob's byte length.
    #[test]
    fn size_base_file_equals_blob_length() {
        let content = b"hello world";
        let (store, index) = make_base("greet.txt", content);
        let overlay = make_overlay();
        let agent = overlay.fork(WS).unwrap();

        let sz = resolve_size(
            &index,
            &store,
            Some(&overlay),
            &no_acl(),
            agent,
            PRINCIPAL,
            NOW,
            "greet.txt",
        )
        .unwrap();
        assert_eq!(sz, content.len() as u64, "base size must equal blob length");
        assert_eq!(sz, 11);
    }

    // (b) Overlay write wins: overlay blob length takes precedence over base.
    //     Base has 5 bytes; overlay write has 14 bytes (different length).
    #[test]
    fn size_overlay_write_wins() {
        let (store, index) = make_base("f.txt", b"hello");
        let overlay = make_overlay();
        let agent = overlay.fork(WS).unwrap();

        route_write(&store, &overlay, agent, WS, "f.txt", b"longer content").unwrap();

        let sz = resolve_size(
            &index,
            &store,
            Some(&overlay),
            &no_acl(),
            agent,
            PRINCIPAL,
            NOW,
            "f.txt",
        )
        .unwrap();
        assert_eq!(
            sz,
            b"longer content".len() as u64,
            "overlay size must win over base"
        );
        assert_ne!(sz, 5, "must not return base length when overlay is present");
        assert_eq!(sz, 14);
    }

    // (c) Directory: resolve_size returns 0.
    #[test]
    fn size_directory_is_zero() {
        let (store, index) = make_base("subdir/file.txt", b"contents");
        let overlay = make_overlay();
        let agent = overlay.fork(WS).unwrap();

        let sz = resolve_size(
            &index,
            &store,
            Some(&overlay),
            &no_acl(),
            agent,
            PRINCIPAL,
            NOW,
            "subdir",
        )
        .unwrap();
        assert_eq!(sz, 0, "directory must resolve to size 0");
    }

    // (d) Tombstoned path: resolve_size returns ENOENT (NotFound).
    #[test]
    fn size_tombstone_is_enoent() {
        let (store, index) = make_base("del.txt", b"bye");
        let overlay = make_overlay();
        let agent = overlay.fork(WS).unwrap();
        overlay.capture_delete(agent, WS, "del.txt").unwrap();

        let err = resolve_size(
            &index,
            &store,
            Some(&overlay),
            &no_acl(),
            agent,
            PRINCIPAL,
            NOW,
            "del.txt",
        )
        .unwrap_err();
        assert!(
            matches!(err, FsError::NotFound),
            "tombstone must return NotFound"
        );
        assert_eq!(errno_for(&err), -libc::ENOENT);
    }

    // (e) ACL-denied path: resolve_size returns EACCES (AccessDenied).
    #[test]
    fn size_acl_deny_is_eacces() {
        let (store, index) = make_base("secret.txt", b"private data");
        let overlay = make_overlay();
        let agent = overlay.fork(WS).unwrap();
        let acl = deny_acl("secret.txt");

        let err = resolve_size(
            &index,
            &store,
            Some(&overlay),
            &acl,
            agent,
            PRINCIPAL,
            NOW,
            "secret.txt",
        )
        .unwrap_err();
        assert!(
            matches!(err, FsError::AccessDenied),
            "ACL deny must return AccessDenied"
        );
        assert_eq!(errno_for(&err), -libc::EACCES);
    }

    // (f) Overlay + ACL interaction:
    //     - Denied tombstone: path is both tombstoned AND ACL-denied.
    //       ACL check runs first => EACCES (not ENOENT).
    //     - Allowed shadow: ACL allows + upper write => upper bytes returned.
    #[test]
    fn resolve_overlay_acl_interaction() {
        let (store, index) = make_base("f.txt", b"base");
        let overlay = make_overlay();

        // Denied tombstone: ACL deny must win over tombstone.
        let agent_a = overlay.fork(WS).unwrap();
        overlay.capture_delete(agent_a, WS, "f.txt").unwrap();
        let acl = deny_acl("f.txt");
        let err = resolve_read(
            &index,
            &store,
            Some(&overlay),
            &acl,
            agent_a,
            PRINCIPAL,
            NOW,
            "f.txt",
            0,
            1024,
        )
        .unwrap_err();
        assert!(
            matches!(err, FsError::AccessDenied),
            "denied+tombstoned must return EACCES, not ENOENT"
        );
        assert_eq!(errno_for(&err), -libc::EACCES);

        // Allowed shadow: allowed ACL + upper write => upper bytes.
        let agent_b = overlay.fork(WS).unwrap();
        route_write(&store, &overlay, agent_b, WS, "f.txt", b"upper-value").unwrap();
        let bytes = resolve_read(
            &index,
            &store,
            Some(&overlay),
            &no_acl(),
            agent_b,
            PRINCIPAL,
            NOW,
            "f.txt",
            0,
            1024,
        )
        .unwrap();
        assert_eq!(
            bytes, b"upper-value",
            "allowed shadow must return upper bytes"
        );
    }
}
