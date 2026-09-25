#!/usr/bin/env bash
# scripts/golden/fetch.sh — clones (or updates) the Plan 2 golden-corpus repos
# and checks each out at the exact commit its tests/golden/<name>.expected.yaml
# was written against.
#
# usage: scripts/golden/fetch.sh
# env:
#   GOLDEN_CACHE_DIR   where repos are cloned (default: ~/.cache/mesh-golden)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CACHE_DIR="${GOLDEN_CACHE_DIR:-$HOME/.cache/mesh-golden}"
REPOS_FILE="$SCRIPT_DIR/repos.txt"

mkdir -p "$CACHE_DIR"

while read -r name url sha; do
  [[ -z "$name" || "$name" == \#* ]] && continue
  dest="$CACHE_DIR/$name"

  if [[ ! -d "$dest/.git" ]]; then
    echo "[$name] cloning..."
    git clone --quiet "$url" "$dest"
  fi

  if ! git -C "$dest" cat-file -e "$sha^{commit}" 2>/dev/null; then
    echo "[$name] fetching pinned commit $sha..."
    git -C "$dest" fetch --quiet origin "$sha"
  fi

  git -C "$dest" checkout --quiet --detach "$sha"
  echo "[$name] pinned at $sha ($dest)"
done < "$REPOS_FILE"
