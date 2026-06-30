//! NFSv3 filesystem backed by CAS + copy-on-write overlay.
//!
//! Read path: getattr/lookup/readdir use IdTable + OverlayView (no blob fetch).
//!            read() uses route_read (one blob fetch via store.get).
//! Write path: write/create/mkdir/rename/remove/setattr route through
//!             route_write/route_delete and update the in-process IdTable.
//! LAZY HYDRATION: store.get is only called inside read() and RMW helpers.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use nfsserve::nfs::{
    fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfstime3, sattr3, specdata3,
};
use nfsserve::nfs::{set_mode3, set_mtime, set_size3};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use rusqlite::Connection;

use crate::cas::{hex_to_hash, Store};
use crate::fuse;
use crate::fuse::translate::{self, FsError, NodeKind};
use crate::index::Index;
use crate::overlay::{AgentId, OverlayStore, Resolution, WorkspaceId};

use super::ids::IdTable;
use super::overlay_view::OverlayView;

// ---------------------------------------------------------------------------
// Per-path attr overrides (mode/mtime set by setattr/create; in-process only)
// ---------------------------------------------------------------------------

struct AttrOverride {
    mode: Option<u32>,
    mtime: Option<nfstime3>,
}

// ---------------------------------------------------------------------------
// CasNfs
// ---------------------------------------------------------------------------

/// NFSv3 filesystem backed by a CAS index + copy-on-write overlay.
///
/// All metadata operations are O(1) in-memory; store.get is called only when
/// bytes are actually read.
pub struct CasNfs {
    index: Index,
    store: Box<dyn Store>,
    ids: IdTable,
    overlay: Arc<OverlayStore>,
    agent: AgentId,
    workspace: WorkspaceId,
    uid: u32,
    gid: u32,
    fsid: u64,
    mount_time: u32,
    /// Sizes of overlay-written blobs; updated on every write/create/setattr.
    overlay_sizes: Mutex<HashMap<String, u64>>,
    /// Per-path mode/mtime overrides; not persisted to the overlay.
    attr_overrides: Mutex<HashMap<String, AttrOverride>>,
}

impl CasNfs {
    /// Build a read-write CasNfs backed by an existing OverlayStore agent.
    pub fn with_overlay(
        index: Index,
        store: Box<dyn Store>,
        overlay: Arc<OverlayStore>,
        agent: AgentId,
        workspace: WorkspaceId,
    ) -> Self {
        assert!(agent > 0, "agent must be a positive rowid");
        assert!(workspace > 0, "workspace must be a positive rowid");
        let ids = IdTable::build(&index);
        #[cfg(unix)]
        let uid = unsafe { libc::getuid() };
        #[cfg(not(unix))]
        let uid = 0u32;
        #[cfg(unix)]
        let gid = unsafe { libc::getgid() };
        #[cfg(not(unix))]
        let gid = 0u32;
        let mount_time = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;
        assert!(mount_time > 0, "system clock must be past Unix epoch");
        Self {
            index,
            store,
            ids,
            overlay,
            agent,
            workspace,
            uid,
            gid,
            fsid: 0x0CA5_1234,
            mount_time,
            overlay_sizes: Mutex::new(HashMap::new()),
            attr_overrides: Mutex::new(HashMap::new()),
        }
    }

    /// Convenience constructor for tests and the read-only path (creates an
    /// in-process in-memory overlay so the overlay API is always available).
    pub fn new(index: Index, store: Box<dyn Store>) -> Self {
        let conn = Connection::open_in_memory().expect("in-memory overlay db for CasNfs");
        let ov = OverlayStore::new(conn);
        ov.init_schema().expect("overlay init_schema");
        let agent = ov.fork(1).expect("overlay fork");
        Self::with_overlay(index, store, Arc::new(ov), agent, 1)
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Build an fattr3 for (id, path, kind). No store.get calls.
    fn fattr_for(&self, id: fileid3, path: &str, kind: NodeKind) -> fattr3 {
        let default_t = nfstime3 {
            seconds: self.mount_time,
            nseconds: 0,
        };
        let (ftype, mode, size, mtime) = if kind == NodeKind::Dir {
            (ftype3::NF3DIR, 0o040755u32, 0u64, default_t)
        } else {
            let sz = {
                let s = self.overlay_sizes.lock().expect("overlay_sizes poisoned");
                s.get(path).copied()
            }
            .or_else(|| self.index.lookup_size(path))
            .unwrap_or(0);
            let (m, mt) = {
                let a = self.attr_overrides.lock().expect("attr_overrides poisoned");
                let ov = a.get(path);
                let m = ov.and_then(|o| o.mode).unwrap_or(0o100644);
                let mt = ov.and_then(|o| o.mtime).unwrap_or(default_t);
                (m, mt)
            };
            (ftype3::NF3REG, m, sz, mt)
        };
        fattr3 {
            ftype,
            mode: mode & 0o7777,
            nlink: if kind == NodeKind::Dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            size,
            used: size,
            rdev: specdata3::default(),
            fsid: self.fsid,
            fileid: id,
            atime: mtime,
            mtime,
            ctime: mtime,
        }
    }

    /// Build an overlay-aware IndexSeam snapshot for this agent.
    /// Releases all locks before returning; OverlayView owns its data.
    fn make_overlay_view(&self) -> OverlayView<'_> {
        let entries = self
            .overlay
            .entries_for_agent(self.agent)
            .unwrap_or_default();
        let sizes_snap = {
            let s = self.overlay_sizes.lock().expect("overlay_sizes poisoned");
            s.clone()
        };
        let mut written: HashMap<String, u64> = HashMap::new();
        let mut tombstoned: HashSet<String> = HashSet::new();
        for e in entries {
            match e.blob_hash {
                Some(_) => {
                    let size = sizes_snap.get(&e.path).copied().unwrap_or(0);
                    written.insert(e.path, size);
                }
                None => {
                    tombstoned.insert(e.path);
                }
            }
        }
        OverlayView::new(&self.index, written, tombstoned)
    }

    /// Read the full content of `path` for RMW. Calls store.get (one hydration).
    fn read_full_for_rmw(&self, path: &str) -> std::io::Result<Vec<u8>> {
        assert!(path.len() <= 4096, "path must not exceed 4096 bytes");
        let resolution = self
            .overlay
            .resolve(self.agent, path)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        match resolution {
            Resolution::Tombstone | Resolution::Base if self.index.lookup(path).is_none() => {
                Ok(Vec::new())
            }
            Resolution::Tombstone => Ok(Vec::new()),
            Resolution::Overlay(hex) => {
                let hash = hex_to_hash(&hex)?;
                Ok(self.store.get(&hash)?.unwrap_or_default())
            }
            Resolution::Base => {
                let hash = self
                    .index
                    .lookup(path)
                    .expect("Base resolution with lookup hit");
                Ok(self.store.get(&hash)?.unwrap_or_default())
            }
        }
    }

    /// Write `data` to the overlay and update the in-process size cache.
    fn do_route_write(&self, path: &str, data: &[u8]) -> Result<(), nfsstat3> {
        assert!(path.len() <= 4096, "path must not exceed 4096 bytes");
        fuse::route_write(
            self.store.as_ref(),
            &self.overlay,
            self.agent,
            self.workspace,
            path,
            data,
        )
        .map_err(|_| nfsstat3::NFS3ERR_IO)?;
        let mut sizes = self.overlay_sizes.lock().expect("overlay_sizes poisoned");
        sizes.insert(path.to_owned(), data.len() as u64);
        Ok(())
    }

    /// Record mode/mtime from an sattr3 into the in-process attr override map.
    fn apply_sattr_overrides(&self, path: &str, attr: &sattr3) {
        let mut overrides = self.attr_overrides.lock().expect("attr_overrides poisoned");
        let entry = overrides.entry(path.to_owned()).or_insert(AttrOverride {
            mode: None,
            mtime: None,
        });
        if let set_mode3::mode(m) = attr.mode {
            entry.mode = Some(m & 0o7777);
        }
        let default_t = nfstime3 {
            seconds: self.mount_time,
            nseconds: 0,
        };
        match attr.mtime {
            set_mtime::SET_TO_CLIENT_TIME(t) => entry.mtime = Some(t),
            set_mtime::SET_TO_SERVER_TIME => entry.mtime = Some(default_t),
            set_mtime::DONT_CHANGE => {}
        }
    }

    /// Returns true if `dir_path` has any visible children (base, overlay, or interned).
    fn has_children(&self, dir_path: &str) -> bool {
        assert!(
            dir_path.len() <= 4096,
            "dir_path must not exceed 4096 bytes"
        );
        let prefix = format!("{}/", dir_path);
        let base_has = self
            .index
            .entries()
            .any(|(p, _)| p.starts_with(&prefix) && !self.ids.is_hidden(p));
        if base_has {
            return true;
        }
        let overlay_has = self
            .overlay
            .entries_for_agent(self.agent)
            .unwrap_or_default()
            .iter()
            .any(|e| e.blob_hash.is_some() && e.path.starts_with(&prefix));
        if overlay_has {
            return true;
        }
        !self.ids.dynamic_dir_children_of(dir_path).is_empty()
    }

    /// Move one file: write bytes to dst, tombstone src, update id table.
    fn rename_file(&self, src: &str, dst: &str) -> Result<(), nfsstat3> {
        assert!(src.len() <= 4096, "src path must not exceed 4096 bytes");
        assert!(dst.len() <= 4096, "dst path must not exceed 4096 bytes");
        let bytes = self
            .read_full_for_rmw(src)
            .map_err(|_| nfsstat3::NFS3ERR_IO)?;
        self.do_route_write(dst, &bytes)?;
        fuse::route_delete(&self.overlay, self.agent, self.workspace, src)
            .map_err(|_| nfsstat3::NFS3ERR_IO)?;
        {
            let mut sizes = self.overlay_sizes.lock().expect("overlay_sizes poisoned");
            sizes.remove(src);
        }
        self.ids.rename_path(src, dst, NodeKind::File);
        Ok(())
    }

    /// Move a directory subtree: re-capture all file bytes, update id table.
    fn rename_dir(&self, src_prefix: &str, dst_prefix: &str) -> Result<(), nfsstat3> {
        assert!(
            src_prefix.len() <= 4096,
            "src_prefix must not exceed 4096 bytes"
        );
        assert!(
            dst_prefix.len() <= 4096,
            "dst_prefix must not exceed 4096 bytes"
        );
        let src_slash = format!("{}/", src_prefix);

        let mut file_paths: Vec<String> = Vec::new();
        for (path, _) in self.index.entries() {
            if path.starts_with(&src_slash) && !self.ids.is_hidden(path) {
                file_paths.push(path.to_owned());
            }
        }
        for e in self
            .overlay
            .entries_for_agent(self.agent)
            .unwrap_or_default()
        {
            if e.blob_hash.is_some()
                && e.path.starts_with(&src_slash)
                && !file_paths.contains(&e.path)
            {
                file_paths.push(e.path.clone());
            }
        }

        for src_path in &file_paths {
            let suffix = &src_path[src_prefix.len()..];
            let dst_path = format!("{}{}", dst_prefix, suffix);
            if dst_path.len() > 4096 {
                continue;
            }
            let bytes = self
                .read_full_for_rmw(src_path)
                .map_err(|_| nfsstat3::NFS3ERR_IO)?;
            self.do_route_write(&dst_path, &bytes)?;
            fuse::route_delete(&self.overlay, self.agent, self.workspace, src_path)
                .map_err(|_| nfsstat3::NFS3ERR_IO)?;
            let mut sizes = self.overlay_sizes.lock().expect("overlay_sizes poisoned");
            sizes.remove(src_path.as_str());
        }

        self.ids.rename_subtree(src_prefix, dst_prefix);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

fn fserr_to_nfs(e: FsError) -> nfsstat3 {
    match e {
        FsError::NotFound => nfsstat3::NFS3ERR_NOENT,
        FsError::NotADir => nfsstat3::NFS3ERR_NOTDIR,
        FsError::IsADir => nfsstat3::NFS3ERR_ISDIR,
        FsError::IoError(_) => nfsstat3::NFS3ERR_IO,
        FsError::AccessDenied => nfsstat3::NFS3ERR_ACCES,
    }
}

/// Join parent path and child name into a child path.
fn join_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{}/{}", parent, name)
    }
}

// ---------------------------------------------------------------------------
// NFSFileSystem impl
// ---------------------------------------------------------------------------

#[async_trait]
impl NFSFileSystem for CasNfs {
    fn root_dir(&self) -> fileid3 {
        IdTable::root()
    }

    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    // --- read path -----------------------------------------------------------

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let (dir_path, dir_kind) = self.ids.path_of(dirid).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        if dir_kind != NodeKind::Dir {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }
        let name = std::str::from_utf8(filename.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        assert!(
            !name.is_empty(),
            "NFS lookup must not request empty filename"
        );
        let child_path = join_path(&dir_path, name);
        self.ids.id_of(&child_path).ok_or(nfsstat3::NFS3ERR_NOENT)
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let (path, kind) = self.ids.path_of(id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        Ok(self.fattr_for(id, &path, kind))
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let (path, kind) = self.ids.path_of(id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        if kind == NodeKind::Dir {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        }
        // Clamp to 128 MiB per route_read assertion
        let safe_count = (count as usize).min(128 * 1024 * 1024);
        let bytes = fuse::route_read(
            self.store.as_ref(),
            &self.index,
            &self.overlay,
            self.agent,
            &path,
            offset as usize,
            safe_count,
        )
        .map_err(|_| nfsstat3::NFS3ERR_IO)?
        .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        let file_size = {
            let s = self.overlay_sizes.lock().expect("overlay_sizes poisoned");
            s.get(&path).copied()
        }
        .or_else(|| self.index.lookup_size(&path))
        .unwrap_or(0);

        let eof = offset.saturating_add(bytes.len() as u64) >= file_size;
        Ok((bytes, eof))
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let (dir_path, dir_kind) = self.ids.path_of(dirid).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        if dir_kind != NodeKind::Dir {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }

        let view = self.make_overlay_view();

        // translate::read_dir returns NotFound for empty dirs (no file children).
        let raw_entries = match translate::read_dir(&view, &dir_path) {
            Ok(entries) => entries,
            Err(FsError::NotFound) => Vec::new(),
            Err(e) => return Err(fserr_to_nfs(e)),
        };

        let mut children: Vec<(fileid3, String, String, NodeKind)> = Vec::new();
        let mut seen_names: HashSet<String> = HashSet::new();

        for de in &raw_entries {
            if de.name == "." || de.name == ".." {
                continue;
            }
            let child_path = join_path(&dir_path, &de.name);
            let child_id = self
                .ids
                .id_of(&child_path)
                .unwrap_or_else(|| self.ids.intern(&child_path, de.kind));
            seen_names.insert(de.name.clone());
            children.push((child_id, child_path, de.name.clone(), de.kind));
        }

        // Surface empty dirs from mkdir that have no file children
        for (child_path, child_kind) in self.ids.dynamic_dir_children_of(&dir_path) {
            let child_name = child_path
                .rsplit('/')
                .next()
                .unwrap_or(&child_path)
                .to_owned();
            if seen_names.contains(&child_name) {
                continue;
            }
            let child_id = self
                .ids
                .id_of(&child_path)
                .unwrap_or_else(|| self.ids.intern(&child_path, child_kind));
            seen_names.insert(child_name.clone());
            children.push((child_id, child_path, child_name, child_kind));
        }

        children.sort_by_key(|item| item.0);
        assert!(
            children.len() <= 65_536,
            "child count must not exceed 65536"
        );

        let mut entries: Vec<DirEntry> = Vec::new();
        for (child_id, child_path, child_name, child_kind) in &children {
            if *child_id <= start_after {
                continue;
            }
            if entries.len() >= max_entries {
                return Ok(ReadDirResult {
                    entries,
                    end: false,
                });
            }
            let attr = self.fattr_for(*child_id, child_path, *child_kind);
            entries.push(DirEntry {
                fileid: *child_id,
                name: child_name.as_bytes().to_vec().into(),
                attr,
            });
        }
        Ok(ReadDirResult { entries, end: true })
    }

    // --- write path ----------------------------------------------------------

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        let (path, kind) = self.ids.path_of(id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        assert!(path.len() <= 4096, "path must not exceed 4096 bytes");
        if kind == NodeKind::Dir {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        }
        let mut buf = self
            .read_full_for_rmw(&path)
            .map_err(|_| nfsstat3::NFS3ERR_IO)?;
        let new_end = (offset as usize).saturating_add(data.len());
        if buf.len() < new_end {
            buf.resize(new_end, 0);
        }
        if !data.is_empty() {
            buf[offset as usize..new_end].copy_from_slice(data);
        }
        self.do_route_write(&path, &buf)?;
        self.ids.intern(&path, NodeKind::File);
        Ok(self.fattr_for(id, &path, NodeKind::File))
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let (dir_path, dir_kind) = self.ids.path_of(dirid).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        if dir_kind != NodeKind::Dir {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }
        let name = std::str::from_utf8(filename.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        assert!(!name.is_empty(), "NFS create must not have empty filename");
        let child_path = join_path(&dir_path, name);
        assert!(
            child_path.len() <= 4096,
            "child path must not exceed 4096 bytes"
        );

        let mut buf: Vec<u8> = Vec::new();
        if let set_size3::size(n) = attr.size {
            buf.resize(n as usize, 0);
        }
        self.do_route_write(&child_path, &buf)?;
        self.apply_sattr_overrides(&child_path, &attr);
        let fid = self.ids.intern(&child_path, NodeKind::File);
        let fattr = self.fattr_for(fid, &child_path, NodeKind::File);
        Ok((fid, fattr))
    }

    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        let (dir_path, dir_kind) = self.ids.path_of(dirid).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        if dir_kind != NodeKind::Dir {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }
        let name = std::str::from_utf8(filename.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        assert!(
            !name.is_empty(),
            "NFS create_exclusive must not have empty filename"
        );
        let child_path = join_path(&dir_path, name);
        assert!(
            child_path.len() <= 4096,
            "child path must not exceed 4096 bytes"
        );

        if self.ids.id_of(&child_path).is_some() {
            return Err(nfsstat3::NFS3ERR_EXIST);
        }
        self.do_route_write(&child_path, b"")?;
        let fid = self.ids.intern(&child_path, NodeKind::File);
        Ok(fid)
    }

    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let (dir_path, dir_kind) = self.ids.path_of(dirid).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        if dir_kind != NodeKind::Dir {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }
        let name = std::str::from_utf8(dirname.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        assert!(!name.is_empty(), "NFS mkdir must not have empty dirname");
        let child_path = join_path(&dir_path, name);
        assert!(
            child_path.len() <= 4096,
            "child path must not exceed 4096 bytes"
        );

        if self.ids.id_of(&child_path).is_some() {
            return Err(nfsstat3::NFS3ERR_EXIST);
        }
        let fid = self.ids.intern(&child_path, NodeKind::Dir);
        let fattr = self.fattr_for(fid, &child_path, NodeKind::Dir);
        Ok((fid, fattr))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let (dir_path, dir_kind) = self.ids.path_of(dirid).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        if dir_kind != NodeKind::Dir {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }
        let name = std::str::from_utf8(filename.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        assert!(!name.is_empty(), "NFS remove must not have empty filename");
        let child_path = join_path(&dir_path, name);
        assert!(
            child_path.len() <= 4096,
            "child path must not exceed 4096 bytes"
        );

        let child_id = self.ids.id_of(&child_path).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        let (_, child_kind) = self.ids.path_of(child_id).ok_or(nfsstat3::NFS3ERR_NOENT)?;

        if child_kind == NodeKind::Dir {
            if self.has_children(&child_path) {
                return Err(nfsstat3::NFS3ERR_NOTEMPTY);
            }
            self.ids.forget(&child_path);
        } else {
            fuse::route_delete(&self.overlay, self.agent, self.workspace, &child_path)
                .map_err(|_| nfsstat3::NFS3ERR_IO)?;
            self.ids.forget(&child_path);
            let mut sizes = self.overlay_sizes.lock().expect("overlay_sizes poisoned");
            sizes.remove(&child_path);
        }
        Ok(())
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        let (from_dir, from_dir_kind) = self
            .ids
            .path_of(from_dirid)
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;
        if from_dir_kind != NodeKind::Dir {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }
        let (to_dir, to_dir_kind) = self.ids.path_of(to_dirid).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        if to_dir_kind != NodeKind::Dir {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }
        let from_name =
            std::str::from_utf8(from_filename.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let to_name =
            std::str::from_utf8(to_filename.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        assert!(!from_name.is_empty(), "from_filename must not be empty");
        assert!(!to_name.is_empty(), "to_filename must not be empty");

        let src_path = join_path(&from_dir, from_name);
        let dst_path = join_path(&to_dir, to_name);
        assert!(
            src_path.len() <= 4096,
            "src path must not exceed 4096 bytes"
        );
        assert!(
            dst_path.len() <= 4096,
            "dst path must not exceed 4096 bytes"
        );

        let src_id = self.ids.id_of(&src_path).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        let (_, src_kind) = self.ids.path_of(src_id).ok_or(nfsstat3::NFS3ERR_NOENT)?;

        if src_kind == NodeKind::File {
            self.rename_file(&src_path, &dst_path)?;
        } else {
            self.rename_dir(&src_path, &dst_path)?;
        }
        Ok(())
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let (path, kind) = self.ids.path_of(id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        assert!(path.len() <= 4096, "path must not exceed 4096 bytes");

        if let set_size3::size(n) = setattr.size {
            if kind == NodeKind::Dir {
                return Err(nfsstat3::NFS3ERR_ISDIR);
            }
            let mut buf = self
                .read_full_for_rmw(&path)
                .map_err(|_| nfsstat3::NFS3ERR_IO)?;
            buf.resize(n as usize, 0);
            self.do_route_write(&path, &buf)?;
        }

        self.apply_sattr_overrides(&path, &setattr);
        Ok(self.fattr_for(id, &path, kind))
    }

    async fn symlink(
        &self,
        _dirid: fileid3,
        _linkname: &filename3,
        _symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn readlink(&self, _id: fileid3) -> Result<nfspath3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::{MemStore, Store};
    use crate::overlay::OverlayStore;
    use crate::tree::{serialize_tree, TreeEntry, MODE_DIR, MODE_FILE};
    use rusqlite::Connection;
    use std::sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    };

    struct CountingStore {
        inner: MemStore,
        count: Arc<AtomicU32>,
    }

    impl Store for CountingStore {
        fn put(&self, data: &[u8]) -> std::io::Result<crate::cas::Hash> {
            self.inner.put(data)
        }
        fn get(&self, hash: &crate::cas::Hash) -> std::io::Result<Option<Vec<u8>>> {
            self.count.fetch_add(1, Ordering::Relaxed);
            self.inner.get(hash)
        }
        fn has(&self, hash: &crate::cas::Hash) -> bool {
            self.inner.has(hash)
        }
    }

    /// Build fixture:  README.md = "hello world",  src/main.rs = "fn main() {}"
    fn make_fixture() -> (Index, CountingStore, Arc<AtomicU32>) {
        let mem = MemStore::new();
        let h_readme = mem.put(b"hello world").unwrap();
        let h_main = mem.put(b"fn main() {}").unwrap();
        let src_tree = mem
            .put(&serialize_tree(&[TreeEntry {
                mode: MODE_FILE,
                name: "main.rs".into(),
                hash: h_main,
            }]))
            .unwrap();
        let root_tree = mem
            .put(&serialize_tree(&[
                TreeEntry {
                    mode: MODE_FILE,
                    name: "README.md".into(),
                    hash: h_readme,
                },
                TreeEntry {
                    mode: MODE_DIR,
                    name: "src".into(),
                    hash: src_tree,
                },
            ]))
            .unwrap();
        let index = Index::build(&mem, &root_tree).expect("index build must succeed");

        let mem2 = MemStore::new();
        mem2.put(b"hello world").unwrap();
        mem2.put(b"fn main() {}").unwrap();
        mem2.put(&serialize_tree(&[TreeEntry {
            mode: MODE_FILE,
            name: "main.rs".into(),
            hash: h_main,
        }]))
        .unwrap();
        mem2.put(&serialize_tree(&[
            TreeEntry {
                mode: MODE_FILE,
                name: "README.md".into(),
                hash: h_readme,
            },
            TreeEntry {
                mode: MODE_DIR,
                name: "src".into(),
                hash: src_tree,
            },
        ]))
        .unwrap();

        let counter = Arc::new(AtomicU32::new(0));
        let counting = CountingStore {
            inner: mem2,
            count: Arc::clone(&counter),
        };
        (index, counting, counter)
    }

    fn make_nfs() -> (CasNfs, Arc<AtomicU32>) {
        let (index, counting, counter) = make_fixture();
        let fs = CasNfs::new(index, Box::new(counting));
        (fs, counter)
    }

    fn make_overlay_store() -> Arc<OverlayStore> {
        let conn = Connection::open_in_memory().expect("in-memory overlay db");
        let ov = OverlayStore::new(conn);
        ov.init_schema().expect("overlay schema");
        Arc::new(ov)
    }

    fn make_nfs_writable() -> (CasNfs, Arc<AtomicU32>) {
        let (index, counting, counter) = make_fixture();
        let overlay = make_overlay_store();
        let agent = overlay.fork(1).expect("fork agent");
        let fs = CasNfs::with_overlay(index, Box::new(counting), overlay, agent, 1);
        (fs, counter)
    }

    // ---- getattr tests -------------------------------------------------------

    #[tokio::test]
    async fn getattr_readme_returns_nf3reg_correct_size() {
        let (fs, _) = make_nfs();
        let id = fs.ids.id_of("README.md").unwrap();
        let attr = fs.getattr(id).await.expect("getattr must succeed");
        assert!(
            matches!(attr.ftype, ftype3::NF3REG),
            "README.md must be NF3REG"
        );
        assert_eq!(attr.size, 11, "hello world is 11 bytes");
        assert_eq!(attr.mode & 0o7777, 0o100644 & 0o7777);
        assert_eq!(attr.nlink, 1);
    }

    #[tokio::test]
    async fn getattr_src_returns_nf3dir() {
        let (fs, _) = make_nfs();
        let id = fs.ids.id_of("src").unwrap();
        let attr = fs.getattr(id).await.expect("getattr must succeed");
        assert!(matches!(attr.ftype, ftype3::NF3DIR), "src must be NF3DIR");
        assert_eq!(attr.size, 0, "directory size is 0");
        assert_eq!(attr.nlink, 2);
    }

    #[tokio::test]
    async fn getattr_root_returns_nf3dir() {
        let (fs, _) = make_nfs();
        let attr = fs.getattr(1).await.expect("root getattr must succeed");
        assert!(matches!(attr.ftype, ftype3::NF3DIR));
    }

    #[tokio::test]
    async fn getattr_unknown_id_returns_noent() {
        let (fs, _) = make_nfs();
        let err = fs.getattr(9999).await.unwrap_err();
        assert!(matches!(err, nfsstat3::NFS3ERR_NOENT));
    }

    // ---- lookup tests --------------------------------------------------------

    #[tokio::test]
    async fn lookup_readme_from_root() {
        let (fs, _) = make_nfs();
        let expected_id = fs.ids.id_of("README.md").unwrap();
        let found = fs
            .lookup(1, &b"README.md"[..].into())
            .await
            .expect("lookup must succeed");
        assert_eq!(found, expected_id);
    }

    #[tokio::test]
    async fn lookup_main_rs_from_src() {
        let (fs, _) = make_nfs();
        let src_id = fs.ids.id_of("src").unwrap();
        let expected = fs.ids.id_of("src/main.rs").unwrap();
        let found = fs.lookup(src_id, &b"main.rs"[..].into()).await.unwrap();
        assert_eq!(found, expected);
    }

    #[tokio::test]
    async fn lookup_missing_returns_noent() {
        let (fs, _) = make_nfs();
        let err = fs.lookup(1, &b"no_such.txt"[..].into()).await.unwrap_err();
        assert!(matches!(err, nfsstat3::NFS3ERR_NOENT));
    }

    #[tokio::test]
    async fn lookup_on_file_returns_notdir() {
        let (fs, _) = make_nfs();
        let file_id = fs.ids.id_of("README.md").unwrap();
        let err = fs
            .lookup(file_id, &b"anything"[..].into())
            .await
            .unwrap_err();
        assert!(matches!(err, nfsstat3::NFS3ERR_NOTDIR));
    }

    // ---- read tests ----------------------------------------------------------

    #[tokio::test]
    async fn read_full_file_returns_exact_bytes() {
        let (fs, _) = make_nfs();
        let id = fs.ids.id_of("README.md").unwrap();
        let (bytes, eof) = fs.read(id, 0, 1024).await.unwrap();
        assert_eq!(bytes, b"hello world");
        assert!(eof);
    }

    #[tokio::test]
    async fn read_with_offset_and_count() {
        let (fs, _) = make_nfs();
        let id = fs.ids.id_of("README.md").unwrap();
        let (bytes, _) = fs.read(id, 6, 5).await.unwrap();
        assert_eq!(bytes, b"world");
    }

    #[tokio::test]
    async fn read_past_eof_returns_empty_with_eof() {
        let (fs, _) = make_nfs();
        let id = fs.ids.id_of("README.md").unwrap();
        let (bytes, eof) = fs.read(id, 9999, 100).await.unwrap();
        assert!(bytes.is_empty());
        assert!(eof);
    }

    #[tokio::test]
    async fn read_on_dir_returns_isdir() {
        let (fs, _) = make_nfs();
        let dir_id = fs.ids.id_of("src").unwrap();
        let err = fs.read(dir_id, 0, 100).await.unwrap_err();
        assert!(matches!(err, nfsstat3::NFS3ERR_ISDIR));
    }

    // ---- LAZY HYDRATION ------------------------------------------------------

    #[tokio::test]
    async fn lazy_hydration_counter_proof() {
        let (fs, counter) = make_nfs();

        let root_attr = fs.getattr(1).await.unwrap();
        let readme_id = fs.ids.id_of("README.md").unwrap();
        let src_id = fs.ids.id_of("src").unwrap();
        let main_id = fs.ids.id_of("src/main.rs").unwrap();
        fs.getattr(readme_id).await.unwrap();
        fs.getattr(src_id).await.unwrap();
        fs.getattr(main_id).await.unwrap();
        assert!(matches!(root_attr.ftype, ftype3::NF3DIR));
        assert_eq!(
            counter.load(Ordering::Relaxed),
            0,
            "no hydration after getattr"
        );

        fs.readdir(1, 0, 100).await.unwrap();
        fs.readdir(src_id, 0, 100).await.unwrap();
        assert_eq!(
            counter.load(Ordering::Relaxed),
            0,
            "no hydration after readdir"
        );

        let (bytes, _eof) = fs.read(readme_id, 0, 1024).await.unwrap();
        assert_eq!(bytes, b"hello world");
        assert_eq!(
            counter.load(Ordering::Relaxed),
            1,
            "exactly one hydration after read"
        );
    }

    // ---- readdir tests -------------------------------------------------------

    #[tokio::test]
    async fn readdir_root_returns_both_children() {
        let (fs, _) = make_nfs();
        let result = fs.readdir(1, 0, 100).await.unwrap();
        assert!(result.end);
        let names: Vec<String> = result
            .entries
            .iter()
            .map(|e| String::from_utf8_lossy(&e.name.0).to_string())
            .collect();
        assert!(names.contains(&"README.md".to_owned()));
        assert!(names.contains(&"src".to_owned()));
        assert!(
            !names.contains(&".".to_owned()),
            "dot entries must be excluded"
        );
    }

    #[tokio::test]
    async fn readdir_pagination_excludes_start_after() {
        let (fs, _) = make_nfs();
        let result_all = fs.readdir(1, 0, 100).await.unwrap();
        assert!(!result_all.entries.is_empty());
        let first_id = result_all.entries[0].fileid;
        let result_page2 = fs.readdir(1, first_id, 100).await.unwrap();
        assert!(result_page2.entries.iter().all(|e| e.fileid != first_id));
    }

    #[tokio::test]
    async fn readdir_max_entries_truncates_with_end_false() {
        let (fs, _) = make_nfs();
        let result = fs.readdir(1, 0, 1).await.unwrap();
        assert_eq!(result.entries.len(), 1);
        assert!(!result.end, "end must be false when truncated");
    }

    #[tokio::test]
    async fn readdir_on_file_returns_notdir() {
        let (fs, _) = make_nfs();
        let file_id = fs.ids.id_of("README.md").unwrap();
        let err = fs.readdir(file_id, 0, 100).await.unwrap_err();
        assert!(matches!(err, nfsstat3::NFS3ERR_NOTDIR));
    }

    // ---- write path (a): write existing base file, read back new bytes -------

    #[tokio::test]
    async fn write_existing_file_roundtrip() {
        let (fs, _) = make_nfs_writable();
        let id = fs.ids.id_of("README.md").unwrap();
        let new_data = b"updated content";
        let fattr = fs.write(id, 0, new_data).await.expect("write must succeed");
        assert_eq!(
            fattr.size,
            new_data.len() as u64,
            "returned fattr must show new size"
        );

        let (bytes, eof) = fs.read(id, 0, 1024).await.expect("read must succeed");
        assert_eq!(bytes.as_slice(), new_data, "read must return new bytes");
        assert!(eof);

        let attr = fs.getattr(id).await.expect("getattr must succeed");
        assert_eq!(
            attr.size,
            new_data.len() as u64,
            "getattr must reflect new size"
        );
        assert_ne!(
            bytes.as_slice(),
            b"hello world",
            "must not return base content"
        );
    }

    #[tokio::test]
    async fn write_at_offset_extends_file() {
        let (fs, _) = make_nfs_writable();
        let id = fs.ids.id_of("README.md").unwrap();
        // "hello world" is 11 bytes; write at offset 11
        let extra = b" extended";
        fs.write(id, 11, extra)
            .await
            .expect("offset write must succeed");
        let (bytes, _) = fs.read(id, 0, 100).await.unwrap();
        assert_eq!(&bytes[..11], b"hello world");
        assert_eq!(&bytes[11..], extra.as_slice());
    }

    // ---- write path (b): create new file ----------------------------------------

    #[tokio::test]
    async fn create_new_file_visible_in_lookup_readdir_read() {
        let (fs, _) = make_nfs_writable();
        let (fid, fattr) = fs
            .create(1, &b"new.txt"[..].into(), sattr3::default())
            .await
            .expect("create must succeed");
        assert!(
            matches!(fattr.ftype, ftype3::NF3REG),
            "created file must be NF3REG"
        );

        // lookup returns same id
        let looked_up = fs.lookup(1, &b"new.txt"[..].into()).await.unwrap();
        assert_eq!(
            looked_up, fid,
            "lookup must return the same fileid as create"
        );

        // getattr
        let attr = fs.getattr(fid).await.unwrap();
        assert!(matches!(attr.ftype, ftype3::NF3REG));

        // write content then read back
        fs.write(fid, 0, b"content").await.unwrap();
        let (bytes, _) = fs.read(fid, 0, 100).await.unwrap();
        assert_eq!(bytes.as_slice(), b"content");

        // parent readdir lists the new file
        let result = fs.readdir(1, 0, 100).await.unwrap();
        let names: Vec<String> = result
            .entries
            .iter()
            .map(|e| String::from_utf8_lossy(&e.name.0).to_string())
            .collect();
        assert!(
            names.contains(&"new.txt".to_owned()),
            "new.txt must appear in readdir"
        );
    }

    // ---- write path (c): create_exclusive ----------------------------------------

    #[tokio::test]
    async fn create_exclusive_on_existing_returns_exist() {
        let (fs, _) = make_nfs_writable();
        let err = fs
            .create_exclusive(1, &b"README.md"[..].into())
            .await
            .unwrap_err();
        assert!(
            matches!(err, nfsstat3::NFS3ERR_EXIST),
            "create_exclusive on existing base file must return EXIST"
        );
    }

    #[tokio::test]
    async fn create_exclusive_fresh_path_succeeds() {
        let (fs, _) = make_nfs_writable();
        let fid = fs
            .create_exclusive(1, &b"brand_new.rs"[..].into())
            .await
            .expect("create_exclusive on fresh path must succeed");
        assert!(fid > 0, "must return a positive fileid");
        let attr = fs.getattr(fid).await.unwrap();
        assert!(matches!(attr.ftype, ftype3::NF3REG));
    }

    // ---- write path (d): mkdir and remove dir ------------------------------------

    #[tokio::test]
    async fn mkdir_then_getattr_and_parent_readdir() {
        let (fs, _) = make_nfs_writable();
        let (dir_fid, dir_attr) = fs
            .mkdir(1, &b"newdir"[..].into())
            .await
            .expect("mkdir must succeed");
        assert!(
            matches!(dir_attr.ftype, ftype3::NF3DIR),
            "mkdir result must be NF3DIR"
        );
        assert_eq!(dir_attr.size, 0);

        let attr2 = fs.getattr(dir_fid).await.unwrap();
        assert!(matches!(attr2.ftype, ftype3::NF3DIR));

        let result = fs.readdir(1, 0, 100).await.unwrap();
        let names: Vec<String> = result
            .entries
            .iter()
            .map(|e| String::from_utf8_lossy(&e.name.0).to_string())
            .collect();
        assert!(
            names.contains(&"newdir".to_owned()),
            "newdir must appear in parent readdir"
        );

        // remove empty dir must succeed
        fs.remove(1, &b"newdir"[..].into())
            .await
            .expect("remove empty dir must succeed");
        let err = fs.getattr(dir_fid).await.unwrap_err();
        assert!(
            matches!(err, nfsstat3::NFS3ERR_NOENT),
            "getattr after remove must return NOENT"
        );
    }

    #[tokio::test]
    async fn remove_nonempty_dir_returns_notempty() {
        let (fs, _) = make_nfs_writable();
        // "src" has children (src/main.rs in base)
        let src_id = fs.ids.id_of("src").unwrap();
        let err = fs.remove(1, &b"src"[..].into()).await.unwrap_err();
        assert!(
            matches!(err, nfsstat3::NFS3ERR_NOTEMPTY),
            "removing non-empty dir must return NOTEMPTY (id={})",
            src_id
        );
    }

    // ---- write path (e): rename file and dir ------------------------------------

    #[tokio::test]
    async fn rename_file_old_gone_new_readable() {
        let (fs, _) = make_nfs_writable();
        let readme_id = fs.ids.id_of("README.md").unwrap();
        fs.rename(1, &b"README.md"[..].into(), 1, &b"NOTES.md"[..].into())
            .await
            .expect("rename must succeed");

        let err = fs.lookup(1, &b"README.md"[..].into()).await.unwrap_err();
        assert!(
            matches!(err, nfsstat3::NFS3ERR_NOENT),
            "old name must be gone"
        );

        let new_id = fs
            .lookup(1, &b"NOTES.md"[..].into())
            .await
            .expect("new name must exist");
        let (bytes, _) = fs.read(new_id, 0, 1024).await.unwrap();
        assert_eq!(
            bytes.as_slice(),
            b"hello world",
            "renamed file must read original bytes"
        );

        // old fileid must also be gone
        let err2 = fs.getattr(readme_id).await.unwrap_err();
        assert!(
            matches!(err2, nfsstat3::NFS3ERR_NOENT),
            "old fileid must return NOENT"
        );
    }

    #[tokio::test]
    async fn rename_dir_subtree_rekeyes_contents() {
        let (fs, _) = make_nfs_writable();
        let src_id = fs.ids.id_of("src").unwrap();
        fs.rename(1, &b"src"[..].into(), 1, &b"lib"[..].into())
            .await
            .expect("dir rename must succeed");

        // Old names gone
        let err = fs.lookup(1, &b"src"[..].into()).await.unwrap_err();
        assert!(matches!(err, nfsstat3::NFS3ERR_NOENT), "src must be gone");

        // New names present
        let lib_id = fs
            .lookup(1, &b"lib"[..].into())
            .await
            .expect("lib must exist");
        let lib_attr = fs.getattr(lib_id).await.unwrap();
        assert!(matches!(lib_attr.ftype, ftype3::NF3DIR));

        let main_id = fs
            .lookup(lib_id, &b"main.rs"[..].into())
            .await
            .expect("lib/main.rs must exist");
        let (bytes, _) = fs.read(main_id, 0, 100).await.unwrap();
        assert_eq!(bytes.as_slice(), b"fn main() {}");

        let _ = src_id; // old id is now tombstoned
    }

    // ---- write path (f): remove file, isolation -----------------------------------

    #[tokio::test]
    async fn remove_file_noent_and_readdir_excludes() {
        let (fs, _) = make_nfs_writable();
        let readme_id = fs.ids.id_of("README.md").unwrap();
        fs.remove(1, &b"README.md"[..].into())
            .await
            .expect("remove must succeed");

        let err = fs.getattr(readme_id).await.unwrap_err();
        assert!(matches!(err, nfsstat3::NFS3ERR_NOENT));
        let err2 = fs.lookup(1, &b"README.md"[..].into()).await.unwrap_err();
        assert!(matches!(err2, nfsstat3::NFS3ERR_NOENT));
        let err3 = fs.read(readme_id, 0, 100).await.unwrap_err();
        assert!(matches!(err3, nfsstat3::NFS3ERR_NOENT));

        let result = fs.readdir(1, 0, 100).await.unwrap();
        let names: Vec<String> = result
            .entries
            .iter()
            .map(|e| String::from_utf8_lossy(&e.name.0).to_string())
            .collect();
        assert!(
            !names.contains(&"README.md".to_owned()),
            "deleted file must not appear in readdir"
        );
    }

    #[tokio::test]
    async fn cross_agent_isolation() {
        // Agent A deletes README.md; agent B must still see it
        let overlay = make_overlay_store();
        let (index_a, store_a, _) = make_fixture();
        let agent_a = overlay.fork(1).expect("fork A");
        let fs_a =
            CasNfs::with_overlay(index_a, Box::new(store_a), Arc::clone(&overlay), agent_a, 1);

        let (index_b, store_b, _) = make_fixture();
        let agent_b = overlay.fork(1).expect("fork B");
        let fs_b =
            CasNfs::with_overlay(index_b, Box::new(store_b), Arc::clone(&overlay), agent_b, 1);

        fs_a.remove(1, &b"README.md"[..].into())
            .await
            .expect("agent A remove must succeed");

        // B should still see README.md
        let id_b = fs_b.ids.id_of("README.md").unwrap();
        let (bytes, _) = fs_b
            .read(id_b, 0, 100)
            .await
            .expect("agent B must still read base file");
        assert_eq!(
            bytes.as_slice(),
            b"hello world",
            "agent B must see base bytes"
        );
    }

    // ---- write path (g): setattr --------------------------------------------------

    #[tokio::test]
    async fn setattr_size_shrink_and_grow() {
        let (fs, _) = make_nfs_writable();
        let id = fs.ids.id_of("README.md").unwrap();

        // Shrink to 5 bytes
        let mut attr = sattr3::default();
        attr.size = set_size3::size(5);
        let fattr = fs
            .setattr(id, attr)
            .await
            .expect("setattr shrink must succeed");
        assert_eq!(fattr.size, 5, "returned fattr must show shrunk size");
        let (bytes, _) = fs.read(id, 0, 100).await.unwrap();
        assert_eq!(bytes.as_slice(), b"hello", "must read back shrunk content");

        // Grow to 10 bytes (zero-fill)
        let mut attr2 = sattr3::default();
        attr2.size = set_size3::size(10);
        let fattr2 = fs
            .setattr(id, attr2)
            .await
            .expect("setattr grow must succeed");
        assert_eq!(fattr2.size, 10);
        let (bytes2, _) = fs.read(id, 0, 100).await.unwrap();
        assert_eq!(bytes2.len(), 10);
        assert_eq!(&bytes2[..5], b"hello");
        assert_eq!(&bytes2[5..], &[0u8; 5]);
    }

    #[tokio::test]
    async fn setattr_mode_reflected_in_fattr() {
        let (fs, _) = make_nfs_writable();
        let id = fs.ids.id_of("README.md").unwrap();
        let mut attr = sattr3::default();
        attr.mode = set_mode3::mode(0o600);
        let fattr = fs
            .setattr(id, attr)
            .await
            .expect("setattr mode must succeed");
        assert_eq!(
            fattr.mode & 0o7777,
            0o600,
            "mode must be reflected in returned fattr"
        );
        let attr2 = fs.getattr(id).await.unwrap();
        assert_eq!(
            attr2.mode & 0o7777,
            0o600,
            "mode must be reflected in subsequent getattr"
        );
    }

    // ---- write path (h): laziness with overlay writes ----------------------------

    #[tokio::test]
    async fn getattr_readdir_with_overlay_writes_zero_base_hydrations() {
        let (index, counting, counter) = make_fixture();
        let overlay = make_overlay_store();
        let agent = overlay.fork(1).expect("fork agent");
        let fs = CasNfs::with_overlay(index, Box::new(counting), Arc::clone(&overlay), agent, 1);

        // Perform an overlay write (creates a new file, no base blob read needed)
        // This internally reads the base blob for RMW -- but only for the written file.
        // Reset counter to 0 after the write to measure only getattr/readdir.
        let (fid, _) = fs
            .create(1, &b"new.txt"[..].into(), sattr3::default())
            .await
            .unwrap();
        fs.write(fid, 0, b"overlay content").await.unwrap();
        counter.store(0, Ordering::Relaxed);

        // getattr on all ids: must not hydrate any blob
        let readme_id = fs.ids.id_of("README.md").unwrap();
        fs.getattr(readme_id).await.unwrap();
        fs.getattr(fid).await.unwrap();
        fs.getattr(1).await.unwrap();
        assert_eq!(
            counter.load(Ordering::Relaxed),
            0,
            "no hydration after getattr with overlay"
        );

        // readdir on root: must not hydrate any blob
        fs.readdir(1, 0, 100).await.unwrap();
        assert_eq!(
            counter.load(Ordering::Relaxed),
            0,
            "no hydration after readdir with overlay"
        );

        // Single read on the overlay-written file: exactly one hydration
        let (bytes, _) = fs.read(fid, 0, 100).await.unwrap();
        assert_eq!(bytes.as_slice(), b"overlay content");
        assert_eq!(
            counter.load(Ordering::Relaxed),
            1,
            "exactly one hydration for overlay read"
        );
    }

    // ---- write path (i): nested rename -------------------------------------------

    #[tokio::test]
    async fn mkdir_create_file_rename_dir() {
        let (fs, _) = make_nfs_writable();

        // mkdir a
        let (a_id, _) = fs.mkdir(1, &b"a"[..].into()).await.expect("mkdir a");
        // create a/x
        let (x_id, _) = fs
            .create(a_id, &b"x"[..].into(), sattr3::default())
            .await
            .expect("create a/x");
        fs.write(x_id, 0, b"x content").await.expect("write a/x");

        // rename a -> b
        fs.rename(1, &b"a"[..].into(), 1, &b"b"[..].into())
            .await
            .expect("rename a -> b");

        // b/x must be readable
        let b_id = fs.lookup(1, &b"b"[..].into()).await.expect("b must exist");
        let bx_id = fs
            .lookup(b_id, &b"x"[..].into())
            .await
            .expect("b/x must exist");
        let (bytes, _) = fs.read(bx_id, 0, 100).await.expect("read b/x");
        assert_eq!(bytes.as_slice(), b"x content");

        // a/x must be gone
        let err = fs.lookup(1, &b"a"[..].into()).await.unwrap_err();
        assert!(matches!(err, nfsstat3::NFS3ERR_NOENT), "a must be gone");
    }

    // ---- updated write-path stub tests (now succeed) ----------------------------

    #[tokio::test]
    async fn write_to_base_file_succeeds() {
        let (fs, _) = make_nfs();
        let id = fs.ids.id_of("README.md").unwrap();
        let fattr = fs
            .write(id, 0, b"new data")
            .await
            .expect("write must succeed");
        assert!(matches!(fattr.ftype, ftype3::NF3REG));
    }

    #[tokio::test]
    async fn mkdir_at_root_succeeds() {
        let (fs, _) = make_nfs();
        let (fid, fattr) = fs
            .mkdir(1, &b"newdir"[..].into())
            .await
            .expect("mkdir must succeed");
        assert!(fid > 0);
        assert!(matches!(fattr.ftype, ftype3::NF3DIR));
    }
}
