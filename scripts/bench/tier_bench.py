#!/usr/bin/env python3
"""Size-tier benchmark (Plan 4 step 4.3): N cold runs of scale_bench.py, medians,
and a check against (or a proposal for) a tier budget file.

A single scale_bench.py run is one sample; tier budgets are defined on the
median of several *cold* runs (fresh HOME per run, so the persistent parse
cache under ~/.cache/mesh-mcp starts empty), which is also what this compares
against, so a budget and its check use the same statistic.

Synthetic tier (corpus generated if missing, `init --auto` config inside it):
  tier_bench.py --files 50000 [--contracts] [--corpus DIR] [--runs 3]
                [--budget-json scripts/bench/budgets-50k.json] [--propose]

Real repository, read-only (config written outside it, reload probe off):
  tier_bench.py --repo ~/bench-repos/kubernetes --workdir /tmp/mesh-real [--runs 3]

--wait-calm S waits (up to S seconds per run) until no `rustc` runs and the
1-minute load average is below --calm-load, and records whether it got there:
budgets measured under contention are wider than the code deserves.

Budget files: {"platforms": {"darwin"|"linux": {"provisional": bool,
"plain"|"contracts": {metric: limit}}}}. A platform/variant with no budget is
a failure when --budget-json is given (an uncalibrated tier must not pass).
Exit 0 iff every median is within budget (or no budget file was given).
"""
import argparse
import json
import math
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
METRICS = ["boot_ms", "rss_peak_mb", "search_p50_ms", "search_p95_ms", "reload_ms"]
BUDGET_FACTOR = 1.3


def loadavg1():
    return round(os.getloadavg()[0], 2)


def rustc_running():
    try:
        return subprocess.run(["pgrep", "-x", "rustc"], stdout=subprocess.DEVNULL).returncode == 0
    except FileNotFoundError:
        return False


def wait_calm(max_wait_s, calm_load):
    """True once calm; False if still contended after max_wait_s."""
    t0 = time.monotonic()
    while True:
        if not rustc_running() and loadavg1() < calm_load:
            return True
        if time.monotonic() - t0 >= max_wait_s:
            return False
        time.sleep(30)


def mesh_bin():
    return os.environ.get(
        "MESH_MCP_BIN", os.path.join(HERE, "..", "..", "target", "release", "mesh-mcp")
    )


def prepare_synthetic(args):
    corpus = args.corpus or f"/tmp/mesh-tier-{args.files}{'-contracts' if args.contracts else ''}"
    if not os.path.isdir(os.path.join(corpus, "services")):
        cmd = [sys.executable, os.path.join(HERE, "gen_synthetic.py"), corpus, str(args.files), "--services", str(args.services)]
        if args.contracts:
            cmd.append("--contracts")
        subprocess.run(cmd, check=True, stdout=sys.stderr)
    config = os.path.join(corpus, ".agents", "mesh-mcp.toml")
    if not os.path.isfile(config):
        home = tempfile.mkdtemp(prefix="mtb-init-")
        try:
            env = dict(os.environ, HOME=home)
            subprocess.run([mesh_bin(), "init", "--auto", "--force"], cwd=corpus, env=env, check=True,
                           stdout=sys.stderr, stderr=sys.stderr)
        finally:
            shutil.rmtree(home, ignore_errors=True)
    return corpus, config, False


def prepare_real(args):
    repo = os.path.realpath(os.path.expanduser(args.repo))
    name = os.path.basename(repo)
    cfg_dir = os.path.join(args.workdir, name, ".agents")
    os.makedirs(cfg_dir, exist_ok=True)
    config = os.path.join(cfg_dir, "mesh-mcp.toml")
    # One absolute root = the whole repository; nothing is written inside it.
    with open(config, "w") as f:
        f.write(
            "[workspace]\n"
            f'name = "{name}"\n'
            f'workspace_root = "{repo}"\n'
            f'roots = ["{repo}"]\n\n'
            "[engines.docs]\nenabled = true\n"
            f'paths = ["{repo}/*.md"]\n\n'
            "[engines.contracts]\nenabled = true\n"
        )
    return repo, config, True


def one_run(i, repo_dir, config, no_reload, queries):
    home = tempfile.mkdtemp(prefix="mtb-")  # fresh HOME: cold parse cache
    env = dict(os.environ, HOME=home, MESH_SOCKET_PATH=os.path.join(home, "s.sock"),
               MESH_BENCH_TIMEOUT=os.environ.get("MESH_BENCH_TIMEOUT", "600"))
    env["MESH_MCP_BIN"] = mesh_bin()
    cmd = [sys.executable, os.path.join(HERE, "scale_bench.py"), repo_dir, config,
           "--queries", str(queries), "--no-budget"]
    if no_reload:
        cmd.append("--no-reload")
    load_start = loadavg1()
    t0 = time.monotonic()
    try:
        out = subprocess.run(cmd, env=env, stdout=subprocess.PIPE, text=True).stdout
    finally:
        shutil.rmtree(home, ignore_errors=True)
    wall = round(time.monotonic() - t0, 1)
    line = out.strip().splitlines()[-1] if out.strip() else "{}"
    r = json.loads(line)
    r.update({"run": i, "load1_start": load_start, "load1_end": loadavg1(), "wall_s": wall})
    return r


def main():
    ap = argparse.ArgumentParser()
    src = ap.add_mutually_exclusive_group(required=True)
    src.add_argument("--files", type=int)
    src.add_argument("--repo")
    ap.add_argument("--contracts", action="store_true")
    ap.add_argument("--services", type=int, default=8)
    ap.add_argument("--corpus")
    ap.add_argument("--workdir", default="/tmp/mesh-real")
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--queries", type=int, default=30)
    ap.add_argument("--budget-json")
    ap.add_argument("--propose", action="store_true", help=f"print budgets = ceil({BUDGET_FACTOR} x median)")
    ap.add_argument("--wait-calm", type=float, default=0, help="max seconds to wait for a calm machine before each run")
    ap.add_argument("--calm-load", type=float, default=4.0)
    ap.add_argument("--out")
    args = ap.parse_args()

    if args.files:
        repo_dir, config, real = prepare_synthetic(args)
    else:
        repo_dir, config, real = prepare_real(args)
    variant = "contracts" if args.contracts else "plain"
    platform = "darwin" if sys.platform == "darwin" else "linux"

    runs, calm_all = [], True
    for i in range(1, args.runs + 1):
        calm = wait_calm(args.wait_calm, args.calm_load) if args.wait_calm > 0 else None
        calm_all = calm_all and calm is not False
        r = one_run(i, repo_dir, config, real, args.queries)
        r["calm_at_start"] = calm
        runs.append(r)
        print(f"run {i}: " + json.dumps({k: r.get(k) for k in METRICS + ["files_indexed", "load1_start", "load1_end", "wall_s", "error"]}),
              file=sys.stderr)

    failed = [r for r in runs if r.get("error") or r.get("violations")]
    medians = {}
    for m in METRICS:
        vals = [r[m] for r in runs if r.get(m) is not None]
        medians[m] = round(statistics.median(vals), 1) if vals else None
    result = {
        "corpus": repo_dir, "variant": variant, "platform": platform, "real_repo": real,
        "runs": [{k: r.get(k) for k in METRICS + ["load1_start", "load1_end", "calm_at_start", "wall_s", "rss_peak_source", "scopes_used", "files_indexed", "contract_nodes", "error", "violations"]} for r in runs],
        "medians": medians, "calm": calm_all if args.wait_calm > 0 else None,
    }
    violations = [f"run {r['run']}: {r.get('error') or r.get('violations')}" for r in failed]

    if args.propose:
        result["proposed_budgets"] = {m: math.ceil(v * BUDGET_FACTOR) for m, v in medians.items() if v is not None}

    if args.budget_json:
        with open(args.budget_json) as f:
            doc = json.load(f)
        section = doc.get("platforms", {}).get(platform, {})
        budget = section.get(variant)
        if not budget:
            violations.append(f"no {platform}/{variant} budget in {args.budget_json} (uncalibrated)")
        else:
            result["budgets"] = budget
            result["provisional"] = bool(section.get("provisional"))
            for m, limit in budget.items():
                v = medians.get(m)
                if v is None:
                    violations.append(f"{m}: no measurement (budget {limit})")
                elif v > limit:
                    violations.append(f"median {m}={v} exceeds budget {limit}")

    result["violations"] = violations
    result["ok"] = not violations
    text = json.dumps(result, indent=1)
    if args.out:
        with open(args.out, "w") as f:
            f.write(text + "\n")
    print(text)
    sys.exit(0 if result["ok"] else 1)


if __name__ == "__main__":
    main()
