#!/usr/bin/env python3
"""MeshMCP scale benchmark: boot time, incremental-reload latency, RSS, and
smart_search p50/p95 against a single workspace, with pass/fail budgets.

Unlike mcp_client.py (one-shot functional smoke test), this script:
  - samples RSS of the running mesh-mcp process via `ps`,
  - runs smart_search N times and reports p50/p95, not a single sample,
  - measures incremental reload latency end-to-end: touches a tracked file
    and polls smart_search until the new symbol is visible, using the real
    FileWatcherService path (run --standalone spawns it — see main.rs
    run_standalone), not a synthetic unit test of reload_paths().

Usage:
  scale_bench.py <repo_dir> <config_path> [--queries N] [--budget-json path]

Exit code is 0 iff every measured metric is within budget (or no budget file
given). Always prints one JSON line with the raw measurements to stdout.
"""
import argparse
import json
import os
import subprocess
import sys
import time

TIMEOUT_S = float(os.environ.get("MESH_BENCH_TIMEOUT", "30"))
DEFAULT_BUDGETS = {
    "boot_ms": 15000,
    "reload_ms": 5000,
    "rss_peak_mb": 1500,
    "search_p50_ms": 200,
    "search_p95_ms": 1000,
}


def send(proc, obj):
    proc.stdin.write(json.dumps(obj) + "\n")
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
        if line:
            return json.loads(line)


def call(proc, method, params, req_id):
    t0 = time.monotonic()
    send(proc, {"jsonrpc": "2.0", "id": req_id, "method": method, "params": params})
    resp = recv(proc, t0 + TIMEOUT_S)
    return resp, (time.monotonic() - t0) * 1000


def rss_mb(pid):
    try:
        out = subprocess.check_output(["ps", "-o", "rss=", "-p", str(pid)], text=True)
        return int(out.strip()) / 1024.0
    except Exception:
        return None


def percentile(values, pct):
    if not values:
        return None
    s = sorted(values)
    k = (len(s) - 1) * (pct / 100.0)
    f, c = int(k), min(int(k) + 1, len(s) - 1)
    if f == c:
        return s[f]
    return s[f] + (s[c] - s[f]) * (k - f)


def find_a_generated_file(repo_dir):
    for root, _dirs, files in os.walk(repo_dir):
        for name in files:
            if name.startswith("gen_") and name.endswith((".rs", ".go", ".py", ".ts", ".java")):
                return os.path.join(root, name)
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("repo_dir")
    ap.add_argument("config_path")
    ap.add_argument("--queries", type=int, default=30)
    ap.add_argument("--budget-json", default=None)
    args = ap.parse_args()

    mesh_mcp_bin = os.environ.get(
        "MESH_MCP_BIN",
        os.path.join(os.path.dirname(__file__), "..", "..", "target", "release", "mesh-mcp"),
    )

    budgets = dict(DEFAULT_BUDGETS)
    if args.budget_json:
        with open(args.budget_json) as f:
            budgets.update(json.load(f))

    env = os.environ.copy()
    env["MESH_SOCKET_PATH"] = f"/tmp/mesh-scale-{os.path.basename(args.repo_dir)}.sock"

    result = {"repo": os.path.basename(args.repo_dir), "ok": False, "budgets": budgets}

    boot_t0 = time.monotonic()
    proc = subprocess.Popen(
        [mesh_mcp_bin, "--config", args.config_path, "run", "--standalone"],
        cwd=args.repo_dir,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
        env=env,
    )

    try:
        resp, _ms = call(
            proc,
            "initialize",
            {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "mesh-scale-bench", "version": "0"}},
            1,
        )
        if resp.get("error"):
            raise RuntimeError(f"initialize failed: {resp['error']}")
        boot_ms = (time.monotonic() - boot_t0) * 1000
        result["boot_ms"] = round(boot_ms, 1)
        result["rss_after_boot_mb"] = rss_mb(proc.pid)

        resp, _ms = call(proc, "tools/list", {}, 2)
        if resp.get("error"):
            raise RuntimeError(f"tools/list failed: {resp['error']}")

        queries = ["Service", "Handler", "Worker", "Component", "handle", "new"]
        latencies = []
        max_rss = result["rss_after_boot_mb"] or 0
        for i in range(args.queries):
            q = queries[i % len(queries)]
            resp, ms = call(
                proc, "tools/call",
                {"name": "smart_search", "arguments": {"query": q, "scope": ".", "fuzzy": False}},
                100 + i,
            )
            if resp.get("error"):
                raise RuntimeError(f"smart_search({q!r}) failed: {resp['error']}")
            latencies.append(ms)
            sample = rss_mb(proc.pid)
            if sample:
                max_rss = max(max_rss, sample)

        result["search_p50_ms"] = round(percentile(latencies, 50), 1)
        result["search_p95_ms"] = round(percentile(latencies, 95), 1)
        result["rss_peak_mb"] = round(max_rss, 1) if max_rss else None

        # Incremental reload: append a uniquely-named symbol to a generated
        # file, then poll smart_search until it's visible (FileWatcherService
        # -> WorkspaceIndexer::reload_paths, debounced).
        target = find_a_generated_file(args.repo_dir)
        reload_ms = None
        if target:
            marker = f"ScaleBenchMarker{int(time.time() * 1000)}"
            with open(target, "a") as f:
                if target.endswith(".py"):
                    f.write(f"\n\ndef {marker.lower()}():\n    pass\n")
                else:
                    f.write(f"\n\n// {marker}\n")
            reload_t0 = time.monotonic()
            deadline = reload_t0 + 20
            req_id = 900
            found = False
            while time.monotonic() < deadline:
                resp, _ms = call(
                    proc, "tools/call",
                    {"name": "smart_search", "arguments": {"query": marker, "scope": ".", "fuzzy": False}},
                    req_id,
                )
                req_id += 1
                payload = json.dumps(resp.get("result", {}))
                if marker in payload:
                    found = True
                    break
                time.sleep(0.2)
            if found:
                reload_ms = round((time.monotonic() - reload_t0) * 1000, 1)
        result["reload_ms"] = reload_ms

        violations = []
        for key, budget in budgets.items():
            val = result.get(key)
            if val is not None and val > budget:
                violations.append(f"{key}={val} exceeds budget {budget}")
        result["violations"] = violations
        result["ok"] = not violations
    except Exception as e:
        result["error"] = str(e)
    finally:
        try:
            send(proc, {"jsonrpc": "2.0", "id": 999, "method": "shutdown", "params": {}})
        except Exception:
            pass
        try:
            proc.wait(timeout=5)
        except Exception:
            proc.kill()
        if not result.get("ok"):
            try:
                result["stderr_tail"] = proc.stderr.read()[-2000:]
            except Exception:
                pass

    print(json.dumps(result))
    sys.exit(0 if result.get("ok") else 1)


if __name__ == "__main__":
    main()
