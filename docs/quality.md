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
| `otel-demo` | 13 | 87.5% | 53.8% | Golden file written 2026-09-25 (see below); real gap found in the same run. |
| `bank-of-anthos` | 0 (gRPC) | 100% | 100% | Golden file written 2026-09-25 — this repo is genuinely gRPC-free; its `http_routes` ground truth (18 Flask routes) has no `score.py` mode yet (see below). |

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

## Update (2026-09-25, step 2.3 — proto imports)

`.proto` `import "other.proto";` declarations are now recorded as dependencies
(`ProtoRelations::dependencies`), attributed to the file's own first declared node. A first
version attributed every import to every node in the file; ruthless review caught that
`reconcile_edges` doesn't dedup `Imports` edges across different `importer_id`s, so that produced
real N-imports × M-nodes graph noise and false-positive `find_dependents` fan-out — fixed before
merge. Also fixed: single-quoted import paths (`import 'other.proto';`, valid per the protobuf
grammar) were left quoted in the dependency key, never matching a real file path.

## Update (2026-09-25, step 2.4 — Java gRPC client/server idioms)

`JavaExtractor` only recognized Spring's `@GrpcService` annotation for server-side classification,
and had **no client-side gRPC detection at all**. Added both, using grpc-java's own universal
codegen convention (protoc-gen-grpc-java, not Spring-specific):
- Server: a class extending `<Service>Grpc.<Service>ImplBase` is now tagged `GrpcService` — e.g.
  Online Boutique's real `AdServiceImpl extends AdServiceGrpc.AdServiceImplBase`, verified
  correctly tagged after this fix (`mesh-mcp graph --format json` against the real repo).
- Client: `<Service>Grpc.newBlockingStub(channel)` / `.newStub(...)` / `.newFutureStub(...)` — e.g.
  Online Boutique's real `AdServiceClient.java`'s
  `blockingStub = hipstershop.AdServiceGrpc.newBlockingStub(channel);` — is now recorded as an RPC
  call to `AdService`, the same signal Go/Python/TypeScript already emit for their own client
  conventions.

`online-boutique` stays at 100%/100% (the affected edge was already resolved via the `.proto`
declaration; this fix improves node classification and adds a correctly-labeled RPC-call signal).

Ruthless review caught four real issues before merge, all fixed:
- The `ImplBase` server check required only that substring, so an unrelated `*ImplBase` base class
  from a non-gRPC framework using the same generic naming convention would have been mistagged.
  Now requires both `"Grpc"` and `"ImplBase"` in the superclass reference — the real grpc-java
  shape.
- The client-stub scan only checked `method_declaration` bodies, but grpc-java's own convention
  (and Online Boutique's real `AdServiceClient.java`) builds the stub in the class's own
  *constructor* — the PR's own motivating example was, at first, not actually detected. Added
  `constructor_declaration` handling, attributing to the enclosing class (constructors aren't
  extracted as their own nodes).
- The three candidate suffixes (`newBlockingStub`/`newStub`/`newFutureStub`) were checked in a
  fixed order and returned on first match, so a method building two different services' stubs
  could silently lose whichever wasn't checked first. Rewritten to return every stub construction
  found, in source order.
- A real, disclosed limitation (not fixed, deliberately): the server node is named after the impl
  class (`AdServiceImpl`), which doesn't bare-match the client's Grpc-stripped target
  (`AdService`) — self-resolution between this step's two new detectors alone, with no `.proto`
  file present, doesn't work. Fixing this in `contracts.rs` by stripping a generic `Impl` suffix
  was rejected as too risky (an extremely common OOP naming convention unrelated to gRPC, same
  reasoning as step 2.1's reverted `Servicer`-suffix attempt) — every golden-corpus repo has a
  real `.proto` declaration, which resolves correctly (verified above), so this is scoped as a
  known gap for Plan 2's later "cross-signal identity" work, not silently claimed as solved.

## Update (2026-09-25, step 2.5 — env-var-with-default topic resolution)

Go's `var Topic = getTopic()` where `getTopic` reads an env var and falls back to a literal
default — `if v := os.Getenv("KAFKA_TOPIC"); v != "" { return v }; return "orders"` — used to be an
explicit non-goal (P0 step 1.7's own test named it exactly that): resolving it would need real
control-flow interpretation, so it correctly recorded nothing rather than fabricating a value.
`collect_getenv_default_consts` now recognizes this *specific* idiom (an env-var read followed by
a literal fallback return — not "any function that happens to return a string somewhere", which
would reopen the same invented-value risk P0 closed) and resolves the variable to its fallback
literal, the best static signal available without running the program.

**Honest limitation, found while trying to verify this against its own motivating case**: the
real target — the OpenTelemetry demo's `checkout/kafka/producer.go` declares `Topic`, but the
actual producer call site (`Topic: kafka.Topic`) is in a *different file*, `checkout/main.go`,
referencing it across a package boundary. Every const-resolution mechanism in this codebase
(this one included) is scoped to one file's own declarations — this genuinely needs real
cross-file/cross-package resolution, the same "out of scope, would need to parse another file too"
limitation already noted for step 2.3's proto imports. A synthetic single-file reproduction of the
same idiom (the shape this step actually implements) resolves correctly and is covered by a real
regression test; a repo-wide fix is deferred rather than half-solved or overclaimed. `otel-demo`
still has no golden file, so this isn't reflected in a regression number yet — the finding stands
regardless of that gap.

Ruthless review caught that the first version scanned the function's raw *source text* for the
last `return "literal"` substring — fooled by an intermediate conditional branch's literal when
the real, unconditional fallback was dynamically computed (a **wrong** resolved value, worse than
P0's "leave it unresolved"), by a `return "..."` sitting in a `//` comment, and by one inside a
nested closure. Rewritten to require the literal be the function's own **last AST statement** in
its own body block — not text found anywhere — which a conditional branch's return can never be, a
comment isn't part of the AST at all, and a nested closure's statements aren't part of the outer
function's own `named_children`. One more real bug surfaced fixing this: tree-sitter-go's grammar
keeps `comment` as a genuine *named* sibling statement inside a block, so "last named child" alone
still landed on a trailing comment instead of the real last statement — the final version skips
over any trailing comment nodes first.

## Update (2026-09-25, step 2.6 — Flask/FastAPI route path and method)

A Python HTTP route decorator's actual path/method (`@app.route('/users', methods=['POST'])`,
`@router.get("/health")`) used to be discarded entirely — only the Python function name was kept
as `signature`, with no record anywhere of the real HTTP contract it serves. New
`extract_flask_route` surfaces `"<METHOD> <path>"` in `signature` (not `name`, so existing
symbol-name lookups are unaffected) for both real shapes sharing the pre-existing `@app.`/
`@router.` gate: Flask's `@app.route(path, methods=[...])` and FastAPI/`APIRouter`-style
`@router.<verb>(path)`. Verified against a fresh scan of the real Bank of Anthos userservice:
`create_user -> POST /users`, `version -> GET /version`, `login -> GET /login`.

Ruthless review caught two real gaps before merge: only the *first* declared HTTP method was kept
for a multi-method route (`methods=['GET', 'POST']` misreported as just `"GET /users"`) — fixed to
join every declared method; and Flask's legitimate `@app.route(rule='/x')` keyword-only path form
(not just the positional argument) was silently unrecognized, falling back to the plain function
signature despite the node still being tagged `HttpEndpoint` — fixed to also check a `rule=`
keyword argument when no positional string is present.

## Update (2026-09-25, step 2.7 — explicit result-shape controls: `granularity`, `depth`)

Two MCP tools used to return exactly one shape no matter how the caller wanted to slice the
question, silently forcing the agent to post-filter a wall of results itself:

- **`find_dependents`** always returned one result per declaring *symbol*. Asking "which
  services depend on this contract" against a package with a dozen small handler symbols meant
  wading through a dozen near-duplicate entries to see three distinct services. New
  `granularity: "package"` collapses the same result set to one entry per distinct `(repo,
  package)` pair before formatting — a pure post-filter over the existing reverse-dependency
  query, so `granularity: "symbol"` (the default, and anything unrecognized) is byte-for-byte
  the prior behavior.
- **`analyze_impact`** only ever reported the *direct* producers/topics/consumers matching the
  target substring — a real second-order blast radius (a consumer that itself re-publishes onto
  another topic) required the caller to notice that and re-query manually. New
  `analyze_impact_with_depth(target, depth)` walks the real `Produces`/`Consumes` edges the graph
  already carries, not another substring pass: a transitive consumer's own outbound `Produces`
  edge is followed to the topic it feeds, and that topic's consumers are pulled in too, up to
  `depth` hops (clamped to 5). Two `HashSet<NodeId>` visited-sets (topics, nodes) bound the walk
  against a causal cycle — a saga re-producing onto a topic upstream of it — so the traversal
  always terminates instead of looping; covered by a regression test with an explicit
  `topic -> handler -> topic -> handler -> (cycle back to the first topic)` graph, asserting
  depth 1 matches the pre-existing direct-only result, depth 2 picks up exactly the one new hop,
  and depth 5 / an over-cap depth of 200 both terminate at the same result as depth 2 (the cycle
  closes the frontier). `depth` defaults to 1 (`analyze_impact`'s unchanged direct-only
  behavior), so every existing caller (including the seven `analyze_impact("...")` call sites in
  the language-parser test suites) is untouched.

Both are additive, opt-in parameters — the golden-corpus (`online-boutique`, 100%/100%) and
determinism suites (`polyglot-shop`, `volontariapp-fixture`, `determinism` fixture, all still "1
fingerprint over 13 runs") were re-run after this change and are unaffected, since neither
existing default path changed. No real-repo blast-radius chain in the current corpus is deep
enough to hand-verify `depth > 1` against ground truth the way `online-boutique`'s gRPC edges
are — that verification is honestly a synthetic regression test, not a real-repo empirical check;
flagged for `otel-demo`'s golden file (§ below) since its Kafka checkout → accounting/
fraud-detection chain is the corpus's first real multi-hop async blast radius.

## Update (2026-09-25, golden corpus — `otel-demo` and `bank-of-anthos` golden files)

Both remaining corpus repos now have a hand-written `tests/golden/<repo>.expected.yaml`, read
from their real source exactly as `online-boutique`'s was (see Methodology above) — never from
mesh-mcp's own output.

- **`otel-demo`**: 13 gRPC edges (frontend → 6 services, checkout → 6, recommendation → 1) plus
  its Kafka `orders` topic (`checkout` produces, `accounting` and `fraud-detection` consume —
  the corpus's first real async multi-hop chain, not yet scored by `score.py`). Running
  `scripts/golden/score.py otel-demo` against it landed **87.5% precision / 53.8% recall**, not
  the 100%/100% online-boutique gets, and the gap is a genuine, newly-found extraction hole, not
  a golden-file mistake: all 7 Go/Python edges (`checkout → *`, `recommendation →
  ProductCatalogService`) matched exactly, but all 6 `frontend → *` edges were missed. Unlike
  online-boutique (frontend in Go), otel-demo's frontend is TypeScript, calling services via raw
  `@grpc/grpc-js` client construction (`new AdServiceClient(AD_ADDR,
  ChannelCredentials.createInsecure())` in `src/frontend/gateways/rpc/*.gateway.ts`) — a pattern
  `typescript.rs`'s extractor does not recognize; it only detects NestJS's
  `ClientGrpc.getService<XServiceClient>(...)` idiom. One spurious extra edge
  (`checkout -> health`, the gRPC health-check service import) also showed up, not filtered as a
  self/infra edge. Neither is fixed here — this task was writing the golden file and running the
  existing scorer honestly against it, not extending the TypeScript extractor; both gaps are
  recorded as new, real findings rather than adjusting the golden file to hide them.
  - Directory-naming note for future golden files: `score.py` infers a caller's name from its
    `src/<name>/` path, and otel-demo's directories drop the `service` suffix online-boutique's
    have (`src/checkout`, not `src/checkoutservice`) — the golden file's `from:` values had to
    match that real spelling, not the `.proto` service name, for the comparison to line up.
- **`bank-of-anthos`**: genuinely gRPC-free (`score.py bank-of-anthos` correctly reports a
  vacuous 100%/100% on 0 golden edges — no gRPC edge is fabricated by the extractor either). Its
  real ground truth is 18 Flask HTTP routes across its three Python services (`userservice`,
  `contacts`, `frontend`), written by hand from `@app.route(...)` decorators the same way step
  2.6's `extract_flask_route` reads them — including `/` and `/home`, which declare no
  `methods=` kwarg (Flask's own default of GET-only). The repo's other three services
  (`balancereader`, `ledgerwriter`, `transactionhistory`) are Java/Spring MVC
  (`@RestController`/`@GetMapping`/...), a different framework step 2.6 does not extract —
  flagged honestly in the golden file rather than fabricated as Flask routes or silently
  dropped. No `score.py` mode consumes `http_routes` yet (see below).

## Update (2026-09-25, step 3.0 — scale bench: synthetic generator, boot/reload/RSS/p50/p95, nightly budgets)

Plan 3 (P2) starts from a scale-bench harness rather than an optimization, on the theory that you
can't validate later scale work (3.2 persistent index, 3.4 `smart_search` limits, 3.5 memory/CPU)
without a repeatable, numeric baseline first.

- `scripts/bench/gen_synthetic.py <out_dir> <file_count> [--seed N] [--services N]` generates a
  deterministic workspace (Rust/Go/Python/TypeScript/Java files split across N
  `services/svc-N/` directories). Fixed seed by default — reproducible across runs and machines,
  unlike a clone of `linux` or `kubernetes` whose HEAD moves. `scripts/bench/repos.txt`'s real
  large repos (`linux`, `kubernetes`, `vscode`, `grpc`, ...) remain useful for functional smoke
  testing (`scripts/bench/bench.sh`) but are not used for the numeric scale budgets below — too
  slow to clone and not reproducible enough to gate CI on.
- `scripts/bench/scale_bench.py <repo_dir> <config_path> [--queries N] [--budget-json path]`
  launches `mesh-mcp run --standalone` (the same code path `main.rs::run_standalone` uses in
  production: `build_snapshot` runs to completion before the JSON-RPC loop starts, and
  `FileWatcherService::spawn` really runs), then measures:
  - `boot_ms` — wall clock from process spawn to a successful `initialize` response.
  - `rss_peak_mb` — max RSS sampled via `ps -o rss=` across the whole run, not just at boot; if
    every single `ps` sample fails (missing binary, sandboxed CI image), that's surfaced as an
    explicit `rss_peak_mb=unmeasurable` violation rather than silently reading as "0 MB, in
    budget".
  - `search_p50_ms` / `search_p95_ms` — percentiles over `--queries` (default 30) real
    `smart_search` calls, not a single sample (a single `tools/call` timing, as in the older
    `scripts/bench/mcp_client.py` functional smoke test, hides tail latency entirely).
  - `reload_ms` — end-to-end incremental-reload latency: appends a real, per-language declared
    symbol (a function/class, not a comment — `smart_search` only matches declared symbol *names*,
    crates/mesh-core/src/contracts.rs `search_symbols`, never raw file text) to a generated file on
    disk, then polls `smart_search` for that exact symbol until the reported match count is > 0, or
    a 20s deadline, through the real `notify`/`FileWatcherService` → `reload_paths` path (step
    3.1), not a synthetic call into `reload_paths()` directly.
  - Exits non-zero if any metric exceeds `scripts/bench/budgets.json` (or `DEFAULT_BUDGETS` if no
    `--budget-json` given), and the violations are listed by name in the JSON output, not just a
    pass/fail bit.
- **`/code-review` caught three real bugs in the first version of this harness before it merged**,
  all fixed here rather than left as known-broken:
  1. The reload probe wrote the marker into a `//` comment and matched on raw substring-in-response,
     which is a false positive: `MarkdownFormatter::format_search_results` always echoes the query
     into its header (`"## Search Results for `<query>`"`) even on zero matches, so the very first
     poll "succeeded" immediately — the originally reported reload numbers (6ms / 41ms) were not
     real measurements. Fixed by appending a real per-language declared symbol and requiring the
     parsed `Matches: N definitions found` count to be `> 0`.
  2. `typescript.rs`'s extractor only turns `class_declaration`/`interface_declaration` into indexed
     symbols, never a bare top-level function — so a `.ts` target file made the reload probe poll
     forever. Fixed by emitting `export class <Marker> {}` instead of a function for `.ts` targets.
  3. The generator's `marker.json`-per-directory convention was never actually read by
     `crates/mesh-server/src/cli/init.rs` (`has_language_marker`'s fixed list is `go.mod`,
     `package.json`, `pyproject.toml`, `requirements.txt`, `pom.xml`, `build.gradle`, `Cargo.toml`;
     the only directory name it recognizes for a services container is a literal top-level
     `services/`) — so `init --auto` always fell back to a single root `.` and the "multi-root"
     claim was false. Fixed by generating `services/svc-N/` instead, which `init --auto` does
     discover as `./services/*`.
  A fourth finding — `ValidatedScope::resolve` requires a query's `scope` to canonicalize *inside
  one specific allowed root*, not merely be an ancestor covering several — meant `scope: "."` threw
  a sandbox-escape error once root discovery actually started returning N per-service roots instead
  of one. `scale_bench.py` now reads the real roots back out of the generated
  `.agents/mesh-mcp.toml` (`discover_scopes`) and round-robins `smart_search` calls across them
  instead of assuming `"."` is always valid.
- **Real measured baseline** (this machine, release build, single-root synthetic workspace so
  `smart_search` scope covers the whole tree — see the multi-root note below for why that matters):

  | file count | boot_ms | rss_peak_mb | search_p50_ms | search_p95_ms | reload_ms |
  |---|---|---|---|---|---|
  | 5,000  | 186  | 47  | 65   | 176   | 452 |
  | 30,000 | 1,097 | 125 | 460  | 1,264 | 842 |

  `scripts/bench/budgets.json` (used by the nightly CI job below) is set from the 5,000-file row
  with 4–5x headroom for CI-runner variance, not the 30,000-file row — see the honest gap this
  exposes, next. The nightly job itself runs the *multi-root* generator default (`--services 8`),
  which shards `smart_search` scope across per-service roots and so reports lower absolute
  latencies than this single-root table (each call searches a fraction of the graph, not all of
  it) — consistent release-over-release for regression detection, but not directly comparable to
  the whole-tree numbers above. Both numbers come from the same `scale_bench.py`; the difference is
  entirely how many `ValidatedScope` roots the workspace has, which is itself real, not a
  measurement artifact.
- **Real gap found, not hidden**: at 30,000 files, whole-tree `smart_search` p50/p95 (460ms /
  1,264ms) already exceed even generous budgets built from the 5,000-file baseline. This is the
  numeric motivation for steps 3.4 (`smart_search` limits/early-stop/hash cache) and 3.5 (O(N log
  N) memory/CPU work) — 3.0's job was to produce this number honestly, not to fix it. Recorded here
  instead of quietly loosening the budget to make a larger synthetic size pass.
- `.github/workflows/nightly-bench.yml` runs `gen_synthetic.py` (5,000 files) + `scale_bench.py`
  against `scripts/bench/budgets.json` once a day (`workflow_dispatch` also available for manual
  runs) and uploads the raw JSON result as a build artifact. It intentionally does not run on
  every PR — wall-clock budgets are noisy on shared CI runners, and gating every push on them
  would make the gate itself flaky rather than meaningful.

## Update (2026-09-26, step 3.2 — persistent SQLite/WAL content-hash cache for `FileIndex`)

Goal (per Plan 3's handoff doc): avoid a full tree-sitter re-parse/re-derive on every cold start
for large workspaces, using a content-hash key rather than Plan 1's `NodeId`.

- **`NodeId` correction**: Plan 1's `NodeId`s (`ContractGraph`'s `BTreeMap<NodeId, ContractNode>`
  key) are *deterministic*, not *stable* — they're a plain `u32` assigned sequentially while
  folding files into the graph in sorted crawl order (`contracts.rs`'s own doc comment on
  `nodes`), specifically so `BTreeMap` iteration order is reproducible run-to-run (idempotence
  invariant I1). They are **not** content-addressed and shift on any add/remove elsewhere in the
  workspace. Persisting them directly as a cache key would have been wrong — the cache instead
  keys on `mesh_parsers::FileIndex`, the per-file, pre-numbering fragment `WorkspaceIndexer::fold`
  consumes to assign fresh `NodeId`s on every build, cache hit or not. Global numbering is
  identical whether a file's `FileIndex` came from a fresh parse or a cache hit.
- **`PersistentIndexCache`** (`crates/mesh-core/src/index_cache.rs`): SQLite in WAL mode at
  `~/.cache/mesh-mcp/index-cache.db` (mode `0600`, same convention as the Commandment 7 audit db),
  one `file_index_cache(cache_key BLOB PRIMARY KEY, payload BLOB, updated_at INTEGER)` table.
  `cache_key = SHA256(schema_version || path || content_hash || repo_id || config_fingerprint)`:
  path and repo id are folded in (not just the content hash) because the same bytes at two paths,
  or the same file re-scanned under a different `repo_id`, are not guaranteed to extract
  identically (e.g. path-derived package inference); `config_fingerprint` is `SHA256(Debug of
  ExtractConfig)`, computed once per scan, so editing `[engines.contracts.*]` (proto dirs,
  controller annotations, `infer_string_topics`, ...) invalidates stale entries instead of
  silently serving extraction computed under different rules; `schema_version` is folded in
  rather than stored-and-checked, so a future format change orphans old rows for free.
- **Wiring**: `WorkspaceIndexer::process_file`'s two `PolyglotIndexer::extract_with_config` call
  sites go through a new `extract_with_cache` helper — cache hit deserializes and skips
  tree-sitter entirely; miss parses as before and hands back `(FileIndex, Some((key, bytes)))` for
  the caller to persist. A parse that fails (`FileIndex::parse_failed`) is never cached — it's
  retried by `run_scan_pass`'s existing sequential-retry path, and caching a transient failure
  would wrongly persist "no facts" past that retry. Writes are collected across the whole parallel
  scan pass and flushed once via `put_batch` (one transaction), not per file — sqlite writes from
  every Rayon thread would serialize against fsync latency and defeat running extraction on the
  pool at all. Threaded through `build_snapshot`/`build_snapshot_from_files`/`build_graph` as a
  new `Option<&PersistentIndexCache>` parameter, `None` everywhere except the two real server boot
  paths (`mesh-server`'s `run_standalone`, `meshd`'s initial ingestion) — incremental `reload()`
  (step 3.1) is untouched: it already only re-parses differential-VFS-flagged changed files, so a
  content-hash cache has nothing additional to skip there. A cache the process can't open
  (permissions, disk full) degrades to "parse everything," never fails the boot.
- **Real measured baseline** (this machine, release build, `scale_bench.py` — cold run against a
  freshly generated synthetic workspace with an empty cache, then an immediate warm re-run against
  the same on-disk workspace and now-populated cache; nothing else changed between the two runs):

  | file count | boot_ms cold | boot_ms warm | reduction |
  |---|---|---|---|
  | 5,000  | 283.5  | ~90–105 (3 warm runs) | ~65% |
  | 30,000 | 1,344.9 | 668.0 | ~50% |

  A third 5,000-file warm run (90.7ms, then 104.9ms) confirms the warm number is stable rather
  than a one-off fluke. `search_p50_ms`/`search_p95_ms`/`reload_ms` are within noise of each other
  cold vs. warm, as expected — this cache only short-circuits tree-sitter parsing, not
  `smart_search` (step 3.4's job) or incremental reload.
- **Honest limitation**: the benchmark above measures "same workspace, unchanged files, unchanged
  config, second boot" — the scenario the handoff explicitly asked for. It does not yet measure a
  *partial* cache-hit cold start (e.g. 90% of a large workspace unchanged, 10% edited since the
  last boot) or cache behavior once `index-cache.db` itself grows large across many distinct
  workspaces sharing the same machine-wide path — no eviction/size cap exists yet.

## Update (2026-09-26, step 3.3 — watcher registration-time filtering, daemon watchdog hardening)

Two independent pieces: closing the nested-`.gitignore` watcher gap `docs/quality.md`'s step 3.1
section left open, and eliminating an orphaned-daemon failure mode.

- **`FilesystemCrawler::plan_watch_dirs`** (`crates/mesh-core/src/crawler.rs`): every directory
  a watcher should individually register, using the exact same `ignore::WalkBuilder` construction
  (`follow_links(false)`, `git_ignore(true)`, sorted) as a full crawl — including its nested-
  `.gitignore` stacking, since the `ignore` crate discovers and chains ignore files from a walk
  root's ancestors up to a repository boundary regardless of where that root sits. Stops early
  and returns `capped: true` (with an empty `dirs`, never a misleading partial list) past a
  caller-given directory-count budget.
- **`FileWatcherService::spawn`** (`crates/mesh-core/src/watcher.rs`) now registers one
  `RecursiveMode::NonRecursive` native watch per planned directory instead of one
  `RecursiveMode::Recursive` watch per root. A genuinely excluded subtree (gitignored, or
  `[workspace] exclude_patterns`) never gets a watch registered on it at all — closing the gap
  `FilesystemCrawler::is_path_excluded`'s own doc describes (a nested `.gitignore` forcing
  `reload_paths`'s full-crawl fallback because a single incoming path can't cheaply reproduce the
  gitignore stacking a full walk gets right) for the common case: if the file was never watched,
  `reload_paths` is never even asked about it.
- **Above `MAX_WATCHED_DIRS` (2048 on Linux/Windows, 200 on macOS, combined across all roots)**:
  falls back to a `PollWatcher` backend (`notify::PollWatcher`, polling every 2s) with one plain
  recursive watch per root — `PollWatcher` re-scans the tree itself on its own interval, so it needs
  no per-directory registration and isn't subject to the OS watch-descriptor ceiling (concretely,
  Linux's `fs.inotify.max_user_watches`) the per-directory design exists to respect. The cap is
  deliberately conservative rather than tuned to any one platform: macOS's FSEvents backend
  doesn't need per-directory registration to work correctly at all (see below), so this cap
  exists purely to protect the Linux case.
- **New directories after startup**: per-directory registration doesn't automatically track
  subdirectories created after `spawn` the way the old single-recursive-watch design did. The
  event loop detects a changed path that is now a directory, isn't already watched and isn't
  itself excluded, and registers it (plus any of its own qualifying subdirectories, via the same
  `plan_watch_dirs`, in case a whole subtree appeared in one burst).
- **Real finding, not a test bug**: measured on this machine, a single dynamic `.watch()` call
  took **over 11 seconds** under this test suite's own CPU load. `notify`'s macOS FSEvents backend
  stops and restarts its *entire* event stream on every `.watch()` call (`FsEventWatcher::stop()`
  busy-waits via `thread::yield_now()` for the stream's background runloop to go idle) — inotify
  on Linux has no equivalent cost (`inotify_add_watch` is a cheap syscall). Doing this inline on
  the same thread that drains the debouncer's channel would have stalled *every* pending reload
  for however long that took. Fixed by deferring the actual `.watch()` calls to
  `state.rescan`'s background pool (`Arc<Mutex<AnyDebouncer>>`) — the event-receive loop computes
  the (cheap, filesystem-walk-only, measured under 1ms) watch *plan* synchronously so
  `watched_dirs` bookkeeping can't race a second event for the same new directory, but the slow
  OS registration itself never blocks it.
- **Honest limitation, test made `#[ignore]`d rather than fixed further**: the end-to-end dynamic-
  registration test (`new_subdirectory_created_after_startup_is_still_watched`) budgets 20s to
  absorb the ~11s macOS registration cost above, which is reliable in isolation but became flaky
  under `cargo test --workspace`'s additional parallel-test CPU contention — not wrong, just
  timing-sensitive in a way this codebase's other tests aren't. `plan_watch_dirs_*` in
  `mesh-core::crawler` covers the same decision (which directories, respecting excludes)
  synchronously and deterministically; the ignored test remains for manually re-confirming real
  wall-clock behavior (`cargo test -p mesh-server --lib -- --ignored
  new_subdirectory_created_after_startup_is_still_watched`).
- **Daemon idle watchdog, startup-grace guard** (`crates/mesh-daemon/src/idle.rs`): the pre-3.3
  watchdog only ever activated *after* the first client connected — a daemon that never got one
  at all (an `ensure_daemon_running` auto-spawn racing or failing after the process itself
  started, a wrong workspace path so no client ever finds its socket) lived forever, unreachable
  except by PID. `spawn_idle_watchdog` now takes a second, independent `startup_grace` deadline
  (60s, `mesh-daemon/src/main.rs`'s `STARTUP_GRACE` constant): if no client has connected at all
  within that window, the daemon shuts itself down the same graceful way idle-after-use does.
  Spawned unconditionally now (previously gated behind `idle_timeout_minutes > 0`) — disabling
  the idle-*after-use* policy (`idle_timeout_minutes = 0`) no longer also disables this orphan
  guard; internally it's just an effectively-infinite idle timeout passed alongside the real
  60s startup grace.
- **`meshd` auto-spawn output, no longer discarded** (`crates/mesh-server/src/main.rs`):
  `ensure_daemon_running`/`ensure_daemon_running_windows` redirected `Stdio::null()` for the
  child's stdout/stderr, so a crash before `meshd`'s own `tracing` subscriber initializes (or a
  Rust panic, which writes to stderr directly, bypassing `tracing` entirely) was unobservable.
  Now redirected to a rotating, per-workspace log (`open_daemon_log`/`rotate_and_open_log`,
  `~/.cache/mesh-mcp/logs/meshd-<workspace_id>.log`, keeping up to 5 previous runs as `.1`–`.5`)
  — workspace-scoped the same way `socket_path_for` already is, so concurrent daemons for
  different workspaces never interleave into the same file. Verified live: a real `meshd`
  auto-spawn's stdout was captured in the rotated log file exactly as the unit tests predict
  (see below), including the new "startup grace: 60s" log line confirming the watchdog wiring.
- **Hardened via `/code-review high` — ten real findings, all fixed**:
  1. **Exclusion-anchor bug**: dynamic re-registration called `plan_watch_dirs(ev.path, ...)`
     directly, using the newly-created directory itself as the exclusion matcher's anchor. Two
     concrete breaks: `ev.path`'s own name could never be excluded (a walk's own root is never
     passed to `filter_entry`), and a root-anchored pattern (`/vendor`) evaluated relative to the
     wrong root would match different paths than at startup. Fixed: `plan_watch_dirs_from` takes
     `walk_root` and `matcher_root` as separate parameters, with an explicit check for whether
     `walk_root` itself is excluded (since `filter_entry` never runs on a walk's own root).
  2. **`.git/refs` never watched**: `.git` is unconditionally excluded, so `plan_root`'s walk
     never returns anything under it; only `.git/HEAD` had an explicit carve-out, but `fetch`/
     `push`/`commit` update a file under `.git/refs/heads|remotes/...`, not `.git/HEAD` — a real
     regression versus the pre-3.3 single recursive watch, which covered `.git/refs/**` simply by
     covering everything. Fixed: `git_watch_targets` walks `.git/refs` directly (bypassing the
     exclude matcher entirely, depth-capped at 5) and watches every subdirectory found.
  3. **Blocking regression, then a second regression fixing it**: moving `plan_root`'s walk and
     initial `.watch()` registration into the spawned OS thread (to stop blocking `meshd`'s
     socket bind) made `spawn()` return before watches were live — a file changed in that window
     was silently missed, caught by `test_file_watcher_live_reload` failing. Reverted: setup
     stays synchronous inside `spawn()` (its real contract — every caller relies on watches being
     live the instant it returns). The actual blocking concern is fixed at `meshd`'s call site
     instead, with `tokio::task::spawn_blocking` — moving the work off the async runtime's worker
     thread without changing `spawn()`'s synchronous guarantee.
  4. **Shared-pool starvation**: deferred dynamic-registration tasks were dispatched onto
     `state.rescan`, the same (as small as 1-thread) pool the real reload work runs on — a burst
     of new-directory events could starve reload jobs behind an 11-second `.watch()` call, the
     exact stall the deferral was meant to prevent, just moved onto a different queue. Fixed:
     dispatched on a plain detached `std::thread::spawn` instead.
  5. **No way to disable the startup-grace guard**: hardcoded 60s with no override broke a
     developer's manual `meshd` debugging workflow (attach a client more than 60s after starting
     it by hand). Fixed: new `--startup-grace-secs` CLI flag (default 60, `0` disables).
  6. **Daemon log file default permissions**: `rotate_and_open_log` didn't harden the log
     directory/file the way `AuditLogger`/`PersistentIndexCache` do for the same `~/.cache/
     mesh-mcp/` tree, despite explicitly capturing a Rust panic's raw output. Fixed: `0700`/`0600`
     on Unix, matching convention.
  7. **Triplicated `$HOME`/`$USERPROFILE` resolution**: `AuditLogger`, `PersistentIndexCache` and
     the new `open_daemon_log` each independently re-derived it. Factored into
     `mesh_core::paths::mesh_cache_dir()`, used by all three.
  8. **Duplicated `WalkBuilder` setup**: `crawl_scope_with` and `plan_watch_dirs_from` each
     independently set the same five builder options — a fix to one (like finding #1 above)
     could silently not apply to the other. Factored into `FilesystemCrawler::base_walk_builder`.
  9. **Redundant walks on same-batch nested directory creation**: accepted and documented rather
     than fixed — `watched_dirs`'s dedup still prevents a double-watch, this is bounded, rare-case
     wasted work, not a correctness gap.
  10. **Silent rotation failures**: `rotate_and_open_log`'s `rename`/`remove_file` calls discarded
      every error, unlike its own `open` failure path. Fixed: each now logs a warning on failure.
  New regression tests for findings #1 and #2: `plan_watch_dirs_from_excludes_the_walk_root_
  itself_when_it_matches_a_pattern`, `plan_watch_dirs_from_anchors_patterns_to_matcher_root_not_
  walk_root`, `plan_watch_dirs_and_plan_watch_dirs_from_agree_when_roots_match`,
  `git_watch_targets_covers_head_and_every_refs_subdirectory`,
  `git_watch_targets_is_empty_for_a_non_git_directory`.
- Verified after all ten fixes: `cargo test --workspace` (369 passed, 1 ignored — stable across
  repeated runs, confirming finding #3's fix didn't reintroduce the race), `cargo clippy
  --workspace --all-targets -- -D warnings` (clean), `cargo fmt --all -- --check` (clean),
  `scripts/determinism.sh` on both fixtures re-run after the fixes (unaffected, same fingerprints
  as before this step).

### A second `/code-review high` round found the fixes above needed fixes of their own

- **The severity-1 finding**: initial per-directory registration calls `.watch()` once per
  planned directory *sequentially* — not just the dynamic re-registration path finding #3
  originally measured. On macOS, every `.watch()` call after the first stops and restarts the
  whole FSEvents stream (the ~11s-under-load cost already documented), so a workspace with
  hundreds of watchable directories would make `spawn()` itself take minutes, not just the
  post-startup dynamic case. Fixed: `MAX_WATCHED_DIRS` is now platform-specific —
  `#[cfg(target_os = "macos")]` uses 200 (chosen to bound worst-case startup cost, not tuned to
  any OS watch-descriptor ceiling, which doesn't apply to FSEvents at all), other platforms use
  2048 (providing safe headroom below typical inotify defaults like 8192 without risking descriptor starvation).
- **`run_standalone` wasn't wrapped in `spawn_blocking`** the way `meshd`'s equivalent call was —
  on macOS, the same slow synchronous setup would have frozen the whole stdio/MCP proxy during
  standalone-mode startup instead of just delaying watcher readiness. Fixed: wrapped identically.
- **The `spawn_blocking` fix reintroduced a race, fire-and-forget with nothing to catch up**: once
  watches are live (Ok from `spawn`), a file changed during the registration window above had no
  mechanism to ever be noticed — `reload`/`reload_paths` are purely event-driven, so a missed
  change would silently persist until the next restart, undermining the very "return-before-
  events-are-missed" contract `spawn()` itself preserves. Fixed: both call sites now trigger one
  `FileWatcherService::execute_reload_sync` right after a successful `spawn()`, inside the same
  blocking-pool closure — it acquires `reload_lock` the same as initial ingestion, so it safely
  queues behind ingestion if that's still running, then does one cheap differential VFS diff
  against whatever changed on disk during setup, closing the gap definitively rather than leaving
  it to chance.
- **Polling fallback's single-root failure aborted the whole `spawn()` call** (`?` on one root's
  `.watch()`, unlike the native path's per-directory resilience a few lines below) — one missing/
  unreadable root in a huge multi-root workspace (the only case that reaches this fallback) would
  silently disable watching for *every* root. Fixed: logged and skipped per-root, matching the
  native path.
- **Stale doc comment**: a test's own comment said dynamic registration was "deferred onto
  `state.rescan`'s background pool" — the opposite of finding #4 above, which moved it off that
  pool specifically to avoid starving it. Fixed the comment.
- **Silent `continue` on a capped dynamic subtree**: unlike every other degraded/capped path in
  this file, a subtree alone exceeding the remaining watch budget logged nothing. Fixed: added
  the missing `tracing::warn!`.
- **Accepted, not fixed**: nothing bounds how many detached registration threads can be *in
  flight* at once — they only serialize against each other via a shared mutex, not a queue. A
  workload creating new directories across many separate debounce windows in rapid succession
  could accumulate more blocked threads (each holding a stack) than drain. A single dedicated
  worker thread with an internal queue would close this properly; not built given how narrow a
  burst pattern needs to be to matter in practice, and the platform-aware cap above already
  reduces how often the native (non-polling) path is reached at all on macOS.
- **Live-verified on this repo** (`meshd` built from this branch against causalmesh's own 6
  configured roots, ~40 total directories — comfortably under the 200 cap): native per-directory
  mode engaged correctly, per-root "watching N directories" log lines appeared, full ingestion +
  watch registration completed in well under 100ms with the machine otherwise idle (consistent
  with the "under load" qualifier on the ~11s FSEvents figure — that cost is real but
  contention-dependent, not a fixed per-call tax).
- Verified again after all seven of these fixes: `cargo test --workspace` (369 passed, 1 ignored,
  stable across three consecutive runs), clippy/fmt clean, `scripts/determinism.sh` unaffected
  (same fingerprints).

## Update (2026-09-26, step 3.4 — `smart_search` at scale, exact line anchoring, Python docstrings, anchored scopes)

- **Pagination + early stop**: `smart_search` gains `limit` (default 20, max 100) and `offset`.
  Files are ranked in memory from the symbol index; only the requested page's files are read and
  decapitated (the old path read + tree-sitter-parsed *every* matching file — thousands for
  `Service` at 30k files — before the 48 KB cap threw most of it away). A page also stops before
  the payload budget and prints the exact next `offset`.
- **Result cache** (`mesh_core::SearchCache`, on `AppState`): keyed by (query, canonical scope,
  include_body, fuzzy, limit, offset), valid only at the snapshot generation it was computed
  from; any reload bump drops it, and a page computed from an older generation is discarded on
  insert. Fuzzy pages (disk state the index doesn't track) are not cached.
- **Exact line anchoring**: removed the `orig_lines.iter().position(...)` re-matching.
  `AstDecapitator::decapitate_auto_mapped` returns a per-output-line map to original lines,
  built from the replaced tree-sitter nodes' byte ranges; indexed hits anchor on the symbol's
  `start_position().row + 1`.
- **Python docstrings** survive decapitation; only the statements after them become `...`.
- **`ValidatedScope`** anchors relative scopes on the resolved `workspace_root`
  (`Config::resolve_workspace_root`) instead of the process CWD.
- **Measured** (`scale_bench.py`, 30,000-file single-root synthetic workspace, release build,
  same machine, baseline = `04f213c`):

  | build | queries | search_p50_ms | search_p95_ms | boot_ms | rss_peak_mb |
  |---|---|---|---|---|---|
  | baseline | 60 (6 distinct) | 905 | 3,342 | 2,978 | 169 |
  | step 3.4 | 60 (6 distinct, 54 cache hits) | 0.2 | 64 | 1,954 | 128 |
  | baseline | 6 cold, 3 runs | 840–885 | 1,849–2,613 | | |
  | step 3.4 | 6 cold (all cache misses), 3 runs | 67–88 | 96–109 | | |

  The cold rows are the honest number: with the cache out of the picture, p50/p95 are ~10x/20x
  under baseline and well inside the 300/800 ms budget. `reload_ms` came back `None` for *both*
  binaries on this corpus (the harness's reload probe did not see its marker within 20 s) — a
  harness issue to investigate separately, not a 3.4 regression.

### Step 3.4 review fixes (PR #29)

- Indexed anchors are verified against the file on disk (in range, surrounding lines mention the
  query or its `_`-insensitive form) before use; pattern / AsyncAPI / OpenAPI nodes now record
  their real line (index-cache `SCHEMA_VERSION` 2). The bounded stub falls back to original lines.
- Pages measure each rendered entry before accepting it, so the formatter's 48 KB truncation can
  no longer silently drop a result that `next_offset` then skips. Fuzzy pages stop announcing
  "More results" without a real further match and show lower-bound totals as `≥N`.
- The cache key includes the raw scope spelling; cached pages re-stat the files they read.
- `ValidatedScope` falls back to the CWD when a relative scope is absent under `workspace_root`.
- Re-measured once (30k synthetic workspace, release, 6 cold queries): search p50 57 ms /
  p95 112 ms (step 3.4: 67–88 / 96–109 ms). Boot 3.7 s, cold because the index-cache schema
  bump orphans previous rows.

## Update (2026-09-26, step 3.5 — streaming YAML, doc memory, `derive()` without quadratic passes, 200k validation)

- **`derive()` / `reconcile_edges` quadratic passes removed**:
  - `Implements` compared every proto method with every gRPC handler (O(P·H)). Handlers are now
    indexed by every key the match predicate can succeed on (exact name, ASCII-lowercased and
    `_`-normalized last segment, PascalCase, signature identifiers — the latter a sorted list
    searched by prefix range); candidates are re-checked with the unchanged predicate, so the
    edge set is identical (`handler_index_never_drops_a_predicate_match`).
  - Import resolution scanned a whole `name_to_nodes` bucket per importer; it is now memoized per
    (target, importer repo), the only inputs it depends on.
  - `patch_files` did one `retain` over a key's bucket per stale node (O(k·N) for a hot name like
    `handle`); removals are grouped per key.
  - The step-3.4 AsyncAPI/OpenAPI line lookup re-scanned the file from the top for every key
    (`lines().skip(from)`), and to EOF on every miss: now one pre-split line index with a
    forward-only cursor that gives up on a section at its first miss, and OpenAPI methods are
    searched only within their path's line range — O(lines + keys) per file.
- **Streaming YAML**: Spring property files flatten through a serde visitor straight into
  `(dotted.key, value)` pairs — no `serde_yaml::Value` tree. Output is byte-identical to the old
  tree flattening (`streaming_yaml_matches_dom_flattening`), ingestion stays atomic on malformed
  input, and a multi-document file now contributes its first (default-profile) document instead of
  being rejected wholesale. AsyncAPI/OpenAPI specs deserialize into key-only shapes
  (`languages/spec_shape.rs`) that skip every schema/example subtree as `IgnoredAny`.
- **Docs memory**: `DocSection` no longer keeps a lowercased copy of ASCII content (matching is
  byte-wise equivalent to `to_lowercase().contains`); only non-ASCII sections keep one.
- **`gen_synthetic.py --contracts`**: the plain generator never produced imports, protos, gRPC
  handlers, YAML or Markdown, so it could not exercise any of the above. The flag adds that mix
  (default off; nightly numbers unchanged).
- **Measured at 200,000 files** (release, single root, cold persistent cache via a fresh `HOME`,
  `/usr/bin/time -l` peak memory footprint — `ps` RSS is unusable on macOS here: memory
  compression dropped an idle server from 900 MB to 12 MB within 2 minutes; baseline = step 3.4
  head `835361f`):

  | build | corpus | boot | peak footprint |
  |---|---|---|---|
  | 3.4 | 200k plain | 13.9 s | 630 MB |
  | 3.5 | 200k plain | 15.3 s | 637 MB |
  | 3.4 | 200k + 48k contract mix | **307.8 s** | 809 MB |
  | 3.5 | 200k + 48k contract mix | **17.0 s** | 829 MB |

  The contract-mix boot drops ~18x and now scales like the plain corpus; memory is flat. **Honest
  gap**: at 200k files both builds are far over the 5k-derived CI budgets (boot 3 s, RSS 300 MB,
  search p50/p95 400-480 / 920-1,040 ms). The peak is dominated by the parallel parse phase and the
  resident graph, not by anything this step touched; it is not hidden by loosening the budgets.

## Plan 3 closeout (2026-09-26, 6.0.0 — final verification after steps 3.0–3.6)

All numbers below come from the release build of `6.0.0` (branch `p2/3.6-visualize-and-mcp-compliance`),
on this machine, with a fresh `HOME` per run so the persistent index cache (step 3.2) is cold, and
compared against step 3.4's head (`835361f`) as the reference where a before/after exists.

**Regression suites**

| suite | result |
|---|---|
| `scripts/determinism.sh` (Plan 1) | ✔ `polyglot-shop` 1 fingerprint / 13 runs; ✔ `volontariapp-fixture` 1 fingerprint / 13 runs (5 sequential + 8 concurrent each) |
| `scripts/golden/score.py online-boutique` (Plan 2) | 14/14 — **100% precision / 100% recall** (unchanged) |
| `scripts/golden/score.py otel-demo` | 7/13 — **87.5% / 53.8%**, identical to the reference binary and to the last recorded value: the known TypeScript `@grpc/grpc-js` client gap + the `checkout -> health` extra edge, not a regression |
| `scripts/golden/score.py bank-of-anthos` | 0/0 — vacuous 100% / 100% (gRPC-free, nothing fabricated) |
| nightly `scale_bench.py` (5k files, 8 roots, `budgets.json`) | ✔ no violation, 3 runs + 1 run outside the repo: boot 0.5–1.1 s (budget 3 s), RSS peak 48–49 MB (300), search p50 4.4–9.7 ms (300), p95 7.6–24.2 ms (800), reload **207 ms** (3 s; reference 210 ms) |

**`reload_ms: None` explained, not a regression.** Every corpus generated under `target/`
(git-ignored by this repo's `/target` rule) reported `reload_ms: None` — for the reference binary
too. Step 3.3's watcher honours `.gitignore` files *above* the watched root, so edits inside a
git-ignored directory are, correctly, never watched and the harness's reload probe times out. The
same 5k corpus generated outside the repository (`~/bench-repos/mesh-synth-5k`) reloads in 207 ms.
Scale corpora must therefore not live under an ignored path; the step 3.4/3.5 notes that called
this "a harness issue to investigate" are resolved by this.

**Scale, end to end (cold)**

| measurement | before (first number in Plan 3) | 6.0.0 |
|---|---|---|
| `smart_search` 30k files, single root, p50 / p95 | 905 / 3,342 ms (`04f213c`) | 57 / 112 ms |
| boot, 200k files + 48k contract mix | 307.8 s (step 3.4 head) | 17.0 s |
| peak footprint, same corpus | 809 MB | 829 MB |
| boot / peak footprint, 200k plain files | 13.9 s / 630 MB | 15.3 s / 637 MB |
| `visualize_mesh`, 248k nodes | raw graph over 48 KB, cut mid-document (invalid HTML/JSON) | 4.3 KB mermaid / 16.9 KB json / 40.1 KB html, all valid |
| `tools/list` schemas | 7,696 bytes | 5,524 bytes (~540 tokens/session less) |

**Still over budget, recorded not hidden**: the nightly budgets are derived from the 5k corpus and
hold there. At 200k files boot (~15–17 s) and peak memory (~630–830 MB) are far above them; that
cost is the parallel parse phase plus the resident graph, which Plan 3 did not target. Budgets
were not loosened to make larger corpora pass.

## What's NOT measured yet

- The 30,000-file `smart_search` budget violation above is not yet re-measured against a *real*
  large repo (`kubernetes`, `linux`) through the same `scale_bench.py` harness — only through the
  synthetic generator so far. Real repos have deeper directory trees and larger individual files,
  which may shift `boot_ms`/`rss_peak_mb` independently of raw file count.
- `reload_ms` has only been measured for a single-file edit; a multi-file burst (e.g. a branch
  checkout touching hundreds of files at once) exercises the debounce-coalescing path in
  `FileWatcherService::schedule_reload` differently and is not yet in the harness.
- Kafka/Pub-Sub topic resolution has real ground truth now (`otel-demo`'s `orders` topic, above)
  but no `score.py` mode reads it yet.
- HTTP route extraction has real ground truth now (`bank-of-anthos`'s 18 Flask routes, above) but
  no comparable `score.py` mode exists yet; nor is there ground truth for its three Java/Spring
  services, a different framework step 2.6 does not extract.
- The TypeScript gRPC-client-construction gap found above (`new XServiceClient(...)` from
  `@grpc/grpc-js`, distinct from the already-covered NestJS `getService<XServiceClient>(...)`
  idiom) — real, unfixed, first observed against `otel-demo`'s frontend.
- The `checkout -> health` spurious edge found above — the gRPC health-check service import
  resolving as if it were a real service dependency.
- `analyze_impact_with_depth`'s `depth > 1` traversal against a *real* multi-hop async chain — the
  ground truth for one now exists (`otel-demo`'s `orders` topic chain, above), but no
  `score.py`-style transitive-impact scorer has been run against it yet; verified so far only
  against the synthetic cycle-detection regression test in step 2.7.
- `PersistentIndexCache` (step 3.2) has no eviction or size cap: `index-cache.db` grows
  unboundedly as distinct workspaces/paths/configs accumulate entries on a shared machine over
  time. Only the "same workspace, second boot" scenario is measured so far — not a mixed
  cache-hit-rate cold start, nor long-run db size under many different repos.
- `FileWatcherService`'s `MAX_WATCHED_DIRS` cap (step 3.3) has not been measured against a real
  200,000+ file repository to confirm the `PollWatcher` fallback actually engages and stays
  responsive at that scale — only proven at the unit level (`plan_watch_dirs_reports_capped_
  without_a_partial_list`) with a synthetic 10-directory tree well under the `MAX_WATCHED_DIRS` threshold.
  Similarly, the dynamic-registration path's real ~11s-per-call FSEvents cost on macOS under load
  (see step 3.3's section above) has not been characterized on Linux (inotify) or Windows
  (ReadDirectoryChangesW) — only asserted to be cheaper by architecture, not measured.
- `open_daemon_log`'s rotation (step 3.3) is tested at the algorithm level (`rotate_and_open_log`
  against a temp directory) but not exercised concurrently — two `meshd` auto-spawns for the
  *same* workspace racing `ensure_daemon_running` at the same moment (unlikely, since the socket
  check should prevent it, but not proven) could interleave their rotation logic.

## Ratchet policy

Once a golden file exists for a repo, its precision/recall must never regress:
`scripts/golden/score.py <repo> --fail-under-precision <P> --fail-under-recall <R>`, set to the
last-measured values, is meant to run in CI (not yet wired in — the corpus is still one repo
deep; wiring this before `otel-demo`/`bank-of-anthos` land would just gate on `online-boutique`
alone). Raising either threshold is itself a P1 improvement PR's job.
