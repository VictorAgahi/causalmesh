#!/usr/bin/env bash
# MeshMCP pilot installer (plan 4 step 4.8).
#
# Installs `mesh-mcp` and `meshd` into ~/.local/bin, then (after confirmation)
# runs `mesh-mcp init --auto --write-ide-config` in the pilot workspace, then
# checks the result with `mesh-mcp doctor --json`.
#
# Idempotent: running it twice leaves the same state (binaries copied only when
# they differ, IDE configs merged — never overwritten — by `init` since 6.0.1,
# an existing .agents/mesh-mcp.toml kept byte-for-byte).
#
# No network access: binaries come from --bin-dir, or from a `cargo build
# --release` of the checkout this script lives in. For a prebuilt release,
# use install.sh (it downloads from GitHub) and then point --bin-dir at
# ~/.local/bin.
#
# Usage: scripts/install_pilot.sh [--bin-dir DIR] [--workspace DIR]
#                                 [--prefix DIR] [--yes]
#
# bash 3.2 compatible (macOS /bin/bash). Every failure prints why and exits
# non-zero; nothing fails silently.

set -euo pipefail

PROG="install_pilot"
BIN_DIR=""
WORKSPACE="$PWD"
PREFIX="${HOME}/.local/bin"
ASSUME_YES=0

die() {
  echo "✖ ${PROG}: $*" >&2
  exit 1
}
info() { echo "${PROG}: $*" >&2; }

on_err() {
  echo "✖ ${PROG}: command failed (exit $1) at line $2: $3" >&2
}
trap 'on_err "$?" "$LINENO" "$BASH_COMMAND"' ERR

usage() {
  sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
  case "$1" in
    --bin-dir) [ $# -ge 2 ] || die "--bin-dir needs a directory"; BIN_DIR="$2"; shift 2 ;;
    --workspace) [ $# -ge 2 ] || die "--workspace needs a directory"; WORKSPACE="$2"; shift 2 ;;
    --prefix) [ $# -ge 2 ] || die "--prefix needs a directory"; PREFIX="$2"; shift 2 ;;
    --yes|-y) ASSUME_YES=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1 (see --help)" ;;
  esac
done

# ── 1. Platform ────────────────────────────────────────────────────────────
OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS" in
  Darwin|Linux) ;;
  *) die "unsupported OS '$OS' (pilot installer supports macOS and Linux; on Windows build from source, see SETUP.md)" ;;
esac
case "$ARCH" in
  x86_64|amd64|arm64|aarch64) ;;
  *) die "unsupported architecture '$ARCH' (x86_64 and arm64/aarch64 only)" ;;
esac
info "platform: $OS $ARCH"

[ -d "$WORKSPACE" ] || die "workspace directory not found: $WORKSPACE"
WORKSPACE="$(cd "$WORKSPACE" && pwd -P)"

# ── 2. Binary source ───────────────────────────────────────────────────────
if [ -z "$BIN_DIR" ]; then
  REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd -P)"
  [ -f "$REPO_ROOT/Cargo.toml" ] || die "no --bin-dir given and $REPO_ROOT is not a MeshMCP checkout"
  command -v cargo >/dev/null 2>&1 || die "no --bin-dir given and cargo is not installed (Rust 1.80+ needed to build)"
  info "building release binaries in $REPO_ROOT (cargo build --release)…"
  (cd "$REPO_ROOT" && cargo build --release -p mesh-server -p mesh-daemon) \
    || die "cargo build --release failed"
  BIN_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}/release"
fi
[ -d "$BIN_DIR" ] || die "binary directory not found: $BIN_DIR"
for b in mesh-mcp meshd; do
  [ -f "$BIN_DIR/$b" ] || die "missing $BIN_DIR/$b (both mesh-mcp and meshd are required)"
done
# Executing it catches a wrong-architecture or corrupt binary before install.
SRC_VERSION="$("$BIN_DIR/mesh-mcp" --version 2>&1)" \
  || die "$BIN_DIR/mesh-mcp does not run on this machine: $SRC_VERSION"
info "source: $BIN_DIR ($SRC_VERSION)"

# ── 3. Install (copy only what differs; atomic rename) ─────────────────────
mkdir -p "$PREFIX" || die "cannot create $PREFIX"
for b in mesh-mcp meshd; do
  src="$BIN_DIR/$b"
  dst="$PREFIX/$b"
  if [ -f "$dst" ] && cmp -s "$src" "$dst"; then
    info "$dst unchanged"
  else
    tmp="$dst.tmp.$$"
    cp "$src" "$tmp" || die "cannot copy $src to $tmp"
    chmod 755 "$tmp" || die "cannot chmod $tmp"
    mv -f "$tmp" "$dst" || die "cannot move $tmp to $dst"
    info "installed $dst"
  fi
done
case ":${PATH}:" in
  *":${PREFIX}:"*) ;;
  *) info "⚠ $PREFIX is not on your PATH; IDE configs call 'mesh-mcp' by name. Add: export PATH=\"$PREFIX:\$PATH\"" ;;
esac
MESH="$PREFIX/mesh-mcp"

# ── 4. init --write-ide-config (after confirmation) ────────────────────────
confirm() {
  if [ "$ASSUME_YES" -eq 1 ]; then
    return 0
  fi
  if [ ! -t 0 ]; then
    die "confirmation needed but stdin is not a terminal; re-run with --yes to accept"
  fi
  printf '%s [y/N] ' "$1" >&2
  answer=""
  read -r answer || answer=""
  case "$answer" in
    y|Y|yes|YES) return 0 ;;
    *) return 1 ;;
  esac
}

if confirm "Run 'mesh-mcp init --auto --write-ide-config' in $WORKSPACE (merges into .cursor/mcp.json and .vscode/mcp.json)?"; then
  CONFIG="$WORKSPACE/.agents/mesh-mcp.toml"
  SAVED=""
  if [ -f "$CONFIG" ]; then
    # `init` regenerates .agents/mesh-mcp.toml; a pilot's hand-tuned config wins.
    SAVED="$(mktemp)"
    cp "$CONFIG" "$SAVED" || die "cannot back up $CONFIG"
  fi
  INIT_LOG="$(mktemp)"
  if ! (cd "$WORKSPACE" && "$MESH" init --auto --write-ide-config) >"$INIT_LOG" 2>&1; then
    cat "$INIT_LOG" >&2
    [ -z "$SAVED" ] || cp "$SAVED" "$CONFIG"
    die "mesh-mcp init failed (output above)"
  fi
  if [ -n "$SAVED" ]; then
    cp "$SAVED" "$CONFIG" || die "cannot restore $CONFIG from $SAVED"
    rm -f "$SAVED"
    info "kept existing $CONFIG unchanged"
  fi
  rm -f "$INIT_LOG"
  info "init done in $WORKSPACE"
else
  info "init skipped (not confirmed)"
fi

# ── 5. doctor --json ───────────────────────────────────────────────────────
DOCTOR_JSON="$(mktemp)"
DOCTOR_LOG="$(mktemp)"
if ! (cd "$WORKSPACE" && "$MESH" doctor --json) >"$DOCTOR_JSON" 2>"$DOCTOR_LOG"; then
  cat "$DOCTOR_LOG" >&2
  die "mesh-mcp doctor failed (output above)"
fi
if [ "$(head -c 1 "$DOCTOR_JSON")" != "[" ]; then
  cat "$DOCTOR_LOG" "$DOCTOR_JSON" >&2
  die "mesh-mcp doctor --json did not print a JSON array"
fi
# Pretty-printed by serde: one `"name": ...` and one `"status": ...` line per check.
SUMMARY="$(awk -F'"' '/"name":/ {n=$4} /"status":/ {print $4 "\t" n}' "$DOCTOR_JSON")"
echo "$SUMMARY" | while IFS="$(printf '\t')" read -r status name; do
  if [ -n "$status" ]; then info "doctor: $status $name"; fi
done
if echo "$SUMMARY" | grep -q '^error'; then
  cat "$DOCTOR_LOG" "$DOCTOR_JSON" >&2
  die "doctor reported errors (details above); 'mesh-mcp doctor --fix' repairs the fixable ones"
fi
rm -f "$DOCTOR_JSON" "$DOCTOR_LOG"
info "✔ pilot install complete ($SRC_VERSION)"
