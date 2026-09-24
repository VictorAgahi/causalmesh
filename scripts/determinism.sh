#!/usr/bin/env bash
# scripts/determinism.sh — idempotence gate (P0 invariant I1).
#
# Indexes each workspace several times, sequentially and then concurrently
# (the concurrent runs create the CPU contention that used to change the
# result), and requires exactly one content fingerprint per workspace, as
# printed by `mesh-mcp graph --format fingerprint`.
#
# usage: scripts/determinism.sh [workspace_dir ...]
#   default workspaces: examples/polyglot-shop examples/volontariapp-fixture
# env:
#   MESH_MCP_BIN     binary to test (default: target/release/mesh-mcp, built if missing)
#   SEQUENTIAL_RUNS  sequential runs per workspace (default: 5)
#   CONCURRENT_RUNS  concurrent runs per workspace (default: 8)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN="${MESH_MCP_BIN:-$REPO_ROOT/target/release/mesh-mcp}"
SEQUENTIAL_RUNS="${SEQUENTIAL_RUNS:-5}"
CONCURRENT_RUNS="${CONCURRENT_RUNS:-8}"

if [[ ! -x "$BIN" ]]; then
  echo "👉 Building release binary..."
  cargo build --release -p mesh-server --manifest-path "$REPO_ROOT/Cargo.toml"
fi

if [[ $# -gt 0 ]]; then
  WORKSPACES=("$@")
else
  WORKSPACES=("$REPO_ROOT/examples/polyglot-shop" "$REPO_ROOT/examples/volontariapp-fixture")
fi

OUT_DIR="$(mktemp -d)"
trap 'rm -rf "$OUT_DIR"' EXIT

fingerprint() {
  (cd "$1" && "$BIN" graph --format fingerprint 2>/dev/null) | head -n 1
}

status=0
for ws in "${WORKSPACES[@]}"; do
  name="$(basename "$ws")"
  runs="$OUT_DIR/$name"
  mkdir -p "$runs"

  for i in $(seq 1 "$SEQUENTIAL_RUNS"); do
    fingerprint "$ws" > "$runs/sequential-$i"
  done
  for i in $(seq 1 "$CONCURRENT_RUNS"); do
    fingerprint "$ws" > "$runs/concurrent-$i" &
  done
  wait

  total=$((SEQUENTIAL_RUNS + CONCURRENT_RUNS))
  distinct="$(sort -u "$runs"/* | wc -l | tr -d ' ')"
  if [[ "$distinct" == "1" ]]; then
    echo "✔ $name: 1 fingerprint over $total runs ($(cut -d' ' -f2 "$runs/sequential-1" | cut -c1-16)…)"
  else
    echo "✖ $name: $distinct distinct fingerprints over $total runs ($SEQUENTIAL_RUNS sequential + $CONCURRENT_RUNS concurrent)"
    sort "$runs"/* | uniq -c | sed 's/^/    /'
    status=1
  fi
done

exit "$status"
