# Extraction quality: precision/recall baseline (Plan 2 step 2.0)

Plan 1 (P0) made every answer reproducible; it said nothing about whether the answer was
*correct*. Plan 2 (P1) is about the second question, and this file is where its baseline and
regression gate live.

## Methodology

- `scripts/golden/repos.txt` pins each golden-corpus repo to an exact commit.
  `scripts/golden/fetch.sh` clones/checks them out.
- `tests/golden/<repo>.expected.yaml` is a **hand-written** ground truth — read from the repo's
  own source (`.proto` service declarations, `grep` for the generated client's construction site
  in each caller), never derived from mesh-mcp's own output. A ground truth generated from the
  tool being measured cannot catch that tool's own mistakes.
- `scripts/golden/score.py <repo>` runs `mesh-mcp graph --format json` against the pinned repo,
  extracts service-to-service gRPC edges (grouping by the `src/<service>/` directory a node's
  file lives under, or by proto service name for `.proto`-declared nodes), and computes
  precision/recall against the golden file's `grpc_edges`.

## Baseline (2026-09-25, mesh-mcp 4.0.0)

| Repo | Golden edges | Precision | Recall | Notes |
| :--- | :---: | :---: | :---: | :--- |
| `online-boutique` | 14 | **100%** | **92.9%** | Original P0 baseline; superseded by step 2.1 below. |
| `otel-demo` | — | — | — | Golden file not yet written (pending). |
| `bank-of-anthos` | — | — | — | Golden file not yet written (pending); this repo is mostly HTTP/REST internally, not gRPC — its golden file should score `http_routes`, not `grpc_edges`, once step 2.6 lands a comparable extraction for those. |

**How to reproduce:**
```bash
scripts/golden/fetch.sh
mesh-mcp init --auto   # run once inside ~/.cache/mesh-golden/<repo>, or point --config at it
scripts/golden/score.py online-boutique
```

## Update (2026-09-25, step 2.1 — Python gRPC client stubs)

`online-boutique` now scores **100% precision / 100% recall** (up from 92.9% recall). The one
miss above — `recommendationservice -> productcatalogservice`, a Python
`demo_pb2_grpc.ProductCatalogServiceStub(channel)` construction site — is now recognized:
`PythonExtractor` had **no gRPC client-side detection at all** (only server-side `*Servicer`
subclasses), unlike every other language extractor. `PythonRelations::rpc_calls` (new) records a
`<Service>Stub(...)` construction site the same way Go's `New<Service>Client(conn)` already did.

`scripts/golden/score.py online-boutique --fail-under-precision 1.0 --fail-under-recall 1.0` now
passes and is the ratchet floor for this repo going forward.

Detection is scoped to the qualified `<x>_pb2_grpc.<Service>Stub(...)` attribute-call shape only
(both generic `grpc_tools.protoc` conventions, not tied to this repo) — a bare `<Service>Stub(...)`
with no `_pb2_grpc`-module qualifier is intentionally not accepted, since nothing would then
distinguish a real generated client from a hand-written test double coincidentally named
`<Something>Stub`. A stub construction with no enclosing function/class (a script wired up inside
`if __name__ == "__main__":`, as Online Boutique's own recommendationservice does) is attributed
to a lazily-created module-level node instead of being dropped or mis-attributed.

## Update (2026-09-25, step 2.2 — manifest-declared service identity)

`ContractNode::package` (used for `pick_or_ambiguous_by_package`'s caller disambiguation, among
other things) came from `detect_service_package`, which used the *directory name* a manifest was
found in, not what that manifest actually declares — a folder named `svc` whose `go.mod` declares
`module github.com/acme/billing-service` was identified as `svc`, not `billing-service`.
`detect_service_package` now reads the real declared name first (`go.mod`'s `module` path,
`package.json`'s `name`, `Cargo.toml`'s `[package].name`, `pyproject.toml`'s
`[project]`/`[tool.poetry].name`), falling back to the directory name exactly as before when a
manifest has none or fails to parse.

Effect measured on Bank of Anthos: resolved edges went from 49 to **81** (node count 358 → 365,
duplicates still exactly 0) — more accurate package identities let more callers disambiguate to a
real match instead of falling into an `Ambiguous` tie or missing a package-scoped resolution
entirely. `online-boutique` stays at 100%/100%.

A real, pre-existing footgun was found and fixed while adding this: `Path::parent()` on a relative
path eventually yields the empty path as its own final ancestor, and `"".join("Cargo.toml")`
resolves against the *process's actual cwd* — inside this workspace, always a real `Cargo.toml`.
The original directory-name-only code never surfaced this (an empty path has no `file_name()` to
return), but reading real manifest *content* would have silently leaked this crate's own
`Cargo.toml` (name `"mesh-core"`) for any synthetic/filesystem-less path with no real manifest in
its own ancestry. Fixed by stopping the walk at the empty path, same as the existing depth cap.

A ruthless review pass also flagged that reading manifest content has no size cap in front of it,
unlike every other file this pipeline touches (`AstGuard`'s 384 KB budget) — `mesh-core` can't
depend on `mesh-parsers`, which owns that guard, so a local `MAX_MANIFEST_SIZE_BYTES` (64 KB,
generous for a real manifest) is checked directly before any read. This also bounds the accepted
(and explicitly not cached — see the field's own doc comment on why) cost of re-reading the same
small manifest once per source file in its directory. A residual, accepted risk: `serde_json`/
`toml`'s recursive-descent parsers have no depth guard against adversarially deep nesting, same as
this project's own `Config::load_from_file` already accepts for the same reason (no practical
attack surface change from what's already tolerated elsewhere in this codebase).

## What's NOT measured yet

- Kafka/Pub-Sub topic resolution (no golden-corpus repo in the current set uses async messaging
  synchronously enough to hand-verify cheaply — `otel-demo`'s golden file should cover this once
  written).
- HTTP route extraction (needs `bank-of-anthos`'s golden file plus a comparable `score.py` mode).
- Anything below the service level (`granularity: "package"` vs per-symbol — step 2.7).

## Ratchet policy

Once a golden file exists for a repo, its precision/recall must never regress:
`scripts/golden/score.py <repo> --fail-under-precision <P> --fail-under-recall <R>`, set to the
last-measured values, is meant to run in CI (not yet wired in — the corpus is still one repo
deep; wiring this before `otel-demo`/`bank-of-anthos` land would just gate on `online-boutique`
alone). Raising either threshold is itself a P1 improvement PR's job.
