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
  deterministic multi-root workspace (Rust/Go/Python/TypeScript/Java files split across N
  `service-*/` directories, each with a `marker.json` so `init --auto` finds them as real roots).
  Fixed seed by default — reproducible across runs and machines, unlike a clone of `linux` or
  `kubernetes` whose HEAD moves. `scripts/bench/repos.txt`'s real large repos (`linux`,
  `kubernetes`, `vscode`, `grpc`, ...) remain useful for functional smoke testing
  (`scripts/bench/bench.sh`) but are not used for the numeric scale budgets below — too slow to
  clone and not reproducible enough to gate CI on.
- `scripts/bench/scale_bench.py <repo_dir> <config_path> [--queries N] [--budget-json path]`
  launches `mesh-mcp run --standalone` (the same code path `main.rs::run_standalone` uses in
  production: `build_snapshot` runs to completion before the JSON-RPC loop starts, and
  `FileWatcherService::spawn` really runs), then measures:
  - `boot_ms` — wall clock from process spawn to a successful `initialize` response.
  - `rss_peak_mb` — max RSS sampled via `ps -o rss=` across the whole run, not just at boot.
  - `search_p50_ms` / `search_p95_ms` — percentiles over `--queries` (default 30) real
    `smart_search` calls, not a single sample (a single `tools/call` timing, as in the older
    `scripts/bench/mcp_client.py` functional smoke test, hides tail latency entirely).
  - `reload_ms` — end-to-end incremental-reload latency: appends a uniquely-named symbol to a
    generated file on disk, then polls `smart_search` for that exact symbol until it's visible or
    a 20s deadline, through the real `notify`/`FileWatcherService` → `reload_paths` path (step
    3.1), not a synthetic call into `reload_paths()` directly.
  - Exits non-zero if any metric exceeds `scripts/bench/budgets.json` (or `DEFAULT_BUDGETS` if no
    `--budget-json` given), and the violations are listed by name in the JSON output, not just a
    pass/fail bit.
- **Real measured baseline** (this machine, release build, synthetic workspace, 30 queries):

  | file count | boot_ms | rss_peak_mb | search_p50_ms | search_p95_ms | reload_ms |
  |---|---|---|---|---|---|
  | 5,000  | 184  | 47  | 58   | 176   | 6  |
  | 30,000 | 882  | 127 | 393  | 1,282 | 41 |

  `scripts/bench/budgets.json` (used by the nightly CI job below) is set from the 5,000-file row
  with 4–16x headroom for CI-runner variance, not the 30,000-file row — see the honest gap this
  exposes, next.
- **Real gap found, not hidden**: at 30,000 files, `smart_search` p50/p95 (393ms / 1,282ms)
  already exceed even generous budgets built from the 5,000-file baseline. This is the numeric
  motivation for steps 3.4 (`smart_search` limits/early-stop/hash cache) and 3.5 (O(N log N)
  memory/CPU work) — 3.0's job was to produce this number honestly, not to fix it. Recorded here
  instead of quietly loosening the budget to make a larger synthetic size pass.
- `.github/workflows/nightly-bench.yml` runs `gen_synthetic.py` (5,000 files) + `scale_bench.py`
  against `scripts/bench/budgets.json` once a day (`workflow_dispatch` also available for manual
  runs) and uploads the raw JSON result as a build artifact. It intentionally does not run on
  every PR — wall-clock budgets are noisy on shared CI runners, and gating every push on them
  would make the gate itself flaky rather than meaningful.

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

## Ratchet policy

Once a golden file exists for a repo, its precision/recall must never regress:
`scripts/golden/score.py <repo> --fail-under-precision <P> --fail-under-recall <R>`, set to the
last-measured values, is meant to run in CI (not yet wired in — the corpus is still one repo
deep; wiring this before `otel-demo`/`bank-of-anthos` land would just gate on `online-boutique`
alone). Raising either threshold is itself a P1 improvement PR's job.
