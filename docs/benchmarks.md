# Benchmarks

This document explains what MeshMCP measures, how to reproduce it on your own hardware, and —
just as importantly — what is *not* measured.

Every number here comes from `cargo bench -p mesh-server` on the machine named in the run
header. Latency is hardware- and workload-dependent; treat these as a baseline to compare
against, not as a specification.

---

## 1. Running the suite

```bash
cargo bench -p mesh-server
```

The harness lives in [`crates/mesh-server/benches/real_benchmarks.rs`](../crates/mesh-server/benches/real_benchmarks.rs).
It runs 20 warm-up iterations then 500 measured ones per case, against real source samples and a
live 1,000-node graph — no mocks, no synthetic stubs.

A regression test asserts the budgets still hold, so a performance regression fails CI like any
other bug:

```bash
cargo test -p mesh-server test_real_benchmarks_regression_budgets
```

---

## 2. Reference run

Apple Silicon, 8 cores, macOS, release profile (thin LTO, `codegen-units = 1`, mimalloc):

```
Category            | Benchmark                            | Avg(µs) | p95(µs) |   Ops/sec | Throughput |  Tokens
------------------------------------------------------------------------------------------------------------------
AST Decapitation    | TypeScript (n=500)                   |  150.57 |  264.21 |      6642 |  11.0 MB/s |  -54.5%
AST Decapitation    | Rust (n=500)                         |  107.03 |  116.33 |      9343 |  13.7 MB/s |  -57.4%
AST Decapitation    | Go (n=500)                           |  109.86 |  121.58 |      9103 |  12.2 MB/s |  -68.9%
Lexical Guard       | AstGuard::max_nesting_depth (n=1000) |    1.20 |    1.29 |    835424 | 988.7 MB/s |       -
Contract Graph      | find_dependents (n=500)              |    3.95 |    4.17 |    252928 |          - |       -
Contract Graph      | analyze_grpc pipeline trace (n=500)  |    9.12 |    9.17 |    109658 |          - |       -
Audit Logging       | record_entry (SHA-256 chain, n=500)  |   12.22 |   22.29 |     81842 |          - |       -
Markdown Formatting | format_search_results 48 KB (n=500)  |   34.40 |   37.42 |     29066 |          - |       -
```

Binary and memory, same machine:

| | |
| :--- | :--- |
| `mesh-mcp` release binary | 12.3 MB |
| `meshd` release binary | 12.1 MB |
| Peak RSS indexing this repo (60 files) | 20.5 MiB |

RSS scales with workspace size — the graph, the doc index and the file-signature cache all grow
with it. Measure on your own repo rather than extrapolating from this one.

---

## 3. What each number means

**AST decapitation** — parse a source file and strip function bodies to `{ /* stripped */ }`
(or `...` in Python). The `Tokens` column is the measured reduction in token count for that
sample, computed by the harness, not an estimate.

Reduction depends entirely on code style: a file of one-line delegating methods barely shrinks,
a file of long business-logic bodies shrinks a lot. The 54–69% range above reflects the three
samples in the harness (TypeScript, Rust, Go). To know your own figure, run the suite — do not
assume a single headline percentage applies to your codebase.

Signatures, parameter types, return types, annotations (`@Service`, `@GrpcMethod`, …) and
docstrings are always preserved. An agent that genuinely needs an implementation asks for it
with `include_body: true` on that one scope.

**Lexical guard** — the pre-parse scan that rejects pathological files (nesting beyond 64
levels), skipping brackets inside strings and comments. It runs over every candidate file, so it
has to be effectively free; ~989 MB/s means it is.

**Contract graph queries** — `find_dependents` and `analyze_grpc` against a 1,000-node graph.
Both resolve through in-memory indices, not scans, so they stay in the microsecond range as the
graph grows.

**Audit logging** — one `record_entry`: a `BEGIN IMMEDIATE` SQLite WAL transaction, reading the
committed tail of the hash chain, computing the SHA-256 link and inserting. This is the slowest
per-call operation in the server and it still costs ~12 µs. It runs on the blocking pool, never
on the async executor.

**Markdown formatting** — rendering search results under the hard 48 KB output cap, including
the truncation path that tells the agent how to narrow its query.

---

## 4. What is not measured here

Be sceptical of any figure in this section's absence — if it isn't in the harness, it isn't a
benchmark:

- **Cold boot / full workspace scan.** Dominated by your disk, file count and file sizes. Time
  it yourself: `time mesh-mcp graph --format json > /dev/null`.
- **End-to-end tool latency through an MCP client.** Depends on the client and transport.
- **Stdio round-trip latency.** `mesh-mcp doctor` prints a loopback measurement for your
  machine.
- **Incremental reload time.** Proportional to the number of *changed* files, not workspace
  size, because unchanged files are rejected on a `stat` before being read.
- **Memory at scale.** Measure with `/usr/bin/time -l` (macOS) or `/usr/bin/time -v` (Linux) on
  your workspace.

---

## 5. Where the performance comes from

The design decisions behind the numbers, in rough order of impact:

- **Index-first queries.** `smart_search` consults the in-memory symbol index and reads only the
  files that declare a match. It does not crawl or parse the scope unless you pass `fuzzy: true`.
- **One atomic snapshot.** Readers take a pointer copy of an immutable `MeshSnapshot`; writers
  build a new one and swap it in. No locks in the query path, and no chance of observing a
  half-updated index.
- **Linear reconciliation.** Edge de-duplication goes through a `HashSet` and target resolution
  through the graph's own indices, so a rescan is linear in nodes plus edges.
- **Differential reloads.** A file whose mtime and size are unchanged is never read, let alone
  parsed. Changed content is confirmed with a SHA-256 hash, so a bare `touch` costs nothing.
- **Amortised resources.** Regexes from `[[engines.contracts.patterns]]` compile once per run;
  tree-sitter parsers are cached per thread instead of being rebuilt per file.
- **Interned paths and compact strings.** `Arc<Path>` gives one path buffer per file rather than
  one per symbol; `CompactString` keeps names up to 24 bytes on the stack.
- **Bounded work.** 15 ms parse timeout, 384 KB file budget (1.5 MB for schemas), 1 KB max line
  length, 48 KB output cap. Worst-case cost per file is bounded by construction.
- **Background priority.** Rescans run on a Rayon pool with `QOS_CLASS_BACKGROUND` (macOS) or
  `nice(10)` (Linux) so re-indexing does not compete with your editor. The boot scan
  deliberately uses the normal-priority pool — you are waiting for it.

For the reasoning behind each, see [architecture.md](architecture.md).

---

## 6. Comparing honestly

The table below is qualitative on purpose. Published RAM and latency figures for other tools
depend on their configuration and corpus, and reproducing them fairly is a project of its own.

| | grep / ripgrep | Language server | MeshMCP |
| :--- | :--- | :--- | :--- |
| Result granularity | text matches | full AST symbols | signatures, bodies stripped |
| Cross-language links | none | rarely | gRPC, events, imports across repos |
| Reverse dependencies | brute-force search | per-project | in-memory index |
| Output bounding | none | structured | 48 KB cap with narrowing guidance |
| Secret masking | none | none | redacted before reaching the prompt |
| Audit trail | none | none | SHA-256 chained SQLite log |

If you benchmark MeshMCP against something else, publish the corpus and the commands. We will
link to it.
