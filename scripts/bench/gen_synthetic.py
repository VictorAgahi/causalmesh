#!/usr/bin/env python3
"""Deterministic synthetic workspace generator for MeshMCP scale benchmarks.

Real large repos (linux, kubernetes, vscode) are useful but slow to clone and
non-reproducible across runs (upstream commits move). This generator produces
a workspace of a chosen file count with a fixed seed, so scale_bench.py can
measure boot/reload/RSS/latency at a known, repeatable N without network
access — the CI nightly job runs this instead of a full clone.

Usage:
  gen_synthetic.py <out_dir> <file_count> [--seed N] [--services N] [--contracts]

--contracts adds a contract mix that exercises `ContractGraph::reconcile_edges`
(derive) and the YAML/Markdown ingestion paths, which plain code files never
touch: every Java file imports a hot shared name (`com.acme.commonK.Shared`,
`Shared` declared in many packages; K varies, so many *distinct* import targets
land in the one `Shared` bucket), every TypeScript file gains a `@GrpcMethod`
handler with a matching `.proto` service, and every 100th file index adds a
Spring-style `application.yml` and a Markdown doc. Extra files are *in
addition to* `file_count` and are reported separately.
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

TS_GRPC_TEMPLATE = """// generated
export class Component{idx} {{
  constructor(private name: string) {{}}

  @GrpcMethod('Svc{idx}', 'Get{idx}')
  get{idx}(data: any): any {{
    return data;
  }}
}}
"""

JAVA_IMPORT_TEMPLATE = JAVA_TEMPLATE.replace(
    "package pkg{idx};\n", "package pkg{idx};\n\nimport com.acme.common{shared}.Shared;\n"
)

PROTO_TEMPLATE = """syntax = "proto3";
package svc{idx};

service Svc{idx} {{
  rpc Get{idx} (Req{idx}) returns (Res{idx});
}}

message Req{idx} {{ string id = 1; }}
message Res{idx} {{ string value = 1; }}
"""

SHARED_TEMPLATE = """package com.acme.common{suffix};

public class Shared {{
    public String id() {{ return "{suffix}"; }}
}}
"""

YAML_TEMPLATE = """server:
  port: {port}
spring:
  application:
    name: svc-{idx}
  datasource:
    url: jdbc:postgresql://db-{idx}:5432/app
    password: secret-{idx}
app:
  features: [a, b, c]
  ratio: 0.{idx}
"""

MD_TEMPLATE = """# Service {idx}

Handles `Svc{idx}.Get{idx}` requests.

## Configuration

Reads `spring.datasource.url` and publishes to topic orders-{idx}.
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
    ap.add_argument("--services", type=int, default=8, help="services/svc-N/ dirs, so init --auto discovers ./services/* as a real multi-root workspace")
    ap.add_argument("--contracts", action="store_true", help="add the derive/YAML/Markdown contract mix (see module docstring)")
    args = ap.parse_args()

    rng = random.Random(args.seed)
    os.makedirs(args.out_dir, exist_ok=True)

    langs = list(TEMPLATES.keys())
    per_service = max(1, args.file_count // args.services)

    idx = 0
    extras = 0

    def write(path, text):
        with open(path, "w") as f:
            f.write(text)
        return 1

    for svc in range(args.services):
        # `crates/mesh-server/src/cli/init.rs::run` only recognizes a
        # top-level `services/` directory (`cur_dir.join("services").exists()`
        # -> root `./services/*`); it does not scan for arbitrary
        # `service-N/` names or a `marker.json` convention, so the layout has
        # to be `services/svc-N/` for `init --auto` to actually discover each
        # one as its own root instead of silently falling back to `.`.
        svc_dir = os.path.join(args.out_dir, "services", f"svc-{svc}", "src")
        os.makedirs(svc_dir, exist_ok=True)
        for _ in range(per_service):
            lang = langs[idx % len(langs)]
            ext = LANG_EXT[lang]
            fname = os.path.join(svc_dir, f"gen_{idx}.{ext}")
            template = TEMPLATES[lang]
            if args.contracts and lang == "typescript":
                template = TS_GRPC_TEMPLATE
                extras += write(os.path.join(svc_dir, f"svc_{idx}.proto"), PROTO_TEMPLATE.format(idx=idx))
            elif args.contracts and lang == "java":
                template = JAVA_IMPORT_TEMPLATE
                if idx % 50 == 4:
                    # The hot bucket: `Shared` declared in many packages, one of
                    # them exactly `com.acme.common` (suffix "").
                    suffix = "" if idx == 4 else str(idx)
                    extras += write(os.path.join(svc_dir, f"shared_{idx}.java"), SHARED_TEMPLATE.format(suffix=suffix))
            if args.contracts and idx % 100 == 0:
                extras += write(os.path.join(svc_dir, f"application-{idx}.yml"), YAML_TEMPLATE.format(idx=idx, port=8000 + idx % 1000))
                extras += write(os.path.join(svc_dir, f"README_{idx}.md"), MD_TEMPLATE.format(idx=idx))
            with open(fname, "w") as f:
                # The `Shared` declared nearest below this file (see the java branch).
                shared_idx = idx // 50 * 50 + 4
                f.write(template.format(idx=idx, shared="" if shared_idx == 4 else shared_idx))
            idx += 1

    print(f"generated {idx} files (+{extras} contract-mix extras) across {args.services} services under {args.out_dir}/services/")


if __name__ == "__main__":
    main()
