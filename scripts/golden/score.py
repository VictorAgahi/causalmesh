#!/usr/bin/env python3
"""scripts/golden/score.py — precision/recall of mesh-mcp's gRPC edge
extraction against a hand-written golden file (Plan 2 step 2.0).

Runs `mesh-mcp graph --format json` against a pinned golden-corpus repo
(scripts/golden/fetch.sh) and compares the resulting service-to-service gRPC
call edges against tests/golden/<repo>.expected.yaml's `grpc_edges`.

usage:
    scripts/golden/score.py <repo-name> [--fail-under-recall R] [--fail-under-precision P]

Exit code is 0 unless a --fail-under-* threshold is given and not met, so this
can gate CI (the "ratchet") without also being a hard requirement for
ad-hoc/local runs.
"""
import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
GOLDEN_CACHE_DIR = Path(os.environ.get("GOLDEN_CACHE_DIR", Path.home() / ".cache" / "mesh-golden"))
GOLDEN_DIR = REPO_ROOT / "tests" / "golden"

# Matches the service/component directory a file lives under, for repos laid
# out as <root>/src/<service>/... (every repo in the golden corpus so far).
SERVICE_DIR_RE = re.compile(r"(?:^|/)src/([^/]+)/")


def infer_service_from_path(file_path: str) -> str:
    m = SERVICE_DIR_RE.search(file_path)
    return m.group(1) if m else "unknown"


def run_mesh_mcp_graph(repo_dir: Path) -> dict:
    with tempfile.NamedTemporaryFile(suffix=".json", delete=False) as tmp:
        out_path = Path(tmp.name)
    subprocess.run(
        ["mesh-mcp", "graph", "--format", "json", "-o", str(out_path)],
        cwd=repo_dir,
        check=True,
        capture_output=True,
        text=True,
    )
    data = json.loads(out_path.read_text())
    out_path.unlink(missing_ok=True)
    return data


def extract_found_edges(graph: dict) -> set[tuple[str, str]]:
    nodes_by_id = {n["id"]: n for n in graph["nodes"]}
    found = set()
    for edge in graph["edges"]:
        if edge["kind"] not in ("CallsRpc", "Implements"):
            continue
        to_node = nodes_by_id.get(edge["to"])
        from_node = nodes_by_id.get(edge["from"])
        if to_node is None or from_node is None:
            continue

        if to_node["kind"] in ("GrpcService", "GrpcMethod"):
            callee = to_node["name"].split(".")[0]
        else:
            callee = infer_service_from_path(to_node["file_path"])

        caller = infer_service_from_path(from_node["file_path"])
        if caller == "unknown" or callee == "unknown" or caller.lower() == callee.lower():
            continue
        found.add((normalize_service_name(caller), normalize_service_name(callee)))
    return found


def normalize_service_name(name: str) -> str:
    """Directory names (`checkoutservice`) and proto service names
    (`CheckoutService`) name the same real service under different
    conventions; comparing them case- and separator-insensitively is what
    makes `caller == callee` self-edge filtering and the golden-file
    comparison meaningful across both naming styles.
    """
    return re.sub(r"[^a-z0-9]", "", name.lower())


FLOW_MAP_RE = re.compile(r"\{from:\s*([\w.-]+),\s*to:\s*([\w.-]+)\}")


def load_golden_edges(repo_name: str) -> set[tuple[str, str]]:
    """Extracts `grpc_edges`' `{from: X, to: Y}` entries directly, without a
    full YAML parser — the golden files' shape is fixed and entirely under
    this project's own control, so a real YAML library would be a dependency
    added for one flow-style list this project itself authors, not for
    parsing arbitrary third-party YAML.
    """
    golden_path = GOLDEN_DIR / f"{repo_name}.expected.yaml"
    if not golden_path.exists():
        print(f"error: no golden file at {golden_path}", file=sys.stderr)
        sys.exit(2)
    text = golden_path.read_text()
    in_grpc_edges = False
    edges = set()
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("#"):
            continue
        if stripped == "grpc_edges:":
            in_grpc_edges = True
            continue
        if in_grpc_edges:
            if stripped.startswith("- "):
                m = FLOW_MAP_RE.search(stripped)
                if m:
                    edges.add(
                        (
                            normalize_service_name(m.group(1)),
                            normalize_service_name(m.group(2)),
                        )
                    )
                continue
            if stripped and not stripped.startswith("-"):
                # Any other top-level key ends the grpc_edges list.
                in_grpc_edges = False
    return edges


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repo", help="golden-corpus repo name, e.g. online-boutique")
    parser.add_argument("--fail-under-recall", type=float, default=None)
    parser.add_argument("--fail-under-precision", type=float, default=None)
    args = parser.parse_args()

    repo_dir = GOLDEN_CACHE_DIR / args.repo
    if not repo_dir.exists():
        print(f"error: {repo_dir} not found — run scripts/golden/fetch.sh first", file=sys.stderr)
        return 2

    golden = load_golden_edges(args.repo)
    graph = run_mesh_mcp_graph(repo_dir)
    found = extract_found_edges(graph)

    true_positives = found & golden
    missing = golden - found
    extra = found - golden

    precision = len(true_positives) / len(found) if found else 1.0
    recall = len(true_positives) / len(golden) if golden else 1.0

    print(f"=== {args.repo}: gRPC edge precision/recall ===")
    print(f"golden edges:  {len(golden)}")
    print(f"found edges:   {len(found)}")
    print(f"true positives: {len(true_positives)}")
    print(f"precision: {precision:.1%}")
    print(f"recall:    {recall:.1%}")
    if missing:
        print(f"\nmissing ({len(missing)}):")
        for c, t in sorted(missing):
            print(f"  {c} -> {t}")
    if extra:
        print(f"\nextra / unexpected ({len(extra)}):")
        for c, t in sorted(extra):
            print(f"  {c} -> {t}")

    ok = True
    if args.fail_under_recall is not None and recall < args.fail_under_recall:
        print(f"\nFAIL: recall {recall:.1%} < required {args.fail_under_recall:.1%}")
        ok = False
    if args.fail_under_precision is not None and precision < args.fail_under_precision:
        print(f"\nFAIL: precision {precision:.1%} < required {args.fail_under_precision:.1%}")
        ok = False
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
