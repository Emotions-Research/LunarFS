# Diffing workspaces: `lunar diff` and `lunar ws diff`

This document covers the two commands for inspecting what changed across workspaces.
All examples assume `target/debug/lunar` is on your `PATH` (or substitute the full path).

---

## Overview

`lunar diff` computes a changeset between two tree roots. Arguments can be literal
64-char BLAKE3 root hashes, workspace labels, or workspace ids. With a single
workspace argument it auto-diffs against that workspace's recorded `base_ref`,
showing exactly what changed since the workspace was forked.

`lunar ws diff` (and `lunar diff --all`) is the workspace-fleet variant. It
enumerates every local workspace in `~/.lunar/state.db`, groups them by their shared
`base_ref`, prints each workspace's changeset relative to the group's shared base,
and prominently flags any path touched by more than one workspace in the same group.
Use `lunar ws diff` when you want an at-a-glance picture of where multiple agents or
branches have diverged from a common fork point, and whether any of them are
stepping on the same files.

---

## `lunar diff`

```
Usage: lunar diff [OPTIONS] [A] [B]

Arguments:
  [A]  First ref: root hash, workspace label, or workspace id. Omit when using --all
  [B]  Second ref. If omitted, <A> must be a workspace (diffs base_ref vs root)
```

### Flags

| Flag | Meaning |
|---|---|
| `--all` | Diff all local workspaces grouped by shared base ref. Equivalent to `lunar ws diff`. |
| `--name-only` | Print only changed paths, one per line. No status markers, no summary. |
| `--stat` | Print per-file byte deltas (`path \| +<added> -<removed>`) plus a totals line. |
| `--json` | Print a JSON array, one object per changed path, with fields `path`, `status` (`"A"`, `"M"`, or `"D"`), `old_size`, and `new_size`. |
| `--patch` | Emit a unified git-style line diff for each changed text blob. |
| `--db <DB>` | Path to the workspace state database. Default: `~/.lunar/state.db`. |

When multiple output flags are present, the precedence is:
`--patch` beats `--json` beats `--stat` beats `--name-only`.

### Argument resolution order

Each argument (`A`, `B`) is resolved using this order (first match wins):

1. Literal 64-char hex root hash.
2. Workspace label (from `~/.lunar/state.db`).
3. Workspace id (from `~/.lunar/state.db`).

An argument that is not a valid 64-char hex and matches no label or id is an error.

### base_ref auto-diff behavior

When only `A` is given and it resolves to a workspace record, `lunar diff` reads
two fields from that record:

- `base_ref`: the ref that was current when the workspace was forked. This becomes
  the **old** side of the diff. It is resolved through the same three-step order
  above (so `base_ref` can itself be a hash, a label, or another workspace id).
- `root`: the most recently recorded root hash for this workspace. This becomes the
  **new** side of the diff.

In short: `lunar diff <workspace>` answers "what has changed in this workspace since
it was forked?" without needing to remember or type either hash.

Passing an explicit `B` bypasses this rule entirely: both `A` and `B` are resolved
independently and the diff is between those two trees directly.

Passing a bare root hash as the only argument (not a workspace) is an error;
without a workspace record there is no `base_ref` to diff against.

### Examples

**Single-workspace auto-diff** (what changed since forking, default output):

```
$ lunar diff agent-run-1
A src/new_feature.rs
M Cargo.toml
3 files changed
```

**Explicit two-ref diff** (hash vs hash):

```
$ lunar diff \
    aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
    bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
M src/main.rs
1 files changed
```

**Machine-readable JSON** (useful for scripted pipelines):

```
$ lunar diff agent-run-1 --json
[
  {
    "path": "Cargo.toml",
    "status": "M",
    "old_size": 1240,
    "new_size": 1290
  },
  {
    "path": "src/new_feature.rs",
    "status": "A",
    "old_size": null,
    "new_size": 842
  }
]
```

**Byte-delta summary** (`--stat`):

```
$ lunar diff agent-run-1 --stat
Cargo.toml | +50 -0
src/new_feature.rs | +842 -0
2 files changed (+892 -0 bytes)
```

---

## `lunar ws diff`

```
Usage: lunar ws diff [OPTIONS]

Options:
  --db <DB>  Path to the workspace state database (default: ~/.lunar/state.db)
  -h, --help Print help
```

`lunar ws diff` reads every workspace from the state database, groups them by their
`base_ref`, and for each group:

1. Resolves the shared `base_ref` to a root hash.
2. Resolves each member workspace's recorded `root` to a root hash.
3. Diffs the base against each member.
4. Detects any path touched by two or more workspaces in the same group and reports
   those paths as overlaps.

A workspace with no recorded `root` is skipped with a warning. A group whose
`base_ref` cannot be resolved is skipped with a warning.

### Output format

```
=== base: <base_ref (first 16 chars)> (<N> workspaces)

<workspace-name> (id=<id>, <N> changed):
A path/to/added.rs
M path/to/modified.rs
D path/to/deleted.rs

OVERLAPS (who stepped on whom)
------------------------------
path/to/shared.rs  <- agent-1, agent-2
```

If no paths overlap in a group, the section reads:

```
no overlapping paths in this group
```

Status markers follow git convention: `A` = added, `M` = modified, `D` = deleted.

The workspace name shown is the label when one is set, otherwise the workspace id.

### Example

Three agent workspaces forked from the same base, two of which both edited
`shared/config.toml`:

```
$ lunar ws diff

=== base: abababababababab (3 workspaces)

agent-alpha (id=ws-001, 2 changed):
A src/alpha.rs
M shared/config.toml

agent-beta (id=ws-002, 2 changed):
M shared/config.toml
A src/beta.rs

agent-gamma (id=ws-003, 1 changed):
A src/gamma.rs

OVERLAPS (who stepped on whom)
------------------------------
shared/config.toml  <- agent-alpha, agent-beta
```

This tells you: `agent-alpha` and `agent-beta` both modified `shared/config.toml`.
Their changes are independent and one will overwrite the other if merged
naively. `agent-gamma` is clean.

### Relation to `lunar diff --all`

`lunar diff --all` is an alias for `lunar ws diff`. Both run the same code path and
produce identical output. The `--db` flag works on both.

---

## Database location

All diff commands read the local workspace state database. Default path:
`~/.lunar/state.db`. Override with `--db <path>` on any of the commands above.

Workspace records are written by `lunar ws fork`. Workspaces that were never forked
locally (created by `lunar fork` against the remote server, for example) do not
appear in this database.

---

## See also

- [Cross-machine sync guide](sync.md): how to push, pull, fork, and autosync workspaces.
- [Agents and the `run_in_workspace` API](agents.md): programmatic workspace lifecycle.
- `lunar ws --help`: full list of local workspace subcommands (`fork`, `ls`, `destroy`, `diff`).
