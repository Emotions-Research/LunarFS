# macOS FUSE-T: build, run, and smoke test

## Overview

`lunar mount` exposes a content-addressed, lazy view of a local repository
through the filesystem. On macOS it uses **FUSE-T**, a kext-free FUSE
implementation. Instead of loading a kernel extension the way macFUSE does,
FUSE-T runs an in-userspace NFSv4 server (go-nfsv4) and presents the mount
through macOS's built-in NFSv4 client. No kernel extension approval, no
Security & Privacy pop-up.

File contents are hydrated lazily from the CAS: `stat` reports the correct
metadata but reads zero bytes until the file is actually opened and read,
at which point the content is fetched from the local CAS and returned to the
caller.

The mount is **read-only**. Only `getattr`, `open`, `read`, and `readdir` are
registered.

---

## Prerequisites

### FUSE-T

Install FUSE-T before building with `--features fuse`. The recommended source is
the FUSE-T project releases page or the community Homebrew tap
(`homebrew-tap/fuse-t`). After installation, FUSE-T places:

- `libfuse-t.dylib` at `/usr/local/lib/libfuse-t.dylib`
- Headers at `/usr/local/include/fuse/fuse.h`,
  `/usr/local/include/fuse/fuse_common.h`, and
  `/usr/local/include/fuse/fuse_opt.h`

Verify the dylib is present:

```sh
ls /usr/local/lib/libfuse-t.dylib
```

### Rust toolchain

Any recent stable Rust toolchain works. The crate targets the 2021 edition.

---

## Build

```sh
cargo build --features fuse
```

### Link wiring (build.rs)

When `--features fuse` is passed on macOS, `build.rs` resolves the FUSE-T
library in two steps:

1. **pkg-config probe**: it runs `pkg_config::Config::new().atleast_version("1.0").probe("fuse-t")`.
   If pkg-config finds the library, Cargo's pkg-config integration handles the
   link flags automatically.

2. **Manual fallback**: if pkg-config does not resolve `fuse-t`, `build.rs`
   emits:
   ```
   cargo:rustc-link-search=native=/usr/local/lib
   cargo:rustc-link-lib=dylib=fuse-t
   ```
   which instructs the linker to look in `/usr/local/lib` and link `libfuse-t.dylib`.

3. **rpath (always emitted)**: regardless of which path succeeded above,
   `build.rs` always emits:
   ```
   cargo:rustc-link-arg=-Wl,-rpath,/usr/local/lib
   ```
   This bakes `/usr/local/lib` into the binary's runtime search path so
   `libfuse-t.dylib` is found at runtime without any `DYLD_LIBRARY_PATH`
   configuration. Do not set `DYLD_LIBRARY_PATH` for this binary; it is not
   needed and will be ignored.

---

## Run

Create an empty directory for the mount point, then run:

```sh
lunar mount <repo> <mountpoint>
```

From source:

```sh
cargo run --features fuse -- mount <repo> <mountpoint>
```

- `<repo>` must be an existing directory containing the repository to mount.
- `<mountpoint>` must be an existing, empty directory.
- The mount is **read-only**. Writes through the mount point are rejected.
- The process **blocks** while serving the mount. The terminal will not return
  to a prompt until the mount is torn down.

### Unmounting

In a separate terminal:

```sh
umount <mountpoint>
```

If `umount` returns an error (resource busy or not a mount point):

```sh
diskutil unmount <mountpoint>
```

Either command signals the FUSE-T backend to stop, which causes `fuse_main_real`
to return and the `LunarFS` process to exit cleanly.

---

## Gated smoke test

The smoke test is **never** part of the default deterministic gate. It only runs
when you explicitly opt in with `LUNAR_SMOKE=1`. Without that variable the
test returns immediately (no mount, no filesystem access, no output).

### What the smoke does

1. Creates a small fixture repository and mounts it via the real FUSE-T backend.
2. Reads every file through the mount point and compares the bytes to the
   original CAS source, verifying lazy-hydration correctness.
3. Spawns N concurrent reader goroutines for the configured duration, hammering
   the mounted tree with reads.
4. Reports reads per second, total error count, and whether the mount is still
   serving at the end of the run (mount survival).

### Running the smoke

Via the provided script (mirrors `scripts/smoke-remote.sh` style):

```sh
scripts/smoke-fuse.sh
```

Or directly:

```sh
LUNAR_SMOKE=1 cargo test --features fuse --test smoke_fuse_mount -- --nocapture
```

### Env knobs

| Variable                        | Default | Cap | Description                         |
|---------------------------------|---------|-----|-------------------------------------|
| `LUNAR_SMOKE`              | unset   | n/a | Must equal `1` to run (gate)        |
| `LUNAR_SMOKE_READERS`      | 16      | 256 | Concurrent reader count             |
| `LUNAR_SMOKE_DURATION_SECS`| 60      | 600 | Stress-run duration in seconds      |

---

## Verification gate

The following three commands must all be green before committing. The smoke is
skipped (not mounted) in all three:

```sh
cargo test
cargo build --features fuse
cargo clippy --features fuse -- -D warnings
```

The fourth command below verifies the smoke test file compiles on the fuse
feature path without actually running the mount:

```sh
cargo test --features fuse --no-run
```

---

## Operator note

The automated gate above never mounts. To capture real performance numbers
(reads/sec, error count, mount survival) on this Mac, run the smoke explicitly:

```sh
LUNAR_SMOKE=1 cargo test --features fuse --test smoke_fuse_mount -- --nocapture
```

Or use `scripts/smoke-fuse.sh`, which sets the variable and passes `--nocapture`
for you. Run this after each substantive change to the FUSE read path to confirm
the live numbers look correct.
