# lunar mount: backends and prerequisites

`lunar mount <repo> <mountpoint>` exposes a content-addressed, lazy view of a
repository through the filesystem. File contents are hydrated on first read from
the local CAS; `stat` and `readdir` return instantly with no blob transfers.

Two backends are available, selected at compile time via Cargo features.

---

## Default backend: kernel NFS (`mount-nfs`)

**No external software required.** The `mount-nfs` backend starts a loopback
NFSv3 server in-process, then asks the operating system's built-in kernel NFS
client to mount it. Both macOS and Linux ship with a kernel NFS client; nothing
extra needs to be installed or enabled.

Build (default):

```sh
cargo build
```

Run:

```sh
lunar mount <repo> <mountpoint>
```

The process blocks while serving the mount. Unmount from a second terminal:

```sh
# macOS
umount <mountpoint>
# or, if the above reports "resource busy":
diskutil unmount <mountpoint>

# Linux
umount <mountpoint>
```

---

## Optional backend: FUSE-T (`fuse`)

The `fuse` backend is available as an opt-in fallback for macOS users who
prefer it. It requires [FUSE-T](https://www.fuse-t.org/) to be installed
separately before building.

Install FUSE-T:

```sh
brew install fuse-t
```

Build with the FUSE backend:

```sh
cargo build --features fuse
```

Run:

```sh
cargo run --features fuse -- mount <repo> <mountpoint>
```

FUSE-T places its library at `/usr/local/lib/libfuse-t.dylib`. The build
automatically links against it via `build.rs` (pkg-config probe with a manual
fallback). No reboot or SIP change is needed.

Unmount the same way as above (`umount` or `diskutil unmount`).

---

## Feature summary

| Feature flag         | Backend          | External dependency       | Platforms          |
|----------------------|------------------|---------------------------|--------------------|
| `mount-nfs` (default)| Kernel NFSv3     | None                      | macOS, Linux       |
| `fuse`               | FUSE-T userspace | FUSE-T (`brew install fuse-t`) | macOS only    |

The `hosted` feature (`cargo build --features hosted`) is a separate build
profile for the cloud-hosted variant and does not affect which mount backend is
used.

---

## Smoke tests

The NFS integration test can be run directly:

```sh
cargo test --test nfs_lifecycle
```

The FUSE smoke test is opt-in (it mounts and hammers the filesystem):

```sh
LUNAR_SMOKE=1 cargo test --features fuse --test smoke_fuse_mount -- --nocapture
```

Or via the helper script:

```sh
scripts/smoke-fuse.sh
```

See `docs/macos-fuse.md` for full FUSE-T smoke-test documentation.
