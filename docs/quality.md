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
