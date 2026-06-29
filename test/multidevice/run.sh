#!/bin/bash
# One-command multi-device sync test for LunarFS.
# Usage: bash test/multidevice/run.sh
# Builds images, starts server + 3 devices, runs 4 scenarios + benchmark,
# writes RESULTS.md, tears down. Exits non-zero if any assertion fails.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

# -- Token generation ----------------------------------------------------------
# Throwaway token. Never hard-code or reuse a production token here.
TOKEN="ddb_$(openssl rand -hex 32)"
TOKEN_HASH_HEX="$(printf '%s' "$TOKEN" | openssl dgst -sha256 | awk '{print $NF}')"
printf 'LUNAR_TOKEN=%s\n' "$TOKEN" > .env
export LUNAR_TOKEN="$TOKEN"
export HOST_API="http://localhost:8787"

echo "=== LunarFS multi-device test ==="
echo "token SHA: ${TOKEN_HASH_HEX:0:16}..."

# -- Cleanup trap --------------------------------------------------------------
FINAL_STATUS=0

cleanup() {
    local rc=$?
    echo ""
    echo "tearing down containers and volumes..."
    docker compose down -v --remove-orphans 2>/dev/null || true
    rm -f .env /tmp/lunar_s1.txt /tmp/lunar_s2.txt \
              /tmp/lunar_s3.txt /tmp/lunar_s4.txt /tmp/lunar_bench.txt
    if [ "$rc" -eq 0 ] && [ "${FINAL_STATUS}" -eq 0 ]; then
        echo "RESULT: PASS"
    else
        echo "RESULT: FAIL"
    fi
}
trap cleanup EXIT

# -- Build + start -------------------------------------------------------------
echo ""
echo "building images and starting containers..."
docker compose up -d --build

# -- Wait for server -----------------------------------------------------------
echo "waiting for server..."
HTTP_CODE="000"
ATTEMPTS=0
while [ $ATTEMPTS -lt 90 ]; do
    HTTP_CODE="$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 \
        "http://localhost:8787/v1/workspaces" 2>/dev/null || echo "000")"
    if [ "$HTTP_CODE" != "000" ]; then
        break
    fi
    ATTEMPTS=$((ATTEMPTS + 1))
    sleep 2
done
if [ "$HTTP_CODE" = "000" ]; then
    echo "FAIL: server did not respond after $((ATTEMPTS * 2))s"
    exit 1
fi
echo "server up (HTTP $HTTP_CODE)"

# -- Seed DB -------------------------------------------------------------------
echo "seeding DB (user, org, workspace, token, ACL grant)..."
TS="$(date +%s)"

# ACL note: the server uses explicit-grants-only authorization. Org membership
# alone does not grant access. The acl_grants row (principal_kind='org') covers
# all org members via the membership table join in acl::principal_matches.
{
    printf "INSERT INTO users(external_clerk_id, created_at) VALUES(NULL, %s);\n" "$TS"
    printf "INSERT INTO organizations(slug, created_at) VALUES('team', %s);\n" "$TS"
    printf "INSERT INTO memberships(user_id, org_id, role) VALUES(1, 1, 'owner');\n"
    printf "INSERT INTO workspaces(name, owner_kind, owner_id, created_at) VALUES('demo', 'org', 1, %s);\n" "$TS"
    printf "INSERT INTO api_tokens(principal_kind, principal_id, token_hash, scope, created_at, expires_at, revoked_at) VALUES('user', '1', X'%s', NULL, %s, NULL, NULL);\n" \
        "$TOKEN_HASH_HEX" "$TS"
    printf "INSERT INTO acl_grants(principal_kind, principal_id, workspace_id, path_prefix, permission, created_at) VALUES('org', '1', 1, '/', 'write', %s);\n" "$TS"
} | docker compose exec -T server sqlite3 /data/lunar.db

echo "DB seeded"

# -- Verify auth ---------------------------------------------------------------
# 404 means the workspace exists but has no ref yet; not 401/403.
echo "verifying token auth..."
AUTH_CHECK="$(curl -s -o /dev/null -w "%{http_code}" --max-time 5 \
    -H "Authorization: Bearer $TOKEN" \
    "http://localhost:8787/v1/ref/demo" 2>/dev/null || echo "000")"
if [ "$AUTH_CHECK" = "401" ] || [ "$AUTH_CHECK" = "403" ] || [ "$AUTH_CHECK" = "000" ]; then
    echo "FAIL: auth check returned $AUTH_CHECK (expected 200 or 404)"
    docker compose logs server --tail 20
    exit 1
fi
echo "auth OK (HTTP $AUTH_CHECK for empty workspace ref)"

# -- Helper: run a scenario script, tee output, track pass/fail ---------------
run_scenario() {
    local label="$1" script="$2" outfile="$3"
    echo ""
    echo "--- $label ---"
    local sc=0
    set +e
    bash "$script" 2>&1 | tee "$outfile"
    sc="${PIPESTATUS[0]}"
    set -e
    if [ "$sc" -ne 0 ]; then
        echo "[$label] FAIL"
        FINAL_STATUS=1
    else
        echo "[$label] PASS"
    fi
}

# -- Run scenarios -------------------------------------------------------------
run_scenario "scenario1-fanout"      scenarios/scenario1_fanout.sh  /tmp/lunar_s1.txt
run_scenario "scenario2-disjoint"    scenarios/scenario2_disjoint.sh /tmp/lunar_s2.txt
run_scenario "scenario3-samefile"    scenarios/scenario3_samefile.sh /tmp/lunar_s3.txt
run_scenario "scenario4-convergence" scenarios/scenario4_convergence.sh /tmp/lunar_s4.txt

# -- Benchmark -----------------------------------------------------------------
echo ""
echo "--- benchmark ---"
set +e
bash benchmark.sh 2>&1 | tee /tmp/lunar_bench.txt
set -e

# -- Write RESULTS.md ----------------------------------------------------------
echo ""
echo "writing RESULTS.md..."

{
    printf '# LunarFS Multi-Device Sync Test Results\n\n'
    printf 'Run date: %s\n\n' "$(date -u '+%Y-%m-%d %H:%M:%S UTC')"

    printf '## Scenario 1: Fan-Out Sync\n\n```\n'
    cat /tmp/lunar_s1.txt
    printf '```\n\n'

    printf '## Scenario 2: Concurrent Disjoint Edits\n\n```\n'
    cat /tmp/lunar_s2.txt
    printf '```\n\n'
    printf '### Concurrent edit semantics\n\n'
    printf 'LunarFS push is unconditional (last-writer-wins) by default because the\n'
    printf 'client sends only the new root without an expected_root CAS field.\n'
    printf 'All blob data is preserved content-addressed in the server store;\n'
    printf 'no data is deleted when a later push advances the ref.\n\n'

    printf '## Scenario 3: Same-File Conflict\n\n```\n'
    cat /tmp/lunar_s3.txt
    printf '```\n\n'
    printf '### Conflict semantics\n\n'
    printf 'When two devices push different versions of the same file simultaneously,\n'
    printf 'the last write wins the workspace ref. The losing version is NOT surfaced\n'
    printf 'as a named conflict copy in the ref; it lives as an unreferenced CAS blob.\n'
    printf 'Both versions are retrievable by their content hash for the lifetime of\n'
    printf 'the store. CAS-enforced conflict detection requires clients to supply\n'
    printf 'expected_root in PUT /v1/ref/:workspace; the default client path omits it.\n\n'

    printf '## Scenario 4: Convergence\n\n```\n'
    cat /tmp/lunar_s4.txt
    printf '```\n\n'

    cat /tmp/lunar_bench.txt
} > RESULTS.md

echo "RESULTS.md written"

# -- Print summary -------------------------------------------------------------
echo ""
echo "=== SUMMARY ==="
if [ "${FINAL_STATUS}" -eq 0 ]; then
    echo "All scenarios PASSED. Results in test/multidevice/RESULTS.md."
else
    echo "One or more scenarios FAILED. See test/multidevice/RESULTS.md."
fi

exit "${FINAL_STATUS}"
