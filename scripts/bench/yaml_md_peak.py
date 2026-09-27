#!/usr/bin/env python3
"""Plan 4.9: per-file peak memory of the YAML / Markdown parsers.

Two measurements per corpus file (see gen_yaml_md_corpus.py), both read from the
"peak memory footprint" line of macOS `/usr/bin/time -l` (not `ps`):

1. parser delta: `parse_peak <mode> <file>` minus `parse_peak read <file>`
   (crates/mesh-server/examples/parse_peak.rs: same read as the indexer, then
   only the parser under test);
2. process delta: `mesh-mcp graph --format fingerprint` run in a workspace
   holding only that file, minus the same command in an empty workspace (same
   default config, isolated HOME and MESH_SOCKET_PATH, never the user's meshd).

Each figure is the median of --runs runs (default 3). The 4.9 decision rule is
applied to the parser delta: ratio = delta / file size, threshold 4x.

Cases whose name starts with `laughs-wide` can grow without bound; they run
under a watchdog that kills the process once its RSS (polled with `ps`, used
only as a kill switch, never as the measurement) passes --cap-mb or after
--timeout seconds, and are then reported as `KILLED`.

Usage (from the repo root, after
`cargo build --release -p mesh-server --bins --example parse_peak`):
    python3 scripts/bench/yaml_md_peak.py [--runs 3] [--out target/bench-4.9]
Prints a Markdown table on stdout.
"""

import argparse
import os
import re
import statistics
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))

# case -> parse_peak modes the indexer runs on that file (process_file in
# crates/mesh-server/src/indexer.rs: every .yml/.yaml goes through the property
# flattener *and* the YAML contract extractor with the default config).
MODES = {
    "openapi-yaml": ["props", "spec"],
    "asyncapi-yaml": ["props", "spec"],
    "application-yml": ["props", "spec"],
    "md-many-sections": ["docs"],
    "md-one-section": ["docs"],
    "md-code-block": ["docs"],
    "laughs-nested": ["props", "spec"],
    "laughs-wide-props": ["props", "spec"],
    "laughs-wide-openapi": ["props", "spec"],
    "laughs-wide-openapi-8k": ["props", "spec"],
    "laughs-wide-openapi-16k": ["props", "spec"],
    "laughs-wide-openapi-32k": ["props", "spec"],
}

PEAK_RE = re.compile(r"^\s*(\d+)\s+peak memory footprint", re.M)


def run_timed(cmd, cwd, env, cap_mb, timeout):
    """Runs `cmd` under /usr/bin/time -l; returns peak bytes or 'KILLED'."""
    with tempfile.TemporaryFile() as err:
        proc = subprocess.Popen(
            ["/usr/bin/time", "-l"] + cmd,
            cwd=cwd,
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=err,
        )
        t0 = time.monotonic()
        killed = False
        while proc.poll() is None:
            if cap_mb is not None:
                rss_kb = child_rss_kb(proc.pid)
                if rss_kb > cap_mb * 1024 or time.monotonic() - t0 > timeout:
                    subprocess.run(["pkill", "-9", "-P", str(proc.pid)])
                    killed = True
            time.sleep(0.05)
        if killed:
            return "KILLED"
        err.seek(0)
        text = err.read().decode(errors="replace")
    m = PEAK_RE.search(text)
    if not m:
        sys.exit(f"no peak footprint in /usr/bin/time output of {cmd}:\n{text}")
    return int(m.group(1))


def child_rss_kb(time_pid):
    """RSS of /usr/bin/time's child (kill switch only)."""
    kids = subprocess.run(
        ["pgrep", "-P", str(time_pid)], capture_output=True, text=True
    ).stdout.split()
    if not kids:
        return 0
    out = subprocess.run(
        ["ps", "-o", "rss=", "-p", ",".join(kids)], capture_output=True, text=True
    ).stdout.split()
    return max((int(x) for x in out if x.isdigit()), default=0)


def median_peak(cmd, cwd, env, runs, cap_mb, timeout):
    peaks = [run_timed(cmd, cwd, env, cap_mb, timeout) for _ in range(runs)]
    if "KILLED" in peaks:
        return "KILLED"
    return int(statistics.median(peaks))


def mb(n):
    return f"{n / (1024 * 1024):.2f} MB"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--out", default=os.path.join(REPO, "target", "bench-4.9"))
    ap.add_argument("--target-dir", default=os.environ.get("CARGO_TARGET_DIR", os.path.join(REPO, "target")))
    ap.add_argument("--cap-mb", type=int, default=2048)
    ap.add_argument("--timeout", type=int, default=60)
    ap.add_argument("--cases", nargs="*", default=list(MODES))
    args = ap.parse_args()

    probe = os.path.join(args.target_dir, "release", "examples", "parse_peak")
    server = os.path.join(args.target_dir, "release", "mesh-mcp")
    for b in (probe, server):
        if not os.path.exists(b):
            sys.exit(f"missing {b}: cargo build --release -p mesh-server --bins --example parse_peak")

    corpus = os.path.join(args.out, "corpus")
    subprocess.run(
        [sys.executable, os.path.join(HERE, "gen_yaml_md_corpus.py"), corpus],
        check=True,
        stdout=subprocess.DEVNULL,
    )
    home = os.path.join(args.out, "home")
    os.makedirs(home, exist_ok=True)
    env = dict(os.environ, HOME=home, MESH_SOCKET_PATH="/tmp/m49.sock", RUST_LOG="error")

    empty = os.path.join(corpus, "empty")
    base_proc = median_peak([server, "graph", "--format", "fingerprint"], empty, env, args.runs, None, 0)

    print(f"runs per figure: {args.runs} (median); empty-workspace `graph --format fingerprint` peak: {mb(base_proc)}\n")
    print("| case | file | size | mode | parser delta | ratio | process delta | process ratio |")
    print("|---|---|---:|---|---:|---:|---:|---:|")
    for case in args.cases:
        case_dir = os.path.join(corpus, case)
        (name,) = os.listdir(case_dir)
        path = os.path.join(case_dir, name)
        size = os.path.getsize(path)
        wide = case.startswith("laughs-wide")
        cap = args.cap_mb if wide else None
        base = median_peak([probe, "read", path], case_dir, env, args.runs, None, 0)
        proc_peak = median_peak(
            [server, "graph", "--format", "fingerprint"], case_dir, env, args.runs, cap, args.timeout
        )
        if proc_peak == "KILLED":
            pdelta, pratio = f"KILLED (> {args.cap_mb} MB or > {args.timeout}s)", "-"
        else:
            pdelta, pratio = mb(proc_peak - base_proc), f"{(proc_peak - base_proc) / size:.1f}x"
        for mode in MODES[case]:
            peak = median_peak([probe, mode, path], case_dir, env, args.runs, cap, args.timeout)
            if peak == "KILLED":
                delta, ratio = f"KILLED (> {args.cap_mb} MB or > {args.timeout}s)", "-"
            else:
                delta, ratio = mb(peak - base), f"{(peak - base) / size:.1f}x"
            print(f"| {case} | {name} | {size} | {mode} | {delta} | {ratio} | {pdelta} | {pratio} |")
            sys.stdout.flush()


if __name__ == "__main__":
    main()
