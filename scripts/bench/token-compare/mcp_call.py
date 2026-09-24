#!/usr/bin/env python3
"""Calls smart_search on a running mesh-mcp --standalone instance and prints
the raw response text to stdout (for token counting), nothing else."""
import json
import os
import subprocess
import sys
import time


def send(proc, obj):
    proc.stdin.write(json.dumps(obj) + "\n")
    proc.stdin.flush()


def recv(proc):
    return json.loads(proc.stdout.readline())


def main():
    repo_dir, config_path, query = sys.argv[1], sys.argv[2], sys.argv[3]
    mesh_mcp_bin = os.environ["MESH_MCP_BIN"]
    env = os.environ.copy()
    env["MESH_SOCKET_PATH"] = f"/tmp/mesh-tokcmp-{os.path.basename(repo_dir)}.sock"

    # Time the whole cold-start-to-answer path: process spawn (which includes
    # the synchronous boot scan gating `initialize`, see the plan's point #3)
    # through the smart_search response — this is what a user actually waits
    # for on a fresh `mesh-mcp run`, not just the query itself.
    t0 = time.monotonic()
    proc = subprocess.Popen(
        [mesh_mcp_bin, "--config", config_path, "run", "--standalone"],
        cwd=repo_dir, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL, text=True, bufsize=1, env=env,
    )
    send(proc, {"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "x", "version": "0"}}})
    recv(proc)

    send(proc, {"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": "smart_search", "arguments": {"query": query, "scope": ".", "fuzzy": False}}})
    resp = recv(proc)
    elapsed_ms = (time.monotonic() - t0) * 1000

    try:
        proc.terminate()
    except Exception:
        pass

    text = resp.get("result", {}).get("content", [{}])[0].get("text", "")
    sys.stderr.write(f"ELAPSED_MS={elapsed_ms:.1f}\n")
    sys.stdout.write(text)


if __name__ == "__main__":
    main()
