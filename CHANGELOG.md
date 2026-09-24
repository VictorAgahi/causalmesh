# Changelog

All notable changes to MeshMCP (`mesh-mcp` / `meshd`) are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). This file starts
at 3.0.0 — there is no reconstructed history before it.

## [Unreleased]

Plan 1 (P0): make every answer reproducible and honest. This first step only
*measures* determinism; nothing about indexing behaviour changes yet.

### Added
- **Content fingerprint of the index.** `ContractGraph::canonical_lines()` /
  `fingerprint()`, `DocIndex::canonical_lines()`, `PropertyRegistry::canonical_lines()` and
  `MeshSnapshot::fingerprint()` (`crates/mesh-core`): a SHA-256 over a sorted,
  `NodeId`-independent form of every node, edge and relation intent, so two builds of the same
  workspace can be compared with a plain string equality. Exposed as
  `mesh-mcp graph --format fingerprint`.
- **`WorkspaceIndexer::build_snapshot_from_files`**: builds a snapshot from an explicit file
  list (used by the determinism tests to shuffle input order); `crawl_all` is now public.
- **Determinism test suite** (`crates/mesh-server/tests/determinism.rs` + fixture
  `tests/fixtures/determinism/`) covering the four idempotence invariants: same input ⇒ same
  snapshot (thread count, file order, repeated runs), incremental reload ⇒ same snapshot as a
  full rebuild (sequential and concurrent reloads), reconcile idempotence, and one set of
  facts per file under overlapping roots.
- **`scripts/determinism.sh`** and a CI job running it: indexes each workspace 5 times
  sequentially and 8 times concurrently and requires a single fingerprint.

### Known violations (baseline, tracked by `#[ignore]`d tests until the fixing step lands)
Measured with `scripts/determinism.sh` (distinct fingerprints over 13 runs, 5 sequential +
8 concurrent): `examples/polyglot-shop` 1, `examples/volontariapp-fixture` 1, the
determinism fixture 2, Online Boutique 13, OpenTelemetry demo 13, Bank of Anthos 8.
- Results depend on thread count and CPU load (15 ms wall-clock parse timeout) — P0 step 1.3.
- Shuffling the file order, or simply re-running, changes the graph: import and RPC resolution
  keep the first candidate in `HashMap` order — P0 step 1.4.
- A file under two overlapping roots is indexed once per root (18 duplicated nodes on the
  fixture with roots `.` + `services/*`) — P0 step 1.2.
- An incremental reload does not converge to a full rebuild, sequentially or with concurrent
  reloads — P0 steps 1.4 and 1.5.

## [3.1.0] — 2026-09-24

Architectural overhaul for total layout agnosticism, semantic extractor precision, 100% Tree-Sitter Protobuf parsing, and zero-noise codebase hygiene across all crates (`mesh-core`, `mesh-parsers`, `mesh-server`, `mesh-daemon`):

### Added & Overhauled
- **100% Tree-Sitter Protobuf Parser (`crates/mesh-parsers/src/languages/proto.rs`, `decapitate.rs`, `guard.rs`)**:
  - Integrated `tree-sitter-proto` as the 13th native grammar (`LanguageKind::Protobuf`) into `AstGuard` and `AstDecapitator`.
  - Replaced legacy string normalization and semicolon splitting with exact CST traversal (`service_definition`, `rpc`, `message_definition`, `package_statement`).
  - Full support for multi-line and nested custom options (`option (google.api.http) = { ... }`).
  - 100% exact source line numbers (`line_start`/`line_end`) preserved across all Protobuf contract nodes.
- **Agnostic Package & Microservice Boundary Detection (`crates/mesh-core/src/types.rs`)**:
  - Refactored `detect_service_package` to strictly prioritize AST-level declarations (`package`, `namespace`).
  - Implemented compilation boundary discovery via manifest files (`go.mod`, `Cargo.toml`, `package.json`, `pom.xml`, `build.gradle`, `pyproject.toml`).
  - Eliminated arbitrary directory name blacklists (`app`, `module`, `custom`) and prioritized standard service container structures (`services`, `apps`, `packages`, `modules`, `crates`, `libs`).
- **Semantic Multi-Language Extractor Precision (`crates/mesh-parsers/src/languages/`)**:
  - **Python (`python.rs`)**: Multi-decorator support via recursive `decorated_definition` traversal; eliminated false-positive event producers/consumers without confirmed imports.
  - **TypeScript (`typescript.rs`)**: Switched import extraction to AST `child_by_field_name("source")` (eliminating `.find("from")` false positives); expanded full HTTP decorator coverage (`@Delete`, `@Patch`, `@Options`, `@Head`, `@All`).
  - **Go (`go.rs`)**: Enforced parameter inspection (`ResponseWriter`, `Request`, `Context`) before classifying `Handle*` as HTTP endpoints; excluded non-gRPC third-party clients (`redis`, `mongo`, `s3`, `http`) from gRPC RPC detection; filtered generic identifiers from Kafka topic fallback.
  - **C++ (`cpp.rs`)**: Resolved intra-project angle-bracket includes (`#include <billing/service.h>`) without system header pollution.
  - **Java (`java.rs`)**: Eliminated `"unknown.topic"` fallback dummy nodes; expanded REST mapping across Spring (`@PutMapping`, `@DeleteMapping`, `@PatchMapping`) and JAX-RS / Jakarta REST (`@GET`, `@POST`, `@PUT`, `@DELETE`, `@PATCH`, `@Path`) with Lombok `@Getter` isolation.

### Fixed & Hardened
- **Case-Insensitive & Source-Mapped Smart Search (`crates/mesh-server/src/tools/smart_search.rs`)**:
  - Aligned `search_file` case-insensitivity with `search_symbols`.
  - Mapped snippet line spans to the original source file line numbers rather than decapitated AST line coordinates.
- **gRPC FQCN Disambiguation (`crates/mesh-core/src/contracts.rs`)**:
  - Separated FQCN (`proto_by_fqcn`) from bare-name (`proto_by_bare`) indices in `reconcile_edges`, preventing name collisions on generic method names (`Ping`, `Status`).
- **Dynamic RSAH Governance (`crates/mesh-core/src/governance.rs`)**:
  - Fixed case-sensitivity bug in `GovernanceEngine::evaluate_guard`.
  - Replaced hardcoded demo workflows with dynamic, rule-derived `RsahResponse` messages.
- **Word-Boundary Secret Redaction (`crates/mesh-core/src/properties.rs`)**:
  - Refined `is_sensitive_key` with strict word boundaries (`_key$`, `.key$`, `secret`, `password`), preventing false-positive redaction of legitimate properties like `kafka.partition.key` and `app.author.email`.
- **Agnostic Init & Doctor Commands (`crates/mesh-server/src/cli/`)**:
  - `init --auto` now derives stop rule keys from actual existing directories (`proto`, `k8s`, `deploy`) and workspace names from directory identity.
  - Pre-commit hook generator detects interface contracts by file extension (`*.proto`) rather than hardcoded path prefixes.
  - `doctor` eliminated user `Cargo.toml` version drift checks and aligned crawler depth to `SCAN_DEPTH = 10`.
- **Codebase Hygiene**:
  - Removed all internal roadmap markers, obsolete docstrings, and benchmark narrative justifications across all crates.
  - Zero warnings under `cargo clippy --all-targets -- -D warnings`; 255/255 passing tests.

## [3.0.4] — 2026-09-24

Eliminated repository-specific heuristics and overfitted patterns to achieve 100% agnostic, cross-language static analysis across arbitrary monorepos and microservice layouts:

### Fixed & Generalized
- **Universal Multi-Anchor gRPC Resolution (`crates/mesh-core/src/contracts.rs`)**: `analyze_grpc` collects all matching anchors (`Vec<NodeId>`) rather than overwriting a single anchor, resolving callers across multi-service monorepos with duplicate or versioned service names (`v1.AuthService` vs `v2.AuthService`). Symmetrically classifies non-proto `bare_names_match` symbol implementations as server handlers without path substring heuristics.
- **Canonical TypeScript gRPC Extraction (`crates/mesh-parsers/src/languages/typescript.rs`)**: Prioritizes canonical string literals passed to `client.getService('XService')` over generic type parameters. In generic-only fallbacks, cleanly strips namespaces, `Client`/`Stub` suffixes, and `I` interface prefixes (`proto.checkout.ICheckoutServiceClient` -> `CheckoutService`).
- **Agnostic Go Constant Resolution (`crates/mesh-parsers/src/languages/go.rs`)**: Added full support for Go raw string literals (backticks `` `topic` ``), typed consts (`const Topic string = "orders"`), multi-variable bindings (`const A, B = ...`), and Confluent Kafka's nested `TopicPartition{Topic: ...}` structs.
- **Polyglot Kafka Pipelines (`crates/mesh-parsers/src/languages/kotlin.rs`, `csharp.rs`, `mod.rs`)**:
  - Wired `KotlinExtractor::extract_with_relations` directly into indexing dispatch in `mod.rs`, eliminating dead code.
  - Generalized Kotlin constant resolution beyond Elvis expressions to include direct `val`/`const val` string assignments, and hardened argument extraction for `ProducerRecord(...)`.
  - Added semantic guards filtering out HTTP, socket, and mail senders on `.send()`.
  - Implemented native C# Confluent.Kafka extraction (`Produce`, `ProduceAsync`, `Subscribe`) with `CSharpRelations` fully integrated into `mod.rs`.
- **Monorepo Workspace Discovery & Scope Jail Security (`crates/mesh-core/src/security.rs`, `crates/mesh-server/src/cli/init.rs`)**:
  - Formally verified the unidirectional containment invariant of `ValidatedScope` (`canonical_target.starts_with(allowed_root)`), proving that parent container bypasses are strictly rejected as sandbox escapes.
  - Added root workspace marker detection (`pnpm-workspace.yaml`, `nx.json`, `turbo.json`, `lerna.json`, `go.work`) in `init --auto` to automatically root monorepo workspaces and grant safe access to shared workspace dependencies.

## [3.0.3] — 2026-09-24

Fixes found and verified while benchmarking `mesh-mcp` against real-world repositories,
including large single-language repos (linux, rust-lang, vscode, grpc) and real polyglot
microservice monorepos (Google's "Online Boutique" — 11 services, 7 languages, real gRPC
wiring). Every item below was confirmed against actual source (file:line), not inferred
from behavior alone, and each has a regression test reproducing the original failure —
including an end-to-end run against the real Online Boutique clone, not just synthetic
fixtures.

### Fixed
- **Crash**: `mesh-mcp` could abort the whole server process with a native stack overflow
  while indexing a repository containing files with very deep expression/call-chain
  nesting (reproduced on `rust-lang/rust`). `AstDecapitator::collect_body_replacements`
  (`crates/mesh-parsers/src/decapitate.rs`) now carries a `depth` parameter capped at
  `AstGuard::MAX_NESTING_DEPTH`, independent of `AstGuard`'s pre-parse lexical bracket
  count (which is only a proxy for real CST depth and can be passed by files whose actual
  parse tree is still hundreds of levels deep). The same unbounded-recursion pattern was
  present in all 12 language extractors' own AST walkers and has been closed in each.

- **`init --auto` silently dropped detected project roots**: `go.mod`, `pom.xml` /
  `build.gradle`, and `pyproject.toml` / `requirements.txt` detection printed a message
  but never added a root, so `smart_search` and every other tool saw none of that
  project's source unless another marker (commonly `docs/`) happened to populate `roots`
  first. Every detection block in `crates/mesh-server/src/cli/init.rs` now pushes a root
  it detects, and duplicate whole-repo roots are deduplicated before being written out.

- **`init --auto` missed nested per-service language markers entirely**: on a real
  polyglot microservices monorepo where each service's manifest lives one level under a
  container directory (`src/checkoutservice/go.mod`, `src/emailservice/pyproject.toml`,
  ...) rather than at the repo root, no marker check ever fired and the generated config
  indexed nothing but `docs/`. `init --auto` now detects a `src/`/`apps/` container
  holding 2+ per-service language markers and roots the whole container as a glob
  (`./src/*`), the same way the existing `services/`/`packages/` detection already does.
  Confirmed on Online Boutique: went from indexing 0 of 11 services to all 11,
  auto-detected, no manual config editing.

- **`find_dependents` returned false negatives for real cross-service dependencies**:
  three compounding gaps, all closed together since they're on the same code path
  (`ContractGraph::find_dependents`, `crates/mesh-core/src/contracts.rs`):
  - Querying by a declared symbol's own name (the tool's own schema example —
    `"Target contract name (ex: 'UserAuthRequest')"`) silently returned nothing, because
    the underlying lookup only matched literal import-path/package strings. Now bridges a
    symbol-name query through `name_to_nodes`/`fqcn_to_node` to the declaring node's own
    package before falling back further.
  - A real cross-service gRPC caller (e.g. a Go client constructing
    `pb.NewFooServiceClient(conn)`) is linked by a graph edge (`CallsRpc`), not an import
    string at all — no file anywhere imports anything literally named after the service.
    `find_dependents` now also walks `CallsRpc`/`Implements` edges targeting the resolved
    symbol (or any node in its package) and returns their callers.
  - The unscoped substring fallback flattened results across unrelated services that
    happen to reuse the same locally-aliased package name (every service in a monorepo
    vendoring its own `genproto`, for instance) into one undifferentiated list. Results
    are now labeled with the root/service they were crawled from
    (`crates/mesh-server/src/tools/find_dependents.rs`) and rendered grouped by that
    label instead of flattened.
  - Confirmed end-to-end on Online Boutique: `find_dependents("CheckoutService")` went
    from 0 (false negative) to correctly resolving `frontend`'s real caller.

- **The Go extractor never recorded gRPC client-call sites at all**
  (`crates/mesh-parsers/src/languages/go.rs`): a `pb.NewFooServiceClient(conn)`
  construction — the standard `protoc-gen-go-grpc` client constructor — was not
  recognized, so no `CallsRpc` edge could ever exist for a Go caller, independent of the
  `find_dependents` fixes above. Now detected symmetrically with the existing
  `RegisterFooServiceServer` server-side detection, attributed to its smallest enclosing
  declaration, and fed into `ContractGraph::add_rpc_call`.
  `ContractGraph::reconcile_edges`'s `CallsRpc` matching also now indexes every declared
  `GrpcService` node by name (not just proto-file `GrpcMethod` nodes), and
  `analyze_grpc`'s edge resolution no longer requires a `.proto` file to be indexed at
  all — a language's own service-implementation node is a sufficient anchor. Confirmed on
  Online Boutique: `analyze_grpc("CheckoutService")`'s "Client Stubs" went from 0 to
  correctly listing `frontend`'s real caller, with zero `.proto` files in scope.

- **`analyze_grpc` reported false-positive server handlers on generated gRPC stub
  files**: the Python extractor tagged *any* class ending in `Servicer` as a declared
  `GrpcService`, including the shared base class inside a `grpc_tools.protoc`-generated
  `*_pb2_grpc.py` file — vendored identically into every service, whether or not that
  service actually implements it. `crates/mesh-parsers/src/languages/python.rs` no longer
  tags a `*Servicer` class found in a `*_pb2_grpc.py` file. Separately, `analyze_grpc` no
  longer buckets a heuristic (non-exact) name match into `server_handlers`/`client_stubs`
  by a bare `path.contains("service")` check — every service directory in a typical
  microservices repo satisfies that trivially. Only an exact-name match, or a real
  `Implements`/`CallsRpc` graph edge, places a node in either bucket now.

## [3.0.2] - 2026-09-23

### Fixed
- Dual-license claim (`Apache-2.0 OR MIT`, advertised in `Cargo.toml` and the README badge) had
  no corresponding `LICENSE-MIT` file — only Apache-2.0 text existed under `LICENSE`. Renamed
  `LICENSE` to `LICENSE-APACHE` and added `LICENSE-MIT`, following the standard Rust dual-license
  layout. Updated the README badge link and License section accordingly.
- Removed `docs/ROADMAP.md`. It was a closed, all-16-resolved audit trail rather than an open
  backlog, but README's Documentation table described it as "known gaps and planned work" and
  the file itself read as a live list of problems unless you noticed the status banner several
  paragraphs in — misleading for anyone landing on a specific item via a direct link. A finished
  audit belongs in CHANGELOG.md, not a document whose own title is "the product promises things
  it does not do". Removed the corresponding README table row.

## [3.0.1] - 2026-09-23

### Fixed
- `mesh-mcp graph --format html`: the interactive topology's legend hardcoded "Kafka Topic" for
  the shared yellow node color, even though that same color (and, previously, node size) covers
  `NodeKind::EventStream` and `NodeKind::Queue` too — the transport-agnostic kinds introduced
  specifically so Redis Streams/BullMQ/SQS custom-pattern nodes wouldn't be mislabeled as Kafka
  (see `ContractGraph`'s own comment on this in `contracts.rs`). A project with zero real Kafka
  usage had every async event/queue node visually reading as "Kafka Topic" in the legend.
  Relabeled to "Topic / Queue / Stream" and aligned node radius so `EventStream`/`Queue` render
  identically to `KafkaTopic` instead of smaller.

## [3.0.0] - 2026-09-23

This release closes out an audit driven by real consumer usage (a 15-root, ~4000-file polyglot
monorepo) that surfaced several silent config and matching bugs, plus a batch of new language
extractors and quality-of-life engine work. Treated as a major bump because tool output
truncation behavior changes for every tool, not just `visualize_mesh` (see Changed).

### Added
- Ruby, PHP, Swift, and Scala extractors, alongside the existing Java/Go/Python/TypeScript/
  Rust/C++/Kotlin/C# support.
- Parallel VFS indexing and a `tsconfig.json` path-alias resolver for more accurate
  cross-file TypeScript import resolution.
- `[workspace.mount_aliases]` is now actually applied (previously accepted but ignored),
  translating container bind-mount paths (e.g. `/workspace`) to local host roots.
- Dead-configuration detection in `mesh-mcp doctor`: any `[engines.docs].paths`,
  `[engines.contracts.grpc].proto_dirs`, or `[engines.contracts.openapi].spec_files` pattern
  that matches zero scanned files now prints an explicit `⚠ ... matched 0 files — likely dead
  config` warning, instead of silently indexing nothing.
- `McpTool::truncation_hint()`: a per-tool hook that adds contextual guidance when output is
  truncated at the 48 KB payload cap — e.g. `find_dependents` reports the real total dependent
  count, `smart_search` echoes back the query/scope that returned too much.
  Every tool's output is now capped centrally in `ToolRegistry::invoke()` (previously only
  `visualize_mesh` enforced this, despite the README documenting it as universal).
- Build-time git commit hash embedded via `build.rs`, shown in `--version` and at the top of
  `mesh-mcp doctor` output (`v3.0.0, commit: <hash>`), so a stale prebuilt binary is now
  visible instead of silently diverging from the source tree it's run against.
- `examples/volontariapp-fixture/`: a dogfood integration fixture covering the exact real-world
  patterns that broke in production — a `@GrpcMethod` decorator using enum/identifier arguments
  instead of string literals, `ClientGrpc.getService()` client injection, the transactional
  outbox pattern (`EventQueueEntity.createEvent<T>()`), and a `BatchPostProcessor<T>` consumer.
- `scripts/smoke_test.sh`: builds the release binary and runs `doctor`/`graph` against both
  `examples/polyglot-shop` and `examples/volontariapp-fixture` as a release gate. Runnable from
  any working directory.
- Test coverage tying `docs/mcp-tools.md`'s documented JSON schemas to the real `*Args` structs
  per tool (targeting the `#### JSON Schema` block specifically, not example-call snippets), and
  a functional integration test proving `${workspace_root}` in `proto_dirs` actually extracts at
  runtime rather than being silently ignored.

### Fixed
- `${workspace_root}` / `${WORKSPACE_ROOT}` substitution in `[engines.docs].paths` and
  `exclude_patterns` was silently never applied — a literal placeholder string doesn't match any
  real path, so a config written exactly as SETUP.md's own examples showed it indexed zero
  markdown files. Centralized the fix in a single `strip_workspace_root_prefix()` helper
  (`mesh-core::config`), used by `ExcludeMatcher::compile()`, `allows_proto_path()`, and
  `spec_file_matches()` alike, closing the same class of bug across all three fields at once
  instead of patching them independently.
- `exclude_patterns` containing the crawl root's own directory name as a literal path segment
  (e.g. `**/deploy/submodules/**` when `deploy` is itself a configured root) never matched,
  because `crawl_scope()` strips each root's own prefix before matching. `ExcludeMatcher` now
  also checks the root-name-prefixed form of the relative path.
- gRPC handler ↔ proto RPC matching failed for the (extremely common) NestJS pattern of passing
  enum members or imported constants to `@GrpcMethod` instead of string literals — e.g.
  `@GrpcMethod(USER_SERVICE_NAME, UserCommandMethod.SIGN_UP)` produced a garbled node name that
  never matched the proto's bare RPC name `SignUp`, so `analyze_grpc`'s "Server Handlers" section
  silently returned empty for essentially every gRPC handler written this way. Fixed via
  last-identifier-segment extraction plus underscore/case-normalized bare-name matching.
- `[engines.contracts.grpc].proto_dirs` and `[engines.contracts.openapi].spec_files` had the
  same unsubstituted-`${workspace_root}` bug as above, silently disabling proto/OpenAPI
  extraction entirely when configured with the documented canonical syntax.
- `mesh-mcp doctor`'s new `proto_dirs` dead-config check now calls the real
  `ExtractConfig::allows_proto_path()` matcher directly (one dir at a time, for per-directory
  reporting) instead of a second, independently-written copy of the same substring logic that
  could silently drift from the real extraction behavior over time.

### Changed
- `docs/mcp-tools.md`: corrected schemas that no longer matched the real `*Args` structs —
  `analyze_grpc` documented `service_name`/`method_name` (real: single `target` field),
  `analyze_impact` documented `changed_file` (real: single `target`, no `direction` field),
  `find_dependents` documented an optional `scope` (real: no `scope` field at all).
- `SETUP.md`'s "Config reference" table corrected: `[engines.contracts.grpc]`, `.openapi`,
  `[engines.docs] enabled/paths/fuzzy_fallback`, and `[engines.policy] enabled` were documented
  as "accepted but not read by anything" — they are, and have been, wired.
- `AnalyzeGrpcTool`'s description now explicitly states that client-side gRPC call detection is
  supported for Java/Go/Rust but not yet for TypeScript/NestJS `ClientGrpc` usage, rather than
  silently returning an empty "Client Stubs" section with no explanation.

### Known limitations
- TypeScript/NestJS client-side call detection (`this.injectedClient.method()` following a
  `ClientGrpc.getService<X>()` injection) is still not implemented — `analyze_grpc`'s "Client
  Stubs" section will report 0 for this pattern even when a real caller exists. Disclosed in the
  tool description; tracked as future work.
- The filesystem crawler always respects `.gitignore` (`ignore::WalkBuilder::git_ignore(true)`).
  A `proto_dirs`/`spec_files`/`docs.paths` pattern that only matches gitignored, build-generated
  files (e.g. a committed-nowhere OpenAPI spec emitted by a build step) will correctly report as
  matching zero files — this is by design, not a bug, but easy to be surprised by.
