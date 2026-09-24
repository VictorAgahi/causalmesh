#!/usr/bin/env bash
set -euo pipefail
read -r name url depth <<< "$1"
dest="$CLONE_DIR/$name"
if [[ -d "$dest/.git" ]]; then
  echo "[$name] already cloned, skipping"
else
  echo "[$name] cloning (depth=$depth)..."
  if git clone --depth "$depth" --single-branch "$url" "$dest" > "/tmp/mesh-bench-clone-$name.log" 2>&1; then
    echo "[$name] clone OK"
  else
    echo "[$name] CLONE FAILED (see /tmp/mesh-bench-clone-$name.log)"
  fi
fi
