#!/bin/sh
# Seed the server DB with test fixtures.
# Called by run.sh with LUNAR_TOKEN and TOKEN_HASH_HEX set in env,
# or can be run standalone: TOKEN_HASH_HEX=<64-hex> TS=<unix> ./seed.sh
# The seeded entities:
#   users:         id=1 (auto), no external clerk id
#   organizations: id=1 (auto), slug='team', plan='free'
#   memberships:   user 1 is 'owner' of org 1
#   workspaces:    id=1 (auto), name='demo', owner_kind='org', owner_id=1
#   api_tokens:    principal user/1, token_hash=SHA-256(token plaintext) as raw BLOB
#   acl_grants:    org/1 write access to workspace 1 at path /
#                  (org grant covers all members via memberships table)
set -eu

TOKEN_HASH_HEX="${TOKEN_HASH_HEX:?TOKEN_HASH_HEX not set}"
TS="${TS:-$(date +%s)}"
DB="${LUNAR_DB_PATH:-/data/lunar.db}"

printf "INSERT INTO users(external_clerk_id, created_at) VALUES(NULL, %s);\n" "$TS"
printf "INSERT INTO organizations(slug, created_at) VALUES('team', %s);\n" "$TS"
printf "INSERT INTO memberships(user_id, org_id, role) VALUES(1, 1, 'owner');\n"
printf "INSERT INTO workspaces(name, owner_kind, owner_id, created_at) VALUES('demo', 'org', 1, %s);\n" "$TS"
printf "INSERT INTO api_tokens(principal_kind, principal_id, token_hash, scope, created_at, expires_at, revoked_at) VALUES('user', '1', X'%s', NULL, %s, NULL, NULL);\n" \
    "$TOKEN_HASH_HEX" "$TS"
printf "INSERT INTO acl_grants(principal_kind, principal_id, workspace_id, path_prefix, permission, created_at) VALUES('org', '1', 1, '/', 'write', %s);\n" "$TS"
