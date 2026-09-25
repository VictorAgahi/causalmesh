# Changelog

All notable changes to MeshMCP (`mesh-mcp` / `meshd`) are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). This file starts
at 3.0.0 — there is no reconstructed history before it.

## [Unreleased]

Plan 2 (P1): make inter-service joins precise, not just deterministic.

### Added (P1 step 2.1 — Python gRPC client detection)
- **`PythonExtractor` now detects gRPC client call sites.** It had server-side (`*Servicer`
  subclass) detection but *no* client-side detection at all — every other language extractor
  (Go's `New<Service>Client(conn)`, TypeScript/C#/Kotlin's client constructions) already emitted
  an RPC-call signal for `find_dependents`/`analyze_grpc`, but a Python gRPC client was invisible.
  New `PythonRelations::rpc_calls` records a `grpc_tools.protoc`-generated
  `<x>_pb2_grpc.<Service>Stub(channel)` construction site, mirroring the same generated-stub
  convention the other languages rely on. Scoped to that qualified attribute-call shape only — a
  bare `<Service>Stub(...)` with no `_pb2_grpc`-module qualifier is deliberately not accepted,
  since a hand-written test double coincidentally named `<Something>Stub` would otherwise
  fabricate an RPC-call edge. A construction with no enclosing function/class (real-world example:
  Online Boutique's recommendationservice wires its client up directly inside
  `if __name__ == "__main__":`) is attributed to a lazily-created module-level node instead of
  being dropped or mis-attributed to whichever `ContractNode` happened to be declared first.
  `online-boutique`'s measured score (`docs/quality.md`) went from 92.9% to **100% recall** on
  its real gRPC edges — the fix closes the exact gap step 2.0's baseline measured, verified against
  the real repo (not just synthetic fixtures) both before and after two rounds of ruthless review.

### Added (P1 step 2.0 — golden corpus and precision/recall baseline)
- **`scripts/golden/{repos.txt,fetch.sh}`**: pins the Plan 2 corpus (`online-boutique`, `otel-demo`,
  `bank-of-anthos`) to exact commits and clones/checks them out reproducibly, distinct from
  `scripts/bench/repos.txt`'s shallow latest-commit clones for scale testing.
- **`tests/golden/online-boutique.expected.yaml`**: hand-written ground truth (from the repo's own
  `.proto` service list and each caller's real client-construction call sites, not derived from
  mesh-mcp's own output) for its 14 real gRPC service-to-service edges.
- **`scripts/golden/score.py`**: runs `mesh-mcp graph --format json` against a golden-corpus repo
  and computes precision/recall of its gRPC edges against the golden file, with optional
  `--fail-under-precision`/`--fail-under-recall` for a future CI ratchet.
- **`docs/quality.md`**: the baseline this step measured — `online-boutique` scores **100%
  precision / 92.9% recall** (one miss: a Python `ServiceStub(channel)` construction site,
  `recommendationservice -> productcatalogservice`, not yet a recognized gRPC client idiom).
  `otel-demo` and `bank-of-anthos` golden files are not yet written — noted as pending, not
  fabricated under time pressure.

## [4.0.0] — 2026-09-25

**Plan 1 (P0) complete: every answer is now reproducible and honest.** Ten steps landed as one
PR apiece (#7–#16), each green on `cargo fmt`/`clippy -D warnings`/`cargo test --workspace` and,
from step 1.1 onward, `scripts/determinism.sh`. IDs, file paths, and several tool output shapes
changed across this range — hence the major version bump — so anything that parsed
`graph --format json` output or depended on `NodeId` values being small sequential integers needs
to re-check those assumptions.

Closing out the plan meant re-running its own original audit scenarios against real workspaces
(`~/bench-repos-micro/{bank-of-anthos,otel-demo,online-boutique}`), not just the synthetic
fixtures each step's own tests used — which is exactly what caught the one gap step 1.7 left open
(below). Every finding the audit raised is now closed:
- **0 duplicate nodes** on an overlapping-roots workspace (Bank of Anthos: 532 → 358 nodes, the
  174 duplicates the audit measured gone entirely, confirmed again here byte-for-byte) — step 1.2.
- **A real v1/v2 service-name homonym resolves stably and fans out to both candidates**, tagged
  `ambiguous`, instead of arbitrarily picking one depending on `HashMap`/thread order — step 1.4.
- **No invented topic/queue names** (`"database is down"`, `"unknown.topic"`, a bare Kafka
  variable's own name, a cross-package constant reference's raw text, ...) survive in any of the
  six extractors the audit flagged, confirmed by re-scanning Bank of Anthos, the OpenTelemetry
  demo, and Online Boutique and inspecting every resulting topic/queue node name by hand — steps
  1.7 and, for one residual case the first pass missed, this step (below).
- **Daemons are isolated per workspace**: two repos open at once never share or race for the same
  socket — step 1.8.
- **No plaintext secret** returned in a `smart_search` snippet sitting next to a matched symbol —
  step 1.9.
- Every measured workspace (`examples/*`, the determinism fixture, and the three real repos above)
  produces exactly one content fingerprint across repeated sequential and concurrent
  `mesh-mcp graph --format fingerprint` runs.

Plan 2 (P1, precision of inter-service joins) and Plan 3 (P2, scale) remain, per the original
three-phase roadmap, and have not started.

### Fixed (end-of-Plan-1 verification, step 1.7 follow-up)
- **A Go composite-literal struct's `Topic` field referencing an unresolvable expression
  (`kafka.Message{Topic: kafka.Topic, ...}`, a cross-package/cross-file constant this extractor
  can't reach) no longer falls back to the raw expression text.** This is the one instance of
  step 1.7's "zero invented values" goal the first pass missed — caught only by indexing the real
  OpenTelemetry demo rather than a synthetic fixture, where `checkout/main.go`'s
  `Topic: kafka.Topic` produced a literal `kafka.topic` node. `extract_topic_value_text` now
  records nothing instead, matching the convention every other language's extractor already
  follows.
- **The same file's generic producer/consumer call detection (`producer.Produce(&kafka.Message{
  ...})`) no longer recurses into a named struct literal's *other* fields looking for any string
  it can find.** `collect_string_literals` walked the entire argument tree indiscriminately, so a
  call like `producer.Produce(&kafka.Message{Value: []byte("payload")})` (no resolvable topic at
  all) picked up `"payload"` — the `Value` field's own literal, an entirely unrelated field — as
  if it were the topic. It now stops descending at any *named* struct literal's boundary (an
  anonymous one like `[]string{"orders"}` is still walked, since that shape has no dedicated
  field-aware extraction of its own).

### Fixed (P0 step 1.9 — reliable `init --auto`, secret hygiene)
- **`init --auto`'s API Gateway detection no longer roots a nonexistent path.** It checked
  `api-gateway/ || gateway/` but always pushed the literal string `./api-gateway` regardless of
  which one actually existed — a repo with only a plain `gateway/` directory got a root that could
  never match anything once the generated config was loaded. Now pushes whichever directory is
  actually present.
- **The architecture-docs root detection had the identical bug**, always pushing `./docs`
  regardless of whether `docs/` or `architecture/` was the one that existed. Same fix.
- **`protos/` (plural) is now detected as a proto root**, matching `proto/` and `proto-registry/`
  — the roots-detection block only checked the singular and `-registry` forms, inconsistent with
  the stop-rules block a few lines below it, which already checked all three.
- **`smart_search` no longer returns a plaintext secret sitting next to a matched symbol.** Its
  snippets are raw source lines, not resolved config properties, so a `.yaml`/`.env`-style line
  like `POSTGRES_PASSWORD: accounts-pwd` right next to what the query matched came back to the
  caller verbatim — the exact case the audit measured. Each returned line is now checked against
  `PropertyRegistry::is_sensitive_key`'s existing patterns (the same ones already used to redact
  *resolved* config values) and its value masked with the same `REDACTED_SECRET` placeholder if
  the key looks sensitive.

Default secret-file exclusions (`**/.env*`, `**/*.pem`, `**/*.key`, ...) were checked against this
step's scope and found already in effect: `WorkspaceConfig::exclude_patterns`'s serde default
applies them whenever a config (including every `init`-generated one, which never writes this
field) doesn't set its own — no `init.rs` change was needed there.

Content-based generated-code detection (masking `.pb.go`/`_pb2.py`-style generated files from
tool output by default, per the roadmap) is deferred: those files are also where gRPC
server/client stub extraction actually lives today, so hiding them outright would regress
`analyze_grpc`/`find_dependents` rather than just improve hygiene — it needs a real per-file
provenance tag surfaced at the *output* layer (Plan 2 territory), not a blanket skip at indexing
time.

### Added (P0 step 1.8 — one `meshd` per workspace)
- **`mesh_core::socket::workspace_id(base_dir)`**: the first 16 hex characters of
  SHA-256(canonical `base_dir` + `CARGO_PKG_VERSION`) — a short, stable identifier for one
  workspace at one binary version.
- **`socket_path_for(workspace_id)`** / **`pipe_name_for(workspace_id)`**: workspace-scoped
  socket/pipe resolution, alongside the existing workspace-agnostic `socket_path()`/`pipe_name()`
  (kept for `MESH_SOCKET_PATH`-style overrides and standalone/test use).

### Fixed (P0 step 1.8)
- **Two unrelated workspaces can no longer share (or race to bind) the same daemon.** Before this
  fix, every `meshd` on a machine bound the same one-per-user socket
  (`~/.cache/mesh/meshd.sock`) regardless of which workspace it indexed — opening two different
  repos in two IDE windows raced to bind it, and whichever lost silently had its `mesh-mcp`
  sessions served by the *other* repo's daemon and data (idempotence invariant I7). `mesh-mcp run`
  now discovers its config first, derives `workspace_id`, and resolves its daemon at
  `socket_path_for(workspace_id)`; when spawning `meshd`, it passes `--socket <that path>` and
  `.current_dir(<canonical base>)` explicitly instead of relying on inherited cwd/environment to
  land on the right workspace. Upgrading the binary changes `workspace_id` too, so a stale daemon
  from before an upgrade is simply never found again rather than serving newer clients against an
  outdated snapshot format.
- **`meshd` opens its socket before ingesting, not after.** Initial ingestion used to run
  synchronously before the socket bound at all, so on a large workspace a client polling for the
  daemon to come up (`mesh-mcp`'s `ensure_daemon_running`, 500ms budget) would reliably time out
  and fall back to standalone mode — spinning up a second, redundant in-process index right as
  the daemon it gave up on finished its own (the "double indexing" the roadmap called out).
  Ingestion now runs in the background (under `AppState::reload_lock`, the same lock every later
  `reload` takes, so a filesystem event racing the initial scan still can't install a stale
  snapshot over it — invariant I2); the socket accepts connections immediately, and `initialize`/
  `ping` succeed right away. A `tools/call` made before that first scan installs its snapshot
  (`generation == 0`, a state a legitimately-indexed-and-empty workspace can never be in — it
  always reaches generation 1) now gets an explicit "still indexing this workspace; retry
  shortly" error instead of an answer computed against the still-default empty snapshot.

Regression tests: `workspace_id` differs by base dir and is stable for the same one;
`tools/call` reports "still indexing" before the first snapshot installs but `initialize`
doesn't wait on it; two `meshd`s bound to two different workspace-scoped sockets never answer
for each other, even with intentionally similar workspace names/config.

### Fixed (P0 step 1.7 — zero invented values)
Six language extractors had a fallback path that turned an arbitrary expression — a
variable's own name, an unrelated statement's string literal, one named param's value
mistaken for another's — into a fabricated topic/queue name instead of recording nothing.
Every one of these now records no topic at all when it can't resolve to a genuine literal
(or, for Kotlin/Java's `@KafkaListener`, falls back to the function's own name — an existing,
already-used convention — instead of inventing a value):

- **Go**: `WriteMessages`/`SendMessage`/`Produce`/`ReadMessage`/etc. calls with no
  string-literal argument used to record the *receiver's own identifier* as the topic
  (`reader.ReadMessage(ctx)` recorded a topic named `"reader"`).
- **Kotlin**: `extract_annotation_param` returned the fabricated literal `"unknown.topic"`
  when `@KafkaListener`'s `topics` param wasn't a plain string; `extract_single_topic_arg`
  emitted a raw `identifier` or `navigation_expression`'s text verbatim. An identifier is
  still resolved against `string_defaults` (a real `val topic = "..."` in the same file) —
  only a *variable* with no resolvable value now drops the signal, instead of falling back
  to using the variable's own name.
- **C#**: `extract_topic_arg`/`extract_single_topic_arg` had the same bare-`identifier`
  fallback as Kotlin's, but with no constant-propagation pass to resolve it against — removed
  outright rather than given a resolution path.
- **TypeScript**: `extract_value_text` returned the raw source text of any non-string
  argument (an identifier, a member expression like `config.topic`, a template literal with
  interpolation) as if it were the topic/queue name.
- **Java**: `extract_annotation_param`'s positional-literal fallback used to search from the
  *first* `(` to the *last* `)` across the whole concatenated multi-annotation blob on a
  method, so a preceding, unrelated annotation's string argument could be picked up as the
  Kafka topic; a new `extract_annotation_text` isolates just `@KafkaListener(...)`'s own
  balanced parens first, and the fallback itself no longer fires when the annotation body has
  a named parameter (e.g. `groupId = "..."`) that isn't the one being looked for.
  `extract_kafka_producer_topic` used to search for the first `"` anywhere in the *rest of the
  method text* after `.send(`, unbounded by that call's own closing paren — an unrelated
  string literal in a later statement (a log message, say) could be picked up as the topic;
  now bounded to the call's own argument list, first-argument position only.
- **Python**: `find_assignment_in_root` called `string_literal_value` on an assignment's
  right-hand side without checking it was actually a string node first. `string_literal_value`
  finds the first and last quote characters in a node's *raw source text* — so
  `TOPIC = os.getenv('KAFKA_TOPIC')` (a call, not a string) had its text scanned regardless,
  finding the quotes around `getenv`'s own argument and returning `"KAFKA_TOPIC"` — the
  *environment variable's key* — as if it were the resolved topic value.

Also: `MarkdownFormatter::format_dependents` no longer claims "O(1) in-memory resolution" —
`find_dependents` includes a linear substring-fallback scan (see the step 1.6 fixes above),
so the claim was never accurate.

Regression tests were added for every fix above, each constructing the exact non-literal shape
that used to be silently accepted as a real value.

### Fixed (P0 step 1.6)
- **`ContractGraph::find_dependents`'s substring fallback (step 3) is now sorted by matched
  package name.** It used to iterate `reverse_deps` — a `HashMap` — directly, so the returned
  node order depended on the process's random hash seed instead of workspace content, silently
  violating idempotence invariant I5 (same query on the same snapshot ⇒ byte-identical output)
  every time this fallback path was hit.
- **`ContractGraph::analyze_impact`'s topic-registry pass is now sorted by matched topic name.**
  Same root cause: `topic_producers`/`topic_consumers` are `HashMap`s, iterated directly, so
  `upstream_producers`/`downstream_consumers` order was randomized per process run.
- **`ContractGraph::search_symbols`'s substring path is now sorted by file path.** It walked
  `file_to_nodes` — a `HashMap` — directly to restrict the scan to in-scope files; sorted the
  paths first instead.
- **`WebGraphPayload` (JSON/Mermaid graph export) nodes are now sorted by `(file_path,
  line_start, name)`** instead of left in internal `NodeId` order. `NodeId` is a content hash
  (P0 step 1.4): stable across runs of the *same* workspace, but this export would still have
  silently reordered itself if the hashing scheme ever changed, even though nothing about the
  workspace did.
- **`MarkdownFormatter::extract_sub_scope` no longer emits a bogus scope like `/Users`** for an
  absolute path. It counted `Path::components()` positionally, so an absolute path's leading
  `RootDir` component (the `/` itself) counted as "component 0," pushing the real top-level
  directory out of the truncation-guidance scope label entirely. Only `Normal` components are
  counted now.
- **Tied sub-scopes in `MarkdownFormatter`'s truncation guidance are now ordered by name.**
  `build_truncated_search_output` sorted scopes by match count only
  (`sort_by_key(Reverse(count))`); ties fell back to `HashMap` iteration order, another
  per-process-random ordering, for what should be a fully reproducible ranking.
- **`DocIndex::search` now tie-breaks explicitly by `(file_path, start_line)`** instead of
  relying on `sort_by_key`'s stability to preserve `self.sections`'s insertion order — a
  deterministic result should not depend on an incidental property of the sort algorithm rather
  than an explicit, documented rule.

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

### Added (P0 step 1.3)
- **`IndexHealth`** (`mesh-core::health`): counts of what happened to every file since the last
  full rebuild — scanned, indexed, rejected (oversized / lexical guard / unreadable), and
  genuinely parse-failed. Carried on `MeshSnapshot::health`, merged onto the snapshot by every
  incremental reload. `mesh-mcp doctor` and tool output can now say a scan wasn't fully healthy
  instead of a failed file being indistinguishable from a legitimately empty one (idempotence
  invariant I6).
- `FileIndex::parse_failed`: set by `PolyglotIndexer::extract_with_config` when a language's
  tree-sitter parse itself failed, as opposed to `nodes` legitimately being empty.
- `AstGuard::parse_with` / `ParseOutcome`: the indexing-time parse primitive, with its own
  timeout budget and thread-local parser cache, independent of `AstGuard::with_parser` (still
  used, unchanged, for on-demand decapitation).

### Fixed
- **Overlapping configured roots no longer double-index files** (P0 step 1.2). A workspace
  configured as `roots = [".", "./services/*"]` — the shape `init --auto` itself generates for
  a polyglot service container — used to crawl every file under `services/*` twice: once from
  `.`, once from its own service root, each time under a different `repo_id`. Fixed at the
  source: `WorkspaceIndexer::crawl_all` now excludes every nested root's subtree from its
  ancestors' crawls, so each file is attributed to its single most specific containing root.
  Confirmed on a real capture of Bank of Anthos: 532 → 358 nodes (the 174 duplicates the audit
  measured are gone); `overlapping_roots_index_each_file_once` no longer needs `#[ignore]`.
- `FilesystemCrawler::crawl_scope_with` now sorts each directory's entries by filename
  (`ignore::WalkBuilder::sort_by_file_name`) instead of relying on the OS's readdir order,
  which differs across filesystems and isn't guaranteed stable on any of them.
- `mesh-mcp doctor` reports overlapping configured roots (which one is redundant, which one
  wins the shared files) instead of only validating that every root resolves.
- **The 15ms wall-clock indexing parse timeout, the single largest cause of the audit's
  nondeterminism finding, is gone** (P0 step 1.3). It made indexing outcomes depend on CPU
  contention: under 4-worker Rayon load, ordinary files tripped it, and *which* files tripped
  it depended on scheduling. Replaced with two budgets that were never one constant to begin
  with: `AstGuard::INDEX_PARSE_TIMEOUT_MICROS` (2s — a hang guard for the 13 tree-sitter
  extractors' entry points, which now take an already-parsed `&Tree` instead of parsing inside
  a `&mut Parser` themselves, via the new `AstGuard::parse_with`) for indexing, and
  `AstGuard::QUERY_PARSE_TIMEOUT_MICROS` (500ms, unchanged in effect) for `smart_search`'s
  on-demand decapitation, which must stay responsive on an agent's request path. A file whose
  parse still fails gets one sequential retry outside the contended pool
  (`WorkspaceIndexer::run_scan_pass`) before being counted into `IndexHealth`; on an incremental
  reload, a still-failing file keeps its last known-good facts and is retried on the next
  reload, instead of having its facts silently wiped to empty.

### Added (P0 step 1.4)
- **`EdgeConfidence::Ambiguous`**: when multiple candidates genuinely tie for an import, an RPC
  call, or a bare gRPC service name — no repo/package tiebreak distinguishes them — the graph
  now emits one edge to *every* tied candidate, tagged `Ambiguous`, instead of silently picking
  whichever one a `HashMap` or file-processing order happened to list first. That pick used to
  be both a hidden source of nondeterminism and, for a real homonym (two proto packages each
  declaring their own `AdminService`), a silently wrong answer.
- `ContractGraph::pick_or_ambiguous` / `pick_or_ambiguous_by_package`: the shared "one winner or
  fan out to every tie" logic behind the above, grouping by `repo_id` (imports) or by the
  caller's `package` (RPC calls).

### Fixed (P0 step 1.4)
- **`ContractGraph::nodes` is now a `BTreeMap`, not a `HashMap`.** Std's `HashMap` uses a
  per-process-random `SipHash` seed, so every `self.nodes.values()` scan used for "pick a
  matching candidate" logic (`analyze_grpc`'s anchor search, the proto-method/handler index
  built by `reconcile_edges`) visited nodes in a *different order on every process run*, even
  for byte-identical input on one thread. This was the dominant remaining cause of the
  nondeterminism the audit measured — larger than the timeout (step 1.3) or overlapping roots
  (step 1.2) — and is why sequential, single-threaded reruns of `mesh-mcp graph --format
  fingerprint` on Online Boutique produced 5 different fingerprints in 5 runs before this fix,
  with no concurrency involved at all. `BTreeMap` iterates in sorted, deterministic `NodeId`
  order instead.
- **`reconcile_edges` fully clears and rebuilds the edge set from raw per-node facts on every
  call**, instead of resolving each `Imports` edge once and then only mutating around the edges
  of that. An incremental reload only re-folds the *changed* files' facts, so an unchanged
  file's edge to a target in a just-reindexed file used to go stale or vanish outright —
  silently, since that unchanged file was never touched this cycle to notice.
  `ContractGraph::add_dependency` no longer materializes a placeholder edge at all; it only
  records the raw `reverse_deps` fact, which survives an unrelated file's reindex and is what
  `reconcile_edges` now reads from directly, every time.
- **Synthetic topic hub nodes are garbage-collected.** A hub node `reconcile_edges` creates for
  a topic (e.g. a Kafka topic literal) used to live forever once created, even after every
  producer and consumer of that topic stopped existing (the file was edited to use a different
  topic, or deleted) — a full rebuild from the same current facts would never produce that node,
  so an incremental reload permanently diverged from one. `reconcile_edges` now removes any
  `event-bus`-package hub node whose topic no longer has a producer or consumer fact backing it.
- **`PropertyRegistry` no longer resolves a `${...}` placeholder in place, destructively.** Once
  a key resolved once, its own literal template was overwritten and lost, so a *different* key
  it depended on (Spring's `${app.kafka.topic}`-style cross-file references) changing on a later
  incremental reload could never be re-resolved against the new value — the stale resolved
  string stuck around forever. `PropertyRegistry` now keeps the ingested value in a separate
  `raw_values` map and derives `flat_properties` fresh from it on every
  `resolve_all_placeholders()` call; a key already redacted to `REDACTED_SECRET` is left alone
  (never re-derived from its own raw value, which would leak it back out).

### Added (P0 step 1.5)
- **`AppState::reload_lock`**: a `Mutex<()>` held for the full duration of one
  `WorkspaceIndexer::reload` call, acquired and released entirely on the single Rayon-pool
  thread running that reload (a `MutexGuard` never crosses a thread boundary here). Two reload
  jobs can still both get spawned — `reload_pending`'s coalescing check in
  `FileWatcherService::schedule_reload` is a best-effort optimization, not the correctness
  guarantee — but the second one now blocks on this lock until the first finishes, then runs
  its own pass against then-current disk state, instead of racing it.

### Fixed (P0 step 1.5)
- **Concurrent reloads no longer clobber each other with a stale snapshot.** Before this fix, two
  reload jobs triggered close together (a burst of filesystem events) could both crawl, both
  build a graph, and both call `install_snapshot`; whichever one finished last won, even if it
  had started from an older `snapshot_clone()` base and therefore produced a snapshot with
  *less* information than the one already installed. `reload_lock` serializes the whole
  crawl-build-install sequence so at most one reload ever mutates the snapshot at a time —
  `concurrent_reloads_converge_to_full_build` no longer needs `#[ignore]`.
- **`WorkspaceIndexer::build_graph` now delegates to `build_snapshot`** instead of duplicating a
  second, independently-maintained indexing path. The CLI's `mesh-mcp graph` command, the
  server, and the daemon now all go through exactly one code path from files to graph, so a fix
  to `build_snapshot` (steps 1.1–1.5) can no longer silently fail to apply to one of the others.

### Known violations (baseline, tracked by `#[ignore]`d tests until the fixing step lands)
Measured with `scripts/determinism.sh` (distinct fingerprints over 13 runs, 5 sequential +
8 concurrent). Progression through steps 1.2–1.5:

| Workspace | Before P0 | After 1.2 | After 1.3 | After 1.4 | After 1.5 |
| :--- | :---: | :---: | :---: | :---: | :---: |
| `examples/polyglot-shop` | 1 | 1 | 1 | 1 | 1 |
| `examples/volontariapp-fixture` | 1 | 1 | 1 | 1 | 1 |
| determinism fixture (homonyms, imports, Kafka, properties) | 2 | 2 | 2 | 1 | 1 |
| Bank of Anthos | 8 | 3 | 1 | 1 | 1 |
| OpenTelemetry demo | 13 | 13 | 6 | 1 | 1 |
| Online Boutique | 13 | 13 | 13 | 1 | 1 |

**Every workspace is now fully deterministic, and all of Plan 1's idempotence invariants
(I1–I3) fully hold**: same input ⇒ same snapshot regardless of thread count, file order, or
repeated runs (I1); an incremental reload converges to the same graph as a full rebuild, even
when reload jobs race each other (I2); re-reconciling an already-reconciled graph is a no-op
(I3). `cargo test -p mesh-server --test determinism` now passes all 7 tests with zero
`#[ignore]` remaining, and the `determinism` CI job (`.github/workflows/ci.yml`) is blocking
instead of `continue-on-error`.

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
