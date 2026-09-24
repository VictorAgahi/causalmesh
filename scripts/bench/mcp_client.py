#!/usr/bin/env python3
"""Minimal MCP stdio client used to functionally exercise a mesh-mcp instance.

Speaks the same protocol as crates/mesh-server/src/framing.rs: one JSON-RPC
object per line (ndjson), not LSP Content-Length framing.

Always launches the server with `run --standalone` so it never touches the
shared meshd daemon socket — this is what makes it safe to run several of
these concurrently against different repos (see scripts/bench/README.md).
"""
import json
import os
import subprocess
import sys
import time

TIMEOUT_S = float(os.environ.get("MESH_BENCH_TIMEOUT", "30"))


def send(proc, obj):
    line = json.dumps(obj) + "\n"
    proc.stdin.write(line)
    proc.stdin.flush()


def recv(proc, deadline):
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("no response before deadline")
        line = proc.stdout.readline()
        if line == "":
            raise EOFError("server closed stdout (check stderr log)")
        line = line.strip()
        if not line:
            continue
        return json.loads(line)


def call(proc, method, params, req_id):
    t0 = time.monotonic()
    send(proc, {"jsonrpc": "2.0", "id": req_id, "method": method, "params": params})
    resp = recv(proc, t0 + TIMEOUT_S)
    elapsed_ms = (time.monotonic() - t0) * 1000
    return resp, elapsed_ms


def main():
    if len(sys.argv) < 3:
        print("usage: mcp_client.py <repo_dir> <config_path> [search_query]", file=sys.stderr)
        sys.exit(2)

    repo_dir = sys.argv[1]
    config_path = sys.argv[2]
    search_query = sys.argv[3] if len(sys.argv) > 3 else "main"

    mesh_mcp_bin = os.environ.get(
        "MESH_MCP_BIN",
        os.path.join(os.path.dirname(__file__), "..", "..", "target", "release", "mesh-mcp"),
    )

    env = os.environ.copy()
    # Belt-and-suspenders isolation on top of --standalone: even if a code
    # path ever tries the daemon, it hits a repo-unique socket, never the
    # shared ~/.cache/mesh/meshd.sock.
    env["MESH_SOCKET_PATH"] = f"/tmp/mesh-bench-{os.path.basename(repo_dir)}.sock"

    proc = subprocess.Popen(
        [mesh_mcp_bin, "--config", config_path, "run", "--standalone"],
        cwd=repo_dir,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
        env=env,
    )

    results = {"repo": os.path.basename(repo_dir), "ok": False, "steps": {}}
    try:
        resp, ms = call(
            proc,
            "initialize",
            {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "mesh-bench", "version": "0"}},
            1,
        )
        results["steps"]["initialize"] = {"ms": round(ms, 1), "error": resp.get("error")}
        if resp.get("error"):
            raise RuntimeError(f"initialize failed: {resp['error']}")

        resp, ms = call(proc, "tools/list", {}, 2)
        tools = [t["name"] for t in resp.get("result", {}).get("tools", [])]
        results["steps"]["tools/list"] = {"ms": round(ms, 1), "count": len(tools), "error": resp.get("error")}
        if resp.get("error"):
            raise RuntimeError(f"tools/list failed: {resp['error']}")

        resp, ms = call(
            proc,
            "tools/call",
            {"name": "smart_search", "arguments": {"query": search_query, "scope": ".", "fuzzy": False}},
            3,
        )
        err = resp.get("error")
        payload = resp.get("result")
        size = len(json.dumps(payload)) if payload is not None else 0
        results["steps"]["smart_search"] = {"ms": round(ms, 1), "response_bytes": size, "error": err}
        if err:
            raise RuntimeError(f"smart_search failed: {err}")

        results["ok"] = True
    except Exception as e:
        results["error"] = str(e)
    finally:
        try:
            send(proc, {"jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": {}})
        except Exception:
            pass
        try:
            proc.wait(timeout=5)
        except Exception:
            proc.kill()
        stderr_tail = ""
        try:
            proc.stderr.flush()
        except Exception:
            pass
        if not results["ok"]:
            try:
                stderr_tail = proc.stderr.read()[-2000:]
            except Exception:
                pass
            results["stderr_tail"] = stderr_tail

    print(json.dumps(results))


if __name__ == "__main__":
    main()
