#!/bin/sh
# LunarFS installer — https://lunarfs.com/install.sh
# Usage: curl --proto '=https' --tlsv1.2 -LsSf https://lunarfs.com/install.sh | sh
set -eu

REPO="Emotions-Research/LunarFS"
BINDIR="${LUNAR_BIN_DIR:-$HOME/.local/bin}"
OS="$(uname -s)"
ARCH="$(uname -m)"

say()  { printf '%s\n' "$*"; }
err()  { printf 'error: %s\n' "$*" >&2; exit 1; }

command -v curl >/dev/null 2>&1 || err "curl is required but was not found"

# Asset names produced by cargo-dist (devdropbox-<target>.tar.xz, binary inside = lunar).
case "${OS}-${ARCH}" in
  Darwin-arm64)                  ASSET="devdropbox-aarch64-apple-darwin.tar.xz" ;;
  Darwin-x86_64)                 ASSET="devdropbox-x86_64-apple-darwin.tar.xz" ;;
  Linux-x86_64)                  ASSET="devdropbox-x86_64-unknown-linux-gnu.tar.xz" ;;
  Linux-aarch64|Linux-arm64)     ASSET="devdropbox-aarch64-unknown-linux-gnu.tar.xz" ;;
  *)
    say "No prebuilt binary for ${OS}-${ARCH} yet — building from source."
    if command -v cargo >/dev/null 2>&1; then
      exec cargo install --git "https://github.com/${REPO}" --features mount-nfs --bin lunar
    fi
    err "Install Rust (https://rustup.rs), then re-run this script."
    ;;
esac

URL="https://github.com/${REPO}/releases/latest/download/${ASSET}"
say "Downloading lunar (${ASSET})..."
mkdir -p "$BINDIR"
LUNAR_TMP="$(mktemp -d)"
trap 'rm -rf "$LUNAR_TMP"' EXIT
curl --proto '=https' --tlsv1.2 -fLsS "$URL" -o "${LUNAR_TMP}/${ASSET}" \
  || err "download failed from ${URL}"
tar xf "${LUNAR_TMP}/${ASSET}" -C "$LUNAR_TMP" \
  || err "failed to extract ${ASSET}"
cp "${LUNAR_TMP}/lunar" "${BINDIR}/lunar" \
  || err "failed to install lunar to ${BINDIR}"
chmod +x "${BINDIR}/lunar"

say "Installed lunar -> ${BINDIR}/lunar"
case ":${PATH}:" in
  *":${BINDIR}:"*) say "Run: lunar --help" ;;
  *) say "Add ${BINDIR} to your PATH, then run: lunar --help" ;;
esac
