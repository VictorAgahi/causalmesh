# Benchmarks and measurement harnesses

This page lists the tools the repository uses to measure MeshMCP and how to run them. It does not
repeat results: every published figure lives, dated and with its platform and command, in
[`quality.md`](quality.md). If a number is not there, it has not been measured.

Rules followed by every measurement in `quality.md`:

- release build (`cargo build --release -p mesh-server`, or `--workspace` when `meshd` is needed);
- isolated `HOME` and a private `MESH_SOCKET_PATH`, so a run never talks to the user's own
  `meshd` or reuses its cache;
- the platform, the corpus (pinned commit or fixed seed) and the exact command are written next
  to the result, and a before/after pair is measured on the same machine in the same session.

---

## 1. Harnesses

| What | Harness | Gate | Results |
| :--- | :--- | :--- | :--- |
| gRPC edge precision/recall on real projects | `scripts/golden/fetch.sh` (pinned corpus in `scripts/golden/repos.txt`), `scripts/golden/score.py <repo>`, ground truth in `tests/golden/*.expected.yaml` | `.github/workflows/golden.yml`, every PR, `ubuntu-latest`, fails under 1.0/1.0 | `quality.md` baseline and step 4.5 |
| Index determinism | `scripts/determinism.sh` (5 sequential + 8 concurrent runs per workspace, one fingerprint required) | `ci.yml` "Determinism Gate", `ubuntu-latest` and `macos-latest` | `quality.md` Plan 3 closeout |
| Boot, peak RSS, `smart_search` p50/p95, reload latency | `scripts/bench/gen_synthetic.py <dir> <files> [--services N]` + `scripts/bench/scale_bench.py <dir> <config> --budget-json scripts/bench/budgets.json` | `nightly-bench.yml`, daily, `ubuntu-latest`, 5,000 files, 8 roots | `quality.md` step 3.0 and Plan 3 closeout |
| Parse cache growth over a simulated work week | `scripts/bench/cache_growth.py` | none | `quality.md` step 4.4 |
| Reload behaviour during a large `git checkout` | `scripts/test_git_storm.sh` | none (exits non-zero on a violation) | `quality.md` step 4.2 |
| YAML / Markdown peak memory per file | `scripts/bench/gen_yaml_md_corpus.py` + `scripts/bench/yaml_md_peak.py` (macOS `/usr/bin/time -l`), probe `crates/mesh-server/examples/parse_peak.rs` | none | `quality.md` step 4.9 |
| `smart_search` page size | `scripts/bench/search_payload.py` on the golden corpora | none | `quality.md` steps 4.13 and 4.1 note |
| `analyze_impact` latency on the fixture | `crates/mesh-server/tests/impact_matrix.rs` | `cargo test` (asserts < 100 ms) | `quality.md` step 4.6a |
| Functional smoke test on large public repos | `scripts/bench/bench.sh` (clones `scripts/bench/repos.txt`) | none | not used for numeric budgets |
| Micro-benchmarks (decapitation, graph queries, audit write, Markdown formatting) | `cargo bench -p mesh-server` (`crates/mesh-server/benches/real_benchmarks.rs`, custom `main`, warm-up then timed iterations) | none | not published; compare before/after on one machine |

`scripts/bench/token-compare/` compares raw output sizes of `rg`, `rtk grep` and `smart_search`
for one query. No result from it is published, and it measures output size for a single query,
not token use by an agent over a task.

---

## 2. Running the main ones

```bash
cargo build --release -p mesh-server

# Golden corpus (clones into ~/.cache/mesh-golden)
scripts/golden/fetch.sh
PATH="$PWD/target/release:$PATH" scripts/golden/score.py online-boutique

# Determinism
scripts/determinism.sh

# 5k synthetic scale bench, same as the nightly job.
# Generate the corpus outside this repository: a corpus under a git-ignored path
# (such as target/) is never watched, and the reload probe then reports None.
python3 scripts/bench/gen_synthetic.py /tmp/mesh-synth-5k 5000 --services 8
(builtin cd /tmp/mesh-synth-5k && "$OLDPWD/target/release/mesh-mcp" init --auto)
MESH_MCP_BIN=target/release/mesh-mcp python3 scripts/bench/scale_bench.py \
  /tmp/mesh-synth-5k /tmp/mesh-synth-5k/.agents/mesh-mcp.toml \
  --queries 30 --budget-json scripts/bench/budgets.json

# smart_search page payload on the golden corpora
MESH_MCP_BIN=target/release/mesh-mcp python3 scripts/bench/search_payload.py --limit 20
```

The Python harnesses document their options under `--help`. Wall-clock results on a shared or busy machine are noisy; the
nightly budgets carry 4–5x headroom for that reason, and `quality.md` notes the machine load
when it matters.

---

## 3. What is deliberately not claimed

- **Token or cost savings.** Stripping function bodies and capping pages reduce what a single
  answer contains, but whether an agent spends fewer tokens to finish real work depends on how it
  uses the tools. That is measured by the pilot's A/B protocol,
  [`pilot-scorecard.md`](pilot-scorecard.md), and nothing is claimed before it is filled in.
- **Comparisons with other tools.** No head-to-head benchmark against language servers or code
  search products has been run.
- **Results on your repository.** Synthetic corpora are reproducible but not representative of
  every layout. `mesh-mcp doctor`, `mesh-mcp stats` and the harnesses above can be run on your own
  workspace.

The open measurement gaps are listed in `quality.md`, "What's NOT measured yet".
