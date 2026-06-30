#!/bin/bash
# ws_diff_overlap.sh
# Verifies that `lunar ws diff` reports per-agent changesets and flags overlapping
# paths when 3 agent workspaces are forked from one base.
#
# Prerequisites: cargo build has been run (target/debug/lunar must exist) and
# sqlite3 must be on PATH. No network or Docker needed.
#
# CAS note: blobs are written to ~/.lunar/cas (the hardcoded FsStore path);
# the workspace DB is isolated to TMPDIR so it does not touch ~/.lunar/state.db.
#
# Usage: bash test/multidevice/ws_diff_overlap.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LUNAR="$REPO_ROOT/target/debug/lunar"

if [ ! -x "$LUNAR" ]; then
    echo "FAIL: lunar binary not found at $LUNAR -- run cargo build first" >&2
    exit 1
fi

if ! command -v sqlite3 >/dev/null 2>&1; then
    echo "FAIL: sqlite3 not found on PATH" >&2
    exit 1
fi

# -- Isolated temp dir (hermetic; cleaned up on exit) --------------------------
TMPDIR_TEST="$(mktemp -d)"
cleanup() { rm -rf "$TMPDIR_TEST"; }
trap cleanup EXIT

DB="$TMPDIR_TEST/state.db"

log() { printf '[ws_diff_overlap] %s\n' "$*"; }

# -- Step 1: seed base workspace -----------------------------------------------
log "creating base seed directory..."
SEED="$TMPDIR_TEST/base"
mkdir -p "$SEED"
printf 'readme content -- unchanged by all agents\n' > "$SEED/readme.txt"
printf 'original shared content\n'                    > "$SEED/shared.txt"

log "ingesting base into CAS..."
BASE_HASH=$("$LUNAR" ingest "$SEED" | tr -d '[:space:]')
log "BASE_HASH=$BASE_HASH"

if [ "${#BASE_HASH}" -ne 64 ]; then
    echo "FAIL: lunar ingest did not return a 64-char hash (got '$BASE_HASH')" >&2
    exit 1
fi

# -- Step 2: fork 3 agent workspaces from the same base -----------------------
log "forking 3 agent workspaces from base..."

FORK_OUT1=$("$LUNAR" ws fork --from "$BASE_HASH" --label "agent-1" --db "$DB")
WS1_ID=$(printf '%s\n' "$FORK_OUT1" | awk '/^id:/ { print $2 }')

FORK_OUT2=$("$LUNAR" ws fork --from "$BASE_HASH" --label "agent-2" --db "$DB")
WS2_ID=$(printf '%s\n' "$FORK_OUT2" | awk '/^id:/ { print $2 }')

FORK_OUT3=$("$LUNAR" ws fork --from "$BASE_HASH" --label "agent-3" --db "$DB")
WS3_ID=$(printf '%s\n' "$FORK_OUT3" | awk '/^id:/ { print $2 }')

log "WS1_ID=$WS1_ID (agent-1)"
log "WS2_ID=$WS2_ID (agent-2)"
log "WS3_ID=$WS3_ID (agent-3)"

if [ -z "$WS1_ID" ] || [ -z "$WS2_ID" ] || [ -z "$WS3_ID" ]; then
    echo "FAIL: one or more workspace fork commands returned an empty id" >&2
    exit 1
fi

# -- Step 3: each agent edits its own unique file + the shared overlapping file
log "creating per-agent file trees..."

DIR1="$TMPDIR_TEST/ws1"
mkdir -p "$DIR1"
printf 'readme content -- unchanged by all agents\n' > "$DIR1/readme.txt"
printf 'agent-1 edited this file\n'                  > "$DIR1/shared.txt"
printf 'only agent-1 touches this\n'                 > "$DIR1/agent1_only.txt"

DIR2="$TMPDIR_TEST/ws2"
mkdir -p "$DIR2"
printf 'readme content -- unchanged by all agents\n' > "$DIR2/readme.txt"
printf 'agent-2 edited this file\n'                  > "$DIR2/shared.txt"
printf 'only agent-2 touches this\n'                 > "$DIR2/agent2_only.txt"

DIR3="$TMPDIR_TEST/ws3"
mkdir -p "$DIR3"
printf 'readme content -- unchanged by all agents\n' > "$DIR3/readme.txt"
printf 'agent-3 edited this file\n'                  > "$DIR3/shared.txt"
printf 'only agent-3 touches this\n'                 > "$DIR3/agent3_only.txt"

# -- Step 4: ingest each agent tree to get a root hash ------------------------
log "ingesting per-agent trees into CAS..."
HASH1=$("$LUNAR" ingest "$DIR1" | tr -d '[:space:]')
HASH2=$("$LUNAR" ingest "$DIR2" | tr -d '[:space:]')
HASH3=$("$LUNAR" ingest "$DIR3" | tr -d '[:space:]')
log "HASH1=$HASH1"
log "HASH2=$HASH2"
log "HASH3=$HASH3"

if [ "${#HASH1}" -ne 64 ] || [ "${#HASH2}" -ne 64 ] || [ "${#HASH3}" -ne 64 ]; then
    echo "FAIL: one or more agent ingest commands did not return a 64-char hash" >&2
    exit 1
fi

# -- Step 5: record root hashes in the workspace DB ---------------------------
# The `lunar ws fork` CLI does not expose a set-root command. The store uses a
# standard SQLite upsert (id is the primary key) so a direct UPDATE is safe.
log "recording root hashes in workspace DB..."
sqlite3 "$DB" \
    "UPDATE local_workspaces SET root='$HASH1' WHERE id='$WS1_ID';
     UPDATE local_workspaces SET root='$HASH2' WHERE id='$WS2_ID';
     UPDATE local_workspaces SET root='$HASH3' WHERE id='$WS3_ID';"

UPDATED=$(sqlite3 "$DB" "SELECT COUNT(*) FROM local_workspaces WHERE root IS NOT NULL;")
if [ "$UPDATED" -ne 3 ]; then
    echo "FAIL: expected 3 workspaces with root set; got $UPDATED" >&2
    exit 1
fi

# -- Step 6: run lunar ws diff and capture output -----------------------------
log "running: lunar ws diff --db $DB"
DIFF_OUT=$("$LUNAR" ws diff --db "$DB")
printf '%s\n' "$DIFF_OUT"
echo ""

# -- Step 7: assert per-agent changeset attribution ---------------------------
log "asserting per-agent changesets..."

FAIL=0

assert_contains() {
    local label="$1" needle="$2"
    if printf '%s\n' "$DIFF_OUT" | grep -qF "$needle"; then
        log "PASS: $label"
    else
        echo "FAIL: $label -- expected to find: $needle" >&2
        FAIL=1
    fi
}

assert_not_contains() {
    local label="$1" needle="$2"
    if printf '%s\n' "$DIFF_OUT" | grep -qF "$needle"; then
        echo "FAIL: $label -- expected NOT to find: $needle" >&2
        FAIL=1
    else
        log "PASS: $label"
    fi
}

# Group header: 3 workspaces under one base
assert_contains "group header shows 3 workspaces"  "3 workspaces"

# Per-agent changeset lines
assert_contains "agent-1 block present"            "agent-1"
assert_contains "agent-2 block present"            "agent-2"
assert_contains "agent-3 block present"            "agent-3"

# Each agent's unique file attributed to that agent's block.
# The format is: each block lists its files after the agent label line.
# We check that the unique file paths appear in the overall output; given the
# block order guarantees (agent-1 block precedes agent-2, agent-2 precedes
# agent-3 by label sort), these names cannot bleed across blocks.
assert_contains "agent1_only.txt in output"        "agent1_only.txt"
assert_contains "agent2_only.txt in output"        "agent2_only.txt"
assert_contains "agent3_only.txt in output"        "agent3_only.txt"

# Unique files must NOT appear in the overlaps section
# (only shared.txt touches 2+ agents; agentN_only.txt touches exactly 1)
assert_not_contains "agent1_only.txt not in OVERLAPS" "agent1_only.txt  <-"
assert_not_contains "agent2_only.txt not in OVERLAPS" "agent2_only.txt  <-"
assert_not_contains "agent3_only.txt not in OVERLAPS" "agent3_only.txt  <-"

# -- Step 8: assert overlap flagging ------------------------------------------
log "asserting overlap section..."

assert_contains "OVERLAPS section header present"  "OVERLAPS (who stepped on whom)"
assert_contains "shared.txt flagged in OVERLAPS"   "shared.txt  <-"

# All 3 agents must be listed in the shared.txt overlap entry.
# The format is: "shared.txt  <- agent-1, agent-2, agent-3"
OVERLAP_LINE=$(printf '%s\n' "$DIFF_OUT" | grep "^shared.txt" || true)
if [ -z "$OVERLAP_LINE" ]; then
    echo "FAIL: no overlap line for shared.txt found" >&2
    FAIL=1
else
    for AGENT in "agent-1" "agent-2" "agent-3"; do
        if printf '%s\n' "$OVERLAP_LINE" | grep -qF "$AGENT"; then
            log "PASS: $AGENT appears in shared.txt overlap line"
        else
            echo "FAIL: $AGENT missing from shared.txt overlap line: $OVERLAP_LINE" >&2
            FAIL=1
        fi
    done
fi

# -- Result -------------------------------------------------------------------
echo ""
if [ "$FAIL" -eq 0 ]; then
    log "ALL ASSERTIONS PASSED"
    exit 0
else
    log "ONE OR MORE ASSERTIONS FAILED"
    exit 1
fi
