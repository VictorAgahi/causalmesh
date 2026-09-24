#!/usr/bin/env bash
set -euo pipefail
read -r name url depth <<< "$1"
dest="$CLONE_DIR/$name"
if [[ ! -d "$dest" ]]; then
  echo "[$name] missing clone, skip"
  exit 0
fi
cd "$dest"
"$MESH_MCP_BIN" init --auto > "$RESULTS_DIR/$name.init.log" 2>&1 || echo "[$name] init failed"
if "$MESH_MCP_BIN" doctor --config .agents/mesh-mcp.toml > "$RESULTS_DIR/$name.doctor.log" 2>&1; then
  echo "[$name] doctor OK"
else
  echo "[$name] doctor FAILED (see $RESULTS_DIR/$name.doctor.log)"
fi
