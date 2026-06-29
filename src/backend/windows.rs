#![cfg(all(target_os = "windows", feature = "projfs"))]

//! Windows ProjFS (Projected File System) backend.
//!
//! windows crate version: 0.58
//! features: ["Win32_Storage_ProjectedFileSystem", "Win32_Foundation"]
//!
//! This file is excluded from every non-windows build and from windows builds
//! without --features projfs via the file-level inner cfg above.
//!
//! Five mandatory ProjFS callbacks are wired to the pure projfs_logic read core
//! (src/backend/projfs_logic.rs), which is compiled and tested on all targets.
//! The instance-context pointer threads ProjFsCtx through every callback.
//!
//! mount() in src/mount.rs dispatches here; keep mount_windows in lock-step
//! with that dispatch (see the selected_backend() mirror).

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;
use std::sync::Mutex;

use windows::Win32::Foundation::{BOOLEAN, E_INVALIDARG, S_OK};
use windows::Win32::Storage::ProjectedFileSystem::{
    PRJ_CALLBACK_DATA, PRJ_CALLBACKS, PRJ_DIR_ENTRY_BUFFER_HANDLE, PRJ_FILE_BASIC_INFO,
    PRJ_NOTIFICATION, PRJ_NOTIFICATION_PARAMETERS,
    PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_DELETED,
    PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_MODIFIED, PRJ_NOTIFICATION_FILE_OVERWRITTEN,
    PRJ_NOTIFICATION_NEW_FILE_CREATED, PRJ_NOTIFICATION_PRE_DELETE,
    PRJ_PLACEHOLDER_INFO, PrjFillDirEntryBuffer, PrjMarkDirectoryAsPlaceholder,
    PrjStartVirtualizing, PrjWriteFileData, PrjWritePlaceholderInfo,
};
use windows::core::{GUID, HRESULT, PCWSTR};

use crate::backend::projfs_logic::{
    apply_write_notification, enumerate_dir, fs_error_to_win32, placeholder_info, read_file_data,
    ProjFsDirEntry, ProjFsWriteOp, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
};

// HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS): directory is already a virt root.
const HR_ALREADY_EXISTS: HRESULT = HRESULT(0x800700B7u32 as i32);

// PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN = 0x1: client is restarting the scan.
// PRJ_CALLBACK_DATA_FLAGS.0 is i32 in windows-rs 0.58; constant must match.
const FLAG_RESTART_SCAN: i32 = 0x0000_0001;

// ---------------------------------------------------------------------------
// Shared context threaded through ProjFS callbacks via instanceContext pointer
// ---------------------------------------------------------------------------

// All fields are owned by mount_windows (Box::into_raw leak); valid for session.
struct ProjFsCtx {
    index: crate::index::Index,
    store: Box<dyn crate::cas::Store>,
    acl: Vec<crate::acl::AclEntry>,
    // overlay=None means basic read-only mount (no write capture).
    // A real overlay-backed mount threads in Some(OverlayStore) and a valid workspace id.
    overlay: Option<crate::overlay::OverlayStore>,
    workspace: crate::overlay::WorkspaceId,
    agent: crate::overlay::AgentId,
    principal: String,
    // Full path to the virtualization root; used to read materialized file bytes
    // in on_notification (Write path). Only accessed when overlay is Some.
    mountpoint: std::path::PathBuf,
    // Enumeration state: enumeration_id bytes -> (sorted entries, cursor).
    // Mutex makes concurrent ProjFS callback threads safe.
    enum_state: Mutex<HashMap<[u8; 16], (Vec<ProjFsDirEntry>, usize)>>,
}

// ---------------------------------------------------------------------------
// Small helpers (not unsafe; unsafe operations inside explicit unsafe blocks)
// ---------------------------------------------------------------------------

fn guid_to_bytes(g: GUID) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[0..4].copy_from_slice(&g.data1.to_ne_bytes());
    b[4..6].copy_from_slice(&g.data2.to_ne_bytes());
    b[6..8].copy_from_slice(&g.data3.to_ne_bytes());
    b[8..16].copy_from_slice(&g.data4);
    b
}

// Convert a Windows-style path (backslash-separated) to forward-slash UTF-8.
fn win_path_to_str(path: &str) -> String {
    path.replace('\\', "/")
}

// Decode a NUL-terminated PCWSTR to an owned String.
// Returns None if the pointer is null or the UTF-16 is invalid.
fn pcwstr_to_string(s: PCWSTR) -> Option<String> {
    if s.is_null() {
        return None;
    }
    unsafe {
        let len = (0usize..).take_while(|&i| *s.0.add(i) != 0).count();
        String::from_utf16(std::slice::from_raw_parts(s.0, len)).ok()
    }
}

fn path_to_wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let mut w: Vec<u16> = path.as_os_str().encode_wide().collect();
    w.push(0); // NUL terminator required by all Windows string APIs
    w
}

fn current_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn fs_err_to_hresult(e: &crate::fuse::translate::FsError) -> HRESULT {
    HRESULT(fs_error_to_win32(e) as i32)
}

// Recover the shared context from the callback's instance-context pointer.
//
// SAFETY: caller must ensure callback_data is valid and InstanceContext was
// set by mount_windows to a live Box::into_raw(ProjFsCtx) pointer.
unsafe fn ctx_from(callback_data: *const PRJ_CALLBACK_DATA) -> &'static ProjFsCtx {
    &*((*callback_data).InstanceContext as *const ProjFsCtx)
}

// ---------------------------------------------------------------------------
// ProjFS callbacks: five mandatory + one optional notification stub
// ---------------------------------------------------------------------------

// Start a directory enumeration: load, sort, and cache the entry list.
//
// SAFETY: ProjFS guarantees callback_data and enumeration_id are valid for
// the duration of this call.
unsafe extern "system" fn start_dir_enum(
    callback_data: *const PRJ_CALLBACK_DATA,
    enumeration_id: *const GUID,
) -> HRESULT {
    assert!(!callback_data.is_null(), "callback_data must not be null");
    assert!(!enumeration_id.is_null(), "enumeration_id must not be null");

    let ctx = ctx_from(callback_data);
    let raw_path = match pcwstr_to_string((*callback_data).FilePathName) {
        Some(p) => p,
        None => return E_INVALIDARG,
    };
    let path = win_path_to_str(&raw_path);
    let now = current_unix_secs();

    let entries = match enumerate_dir(
        &ctx.index,
        ctx.store.as_ref(),
        None,
        &ctx.acl,
        ctx.agent,
        &ctx.principal,
        now,
        &path,
    ) {
        Ok(e) => e,
        Err(e) => return fs_err_to_hresult(&e),
    };

    let id = guid_to_bytes(*enumeration_id);
    let mut guard = ctx.enum_state.lock().unwrap_or_else(|p| p.into_inner());
    guard.insert(id, (entries, 0));
    S_OK
}

// End a directory enumeration: discard the cached entry list.
//
// SAFETY: ProjFS guarantees pointers are valid for the duration of this call.
unsafe extern "system" fn end_dir_enum(
    callback_data: *const PRJ_CALLBACK_DATA,
    enumeration_id: *const GUID,
) -> HRESULT {
    assert!(!callback_data.is_null(), "callback_data must not be null");
    assert!(!enumeration_id.is_null(), "enumeration_id must not be null");

    let ctx = ctx_from(callback_data);
    let id = guid_to_bytes(*enumeration_id);
    let mut guard = ctx.enum_state.lock().unwrap_or_else(|p| p.into_inner());
    guard.remove(&id);
    S_OK
}

// Fill the directory entry buffer for an in-progress enumeration.
//
// Advances the cursor per successful PrjFillDirEntryBuffer call, stopping
// when the buffer is full (HRESULT error from fill fn). If FLAG_RESTART_SCAN
// is set the cursor is reset to 0 first.
//
// SAFETY: ProjFS guarantees all pointer args are valid for this call duration.
unsafe extern "system" fn get_dir_enum(
    callback_data: *const PRJ_CALLBACK_DATA,
    enumeration_id: *const GUID,
    _search_expression: PCWSTR,
    dir_entry_buffer_handle: PRJ_DIR_ENTRY_BUFFER_HANDLE,
) -> HRESULT {
    assert!(!callback_data.is_null(), "callback_data must not be null");
    assert!(!enumeration_id.is_null(), "enumeration_id must not be null");

    let ctx = ctx_from(callback_data);
    let restart = ((*callback_data).Flags.0 & FLAG_RESTART_SCAN) != 0;
    let id = guid_to_bytes(*enumeration_id);

    let mut guard = ctx.enum_state.lock().unwrap_or_else(|p| p.into_inner());
    let Some((entries, cursor)) = guard.get_mut(&id) else {
        return E_INVALIDARG;
    };

    if restart {
        *cursor = 0;
    }

    // Write entries until the buffer signals full (is_err) or list exhausted.
    while *cursor < entries.len() {
        let entry = &entries[*cursor];
        let mut name_wide: Vec<u16> = entry.name.encode_utf16().collect();
        name_wide.push(0);
        let name_pcwstr = PCWSTR(name_wide.as_ptr());

        let mut info: PRJ_FILE_BASIC_INFO = std::mem::zeroed();
        info.IsDirectory = BOOLEAN(if entry.is_dir { 1 } else { 0 });
        info.FileSize = entry.size as i64;
        info.FileAttributes =
            if entry.is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };

        let hr = PrjFillDirEntryBuffer(name_pcwstr, Some(&info as *const _), dir_entry_buffer_handle);
        if hr.is_err() {
            // Buffer full: keep cursor at this entry; next call resumes here.
            break;
        }
        *cursor += 1;
    }

    S_OK
}

// Deliver placeholder metadata (size, is-dir, attributes) for a path.
//
// SAFETY: ProjFS guarantees callback_data is valid for the duration of this call.
unsafe extern "system" fn get_placeholder_info(
    callback_data: *const PRJ_CALLBACK_DATA,
) -> HRESULT {
    assert!(!callback_data.is_null(), "callback_data must not be null");

    let data = &*callback_data;
    let ctx = ctx_from(callback_data);
    let raw_path = match pcwstr_to_string(data.FilePathName) {
        Some(p) => p,
        None => return E_INVALIDARG,
    };
    let path = win_path_to_str(&raw_path);
    let now = current_unix_secs();

    let attrs = match placeholder_info(
        &ctx.index,
        ctx.store.as_ref(),
        None,
        &ctx.acl,
        ctx.agent,
        &ctx.principal,
        now,
        &path,
    ) {
        Ok(a) => a,
        Err(e) => return fs_err_to_hresult(&e),
    };

    // Build a zeroed placeholder; fill only the fields that matter for
    // the basic read-only case. LARGE_INTEGER time fields stay zero (epoch).
    let mut info: PRJ_PLACEHOLDER_INFO = std::mem::zeroed();
    info.FileBasicInfo.IsDirectory = BOOLEAN(if attrs.is_dir { 1 } else { 0 });
    info.FileBasicInfo.FileSize = attrs.size as i64;
    info.FileBasicInfo.FileAttributes =
        if attrs.is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };

    match PrjWritePlaceholderInfo(
        data.NamespaceVirtualizationContext,
        data.FilePathName,
        &info,
        std::mem::size_of::<PRJ_PLACEHOLDER_INFO>() as u32,
    ) {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    }
}

// Hydrate file content for the requested byte range via PrjWriteFileData.
//
// Reads from the shared read core; delivers clamped bytes (empty if past EOF).
//
// SAFETY: ProjFS guarantees callback_data is valid for the duration of this call.
unsafe extern "system" fn get_file_data(
    callback_data: *const PRJ_CALLBACK_DATA,
    byte_offset: u64,
    length: u32,
) -> HRESULT {
    assert!(!callback_data.is_null(), "callback_data must not be null");

    let data = &*callback_data;
    let ctx = ctx_from(callback_data);
    let raw_path = match pcwstr_to_string(data.FilePathName) {
        Some(p) => p,
        None => return E_INVALIDARG,
    };
    let path = win_path_to_str(&raw_path);
    let now = current_unix_secs();

    let mut bytes = match read_file_data(
        &ctx.index,
        ctx.store.as_ref(),
        None,
        &ctx.acl,
        ctx.agent,
        &ctx.principal,
        now,
        &path,
        byte_offset,
        length as u64,
    ) {
        Ok(b) => b,
        Err(e) => return fs_err_to_hresult(&e),
    };

    if bytes.is_empty() {
        return S_OK; // Past EOF; nothing to write.
    }

    // nyx: bytes.len() <= 128 MiB (projfs_logic cap); u32 cast is safe.
    match PrjWriteFileData(
        data.NamespaceVirtualizationContext,
        &data.DataStreamId,
        bytes.as_mut_ptr() as *mut c_void,
        byte_offset,
        bytes.len() as u32,
    ) {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    }
}

// Capture write/delete filesystem events into the overlay, or accept silently
// when no overlay is configured (basic read-only mount).
//
// SAFETY: pointer args are kernel-supplied and valid for the call duration.
unsafe extern "system" fn on_notification(
    callback_data: *const PRJ_CALLBACK_DATA,
    _is_directory: BOOLEAN,
    notification: PRJ_NOTIFICATION,
    _destination_file_name: PCWSTR,
    _operation_parameters: *mut PRJ_NOTIFICATION_PARAMETERS,
) -> HRESULT {
    assert!(!callback_data.is_null(), "callback_data must not be null");
    let ctx = ctx_from(callback_data);

    // Basic read-only mount: no overlay configured, nothing to capture.
    let ov = match ctx.overlay.as_ref() {
        Some(o) => o,
        None => return S_OK,
    };

    let op = match notification {
        PRJ_NOTIFICATION_FILE_OVERWRITTEN
        | PRJ_NOTIFICATION_NEW_FILE_CREATED
        | PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_MODIFIED => ProjFsWriteOp::Write,
        PRJ_NOTIFICATION_PRE_DELETE | PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_DELETED => {
            ProjFsWriteOp::Delete
        }
        // Any other notification (rename, hardlink, etc.): accept silently.
        _ => return S_OK,
    };

    let raw_path = match pcwstr_to_string((*callback_data).FilePathName) {
        Some(p) => p,
        None => return E_INVALIDARG,
    };
    let path = win_path_to_str(&raw_path);
    let now = current_unix_secs();

    // For Write, read the materialized file bytes from disk under the mountpoint.
    // ProjFS guarantees the file is fully materialized before firing
    // FILE_HANDLE_CLOSED_FILE_MODIFIED or FILE_OVERWRITTEN.
    // nyx: bytes sourced from mountpoint/<raw_path>; Windows-specific, not macOS-verifiable;
    // the pure apply_write_notification logic is tested on macOS.
    let bytes_buf: Option<Vec<u8>> = match op {
        ProjFsWriteOp::Write => {
            let full_path = ctx.mountpoint.join(&raw_path);
            match std::fs::read(&full_path) {
                Ok(b) => Some(b),
                Err(_) => {
                    use crate::fuse::translate::FsError;
                    return fs_err_to_hresult(&FsError::IoError(
                        "failed to read materialized file".into(),
                    ));
                }
            }
        }
        ProjFsWriteOp::Delete => None,
    };

    match apply_write_notification(
        ctx.store.as_ref(),
        ov,
        &ctx.acl,
        ctx.agent,
        ctx.workspace,
        &ctx.principal,
        now,
        &path,
        op,
        bytes_buf.as_deref(),
    ) {
        Ok(()) => S_OK,
        Err(e) => fs_err_to_hresult(&e),
    }
}

// ---------------------------------------------------------------------------
// Callback table
// ---------------------------------------------------------------------------

/// Build a `PRJ_CALLBACKS` table wired to the five mandatory callbacks above.
///
/// QueryFileNameCallback and CancelCommandCallback are left None (optional in
/// the ProjFS contract). NotificationCallback is stubbed (accepts all events).
fn projfs_callbacks() -> PRJ_CALLBACKS {
    PRJ_CALLBACKS {
        StartDirectoryEnumerationCallback: Some(start_dir_enum),
        EndDirectoryEnumerationCallback: Some(end_dir_enum),
        GetDirectoryEnumerationCallback: Some(get_dir_enum),
        GetPlaceholderInfoCallback: Some(get_placeholder_info),
        GetFileDataCallback: Some(get_file_data),
        QueryFileNameCallback: None,
        NotificationCallback: Some(on_notification),
        CancelCommandCallback: None,
    }
}

// ---------------------------------------------------------------------------
// Public mount entry point
// ---------------------------------------------------------------------------

/// Mount the CAS view of `repo` at `mountpoint` via Windows ProjFS.
///
/// Calls PrjMarkDirectoryAsPlaceholder to register the virtualization root,
/// then PrjStartVirtualizing with callbacks wired to the shared read core.
/// Blocks until process termination (no graceful shutdown channel yet).
///
/// mount() in src/mount.rs dispatches here; keep this in lock-step with the
/// selected_backend() cfg cascade in that file.
pub fn mount_windows(repo: &Path, mountpoint: &Path) -> anyhow::Result<()> {
    assert!(repo.is_dir(), "repo must be an existing directory before calling mount_windows");
    assert!(
        mountpoint.is_dir(),
        "mountpoint must be an existing directory before calling mount_windows"
    );

    let core = crate::core::Core::new(repo)?;
    // On Windows, USERNAME is the conventional current-user identity.
    let principal = std::env::var("USERNAME").unwrap_or_else(|_| "SYSTEM".to_string());

    // Heap-allocate the context and intentionally leak it (Box::into_raw).
    // The raw pointer is passed to PrjStartVirtualizing as instanceContext
    // and recovered by every callback; it must outlive the virtualization session.
    // nyx: ctx lives forever (loop below never exits); Box::leak would be clearer
    // but Box::into_raw is used so ctx_ptr is an explicit *mut for casting.
    let ctx_ptr = Box::into_raw(Box::new(ProjFsCtx {
        index: core.index,
        store: core.store,
        acl: Vec::new(),
        // nyx: basic mount has no overlay; a real overlay-backed mount threads in
        // Some(OverlayStore) and a valid workspace id here.
        overlay: None,
        workspace: 0,
        agent: 0,
        principal,
        mountpoint: mountpoint.to_path_buf(),
        enum_state: Mutex::new(HashMap::new()),
    }));

    // Generate a fresh instance GUID via getrandom (BCryptGenRandom on Windows).
    let mut gb = [0u8; 16];
    getrandom::getrandom(&mut gb)
        .map_err(|e| anyhow::anyhow!("getrandom failed: {}", e))?;
    let instance_guid = GUID {
        data1: u32::from_ne_bytes(gb[0..4].try_into().unwrap()),
        data2: u16::from_ne_bytes(gb[4..6].try_into().unwrap()),
        data3: u16::from_ne_bytes(gb[6..8].try_into().unwrap()),
        data4: gb[8..16].try_into().unwrap(),
    };

    // root_wide and callbacks must stay alive for the duration of the session.
    // root_wide backs the PCWSTR passed to ProjFS; callbacks backs the fn-ptr table.
    let root_wide = path_to_wide(mountpoint);
    let root = PCWSTR(root_wide.as_ptr());
    let callbacks = projfs_callbacks();

    // Step 1: Mark mountpoint as the virtualization root.
    // HR_ALREADY_EXISTS means a prior run already marked it; treat as success.
    if let Err(e) = unsafe {
        PrjMarkDirectoryAsPlaceholder(
            root,
            PCWSTR(std::ptr::null::<u16>()),
            None,
            &instance_guid,
        )
    } {
        if e.code() != HR_ALREADY_EXISTS {
            return Err(anyhow::anyhow!("PrjMarkDirectoryAsPlaceholder failed: {}", e));
        }
    }

    // Step 2: Start the ProjFS provider; callbacks are now live.
    // PrjStartVirtualizing returns the virtualization context (windows-rs 0.58 out-param -> Result).
    let _virt_ctx = unsafe {
        PrjStartVirtualizing(
            root,
            &callbacks,
            Some(ctx_ptr as *const c_void),
            None,
        )
    }
    .map_err(|e| anyhow::anyhow!("PrjStartVirtualizing failed: {}", e))?;

    // Block until process is killed or an OS signal triggers shutdown.
    // nyx: ceiling = process lifetime; upgrade path = add a shutdown channel.
    loop {
        std::thread::park();
    }
}
