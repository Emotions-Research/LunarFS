# LunarFS Multi-Device Test Results

Three independent "devices" (separate local CAS state) syncing through one
LunarFS server, plus a speed benchmark vs git/rsync. The Docker harness in this
directory (`run.sh`) builds the same topology in containers; these results were
produced by the no-Docker equivalent (`run_local.py`: one `lunar serve` + three
device HOME dirs against the current-source binary) because the local Docker
registry was timing out. Same proof, same code paths.

## Sync scenarios (all PASS)

| Scenario | What it tests | Result |
|----------|---------------|--------|
| 1. Fan-out | dev1 pushes; dev2 + dev3 pull | All three converge to the **identical** content-addressed root (`5ade4022…`). Content-addressing means same bytes ⇒ same hash on every machine. |
| 2. Concurrent disjoint edits | dev1/dev2/dev3 each edit a different file and push **simultaneously** | All three produced distinct roots; the workspace ref advances **last-writer-wins** (dev2 won this run). Every pushed tree stays in the append-only CAS, so **no data is destroyed**. |
| 3. Same-file conflict | dev1 and dev2 edit the **same** file differently and push together | Last-writer-wins on the ref; **both versions remain retrievable** by content hash in the store. |
| 4. Convergence | all devices pull after the dust settles | All reach a consistent view (pull rc=0). |

### Concurrency semantics (honest)
The default client push is **unconditional last-writer-wins**: it sends the new
root without an `expected_root`, so the server advances the ref to whoever writes
last. Because the store is **content-addressed and append-only**, the "losing"
tree is never deleted, it just isn't the current ref, and it stays fetchable by
its hash. True auto-conflict-copies (server keeps both as named refs) would need
the client to send `expected_root` for a compare-and-swap on `PUT /v1/ref/:ws`,
a small follow-up. For the "one developer, many machines" case this is exactly
the right default; for multi-writer teams, CAS-guarded refs are the next step.

## Speed benchmark

Repo: **805 files, 1.1 MB** (mostly many tiny files, node_modules-style).

| Operation | LunarFS | git / rsync | Result |
|-----------|---------|-------------|--------|
| **Fork a workspace** | `lunar ws fork` **3.7 ms** | `git worktree add` 108.6 ms | **29x faster** |
| Distribute to a 2nd machine (eager full copy) | `lunar pull` 522 ms | `git clone` 413 ms / `rsync` 192 ms | comparable |

### Reading the numbers
- **Fork is the headline: 29x faster**, and it's **O(1)** (copies the root ref,
  not the bytes), so the gap widens with repo size and with the number of
  workspaces. This is the agent-fleet use case: spin up N isolated workspaces
  instantly.
- The **eager full pull** is intentionally not LunarFS's fast path. LunarFS is
  **lazy**: you `mount` (instant, no bytes moved) and files hydrate **on first
  read**. You almost never bulk-download a whole tree, so comparing a cold full
  pull of a tiny repo over HTTP against git's packed transfer is the worst case
  for LunarFS and it's still in the same ballpark. On large repos where you touch
  a fraction of the files, lazy hydration wins decisively.

## Reproduce
- No Docker: `python3 test/multidevice/run_local.py`
- Docker (1 server + 3 device containers): `bash test/multidevice/run.sh`

---

# Auto-sync + CAS-conflict multidevice results

## 2026-06-29 auto-sync convergence and CAS conflict scenarios

Environment: macOS 15.6, 10 logical CPUs, rustc 1.96.0
Driver: `python3 test/multidevice/run_local.py` (no args)

All 6 scenarios PASS. Full JSON output appended below.

### Scenario 5: Auto-sync convergence (no explicit `lunar push`)

Device A uses `lunar sync <workspace> <dir> --once` (the auto-sync one-shot mode).
The test modifies three files in A's workspace (edit README.md, create new_file.txt,
delete delete_me.txt) then invokes `lunar sync --once` on A. No `lunar push` is
called anywhere in the test body. Device B then runs `lunar pull` and must converge
to A's content.

Convergence assertion: walk both A's filesystem and B's materialized CAS tree,
build sorted `{relpath: sha256(bytes)}` maps (excluding `.lunar/` metadata dir),
assert equal. On failure the first differing relpath is reported.

| Metric | Value |
|--------|-------|
| sync exit code | 0 |
| server ref advanced | yes |
| B converged to new ref | yes |
| paths match (A fs vs B CAS) | yes |
| hashes match | yes |
| **End-to-end propagation latency** | **9 ms** |

Latency measured as wall-clock from the moment the last file write completes in A to
the moment `lunar sync --once` returns (which means the server ref has been updated
and B can pull). With `--once`, there is no debounce window; the entire
snapshot-upload-commit pipeline runs in one blocking call. The 9 ms is essentially
one HTTP round-trip for `missing_blobs` + blob uploads + `put_ref` CAS over localhost.
For the continuous watch mode (`lunar sync` without `--once` or `lunar autosync`),
the propagation latency includes the configurable debounce window (default 800 ms)
plus the same upload time.

A paths after sync: README.md, new_file.txt, src/main.py
B paths after pull: README.md, new_file.txt, src/main.py

### Scenario 6: Concurrent-edit CAS conflict — both versions preserved

Device A edits `shared.txt` and pushes (CAS-commit: expected=R0, server advances to R_A).
Device B independently edits `shared.txt` (diverged from R0, never saw A's push).
B uploads its blobs to the server, then attempts `PUT /v1/ref/:ws` with
`expected_root=R0` (stale). The server is at R_A, so the CAS attempt fails.

**Observed conflict representation**: HTTP 409 response with JSON body:
```json
{"conflict_ref": "<workspace>@conflict-<first 8 hex chars of B_root>", "current_root": "<R_A_hex>"}
```
The server saves B's root at `store/ref/<conflict_ref>`. The winner (A's) ref is
not clobbered: `GET /v1/ref/<workspace>` still returns R_A.

Both versions are preserved: B's blobs are uploaded *before* the CAS attempt (by
`push_cas`), so all of B's file content is durably on the server even after the 409.
A HEAD request to each of B's blob hashes returns 200/204.

| Check | Result |
|-------|--------|
| HTTP status on B's stale push | 409 |
| conflict_ref follows naming convention | yes (`conflict@conflict-95cd22d2`) |
| conflict ref stores B's root | yes |
| main ref is still A's root | yes |
| A's root committed before conflict | yes |
| B's blobs present on server after 409 | yes |

### CLI mapping note

`lunar sync <workspace> <path> --once` is the auto-sync one-shot command (not `lunar push`).
It uses the `AutoSyncEngine::run_once()` pipeline: walk directory into local CAS,
compute missing blobs, upload, CAS-advance the workspace ref.
`lunar autosync <workspace> <dir>` is the continuous background variant with a configurable
debounce window.

### Bug fixed during this run

`do_sync` and `do_autosync` in `src/main.rs` previously reused the same `reqwest::Client`
across two separate tokio runtimes: a short-lived `seed_rt` (for the initial `get_ref`)
and the uploader's internal runtime. reqwest's connection pool added the keep-alive
connection to `seed_rt`'s reactor; when `seed_rt`'s `block_on` returned, hyper's
background polling task for that connection was dropped. The uploader's runtime then
tried to reuse the dead pooled connection, causing `push_cas` to hang indefinitely
(TCP connection established, zero bytes in flight). Fixed by creating a fresh
`HttpRemote` (fresh `reqwest::Client`) for the uploader so each runtime owns its own
connection pool.

### Run command

```
python3 test/multidevice/run_local.py
```

(Port is now allocated dynamically via `socket.bind(("127.0.0.1", 0))` to avoid
conflicts with other test runs. ROOT remains `/tmp/mdtest`.)

### Status: GREEN / PASS (all 6 scenarios)

---

# Multi-device pull timing results

## 2026-06-29 parallel blob transfer

Environment: macOS 15.6, 10 logical CPUs, rustc 1.96.0, cargo 1.96.0
Driver: test/multidevice/run_local.py (no args)
Method: BEFORE = LUNAR_TRANSFER_CONCURRENCY=1 (serial Phase 2 file fetches); AFTER = default env (LUNAR_TRANSFER_CONCURRENCY=24, parallel). Driver run 3 times per config; each run internally times 2 reps (reps=2 via timed()) and reports the median. The three per-config medians are sorted and the center value is the reported median below.

| Config            | Runs | Median pull (s) | Min    | Max    |
|-------------------|------|-----------------|--------|--------|
| BEFORE (serial)   | 3    | 0.490           | 0.482  | 0.495  |
| AFTER (parallel)  | 3    | 0.358           | 0.358  | 0.414  |

Speedup: 1.37x (490.3ms / 358.3ms)

### Build and test status (same commit)

| Feature config       | cargo build | cargo test            |
|----------------------|-------------|-----------------------|
| default (no flags)   | exit 0      | 238 lib + 29 integration tests, all ok |
| --features hosted    | exit 0      | 299 lib + 29+ integration tests, all ok |

### Raw timing lines (one BEFORE and one AFTER run for traceability)

BEFORE (LUNAR_TRANSFER_CONCURRENCY=1):
```
  2nd-machine get: lunar pull = 494.6ms  vs  git clone = 406.6ms  vs  rsync = 158.7ms
  2nd-machine get: lunar pull = 490.3ms  vs  git clone = 428.0ms  vs  rsync = 156.2ms
  2nd-machine get: lunar pull = 481.9ms  vs  git clone = 409.0ms  vs  rsync = 158.9ms
```

AFTER (parallel default, LUNAR_TRANSFER_CONCURRENCY=24):
```
  2nd-machine get: lunar pull = 413.8ms  vs  git clone = 416.5ms  vs  rsync = 154.6ms
  2nd-machine get: lunar pull = 358.3ms  vs  git clone = 407.2ms  vs  rsync = 157.3ms
  2nd-machine get: lunar pull = 357.9ms  vs  git clone = 409.0ms  vs  rsync = 155.8ms
```

### Notes
- Phase 1 (tree blob DFS) is always serial because each tree must be parsed to discover its children. Only Phase 2 (file blob fetching via buffer_unordered) is parallelized. For this 805-file repo the Phase 2 pool dominates pull latency, so the speedup is meaningful even at localhost.
- The toggle is LUNAR_TRANSFER_CONCURRENCY: any value > 0 sets the buffer_unordered concurrency cap; 0 or unparseable falls back to 24.
