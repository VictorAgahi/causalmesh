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
| `online-boutique` | 14 | **100%** | **92.9%** | 1 miss: `recommendationservice -> productcatalogservice` (Python gRPC stub construction, `demo_pb2_grpc.ProductCatalogServiceStub(channel)`, not yet a recognized idiom — step 2.4). |
| `otel-demo` | — | — | — | Golden file not yet written (pending). |
| `bank-of-anthos` | — | — | — | Golden file not yet written (pending); this repo is mostly HTTP/REST internally, not gRPC — its golden file should score `http_routes`, not `grpc_edges`, once step 2.6 lands a comparable extraction for those. |

**How to reproduce:**
```bash
scripts/golden/fetch.sh
mesh-mcp init --auto   # run once inside ~/.cache/mesh-golden/<repo>, or point --config at it
scripts/golden/score.py online-boutique
```

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
