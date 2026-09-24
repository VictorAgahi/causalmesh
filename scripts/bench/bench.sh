#!/usr/bin/env bash
# End-to-end MeshMCP benchmark: clone real-world repos, set up MCP config on
# each, then functionally exercise the MCP server over stdio.
#
# Parallelism notes (read before raising -j):
#   - Cloning is pure I/O, safe at any concurrency your network/disk handles.
#   - `init`/`doctor` only touch the target repo's own .agents/mesh-mcp.toml,
#     safe to parallelize.
#   - The functional MCP step launches `mesh-mcp run --standalone`, which
#     never talks to the shared meshd daemon (~/.cache/mesh/meshd.sock) — if
#     you ever drop --standalone, concurrent repos WILL collide on that one
#     daemon's in-memory snapshot. audit.db itself is WAL-mode SQLite and
#     already multi-process safe, so it is not the bottleneck; the daemon
#     singleton is. We keep functional-step concurrency modest by default
#     anyway to keep results comparable (CPU contention skews timings).
#
# Usage:
#   scripts/bench/bench.sh [repos.txt] [clone_dir]
#
# Env overrides:
#   CLONE_JOBS=6        parallel git clones
#   SETUP_JOBS=4        parallel init+doctor
#   BENCH_JOBS=2        parallel functional MCP runs
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
REPOS_FILE="${1:-$SCRIPT_DIR/repos.txt}"
CLONE_DIR="${2:-$HOME/bench-repos}"
RESULTS_DIR="$SCRIPT_DIR/results/$(date +%Y%m%d-%H%M%S)"

CLONE_JOBS="${CLONE_JOBS:-6}"
SETUP_JOBS="${SETUP_JOBS:-4}"
BENCH_JOBS="${BENCH_JOBS:-2}"

MESH_MCP_BIN="$REPO_ROOT/target/release/mesh-mcp"

export CLONE_DIR RESULTS_DIR MESH_MCP_BIN SCRIPT_DIR

mkdir -p "$CLONE_DIR" "$RESULTS_DIR"

if [[ ! -x "$MESH_MCP_BIN" ]]; then
  echo "Building mesh-mcp --release first..."
  (cd "$REPO_ROOT" && cargo build --release -p mesh-server)
fi

CLEAN_REPOS="$RESULTS_DIR/.repos.clean.txt"
grep -vE '^[[:space:]]*#|^[[:space:]]*$' "$REPOS_FILE" > "$CLEAN_REPOS"

echo "== Step 1/3: clone (${CLONE_JOBS} parallel) =="
xargs -P "$CLONE_JOBS" -I{} bash "$SCRIPT_DIR/step_clone.sh" {} < "$CLEAN_REPOS"

echo "== Step 2/3: mesh-mcp init + doctor (${SETUP_JOBS} parallel) =="
xargs -P "$SETUP_JOBS" -I{} bash "$SCRIPT_DIR/step_setup.sh" {} < "$CLEAN_REPOS"

echo "== Step 3/3: functional MCP benchmark (${BENCH_JOBS} parallel, --standalone) =="
xargs -P "$BENCH_JOBS" -I{} bash "$SCRIPT_DIR/step_functional.sh" {} < "$CLEAN_REPOS"

echo "== Building report =="
python3 "$SCRIPT_DIR/report.py" "$RESULTS_DIR" | tee "$RESULTS_DIR/REPORT.md"

echo
echo "Results dir: $RESULTS_DIR"
