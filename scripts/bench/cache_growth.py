#!/usr/bin/env python3
"""Measures persistent index-cache growth over one simulated work week (plan 4, step 4.4).

The week is 30 branch switches and 10 rebases on a copy of a ~5k-file corpus, with one
cold boot (`mesh-mcp run --standalone`, stdin closed, so the process indexes and exits)
after each event. A cold boot per event is the worst case for cache growth: every
content version the working tree ever shows is parsed once and written to the cache. A
long-lived daemon that reloads instead of restarting writes at most the same entries.

Everything runs in a fresh temp directory: the corpus is copied OUT of any git-ignored
path (the watcher ignores changes under `target/`), `HOME` points into the temp dir so
the cache never touches the real `~/.cache/mesh-mcp`, and `MESH_SOCKET_PATH` is a short,
private path so the user's own `meshd` is never contacted.

Usage:
  cache_growth.py <mesh-mcp binary> [--corpus DIR] [--seed N] [--json OUT]
                  [--max-size-mb N]

`--max-size-mb` writes a `[cache] max_size_mb` section into the copied config (only
builds that know the key accept it).

Size is measured as db + `-wal` file bytes (no checkpoint forced before measuring, so
whatever the process left in the WAL is counted). Requires `git` and `sqlite3` on PATH.
"""
import argparse
import glob
import json
import os
import random
import shutil
import subprocess
import sys
import tempfile
import time

CODE_EXT = {".rs": "//", ".go": "//", ".ts": "//", ".java": "//", ".py": "#", ".proto": "//"}
FEATURE_BRANCHES = 6
FILES_PER_FEATURE = 50
FILES_PER_UPSTREAM_COMMIT = 100
DAYS = 5
SWITCHES_PER_DAY = 6
REBASES_PER_DAY = 2


def run(cmd, cwd, env=None, stdin=None):
    return subprocess.run(
        cmd, cwd=cwd, env=env, stdin=stdin, check=True,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )


def git(ws, *args):
    return run(["git", "-c", "user.name=bench", "-c", "user.email=bench@example.invalid",
                "-c", "commit.gpgsign=false", *args], ws).stdout


def edit(ws, rel, tag):
    path = os.path.join(ws, rel)
    marker = CODE_EXT[os.path.splitext(rel)[1]]
    with open(path, "a") as f:
        f.write(f"\n{marker} edit {tag}\n")


def cache_db(home):
    found = sorted(glob.glob(os.path.join(home, ".cache", "mesh-mcp", "**", "index-cache.db"),
                             recursive=True))
    return found


def measure(home):
    dbs = cache_db(home)
    total = {"db_files": len(dbs), "db_bytes": 0, "wal_bytes": 0, "rows": 0, "payload_bytes": 0}
    for db in dbs:
        total["db_bytes"] += os.path.getsize(db)
        wal = db + "-wal"
        if os.path.exists(wal):
            total["wal_bytes"] += os.path.getsize(wal)
        out = run(["sqlite3", "-readonly", db,
                   "SELECT count(*), coalesce(sum(length(payload)),0) FROM file_index_cache;"],
                  os.path.dirname(db)).stdout.strip()
        rows, payload = out.split("|")
        total["rows"] += int(rows)
        total["payload_bytes"] += int(payload)
    total["total_bytes"] = total["db_bytes"] + total["wal_bytes"]
    return total


def boot(binary, ws, home, sock):
    env = dict(os.environ, HOME=home, MESH_SOCKET_PATH=sock, RUST_LOG="warn")
    start = time.monotonic()
    with open(os.devnull) as devnull:
        run([binary, "--config", ".agents/mesh-mcp.toml", "run", "--standalone"], ws, env, devnull)
    return round((time.monotonic() - start) * 1000)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("binary")
    ap.add_argument("--corpus", default=os.path.expanduser("~/bench-repos/mesh-synth-5k"))
    ap.add_argument("--seed", type=int, default=44)
    ap.add_argument("--json")
    ap.add_argument("--max-size-mb", type=int)
    args = ap.parse_args()
    binary = os.path.abspath(args.binary)

    work = tempfile.mkdtemp(prefix="m44-week.", dir="/tmp")
    ws = os.path.join(work, "ws")
    home = os.path.join(work, "home")
    sock = os.path.join("/tmp", f"m44g-{os.getpid()}.sock")
    os.makedirs(home)
    shutil.copytree(args.corpus, ws, ignore=shutil.ignore_patterns(".git"))
    if args.max_size_mb is not None:
        with open(os.path.join(ws, ".agents", "mesh-mcp.toml"), "a") as f:
            f.write(f"\n[cache]\nmax_size_mb = {args.max_size_mb}\n")

    git(ws, "init", "-q", "-b", "main")
    git(ws, "add", "-A")
    git(ws, "commit", "-q", "-m", "base")
    files = sorted(
        p for p in git(ws, "ls-files").splitlines() if os.path.splitext(p)[1] in CODE_EXT
    )
    rng = random.Random(args.seed)
    rng.shuffle(files)
    # Disjoint pools: feature branches and upstream commits never edit the same file, so
    # every rebase applies cleanly and is deterministic.
    half = len(files) // 2
    feature_pool, upstream_pool = files[:half], files[half:]

    branches = []
    for b in range(FEATURE_BRANCHES):
        name = f"feature-{b}"
        git(ws, "checkout", "-q", "-b", name, "main")
        for rel in rng.sample(feature_pool, FILES_PER_FEATURE):
            edit(ws, rel, f"{name}-0")
        git(ws, "commit", "-q", "-am", f"{name} work")
        branches.append(name)
    git(ws, "checkout", "-q", "main")

    rows = []
    t0 = boot(binary, ws, home, sock)
    rows.append({"event": 0, "kind": "initial boot", "branch": "main", "boot_ms": t0, **measure(home)})
    print(json.dumps(rows[-1]), file=sys.stderr)

    event = 0
    upstream = 0
    current = "main"
    for day in range(DAYS):
        kinds = ["switch"] * SWITCHES_PER_DAY + ["rebase"] * REBASES_PER_DAY
        rng.shuffle(kinds)
        for kind in kinds:
            event += 1
            if kind == "switch" or current == "main":
                # A rebase while on main degenerates into a switch followed by one,
                # counted as the rebase; a plain switch always changes branch.
                current = rng.choice([b for b in branches + ["main"] if b != current])
                git(ws, "checkout", "-q", current)
            if kind == "rebase":
                if current == "main":
                    current = rng.choice(branches)
                    git(ws, "checkout", "-q", current)
                upstream += 1
                git(ws, "checkout", "-q", "main")
                for rel in rng.sample(upstream_pool, FILES_PER_UPSTREAM_COMMIT):
                    edit(ws, rel, f"upstream-{upstream}")
                git(ws, "commit", "-q", "-am", f"upstream {upstream}")
                git(ws, "checkout", "-q", current)
                git(ws, "rebase", "-q", "main")
            ms = boot(binary, ws, home, sock)
            rows.append({"event": event, "kind": kind, "branch": current, "day": day + 1,
                         "boot_ms": ms, **measure(home)})
            print(json.dumps(rows[-1]), file=sys.stderr)

    first, last = rows[0], rows[-1]
    summary = {
        "binary": binary,
        "corpus": args.corpus,
        "code_files": len(files),
        "events": event,
        "initial_rows": first["rows"],
        "initial_total_bytes": first["total_bytes"],
        "initial_bytes_per_row": round(first["total_bytes"] / max(first["rows"], 1)),
        "initial_payload_bytes_per_row": round(first["payload_bytes"] / max(first["rows"], 1)),
        "final_rows": last["rows"],
        "final_total_bytes": last["total_bytes"],
        "final_bytes_per_row": round(last["total_bytes"] / max(last["rows"], 1)),
        "growth_bytes": last["total_bytes"] - first["total_bytes"],
        "growth_ratio": round(last["total_bytes"] / max(first["total_bytes"], 1), 2),
        "cache_files": last["db_files"],
    }
    out = {"summary": summary, "events": rows}
    text = json.dumps(out, indent=2)
    if args.json:
        with open(args.json, "w") as f:
            f.write(text + "\n")
    print(json.dumps(summary, indent=2))
    shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
