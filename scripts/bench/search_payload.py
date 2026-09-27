#!/usr/bin/env python3
"""scripts/bench/search_payload.py — measures the real Markdown size of one
`smart_search` page (plan 4 step 4.13, point 2).

For each golden corpus (scripts/golden/fetch.sh, default ~/.cache/mesh-golden)
it starts `mesh-mcp run --standalone` with an isolated HOME and a short,
private MESH_SOCKET_PATH (never the user's meshd), waits for the index, and
calls `smart_search` once per query with the given `limit` (omitted when 0,
i.e. the server default). It prints one row per (corpus, query): the page's
byte size, the number of entries shown, the total match count, and the size
of the largest single entry (the not-indexed note of step 4.1 is reported
separately and excluded from entry sizes).

usage:
    MESH_MCP_BIN=target/release/mesh-mcp scripts/bench/search_payload.py \
        [--limit 20] [--corpus-dir ~/.cache/mesh-golden] [QUERY ...]
"""
import argparse
import json
import os
import re
import subprocess
import tempfile
import time
from pathlib import Path

CORPORA = ["online-boutique", "bank-of-anthos", "otel-demo"]
DEFAULT_QUERIES = ["Service", "Request", "Handler", "Client", "get"]
ENTRY_RE = re.compile(r"^### \[\d+\] ", re.M)


def rpc(proc, req_id, method, params):
    proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": req_id, "method": method, "params": params}) + "\n")
    proc.stdin.flush()
    while True:
        line = proc.stdout.readline()
        if not line:
            raise EOFError("server closed stdout")
        msg = json.loads(line)
        if msg.get("id") == req_id:
            return msg


def measure(binary, repo, queries, limit):
    rows = []
    with tempfile.TemporaryDirectory() as home:
        cfg = Path(home) / "mesh-mcp.toml"
        cfg.write_text(f'[workspace]\nname = "bench"\nversion = "0"\nroots = ["{repo}"]\n')
        env = dict(os.environ, HOME=home, MESH_SOCKET_PATH=f"/tmp/mesh-pl-{os.getpid()}.sock")
        env.pop("XDG_RUNTIME_DIR", None)
        proc = subprocess.Popen(
            [binary, "--config", str(cfg), "run", "--standalone"],
            cwd=repo, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, text=True, env=env,
        )
        try:
            rpc(proc, 0, "initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                        "clientInfo": {"name": "payload-bench", "version": "0"}})
            proc.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
            proc.stdin.flush()
            for i, q in enumerate(queries, start=1):
                args = {"query": q, "scope": str(repo)}
                if limit:
                    args["limit"] = limit
                # The index is built in the background: retry until it answers.
                for _ in range(120):
                    resp = rpc(proc, i, "tools/call", {"name": "smart_search", "arguments": args})
                    text = "".join(c.get("text", "") for c in resp.get("result", {}).get("content", []))
                    busy = "indexing" in json.dumps(resp.get("error", "")).lower()
                    if not busy:
                        break
                    time.sleep(0.5)
                # Entries end where the footer ("More results"/"Tip") starts;
                # the 4.1 not-indexed note, if any, follows the footer.
                raw = text.encode()
                note_at = raw.find(b"\n> [!WARNING]")
                note = len(raw) - note_at if note_at >= 0 else 0
                starts = [m.start() for m in ENTRY_RE.finditer(text)]
                foot = min((i for i in (text.find("*More results"), text.find("*Tip: Use")) if i >= 0),
                           default=len(text))
                bounds = starts + [foot]
                entries = [len(text[bounds[k]:bounds[k + 1]].encode()) for k in range(len(starts))]
                total = re.search(r"Matches: (≥?\d+)", text)
                rows.append((repo.name, q, len(raw), note, len(entries), total.group(1) if total else "0",
                             sum(entries), max(entries, default=0)))
        finally:
            proc.stdin.close()
            proc.wait(timeout=30)
    return rows


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--limit", type=int, default=20)
    ap.add_argument("--corpus-dir", default=str(Path.home() / ".cache" / "mesh-golden"))
    ap.add_argument("queries", nargs="*", default=DEFAULT_QUERIES)
    a = ap.parse_args()
    binary = str(Path(os.environ.get("MESH_MCP_BIN", "target/release/mesh-mcp")).resolve())
    print("| corpus | query | page bytes | of which not-indexed note | entries | total matches | entry bytes | largest entry |")
    print("|---|---|---:|---:|---:|---:|---:|---:|")
    for name in CORPORA:
        repo = Path(a.corpus_dir) / name
        if not repo.is_dir():
            continue
        for row in measure(binary, repo, a.queries, a.limit):
            print("| {} | `{}` | {} | {} | {} | {} | {} | {} |".format(*row))


if __name__ == "__main__":
    main()
