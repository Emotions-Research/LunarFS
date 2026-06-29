#!/bin/sh
# Scenario 1: FAN-OUT SYNC
# device-a ingests and pushes a sample repo. device-b and device-c pull.
# ASSERTION: all three resolve the identical root hash.
set -eu

HOST_API="${HOST_API:-http://localhost:8787}"
LUNAR_TOKEN="${LUNAR_TOKEN:?LUNAR_TOKEN not set}"
COMPOSE_DIR="$(cd "$(dirname "$0")/.." && pwd)"

exec_a() { docker compose --project-directory "$COMPOSE_DIR" exec -T device-a sh -c "$1"; }
exec_b() { docker compose --project-directory "$COMPOSE_DIR" exec -T device-b sh -c "$1"; }
exec_c() { docker compose --project-directory "$COMPOSE_DIR" exec -T device-c sh -c "$1"; }

echo "[scenario1] creating sample repo on device-a..."
exec_a "mkdir -p /work/s1 && printf 'Hello LunarFS\n' > /work/s1/readme.txt && printf 'data: 42\n' > /work/s1/data.txt"

echo "[scenario1] ingesting on device-a..."
ROOT_A=$(exec_a "lunar ingest /work/s1" | tr -d '[:space:]')
echo "[scenario1] root from device-a ingest: $ROOT_A"

echo "[scenario1] pushing from device-a..."
exec_a "lunar push team/demo $ROOT_A"

echo "[scenario1] pulling on device-b..."
ROOT_B=$(exec_b "lunar pull team/demo" | grep -oE '[0-9a-f]{64}' | head -1)
echo "[scenario1] root pulled by device-b: $ROOT_B"

echo "[scenario1] pulling on device-c..."
ROOT_C=$(exec_c "lunar pull team/demo" | grep -oE '[0-9a-f]{64}' | head -1)
echo "[scenario1] root pulled by device-c: $ROOT_C"

if [ "$ROOT_A" = "$ROOT_B" ] && [ "$ROOT_B" = "$ROOT_C" ]; then
    echo "[scenario1] PASS: all three devices resolved identical root $ROOT_A"
    exit 0
else
    echo "[scenario1] FAIL: roots diverged"
    echo "  device-a: $ROOT_A"
    echo "  device-b: $ROOT_B"
    echo "  device-c: $ROOT_C"
    exit 1
fi
