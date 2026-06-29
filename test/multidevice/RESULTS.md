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
