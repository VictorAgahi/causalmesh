#!/usr/bin/env bash
# Idempotence test for scripts/install_pilot.sh (plan 4 step 4.8).
#
# Runs the installer twice against an isolated HOME and a throwaway workspace,
# snapshots the resulting state after each run (installed binaries, workspace
# files incl. IDE configs, doctor --json output) and requires both snapshots to
# be identical. Also checks that a pre-existing IDE MCP server and a hand-edited
# .agents/mesh-mcp.toml survive, and that failures are loud (non-zero + message).
#
# Never touches the real $HOME: HOME, MESH_SOCKET_PATH and the workspace all
# live under one mktemp directory, removed on exit.
#
# Usage: MESH_BIN_DIR=target/release scripts/test_install_pilot.sh
# (default: ${CARGO_TARGET_DIR:-<repo>/target}/release, must hold mesh-mcp + meshd)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd -P)"
INSTALLER="$REPO_ROOT/scripts/install_pilot.sh"
BIN_DIR="${MESH_BIN_DIR:-${CARGO_TARGET_DIR:-$REPO_ROOT/target}/release}"
BIN_DIR="$(cd "$BIN_DIR" && pwd -P)"

fail() {
  echo "✖ test_install_pilot: $*" >&2
  exit 1
}

# Short path: macOS caps Unix socket paths at 104 bytes.
T="$(mktemp -d /tmp/mpilot.XXXXXX)"
trap 'rm -rf "$T"' EXIT
export HOME="$T/home"
export MESH_SOCKET_PATH="$T/s.sock"
mkdir -p "$HOME"
WS="$T/ws"
mkdir -p "$WS/proto" "$WS/.cursor" "$WS/.agents"
printf 'module example.com/pilot\n\ngo 1.21\n' >"$WS/go.mod"
printf 'syntax = "proto3";\npackage pilot;\nservice Ping { rpc Ping (Req) returns (Res); }\nmessage Req {}\nmessage Res {}\n' >"$WS/proto/ping.proto"
# A user's own MCP server that the merge must keep.
printf '{\n  "mcpServers": {\n    "other-server": { "command": "other", "args": [] }\n  }\n}\n' >"$WS/.cursor/mcp.json"
# A hand-edited config that init must not clobber.
printf '[workspace]\nname = "pilot-hand-edited"\nversion = "0"\nroots = [".."]\n' >"$WS/.agents/mesh-mcp.toml"
cp "$WS/.agents/mesh-mcp.toml" "$T/config.orig"

snapshot() {
  # Every regular file under the installed prefix and the workspace, with its
  # content hash, plus the doctor --json output. Nothing is excluded.
  (
    cd "$T"
    find home/.local ws -type f | LC_ALL=C sort | while read -r f; do
      printf '%s  %s\n' "$(cksum <"$f" | awk '{print $1 "-" $2}')" "$f"
    done
    echo "--- doctor --json"
    (cd "$WS" && "$HOME/.local/bin/mesh-mcp" doctor --json 2>/dev/null)
  )
}

run_installer() {
  (cd "$WS" && bash "$INSTALLER" --bin-dir "$BIN_DIR" --yes) >"$T/run$1.log" 2>&1 \
    || { cat "$T/run$1.log" >&2; fail "installer run $1 failed"; }
}

run_installer 1
snapshot >"$T/state1"
run_installer 2
snapshot >"$T/state2"

if ! diff -u "$T/state1" "$T/state2"; then
  fail "state differs between two successive runs (diff above)"
fi
grep -q 'unchanged' "$T/run2.log" || fail "second run re-copied the binaries: $(cat "$T/run2.log")"

for b in mesh-mcp meshd; do
  cmp -s "$BIN_DIR/$b" "$HOME/.local/bin/$b" || fail "$b not installed into isolated HOME"
done
grep -q '"other-server"' "$WS/.cursor/mcp.json" || fail "pre-existing MCP server dropped from .cursor/mcp.json"
grep -q '"mesh-mcp"' "$WS/.cursor/mcp.json" || fail "mesh-mcp entry missing from .cursor/mcp.json"
cmp -s "$T/config.orig" "$WS/.agents/mesh-mcp.toml" || fail "hand-edited .agents/mesh-mcp.toml was modified"
[ ! -e "$MESH_SOCKET_PATH" ] || fail "installer left a daemon socket behind at $MESH_SOCKET_PATH"

# Failures must be loud: non-zero exit and a message on stderr.
if (cd "$WS" && bash "$INSTALLER" --bin-dir "$T/nope" --yes) >"$T/bad.log" 2>&1; then
  fail "missing --bin-dir did not fail"
fi
grep -q 'binary directory not found' "$T/bad.log" || fail "missing --bin-dir failed silently: $(cat "$T/bad.log")"
if (cd "$WS" && bash "$INSTALLER" --bin-dir "$BIN_DIR" </dev/null) >"$T/noyes.log" 2>&1; then
  fail "no --yes with a non-terminal stdin did not fail"
fi
grep -q 'confirmation needed' "$T/noyes.log" || fail "unconfirmed run failed without explanation: $(cat "$T/noyes.log")"

echo "✔ install_pilot.sh idempotent: $(wc -l <"$T/state1" | tr -d ' ') state lines identical across two runs"
