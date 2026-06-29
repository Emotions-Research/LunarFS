//! Thin C-ABI shims for the macOS FUSE-T read-path callbacks.
//!
//! Each shim:
//!   1. Converts the raw C arguments to safe Rust types.
//!   2. Delegates to the resolution layer (fuse::resolve) which composes
//!      ACL and overlay logic identically to the Linux fuser backend.
//!   3. Maps Ok/Err to the FUSE return contract.
//!
//! ALL unsafe code lives here. The resolution and translation layers are
//! 100% safe Rust. unsafe blocks are commented with the invariant they uphold.
//!
//! readlink: omitted. The tree format (tree.rs) does not record symlink targets;
//! the Index only stores file paths, so there is no link_target to return.
//!
//! access: omitted. The mount options do not require it and the read flow does
//! not call it; FUSE returns -ENOSYS for unregistered callbacks, which is
//! correct for a read-only content store.

#![cfg(all(target_os = "macos", feature = "fuse"))]

use libc::{c_char, c_int, c_void, off_t, size_t, stat};
use std::ffi::CStr;
use std::sync::{Arc, OnceLock};

use crate::acl::AclEntry;
use crate::cas::{Hash, Store};
use crate::fuse::resolve::{resolve_attr, resolve_open, resolve_read, resolve_readdir, resolve_size};
use crate::fuse::translate::{errno_for, FsError, IndexSeam, NodeKind, NodeMeta, StoreSeam};
use crate::fuse_t::{FuseFillDirT, FuseFileInfo, FuseOperations};
use crate::index::Index;
use crate::overlay::{AgentId, OverlayStore, WorkspaceId};

// ---------------------------------------------------------------------------
// Global filesystem state (set once before fuse_main, read from callbacks)
// ---------------------------------------------------------------------------

/// Holds the immutable filesystem state for the lifetime of the mount.
pub struct FsData {
    pub index: Index,
    pub store: Box<dyn Store>,
    pub overlay: Option<Arc<OverlayStore>>,
    pub agent: AgentId,
    pub workspace: WorkspaceId,
    pub acl: Vec<AclEntry>,
    pub principal: String,
}

// SAFETY: Index is Send+Sync (HashMap with no interior mutability).
// Store is already Send+Sync per its trait bound.
// OverlayStore uses a Mutex<Connection> internally and is Send+Sync.
unsafe impl Send for FsData {}
unsafe impl Sync for FsData {}

static FS_STATE: OnceLock<Arc<FsData>> = OnceLock::new();

/// Call this once before fuse_main. Returns false if called more than once.
pub fn init_state(data: Arc<FsData>) -> bool {
    FS_STATE.set(data).is_ok()
}

fn state() -> Option<&'static Arc<FsData>> {
    FS_STATE.get()
}

// ---------------------------------------------------------------------------
// IndexSeam + StoreSeam adapters for FsData
// ---------------------------------------------------------------------------

impl IndexSeam for FsData {
    fn lookup(&self, path: &str) -> Option<NodeMeta> {
        self.index.lookup_entry(path).map(|(hash, size)| NodeMeta {
            kind: NodeKind::File,
            size,
            mode: 0o100644,
            hash: Some(hash),
        })
    }
    fn file_paths(&self) -> Vec<String> {
        self.index.entries().map(|(k, _)| k.to_owned()).collect()
    }
}

impl StoreSeam for FsData {
    fn hydrate(&self, hash: &Hash) -> Option<Vec<u8>> {
        self.store.get(hash).ok().flatten()
    }
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Converts a raw C string path to a &str, returning -EINVAL on failure.
/// FUSE guarantees the pointer is non-null and valid for the callback duration.
unsafe fn path_str<'a>(raw: *const c_char) -> Result<&'a str, c_int> {
    if raw.is_null() {
        return Err(-libc::EINVAL);
    }
    // SAFETY: FUSE guarantees a valid, null-terminated, non-null C string for
    // the duration of this callback invocation.
    CStr::from_ptr(raw)
        .to_str()
        .map_err(|_| -libc::EINVAL)
}

/// Strips the leading '/' that FUSE always prepends. "/" becomes "".
fn strip_root(fuse_path: &str) -> &str {
    fuse_path.strip_prefix('/').unwrap_or(fuse_path)
}

fn fs_err_to_errno(e: FsError) -> c_int {
    errno_for(&e)
}

/// Current unix seconds; supplied to ACL decide() so it stays clock-free.
fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Callback implementations
// ---------------------------------------------------------------------------

/// getattr: fill in *stbuf for the node at path, return 0 or negative errno.
unsafe extern "C" fn fuse_getattr(raw_path: *const c_char, stbuf: *mut stat) -> c_int {
    // SAFETY: stbuf is a valid, writable stat pointer for the callback duration
    // (guaranteed by libfuse).
    if stbuf.is_null() {
        return -libc::EINVAL;
    }

    let path_s = match unsafe { path_str(raw_path) } {
        Ok(p) => p,
        Err(e) => return e,
    };
    let index_path = strip_root(path_s);

    let fs = match state() {
        Some(s) => s,
        None => return -libc::EIO,
    };

    let now = now_unix_secs();
    let attrs = match resolve_attr(
        fs.as_ref(),
        fs.overlay.as_deref(),
        &fs.acl,
        fs.agent,
        &fs.principal,
        now,
        index_path,
    ) {
        Ok(a) => a,
        Err(e) => return fs_err_to_errno(e),
    };

    let size = match resolve_size(
        &fs.index,
        fs.store.as_ref(),
        fs.overlay.as_deref(),
        &fs.acl,
        fs.agent,
        &fs.principal,
        now,
        index_path,
    ) {
        Ok(sz) => sz,
        Err(e) => return fs_err_to_errno(e),
    };

    // SAFETY: We verified stbuf is non-null above; we zero it before writing
    // to avoid leaving garbage in fields we do not set.
    let st = unsafe { &mut *stbuf };
    *st = unsafe { std::mem::zeroed() };

    st.st_mode = attrs.mode as libc::mode_t;
    st.st_size = size as libc::off_t;
    // Conventional nlink: 2 for dirs (self + parent), 1 for files.
    st.st_nlink = if attrs.kind == NodeKind::Dir { 2 } else { 1 };

    0
}

/// open: validate path exists and is a file. Returns 0, -ENOENT, -EISDIR, or -EACCES.
/// We do not check O_RDWR/O_WRONLY because write callbacks are not registered;
/// FUSE will return -EROFS on any write attempt without us doing anything.
unsafe extern "C" fn fuse_open(raw_path: *const c_char, _fi: *mut FuseFileInfo) -> c_int {
    let path_s = match unsafe { path_str(raw_path) } {
        Ok(p) => p,
        Err(e) => return e,
    };
    let index_path = strip_root(path_s);

    let fs = match state() {
        Some(s) => s,
        None => return -libc::EIO,
    };

    match resolve_open(
        fs.as_ref(),
        fs.overlay.as_deref(),
        &fs.acl,
        fs.agent,
        &fs.principal,
        now_unix_secs(),
        index_path,
    ) {
        Ok(_) => 0,
        Err(e) => fs_err_to_errno(e),
    }
}

/// read: copy at most `size` bytes starting at `offset` into `buf`.
/// Returns bytes written (>= 0) or a negative errno.
unsafe extern "C" fn fuse_read(
    raw_path: *const c_char,
    buf: *mut c_char,
    size: size_t,
    offset: off_t,
    _fi: *mut FuseFileInfo,
) -> c_int {
    if buf.is_null() {
        return -libc::EINVAL;
    }
    // FUSE size is always <= INT_MAX; clamp to u32 safely.
    let size_u32 = match u32::try_from(size.min(u32::MAX as size_t)) {
        Ok(v) => v,
        Err(_) => return -libc::EINVAL,
    };
    let offset_u64 = if offset < 0 { return -libc::EINVAL; } else { offset as u64 };

    let path_s = match unsafe { path_str(raw_path) } {
        Ok(p) => p,
        Err(e) => return e,
    };
    let index_path = strip_root(path_s);

    let fs = match state() {
        Some(s) => s,
        None => return -libc::EIO,
    };

    let bytes = match resolve_read(
        &fs.index,
        fs.store.as_ref(),
        fs.overlay.as_deref(),
        &fs.acl,
        fs.agent,
        &fs.principal,
        now_unix_secs(),
        index_path,
        offset_u64,
        size_u32,
    ) {
        Ok(b) => b,
        Err(e) => return fs_err_to_errno(e),
    };

    let n = bytes.len();
    // SAFETY: FUSE guarantees buf points to a writable region of at least `size`
    // bytes for the callback duration. We only write `n <= size` bytes.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, n) };

    n as c_int
}

/// readdir: call filler once per child entry. Returns 0 or negative errno.
unsafe extern "C" fn fuse_readdir(
    raw_path: *const c_char,
    buf: *mut c_void,
    filler: FuseFillDirT,
    _offset: off_t,
    _fi: *mut FuseFileInfo,
) -> c_int {
    let path_s = match unsafe { path_str(raw_path) } {
        Ok(p) => p,
        Err(e) => return e,
    };
    let index_path = strip_root(path_s);

    let fs = match state() {
        Some(s) => s,
        None => return -libc::EIO,
    };

    let entries = match resolve_readdir(
        fs.as_ref(),
        fs.overlay.as_deref(),
        &fs.acl,
        fs.agent,
        &fs.principal,
        now_unix_secs(),
        index_path,
    ) {
        Ok(v) => v,
        Err(e) => return fs_err_to_errno(e),
    };

    for entry in &entries {
        // SAFETY: name is valid UTF-8 from our Index (no embedded NULs); we
        // append a NUL terminator via CString so filler receives a proper C str.
        let cname = match std::ffi::CString::new(entry.name.as_str()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // SAFETY: filler, buf, and cname.as_ptr() are valid for this callback;
        // passing a null stat pointer (third arg) tells FUSE to stat the entry
        // itself via getattr, which is the correct behaviour for a simple readdir.
        unsafe { filler(buf, cname.as_ptr(), std::ptr::null(), 0) };
    }

    0
}

// ---------------------------------------------------------------------------
// Populate a FuseOperations struct with the read callbacks
// ---------------------------------------------------------------------------

/// Returns a zeroed FuseOperations with the four read-path callbacks installed.
///
/// Callers pass this (by reference) to `fuse_main_real` after calling
/// `init_state` with the filesystem data.
pub fn make_read_ops() -> FuseOperations {
    // SAFETY: FuseOperations is #[repr(C)] with no invariants; zeroed is valid
    // and encodes every Option<fn> field as a C null pointer.
    let mut ops: FuseOperations = unsafe { std::mem::zeroed() };
    ops.getattr = Some(fuse_getattr);
    ops.open = Some(fuse_open);
    ops.read = Some(fuse_read);
    ops.readdir = Some(fuse_readdir);
    ops
}

// ---------------------------------------------------------------------------
// Backend-level size tests (macos+fuse cfg inherited from module attribute)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{FsData, resolve_size};
    use crate::acl::{AclEntry, Permission};
    use crate::cas::{MemStore, Store};
    use crate::fuse::route_write;
    use crate::fuse::translate::{errno_for, FsError, IndexSeam};
    use crate::overlay::{OverlayStore, WorkspaceId};
    use crate::tree::{serialize_tree, TreeEntry, MODE_FILE};
    use rusqlite::Connection;
    use std::sync::Arc;

    const WS: WorkspaceId = 1;
    const PRINCIPAL: &str = "alice";
    const NOW: i64 = 1_000_000_000;

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

    fn deny_acl(path_prefix: &str) -> Vec<AclEntry> {
        vec![AclEntry {
            path_prefix: path_prefix.to_owned(),
            principal: PRINCIPAL.to_owned(),
            permission: Permission::Deny,
            expires_at: None,
        }]
    }

    fn make_fs(
        store: MemStore,
        index: crate::index::Index,
        overlay: Arc<OverlayStore>,
        agent: crate::overlay::AgentId,
        acl: Vec<AclEntry>,
    ) -> FsData {
        FsData {
            index,
            store: Box::new(store),
            overlay: Some(overlay),
            agent,
            workspace: WS,
            acl,
            principal: PRINCIPAL.to_string(),
        }
    }

    // (a) Base file: resolve_size == blob length; IndexSeam::lookup.size matches.
    #[test]
    fn size_base_file_equals_blob_length() {
        let content = b"hello world";
        let (store, index) = make_base("greet.txt", content);
        let overlay = Arc::new(make_overlay());
        let agent = overlay.fork(WS).unwrap();
        let fs = make_fs(store, index, overlay, agent, Vec::new());

        let sz = resolve_size(
            &fs.index, fs.store.as_ref(), fs.overlay.as_deref(),
            &fs.acl, fs.agent, &fs.principal, NOW, "greet.txt",
        ).unwrap();
        assert_eq!(sz, content.len() as u64);
        assert_eq!(sz, 11);

        // IndexSeam::lookup must also carry the real size now.
        let meta = fs.lookup("greet.txt").unwrap();
        assert_eq!(meta.size, sz, "IndexSeam::lookup size must match blob length");
    }

    // (b) Overlay write of a different length: overlay length wins.
    #[test]
    fn size_overlay_write_wins() {
        let (store, index) = make_base("f.txt", b"hi");
        let overlay = Arc::new(make_overlay());
        let agent = overlay.fork(WS).unwrap();
        route_write(&store, &overlay, agent, WS, "f.txt", b"longer content").unwrap();
        let fs = make_fs(store, index, overlay, agent, Vec::new());

        let sz = resolve_size(
            &fs.index, fs.store.as_ref(), fs.overlay.as_deref(),
            &fs.acl, fs.agent, &fs.principal, NOW, "f.txt",
        ).unwrap();
        assert_eq!(sz, b"longer content".len() as u64);
        assert_ne!(sz, b"hi".len() as u64, "must not return base length");
    }

    // (c) Directory: resolve_size returns 0.
    #[test]
    fn size_directory_is_zero() {
        let (store, index) = make_base("sub/file.txt", b"data");
        let overlay = Arc::new(make_overlay());
        let agent = overlay.fork(WS).unwrap();
        let fs = make_fs(store, index, overlay, agent, Vec::new());

        let sz = resolve_size(
            &fs.index, fs.store.as_ref(), fs.overlay.as_deref(),
            &fs.acl, fs.agent, &fs.principal, NOW, "sub",
        ).unwrap();
        assert_eq!(sz, 0);
    }

    // (d) Tombstone: resolve_size returns NotFound / -ENOENT.
    #[test]
    fn size_tombstone_is_enoent() {
        let (store, index) = make_base("del.txt", b"bye");
        let overlay = Arc::new(make_overlay());
        let agent = overlay.fork(WS).unwrap();
        overlay.capture_delete(agent, WS, "del.txt").unwrap();
        let fs = make_fs(store, index, overlay, agent, Vec::new());

        let err = resolve_size(
            &fs.index, fs.store.as_ref(), fs.overlay.as_deref(),
            &fs.acl, fs.agent, &fs.principal, NOW, "del.txt",
        ).unwrap_err();
        assert!(matches!(err, FsError::NotFound));
        assert_eq!(errno_for(&err), -libc::ENOENT);
    }

    // (e) ACL-deny: resolve_size returns AccessDenied / -EACCES.
    #[test]
    fn size_acl_deny_is_eacces() {
        let (store, index) = make_base("secret.txt", b"private");
        let overlay = Arc::new(make_overlay());
        let agent = overlay.fork(WS).unwrap();
        let fs = make_fs(store, index, overlay, agent, deny_acl("secret.txt"));

        let err = resolve_size(
            &fs.index, fs.store.as_ref(), fs.overlay.as_deref(),
            &fs.acl, fs.agent, &fs.principal, NOW, "secret.txt",
        ).unwrap_err();
        assert!(matches!(err, FsError::AccessDenied));
        assert_eq!(errno_for(&err), -libc::EACCES);
    }
}
