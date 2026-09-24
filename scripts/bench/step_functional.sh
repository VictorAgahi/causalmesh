#!/usr/bin/env bash
set -euo pipefail
read -r name url depth <<< "$1"
dest="$CLONE_DIR/$name"
cfg="$dest/.agents/mesh-mcp.toml"
if [[ ! -f "$cfg" ]]; then
  echo "[$name] no config, skip functional test"
  exit 0
fi
MESH_MCP_BIN="$MESH_MCP_BIN" python3 "$SCRIPT_DIR/mcp_client.py" "$dest" "$cfg" \
  > "$RESULTS_DIR/$name.functional.json" 2> "$RESULTS_DIR/$name.functional.stderr.log"
ok=$(python3 -c "import json;print(json.load(open('$RESULTS_DIR/$name.functional.json'))['ok'])")
echo "[$name] functional test: $ok"
