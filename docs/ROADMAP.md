# MeshMCP Roadmap

Backlog of work needed for the product to deliver what it advertises. Every item was verified
against the code at commit `27520b4`; each one names the files to touch and what "done" means, so
it can be picked up cold by a person or an agent without re-deriving the analysis.

**How to use this file**: pick an item, read its *Evidence* to confirm it still holds, implement
against *Definition of done*. Items are independent unless *Depends on* says otherwise.

Effort scale: **S** ≈ half a day · **M** ≈ 1–3 days · **L** ≈ a week or more.

**Status as of 2026-09-23**: 15 of 16 items resolved and merged. Item 7 is partial — Kotlin and
C# extractors are done; Ruby, PHP, Swift and Scala are not started. Each item below carries a
**Status** line; the *Problem*/*Evidence* text below it is left as the historical record of what
was true at `27520b4`, not updated to describe the fix — read the linked source for current
behaviour.

| Priority | Theme | Items |
| :--- | :--- | :--- |
| P0 | The product promises things it does not do | [1](#1-config-surface-is-largely-decorative) ✅ [2](#2-find_dependents-only-works-for-typescript) ✅ [3](#3-analyze_impact-natively-covers-java-and-asyncapi-only) ✅ [4](#4-rsah-governance-has-no-production-caller) ✅ |
| P1 | Extraction quality | [5](#5-language-extractors-are-uneven) ✅ [6](#6-no-type-resolution--the-graph-is-name-matching) ✅ [7](#7-missing-languages) ⚠️ partial |
| P2 | Robustness and correctness | [8](#8-mesh-daemon-is-under-tested) ✅ [9](#9-audit-hash-chain-does-not-cover-all-stored-fields) ✅ [10](#10-propertyregistry-has-no-per-file-provenance) ✅ [11](#11-dispatchesto-generation-is-quadratic-per-topic) ✅ [12](#12-println-in-cligraphrs-contradicts-commandment-3) ✅ |
| P3 | Ecosystem and polish | [13](#13-windows-is-a-second-class-target) ✅ [14](#14-rfc-001-has-drifted-from-the-implementation) ✅ [15](#15-no-local-telemetry-despite-having-the-data) ✅ [16](#16-smart_search-results-are-unranked) ✅ |

---

## P0 — The product promises things it does not do

These are the items where a user follows the documentation, sees no error, and gets nothing.
That is worse than an unimplemented feature, because it costs trust in everything else.

### 1. Config surface is largely decorative

**Status: ✅ Resolved.** All keys listed below are now wired: `enabled` flags gate their engine
in `WorkspaceIndexer::process_file`; `mount_aliases` threads through every production
`ValidatedScope::resolve_with_aliases` call site; the grpc/spring/openapi/asyncapi knobs feed
`ExtractConfig`/`SpringSettings`; `enforce_git_hooks`/`cryptographic_audit_trail` gate hook
install and audit writes; `docs.paths`/`fuzzy_fallback` scope indexing and add edit-distance
fallback search. No config key remains unread.

**Problem.** Around 18 of ~25 configuration keys parse successfully and are then read by nothing.
`#[serde(deny_unknown_fields)]` means a typo fails loudly, which trains users to believe that a
config which *parses* is a config which *works*.

Partially fixed in `ed81510`: `[engines.docs]` `aliases` / `stop_words` / `exact_phrase_boost` /
`sanitize_prompt_injections` are now applied (`DocIndex::new` previously had exactly one
call-site — a unit test). The rest is documented as *accepted but not wired* in
[SETUP.md](../SETUP.md#config-reference) and still needs a decision.

**Evidence.** For each key below, `grep -rn "\.<key>\b" crates --include='*.rs'` returns hits only
in `config.rs` (the declaration itself):

| Key | Current reality |
| :--- | :--- |
| `[workspace.mount_aliases]` | `ValidatedScope::resolve_with_aliases` exists (`mesh-core/src/security.rs:51`) but every production call goes through `resolve()`, which passes an empty map (`security.rs:47`). Container path translation does not happen. |
| `[engines.contracts.grpc]` `proto_dirs`, `controller_annotations`, `canonical_fqcn_projection` | Never read. `.proto` files are detected by extension; TS decorators are hardcoded in the extractor. |
| `[engines.contracts.spring]` `property_files`, `resolve_placeholders`, `auto_redact_secrets` | Never read. Redaction is unconditional in `PropertyRegistry::insert_sanitized`. |
| `[engines.contracts.openapi]` / `.asyncapi` `spec_files`, `infer_string_topics` | Never read. Detection is by filename and content sniffing in `extract_yaml_contracts`. |
| `enabled` (every engine) | Never read. No engine can be switched off. |
| `[engines.policy]` `enforce_git_hooks`, `cryptographic_audit_trail` | Never read. The audit log is always written; hooks install only via `mesh-mcp install-hooks`. |
| `[engines.docs]` `paths`, `fuzzy_fallback` | Never read. Markdown is indexed because it falls under `roots`. |

**Decision required** — per key, one of:

- **Wire it.** Cheapest wins: `enabled` flags (skip the engine in `WorkspaceIndexer::process_file`),
  `mount_aliases` (thread the table from `AppState` into every `ValidatedScope::resolve` call site).
- **Delete it from the schema.** Honest, and `deny_unknown_fields` will then tell users their key
  is gone rather than silently ignoring it. Requires a migration note, since existing configs
  containing the key would start failing to parse.

**Definition of done.** No key exists in `config.rs` that no other module reads. A test asserts
this — e.g. a config setting every flag to a non-default value must produce observably different
behaviour, or the key must be absent from the struct.

**Effort.** `enabled` + `mount_aliases`: **M**. Full sweep with deletions and migration note: **L**.

**Watch out.** Deleting keys is a breaking change for anyone with the example `mesh-mcp.toml`.
Consider one release that warns (`#[serde(default)]` + a `doctor` note) before one that rejects.

---

### 2. `find_dependents` only works for TypeScript

**Status: ✅ Resolved.** Java, Go, Python, Rust and C++ import extractors now populate
`FileIndex.dependencies`, verified through the real `PolyglotIndexer::extract` dispatch (not
just each extractor's own unit tests — see `test_polyglot_indexer_wires_*_dependency` in
`crates/mesh-parsers/src/languages/mod.rs`).

**Problem.** One of the four questions the README leads with is "who breaks if I change this?".
For a Java, Go, Python, Rust or C++ workspace, `find_dependents` returns empty — always.

**Evidence.** `FileIndex.dependencies` is populated at exactly one place, inside the TypeScript
branch: `crates/mesh-parsers/src/languages/mod.rs:200`. Custom regex patterns
(`extract_custom_patterns`) produce producers, consumers, sagas and RPC calls — never
dependencies. So `ContractGraph::reverse_deps` is fed by TypeScript imports alone.

**Work.** Add import extraction to the remaining extractors, emitting into `FileIndex.dependencies`
the same way the TS branch does:

| Language | Construct to extract |
| :--- | :--- |
| Java | `import a.b.C;`, `import static`, wildcard `import a.b.*` |
| Go | `import ( "module/path" )`, including named and blank imports |
| Python | `import x`, `from x import y`, relative `from .x import y` |
| Rust | `use a::b::C;`, nested groups `use a::{b, c::D};`, `extern crate` |
| C++ | `#include "local.hpp"` (quoted only — angle-bracket includes are system headers) |

Reuse the TS "is it actually used in this node's line range?" heuristic (`languages/mod.rs:185-205`)
so a file-level import does not attach to every symbol in the file.

**Definition of done.** For each language, an integration test: two files, one importing a symbol
declared in the other, `find_dependents` on that symbol returns the consumer. The README's
"TypeScript/JavaScript only" caveat is removed.

**Effort.** **M** per language, **L** for all five. Do Java and Go first — they cover most
polyglot backends.

**Watch out.** `resolve_import_target` (`mesh-core/src/contracts.rs`) resolves by symbol name,
package name, relative stem, and `package.Name`. Java's fully-qualified imports and Rust's `::`
paths may need an extra resolution strategy there; add it in one place rather than per extractor.

---

### 3. `analyze_impact` natively covers Java and AsyncAPI only

**Status: ✅ Resolved.** Native producer/consumer detection added for Go (kafka-go, sarama,
confluent-kafka-go), Python (confluent_kafka, aiokafka, Celery), Rust (rdkafka), TypeScript
(kafkajs, `@nestjs/microservices`, BullMQ), plus Java `KafkaTemplate.send` on the producer
side. Non-literal topic names emit the variable/expression text rather than being dropped.

**Problem.** Event tracing across services — the flagship feature for event-driven architectures —
has native detection for Java `@KafkaListener` and AsyncAPI `channels`. Everything else requires
the user to hand-write regex patterns in `[[engines.contracts.patterns]]`.

**Evidence.** `FileIndex.consumers` / `.producers` are populated only at:
- `languages/mod.rs:151` — Java, `NodeKind::KafkaTopic` nodes → consumers
- `extract_yaml_contracts` — AsyncAPI `channels` → producers
- `extract_custom_patterns:330,333` — user-supplied regexes

Go, Rust, TypeScript, Python and C++ contribute nothing natively.

**Work.** Add native producer/consumer detection per language, for the dominant client libraries:

| Language | Libraries worth detecting |
| :--- | :--- |
| Go | `segmentio/kafka-go`, `Shopify/sarama`, `confluent-kafka-go` — `ReadMessage`, `WriteMessages`, topic literals in `ReaderConfig`/`WriterConfig` |
| TypeScript | `kafkajs` (`consumer.subscribe({topic})`, `producer.send({topic})`), `@nestjs/microservices` `@EventPattern` / `@MessagePattern`, BullMQ `new Queue('name')` |
| Python | `confluent_kafka`, `aiokafka`, Celery `@task`, `@app.task` |
| Rust | `rdkafka` `subscribe(&["topic"])`, `send(FutureRecord::to("topic"))` |
| Java (extend) | `@KafkaListener` is covered; add `KafkaTemplate.send(...)` for the producer side |

**Definition of done.** For each language, an integration test where a producer in language A and
a consumer in language B are linked by `analyze_impact` with **no** custom pattern configured.

**Effort.** **M** per language. Go and TypeScript first.

**Watch out.** Topic names are frequently constants or config lookups, not literals. Extract the
literal case, and do not silently miss the rest — consider emitting a node with the variable name
so the agent can at least see that a topic is used there.

---

### 4. RSAH governance has no production caller

**Status: ✅ Resolved — option (a), wired.** `McpTool::mutates()` (default `false`) gates a
call to `evaluate_guard` inside `ToolRegistry::invoke`, returning `GOVERNANCE_BLOCKED_CODE`
(`-32001`) on a hit. Every shipped tool is read-only, so this never fires in practice today —
see [docs/governance-rsah.md](governance-rsah.md) for the honest scope. The French
`message_to_user` strings were also translated to English since they are now reachable
through a production path.

**Problem.** `GovernanceEngine::evaluate_guard` (`mesh-core/src/governance.rs:75`) builds complete
RSAH refusal envelopes — status, policy, four-step required workflow, user-facing message — and is
reachable only from tests. Actual enforcement is the git pre-commit hook
(`mesh-server/src/cli/hooks.rs`), which reimplements a narrower rule in shell.

**Evidence.** `grep -rn "evaluate_guard" crates --include='*.rs'` returns the definition, a unit
test, and one integration test. No tool, no server path.

**Also.** The hardcoded `message_to_user` strings in `build_rsah_response` are in French while the
rest of the product is in English. If these become user-visible, that needs resolving.

**Decision required.** Either:

- **(a) Wire it.** The natural place mirrors the skills hint: `ToolRegistry::invoke` already computes
  `T::subject(&args)`. Call `evaluate_guard(subject)` and, on a hit, return the RSAH as a structured
  error or prepend it to the output. Note that read-only queries must stay allowed — the existing
  comment in `smart_search.rs:67-69` is explicit that governance targets mutations, and MeshMCP has
  no mutation tools today. So the honest version may be **(b)**.
- **(b) Acknowledge it as hook-only** and delete the unused envelope-building code, keeping the
  pre-commit hook as the single enforcement point. The docs already say this
  ([governance-rsah.md](governance-rsah.md)); the dead code is what remains.

**Definition of done.** Either an integration test showing a tool call returning an RSAH refusal,
or `evaluate_guard` and `RsahResponse` removed with the hook documented as the sole mechanism.

**Effort.** **S** either way.

---

## P1 — Extraction quality

### 5. Language extractors are uneven

**Status: ✅ Resolved for the languages listed.** Rust gained tonic (`GrpcService`/
`GrpcMethod`), axum/actix-web (`HttpEndpoint`), and trait→`Interface` — previously
`ServiceClass` only. Go gained gRPC server-registration detection and chi/gin/echo route
recognition. C++ gained `::grpc::Service` subclass detection.

**Problem.** Extractor coverage varies wildly, so graph richness depends on which language a
service happens to be written in.

**Evidence.** `NodeKind` variants emitted per extractor:

| Extractor | Emits |
| :--- | :--- |
| `java.rs` | `GrpcService`, `HttpEndpoint`, `Interface`, `KafkaTopic`, `ServiceClass` |
| `proto.rs` | `GrpcMethod`, `GrpcService`, `ProtoMessage` |
| `python.rs` | `GrpcService`, `HttpEndpoint`, `Interface`, `Queue`, `ServiceClass` |
| `typescript.rs` | `GrpcMethod`, `HttpEndpoint`, `Interface`, `ServiceClass` |
| `go.rs` | `HttpEndpoint`, `Interface`, `ServiceClass` |
| `cpp.rs` | `HttpEndpoint`, `Interface`, `ServiceClass` |
| `rust_lang.rs` | `ServiceClass` **only** |

**Work.**
- **Rust**: `tonic` (`#[tonic::async_trait]` impls, `Service` trait impls generated from proto) →
  `GrpcService` / `GrpcMethod`; `axum` (`Router::route("/path", get(handler))`), `actix-web`
  (`#[get("/path")]`) → `HttpEndpoint`; `trait` declarations → `Interface`.
- **Go**: gRPC server registration (`RegisterXServer`), `chi` / `gin` / `echo` route declarations.
- **C++**: gRPC `::grpc::Service` subclasses.

**Definition of done.** A parity table in `docs/development.md` listing which constructs each
language extractor recognises, backed by a test per row.

**Effort.** **M** per language.

---

### 6. No type resolution — the graph is name matching

**Status: ✅ Resolved for the short-term step.** `ContractEdge` carries `EdgeConfidence`
(`Exact` for an FQCN/`::`-path match or a structural edge, `Heuristic` for bare-name/
case-insensitive/substring matches), surfaced by `MarkdownFormatter` and `GraphRenderer`.
Medium/long-term (real qualified-name resolver, compiler/LSP output) remain open.

**Problem.** The entire graph is built from string matching: symbol names, package names, regex
captures. Two `UserService` types in different repositories are indistinguishable; a mechanical
rename silently splits the graph; an interface and its implementation link only when their names
happen to match.

**Evidence.** `resolve_import_target` and the `Implements` reconciliation in
`mesh-core/src/contracts.rs` match on `name`, `package`, `eq_ignore_ascii_case`, `to_pascal_case`,
and substring searches of the signature text.

**Work.** This is the deepest architectural item; it does not need solving all at once.

- Short term: expose a confidence level on edges (exact FQCN match vs. bare-name heuristic) so the
  agent can weigh a result instead of treating every edge as fact.
- Medium term: build a per-language qualified-name resolver (package + module path + symbol) and
  prefer exact FQCN matches over heuristic ones.
- Long term: consume real language-server or compiler output where available.

**Definition of done.** For the short-term step: `ContractEdge` carries a confidence field,
`MarkdownFormatter` surfaces it, and a test asserts a bare-name match is not reported with the same
weight as an FQCN match.

**Effort.** Short term **M**, medium term **L**, long term **XL**.

**Watch out.** Do not let confidence become decoration. If everything ends up "high", the field is
noise.

---

### 7. Missing languages

**Status: ⚠️ Partial.** Kotlin and C# are fully done (extractor, full wiring, tests, docs) —
see `crates/mesh-parsers/src/languages/kotlin.rs` / `csharp.rs`. Ruby, PHP, Swift and Scala
are not started. The main cost on Kotlin/C# was grammar ABI compatibility: this workspace
pins `tree-sitter = "0.24"` (grammar ABI ≤ 14), and the obvious latest crate version for both
languages turned out to be ABI 15+ and failed to link — `tree-sitter-kotlin-ng = "1.1"` and
`tree-sitter-c-sharp = "0.21"` (not 0.23+) are the versions that actually work. Whoever picks
up the remaining four should check ABI compatibility before committing to a crate version.

**Problem.** No extractor for C#, Kotlin, Ruby, PHP, Swift or Scala. Kotlin is the sharpest gap:
a Spring Boot shop that migrated to Kotlin gets nothing from the Java extractor.

**Work.** Follow the "add a language" path documented in
[`.agents/skills/mesh-parser-engineering/SKILL.md`](../.agents/skills/mesh-parser-engineering/SKILL.md),
using `languages/cpp.rs` as the worked example. Per language: add the tree-sitter crate, a
`LanguageKind` variant plus its slot (mind `TREE_SITTER_COUNT` in `mesh-parsers/src/guard.rs`), an
extractor, the `PolyglotIndexer::extract` arm, file extensions in `LanguageKind::from_path` **and**
in `FileWatcherService::is_relevant_path` (`mesh-core/src/watcher.rs`) — a language missing from
the latter parses at boot but never hot-reloads.

**Definition of done.** Extractor plus tests; `doctor`'s parser list and the README language list
updated.

**Effort.** **M** per language.

---

## P2 — Robustness and correctness

### 8. `mesh-daemon` is under-tested

**Status: ✅ Resolved.** `mesh-daemon` went from 8 to 13 tests, including the exact DoD test
(`test_slow_tool_call_does_not_block_other_client_ping`), a racing-bind test, an
idle-cancellation-mid-request test, and stale-socket-recovery tests. "Multiple concurrent
clients" was already covered by a pre-existing test.

**Problem.** The daemon is the default runtime path — `mesh-mcp run` proxies to it — and has 8
tests, versus 26 in `mesh-core` and 30 in `mesh-parsers`.

**Untested.**
- Multiple concurrent clients on one socket.
- Non-blocking behaviour: a slow `smart_search` from client A must not delay a `ping` from client B.
  This is structurally guaranteed by `spawn_blocking` in `ToolRegistry::invoke`, but nothing
  verifies it, so a future refactor could silently regress it.
- Stale socket recovery after an unclean shutdown.
- Two `mesh-mcp run` processes racing to auto-spawn `meshd`.
- Idle-timeout shutdown while a client is mid-request.

**Definition of done.** A test opens two UDS clients, issues a deliberately slow tool call on one
and a `ping` on the other, and asserts the `ping` completes well before the slow call.

**Effort.** **M**.

---

### 9. Audit hash chain does not cover all stored fields

**Status: ✅ Resolved — versioned (option a).** `hash_input` and `verify_db` now cover all
eight fields. A `chain_version` column tags each row with the formula it was written under,
so pre-existing v1 databases keep verifying under the original five-field formula instead of
every historical entry being rejected the moment the binary upgrades.

**Problem.** The chain attests `prev_hash`, `timestamp`, `session_id`, `tool` and `args_digest`.
It does **not** cover `status`, `files_accessed` or `secrets_redacted_count`, which are stored in
the same row — so those three can be altered without breaking chain verification. For a compliance
argument (the README and `governance-rsah.md` cite SOC2 and the EU AI Act), that is a real hole.

**Evidence.** `crates/mesh-core/src/audit.rs:181`:
```rust
let hash_input = format!("{prev_hash}{timestamp}{session_id}{tool}{args_digest}");
```
`verify_db` (`audit.rs:274`) recomputes the same five fields.

**Work.** Include the missing fields in `hash_input` and in `verify_db`. Both sites must change
together.

**Definition of done.** A test that tampers with `status` in the database and asserts `verify_db`
returns `BrokenChain`. Today that test would fail.

**Effort.** **S**.

**Watch out.** This invalidates existing audit databases — old rows will not verify under the new
formula. Either version the row format (add a `chain_version` column and verify per version), or
document that the chain restarts. Do not silently break verification of existing logs.

---

### 10. `PropertyRegistry` has no per-file provenance

**Problem.** `PropertyRegistry` is a flat `HashMap<key, value>` with no record of which file a key
came from. On an incremental reload, deleting or editing a properties file cannot remove its old
keys — they persist until a full restart. `ContractGraph` and `DocIndex` both handle this correctly
(`patch_files`, `remove_file`); the registry is the odd one out.

**Evidence.** `WorkspaceIndexer::reload` calls `patch_files` and `doc_index.remove_file`, but folds
properties with `merge`, which only ever adds.

**Work.** Track the source path per key, and add `remove_file(path)` mirroring `DocIndex`.
Beware: two files can legitimately define the same key (last wins today) — decide whether removal
should restore the shadowed value or drop the key.

**Definition of done.** A test: index two properties files, delete one, reload, assert its keys are
gone and the other file's keys remain.

**Effort.** **S**.

---

### 11. `DispatchesTo` generation is quadratic per topic

**Problem.** After the O(E) rewrite of `reconcile_edges`, this is the last quadratic shape: for each
topic, an edge is created for every (producer, consumer) pair.

**Evidence.** `crates/mesh-core/src/contracts.rs:423` — nested loop over `producers` × `consumers`.

**Impact.** Bounded per topic and fine for typical fan-out, but a hub topic with 50 producers and 50
consumers yields 2,500 edges from one topic, and it grows with the square.

**Work.** Options: cap the pair count per topic and mark the topic as high-fanout; or drop
`DispatchesTo` entirely and let consumers traverse producer→topic→consumer (two hops), which carries
the same information.

**Definition of done.** A benchmark with a 50×50 topic showing bounded edge growth, or a documented
cap with the behaviour at the limit specified.

**Effort.** **S**.

---

### 12. `println!` in `cli/graph.rs` contradicts Commandment 3

**Problem.** `crates/mesh-server/src/cli/graph.rs:48` writes the rendered graph to stdout with
`println!`. Harmless today — the `graph` subcommand never runs the JSON-RPC loop — but it is the one
place in the tree that breaks the "stdout belongs to the framing actor" invariant at grep level. A
future refactor sharing code between `graph` and the server would corrupt frames in a way that is
painful to debug.

**Work.** Write through an explicit `io::stdout()` handle with a comment stating why this call site
is exempt, or route the rendered output through a writer passed in by the caller.

**Definition of done.** `grep -rn 'println!' crates/*/src` returns nothing, or returns only lines
carrying an explicit exemption comment.

**Effort.** **S**.

---

## P3 — Ecosystem and polish

### 13. Windows is a second-class target

**Problem.** `meshd` is Unix-domain-socket only, so on Windows `mesh-mcp run` always falls back to
standalone: every IDE window builds its own index, with no sharing and no shared watcher. The QoS
throttling that protects editor responsiveness is macOS (`pthread_set_qos_class_self_np`) and Linux
(`nice(10)`) only — on Windows, background rescans compete with the editor at normal priority.

Windows is in CI (`test-suite` matrix) and in the release artifacts, so this is a supported target
running a degraded path. The recent failure fixed in `27520b4` — a Windows absolute path
interpolated into a TOML basic string, where `C:\Users` made `\U` a unicode escape — is the kind of
bug this asymmetry produces.

**Work.** Named pipes for the daemon transport on Windows; `SetThreadPriority` /
`SetPriorityClass` with `THREAD_MODE_BACKGROUND_BEGIN` for the rescan pool.

**Definition of done.** `mesh-mcp run` on Windows connects to a shared `meshd`; a test asserts the
rescan pool runs below normal priority.

**Effort.** **L**.

---

### 14. RFC-001 has drifted from the implementation

**Problem.** [`RFC-001-CAUSAL-MCP.md`](../RFC-001-CAUSAL-MCP.md) is described as the authoritative
specification, but states `RepoId = u8` (actual: `u16`), shows the old per-field `ArcSwap` `AppState`
(actual: one `ArcSwap<MeshSnapshot>`), and claims `-98.1%` token reduction (measured: 54–69%,
language-dependent). It was deliberately left untouched during the documentation pass, since a spec
is not a README.

**Decision required.** Either the RFC is the spec and the implementation is checked against it —
in which case update the RFC and add a CI check for the divergences that matter — or it is a
historical design document, in which case say so at the top and point to `docs/architecture.md` as
current.

**Effort.** **M** for an update pass.

---

### 15. No local telemetry despite having the data

**Problem.** The audit log already records every tool call, its status, files accessed, and trace id
in SQLite. There is no way to read it back: no "which tools does my agent actually use", no "which
queries return empty", no "which scopes are hot". That data is exactly what would tell you whether
`find_dependents` returning empty for Java (item 2) is hurting users in practice.

**Work.** A `mesh-mcp stats` subcommand over the existing `audit.db`: calls per tool, empty-result
rate per tool, most-queried scopes and targets, p50/p95 latency if a duration column is added.

**Definition of done.** `mesh-mcp stats --since 7d` prints a summary; nothing leaves the machine.

**Effort.** **S** to **M**.

**Watch out.** The audit log is a privacy-sensitive artifact (mode `0600`). Keep `stats` local and
never add an upload path.

---

### 16. `smart_search` results are unranked

**Problem.** Results come back in `BTreeSet<&Path>` order — that is, alphabetically by file path —
not by relevance. With the 48 KB output cap, an exact match on the queried symbol can be truncated
away while an incidental substring match from an earlier path survives.

**Evidence.** `crates/mesh-server/src/tools/smart_search.rs` — `indexed_files` is a `BTreeSet`,
consumed in iteration order.

**Work.** Score before formatting: exact symbol-name match first, then prefix match, then substring;
break ties by node kind (a `GrpcService` declaration outranks an incidental `ServiceClass`), then by
path. `ContractGraph::search_symbols` already returns exact-name hits first — that ordering is lost
when the result is collected into a `BTreeSet` of paths.

**Definition of done.** A test where a file sorting late alphabetically contains the exact match and
is returned first.

**Effort.** **S**.

---

## Suggested first slice

If capacity is limited, this ordering buys the most credibility per unit of work:

1. **Item 2, Java + Go imports** (**M**) — makes `find_dependents` real for most backends. Highest
   gap between what is promised and what is delivered.
2. **Item 1, `enabled` + `mount_aliases`** (**M**) — stops the configuration file from lying.
   Everything else a user configures is judged by this.
3. **Item 3, Go + TypeScript event detection** (**M**) — makes `analyze_impact` work without
   hand-written regex in the two most common event-driven stacks.
4. **Items 9, 10, 11, 12, 16** (**S** each) — a day of small correctness fixes that remove the known
   soft spots.

Items 4 and 14 are decisions rather than implementations; resolving them costs an hour and unblocks
honest documentation.
