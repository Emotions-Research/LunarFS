#!/bin/sh
# Scenario 4: CONVERGENCE
# After concurrent scenarios, all three devices pull and reach a consistent view.
# ASSERTION: all three devices resolve the same root hash.
set -eu

COMPOSE_DIR="$(cd "$(dirname "$0")/.." && pwd)"

exec_a() { docker compose --project-directory "$COMPOSE_DIR" exec -T device-a sh -c "$1"; }
exec_b() { docker compose --project-directory "$COMPOSE_DIR" exec -T device-b sh -c "$1"; }
exec_c() { docker compose --project-directory "$COMPOSE_DIR" exec -T device-c sh -c "$1"; }

echo "[scenario4] all three devices pulling team/demo..."
ROOT_A=$(exec_a "lunar pull team/demo" | grep -oE '[0-9a-f]{64}' | head -1)
ROOT_B=$(exec_b "lunar pull team/demo" | grep -oE '[0-9a-f]{64}' | head -1)
ROOT_C=$(exec_c "lunar pull team/demo" | grep -oE '[0-9a-f]{64}' | head -1)

echo "[scenario4] roots after pull:"
echo "  device-a: $ROOT_A"
echo "  device-b: $ROOT_B"
echo "  device-c: $ROOT_C"

if [ "$ROOT_A" = "$ROOT_B" ] && [ "$ROOT_B" = "$ROOT_C" ]; then
    echo "[scenario4] PASS: all three devices converged to root $ROOT_A"
    exit 0
else
    echo "[scenario4] FAIL: devices did not converge"
    exit 1
fi
