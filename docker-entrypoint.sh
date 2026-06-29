#!/bin/sh
# Translates LUNAR_* env vars into lunar serve CLI arguments and execs the server.
# All LUNAR_* vars have defaults that produce a working local-storage self-host.
set -e

LUNAR_STORAGE_BACKEND="${LUNAR_STORAGE_BACKEND:-local}"
LUNAR_STORAGE_PATH="${LUNAR_STORAGE_PATH:-/data/store}"
LUNAR_DB_PATH="${LUNAR_DB_PATH:-/data/lunar.db}"
LUNAR_HOST="${LUNAR_HOST:-0.0.0.0}"
LUNAR_PORT="${LUNAR_PORT:-8787}"

case "$LUNAR_STORAGE_BACKEND" in
  local)
    STORE_SPEC="local:${LUNAR_STORAGE_PATH}"
    ;;
  s3)
    if [ -z "${LUNAR_S3_BUCKET:-}" ]; then
      echo "error: LUNAR_S3_BUCKET must be set when LUNAR_STORAGE_BACKEND=s3" >&2
      exit 1
    fi
    STORE_SPEC="s3://${LUNAR_S3_BUCKET}"
    ;;
  *)
    echo "error: unknown LUNAR_STORAGE_BACKEND value: ${LUNAR_STORAGE_BACKEND}" >&2
    echo "       valid values: local, s3" >&2
    exit 1
    ;;
esac

exec /usr/local/bin/lunar serve \
  --store "${STORE_SPEC}" \
  --addr "${LUNAR_HOST}:${LUNAR_PORT}" \
  --db "${LUNAR_DB_PATH}"
