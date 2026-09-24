#!/usr/bin/env python3
"""Aggregate a bench.sh results dir into a markdown report."""
import glob
import json
import os
import sys


def main():
    if len(sys.argv) != 2:
        print("usage: report.py <results_dir>", file=sys.stderr)
        sys.exit(2)
    results_dir = sys.argv[1]

    rows = []
    for path in sorted(glob.glob(os.path.join(results_dir, "*.functional.json"))):
        name = os.path.basename(path).replace(".functional.json", "")
        try:
            data = json.load(open(path))
        except Exception as e:
            rows.append({"repo": name, "ok": False, "error": f"unreadable result: {e}"})
            continue
        rows.append(data)

    print("# MeshMCP Benchmark Report\n")
    print("| repo | status | initialize (ms) | tools/list (ms) | smart_search (ms) | response bytes | error |")
    print("|---|---|---|---|---|---|---|")
    for r in rows:
        steps = r.get("steps", {})
        init_ms = steps.get("initialize", {}).get("ms", "-")
        list_ms = steps.get("tools/list", {}).get("ms", "-")
        search = steps.get("smart_search", {})
        search_ms = search.get("ms", "-")
        search_bytes = search.get("response_bytes", "-")
        status = "PASS" if r.get("ok") else "FAIL"
        err = r.get("error", "").replace("|", "\\|")[:120]
        print(f"| {r.get('repo', '?')} | {status} | {init_ms} | {list_ms} | {search_ms} | {search_bytes} | {err} |")

    n_pass = sum(1 for r in rows if r.get("ok"))
    print(f"\n**{n_pass}/{len(rows)} repos passed the functional MCP smoke test.**")


if __name__ == "__main__":
    main()
