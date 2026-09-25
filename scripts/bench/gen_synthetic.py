#!/usr/bin/env python3
"""Deterministic synthetic workspace generator for MeshMCP scale benchmarks.

Real large repos (linux, kubernetes, vscode) are useful but slow to clone and
non-reproducible across runs (upstream commits move). This generator produces
a workspace of a chosen file count with a fixed seed, so scale_bench.py can
measure boot/reload/RSS/latency at a known, repeatable N without network
access — the CI nightly job runs this instead of a full clone.

Usage:
  gen_synthetic.py <out_dir> <file_count> [--seed N] [--services N]
"""
import argparse
import os
import random

LANG_EXT = {
    "rust": "rs",
    "go": "go",
    "python": "py",
    "typescript": "ts",
    "java": "java",
}

RUST_TEMPLATE = """// generated
pub struct Service{idx} {{
    pub name: String,
}}

impl Service{idx} {{
    pub fn new(name: String) -> Self {{
        Self {{ name }}
    }}

    pub fn handle_{idx}(&self, input: &str) -> String {{
        format!("{{}}::{{}}", self.name, input)
    }}
}}
"""

GO_TEMPLATE = """package pkg{idx}

type Handler{idx} struct {{
    Name string
}}

func NewHandler{idx}(name string) *Handler{idx} {{
    return &Handler{idx}{{Name: name}}
}}

func (h *Handler{idx}) Handle(input string) string {{
    return h.Name + "::" + input
}}
"""

PY_TEMPLATE = """# generated


class Worker{idx}:
    def __init__(self, name):
        self.name = name

    def handle(self, payload):
        return f"{{self.name}}::{{payload}}"
"""

TS_TEMPLATE = """// generated
export class Component{idx} {{
  constructor(private name: string) {{}}

  handle(input: string): string {{
    return `${{this.name}}::${{input}}`;
  }}
}}
"""

JAVA_TEMPLATE = """// generated
package pkg{idx};

public class Handler{idx} {{
    private final String name;

    public Handler{idx}(String name) {{
        this.name = name;
    }}

    public String handle(String input) {{
        return name + "::" + input;
    }}
}}
"""

TEMPLATES = {
    "rust": RUST_TEMPLATE,
    "go": GO_TEMPLATE,
    "python": PY_TEMPLATE,
    "typescript": TS_TEMPLATE,
    "java": JAVA_TEMPLATE,
}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out_dir")
    ap.add_argument("file_count", type=int)
    ap.add_argument("--seed", type=int, default=42)
    ap.add_argument("--services", type=int, default=8, help="top-level service/ dirs, to give init --auto real roots to find")
    args = ap.parse_args()

    rng = random.Random(args.seed)
    os.makedirs(args.out_dir, exist_ok=True)

    langs = list(TEMPLATES.keys())
    per_service = max(1, args.file_count // args.services)

    idx = 0
    for svc in range(args.services):
        svc_dir = os.path.join(args.out_dir, f"service-{svc}", "src")
        os.makedirs(svc_dir, exist_ok=True)
        for _ in range(per_service):
            lang = langs[idx % len(langs)]
            ext = LANG_EXT[lang]
            fname = os.path.join(svc_dir, f"gen_{idx}.{ext}")
            with open(fname, "w") as f:
                f.write(TEMPLATES[lang].format(idx=idx))
            idx += 1
        # one Cargo.toml / package.json style marker per service so
        # `init --auto` recognizes each service dir as its own root.
        with open(os.path.join(args.out_dir, f"service-{svc}", "marker.json"), "w") as f:
            f.write('{"synthetic": true}\n')

    print(f"generated {idx} files across {args.services} services under {args.out_dir}")


if __name__ == "__main__":
    main()
