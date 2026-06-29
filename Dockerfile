# LunarFS self-host server -- multi-stage build
# Stage 1 (builder): compiles the release binary.
# Stage 2 (runtime): copies only the binary; runs as a non-root user.

# ----- builder ----------------------------------------------------------------

FROM rust:1-slim-bookworm AS builder

# Install C toolchain, pkg-config, and OpenSSL headers.
# rusqlite (bundled) compiles SQLite from source; reqwest uses native-tls.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       build-essential \
       pkg-config \
       libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy dependency manifests first so Cargo's registry download is cached
# independently of source changes.
COPY Cargo.toml Cargo.lock build.rs ./
RUN cargo fetch --locked

# Copy source and build the release binary.
COPY src/ src/
COPY tests/ tests/
RUN cargo build --release --locked --bin lunar

# ----- runtime ----------------------------------------------------------------

FROM debian:bookworm-slim AS runtime

# OpenSSL runtime library and CA bundle for TLS; sqlite3 for operator token management.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       ca-certificates \
       libssl3 \
       sqlite3 \
    && rm -rf /var/lib/apt/lists/*

# Create a non-root system user and group for the server process.
RUN groupadd -r -g 1001 lunar \
    && useradd -r -u 1001 -g lunar -s /sbin/nologin -M lunar

COPY --from=builder /build/target/release/lunar /usr/local/bin/lunar
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

# /data holds the SQLite identity database, the local blob store, and overlays.
# The volume is declared here; docker-compose mounts a named volume at this path.
RUN mkdir -p /data && chown lunar:lunar /data
VOLUME ["/data"]

USER lunar

# --------------------------------------------------------------------------
# LUNAR_* defaults. All are overridable via docker-compose environment.
#
# LUNAR_STORAGE_BACKEND  local|s3       (local = filesystem under LUNAR_STORAGE_PATH)
# LUNAR_STORAGE_PATH     host path      (used when backend=local)
# LUNAR_DB_PATH          SQLite file    (identity + ACL + workspace state)
# LUNAR_HOST             bind address   (0.0.0.0 in Docker; 127.0.0.1 outside)
# LUNAR_PORT             TCP port
#
# Clerk JWT auth is DISABLED by default: the server enters self-host (token-only)
# mode whenever CLERK_ISSUER / CLERK_AUDIENCE / CLERK_JWKS_URL are absent.
# Do NOT set those three variables in a self-host deployment.
# --------------------------------------------------------------------------
ENV LUNAR_STORAGE_BACKEND=local \
    LUNAR_STORAGE_PATH=/data/store \
    LUNAR_DB_PATH=/data/lunar.db \
    LUNAR_HOST=0.0.0.0 \
    LUNAR_PORT=8787

EXPOSE 8787

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
