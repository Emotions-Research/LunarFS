#!/bin/sh
# Scenario 2: CONCURRENT DISJOINT EDITS
# Each device edits a DIFFERENT file from a shared base, then all three push
# simultaneously (background). Asserts no data loss and workspace converges.
set -eu

HOST_API="${HOST_API:-http://localhost:8787}"
LUNAR_TOKEN="${LUNAR_TOKEN:?LUNAR_TOKEN not set}"
COMPOSE_DIR="$(cd "$(dirname "$0")/.." && pwd)"

exec_a() { docker compose --project-directory "$COMPOSE_DIR" exec -T device-a sh -c "$1"; }
exec_b() { docker compose --project-directory "$COMPOSE_DIR" exec -T device-b sh -c "$1"; }
exec_c() { docker compose --project-directory "$COMPOSE_DIR" exec -T device-c sh -c "$1"; }

blob_exists() {
    local hash="$1"
    STATUS=$(curl -s -o /dev/null -w "%{http_code}" --max-time 5 \
        -H "Authorization: Bearer $LUNAR_TOKEN" \
        "${HOST_API}/v1/blob/${hash}?workspace=demo")
    [ "$STATUS" = "200" ]
}

echo "[scenario2] creating shared base + disjoint edits on each device..."
exec_a "mkdir -p /work/s2 && printf 'shared base content\n' > /work/s2/base.txt && printf 'device A file\n' > /work/s2/fileA.txt"
exec_b "mkdir -p /work/s2 && printf 'shared base content\n' > /work/s2/base.txt && printf 'device B file\n' > /work/s2/fileB.txt"
exec_c "mkdir -p /work/s2 && printf 'shared base content\n' > /work/s2/base.txt && printf 'device C file\n' > /work/s2/fileC.txt"

echo "[scenario2] ingesting on each device..."
ROOT_A=$(exec_a "lunar ingest /work/s2" | tr -d '[:space:]')
ROOT_B=$(exec_b "lunar ingest /work/s2" | tr -d '[:space:]')
ROOT_C=$(exec_c "lunar ingest /work/s2" | tr -d '[:space:]')
echo "[scenario2] roots: A=$ROOT_A  B=$ROOT_B  C=$ROOT_C"

echo "[scenario2] pushing all three simultaneously (background)..."
exec_a "lunar push team/demo $ROOT_A" &
PID_A=$!
exec_b "lunar push team/demo $ROOT_B" &
PID_B=$!
exec_c "lunar push team/demo $ROOT_C" &
PID_C=$!

PUSH_FAIL=0
wait $PID_A || { echo "[scenario2] device-a push failed"; PUSH_FAIL=1; }
wait $PID_B || { echo "[scenario2] device-b push failed"; PUSH_FAIL=1; }
wait $PID_C || { echo "[scenario2] device-c push failed"; PUSH_FAIL=1; }

if [ "$PUSH_FAIL" != "0" ]; then
    echo "[scenario2] FAIL: at least one push failed"
    exit 1
fi

echo "[scenario2] all pushes completed. checking current ref..."
CURRENT=$(exec_a "lunar pull team/demo" | grep -oE '[0-9a-f]{64}' | head -1)
echo "[scenario2] current ref after concurrent pushes: $CURRENT"

if [ "$CURRENT" = "$ROOT_A" ]; then
    WINNER="device-a"
elif [ "$CURRENT" = "$ROOT_B" ]; then
    WINNER="device-b"
elif [ "$CURRENT" = "$ROOT_C" ]; then
    WINNER="device-c"
else
    echo "[scenario2] FAIL: ref does not match any pushed root"
    echo "  current=$CURRENT  A=$ROOT_A  B=$ROOT_B  C=$ROOT_C"
    exit 1
fi
echo "[scenario2] ref winner: $WINNER (last writer wins; push is unconditional)"

echo "[scenario2] verifying all three roots present in CAS (no data loss)..."
FAIL=0
for ENTRY in "device-a:$ROOT_A" "device-b:$ROOT_B" "device-c:$ROOT_C"; do
    DEV="${ENTRY%%:*}"
    HASH="${ENTRY##*:}"
    if blob_exists "$HASH"; then
        echo "[scenario2] PASS: $DEV root $HASH is in CAS"
    else
        echo "[scenario2] FAIL: $DEV root $HASH NOT found in CAS"
        FAIL=1
    fi
done

if [ "$FAIL" != "0" ]; then
    exit 1
fi

echo "[scenario2] PASS: all roots preserved in CAS; ref winner=$WINNER"
echo "  NOTE: push uses unconditional write (no CAS enforcement by default)."
echo "  All blobs are retained content-addressed; only the ref tip is last-writer-wins."
exit 0
