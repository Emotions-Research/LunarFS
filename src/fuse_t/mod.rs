//! FFI bindings to FUSE-T's high-level libfuse C API (version 2.9).
//!
//! All types, signatures, and field order are derived from the installed headers at
//! /usr/local/include/fuse/{fuse.h,fuse_common.h,fuse_opt.h}. Do not edit field
//! order or pointer types without re-reading those headers -- the ABI is exact.
//!
//! The bitfield block in fuse_operations (flag_nullpath_ok:1 + flag_nopath:1 +
//! flag_utime_omit_ok:1 + flag_reserved:29 = 32 bits) is represented as a single
//! u32. Rust #[repr(C)] inserts 4 bytes of natural padding after that u32 before
//! the next 8-byte-aligned pointer (ioctl), matching the C ABI on LP64 macOS.
#![cfg(all(target_os = "macos", feature = "fuse"))]
#![allow(non_camel_case_types)]

use libc::{c_char, c_int, c_uint, c_ulong, c_void, off_t, size_t};
use libc::{dev_t, gid_t, mode_t, uid_t};
use libc::{flock, stat, statvfs, timespec, utimbuf};

// ---------------------------------------------------------------------------
// Opaque C types -- only valid behind a pointer
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct Fuse {
    _private: [u8; 0],
}

#[repr(C)]
pub struct FuseSession {
    _private: [u8; 0],
}

#[repr(C)]
pub struct FuseChan {
    _private: [u8; 0],
}

#[repr(C)]
pub struct FusePollhandle {
    _private: [u8; 0],
}

#[repr(C)]
pub struct FuseDirhandle {
    _private: [u8; 0],
}

#[repr(C)]
pub struct FuseFs {
    _private: [u8; 0],
}

#[repr(C)]
pub struct FuseCmd {
    _private: [u8; 0],
}

#[repr(C)]
pub struct FuseBufvec {
    _private: [u8; 0],
}

#[repr(C)]
pub struct FuseContext {
    _private: [u8; 0],
}

/// macOS-specific extended attribute set struct (fuse_common.h, __APPLE__ block).
/// Passed as a pointer only; callers use the SETATTR_WANTS_* macros on the C side.
#[repr(C)]
pub struct SetAttrX {
    _private: [u8; 0],
}

// ---------------------------------------------------------------------------
// Type aliases used in struct fuse_operations field signatures
// ---------------------------------------------------------------------------

/// Callback type for fuse_fill_dir_t: add an entry in a readdir() operation.
pub type FuseFillDirT =
    unsafe extern "C" fn(*mut c_void, *const c_char, *const stat, off_t) -> c_int;

/// Deprecated dirhandle type (for getdir field).
pub type FuseDirH = *mut FuseDirhandle;

/// Deprecated dirfil callback type (for getdir field).
pub type FuseDirfilT =
    unsafe extern "C" fn(h: FuseDirH, name: *const c_char, type_: c_int, ino: libc::ino_t) -> c_int;

/// Processing function for fuse_opt_parse.
pub type FuseOptProcT = Option<
    unsafe extern "C" fn(
        data: *mut c_void,
        arg: *const c_char,
        key: c_int,
        outargs: *mut FuseArgs,
    ) -> c_int,
>;

// ---------------------------------------------------------------------------
// struct fuse_file_info (fuse_common.h)
//
// On macOS LP64: flags(4) + pad(4) + fh_old(8) + writepage(4) + bitfield(4) +
//                fh(8) + lock_owner(8) = 40 bytes.
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct FuseFileInfo {
    pub flags: c_int,
    /// Deprecated, do not use.
    pub fh_old: c_ulong,
    pub writepage: c_int,
    /// Packed bitfield: direct_io:1, keep_cache:1, flush:1, nonseekable:1,
    /// flock_release:1, padding:25, purge_attr:1 (macOS), purge_ubc:1 (macOS).
    pub bitfield: u32,
    pub fh: u64,
    pub lock_owner: u64,
}

// ---------------------------------------------------------------------------
// struct fuse_conn_info (fuse_common.h)
//
// On macOS: 10 u32 fields + enable bitfield (u32) + reserved[22] = 128 bytes.
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct FuseConnInfo {
    pub proto_major: c_uint,
    pub proto_minor: c_uint,
    pub async_read: c_uint,
    pub max_write: c_uint,
    pub max_readahead: c_uint,
    /// macOS: anonymous struct with case_insensitive:1, setvolname:1, xtimes:1
    /// packed into one unsigned int. Deprecated -- use capability flags instead.
    pub enable: u32,
    pub capable: c_uint,
    pub want: c_uint,
    pub max_background: c_uint,
    pub congestion_threshold: c_uint,
    pub reserved: [c_uint; 22],
}

// ---------------------------------------------------------------------------
// struct fuse_opt (fuse_opt.h)
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct FuseOpt {
    pub templ: *const c_char,
    pub offset: c_ulong,
    pub value: c_int,
}

// ---------------------------------------------------------------------------
// struct fuse_args (fuse_opt.h)
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct FuseArgs {
    pub argc: c_int,
    pub argv: *mut *mut c_char,
    pub allocated: c_int,
}

// ---------------------------------------------------------------------------
// struct fuse_operations (fuse.h, __APPLE__ variant, libfuse 2.9)
//
// Field order matches the installed header exactly. Function-pointer fields are
// Option<fn> so that None encodes a C null pointer (null-pointer optimisation).
//
// Layout on macOS LP64 (all pointers are 8 bytes):
//   fields 1..38 (fn ptrs)  @ 0..304
//   flags_bitfield (u32)    @ 304   (4 bytes)
//   [4 bytes implicit pad]  @ 308   (aligns ioctl to offset 312)
//   ioctl..fallocate (6 fn) @ 312..360
//   reserved00..fsetattr_x (13 Apple fn ptrs) @ 360..464
//   sizeof = 464
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct FuseOperations {
    // --- standard fields (all platforms) ------------------------------------
    pub getattr: Option<unsafe extern "C" fn(*const c_char, *mut stat) -> c_int>,
    pub readlink: Option<unsafe extern "C" fn(*const c_char, *mut c_char, size_t) -> c_int>,
    /// Deprecated -- use readdir instead.
    pub getdir: Option<unsafe extern "C" fn(*const c_char, FuseDirH, FuseDirfilT) -> c_int>,
    pub mknod: Option<unsafe extern "C" fn(*const c_char, mode_t, dev_t) -> c_int>,
    pub mkdir: Option<unsafe extern "C" fn(*const c_char, mode_t) -> c_int>,
    pub unlink: Option<unsafe extern "C" fn(*const c_char) -> c_int>,
    pub rmdir: Option<unsafe extern "C" fn(*const c_char) -> c_int>,
    pub symlink: Option<unsafe extern "C" fn(*const c_char, *const c_char) -> c_int>,
    pub rename: Option<unsafe extern "C" fn(*const c_char, *const c_char) -> c_int>,
    pub link: Option<unsafe extern "C" fn(*const c_char, *const c_char) -> c_int>,
    pub chmod: Option<unsafe extern "C" fn(*const c_char, mode_t) -> c_int>,
    pub chown: Option<unsafe extern "C" fn(*const c_char, uid_t, gid_t) -> c_int>,
    pub truncate: Option<unsafe extern "C" fn(*const c_char, off_t) -> c_int>,
    /// Deprecated -- use utimens instead.
    pub utime: Option<unsafe extern "C" fn(*const c_char, *mut utimbuf) -> c_int>,
    pub open: Option<unsafe extern "C" fn(*const c_char, *mut FuseFileInfo) -> c_int>,
    pub read: Option<
        unsafe extern "C" fn(*const c_char, *mut c_char, size_t, off_t, *mut FuseFileInfo) -> c_int,
    >,
    pub write: Option<
        unsafe extern "C" fn(
            *const c_char,
            *const c_char,
            size_t,
            off_t,
            *mut FuseFileInfo,
        ) -> c_int,
    >,
    pub statfs: Option<unsafe extern "C" fn(*const c_char, *mut statvfs) -> c_int>,
    pub flush: Option<unsafe extern "C" fn(*const c_char, *mut FuseFileInfo) -> c_int>,
    pub release: Option<unsafe extern "C" fn(*const c_char, *mut FuseFileInfo) -> c_int>,
    pub fsync: Option<unsafe extern "C" fn(*const c_char, c_int, *mut FuseFileInfo) -> c_int>,
    // macOS xattr variants include an extra uint32_t `position` argument (resource fork).
    pub setxattr: Option<
        unsafe extern "C" fn(
            *const c_char,
            *const c_char,
            *const c_char,
            size_t,
            c_int,
            u32,
        ) -> c_int,
    >,
    pub getxattr: Option<
        unsafe extern "C" fn(*const c_char, *const c_char, *mut c_char, size_t, u32) -> c_int,
    >,
    pub listxattr: Option<unsafe extern "C" fn(*const c_char, *mut c_char, size_t) -> c_int>,
    pub removexattr: Option<unsafe extern "C" fn(*const c_char, *const c_char) -> c_int>,
    pub opendir: Option<unsafe extern "C" fn(*const c_char, *mut FuseFileInfo) -> c_int>,
    pub readdir: Option<
        unsafe extern "C" fn(
            *const c_char,
            *mut c_void,
            FuseFillDirT,
            off_t,
            *mut FuseFileInfo,
        ) -> c_int,
    >,
    pub releasedir: Option<unsafe extern "C" fn(*const c_char, *mut FuseFileInfo) -> c_int>,
    pub fsyncdir: Option<unsafe extern "C" fn(*const c_char, c_int, *mut FuseFileInfo) -> c_int>,
    /// Returns user data (private_data in fuse_context); return type differs from others.
    pub init: Option<unsafe extern "C" fn(*mut FuseConnInfo) -> *mut c_void>,
    /// Returns void, not c_int.
    pub destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    pub access: Option<unsafe extern "C" fn(*const c_char, c_int) -> c_int>,
    pub create: Option<unsafe extern "C" fn(*const c_char, mode_t, *mut FuseFileInfo) -> c_int>,
    pub ftruncate: Option<unsafe extern "C" fn(*const c_char, off_t, *mut FuseFileInfo) -> c_int>,
    pub fgetattr:
        Option<unsafe extern "C" fn(*const c_char, *mut stat, *mut FuseFileInfo) -> c_int>,
    pub lock:
        Option<unsafe extern "C" fn(*const c_char, *mut FuseFileInfo, c_int, *mut flock) -> c_int>,
    pub utimens: Option<unsafe extern "C" fn(*const c_char, *const timespec) -> c_int>,
    pub bmap: Option<unsafe extern "C" fn(*const c_char, size_t, *mut u64) -> c_int>,

    // Bitfield block: flag_nullpath_ok:1, flag_nopath:1, flag_utime_omit_ok:1,
    // flag_reserved:29. Total = 32 bits = one unsigned int = u32.
    // #[repr(C)] inserts 4 bytes of padding after this field before ioctl (ptr, align 8).
    pub flags_bitfield: u32,

    // --- added in 2.8 / 2.9 ------------------------------------------------
    pub ioctl: Option<
        unsafe extern "C" fn(
            *const c_char,
            c_int,
            *mut c_void,
            *mut FuseFileInfo,
            c_uint,
            *mut c_void,
        ) -> c_int,
    >,
    pub poll: Option<
        unsafe extern "C" fn(
            *const c_char,
            *mut FuseFileInfo,
            *mut FusePollhandle,
            *mut c_uint,
        ) -> c_int,
    >,
    pub write_buf: Option<
        unsafe extern "C" fn(*const c_char, *mut FuseBufvec, off_t, *mut FuseFileInfo) -> c_int,
    >,
    pub read_buf: Option<
        unsafe extern "C" fn(
            *const c_char,
            *mut *mut FuseBufvec,
            size_t,
            off_t,
            *mut FuseFileInfo,
        ) -> c_int,
    >,
    pub flock: Option<unsafe extern "C" fn(*const c_char, *mut FuseFileInfo, c_int) -> c_int>,
    pub fallocate: Option<
        unsafe extern "C" fn(*const c_char, c_int, off_t, off_t, *mut FuseFileInfo) -> c_int,
    >,

    // --- macOS (__APPLE__) extensions ----------------------------------------
    pub reserved00: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *mut c_void,
            *mut c_void,
            *mut c_void,
            *mut c_void,
            *mut c_void,
            *mut c_void,
            *mut c_void,
        ) -> c_int,
    >,
    pub reserved01: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *mut c_void,
            *mut c_void,
            *mut c_void,
            *mut c_void,
            *mut c_void,
            *mut c_void,
            *mut c_void,
        ) -> c_int,
    >,
    pub renamex: Option<unsafe extern "C" fn(*const c_char, *const c_char, c_uint) -> c_int>,
    pub statfs_x: Option<unsafe extern "C" fn(*const c_char, *mut libc::statfs) -> c_int>,
    pub setvolname: Option<unsafe extern "C" fn(*const c_char) -> c_int>,
    pub exchange: Option<unsafe extern "C" fn(*const c_char, *const c_char, c_ulong) -> c_int>,
    pub getxtimes:
        Option<unsafe extern "C" fn(*const c_char, *mut timespec, *mut timespec) -> c_int>,
    pub setbkuptime: Option<unsafe extern "C" fn(*const c_char, *const timespec) -> c_int>,
    pub setchgtime: Option<unsafe extern "C" fn(*const c_char, *const timespec) -> c_int>,
    pub setcrtime: Option<unsafe extern "C" fn(*const c_char, *const timespec) -> c_int>,
    pub chflags: Option<unsafe extern "C" fn(*const c_char, u32) -> c_int>,
    pub setattr_x: Option<unsafe extern "C" fn(*const c_char, *mut SetAttrX) -> c_int>,
    pub fsetattr_x:
        Option<unsafe extern "C" fn(*const c_char, *mut SetAttrX, *mut FuseFileInfo) -> c_int>,
}

// ---------------------------------------------------------------------------
// extern "C" -- high-level libfuse API (fuse.h, fuse_common.h, fuse_opt.h)
// ---------------------------------------------------------------------------

extern "C" {
    /// The real implementation behind the fuse_main() macro.
    pub fn fuse_main_real(
        argc: c_int,
        argv: *mut *mut c_char,
        op: *const FuseOperations,
        op_size: size_t,
        user_data: *mut c_void,
    ) -> c_int;

    pub fn fuse_new(
        ch: *mut FuseChan,
        args: *mut FuseArgs,
        op: *const FuseOperations,
        op_size: size_t,
        user_data: *mut c_void,
    ) -> *mut Fuse;

    pub fn fuse_destroy(f: *mut Fuse);

    pub fn fuse_loop(f: *mut Fuse) -> c_int;

    pub fn fuse_exit(f: *mut Fuse);

    pub fn fuse_loop_mt(f: *mut Fuse) -> c_int;

    pub fn fuse_get_context() -> *mut FuseContext;

    pub fn fuse_get_session(f: *mut Fuse) -> *mut FuseSession;

    pub fn fuse_mount(mountpoint: *const c_char, args: *mut FuseArgs) -> *mut FuseChan;

    pub fn fuse_unmount(mountpoint: *const c_char, ch: *mut FuseChan);

    pub fn fuse_parse_cmdline(
        args: *mut FuseArgs,
        mountpoint: *mut *mut c_char,
        multithreaded: *mut c_int,
        foreground: *mut c_int,
    ) -> c_int;

    pub fn fuse_daemonize(foreground: c_int) -> c_int;

    pub fn fuse_version() -> c_int;

    pub fn fuse_set_signal_handlers(se: *mut FuseSession) -> c_int;

    pub fn fuse_remove_signal_handlers(se: *mut FuseSession);

    // --- fuse_opt.h ---------------------------------------------------------

    pub fn fuse_opt_parse(
        args: *mut FuseArgs,
        data: *mut c_void,
        opts: *const FuseOpt,
        proc_: FuseOptProcT,
    ) -> c_int;

    pub fn fuse_opt_add_arg(args: *mut FuseArgs, arg: *const c_char) -> c_int;

    pub fn fuse_opt_insert_arg(args: *mut FuseArgs, pos: c_int, arg: *const c_char) -> c_int;

    pub fn fuse_opt_free_args(args: *mut FuseArgs);

    pub fn fuse_opt_add_opt(opts: *mut *mut c_char, opt: *const c_char) -> c_int;

    pub fn fuse_opt_add_opt_escaped(opts: *mut *mut c_char, opt: *const c_char) -> c_int;

    pub fn fuse_opt_match(opts: *const FuseOpt, opt: *const c_char) -> c_int;
}

// ---------------------------------------------------------------------------
// Tests: layout assertions -- no real mount, no fuse_main call.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem;

    unsafe extern "C" fn stub_getattr(_path: *const c_char, _stat: *mut stat) -> c_int {
        0
    }

    unsafe extern "C" fn stub_readdir(
        _path: *const c_char,
        _buf: *mut c_void,
        _filler: FuseFillDirT,
        _offset: off_t,
        _fi: *mut FuseFileInfo,
    ) -> c_int {
        0
    }

    #[test]
    fn fuse_operations_layout() {
        // Size derived from header field-by-field analysis:
        //   38 fn ptrs (offset 0..304) + u32 (304) + 4B pad + 19 fn ptrs (312..464) = 464
        assert_eq!(mem::size_of::<FuseOperations>(), 464);

        // Spot-check offsets of well-known fields.
        assert_eq!(mem::offset_of!(FuseOperations, getattr), 0);
        assert_eq!(mem::offset_of!(FuseOperations, readdir), 208);
        assert_eq!(mem::offset_of!(FuseOperations, flags_bitfield), 304);
        // ioctl at 312 proves the bitfield u32 was sized correctly (not 8B).
        assert_eq!(mem::offset_of!(FuseOperations, ioctl), 312);
        // renamex is the first Apple-extension field after fallocate.
        assert_eq!(mem::offset_of!(FuseOperations, renamex), 376);

        // Construct a zeroed instance and populate two slots.
        // SAFETY: fuse_operations is #[repr(C)] with no invariants; zeroed is valid.
        let mut ops: FuseOperations = unsafe { mem::zeroed() };
        ops.getattr = Some(stub_getattr);
        ops.readdir = Some(stub_readdir);

        assert!(ops.getattr.is_some(), "getattr callback must be Some");
        assert!(ops.readdir.is_some(), "readdir callback must be Some");
        assert!(ops.readlink.is_none(), "untouched slot must be None");
        assert!(ops.mkdir.is_none(), "untouched slot must be None");
    }
}
