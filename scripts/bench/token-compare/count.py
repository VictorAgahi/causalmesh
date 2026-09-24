#!/usr/bin/env python3
"""Counts tokens (cl100k_base, a standard proxy tokenizer -- not Claude's exact
tokenizer, but consistent across all three tools so the comparison is fair)
for every captured .txt output in out/, and prints a markdown table."""
import glob
import os
import sys

try:
    import tiktoken
except ImportError:
    sys.exit("tiktoken not installed for this interpreter")

enc = tiktoken.get_encoding("cl100k_base")

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
OUT_DIR = os.path.join(SCRIPT_DIR, "out")

REPOS = ["deno", "googleapis", "grpc", "kubernetes", "linux", "react", "rust-lang", "vscode"]
TOOLS = ["rg", "rtk", "mesh"]


def tokens_of(path):
    if not os.path.exists(path):
        return None
    with open(path, "r", errors="replace") as f:
        text = f.read()
    return len(enc.encode(text)), len(text)


def ms_of(path):
    if not os.path.exists(path):
        return None
    with open(path) as f:
        v = f.read().strip()
    try:
        return float(v)
    except ValueError:
        return None


def main():
    print("| repo | rg tokens | rg ms | rtk tokens | rtk ms | mesh-mcp tokens | mesh-mcp ms (cold) | mesh vs rg | mesh vs rtk |")
    print("|---|---|---|---|---|---|---|---|---|")
    for repo in REPOS:
        row = {}
        for tool in TOOLS:
            t = tokens_of(os.path.join(OUT_DIR, f"{repo}.{tool}.txt"))
            m = ms_of(os.path.join(OUT_DIR, f"{repo}.{tool}.ms"))
            row[tool] = (t[0] if t else None, m)
        rg_tok, rg_ms = row["rg"]
        rtk_tok, rtk_ms = row["rtk"]
        mesh_tok, mesh_ms = row["mesh"]

        def ratio(a, b):
            if a is None or b is None or b == 0:
                return "-"
            return f"{a / b:.2f}x"

        print(
            f"| {repo} | {rg_tok if rg_tok is not None else '-'} | {rg_ms if rg_ms is not None else '-'} "
            f"| {rtk_tok if rtk_tok is not None else '-'} | {rtk_ms if rtk_ms is not None else '-'} "
            f"| {mesh_tok if mesh_tok is not None else '-'} | {mesh_ms if mesh_ms is not None else '-'} "
            f"| {ratio(rg_tok, mesh_tok)} | {ratio(rtk_tok, mesh_tok)} |"
        )


if __name__ == "__main__":
    main()
