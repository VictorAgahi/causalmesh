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
import re
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


_ROOTS_LINE_RE = re.compile(r'^roots\s*=\s*\[(.*)\]', re.MULTILINE)


# `ValidatedScope::resolve` (crates/mesh-core/src/security.rs) requires the
# requested scope to canonicalize *inside one specific allowed root*, not
# merely be an ancestor that happens to contain several roots. A workspace
# with N discovered `services/svc-*` roots (see gen_synthetic.py) therefore
# has no single scope value that covers "the whole repo" the way scope="."
# does for a single-root config — each `tools/call` has to target one real
# root. This reads the roots straight out of the generated
# `.agents/mesh-mcp.toml` (the same roots `WorkspaceIndexer::resolve_roots`
# would compute) instead of assuming ".".
def discover_scopes(repo_dir, config_path):
    config_dir = os.path.dirname(os.path.abspath(config_path))
    m = _ROOTS_LINE_RE.search(open(config_path).read())
    if not m:
        return [repo_dir]
    raw = [r.strip().strip('"') for r in m.group(1).split(",") if r.strip()]
    scopes = []
    for r in raw:
        if "*" in r:
            base = os.path.normpath(os.path.join(config_dir, r.replace("*", "")))
            if os.path.isdir(base):
                for entry in sorted(os.listdir(base)):
                    full = os.path.join(base, entry)
                    if os.path.isdir(full):
                        scopes.append(full)
        else:
            full = os.path.normpath(os.path.join(config_dir, r))
            if os.path.isdir(full):
                scopes.append(full)
    return scopes or [repo_dir]


# `smart_search` -> `ContractGraph::search_symbols` only matches declared
# symbol *names* (crates/mesh-core/src/contracts.rs, contains_ignore_ascii_case
# over node.name) — it never greps raw file/comment text. So the reload probe
# below has to append a real, syntactically valid declaration per language,
# not a comment, or the marker can never be found regardless of how long we
# poll.
def append_symbol(path, marker):
    if path.endswith(".py"):
        snippet = f"\n\ndef {marker.lower()}():\n    pass\n"
    elif path.endswith(".go"):
        snippet = f"\n\nfunc {marker}() {{}}\n"
    elif path.endswith(".ts"):
        # `typescript.rs`'s extractor only turns `class_declaration` /
        # `interface_declaration` nodes into indexed symbols — a bare
        # top-level `export function` is never captured, so the reload probe
        # would poll forever if it used one.
        snippet = f"\n\nexport class {marker} {{}}\n"
    elif path.endswith(".java"):
        # A second, non-public top-level type is legal alongside the file's
        # existing `public class HandlerN`.
        snippet = f"\n\nclass {marker} {{\n    static void run() {{}}\n}}\n"
    else:  # .rs
        snippet = f"\n\npub fn {marker.lower()}() {{}}\n"
    with open(path, "a") as f:
        f.write(snippet)


# `MarkdownFormatter::format_search_results` (crates/mesh-parsers/src/markdown.rs)
# always echoes the query into the response header — "## Search Results for
# `<query>` ... *Matches: N definitions found*" — even on zero matches. So a
# naive `marker in payload` substring check is a false positive on the very
# first poll, before the file watcher has done anything. Require the reported
# match count to be > 0 instead.
_MATCH_COUNT_RE = re.compile(r"Matches:\s*(\d+)\s*definitions found")


def has_real_match(payload_text):
    m = _MATCH_COUNT_RE.search(payload_text)
    return bool(m) and int(m.group(1)) > 0


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

        scopes = discover_scopes(args.repo_dir, args.config_path)
        result["scopes_used"] = len(scopes)

        queries = ["Service", "Handler", "Worker", "Component", "handle", "new"]
        latencies = []
        max_rss = result["rss_after_boot_mb"] or 0
        rss_samples_ok = 1 if result["rss_after_boot_mb"] is not None else 0
        rss_samples_total = 1
        for i in range(args.queries):
            q = queries[i % len(queries)]
            scope = scopes[i % len(scopes)]
            resp, ms = call(
                proc, "tools/call",
                {"name": "smart_search", "arguments": {"query": q, "scope": scope, "fuzzy": False}},
                100 + i,
            )
            if resp.get("error") or resp.get("result", {}).get("isError"):
                raise RuntimeError(f"smart_search({q!r}) failed: {resp.get('error') or resp['result']}")
            latencies.append(ms)
            rss_samples_total += 1
            sample = rss_mb(proc.pid)
            if sample is not None:
                rss_samples_ok += 1
                max_rss = max(max_rss, sample)

        result["search_p50_ms"] = round(percentile(latencies, 50), 1)
        result["search_p95_ms"] = round(percentile(latencies, 95), 1)
        # `ps` failing on every single sample (missing binary, sandboxed CI
        # image, PID reuse) must not silently read as "0 MB, under budget" —
        # that's a broken measurement, not a good one. Only report a peak (and
        # let the budget check run) if at least one sample actually worked.
        rss_measurement_failed = rss_samples_ok == 0
        result["rss_peak_mb"] = round(max_rss, 1) if rss_samples_ok > 0 else None

        # Incremental reload: append a uniquely-named symbol to a generated
        # file, then poll smart_search until it's visible (FileWatcherService
        # -> WorkspaceIndexer::reload_paths, debounced). Scope must be the
        # specific root the target file lives under (see discover_scopes).
        reload_scope = scopes[0]
        target = find_a_generated_file(reload_scope)
        reload_ms = None
        if target:
            marker = f"ScaleBenchMarker{int(time.time() * 1000)}"
            append_symbol(target, marker)
            reload_t0 = time.monotonic()
            deadline = reload_t0 + 20
            req_id = 900
            found = False
            while time.monotonic() < deadline:
                resp, _ms = call(
                    proc, "tools/call",
                    {"name": "smart_search", "arguments": {"query": marker, "scope": reload_scope, "fuzzy": False}},
                    req_id,
                )
                req_id += 1
                payload = json.dumps(resp.get("result", {}))
                if has_real_match(payload):
                    found = True
                    break
                time.sleep(0.2)
            if found:
                reload_ms = round((time.monotonic() - reload_t0) * 1000, 1)
        result["reload_ms"] = reload_ms

        violations = []
        if rss_measurement_failed:
            violations.append(f"rss_peak_mb=unmeasurable ({rss_samples_total} `ps` samples all failed)")
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
