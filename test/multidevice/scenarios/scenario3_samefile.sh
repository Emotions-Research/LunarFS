#!/bin/sh
# Scenario 3: SAME-FILE CONFLICT
# device-a and device-b both edit the SAME file differently and push simultaneously.
# ASSERTION: both roots/blobs are retrievable from the CAS (no data loss).
# The ref points to the last writer; both versions exist in the blob store.
set -eu

HOST_API="${HOST_API:-http://localhost:8787}"
LUNAR_TOKEN="${LUNAR_TOKEN:?LUNAR_TOKEN not set}"
COMPOSE_DIR="$(cd "$(dirname "$0")/.." && pwd)"

exec_a() { docker compose --project-directory "$COMPOSE_DIR" exec -T device-a sh -c "$1"; }
exec_b() { docker compose --project-directory "$COMPOSE_DIR" exec -T device-b sh -c "$1"; }

blob_exists() {
    local hash="$1"
    STATUS=$(curl -s -o /dev/null -w "%{http_code}" --max-time 5 \
        -H "Authorization: Bearer $LUNAR_TOKEN" \
        "${HOST_API}/v1/blob/${hash}?workspace=demo")
    [ "$STATUS" = "200" ]
}

echo "[scenario3] creating divergent edits of the same file on device-a and device-b..."
exec_a "mkdir -p /work/s3 && printf 'version A: device A content\n' > /work/s3/conflict.txt"
exec_b "mkdir -p /work/s3 && printf 'version B: device B content\n' > /work/s3/conflict.txt"

echo "[scenario3] ingesting on both devices..."
ROOT_A=$(exec_a "lunar ingest /work/s3" | tr -d '[:space:]')
ROOT_B=$(exec_b "lunar ingest /work/s3" | tr -d '[:space:]')
echo "[scenario3] roots: A=$ROOT_A  B=$ROOT_B"

echo "[scenario3] pushing both simultaneously (background)..."
exec_a "lunar push team/demo $ROOT_A" &
PID_A=$!
exec_b "lunar push team/demo $ROOT_B" &
PID_B=$!

FAIL=0
wait $PID_A || { echo "[scenario3] device-a push failed"; FAIL=1; }
wait $PID_B || { echo "[scenario3] device-b push failed"; FAIL=1; }

if [ "$FAIL" != "0" ]; then
    echo "[scenario3] FAIL: a push failed"
    exit 1
fi

echo "[scenario3] both pushes completed. checking current ref..."
CURRENT=$(exec_a "lunar pull team/demo" | grep -oE '[0-9a-f]{64}' | head -1)

if [ "$CURRENT" = "$ROOT_A" ]; then
    WINNER="device-a (version A)"
elif [ "$CURRENT" = "$ROOT_B" ]; then
    WINNER="device-b (version B)"
else
    echo "[scenario3] FAIL: ref does not match either pushed root"
    echo "  current=$CURRENT  A=$ROOT_A  B=$ROOT_B"
    exit 1
fi
echo "[scenario3] conflict resolution: ref points to $WINNER (last-writer-wins)"

echo "[scenario3] verifying both roots exist in CAS (both versions preserved)..."
ROOT_A_OK=0
ROOT_B_OK=0
blob_exists "$ROOT_A" && ROOT_A_OK=1
blob_exists "$ROOT_B" && ROOT_B_OK=1

if [ "$ROOT_A_OK" = "1" ] && [ "$ROOT_B_OK" = "1" ]; then
    echo "[scenario3] PASS: both version A ($ROOT_A) and version B ($ROOT_B) in CAS"
    echo "  The losing version is NOT surfaced as a 'conflict copy' in the workspace ref;"
    echo "  it is retained as an unreferenced blob in the content-addressed store."
    exit 0
else
    [ "$ROOT_A_OK" = "0" ] && echo "[scenario3] FAIL: version A root NOT in CAS"
    [ "$ROOT_B_OK" = "0" ] && echo "[scenario3] FAIL: version B root NOT in CAS"
    exit 1
fi
