# Windows ProjFS: enable, build, and smoke test

## Overview

`lunar` uses the Windows Projected File System (ProjFS) to present
a virtualized view of a content-addressed repository without materializing file
contents on disk upfront. The provider registers a virtualization root and
returns placeholder file entries during directory enumeration: each placeholder
carries correct size metadata but no byte content. When a process opens and reads
a file for the first time, Windows invokes the provider's `GetFileData` callback,
which streams the real bytes from the local CAS into the file region on demand.
This path is Windows-only and is controlled by the `projfs` Cargo feature.

---

## Enable the ProjFS Windows optional feature

ProjFS is an optional Windows component and must be enabled before the provider
can register a virtualization root.

### PowerShell (run as Administrator)

```powershell
Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS -NoRestart -All
```

Run this in an elevated PowerShell session. The `-NoRestart` flag suppresses the
automatic reboot prompt; add `-Restart` if you want the machine to reboot
automatically. A restart is typically required for the feature to take effect.

### Settings UI

Open **Settings**, then go to **Apps**, then **Optional features**, then
**More Windows features** (or search for "Turn Windows features on or off" in
the Start menu). Scroll to **Windows Projected File System**, check the box, and
click **OK**. A restart may be required.

---

## Build with the projfs feature

```powershell
cargo build --features projfs
```

This activates the `projfs` feature, which pulls in the `windows` crate with the
`Win32_Storage_ProjectedFileSystem` and `Win32_Foundation` feature flags. On
macOS and Linux, `cargo build` (no feature flag) works without touching any
Windows-specific code.

---

## Run the mount and the smoke

The integration smoke test is compile-gated to Windows only (`#![cfg(target_os = "windows")]`
at the top of `tests/projfs_smoke.rs`) and runtime-gated by the `LUNAR_SMOKE`
environment variable. Set the variable to `1` before running, otherwise every
test function returns immediately without touching ProjFS.

### PowerShell

```powershell
$env:LUNAR_SMOKE = "1"
cargo test --features projfs --test projfs_smoke -- --nocapture
```

Or inline:

```powershell
$env:LUNAR_SMOKE="1"; cargo test --features projfs --test projfs_smoke -- --nocapture
```

### What the smoke proves

1. **`projfs_lazy_hydration_byte_correctness`** (lazy hydration, byte-correctness):
   a fixture tree is registered as a virtualization root. The smoke confirms that
   each placeholder file appears in directory listings and that reading the file
   produces exactly the bytes the provider would return via `GetFileData`, with no
   truncation or corruption.
2. **`projfs_concurrent_read_stress`** (concurrent-read stress): multiple threads
   open and read multiple files concurrently. The smoke asserts that every
   concurrent read returns the correct bytes, proving the provider is safe under
   parallel access.

---

## Verification note

CI verifies two distinct things about the ProjFS backend. Do not conflate them.

### Compile gate (runs on every push)

Every push triggers a Windows build of the ProjFS backend on a `windows-latest`
GitHub Actions runner (`.github/workflows/windows.yml`). The step runs
`cargo build --features projfs` and is a required must-pass gate: if the Windows
compile fails, CI fails. This gate runs on every push and confirms the code
compiles on Windows. It does not exercise a real ProjFS mount.

### Live mount smoke (probe-and-skip on GitHub hosted runners)

The `windows.yml` job also attempts to enable the ProjFS optional feature via a
tolerant step (`continue-on-error: true`) and then runs the full smoke suite with
`LUNAR_SMOKE=1`. On GitHub's ephemeral `windows-latest` runners, the enable
step writes the feature to the registry but the feature state reads
`enabled-pending-reboot`. A reboot is not possible on ephemeral runners, so the
ProjFS kernel driver is not activated.

The two mount-dependent tests, `projfs_lazy_hydration_byte_correctness` and
`projfs_concurrent_read_stress`, each begin by calling `projfs_mount_probe()`. That
function attempts a real mount round-trip: spawn `lunar mount`, poll until the
sentinel file appears in the virtual root, then tear down. The verdict derives from
the actual mount attempt, not from reading optional-feature status flags (which
report `enabled-pending-reboot` and cannot be trusted without a reboot). When the
ProjFS kernel driver is not active, the mount attempt fails, the probe returns
`false`, and both tests log a message and return early without running any mount
assertions.

The six tests in `detect::tests` are pure HRESULT classification logic with no
ProjFS kernel dependency. They pass on every runner, including GitHub-hosted ones.

Why `.github/workflows/windows.yml` is GREEN: compile passes + 6 logic tests pass +
2 mount tests skip.

Do not read a green CI run as proof that the live ProjFS mount works. The compile
gate and the six logic tests pass on every push; the two mount-dependent tests skip
on GitHub's ephemeral runners because the ProjFS kernel driver is not active there.

### Getting real live-mount coverage

A full live-mount verification requires one of:

- A real Windows host with the `Client-ProjFS` optional feature enabled and the
  host rebooted after enablement (see
  [Enable the ProjFS Windows optional feature](#enable-the-projfs-windows-optional-feature)
  above). Run the smoke locally as described in
  [Run the mount and the smoke](#run-the-mount-and-the-smoke).
- A self-hosted Windows Actions runner with ProjFS enabled and the host rebooted
  after enablement, so the ProjFS kernel driver is active and
  `projfs_mount_probe()` succeeds.

On either host, `projfs_mount_probe()` completes the mount round-trip, both
`projfs_lazy_hydration_byte_correctness` and `projfs_concurrent_read_stress` run
their full assertion sequences, and the run is a fully verified green.

### macOS dev host behavior

On the macOS dev host, the default gate (`cargo build` and `cargo test` with no
feature flags) stays green because `tests/projfs_smoke.rs` is compile-gated out
entirely on non-Windows targets. The macOS host confirms only that the smoke file
contributes zero compiled tests and that the default build is clean. It never
mounts a real ProjFS volume, and no Windows-specific code runs.

---

## FFI Signature Reference

**Pinned `windows` crate version: `0.58.0`**
Confirmed: 2026-06-28

Sources:
- Primary: actual `windows-rs 0.58.0` source via `raw.githubusercontent.com/microsoft/windows-rs/0.58.0/crates/libs/windows/src/Windows/Win32/Storage/ProjectedFileSystem/mod.rs` (function signatures, where clauses, struct fields confirmed from this file).
- Cross-check: `microsoft.github.io/windows-docs-rs/` (latest/0.62.2) for struct field shapes and callback type aliases. Where 0.62.2 and 0.58.0 differ, the 0.58.0 source is authoritative; differences are called out in the Notes column.
- `docs.rs/windows/0.58.0/` returned HTTP 404 (the `docs.rs` stub redirects to the Microsoft docs site which only serves latest).

**Why this pin matters:** windows-rs changes FFI signatures between minor versions. In 0.58.0, `PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT` is passed through a generic `Param<>` bound in write/stop functions; in 0.62.2 it is taken directly by value. Cargo.toml uses `version = "=0.58.0"` (exact pin) to prevent silent drift between the macOS edit pass and the Windows CI resolver.

---

### Import Paths

All types below are accessible through the top-level `windows` crate. Use these
exact paths in `use` statements.

| Symbol | Import path | Notes |
|--------|-------------|-------|
| `HRESULT` | `windows::core::HRESULT` | NOT `windows::Win32::Foundation::HRESULT` in 0.58.x |
| `Result<T>` | `windows::core::Result<T>` | Wraps `windows::core::Error` on failure |
| `Error` | `windows::core::Error` | Carries HRESULT + optional message |
| `PCWSTR` | `windows::core::PCWSTR` | Pointer to null-terminated wide string |
| `GUID` | `windows::core::GUID` | COM GUID, always in `windows::core` |
| `Param<T>` | `windows::core::Param<T>` | Generic bound trait (replaces `IntoParam<T>` pre-0.58) |
| `BOOLEAN` | `windows::Win32::Foundation::BOOLEAN` | Win32 u8-newtype; requires `Win32_Foundation` feature |
| ProjFS types | `windows::Win32::Storage::ProjectedFileSystem::*` | Requires `Win32_Storage_ProjectedFileSystem` feature |

Typical `use` block for a ProjFS provider:

```rust
use windows::core::{GUID, HRESULT, PCWSTR, Param, Result};
use windows::Win32::Foundation::BOOLEAN;
use windows::Win32::Storage::ProjectedFileSystem::{
    PRJ_CALLBACKS, PRJ_CALLBACK_DATA, PRJ_FILE_BASIC_INFO,
    PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT, PRJ_NOTIFICATION_MAPPING,
    PRJ_PLACEHOLDER_INFO, PRJ_PLACEHOLDER_VERSION_INFO,
    PRJ_STARTVIRTUALIZING_FLAGS, PRJ_STARTVIRTUALIZING_OPTIONS,
    PrjMarkDirectoryAsPlaceholder, PrjStartVirtualizing,
    PrjStopVirtualizing, PrjWriteFileData, PrjWritePlaceholderInfo,
};
```

---

### Function Signatures

All functions are `pub unsafe fn` and live in `windows::Win32::Storage::ProjectedFileSystem`.
Return type `Result<T>` is `windows::core::Result<T>`.

| Function | Full Rust Signature | Return Type | Notes |
|----------|---------------------|-------------|-------|
| `PrjStartVirtualizing` | `pub unsafe fn PrjStartVirtualizing<P0>(virtualizationrootpath: P0, callbacks: *const PRJ_CALLBACKS, instancecontext: Option<*const core::ffi::c_void>, options: Option<*const PRJ_STARTVIRTUALIZING_OPTIONS>) -> Result<PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT> where P0: Param<PCWSTR>` | `Result<PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT>` | `callbacks` is NOT Option-wrapped (must be a valid pointer); `instancecontext` and `options` are `Option<*const ...>`; confirmed from 0.58.0 source |
| `PrjStopVirtualizing` | `pub unsafe fn PrjStopVirtualizing<P0>(namespacevirtualizationcontext: P0) where P0: Param<PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT>` | `()` (infallible) | Context via `Param<>` generic in 0.58.0; in 0.62.2 it takes the value directly. No return value -- cannot fail once started. |
| `PrjMarkDirectoryAsPlaceholder` | `pub unsafe fn PrjMarkDirectoryAsPlaceholder<P0, P1>(rootpathname: P0, targetpathname: P1, versioninfo: Option<*const PRJ_PLACEHOLDER_VERSION_INFO>, virtualizationinstanceid: *const GUID) -> Result<()> where P0: Param<PCWSTR>, P1: Param<PCWSTR>` | `Result<()>` | `versioninfo` is Option-wrapped; `virtualizationinstanceid` is a raw pointer (not Option); confirmed from 0.58.0 source |
| `PrjWritePlaceholderInfo` | `pub unsafe fn PrjWritePlaceholderInfo<P0, P1>(namespacevirtualizationcontext: P0, destinationfilename: P1, placeholderinfo: *const PRJ_PLACEHOLDER_INFO, placeholderinfosize: u32) -> Result<()> where P0: Param<PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT>, P1: Param<PCWSTR>` | `Result<()>` | In 0.58.0 context is generic `Param<>` (key difference from 0.62.2 where it is taken by value); confirmed from 0.58.0 source |
| `PrjWriteFileData` | `pub unsafe fn PrjWriteFileData<P0>(namespacevirtualizationcontext: P0, datastreamid: *const GUID, buffer: *const core::ffi::c_void, byteoffset: u64, length: u32) -> Result<()> where P0: Param<PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT>` | `Result<()>` | In 0.58.0 context is generic `Param<>`; `datastreamid` is raw `*const GUID` (not Option); `buffer` is raw `*const c_void`; confirmed from 0.58.0 source |

---

### Struct Reference

All structs are `#[repr(C)]` and implement `Copy + Clone + Debug + Default + PartialEq`.

#### `PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT`

Import: `windows::Win32::Storage::ProjectedFileSystem::PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT`

```rust
pub struct PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT(pub *mut core::ffi::c_void);
```

Newtype around a raw Win32 handle pointer. Returned by `PrjStartVirtualizing`.
Passed to write/stop functions via `Param<>` bound in 0.58.0.

---

#### `PRJ_CALLBACKS`

Import: `windows::Win32::Storage::ProjectedFileSystem::PRJ_CALLBACKS`

```rust
pub struct PRJ_CALLBACKS {
    pub StartDirectoryEnumerationCallback: PRJ_START_DIRECTORY_ENUMERATION_CB,
    pub EndDirectoryEnumerationCallback:   PRJ_END_DIRECTORY_ENUMERATION_CB,
    pub GetDirectoryEnumerationCallback:   PRJ_GET_DIRECTORY_ENUMERATION_CB,
    pub GetPlaceholderInfoCallback:        PRJ_GET_PLACEHOLDER_INFO_CB,
    pub GetFileDataCallback:               PRJ_GET_FILE_DATA_CB,
    pub QueryFileNameCallback:             PRJ_QUERY_FILE_NAME_CB,
    pub NotificationCallback:              PRJ_NOTIFICATION_CB,
    pub CancelCommandCallback:             PRJ_CANCEL_COMMAND_CB,
}
```

Each field is a type alias for `Option<unsafe extern "system" fn(...) -> HRESULT>`.
Selected aliases (source: microsoft.github.io/windows-docs-rs, confirmed stable across 0.58/0.62):

```rust
pub type PRJ_GET_PLACEHOLDER_INFO_CB =
    Option<unsafe extern "system" fn(callbackdata: *const PRJ_CALLBACK_DATA) -> HRESULT>;

pub type PRJ_GET_FILE_DATA_CB =
    Option<unsafe extern "system" fn(callbackdata: *const PRJ_CALLBACK_DATA, byteoffset: u64, length: u32) -> HRESULT>;
```

Note: callbacks return raw `HRESULT`, not `windows::core::Result<()>`. The `Result<>` wrapper
is only applied at the public call-site wrappers (`PrjWriteFileData`, etc.), not in callback implementations.

---

#### `PRJ_FILE_BASIC_INFO`

Import: `windows::Win32::Storage::ProjectedFileSystem::PRJ_FILE_BASIC_INFO`

```rust
pub struct PRJ_FILE_BASIC_INFO {
    pub IsDirectory:    BOOLEAN,  // windows::Win32::Foundation::BOOLEAN (u8 newtype)
    pub FileSize:       i64,
    pub CreationTime:   i64,      // FILETIME as i64 (100ns intervals since 1601-01-01)
    pub LastAccessTime: i64,
    pub LastWriteTime:  i64,
    pub ChangeTime:     i64,
    pub FileAttributes: u32,      // Win32 file attribute flags (FILE_ATTRIBUTE_*)
}
```

---

#### `PRJ_PLACEHOLDER_VERSION_INFO`

Import: `windows::Win32::Storage::ProjectedFileSystem::PRJ_PLACEHOLDER_VERSION_INFO`

```rust
pub struct PRJ_PLACEHOLDER_VERSION_INFO {
    pub ProviderID: [u8; 128],
    pub ContentID:  [u8; 128],
}
```

---

#### `PRJ_PLACEHOLDER_INFO`

Import: `windows::Win32::Storage::ProjectedFileSystem::PRJ_PLACEHOLDER_INFO`

```rust
pub struct PRJ_PLACEHOLDER_INFO {
    pub FileBasicInfo:       PRJ_FILE_BASIC_INFO,
    pub EaInformation:       PRJ_PLACEHOLDER_INFO_0,  // { EaBufferSize: u32, OffsetToFirstEa: u32 }
    pub SecurityInformation: PRJ_PLACEHOLDER_INFO_1,  // { SecurityBufferSize: u32, OffsetToSecurityDescriptor: u32 }
    pub StreamsInformation:  PRJ_PLACEHOLDER_INFO_2,  // { StreamsInfoBufferSize: u32, OffsetToFirstStreamInfo: u32 }
    pub VersionInfo:         PRJ_PLACEHOLDER_VERSION_INFO,
    pub VariableData:        [u8; 1],                 // trailing flexible data; size passed separately
}
```

Sub-struct shapes (windows-rs generates numbered types for C anonymous inner structs):

```rust
pub struct PRJ_PLACEHOLDER_INFO_0 { pub EaBufferSize: u32, pub OffsetToFirstEa: u32 }
pub struct PRJ_PLACEHOLDER_INFO_1 { pub SecurityBufferSize: u32, pub OffsetToSecurityDescriptor: u32 }
pub struct PRJ_PLACEHOLDER_INFO_2 { pub StreamsInfoBufferSize: u32, pub OffsetToFirstStreamInfo: u32 }
```

Pass `size_of::<PRJ_PLACEHOLDER_INFO>() as u32` as `placeholderinfosize` when the
`VariableData` area is unused (EA / security / stream fields all zeroed).

---

#### `PRJ_STARTVIRTUALIZING_OPTIONS`

Import: `windows::Win32::Storage::ProjectedFileSystem::PRJ_STARTVIRTUALIZING_OPTIONS`

```rust
pub struct PRJ_STARTVIRTUALIZING_OPTIONS {
    pub Flags:                    PRJ_STARTVIRTUALIZING_FLAGS,
    pub PoolThreadCount:          u32,
    pub ConcurrentThreadCount:    u32,
    pub NotificationMappings:     *mut PRJ_NOTIFICATION_MAPPING,
    pub NotificationMappingsCount: u32,
}
```

Note: does NOT implement `Send`/`Sync` (contains a raw pointer). Pass via
`Option<*const PRJ_STARTVIRTUALIZING_OPTIONS>` to `PrjStartVirtualizing`.

---

### Version delta: 0.58.0 vs 0.62.2

The signatures retrieved from the Microsoft docs site (microsoft.github.io) reflect
the latest crate (0.62.2). This table records confirmed differences:

| Symbol | 0.58.0 (pinned, authoritative) | 0.62.2 (latest docs) |
|--------|-------------------------------|----------------------|
| `PrjWritePlaceholderInfo` context param | `P0: Param<PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT>` | taken by value `PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT` |
| `PrjWriteFileData` context param | `P0: Param<PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT>` | taken by value |
| `PrjStopVirtualizing` context param | `P0: Param<PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT>` | taken by value |
| `PrjWritePlaceholderInfo` generic count | 2 (`P0`, `P1`) | 1 (`P1` for filename only) |

Code increments that follow MUST use the 0.58.0 column. Do not copy the latest docs signatures.

---

## Gotchas for future maintainers

### 1. Callbacks must return bare HRESULT, not Result

ProjFS callbacks (registered in `PRJ_CALLBACKS`) must be declared as
`unsafe extern "system" fn(...) -> HRESULT`. Do NOT return `windows::core::Result<()>` --
that is the return type of the public call-site wrappers (`PrjWriteFileData`, etc.), not
of the callbacks themselves. Convert: use `S_OK` for success, and `e.code()` to extract
the raw `HRESULT` from a `windows::core::Error`.

```rust
// Correct pattern inside a callback:
match PrjWriteFileData(...) {
    Ok(()) => S_OK,
    Err(e) => e.code(),
}
```

### 2. Option<*const T> pointer wrapping rules

In windows-rs 0.58, some ProjFS function parameters are typed `Option<*const T>`:

| Parameter | Type | Correct call-site value |
|-----------|------|------------------------|
| `PrjFillDirEntryBuffer` `filebasicinfo` | `Option<*const PRJ_FILE_BASIC_INFO>` | `Some(&info as *const _)` |
| `PrjMarkDirectoryAsPlaceholder` `versioninfo` | `Option<*const PRJ_PLACEHOLDER_VERSION_INFO>` | `None` if unused |
| `PrjStartVirtualizing` `instancecontext` | `Option<*const c_void>` | `Some(ctx_ptr as *const c_void)` |
| `PrjStartVirtualizing` `options` | `Option<*const PRJ_STARTVIRTUALIZING_OPTIONS>` | `None` if unused |

Do NOT pass `std::ptr::null()` to an `Option<*const T>` parameter -- that is a type
error (`*const _` does not coerce to `Option<*const _>`). Use `None` instead.

Pass `Some(&value as *const _)` only when the binding's parameter type is literally
`Option<*const T>`. If the parameter is a raw `*const T` (no `Option`), pass
`&value as *const _` directly with no `Some(...)`.

### 3. HRESULT import path

In windows-rs 0.58, `HRESULT` lives in `windows::core`, not `windows::Win32::Foundation`.
Importing from `Win32::Foundation` causes E0432 (unresolved import).

```rust
// Correct:
use windows::core::HRESULT;

// Wrong (E0432 in windows-rs 0.58):
use windows::Win32::Foundation::HRESULT;
```

`E_INVALIDARG`, `S_OK`, and `BOOLEAN` remain in `windows::Win32::Foundation`.

### 4. Flag constant types: PRJ_CALLBACK_DATA_FLAGS.0 is i32

`PRJ_CALLBACK_DATA_FLAGS` (the type of `PRJ_CALLBACK_DATA.Flags`) wraps `i32` in
windows-rs 0.58. Flag constants that are ANDed against `.0` must therefore be `i32`
as well -- declaring them `u32` produces E0308 (mismatched types) and E0277
(no BitAnd implementation for `i32` and `u32`).

```rust
// Correct:
const FLAG_RESTART_SCAN: i32 = 0x0000_0001;
let restart = ((*callback_data).Flags.0 & FLAG_RESTART_SCAN) != 0;

// Wrong -- E0277/E0308 on Windows:
const FLAG_RESTART_SCAN: u32 = 0x00000001;
```
