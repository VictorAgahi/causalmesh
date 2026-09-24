#!/usr/bin/env bash
# PreToolUse hook on Read: blocks a whole-file read of a large file when no
# offset/limit was given, and points the caller at mesh-mcp's smart_search
# (symbol-aware, AST-decapitated) instead of dumping the raw file into context.
#
# Threshold and MCP tool name are overridable via env for local tuning:
#   MESH_HOOK_MAX_LINES (default 300)
set -euo pipefail

MAX_LINES="${MESH_HOOK_MAX_LINES:-300}"

input="$(cat)"

file="$(jq -r '.tool_input.file_path // empty' <<<"$input")"
offset="$(jq -r '.tool_input.offset // empty' <<<"$input")"
limit="$(jq -r '.tool_input.limit // empty' <<<"$input")"

# A targeted read (offset/limit already given) is exactly what we want to
# encourage — never block it.
if [[ -n "$offset" || -n "$limit" ]]; then
  exit 0
fi

if [[ -z "$file" || ! -f "$file" ]]; then
  exit 0
fi

lines="$(wc -l < "$file" 2>/dev/null | tr -d ' ')"
if [[ -z "$lines" || "$lines" -le "$MAX_LINES" ]]; then
  exit 0
fi

reason="This file has ${lines} lines (> ${MAX_LINES}). Reading it whole burns context on mostly-irrelevant code. Use the mesh-mcp MCP tool \`smart_search\` (symbol-aware, returns AST-decapitated signatures) to find the specific symbol first, or call Read again with offset/limit for a narrow range."

jq -n --arg reason "$reason" \
  '{hookSpecificOutput: {hookEventName: "PreToolUse", permissionDecision: "deny", permissionDecisionReason: $reason}}'
