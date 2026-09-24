#!/usr/bin/env python3
"""Calls one MeshMCP graph tool (find_dependents/analyze_grpc/analyze_impact)
against a running --standalone instance, prints ELAPSED_MS to stderr and the
raw response text to stdout."""
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
    repo_dir, config_path, tool, args_json = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
    mesh_mcp_bin = os.environ["MESH_MCP_BIN"]
    env = os.environ.copy()
    env["MESH_SOCKET_PATH"] = f"/tmp/mesh-graphcmp-{os.path.basename(repo_dir)}-{tool}.sock"

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
                "params": {"name": tool, "arguments": json.loads(args_json)}})
    resp = recv(proc)
    elapsed_ms = (time.monotonic() - t0) * 1000

    try:
        proc.terminate()
    except Exception:
        pass

    result = resp.get("result")
    if result is None:
        text = f"ERROR: {resp.get('error')}"
    else:
        text = result.get("content", [{}])[0].get("text", "")
    sys.stderr.write(f"ELAPSED_MS={elapsed_ms:.1f}\n")
    sys.stdout.write(text)


if __name__ == "__main__":
    main()
