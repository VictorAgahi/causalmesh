#!/usr/bin/env bash
# Token-level comparison: naive rg full-text scan vs rtk grep (compressed) vs
# mesh-mcp smart_search, for the same query on the same repo. All three raw
# outputs are saved so tiktoken can count them with one consistent tokenizer
# (count.py), rather than trusting each tool's own self-reported numbers.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
OUT_DIR="$SCRIPT_DIR/out"
mkdir -p "$OUT_DIR"

export MESH_MCP_BIN="$REPO_ROOT/target/release/mesh-mcp"
QUERY="${1:-main}"
CLONE_DIR="${2:-$HOME/bench-repos}"

REPOS="deno googleapis grpc kubernetes linux react rust-lang vscode"

for name in $REPOS; do
  dest="$CLONE_DIR/$name"
  cfg="$dest/.agents/mesh-mcp.toml"
  echo "== $name ==" >&2

  # 1. naive rg full-text scan (git-aware excludes: .gitignore already skips
  #    build/vendor dirs in these real repos, same as a plain Grep tool call)
  t0=$(date +%s.%N)
  rg -n --no-heading "$QUERY" "$dest" > "$OUT_DIR/$name.rg.txt" 2>/dev/null
  t1=$(date +%s.%N)
  echo "$t1 $t0" | awk '{printf "%.1f\n", ($1-$2)*1000}' > "$OUT_DIR/$name.rg.ms"

  # 2. rtk grep (already-compressed baseline)
  t0=$(date +%s.%N)
  rtk grep -r "$QUERY" "$dest" > "$OUT_DIR/$name.rtk.txt" 2>/dev/null
  t1=$(date +%s.%N)
  echo "$t1 $t0" | awk '{printf "%.1f\n", ($1-$2)*1000}' > "$OUT_DIR/$name.rtk.ms"

  # 3. mesh-mcp smart_search (full standalone boot + query, same as production use)
  python3 "$SCRIPT_DIR/mcp_call.py" "$dest" "$cfg" "$QUERY" \
    > "$OUT_DIR/$name.mesh.txt" 2> "$OUT_DIR/$name.mesh.stderr"
  grep -o 'ELAPSED_MS=.*' "$OUT_DIR/$name.mesh.stderr" | cut -d= -f2 > "$OUT_DIR/$name.mesh.ms"

  echo "  rg: $(cat "$OUT_DIR/$name.rg.ms")ms  rtk: $(cat "$OUT_DIR/$name.rtk.ms")ms  mesh: $(cat "$OUT_DIR/$name.mesh.ms" 2>/dev/null || echo '?')ms" >&2
done

echo "done" >&2
