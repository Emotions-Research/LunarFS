# Device image for the LunarFS multi-device integration test.
# Builds the same lunar binary as the server image but also installs
# git, rsync, curl for benchmark comparisons.
# Build context: repo root (so Cargo.toml / src/ are accessible).

FROM rust:1-slim-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
RUN cargo fetch --locked
COPY src/ src/
COPY tests/ tests/
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       build-essential \
       pkg-config \
       libssl-dev \
    && rm -rf /var/lib/apt/lists/* \
    && cargo build --release --locked --bin lunar

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       ca-certificates \
       libssl3 \
       sqlite3 \
       git \
       rsync \
       curl \
       time \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/lunar /usr/local/bin/lunar
