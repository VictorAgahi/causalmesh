#!/usr/bin/env python3
"""Runs every MeshMCP tool against a repo, capturing wall time + full response
text (for later tiktoken counting). One JSON line per call to stdout."""
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


def call_tool(proc, tool, args, req_id):
    t0 = time.monotonic()
    send(proc, {"jsonrpc": "2.0", "id": req_id, "method": "tools/call", "params": {"name": tool, "arguments": args}})
    resp = recv(proc)
    elapsed_ms = (time.monotonic() - t0) * 1000
    result = resp.get("result")
    if result is None:
        return {"tool": tool, "args": args, "ms": round(elapsed_ms, 1), "error": resp.get("error"), "text": ""}
    text = result.get("content", [{}])[0].get("text", "")
    return {"tool": tool, "args": args, "ms": round(elapsed_ms, 1), "error": None, "text": text}


def main():
    repo_dir, config_path, calls_json = sys.argv[1], sys.argv[2], sys.argv[3]
    calls = json.loads(calls_json)  # [{"tool": ..., "args": {...}}, ...]
    mesh_mcp_bin = os.environ["MESH_MCP_BIN"]
    env = os.environ.copy()
    env["MESH_SOCKET_PATH"] = f"/tmp/mesh-fullbench-{os.path.basename(repo_dir)}.sock"

    t_boot = time.monotonic()
    proc = subprocess.Popen(
        [mesh_mcp_bin, "--config", config_path, "run", "--standalone"],
        cwd=repo_dir, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL, text=True, bufsize=1, env=env,
    )
    send(proc, {"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "x", "version": "0"}}})
    recv(proc)
    boot_ms = (time.monotonic() - t_boot) * 1000

    print(json.dumps({"tool": "__boot__", "args": {}, "ms": round(boot_ms, 1), "error": None, "text": ""}))

    for i, call in enumerate(calls, start=2):
        result = call_tool(proc, call["tool"], call["args"], i)
        print(json.dumps(result))

    try:
        proc.terminate()
    except Exception:
        pass


if __name__ == "__main__":
    main()
