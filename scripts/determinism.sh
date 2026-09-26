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
#
# The fingerprint covers absolute file paths (node, doc section and property
# source paths), so it is only comparable between runs over the same checkout
# path: the same content checked out elsewhere, or on another machine or OS,
# gives a different value. This gate compares runs within one checkout only.
#
# Every failure is reported, never silent: a failing mesh-mcp run prints its
# command, exit code and captured stderr, and any other failing command is
# reported by the ERR trap (command, line, exit code).
#
# Portability: must run under bash 3.2 (macOS /bin/bash) and bash 5 (ubuntu),
# so no `wait -n`, `mapfile`, associative arrays or empty-array expansions.
#
# Why the binary's stdout is captured to a file instead of piped (4.12a): the
# fingerprint is 4 lines, written by Rust's line-buffered stdout in more than
# one write(2). The previous `mesh-mcp … | head -n 1` let `head` exit after the
# first line; when the scheduler ran it between two writes (a loaded macOS
# runner), the next write hit a closed pipe, mesh-mcp exited 1 on EPIPE with
# its stderr discarded, and `pipefail` + `set -e` ended the script with exit 1
# and no output at all.
set -Eeuo pipefail

on_err() {
  local rc=$? line=$1 cmd=$2
  echo "✖ determinism.sh: command failed (exit $rc) at line $line: $cmd" >&2
  exit "$rc"
}
trap 'on_err "$LINENO" "$BASH_COMMAND"' ERR

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN="${MESH_MCP_BIN:-$REPO_ROOT/target/release/mesh-mcp}"
SEQUENTIAL_RUNS="${SEQUENTIAL_RUNS:-5}"
CONCURRENT_RUNS="${CONCURRENT_RUNS:-8}"

if [[ ! -x "$BIN" ]]; then
  echo "👉 Building release binary..."
  cargo build --release -p mesh-server --manifest-path "$REPO_ROOT/Cargo.toml"
fi
if [[ ! -x "$BIN" ]]; then
  echo "✖ determinism.sh: mesh-mcp binary not found or not executable: $BIN" >&2
  exit 1
fi
# Each run cds into its workspace, so a relative MESH_MCP_BIN must be made
# absolute here or every run would fail with "No such file or directory".
case "$BIN" in
  /*) ;;
  *) BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")" ;;
esac

for var in SEQUENTIAL_RUNS CONCURRENT_RUNS; do
  case "${!var}" in
    '' | *[!0-9]*)
      echo "✖ determinism.sh: $var must be a non-negative integer, got '${!var}'" >&2
      exit 1
      ;;
  esac
done
if [[ $((SEQUENTIAL_RUNS + CONCURRENT_RUNS)) -lt 2 ]]; then
  echo "✖ determinism.sh: SEQUENTIAL_RUNS + CONCURRENT_RUNS must be at least 2 to compare fingerprints" >&2
  exit 1
fi

if [[ $# -gt 0 ]]; then
  WORKSPACES=("$@")
else
  WORKSPACES=("$REPO_ROOT/examples/polyglot-shop" "$REPO_ROOT/examples/volontariapp-fixture")
fi

OUT_DIR="$(mktemp -d)"
trap 'rm -rf "$OUT_DIR"' EXIT

# fingerprint <workspace> <log_prefix> <result_file>
# Runs `mesh-mcp graph --format fingerprint` in <workspace> with stdout and
# stderr captured whole to <log_prefix>.out / .err (no pipe, so no reader can
# close early), then writes the `fingerprint: <sha>` line to <result_file>.
# On any failure, writes the command, exit code and captured stdout/stderr to
# <log_prefix>.report and returns non-zero; the caller prints the reports once
# every run has finished (concurrent runs printing directly would interleave).
fingerprint() {
  local ws=$1 out=$2 result=$3 rc=0 first
  (cd "$ws" && "$BIN" graph --format fingerprint) >"$out.out" 2>"$out.err" || rc=$?
  if [[ $rc -ne 0 ]]; then
    {
      echo "✖ mesh-mcp failed (exit $rc) in $ws"
      echo "    command: (cd $ws && $BIN graph --format fingerprint)"
      echo "    stderr:"
      sed 's/^/      /' "$out.err"
    } >"$out.report"
    return "$rc"
  fi
  first=""
  IFS= read -r first <"$out.out" || true
  case "$first" in
    "fingerprint: "?*) ;;
    *)
      {
        echo "✖ mesh-mcp printed no 'fingerprint: …' line in $ws (exit 0)"
        echo "    stdout:"
        sed 's/^/      /' "$out.out"
        echo "    stderr:"
        sed 's/^/      /' "$out.err"
      } >"$out.report"
      return 1
      ;;
  esac
  printf '%s\n' "$first" >"$result"
}

status=0
ws_index=0
for ws in "${WORKSPACES[@]}"; do
  ws_index=$((ws_index + 1))
  if [[ ! -d "$ws" ]]; then
    echo "✖ determinism.sh: workspace is not a directory: $ws" >&2
    status=1
    continue
  fi
  name="$(basename "$ws")"
  # Indexed, so two workspaces sharing a basename never share result/log files.
  runs="$OUT_DIR/$ws_index-$name"
  logs="$OUT_DIR/$ws_index-$name.logs"
  mkdir -p "$runs" "$logs"
  failed=0

  i=1
  while [[ $i -le $SEQUENTIAL_RUNS ]]; do
    fingerprint "$ws" "$logs/sequential-$i" "$runs/sequential-$i" || failed=$((failed + 1))
    i=$((i + 1))
  done

  # Wait on each background run by pid: a bare `wait` returns 0 even when a
  # run failed, which used to leave an empty result file behind silently.
  pids=""
  i=1
  while [[ $i -le $CONCURRENT_RUNS ]]; do
    fingerprint "$ws" "$logs/concurrent-$i" "$runs/concurrent-$i" &
    pids="$pids $!"
    i=$((i + 1))
  done
  for pid in $pids; do
    wait "$pid" || failed=$((failed + 1))
  done

  total=$((SEQUENTIAL_RUNS + CONCURRENT_RUNS))
  if [[ $failed -ne 0 ]]; then
    for report in "$logs"/*.report; do
      if [[ -f "$report" ]]; then cat "$report" >&2; fi
    done
    echo "✖ $name: $failed of $total mesh-mcp runs failed (details above)"
    status=1
    continue
  fi

  distinct="$(sort -u "$runs"/* | wc -l | tr -d ' ')"
  if [[ "$distinct" == "1" ]]; then
    echo "✔ $name: 1 fingerprint over $total runs ($(sort -u "$runs"/* | cut -d' ' -f2 | cut -c1-16)…)"
  else
    echo "✖ $name: $distinct distinct fingerprints over $total runs ($SEQUENTIAL_RUNS sequential + $CONCURRENT_RUNS concurrent)"
    sort "$runs"/* | uniq -c | sed 's/^/    /'
    status=1
  fi
done

exit "$status"
