use crate::cas::Hash;
use crate::cas::Store;
use crate::index::Index;
use std::collections::HashMap;
use std::io;

// nyx: cache capped at 4096 blobs; upgrade path: LRU eviction with lru crate
const CACHE_LIMIT: usize = 4096;

pub struct ReadCache {
    store: Box<dyn Store>,
    index: Index,
    // keyed by blob hash so two paths pointing at the same blob share one cache entry
    cache: HashMap<Hash, Vec<u8>>,
    /// count of store.get() calls; exposed for tests to verify cache hits
    pub fetch_count: usize,
    acl_entries: Vec<crate::acl::AclEntry>,
    principal: String,
}

impl ReadCache {
    pub fn new(store: Box<dyn Store>, index: Index) -> Self {
        Self {
            store,
            index,
            cache: HashMap::new(),
            fetch_count: 0,
            acl_entries: Vec::new(),
            principal: String::new(),
        }
    }

    /// Set the ACL entries and caller principal for blob-fetch enforcement.
    /// Returns self so it can be chained after new().
    pub fn with_acl(mut self, entries: Vec<crate::acl::AclEntry>, principal: String) -> Self {
        self.acl_entries = entries;
        self.principal = principal;
        self
    }

    /// Resolve `path` via the Index, lazily fetch the blob from the Store on
    /// first access, cache it, and return the requested byte range.
    /// Returns None if the path is not in the index.
    /// Returns Err(PermissionDenied) when the ACL denies access to `path`
    /// (distinct from Ok(None) = not found).
    pub fn read_path(
        &mut self,
        path: &str,
        offset: usize,
        size: usize,
    ) -> io::Result<Option<Vec<u8>>> {
        assert!(!path.is_empty(), "path must not be empty");
        assert!(
            size <= 128 * 1024 * 1024,
            "requested size must be sane (<=128 MiB)"
        );

        // ACL check: denied paths return PermissionDenied, not None (not-found).
        // Uses the caller-supplied now so decide() stays clock-free.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if crate::acl::decide(
            &self.acl_entries,
            path,
            &self.principal,
            now,
            crate::acl::Permission::Read,
        ) == crate::acl::AclDecision::Deny
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "access denied by ACL",
            ));
        }

        let hash = match self.index.lookup(path) {
            Some(h) => h,
            None => return Ok(None),
        };

        if !self.cache.contains_key(&hash) {
            let data = self.store.get(&hash)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("blob for path '{}' not found in store", path),
                )
            })?;
            if self.cache.len() >= CACHE_LIMIT {
                // nyx: clear-all eviction when full; upgrade path: LRU
                self.cache.clear();
            }
            self.cache.insert(hash, data);
            self.fetch_count += 1;
        }

        let blob = self.cache.get(&hash).expect("just inserted");
        let start = offset.min(blob.len());
        let end = (offset + size).min(blob.len());
        Ok(Some(blob[start..end].to_vec()))
    }

    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    pub fn put_blob(&self, data: &[u8]) -> io::Result<Hash> {
        self.store.put(data)
    }

    pub fn get_by_hash(&self, hash: &Hash) -> io::Result<Option<Vec<u8>>> {
        self.store.get(hash)
    }

    pub fn store_ref(&self) -> &dyn Store {
        self.store.as_ref()
    }

    pub fn index_ref(&self) -> &Index {
        &self.index
    }
}

// ---------- FUSE filesystem (Linux only, requires --features fuser) ----------
#[cfg(all(target_os = "linux", feature = "fuser"))]
pub use fuse_impl::mount;
#[cfg(all(target_os = "linux", feature = "fuser"))]
pub use fuse_impl::LunarFs;

#[cfg(all(target_os = "linux", feature = "fuser"))]
mod fuse_impl {
    use super::ReadCache;
    use crate::cas::Store;
    use crate::fuse::resolve::resolve_size;
    use crate::index::Index;
    use crate::overlay::{AgentId, OverlayStore, WorkspaceId};
    use crate::tree::MODE_DIR;
    use fuser::{
        FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory,
        ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
    };
    use std::collections::HashMap;
    use std::ffi::OsStr;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    const TTL: Duration = Duration::from_secs(1);

    fn now_unix_secs() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    struct InoEntry {
        name: String,
        #[allow(dead_code)]
        parent_ino: u64,
        is_dir: bool,
        #[allow(dead_code)]
        hash: Option<crate::cas::Hash>,
        children: Vec<u64>,
    }

    pub struct LunarFs {
        read_cache: ReadCache,
        inodes: HashMap<u64, InoEntry>,
        path_to_ino: HashMap<String, u64>,
        ino_to_path: HashMap<u64, String>,
        next_ino: u64,
        overlay: Option<Arc<OverlayStore>>,
        agent: AgentId,
        workspace: WorkspaceId,
        acl_entries: Vec<crate::acl::AclEntry>,
    }

    impl LunarFs {
        pub fn new(store: Box<dyn Store>, index: Index) -> Self {
            let mut inodes: HashMap<u64, InoEntry> = HashMap::new();
            let mut path_to_ino: HashMap<String, u64> = HashMap::new();
            let mut next_ino: u64 = 2;

            inodes.insert(
                1,
                InoEntry {
                    name: "/".to_string(),
                    parent_ino: 1,
                    is_dir: true,
                    hash: None,
                    children: Vec::new(),
                },
            );
            path_to_ino.insert(String::new(), 1);

            let mut paths: Vec<(String, crate::cas::Hash)> =
                index.entries().map(|(p, h)| (p.to_string(), *h)).collect();
            paths.sort_by(|a, b| a.0.cmp(&b.0));

            for (path, hash) in paths {
                let components: Vec<&str> = path.split('/').collect();
                let mut parent_ino = 1u64;
                let mut so_far = String::new();
                let last_idx = components.len() - 1;

                for (ci, &comp) in components.iter().enumerate() {
                    let full = if so_far.is_empty() {
                        comp.to_string()
                    } else {
                        format!("{}/{}", so_far, comp)
                    };

                    if ci < last_idx {
                        let ino = if let Some(&existing) = path_to_ino.get(&full) {
                            existing
                        } else {
                            let ino = next_ino;
                            next_ino += 1;
                            path_to_ino.insert(full.clone(), ino);
                            inodes.insert(
                                ino,
                                InoEntry {
                                    name: comp.to_string(),
                                    parent_ino,
                                    is_dir: true,
                                    hash: None,
                                    children: Vec::new(),
                                },
                            );
                            if let Some(p) = inodes.get_mut(&parent_ino) {
                                p.children.push(ino);
                            }
                            ino
                        };
                        parent_ino = ino;
                        so_far = full;
                    } else {
                        let ino = next_ino;
                        next_ino += 1;
                        path_to_ino.insert(full.clone(), ino);
                        inodes.insert(
                            ino,
                            InoEntry {
                                name: comp.to_string(),
                                parent_ino,
                                is_dir: false,
                                hash: Some(hash),
                                children: Vec::new(),
                            },
                        );
                        if let Some(p) = inodes.get_mut(&parent_ino) {
                            p.children.push(ino);
                        }
                        so_far = full;
                    }
                }
            }

            let ino_to_path: HashMap<u64, String> =
                path_to_ino.iter().map(|(p, &i)| (i, p.clone())).collect();
            let read_cache = ReadCache::new(store, index);
            Self {
                read_cache,
                inodes,
                path_to_ino,
                ino_to_path,
                next_ino,
                overlay: None,
                agent: 0,
                workspace: 0,
                acl_entries: Vec::new(),
            }
        }

        /// Set the ACL entries used in getattr and read callbacks.
        /// Returns self so it can be chained after new() or with_overlay().
        pub fn with_acl(mut self, entries: Vec<crate::acl::AclEntry>) -> Self {
            self.acl_entries = entries;
            self
        }

        /// Construct an overlay-aware filesystem. Same as `new` but routes reads,
        /// writes, and deletes through `overlay` for the given `agent`.
        pub fn with_overlay(
            store: Box<dyn Store>,
            index: Index,
            overlay: Arc<OverlayStore>,
            agent: AgentId,
            workspace: WorkspaceId,
        ) -> Self {
            let mut fs = Self::new(store, index);
            fs.overlay = Some(overlay);
            fs.agent = agent;
            fs.workspace = workspace;
            fs
        }

        /// Resolve ino to the relative path string, or None for the root inode.
        fn path_for_ino(&self, ino: u64) -> Option<String> {
            self.ino_to_path.get(&ino).cloned()
        }

        /// Read the full current content for a file inode through the overlay or base.
        /// Returns empty Vec for paths not found in either layer (new files).
        // nyx: capped at 128 MiB; upgrade path: streaming read for large files
        fn read_full(&mut self, path: &str) -> Vec<u8> {
            const CAP: usize = 128 * 1024 * 1024;
            if let Some(ref ov) = self.overlay.clone() {
                match crate::fuse::route_read(
                    self.read_cache.store_ref(),
                    self.read_cache.index_ref(),
                    ov,
                    self.agent,
                    path,
                    0,
                    CAP,
                ) {
                    Ok(Some(data)) => data,
                    _ => Vec::new(),
                }
            } else {
                match self.read_cache.read_path(path, 0, CAP) {
                    Ok(Some(data)) => data,
                    _ => Vec::new(),
                }
            }
        }

        /// Ensure a leaf inode exists for `path`, creating it if absent.
        /// Returns the inode number. Does not create intermediate directories.
        fn ensure_leaf_inode(&mut self, path: &str) -> u64 {
            if let Some(&ino) = self.path_to_ino.get(path) {
                return ino;
            }
            // Find or infer parent ino.
            let (parent_ino, leaf_name) = match path.rfind('/') {
                None => (1u64, path.to_string()),
                Some(pos) => {
                    let parent_path = &path[..pos];
                    let name = path[pos + 1..].to_string();
                    let pino = self.path_to_ino.get(parent_path).copied().unwrap_or(1);
                    (pino, name)
                }
            };
            let ino = self.next_ino;
            self.next_ino += 1;
            self.path_to_ino.insert(path.to_string(), ino);
            self.ino_to_path.insert(ino, path.to_string());
            self.inodes.insert(
                ino,
                InoEntry {
                    name: leaf_name,
                    parent_ino,
                    is_dir: false,
                    hash: None,
                    children: Vec::new(),
                },
            );
            if let Some(parent) = self.inodes.get_mut(&parent_ino) {
                parent.children.push(ino);
            }
            ino
        }

        fn make_attr(&self, ino: u64, is_dir: bool, size: u64) -> FileAttr {
            let kind = if is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            FileAttr {
                ino,
                size,
                blocks: 0,
                atime: SystemTime::UNIX_EPOCH,
                mtime: SystemTime::UNIX_EPOCH,
                ctime: SystemTime::UNIX_EPOCH,
                crtime: SystemTime::UNIX_EPOCH,
                kind,
                perm: if is_dir { 0o755 } else { 0o644 },
                nlink: if is_dir { 2 } else { 1 },
                uid: 0,
                gid: 0,
                rdev: 0,
                blksize: 512,
                flags: 0,
            }
        }
    }

    impl Filesystem for LunarFs {
        fn lookup(&mut self, req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
            let name_str = name.to_string_lossy();
            let children: Vec<u64> = match self.inodes.get(&parent) {
                Some(e) => e.children.clone(),
                None => {
                    reply.error(2 /* ENOENT */);
                    return;
                }
            };
            let child_ino = children.iter().find_map(|&ino| {
                self.inodes
                    .get(&ino)
                    .filter(|e| e.name == name_str.as_ref())
                    .map(|_| ino)
            });
            match child_ino {
                Some(ino) => {
                    // Check overlay tombstone before reporting the file as present.
                    if let Some(ref ov) = self.overlay.clone() {
                        if let Some(path) = self.path_for_ino(ino) {
                            if crate::fuse::is_tombstoned(ov, self.agent, &path) {
                                reply.error(2 /* ENOENT */);
                                return;
                            }
                        }
                    }
                    let is_dir = self.inodes[&ino].is_dir;
                    let size = if is_dir {
                        0
                    } else if let Some(path) = self.path_for_ino(ino) {
                        let principal = req.uid().to_string();
                        let now = now_unix_secs();
                        resolve_size(
                            self.read_cache.index_ref(),
                            self.read_cache.store_ref(),
                            self.overlay.as_deref(),
                            &self.acl_entries,
                            self.agent,
                            &principal,
                            now,
                            &path,
                        )
                        .unwrap_or(0)
                    } else {
                        0
                    };
                    let attr = self.make_attr(ino, is_dir, size);
                    reply.entry(&TTL, &attr, 0);
                }
                None => reply.error(2 /* ENOENT */),
            }
        }

        fn getattr(&mut self, req: &Request<'_>, ino: u64, reply: ReplyAttr) {
            match self.inodes.get(&ino) {
                Some(entry) => {
                    let is_dir = entry.is_dir;
                    let size;
                    if !is_dir {
                        if let Some(path) = self.path_for_ino(ino) {
                            // ACL check: EACCES before tombstone so a denied-but-known
                            // path returns EACCES, never ENOENT.
                            let principal = req.uid().to_string();
                            let now = now_unix_secs();
                            if crate::acl::decide(
                                &self.acl_entries,
                                &path,
                                &principal,
                                now,
                                crate::acl::Permission::Read,
                            ) == crate::acl::AclDecision::Deny
                            {
                                reply.error(13 /* EACCES */);
                                return;
                            }
                            // A tombstoned file must report ENOENT on getattr too.
                            if let Some(ref ov) = self.overlay.clone() {
                                if crate::fuse::is_tombstoned(ov, self.agent, &path) {
                                    reply.error(2 /* ENOENT */);
                                    return;
                                }
                            }
                            size = resolve_size(
                                self.read_cache.index_ref(),
                                self.read_cache.store_ref(),
                                self.overlay.as_deref(),
                                &self.acl_entries,
                                self.agent,
                                &principal,
                                now,
                                &path,
                            )
                            .unwrap_or(0);
                        } else {
                            size = 0;
                        }
                    } else {
                        size = 0;
                    }
                    let attr = self.make_attr(ino, is_dir, size);
                    reply.attr(&TTL, &attr);
                }
                None => reply.error(2 /* ENOENT */),
            }
        }

        fn readdir(
            &mut self,
            _req: &Request<'_>,
            ino: u64,
            _fh: u64,
            offset: i64,
            mut reply: ReplyDirectory,
        ) {
            let children: Vec<u64> = match self.inodes.get(&ino) {
                Some(e) if e.is_dir => e.children.clone(),
                _ => {
                    reply.error(2 /* ENOENT */);
                    return;
                }
            };

            let mut idx: i64 = 1;
            if offset < idx && reply.add(ino, idx, FileType::Directory, ".") {
                reply.ok();
                return;
            }
            idx += 1;
            if offset < idx && reply.add(ino, idx, FileType::Directory, "..") {
                reply.ok();
                return;
            }
            idx += 1;

            for &child_ino in &children {
                if offset >= idx {
                    idx += 1;
                    continue;
                }
                if let Some(child) = self.inodes.get(&child_ino) {
                    let kind = if child.is_dir {
                        FileType::Directory
                    } else {
                        FileType::RegularFile
                    };
                    let name = child.name.clone();
                    if reply.add(child_ino, idx, kind, &name) {
                        reply.ok();
                        return;
                    }
                }
                idx += 1;
            }
            reply.ok();
        }

        fn open(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
            reply.opened(0, 0);
        }

        fn opendir(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
            reply.opened(0, 0);
        }

        fn read(
            &mut self,
            req: &Request<'_>,
            ino: u64,
            _fh: u64,
            offset: i64,
            size: u32,
            _flags: i32,
            _lock_owner: Option<u64>,
            reply: ReplyData,
        ) {
            let path = match self.path_for_ino(ino).filter(|p| !p.is_empty()) {
                Some(p) => p,
                None => {
                    reply.error(2 /* ENOENT */);
                    return;
                }
            };
            // ACL check: EACCES before any read logic so a denied-but-known path
            // returns EACCES, never ENOENT.
            let principal = req.uid().to_string();
            let now = now_unix_secs();
            if crate::acl::decide(
                &self.acl_entries,
                &path,
                &principal,
                now,
                crate::acl::Permission::Read,
            ) == crate::acl::AclDecision::Deny
            {
                reply.error(13 /* EACCES */);
                return;
            }
            if let Some(ref ov) = self.overlay.clone() {
                match crate::fuse::route_read(
                    self.read_cache.store_ref(),
                    self.read_cache.index_ref(),
                    ov,
                    self.agent,
                    &path,
                    offset as usize,
                    size as usize,
                ) {
                    Ok(Some(data)) => reply.data(&data),
                    Ok(None) => reply.error(2 /* ENOENT */),
                    Err(_) => reply.error(5 /* EIO */),
                }
            } else {
                match self
                    .read_cache
                    .read_path(&path, offset as usize, size as usize)
                {
                    Ok(Some(data)) => reply.data(&data),
                    Ok(None) => reply.error(2 /* ENOENT */),
                    Err(_) => reply.error(5 /* EIO */),
                }
            }
        }

        fn write(
            &mut self,
            _req: &Request<'_>,
            ino: u64,
            _fh: u64,
            offset: i64,
            data: &[u8],
            _write_flags: u32,
            _flags: i32,
            _lock_owner: Option<u64>,
            reply: ReplyWrite,
        ) {
            let ov = match self.overlay.clone() {
                Some(o) => o,
                None => {
                    reply.error(30 /* EROFS */);
                    return;
                }
            };
            let path = match self.path_for_ino(ino).filter(|p| !p.is_empty()) {
                Some(p) => p,
                None => {
                    reply.error(2 /* ENOENT */);
                    return;
                }
            };
            // RMW: read current content, apply write at offset, store new blob.
            let mut current = self.read_full(&path);
            let off = offset as usize;
            let new_len = off + data.len();
            if current.len() < new_len {
                current.resize(new_len, 0);
            }
            current[off..new_len].copy_from_slice(data);

            match crate::fuse::route_write(
                self.read_cache.store_ref(),
                &ov,
                self.agent,
                self.workspace,
                &path,
                &current,
            ) {
                Ok(_) => reply.written(data.len() as u32),
                Err(_) => reply.error(5 /* EIO */),
            }
        }

        fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
            let ov = match self.overlay.clone() {
                Some(o) => o,
                None => {
                    reply.error(30 /* EROFS */);
                    return;
                }
            };
            let parent_path = self.path_for_ino(parent);
            let name_str = name.to_string_lossy();
            let path = match parent_path {
                None => name_str.to_string(),
                Some(ref p) if p.is_empty() => name_str.to_string(),
                Some(ref p) => format!("{}/{}", p, name_str),
            };
            match crate::fuse::route_delete(&ov, self.agent, self.workspace, &path) {
                Ok(()) => reply.ok(),
                Err(_) => reply.error(5 /* EIO */),
            }
        }
    }

    pub fn mount(fs: LunarFs, mountpoint: &Path) -> anyhow::Result<()> {
        let opts = vec![MountOption::RO, MountOption::FSName("lunar".to_string())];
        fuser::mount2(fs, mountpoint, &opts)?;
        Ok(())
    }

    /// Mount with write support enabled (required for overlay write/unlink handlers).
    pub fn mount_rw(fs: LunarFs, mountpoint: &Path) -> anyhow::Result<()> {
        let opts = vec![MountOption::FSName("lunar".to_string())];
        fuser::mount2(fs, mountpoint, &opts)?;
        Ok(())
    }

    // Linux fuser backend size tests. Only compiled on Linux with --features fuser.
    // On macOS (the dev host) this module is excluded by the parent cfg; that is expected.
    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::acl::{AclEntry, Permission};
        use crate::cas::MemStore;
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

        // (a) make_attr with resolved base blob size returns the real length.
        #[test]
        fn make_attr_base_file_size() {
            let content = b"hello world";
            let (store, index) = make_base("greet.txt", content);
            let ov = Arc::new(make_overlay());
            let agent = ov.fork(WS).unwrap();
            let sz = resolve_size(
                &index,
                &store,
                Some(ov.as_ref()),
                &[],
                agent,
                PRINCIPAL,
                NOW,
                "greet.txt",
            )
            .unwrap();
            assert_eq!(sz, content.len() as u64);

            let fs = LunarFs::with_overlay(Box::new(store), index, ov, agent, WS);
            let ino = *fs.path_to_ino.get("greet.txt").unwrap();
            let attr = fs.make_attr(ino, false, sz);
            assert_eq!(attr.size, content.len() as u64);
        }

        // (b) make_attr with overlay size wins over base length.
        #[test]
        fn make_attr_overlay_size_wins() {
            let (store, index) = make_base("f.txt", b"hi");
            let ov = Arc::new(make_overlay());
            let agent = ov.fork(WS).unwrap();
            crate::fuse::route_write(&store, &ov, agent, WS, "f.txt", b"longer content").unwrap();
            let sz = resolve_size(
                &index,
                &store,
                Some(ov.as_ref()),
                &[],
                agent,
                PRINCIPAL,
                NOW,
                "f.txt",
            )
            .unwrap();
            assert_eq!(sz, b"longer content".len() as u64);
            assert_ne!(sz, b"hi".len() as u64);

            let fs = LunarFs::with_overlay(Box::new(store), index, ov, agent, WS);
            let ino = *fs.path_to_ino.get("f.txt").unwrap();
            let attr = fs.make_attr(ino, false, sz);
            assert_eq!(attr.size, b"longer content".len() as u64);
        }

        // (c) Directory inode always gets size 0.
        #[test]
        fn make_attr_directory_size_zero() {
            let (store, index) = make_base("sub/file.txt", b"data");
            let fs = LunarFs::new(Box::new(store), index);
            let attr = fs.make_attr(1, true, 0);
            assert_eq!(attr.size, 0);
        }

        // (d) resolve_size returns NotFound for tombstoned path (feeds ENOENT guard).
        #[test]
        fn size_tombstone_is_not_found() {
            let (store, index) = make_base("del.txt", b"bye");
            let ov = Arc::new(make_overlay());
            let agent = ov.fork(WS).unwrap();
            ov.capture_delete(agent, WS, "del.txt").unwrap();
            let err = resolve_size(
                &index,
                &store,
                Some(ov.as_ref()),
                &[],
                agent,
                PRINCIPAL,
                NOW,
                "del.txt",
            )
            .unwrap_err();
            assert!(matches!(err, crate::fuse::translate::FsError::NotFound));
        }

        // (e) resolve_size returns AccessDenied for ACL-denied path (feeds EACCES guard).
        #[test]
        fn size_acl_deny_is_access_denied() {
            let (store, index) = make_base("secret.txt", b"private");
            let ov = Arc::new(make_overlay());
            let agent = ov.fork(WS).unwrap();
            let acl = deny_acl("secret.txt");
            let err = resolve_size(
                &index,
                &store,
                Some(ov.as_ref()),
                &acl,
                agent,
                PRINCIPAL,
                NOW,
                "secret.txt",
            )
            .unwrap_err();
            assert!(matches!(err, crate::fuse::translate::FsError::AccessDenied));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ReadCache;
    use crate::acl::{AclEntry, Permission};
    use crate::cas::MemStore;
    use crate::cas::Store;
    use crate::index::Index;
    use crate::tree::{serialize_tree, TreeEntry, MODE_DIR, MODE_FILE};
    use std::sync::{Arc, Mutex};

    /// Wraps MemStore and counts store.get() calls to verify cache behavior.
    struct CountingStore {
        inner: MemStore,
        get_count: Arc<Mutex<usize>>,
    }

    impl CountingStore {
        fn new() -> (Self, Arc<Mutex<usize>>) {
            let counter = Arc::new(Mutex::new(0usize));
            let s = Self {
                inner: MemStore::new(),
                get_count: counter.clone(),
            };
            (s, counter)
        }
    }

    impl Store for CountingStore {
        fn put(&self, data: &[u8]) -> std::io::Result<crate::cas::Hash> {
            self.inner.put(data)
        }

        fn get(&self, hash: &crate::cas::Hash) -> std::io::Result<Option<Vec<u8>>> {
            *self.get_count.lock().unwrap() += 1;
            self.inner.get(hash)
        }

        fn has(&self, hash: &crate::cas::Hash) -> bool {
            self.inner.has(hash)
        }
    }

    #[test]
    fn read_cache_gate_test() {
        // THE GATE: exercises path -> index -> store -> bytes + cache.
        let (cs, counter) = CountingStore::new();

        let h_main = cs.put(b"fn main() {}").unwrap();
        let h_readme = cs.put(b"# hi").unwrap();

        let src_tree_bytes = serialize_tree(&[TreeEntry {
            mode: MODE_FILE,
            name: "main.rs".into(),
            hash: h_main,
        }]);
        let src_tree_h = cs.put(&src_tree_bytes).unwrap();

        let root_bytes = serialize_tree(&[
            TreeEntry {
                mode: MODE_DIR,
                name: "src".into(),
                hash: src_tree_h,
            },
            TreeEntry {
                mode: MODE_FILE,
                name: "README.md".into(),
                hash: h_readme,
            },
        ]);
        let root_h = cs.put(&root_bytes).unwrap();

        let index = Index::build(&cs, &root_h).unwrap();

        // Reset counter: Index::build read tree blobs, not file blobs.
        *counter.lock().unwrap() = 0;

        let mut cache = ReadCache::new(Box::new(cs), index);

        // First read: must fetch from store.
        let bytes = cache
            .read_path("src/main.rs", 0, 1024)
            .unwrap()
            .expect("path must exist");
        assert_eq!(bytes, b"fn main() {}");
        assert_eq!(cache.fetch_count, 1, "first read must fetch from store");

        // Second read: must serve from cache, fetch_count unchanged.
        let bytes2 = cache
            .read_path("src/main.rs", 0, 1024)
            .unwrap()
            .expect("path must exist");
        assert_eq!(bytes2, b"fn main() {}");
        assert_eq!(
            cache.fetch_count, 1,
            "second read must be served from cache, not re-fetched"
        );

        // Partial read with offset and size.
        let partial = cache
            .read_path("src/main.rs", 3, 4)
            .unwrap()
            .expect("path exists");
        assert_eq!(partial, b"main");
        assert_eq!(cache.fetch_count, 1, "offset read still comes from cache");

        // Read past EOF clamps to empty slice, no panic.
        let beyond = cache
            .read_path("src/main.rs", 9999, 10)
            .unwrap()
            .expect("path exists");
        assert_eq!(beyond, b"", "read past EOF must return empty bytes");

        // Absent path returns None.
        let absent = cache.read_path("no/such/file", 0, 10).unwrap();
        assert!(absent.is_none(), "absent path must return None");

        // Different blob increments fetch count.
        cache
            .read_path("README.md", 0, 1024)
            .unwrap()
            .expect("readme must exist");
        assert_eq!(cache.fetch_count, 2, "new blob must fetch from store");
    }

    #[test]
    fn read_cache_absent_path_is_none() {
        let store = MemStore::new();
        let empty_tree = serialize_tree(&[]);
        let root = store.put(&empty_tree).unwrap();
        let index = Index::build(&store, &root).unwrap();
        let mut cache = ReadCache::new(Box::new(store), index);
        let result = cache.read_path("anything", 0, 64).unwrap();
        assert!(result.is_none());
    }

    fn make_index_with_file(name: &str, content: &[u8]) -> (MemStore, Index) {
        let store = MemStore::new();
        let hash = store.put(content).unwrap();
        let tree_bytes = serialize_tree(&[TreeEntry {
            mode: MODE_FILE,
            name: name.to_string(),
            hash,
        }]);
        let root = store.put(&tree_bytes).unwrap();
        let index = Index::build(&store, &root).unwrap();
        (store, index)
    }

    // Blob-fetch ACL: denied path returns PermissionDenied, not None (not-found).
    #[test]
    fn read_path_denied_by_acl_returns_permission_denied() {
        let (store, index) = make_index_with_file("secret.txt", b"top secret");
        let entries = vec![AclEntry {
            path_prefix: "secret.txt".to_string(),
            principal: "*".to_string(),
            permission: Permission::Deny,
            expires_at: None,
        }];
        let mut cache = ReadCache::new(Box::new(store), index).with_acl(entries, "any".to_string());
        let err = cache
            .read_path("secret.txt", 0, 1024)
            .expect_err("must be denied");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }

    // Blob-fetch ACL: allowed path returns bytes normally.
    #[test]
    fn read_path_allowed_by_acl_returns_bytes() {
        let (store, index) = make_index_with_file("pub.txt", b"public data");
        let entries = vec![AclEntry {
            path_prefix: "pub.txt".to_string(),
            principal: "*".to_string(),
            permission: Permission::Read,
            expires_at: None,
        }];
        let mut cache = ReadCache::new(Box::new(store), index).with_acl(entries, "any".to_string());
        let got = cache
            .read_path("pub.txt", 0, 1024)
            .unwrap()
            .expect("must return bytes");
        assert_eq!(got, b"public data");
    }

    // Blob-fetch ACL: path not in ACL is open by default.
    #[test]
    fn read_path_not_in_acl_is_open() {
        let (store, index) = make_index_with_file("open.txt", b"open");
        // ACL has an unrelated entry; "open.txt" has no matching prefix.
        let entries = vec![AclEntry {
            path_prefix: "other".to_string(),
            principal: "*".to_string(),
            permission: Permission::Deny,
            expires_at: None,
        }];
        let mut cache = ReadCache::new(Box::new(store), index).with_acl(entries, "any".to_string());
        let got = cache
            .read_path("open.txt", 0, 1024)
            .unwrap()
            .expect("open path must be readable");
        assert_eq!(got, b"open");
    }

    // Blob-fetch ACL: permission-denied is distinct from not-found (None).
    #[test]
    fn read_path_denied_vs_absent_are_distinct() {
        let (store, index) = make_index_with_file("exists.txt", b"content");
        let deny_entries = vec![AclEntry {
            path_prefix: "exists.txt".to_string(),
            principal: "*".to_string(),
            permission: Permission::Deny,
            expires_at: None,
        }];
        let mut cache =
            ReadCache::new(Box::new(store), index).with_acl(deny_entries, "u".to_string());

        // Denied path: returns Err(PermissionDenied), not Ok(None).
        let err = cache
            .read_path("exists.txt", 0, 64)
            .expect_err("must error");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);

        // Absent path (no ACL entry): returns Ok(None).
        let absent = cache.read_path("nonexistent.txt", 0, 64).unwrap();
        assert!(
            absent.is_none(),
            "absent path must be Ok(None), not an error"
        );
    }
}
