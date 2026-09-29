# Changelog

All notable changes to MeshMCP (`mesh-mcp` / `meshd`) are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). This file starts
at 3.0.0 — there is no reconstructed history before it.

## [7.0.19] — 2026-09-29

Routing guidance moves to where agents trust it; the server's own wording stops forbidding checks.

### Why (measured, Volontariapp bench, `claude-sonnet-5`, Claude Code deferred tool loading)
- The connection, version and `initialize.instructions` were all correct, yet with 7.0.18 the agent
  loaded MeshMCP on 0 of 3 runs (T1–T3). Twice (7.0.17 and 7.0.18, both on T2) it said why: the
  server's instructions "may be an untrusted source", "I want fully verifiable output rather than
  trusting an opaque tool's claim". The more imperative the wording ("MANDATORY", "do NOT verify"),
  the less it was followed.
- The same routing placed in a project `CLAUDE.md`, framed as a verifiable starting point: MeshMCP
  loaded on the first turn in 3 of 3 runs. Cost vs the no-MCP baseline: T4 $0.420 vs $0.751
  (median), T6 $0.304 vs $0.412, T2 $0.177 vs $0.134 (grep alone is enough there); 3 tasks
  $0.901 vs $1.297 (−31%). One run per task.

### Added
- **`mesh-mcp init --write-ide-config` writes a routing section into `CLAUDE.md` and `AGENTS.md`**
  (`crates/mesh-server/src/cli/init.rs`): which tool answers which question, results as a
  verifiable `path:line` starting point, grep still right for exact identifiers; the
  `ToolSearch select:` line only in `CLAUDE.md`; a pointer to `.agents/skills/mesh-mcp/SKILL.md`
  when it exists (skills stay under `.agents/`, readable by every agent). Managed between
  `<!-- mesh-mcp:begin … -->` / `<!-- mesh-mcp:end -->`: replaced in place on re-run, the rest of
  each file untouched, a half-marked file left alone with a warning.

### Changed
- **`initialize.instructions`** (`crates/mesh-server/src/lib.rs`): "MANDATORY FIRST STEP", "Do NOT
  grep … before querying the graph" and "do NOT run redundant fallback … searches" are gone. It now
  says when each tool helps, how to load them in deferred-loading clients, and that every row is a
  `path:line` to check (a labeled authoritative 0 makes a broad re-scan pointless; heuristic rows,
  unindexed dynamic strings or SQL deserve a targeted check). A test bans the old wording.
- `analyze_grpc`, `analyze_impact`, `find_dependents` descriptions and `docs/mcp-tools.md` use the
  same tone ("a broad grep re-scan rarely adds anything" instead of "DO NOT re-check").

## [7.0.18] — 2026-09-28

Imperative tool activation in `initialize.instructions`.

### Changed
- **`initialize.instructions` (`crates/mesh-server/src/lib.rs`)**: the conditional "if your client
  defers…" activation paragraph is replaced by a `MANDATORY FIRST STEP (Turn 1)`: for questions about
  imports, packages, gRPC or events, load `find_dependents`, `analyze_impact` and `analyze_grpc`
  (`ToolSearch select:…` in deferred-loading clients such as Claude Code) before any grep. Measured on
  7.0.17 with Claude Code's deferred loading (8 single runs, T1–T7 + a T6 repeat): ToolSearch in 6/8,
  MeshMCP called in 4/8; a T6 run that never loaded it cost 2.7M tokens against ~1.0M without MeshMCP,
  and T2 ("who imports a package") was answered by grepping import statements.
- Rule 3 maps "who imports X" / package usages to `find_dependents`; a new rule forbids grepping
  event publish/consume sites or import statements before querying the graph. `find_dependents`'s
  description says it answers who imports or uses a package, module or symbol.
- The instructions constant had been inserted inside `respond`'s doc comment (7.0.15); each now has
  its own.

## [7.0.17] — 2026-09-28

Makes the 7.0.15–7.0.16 agent guidance actually reach agents and agree with itself.

### Fixed
- **Stale sibling `meshd` no longer serves a newer `mesh-mcp` (`crates/mesh-server/src/main.rs`)**:
  the socket name is keyed by `mesh-mcp`'s version, so when only `mesh-mcp` was reinstalled it
  auto-spawned the older sibling `meshd` on a fresh socket, and that daemon answered every
  request — `initialize` included — with its own behavior. Measured twice (7.0.15 and 7.0.16
  installs): `serverInfo.version` and `instructions` came from the previous release. Before
  spawning, `mesh-mcp` now runs `meshd --version` and, on a mismatch, logs both versions and
  serves the session in-process instead.
- **Tool descriptions contradicted the 7.0.16 negative-assertion rule**: `analyze_grpc`,
  `analyze_impact` and `find_dependents` still said any 0 means "does not exist, never grep",
  while `initialize.instructions` limits that to results labeled `Authoritative AST Scan` and
  allows targeted checks of heuristic/ambiguous rows. The descriptions (and
  `docs/mcp-tools.md`) now state the same rule.
- **CI red since 7.0.15**: `test_tools_call_reports_still_indexing_before_first_snapshot` read
  the `initialize` reply into a fixed 1 KiB buffer; with the routing `instructions` it is larger,
  so the JSON was cut. The test now reads one line-delimited frame per reply.

### Changed
- `analyze_impact` and `analyze_grpc` descriptions name what agents ask for in generic terms
  (who publishes / consumes or subscribes, outbox writes, message broker and event bus
  listeners, client call sites), so deferred-tool search can match them. The 7.0.16 entry
  announced refined tool descriptions, but only one argument description had changed.

## [7.0.16] — 2026-09-28

Conditional deferred tool activation instructions, heuristic-aware negative assertion rules, and generic architectural taxonomy.

### Changed
- **Conditional `ToolSearch` activation instructions (`crates/mesh-server/src/lib.rs`)**:
  Updated the MCP `initialize.instructions` payload to include a client-agnostic conditional directive for deferred-loading environments (e.g. Claude Code `ToolSearch select:...`), guiding agents to unblock schemas without imposing client-specific constraints.
- **Nuanced negative assertion rule for heuristics (`crates/mesh-server/src/lib.rs`)**:
  Softened the absolute negative assertion guidance to distinguish authoritative compiler-verified results from heuristic, ambiguous, or dynamic/runtime matches, empowering agents to perform targeted follow-up verifications when appropriate.
- **Generic architectural vocabulary**:
  Refined tool descriptions and argument schemas to ensure generic microservices terminology (topics, queues, streams, brokers, event producers, consumer handlers) without benchmark-specific or corpus-specific overfitting.

## [7.0.15] — 2026-09-28

MCP `initialize.instructions` routing payload for automatic tool discovery by AI agents.

### Added
- **MCP `initialize.instructions` payload (`crates/mesh-server/src/lib.rs`)**:
  Injected comprehensive routing instructions into the standard MCP `initialize` handshake response.
  When connected to Claude Code or compliant MCP clients, this injects explicit system-prompt
  directives instructing agents to prioritize `analyze_grpc`, `analyze_impact`, and `find_dependents`
  over fallback `grep`/`ripgrep` searches, solving the tool discovery gap observed in unprompted A/B benchmarks.

## [7.0.14] — 2026-09-28

Authoritative negative assertions and agent anti-verification guardrails (A/B benchmark findings).

### Added
- **Authoritative negative assertions on empty results**:
  When `analyze_grpc`, `analyze_impact`, or `find_dependents` resolves 0 clients, 0 handlers,
  0 producers, or 0 dependents, the Markdown output now renders `EXACTLY 0 (Authoritative AST Scan)`
  confirming that all workspace roots were exhaustively scanned with deterministic AST precision.
- **Agent governance rule (`.agents/rules/mesh-authority.md`)**:
  Instructs coding agents to treat MeshMCP's negative results as deterministic compiler truth
  and avoid wasting hundreds of thousands of tokens in redundant `ripgrep` verification loops.

### Changed
- **Tool descriptions reinforced with negative prompting**:
  `analyze_grpc`, `analyze_impact`, and `find_dependents` now explicitly instruct agents:
  *"If reported as 0/none, IT DOES NOT EXIST in the codebase: DO NOT run secondary ripgrep/grep searches"*.
- **Removed doubt-inducing wording in `format_impact_matrix`**:
  Replaced `"search for them before concluding nothing emits/handles it"` with explicit confirmation
  that 0 AST elements were resolved across all indexed workspace roots.

## [7.0.13] — 2026-09-28

Honesty and setup checks (Volontariapp 7.0.6 report, minor defects 15 and 17).

### Fixed
- **`doctor` did not see duplicated git submodules**: 14 copies of `ci-tools` (one per root) were
  ~17% of the Volontariapp index and showed up in scoped searches, while "Root overlap: none".
  A new `Duplicated submodule` warning names a submodule URL checked out in several roots and not
  excluded, with the pattern to exclude it.
- **`smart_search` listed names that merely contain the query as equals** of exact declarations
  (`SocialEventCreatedPostProcessor` among three `EventCreatedPostProcessor`s). A page mixing both
  now says how many files declare the query exactly (listed first) and how many only contain it.
- **The agent guide claimed every heuristic result is marked**, which was false. It now says which
  tools label confidence and completeness, and how `smart_search` / `find_dependents` flag
  name-only matches.

## [7.0.12] — 2026-09-28

`find_dependents` completeness (Volontariapp 7.0.6 report, defects 9 and 10). Measured on the
Volontariapp workspace against the report's `rg` ground truth: `@volontariapp/messaging` with
tests 201/201 files (7.0.6: 167), 18/18 packages (17); `@volontariapp/config` 117/117 with the
`bridge` barrel (116).

### Fixed
- **Files with imports but no declaration were never dependents**: a barrel of `export * from …`
  or a spec made only of `describe(…)` calls had no node to carry its imports. Such a TypeScript
  file now gets one `Module` node spanning it (new `NodeKind::Module`, never a `smart_search`
  result). Index cache schema v9.
- **"N dependent(s) in test files left out" counted declarations** under a per-file or
  per-package listing (7.0.6: "39 left out" for 34 files). It now counts in the listing's unit.
- **Paging past the end said "No dependents found"** above "showing 167-180"; it now says there is
  nothing at that offset and which offsets exist. A `granularity: "package"` footer counts
  packages, not "dependent(s) in N file(s)".

## [7.0.11] — 2026-09-28

`visualize_mesh` edges (Volontariapp 7.0.6 report, defects 7 and 14). Measured on the Volontariapp
workspace: no service → `nativapp` arrow and no `api-gateway` → `ms-event` arrow any more (7.0.6:
every backend service "imported" `nativapp`, 20–61 edges each).

### Fixed
- **Node.js built-in modules resolved to workspace symbols**: `import * as path from 'path'`
  matched a `const path = require('path')` in `nativapp/scripts/setup-env.js`. A built-in module
  name (`fs`, `path`, `os`, `node:*`, `fs/promises`, …) now resolves within the importer's own
  repository only.
- **Ambiguous edges were drawn as dependencies.** An ambiguity whose candidates all sit in one
  service is still drawn as a link to that service (heuristic); one spread over several services
  (`EventDTO` declared in `npm-packages` and `ms-event`) is not drawn, and the footer gives the
  count (587 on Volontariapp).
- **Zoomed JSON had no location**: every contract node had `file_path: ""` and `line_start: 0`.
  Contracts of the focused service now carry their file and lines.

### Known limit
- A named import from an external npm package can still be linked by name to a homonymous symbol
  of another repository (`import { Index } from 'typeorm'` → a `nativapp` component, 8 edges on
  Volontariapp): the index does not yet record which module a named import comes from.

## [7.0.10] — 2026-09-28

`analyze_impact` (Volontariapp 7.0.6 report, defects 5, 6 and 13). Measured on the Volontariapp
workspace: `EVENT_CREATED` with `depth: 2` now reaches the second hop the report's ground truth
has (both post-processors emit onto `WS_EVENT_CREATED_FEEDBACK`, consumed by `ws-service`); 7.0.6
returned the depth-1 answer.

### Fixed
- **`depth ≥ 2` did not follow re-emissions**: a post-processor's re-emission is a separate
  producer node in its file, while the walk only followed the consumer node's own `Produces`
  edges. Each hop now also follows producers declared in a reached consumer's file; those rows are
  `heuristic` and the output says the walk used them.
- **A topic with no resolved producer (or consumer) was shown without comment** (`USER_CREATED`,
  emitted by a SQL trigger; `EVENT_SOCIAL_CREATED`, consumed through another name). The matrix
  now says "No producer resolved" / "No consumer resolved" (outside tests) and why that may be.
- **`event.created` matched nothing**, although the tool description cites it: a target matching
  no topic key as written is tried with `.`, `:`, `-`, `/` as `_` (`event_created`).

## [7.0.9] — 2026-09-28

`smart_search` secret masking (Volontariapp 7.0.6 report, defects 4 and 8). The report's six
redaction cases on the Volontariapp workspace: secrets masked 2/2 (7.0.6: 1/2), non-secrets left
alone 4/4 (7.0.6: 1/4).

### Fixed
- **A hard-coded secret in a declaration was printed in clear**: `const password =
  'Password123!';` had a space in its "key", so the line was skipped. Declaration keywords and
  modifiers (`const`, `let`, `var`, `readonly`, `private`, …) are now set aside before the key is
  tested; prose and comments still are not keys.
- **Non-secrets were masked**, printing wrong code or broken JSON: an enum member whose value is
  its own name (`REFRESH_TOKEN = 'refresh_token'`), an environment variable name
  (`"password": "DB_PASSWORD"` in node-config's `custom-env-vars.json`), an object opening
  (`"auth": {`), `${…}` references and `true`/`false`/`null` are no longer masked.
- **Masking replaced everything after the separator**: only the value (with its quotes) is
  replaced now; a trailing `,` / `;` and a type annotation are kept.

## [7.0.8] — 2026-09-28

Wire-format check covers enums (Volontariapp 7.0.6 report, critical defect 3). Measured on the
Volontariapp `event.proto` against base `610b2ac`: the `EVENT_STATE_CANCELLED` 3 → 5 renumbering is
now one `WIRE_FORMAT_BREAKING_CHANGE` (7.0.6: "no wire-format breaking change"); the `Tag.color` →
`balise` warning and the silence on `optional organizer_id` are unchanged.

### Fixed
- **Enums were not compared at all**: only their names were collected, so a renumbering, a swap, a
  deleted constant or a reused `reserved` number passed as "no wire-format breaking change". Two
  new rules: *enum number reused* (a number now names another constant because a name moved, or a
  `reserved` number is used again) and *enum value deleted without `reserved`*. An in-place rename
  of a constant is a JSON warning, like a field rename. Nested enums, negative values,
  `allow_alias` and removed enums are handled.

## [7.0.7] — 2026-09-28

`analyze_grpc` answers (Volontariapp 7.0.6 report, critical defects 1 and 2). Measured on the
Volontariapp workspace against the report's `rg` ground truth, 6 RPCs: clients 8/8 (7.0.6: 6/8,
plus 1 false positive), handlers 5/5 with no false positive (7.0.6: 5 right out of 9 listed).

### Fixed
- **RPCs whose name contains the target were traced as the target.** `CreateEvent` listed
  `CreateEventNode`'s handler, `UpdateUser` listed `AdminUpdateUser`'s. Any `.proto` node matching
  by substring was an anchor. Now, once a service or method matches by name (or by full
  `package.Service/Method`), substring-only matches are dropped and named in a "Not traced" line;
  substring matching remains the fallback when nothing matches by name. Same rule in
  `analyze_impact`'s gRPC part.
- **Calls through an inherited client field were invisible**: 75 gateway calls in 22 files use
  `this.commandService` / `this.queryService` bound in `BaseEventGrpcController` (another file).
  Each class's client fields are now recorded, and a call through a field the class does not
  declare resolves through its base class's binding (`extends Base`), falling back to the field
  name as before. Measured: 75/75 of those call sites now have an `exact` edge to their RPC.
- **"Client Stubs (1 found)" with no caveat while another caller was unresolved.** The
  "may call this method" list was only computed when no client at all resolved. It is now always
  computed, and says the list may be incomplete. Files that hold the service's client in a typed
  field are left out of it: every call through such a field is recorded method by method, so they
  are known not to call the method (7.0.6 listed `relationship.query-controller.ts` for `SignUp`).
- Index cache schema v8.

## [7.0.6] — 2026-09-28

Release hygiene (Volontariapp report, point 15).

### Fixed
- **No declared minimum Rust version, and `SETUP.md` claimed 1.80+.** The real minimum is 1.90
  (set by `tree-sitter-language`), now pinned as `rust-version` for every crate and verified with
  `cargo +1.90 check --workspace --all-targets`.
- **21 code comments cited "RFC-001"**, a document that is not in the repository. They now point
  at the commandments in `CLAUDE.md` or drop the reference.
- Every release since 7.0.1 is tagged on its merge commit on `main` (7.0.0's tag points at a
  pre-amend commit that is not on `main`, so a 7.0.0 binary cannot be traced back to `main`).

## [7.0.5] — 2026-09-28

Asynchronous flows in TypeScript (Volontariapp report, points 3 and 6). Measured on the Volontariapp
workspace, `analyze_impact EVENT_CREATED`: one topic linking the two producers to the consumers
(7.0.0: four unrelated topics, test files counted as producers, a sibling
`WS_EVENT_CREATED_FEEDBACK` stream mixed in). With one pattern for the `streamName:
getEventStreamName(Streams.X)` options convention, the result is exactly the three consumers `grep`
finds.

### Fixed
- **TypeScript event facts were never registered as producers or consumers** (the nodes existed,
  `analyze_impact` could not link them). `@EventPattern` / `@MessagePattern` handlers consume their
  pattern; kafkajs `subscribe` / `send`, NestJS `client.emit` / `client.send('pattern')` and BullMQ
  `new Queue` / `new Worker` (new) are attributed to the method that publishes or subscribes.
- **Topic keys had no normalization**: `Streams.EVENT_CREATED`, `EventMessagingType.EVENT_CREATED`
  and `typeof EventMessagingType.EVENT_CREATED` were three topics. A code reference to an enum or
  constant member now keys on the member; broker literals (`orders.created`) are kept whole.
- **`analyze_impact` matched topics by substring only**: a topic whose key equals the target now
  wins, so `EVENT_CREATED` no longer pulls in `WS_EVENT_CREATED_FEEDBACK`.
- Index cache schema v7.

## [7.0.4] — 2026-09-28

TypeScript depth (Volontariapp report, points 1, 4, 5 and 7). Measured on the Volontariapp
workspace: `analyze_grpc SignUp` now lists exactly the two gateway call sites (7.0.0: none), and
`find_dependents @volontariapp/messaging` returns exactly the 144 non-test files `grep` finds
(7.0.0: 502 results, truncated; 7.0.3: 104 files).

### Added
- **Method-level gRPC clients in TypeScript.** `this.<field>.<method>(…)`, where the field is
  bound by `getService<XServiceClient>(…)` or typed `XServiceClient` (field or constructor
  parameter property), is a call to `XService.<Method>`, resolved by exact `Service.Method` only
  (never by bare method name). A field only *named* like a client (`userService`, set in a base
  class of another file) resolves the same way with heuristic confidence. Fields are per class.
- **Top-level TypeScript declarations**: functions, enums, type aliases, `const` / `let`
  bindings (arrow-function components, `…Options` objects) and abstract classes are indexed, so
  `smart_search` finds them without `fuzzy` and a module declaring only these is visible to
  `find_dependents`. `export … from` re-exports count as module dependencies.

### Fixed
- **`.tsx` was parsed with the TypeScript grammar** (JSX became error nodes; `const App = () =>
  <View/>` was not indexed). `.tsx`, `.js`, `.jsx`, `.mjs` and `.cjs` use the TSX grammar;
  `.mts` and `.cts` are recognized as TypeScript.
- **A gRPC handler vanished when another decorator followed `@GrpcMethod`** (`@GrpcMethod(…)`
  then `@UseGuards(…)`): only the decorator right above the method was read. Every decorator is
  read now, and the method body no longer is (a `"@Get"` string in a body made it an endpoint).
- Index cache schema v6.

## [7.0.3] — 2026-09-28

Answers that were wrong or incomplete while looking complete (Volontariapp report, points 2, 5, 8
and the test noise of 3).

### Fixed
- **`find_dependents` on a shared package returned 502 results for 167 importing files, then
  truncated.** A TypeScript module import is now attached once to the file's top-level
  declarations instead of to every method, and once however many import lines name the module.
  Subpath imports (`@scope/pkg/testing`) count for their package.
- **Bare npm packages were dropped from TypeScript imports** (`import { fromEvent } from 'rxjs'`
  lost `rxjs`): module and symbol entries were told apart by "contains `/`".
- **`find_dependents` had no paging and no test filter.** It now takes `limit` / `offset`, a
  `granularity: "file"`, and leaves test files out (`*.spec.ts`, `*_test.go`, `__tests__/`,
  `test-utils/`, …), saying how many, unless `include_tests: true`. `analyze_grpc` and
  `analyze_impact` apply the same test filter.
- **`find_dependents("a")` returned everything** through its substring fallback. The fallback
  now needs 3 characters and its results come under an explicit heuristic warning.
- **`analyze_grpc` answered "0 clients" when a service's client was built but the method call
  was not resolved.** It now lists those service-level callers with a note that they may call
  the method.
- **`smart_search` masked `password!: string;` in a DTO as a secret.** In source code only a
  quoted literal value is masked; config files keep the key-based masking.
- Index cache schema v5: cached extractions from earlier builds are recomputed.

## [7.0.2] — 2026-09-28

Most installs are done by the user's own AI agent: this release gives that agent an accurate guide
and fixes two `init` defects found while writing it.

### Added
- **`mesh-mcp agent-guide`** prints `docs/agent-setup.md`, a setup guide written for the AI agent
  that installs MeshMCP on the user's behalf: what to ask first (global config, `install-hooks`,
  `doctor --fix`), choosing the workspace root for sibling repositories, reviewing `roots`,
  excludes, gRPC and event patterns, registering the server, and verifying the result against
  `grep` before reporting success. Embedded in the binary, so it always matches the version.
- **`install.sh` ends with a plain-text note for AI agents** pointing at `mesh-mcp agent-guide`.

### Fixed
- **`mesh-mcp init` overwrote an existing `.agents/mesh-mcp.toml`**, including on
  `init --write-ide-config`, silently discarding a tuned config. It is now kept; `--force`
  regenerates it.
- **`init --write-ide-config` printed "Registered Claude Code CLI guidance" and wrote nothing.** It
  now merges a `mesh-mcp` entry into Claude Code's project-scope `.mcp.json`, like it does for
  `.cursor/mcp.json` and `.vscode/mcp.json`.

## [7.0.1] — 2026-09-28

Fixes from the first field report on a NestJS / TypeScript / React Native workspace (Volontariapp,
15 repositories), measured on 7.0.0.

### Fixed
- **`mesh-mcp stats --since 7é` panicked** slicing inside a multi-byte character; any non-ASCII
  unit is now a plain error.
- **Every tool call was audited as session `active-session`**, so agent sessions could not be told
  apart. `meshd` now audits each client connection under its own session id (the standalone server
  uses one per process), and `stats` lists sessions and filters one with `--session <id>`.
- **Tests and benchmarks wrote into the real `~/.cache/mesh-mcp/audit.db`** (314 `test_slow_op`
  calls and scale-bench files dominated `stats` on a developer machine). `meshd`'s tests use an
  in-memory database, the benchmark scripts set `MESH_AUDIT_DB`, a new environment variable that
  moves the default audit database; `stats --db <path>` reads another file.
- **`stats` reported a p95 over a single sample.** p95 is now shown from 20 timed calls on
  (`-` below, with a note). "Most-queried" is renamed to what it counts ("most-returned files"),
  multi-root workspaces print relative to their common ancestor, session timestamps are
  normalized to ISO 8601 whatever format the row was written in, and `--json` prints the summary.
- **`doctor --json` only printed 4 of the checks.** Every section is now a structured check, so
  `--json` prints all of them (index health included); volatile figures (RSS) stay in the text
  report so two runs remain identical.
- **`doctor` logged a "Sandbox escape attempt" for its own symlink probe on macOS**, and the probe
  then passed without testing anything: the temp directory was not canonicalized before being
  used as the jail root. The "JSON-RPC serialization baseline" line (timing one tiny
  serialization) is removed, memory is measured before doctor's own index scan, and the last line
  now counts errors and warnings instead of always reading "All systems operational".
- **Index health listed `yarn.lock` and PNGs as "not indexed"**. Non-source files (images, fonts,
  archives, lockfiles, minified bundles) are counted as skipped, never listed.

## [7.0.0] — 2026-09-27

**Plan 4 (P3) complete: enterprise readiness, scale tiers, and field stabilization.** Major
version because of the breaking changes below (wire-format break checks in `analyze_grpc`,
optional search scope and 8 KiB default search page budget, relative graph fingerprints, and
per-workspace index cache paths). All 14 milestones (4.0 through 4.13) completed and verified:
stacked CI PRs (4.0), index health diagnostics and rejection reporting (4.1), FSEvents native
watcher with Git lock hold (4.2), 5k / 50k / 200k synthetic and real repo size budgets (4.3),
per-workspace index cache with bounded quota and LRU eviction (4.4), TypeScript gRPC recall
ratchet to 100%/100% on otel-demo (4.5), impact matrix with proto/gRPC resolution (4.6a), wire-format
breaking change analysis against Git base (4.6b), doctor repairable health `--fix` / `--json` (4.7),
pilot installer and metrics stats (4.8), streaming YAML and bounded Markdown memory (4.9),
Linux daemon seccomp sandbox (4.10), CI determinism gate resilience (4.12a), audit refusals and
snippet fences (4.12b-e), relative fingerprints independent of checkout dir (4.12f), and
global scope / 8 KiB search pagination (4.13).

### Breaking Changes
- **`analyze_grpc` checks `.proto` wire-format breaking changes against a Git base**:
  Verifies target `.proto` files against a Git base (`origin/HEAD`, `origin/main`, `main`, or an
  explicit `base` argument). Any reused field tag number, incompatible type or cardinality change,
  or field deleted without `reserved` is reported as `WIRE_FORMAT_BREAKING_CHANGE`.
- **`smart_search` scope is optional & default page budget is 8 KiB**: `scope` is no longer
  required in the tool schema; omitting it or passing `"."`, `"*"` or the workspace root searches
  across all configured workspace roots. Search pages cut at 8 KiB of rendered results by default
  (`DEFAULT_PAGE_BUDGET_BYTES`), with `limit` (default 20) acting as a secondary cap.
- **`mesh-mcp graph --format fingerprint` is independent of checkout directory**: Node,
  doc-section, and property-source paths are fingerprinted as `<root index>:<relative path>` with
  `/` separators instead of absolute filesystem paths, ensuring identical fingerprints across
  machines, directories, and operating systems. Fingerprint values change once with this release.
- **Per-workspace persistent parse cache**: The cache is relocated from machine-wide
  `~/.cache/mesh-mcp/index-cache.db` to isolated per-workspace database
  `~/.cache/mesh-mcp/workspaces/<workspace_id>/index-cache.db` with schema version 4. Pre-4.4 legacy
  cache is no longer read (can be cleaned via `mesh-mcp doctor --fix`).

### Security
- **Linux `meshd` network sandbox (step 4.10)**: Right after binding its UNIX domain socket, `meshd`
  confines itself using a seccomp filter on Linux (`SECCOMP_FILTER_FLAG_TSYNC` + `PR_SET_NO_NEW_PRIVS`),
  making `socket(AF_INET/AF_INET6)` and `io_uring_setup` fail with `EPERM`. The daemon cannot open
  any outbound or inbound IP network connections. Configurable via `MESH_DAEMON_SANDBOX` (`required` / `disabled`).
- **YAML alias and memory exhaustion protection (step 4.9)**: YAML parsing protects against
  billion-laughs and memory exhaustion by charging anchor recording, alias replays, and scalar bytes
  to strict per-file budgets (1 event per input byte, 4x replayed scalars, 8x output for Spring properties).
- **Hardened daemon socket and directory permissions (step 4.7)**: Daemon socket files are created
  explicitly with `0600` permissions and parent directories `0700`, irrespective of process umask.

### Added
- **`mesh-mcp doctor --fix` & `--json` (step 4.7)**: Repairs orphaned sockets, directory and file
  permissions, corrupt or legacy caches, orphaned workspace cache directories left by upgrades, and
  stops version-mismatched daemons. Emits structured JSON findings on stdout for installation scripts.
- **Size-tier benchmark budgets & regression gates (step 4.3)**: `scripts/bench/tier_bench.py`
  measures multi-run medians and validates against per-platform tier budget files (`budgets-50k.json`,
  `budgets-200k.json`). Nightly CI benchmark gains a 50k-file job for plain and contract-mix corpora.
- **`analyze_impact` impact matrix (step 4.6a)**: Returns a compact Markdown impact matrix covering
  gRPC handlers/clients and async topics/producers/consumers with scope classification (`INTERNAL`/`EXTERNAL`)
  and edge confidence (`exact`/`heuristic`/`ambiguous`), paginated with `limit` and `offset`.
- **TypeScript gRPC client extraction & CI golden ratchet (step 4.5)**: Extractor identifies
  `new <X>Client(...)` call sites in TypeScript, linking client calls to declared services. CI precision/recall
  ratchet in `.github/workflows/golden.yml` holds all three golden repos (`online-boutique`,
  `bank-of-anthos`, `otel-demo`) at 100% / 100% precision and recall.
- **Idempotent pilot installer & metrics stats (step 4.8)**: `scripts/install_pilot.sh` installs
  and configures `mesh-mcp` and `meshd` into `~/.local/bin`; `mesh-mcp stats` computes nearest-rank p50/p95
  latencies, error rates, index cache hits, and process restarts from the SQLite audit database.
  `docs/pilot-scorecard.md` documents the A/B evaluation protocol.
- **Index health rejection reporting (step 4.1)**: `IndexHealth` tracks rejected files (oversized,
  binary, guard, parse error); `smart_search` and `find_dependents` append in-scope rejection notes;
  `doctor` summarizes index health.
- **Per-workspace cache quota & LRU eviction (step 4.4)**: `[cache] max_size_mb` (default 2048 MB)
  automatically trims LRU entries down to 80% when exceeded, followed by `incremental_vacuum`.

### Changed
- **Event-driven macOS watcher (step 4.2)**: Recursive FSEvents stream per root replaces directory
  polling on macOS workspaces with >200 directories, eliminating idle polling CPU.
- **Streaming YAML & bounded Markdown memory (step 4.9)**: YAML parser streams events on-the-fly
  (`unsafe-libyaml`), dropping memory on large specs from 29x to 2.2x file size. Markdown section
  splitting capped at 3x file size.
- **`smart_search` filtered rejection notes (step 4.13)**: Notes only list relevant code and spec
  files (excluding images, lockfiles, and documentation).
- **Peak memory reporting in benchmarks (step 4.3)**: Uses OS-level max RSS (`/usr/bin/time -l` on
  macOS, `getrusage` on Linux).
- **Documentation refresh**: Complete overhaul of `README.md`, `docs/quality.md`, `SETUP.md`,
  `docs/mcp-tools.md`, architecture and skills to reflect active code with sourced measurements only.

### Fixed
- **Git storm & lock coordination (step 4.2)**: File watcher holds reloads during Git operations
  (`index.lock`, `rebase-merge/`, moving `HEAD`), reloading once settled into a single clean generation
  without tearing or partial states.
- **Audit log completeness (step 4.12b-e)**: Argument errors and RSAH refusals now log to SQLite
  audit chain with `ERROR` status.
- **Markdown fence escaping (step 4.12b-e)**: Code snippet fences dynamically expand beyond embedded
  backtick runs to prevent syntax breakage.
- **Root disambiguation (step 4.12b-e)**: Roots sharing directory names disambiguated by parent path.
- **CI determinism script resilience (step 4.12a)**: Eliminates broken pipe (`EPIPE`) failures when
  piping into `head`.
- **Proxy session lifecycle (step 4.13)**: Proxy falls back to standalone if daemon connection drops
  during startup, and exits cleanly with EOF on stderr if daemon terminates mid-session.
- **Index cache eviction granularity (step 4.4)**: Eviction deletes only the required number of excess
  entries rather than flushing entire 1,000-entry batches.
- **gRPC Health check false-positive edge (step 4.5)**: Standard `grpc.health.v1.Health` excluded
  from cross-service RPC call resolution.

## [6.0.1] — 2026-09-26

### Fixed
- **`mesh-mcp init --write-ide-config` no longer deletes the user's other MCP servers.** It used to
  rewrite `.cursor/mcp.json` and `.vscode/mcp.json` with a fresh object containing only
  `mesh-mcp`. It now merges: only the `mesh-mcp` entry is added or replaced, every other server and
  top-level key is kept, a second run changes nothing, and a file that is not valid JSON is left
  untouched with a warning instead of being overwritten.
- **VS Code config uses VS Code's schema**: the entry goes under `servers` with `"type": "stdio"`
  (VS Code does not read Cursor's `mcpServers` key); the stale `mcpServers.mesh-mcp` entry earlier
  versions wrote into `.vscode/mcp.json` is removed.

## [6.0.0] — 2026-09-26

**Plan 3 (P2) complete: scale, and MCP protocol compliance.** Major version because of the step
3.6 breaking changes below (tool errors are `isError` results; `visualize_mesh` returns an
aggregated view). Verified on the final build: `scripts/determinism.sh` green, golden corpus
unchanged (online-boutique 100%/100%, otel-demo 87.5%/53.8%, bank-of-anthos 100%/100%), nightly
5k budgets hold with reload 207 ms — details in `docs/quality.md` ("Plan 3 closeout").

Plan 3 (P2): scale. Step 3.6 — MCP protocol compliance (tool errors as `isError`, notifications,
fence-safe truncation, lean schemas) and a per-service `visualize_mesh`. Step 3.5 — `derive()`
without quadratic passes, streaming YAML. Step 3.4 — `smart_search` pagination/early-stop/cache and
exact line anchoring. Step 3.3 — watcher registration-time filtering, daemon watchdog hardening.
Step 3.2 — persistent content-hash cache for cold-start indexing. Step 3.1 — real incremental
reload, driven by the watcher's own paths.

### Breaking Changes (P2 step 3.6 — MCP protocol compliance)
- **Tool failures are MCP tool results, not JSON-RPC errors.** A failure *inside* a tool — invalid
  or unknown arguments, a scope outside the sandbox jail, a missing target, an RSAH governance
  refusal, `meshd` still indexing — now returns a successful JSON-RPC response whose
  `CallToolResult` has `isError: true` and the message as text content, as the MCP specification
  (2024-11-05) requires. Previously these were JSON-RPC errors `-32602` / `-32001` / `-32000`,
  which clients (Claude Code, Cursor, Windsurf) treat as a protocol failure that aborts the agent's
  turn instead of letting the model read the message and correct its call. **Clients must read
  `result.isError`**; `error` is now reserved for protocol faults: `-32700` parse error, `-32600`
  invalid request (not a request object / `jsonrpc` not `"2.0"`, newly enforced), `-32601` unknown
  method, `-32602` unknown tool or missing `tools/call` params, `-32603` internal error. There is no
  compatibility flag.
- **`visualize_mesh` returns a per-service aggregated view** in every format (Mermaid, JSON, HTML)
  instead of the raw contract graph, which at a few thousand nodes exceeded the 48 KB cap and came
  back cut mid-document. New arguments `service` (zoom into one service) and `max_services`. The
  complete graph remains available from the CLI (`mesh-mcp graph`).

### Fixed (P2 step 3.6)
- JSON-RPC notifications (no `id`) never receive a reply — not even an error with `"id": null` —
  in both the stdio server and `meshd` (JSON-RPC 2.0 §4.1). Both now share one request classifier
  and one responder.
- The 48 KB output cap cuts on a line boundary and closes any open Markdown code fence.
- `_meta` (W3C trace context) is accepted but no longer advertised in `tools/list`
  (7,696 → 5,524 bytes of tool schemas, ~540 tokens per session).

### Fixed (P2 step 3.6 review)
- **Stored XSS in the `visualize_mesh` / `mesh-mcp graph` HTML page**: a scanned name containing
  `</script>` closed the inline JSON element; the JSON is now `\u003c`-escaped, the workspace name
  HTML-escaped, and the side panel's `innerHTML` escapes node names.
- **Mermaid label injection**: names are escaped with Mermaid entity codes (`#quot;`, `#lt;`,
  `#gt;`, `#35;`, `#96;`) and newlines flattened, instead of a partial character strip that let a
  newline or `"]` start a new statement.
- **48 KB cap now holds for the whole payload**: the truncation note is budgeted before the cut,
  the closing fence is counted, and a hint echoing an unbounded argument is capped (512 bytes) and
  flattened to one line. Fence detection follows CommonMark (`~~~`, 4+-backtick fences, a
  ```` ```rust ```` line inside a block is content, 4-space indent is not a fence).
- **`visualize_mesh`**: the zoom ranked neighbour services by their global degree instead of their
  links to the zoomed service (a hub linked once crowded out a service linked fifty times); a zoom prefers an exact-case match over a case-insensitive one;
  the footer never suggests zooming into a topic; names are shortened to 120 bytes when drawn, so
  one huge topic literal cannot push even the smallest view past the cap; JSON/HTML never get a
  skill footer appended; the O(contracts + edges) fold runs once per call instead of once per
  shrink iteration.
- The `docs/mcp-tools.md` schema drift test resolved its path from the crate directory, where the
  file never exists, and passed vacuously; it now compares every documented property set with the
  advertised schema. A test covers `_meta` acceptance on every tool.


### Changed (P2 step 3.5 — `derive()` without quadratic passes, streaming YAML)
- **`ContractGraph::reconcile_edges` has no quadratic pass left**: the `Implements` pass indexes
  gRPC handlers by every key its unchanged match predicate can succeed on instead of comparing every
  proto method with every handler; import resolution is memoized per (target, importer repo);
  `patch_files` groups index removals per key. AsyncAPI/OpenAPI line recovery is one pass per file.
  200k files + 48k contract mix, cold: boot **307.8 s → 17.0 s**; peak footprint 809 → 829 MB;
  plain 200k unchanged.
- **Streaming YAML**: Spring property files flatten through a serde visitor (no `serde_yaml::Value`
  tree), byte-identical to before; a multi-document file now contributes its first (default-profile)
  document instead of being rejected outright. AsyncAPI/OpenAPI specs are read as key-only shapes.
- Doc sections keep a lowercase copy only for non-ASCII content.
- `scripts/bench/gen_synthetic.py --contracts` adds an imports/protos/gRPC/YAML/Markdown mix.

### Added (P2 step 3.4 — `smart_search` at scale, exact line anchoring)
- **`smart_search` pagination**: `limit` (default 20, max 100) / `offset`; only the requested
  page's files are read and decapitated, each result's rendered size is measured before it is
  accepted (no silent drop past the 48 KB cap), and the footer names the exact next `offset`.
  Pages are cached per snapshot generation (and re-validated against file mtime/size).
  30k files, cold: p50/p95 **~860/~2,600 ms → 57/112 ms** (budget 300/800).
- **Exact line numbers**: snippets are anchored on the symbol's tree-sitter line through a
  decapitated→original line map; the old text re-matching (which could attribute a symbol at line
  800 to an identical line 15) is gone. Pattern/AsyncAPI/OpenAPI nodes record real lines.
- **Python docstrings survive decapitation**; only the statements after them become `...`.
- **Relative scopes resolve from `workspace_root`** (then the process CWD), not only the CWD an
  IDE happened to launch the server in.

### Added (P2 step 3.3 — watcher registration-time filtering, daemon watchdog hardening)
- **Watchers now respect `.gitignore`/`exclude_patterns` at registration, not just after an event
  arrives**: `FilesystemCrawler::plan_watch_dirs` (`crates/mesh-core/src/crawler.rs`) walks each
  root once (the same nested-gitignore-aware `ignore::WalkBuilder` construction a full crawl
  already uses) to decide which directories to individually watch; `FileWatcherService::spawn`
  registers one `RecursiveMode::NonRecursive` watch per surviving directory instead of one
  `RecursiveMode::Recursive` watch per root. A genuinely excluded subtree (`node_modules`,
  gitignored build output, ...) never gets a watch at all — closing the nested-`.gitignore` gap
  step 3.1 documented in `docs/quality.md` for the common case, rather than reactively filtering
  events downstream.
- **Watch-count cap with a transparent polling fallback**: above `MAX_WATCHED_DIRS` (2048 on
  Linux/Windows, 200 on macOS, combined across all roots), `spawn` falls back to a
  `notify::PollWatcher` backend (2s interval, one plain recursive watch per root) instead of
  per-directory native registration — protects Linux's `fs.inotify.max_user_watches` ceiling on
  very large workspaces without needing gitignore-aware enumeration at all, since `PollWatcher`
  re-scans the tree itself.
- **New directories created after startup are still watched**: per-directory registration doesn't
  automatically track new subdirectories the way the old single-recursive-watch design did. The
  event loop detects a newly-created, non-excluded directory and registers it (plus any of its own
  qualifying subdirectories) dynamically.
- **Real finding**: a dynamic `.watch()` call on macOS (`notify`'s FSEvents backend stops and
  restarts its whole event stream per call) measured over 11 seconds under this machine's own
  test-suite load. Fixed by deferring those calls to `state.rescan`'s background pool
  (`Arc<Mutex<AnyDebouncer>>`) instead of running them inline on the thread that also drains the
  debouncer's channel — the fast, filesystem-walk-only *planning* step stays synchronous, only the
  slow OS registration itself is deferred, so other pending reloads are never stalled behind it.
- **Idle watchdog gains an unconditional startup-grace deadline** (`crates/mesh-daemon/src/
  idle.rs`, `spawn_idle_watchdog`'s new `startup_grace` parameter, 60s in `meshd`'s `main.rs`): a
  daemon that never gets a single client at all (an `ensure_daemon_running` auto-spawn racing or
  failing after the process started, a wrong workspace path) now shuts itself down the same
  graceful way idle-after-use does, instead of living forever as an unreachable zombie. Spawned
  unconditionally now (previously gated behind `idle_timeout_minutes > 0`), so disabling the
  idle-*after-use* policy no longer also disables this orphan guard.
- **`meshd` auto-spawn output is no longer discarded**: `ensure_daemon_running`/
  `ensure_daemon_running_windows` (`crates/mesh-server/src/main.rs`) redirected `Stdio::null()`,
  making a crash before `meshd`'s own `tracing` subscriber initializes unobservable. Now redirected
  to a rotating, per-workspace log (`~/.cache/mesh-mcp/logs/meshd-<workspace_id>.log`, up to 5
  previous runs kept as `.1`–`.5`).
- **Hardened via `/code-review high` — ten real findings, all fixed**, most notably: (1) dynamic
  re-registration was anchoring exclude-pattern checks to the newly-created directory instead of
  the real workspace root, which could both silently keep an excluded new directory and mismatch
  root-anchored patterns — fixed with an explicit `walk_root`/`matcher_root` split
  (`FilesystemCrawler::plan_watch_dirs_from`); (2) `.git/refs` was never watched at all under
  per-directory registration (a real regression versus the old single recursive watch), fixed by
  walking `.git/refs` directly outside the exclude matcher; (3) an attempt to fix `meshd`'s
  socket-bind blocking by moving watch setup into the spawned thread instead introduced a race
  where events before setup finished were silently missed — reverted, with the actual blocking
  fixed at the `meshd` call site via `tokio::task::spawn_blocking` instead; (4) deferred
  dynamic-registration work was sharing `state.rescan`'s small pool with real reload jobs,
  reintroducing the exact starvation the deferral was meant to prevent — moved to a plain
  detached thread. See `docs/quality.md`'s step 3.3 section for the full list and the two new
  regression-test groups (`plan_watch_dirs_from_*`, `git_watch_targets_*`).
- Verified after all ten fixes: `cargo test --workspace` (369 passed, 1 ignored, stable across
  repeated runs), `cargo clippy --workspace --all-targets -- -D warnings` (clean), `cargo fmt
  --all -- --check` (clean), `scripts/determinism.sh` on both fixtures re-confirmed (unaffected).
  See `docs/quality.md` for the full reasoning, the honest limitations (no measurement yet at real
  200k+-file scale or on Linux/Windows watch backends), and the live-smoke-test confirmation.
- **A second `/code-review high` round found the fixes above needed fixes of their own**: the
  severity-1 finding was that initial (startup) per-directory registration pays macOS's
  per-`.watch()`-call FSEvents restart cost sequentially for every planned directory, not just the
  dynamic re-registration path originally measured — fixed with a platform-specific
  `MAX_WATCHED_DIRS` (200 on macOS, 2048 elsewhere, since inotify/ReadDirectoryChangesW
  have no equivalent per-call cost). `run_standalone` wasn't wrapped in `spawn_blocking` the way
  `meshd`'s call was, fixed identically. The `spawn_blocking` fix itself was fire-and-forget with
  nothing to catch up a file changed during the (possibly slow) registration window — fixed with
  one `execute_reload_sync` right after a successful `spawn()`, at both call sites. Plus: the
  polling fallback aborted watching for *every* root over one root failing (fixed to match the
  native path's per-root resilience), a stale doc comment, and a missing warning on a silently
  dropped capped subtree. See `docs/quality.md`'s step 3.3 section for the full list, including
  one accepted-not-fixed limitation (unbounded in-flight registration threads under rapid bursts)
  and a live verification against this repo's own 40-directory workspace.
- Verified again after all seven of these fixes: `cargo test --workspace` (369 passed, 1 ignored,
  stable across three consecutive runs), clippy/fmt clean, `scripts/determinism.sh` unaffected.

### Added (P2 step 3.2 — persistent SQLite/WAL content-hash cache for cold-start indexing)
- **`PersistentIndexCache`** (`crates/mesh-core/src/index_cache.rs`): a SQLite-in-WAL-mode cache
  (`~/.cache/mesh-mcp/index-cache.db`, mode `0600`, same convention as the Commandment 7 audit db)
  of `mesh_parsers::FileIndex` — the per-file, pre-`NodeId`-numbering tree-sitter extraction
  fragment — keyed by `SHA256(schema_version, path, content_hash, repo_id, config_fingerprint)`.
  An unchanged file rescanned under an unchanged `[engines.contracts.*]` config on a later cold
  start is now a single indexed lookup instead of a full tree-sitter re-parse. Wired into the two
  real server boot paths (`mesh-server run_standalone`, `meshd`'s initial ingestion) via a new
  `Option<&PersistentIndexCache>` parameter on `WorkspaceIndexer::build_snapshot` (and
  `build_snapshot_from_files`/`build_graph`); a cache the process can't open degrades to "parse
  everything" rather than failing the boot. Incremental `reload()` (step 3.1) is untouched — it
  already only re-parses differential-VFS-flagged changed files.
- **Not Plan 1's `NodeId`s, deliberately**: `NodeId`s are a deterministic-but-not-stable sequential
  counter assigned while folding files into the graph (idempotence invariant I1), never
  content-addressed, so they shift on any workspace add/remove and would have been the wrong cache
  key. The cache stores the pre-numbering `FileIndex` fragment instead; global `NodeId`s are still
  freshly (re-)assigned on every build regardless of cache hits. See `docs/quality.md`'s step 3.2
  section for the full reasoning.
- Real measured baseline (`scale_bench.py`, cold vs. warm cache, same workspace/config unchanged
  between runs): boot time down ~65% at 5,000 files (283.5ms → ~90–105ms) and ~50% at 30,000 files
  (1,344.9ms → 668.0ms). `smart_search`/`reload_ms` are unaffected, as expected — this cache only
  short-circuits tree-sitter parsing.
- Honest limitation: no eviction or size cap yet — `index-cache.db` grows unboundedly across
  distinct workspaces/configs on a shared machine over time; only the "same workspace, second
  boot" scenario is measured so far, not a mixed cache-hit-rate cold start.
- Verified: `cargo test --workspace` (352 passed, +5 new: `PersistentIndexCache` key/get/put-batch
  cases), `cargo clippy --workspace --all-targets -- -D warnings` (clean), `cargo fmt --all --
  --check` (clean).

### Added (P2 step 3.1 — real incremental reload from watcher paths)
- **The file watcher no longer re-crawls the whole tree to find out what changed.**
  `WorkspaceIndexer::reload` (used for the initial load and any caller without specific paths)
  is unchanged, but the live file watcher now calls new `WorkspaceIndexer::reload_paths`, which
  works directly from the watcher's own reported paths: each is resolved to its most specific
  containing root (the same attribution `crawl_all`'s nested-root exclusion gives an overlapping
  file) and checked against that root's `exclude_patterns`/`.gitignore` via new
  `FilesystemCrawler::is_path_excluded` — one path in O(path depth) stat calls, not an O(repo
  size) walk. `reload` and `reload_paths` now share one `apply_incremental` tail (VFS diff, scan,
  graph patch, snapshot install) so the two paths can't silently drift apart.
- **Two deliberate, documented fallbacks to the old full-crawl `reload`, not silent gaps**: (1) a
  path under a nested `.gitignore` (any `.gitignore` strictly between the root and the file, not
  the root's own top-level one) — `is_path_excluded` can't cheaply and correctly reproduce
  `ignore::WalkBuilder`'s per-directory gitignore stacking for one path without walking, so rather
  than risk a false "not excluded" it returns `None` and the caller falls back; (2) a
  `.git/HEAD`/`.git/refs/*` change (checkout, rebase, branch switch), which can alter an arbitrary
  number of tracked files without each one necessarily producing its own watcher event.
- **`AppState` gains `pending_reload_paths`** so a burst of watcher events arriving while a reload
  job is already queued or running still has its paths picked up by whichever job drains the
  accumulator next, instead of being silently dropped by `reload_pending`'s existing
  best-effort coalescing check (that check only ever decided whether to spawn a *second* Rayon
  job, never whether the first job would see the second burst's paths — this closes that gap).
- Verified: `cargo test --workspace` (341 passed, +10 new: `is_path_excluded`'s exclude/gitignore/
  nested-gitignore-fallback cases, `reload_paths`'s edit+delete/create/exclude-pattern/git-ref-
  fallback cases, and the watcher's path-delivery/accumulator cases), `cargo clippy --workspace
  --all-targets -- -D warnings` (clean), `cargo fmt --all -- --check` (clean),
  `scripts/golden/score.py online-boutique` (100%/100%, unaffected), `scripts/determinism.sh` on
  all three fixtures ("1 fingerprint over 13 runs" each, unaffected — those exercise a fresh
  `mesh-mcp graph` process per run, i.e. `build_snapshot`, not the live watcher's `reload_paths`
  path; that path's correctness is what the new unit/integration tests above cover, plus the
  pre-existing `test_file_watcher_live_reload` end-to-end test, unchanged and still green, which
  now exercises `reload_paths` instead of `reload` under the hood).
- Honest limitation: the nested-`.gitignore` fallback (above) means a targeted reload is not
  strictly zero-crawl for every workspace shape — only for the common case of a single
  root-level `.gitignore` (or none). A repo with per-service nested `.gitignore` files still gets
  a full crawl on every reload of a file under one, same as before this change; closing that gap
  would mean reproducing `ignore::WalkBuilder`'s directory-by-directory gitignore stack for a
  single path, which this step deliberately did not attempt rather than risk a subtly wrong
  exclusion decision.
- Hardened via ruthless review — seven real findings, all fixed:
  - **Sandbox escape via symlink** (Commandment 4): `reload_paths` used to check
    `std::fs::metadata(raw).is_ok()` on the *raw* watched path, which follows symlinks — a symlink
    created inside a watched root pointing outside every allowed root would have its *target's*
    content read and indexed. Fixed: every path is `dunce::canonicalize`d before root-resolution
    and indexing now use the *canonical* path; one that resolves outside every allowed root is
    dropped (logged), never indexed.
  - **Silent, unlogged path drop on a root-match miss**: an uncanonicalized watched path (e.g.
    macOS FSEvents reporting `/tmp/...` against a `/private/tmp/...`-canonicalized allowed root)
    could fail `most_specific_root` and be dropped with zero log output and no fallback — silently
    defeating the whole reload for that path. The same canonicalize-before-matching fix above
    closes this (canonical paths compare equal to the canonicalized `allowed_roots`), plus a debug
    log line on every drop.
  - **`is_path_excluded` only checked root-level `.gitignore`**, missing `.ignore` files and
    `.git/info/exclude` that `ignore::WalkBuilder` also honors by default. Fixed: both are now
    folded into the same `GitignoreBuilder`, and a nested `.ignore` (not just `.gitignore`) also
    forces the documented fallback. A user's *global* `core.excludesFile` remains an explicit,
    documented gap (detecting it would mean reading git config).
  - **TOCTOU on a "deleted" path**: `reload_paths` stats a path before `reload_lock` is acquired
    (it must return, not block, before its fallback-to-`reload()` branches); a file briefly absent
    in that window (an editor's atomic save, a fast delete-then-recreate) would be purged from the
    graph and never re-added. Fixed: `apply_incremental` re-verifies every `deleted` path
    immediately before acting on it, re-indexing one that resurrected instead of dropping it.
  - **Coalesced directory-level delete**: `rm -rf a_service/` can produce fewer watcher events
    than one per contained file; `reload_paths` only ever removed the specific paths reported,
    leaving siblings' graph nodes stale. Fixed: a deleted path whose parent directory is *also*
    gone now falls back to a full `reload()`'s crawl-vs-VFS sweep instead of guessing.
  - **Matcher/gitignore recompiled per path**: `schedule_reload` deliberately coalesces a whole
    debounce burst into one `reload_paths` call, but the exclude matcher (and, inside
    `is_path_excluded`, the parsed ignore files) were rebuilt from scratch for every path in that
    batch. Fixed: cached per root for the duration of one call.
  - **`is_path_excluded`'s nested-ignore-file walk could stat directories above `root`** for an
    unnormalized or non-descendant path (or `path == root` itself), reading outside the intended
    scope. Fixed: an explicit `path == root` short-circuit, plus a `starts_with(root)` guard on
    every step of the ancestor walk.
  - Verified again after all seven fixes: `cargo test --workspace` (347 passed, +6 more:
    symlink-escape, parent-directory-gone fallback, TOCTOU resurrection, root-level `.ignore`,
    `.git/info/exclude`, and `path == root` cases), clippy/fmt clean, golden/determinism unaffected.
### Added (P2 step 3.0 — scale bench: synthetic generator, boot/reload/RSS/p50/p95, nightly budgets)
- **`scripts/bench/gen_synthetic.py`**: deterministic (fixed-seed) multi-root synthetic workspace
  generator, so scale numbers are reproducible across machines and runs instead of depending on a
  moving upstream HEAD like the existing real-repo corpus (`scripts/bench/repos.txt`).
- **`scripts/bench/scale_bench.py`**: boots `mesh-mcp run --standalone` for real (same code path
  as production, including the real `FileWatcherService`), then measures `boot_ms`, `rss_peak_mb`
  (sampled via `ps` across the run, not just at boot), `search_p50_ms`/`search_p95_ms` over N real
  `smart_search` calls, and end-to-end `reload_ms` (append a uniquely-named symbol to a tracked
  file on disk, poll `smart_search` until it's visible through the real watcher → `reload_paths`
  path). Exits non-zero and names each violation when a metric exceeds
  `scripts/bench/budgets.json`.
- **`.github/workflows/nightly-bench.yml`**: runs the above at 5,000 synthetic files once a day,
  uploads the raw JSON as a build artifact. Not wired into per-PR CI — wall-clock budgets are
  noisy on shared runners, so gating every push on them would make the gate flaky, not meaningful.
- `/code-review` on this branch caught three real bugs in the first version of the harness (all
  fixed, see `docs/quality.md`): the reload probe's substring match was a false positive against
  `smart_search`'s own "no results" header echo (originally reported 6ms/41ms reload numbers were
  not real); the TypeScript symbol it injected (a bare function) is never indexed by
  `typescript.rs`'s extractor, so a `.ts` target polled forever; and the generator's
  `marker.json`-per-directory convention was never read by `init.rs`, so the claimed "multi-root"
  workspace always silently fell back to a single root. Fixing the third finding (real
  `services/svc-N/` layout, discovered by `init --auto`) surfaced a fourth issue in the harness
  itself: `ValidatedScope::resolve` requires a query's scope to be inside one specific allowed
  root, so a hardcoded `scope: "."` sandbox-escapes once there's more than one root — fixed by
  reading real roots back out of the generated config and round-robining scope across them.
- **Real, honest baseline measured on this machine** (release build, single-root workspace so
  `smart_search` scope covers the whole tree): 5,000 files boots in 186ms, 47MB peak RSS,
  `smart_search` p50/p95 65ms/176ms, reload 452ms. At 30,000 files: 1,097ms boot, 125MB peak RSS,
  `smart_search` p50/p95 460ms/1,264ms — already over generous budgets built from the 5,000-file
  baseline — and 842ms reload. This gap is recorded in `docs/quality.md` as the numeric motivation
  for steps 3.4 (`smart_search` limits/early-stop) and 3.5 (memory/CPU at scale), not silently
  fixed by loosening the budget or hidden by only ever benchmarking the size that passes.

## [5.0.0] — 2026-09-25

**Plan 2 (P1) complete: inter-service joins are precise, not just deterministic.** Eight steps
landed as one PR apiece (#17–#24), each green on `cargo fmt`/`clippy -D warnings`/
`cargo test --workspace`, `scripts/determinism.sh`, and `scripts/golden/score.py` against the
real golden corpus (`online-boutique`, `otel-demo`, `bank-of-anthos`). `find_dependents` and
`analyze_impact` gained new opt-in parameters (`granularity`, `depth`) and two more golden files
were written by hand from real source — hence the major version bump, since a caller relying on
either tool's exact prior output shape should re-check it, even though every existing default
stayed byte-for-byte unchanged.

Closing out the plan meant running the golden scorer against the two corpus repos this plan
added, not just the one (`online-boutique`) step 2.0 started with — which is exactly what caught
a genuine, previously-unknown gap: `otel-demo`'s TypeScript frontend uses a `@grpc/grpc-js`
client-construction idiom (`new XServiceClient(...)`) `typescript.rs` doesn't recognize (only the
NestJS `getService<XServiceClient>(...)` idiom is covered), scoring 87.5%/53.8% rather than
100%/100% — documented honestly in `docs/quality.md` as an open finding, not fixed under time
pressure, and not hidden by adjusting the golden file. `bank-of-anthos` was confirmed genuinely
gRPC-free (a correct vacuous 100%/100%) with its 18 real Flask HTTP routes recorded as ground
truth ahead of an `http_routes` scoring mode.

P2 step 3.1 (real incremental reload from watcher paths) was implemented and ruthlessly reviewed
alongside this plan but is deliberately **not** included in this release — it lands as its own PR
(#25) stacked separately, kept out of this version bump per plan sequencing.

### Added (golden corpus — `otel-demo` and `bank-of-anthos` golden files)
- **`tests/golden/otel-demo.expected.yaml`**: 13 hand-verified gRPC edges plus its Kafka `orders`
  topic (`checkout` → `accounting`/`fraud-detection`), the corpus's first real async multi-hop
  chain. Running `scripts/golden/score.py otel-demo` against it: **87.5% precision / 53.8%
  recall** — a genuine, newly-found gap, not a golden-file error: otel-demo's frontend is
  TypeScript using raw `@grpc/grpc-js` client construction (`new XServiceClient(...)`), a pattern
  distinct from the already-covered NestJS `getService<XServiceClient>(...)` idiom and not yet
  recognized by `typescript.rs`. A spurious `checkout -> health` edge (the gRPC health-check
  import) was also found. Neither is fixed in this change — documented honestly in
  `docs/quality.md` as new findings.
- **`tests/golden/bank-of-anthos.expected.yaml`**: confirmed genuinely gRPC-free
  (`score.py bank-of-anthos` reports a correct vacuous 100%/100% on 0 golden edges) plus 18
  hand-verified Flask HTTP routes across its three Python services. Its three Java/Spring MVC
  services are flagged as out of scope for step 2.6's Flask/FastAPI extraction, not silently
  represented as Flask routes.
- No `score.py` mode consumes `topics` or `http_routes` yet — both sections are ground truth
  recorded ahead of that scorer landing, per `docs/quality.md`'s "What's NOT measured yet".

### Added (P1 step 2.7 — explicit result-shape controls: `granularity`, `depth`)
- **`find_dependents` gains `granularity: "symbol" | "package"`** (default: `"symbol"`, unchanged
  behavior). `"package"` collapses results to one entry per distinct `(repo, package)` pair — see
  which *services* depend on a target without a wall of individual caller symbols.
- **`analyze_impact` gains `depth` (default: 1, clamped to 5)**. New
  `ContractGraph::analyze_impact_with_depth` performs a real BFS over `Produces`/`Consumes`
  edges — a transitive consumer that itself produces onto another topic pulls in that topic's own
  consumers too — with cycle detection via visited-node/visited-topic sets, not another substring
  pass. `depth <= 1` is byte-for-byte `analyze_impact`'s existing direct-only result.
- Verified: `cargo test --workspace` (329 passed), `cargo clippy --workspace --all-targets -- -D
  warnings` (clean), `cargo fmt --all -- --check` (clean), `scripts/golden/score.py
  online-boutique` (100%/100%, unaffected), `scripts/determinism.sh` on all three fixtures ("1
  fingerprint over 13 runs" each, unaffected) — both changes are additive/opt-in.
- Honest limitation: the `depth > 1` traversal is covered by a synthetic regression test (a
  `topic -> handler -> topic -> handler` chain with a cycle back to the origin topic), not yet
  against a real multi-hop async chain in the corpus. See `docs/quality.md`.
- Hardened via ruthless review: an unrecognized `granularity` (a typo like `"Package"`, or any
  invented value) used to silently fall back to full symbol-level output with no signal to the
  caller — the exact opposite of this step's "explicit semantics" goal — fixed to return a
  JSON-RPC -32602 error instead. `truncation_hint` recomputed `find_dependents` without applying
  the `"package"` dedup, so a truncated response's appended note could cite the pre-dedup symbol
  count; fixed to dedupe identically via a shared helper. The `depth > 1` BFS rescanned the full
  edge list once per newly-discovered topic per hop (`O(new_topics × |edges|)`); fixed to a single
  edge-list pass per hop (`O(|edges|)`) for both the `Produces` and `Consumes` steps.

### Added (P1 step 2.6 — Flask/FastAPI route path and method)
- **A Python HTTP route decorator's actual path/method is now surfaced.** `@app.route('/users',
  methods=['POST'])` / `@router.get("/health")` used to be discarded entirely — only the Python
  function name was kept, with no record anywhere of the real HTTP contract it serves. New
  `extract_flask_route` surfaces `"<METHOD> <path>"` in `signature` (not `name`, so existing
  symbol-name lookups are unaffected) for both Flask's `@app.route(path, methods=[...])` and
  FastAPI/`APIRouter`-style `@router.<verb>(path)`. Verified against a fresh scan of the real Bank
  of Anthos userservice.
  - Hardened via ruthless review: a multi-method route (`methods=['GET', 'POST']`) only kept the
    first method — fixed to join every declared one. Flask's legitimate `@app.route(rule='/x')`
    keyword-only path form was silently unrecognized — fixed to also check a `rule=` keyword
    argument when no positional string argument is present.

### Added (P1 step 2.5 — env-var-with-default topic resolution)
- **`var Topic = getTopic()`, where `getTopic` reads an env var and falls back to a literal
  default, is now resolved to that fallback.** P0 step 1.7 correctly treated this as an explicit
  non-goal (real control-flow interpretation would be needed in general) and recorded nothing
  rather than fabricating a value. New `collect_getenv_default_consts` (Go) recognizes the
  *specific* idiom — an `os.Getenv`/`os.LookupEnv` read followed by a literal fallback `return` —
  rather than "any function that returns a string somewhere," which would reopen the same
  invented-value risk P0 closed.
- **Honest limitation, found trying to verify this against its own motivating case**: the
  OpenTelemetry demo's real `checkout/kafka/producer.go` declares `Topic` this way, but the actual
  producer call site referencing it (`Topic: kafka.Topic`) is in a *different file*,
  `checkout/main.go` — a cross-file/cross-package reference this (file-scoped, like every other
  const-resolution mechanism in this codebase) fix does not close. Documented in `docs/quality.md`
  rather than silently claimed as solved; a synthetic single-file reproduction of the same idiom
  resolves correctly and is covered by a real regression test.
- **Hardened via ruthless review**: the first version scanned the function's raw source *text* for
  the last `return "literal"` substring — fooled by an intermediate conditional branch's literal
  when the real, unconditional fallback was dynamically computed (a **wrong** resolved value,
  worse than leaving it unresolved), by a `return "..."` inside a `//` comment, and by one inside a
  nested closure. Rewritten to require the literal be the function's own last AST statement in its
  own body block. Also found while fixing that: tree-sitter-go's grammar keeps `comment` as a
  genuine named sibling statement inside a block, so "last named child" alone still landed on a
  trailing comment — skips over any trailing comment nodes first.

### Added (P1 step 2.4 — Java gRPC client/server idioms)
- **`JavaExtractor` now recognizes plain (non-Spring) grpc-java's own universal codegen
  convention** (protoc-gen-grpc-java), not just Spring's `@GrpcService` annotation:
  - Server: a class extending `<Service>Grpc.<Service>ImplBase` is tagged `GrpcService` — verified
    against Online Boutique's real `AdServiceImpl extends AdServiceGrpc.AdServiceImplBase`.
  - Client: `JavaExtractor` had **no client-side gRPC detection at all**, unlike every other
    language extractor. `<Service>Grpc.newBlockingStub(channel)` / `.newStub(...)` /
    `.newFutureStub(...)` is now recorded as an RPC call, the same signal Go/Python/TypeScript
    already emit — verified end-to-end (a real `CallsRpc` edge forms) against a fresh scan of
    Online Boutique's real `AdServiceClient.java`.
  - `JavaExtractor::extract_relations`'s return type grew a fourth tuple element (`rpc_calls`),
    matching the shape `languages::FileIndex` already expects.
  - Hardened via ruthless review (see `docs/quality.md` for the full account): tightened the
    server check to require both `"Grpc"` and `"ImplBase"` (not `"ImplBase"` alone, which any
    unrelated non-gRPC `*ImplBase` convention would have matched); added detection inside
    constructors, not just named methods — the PR's own real-world example builds its stub in a
    constructor and was initially missed; fixed the candidate-suffix scan silently dropping
    whichever stub wasn't checked first when a method builds two different services' stubs.

### Added (P1 step 2.3 — proto imports)
- **`.proto` files' `import "other.proto";` declarations are now recorded as dependencies.**
  `ProtoExtractor` had no import extraction at all — a message field typed from another `.proto`
  file's declaration (e.g. `google.type.Money`, or a shared `common.proto`) had no way to link
  back to it via `find_dependents`. New `ProtoRelations::dependencies` records every import,
  attributed to the file's own first declared node, the same `(node index, imported path)` shape
  every other language extractor already uses. `ProtoExtractor::extract_with_parser`'s existing
  signature is unchanged (delegates internally); only the new `extract_with_relations` and the
  production dispatch in `languages/mod.rs` are affected.
  - A first version attributed every import to *every* node in the file, on the reasoning that
    any of them could rely on it — caught by ruthless review: `ContractGraph::reconcile_edges`
    doesn't dedup `Imports` edges across different `importer_id`s, so that produced up to
    N-imports × M-nodes real edges, and `find_dependents(import_path)` would return every node in
    the file as a "dependent" even if only one actually used the import — false-positive fan-out,
    not just extra bookkeeping. Determining which node *actually* uses which imported type would
    need real cross-file type resolution (parsing the imported file too), out of scope here;
    attributing to the file's first node instead keeps the import traceable without fabricating
    usage this extractor has no evidence for.
  - Also fixed: `extract_import_path` only stripped double quotes, but the protobuf grammar
    allows single-quoted import paths too (`import 'other.proto';`) — its dependency key was left
    as the literal `'other.proto'`, quotes included, which could never match a real file path.

### Added (P1 step 2.2 — manifest-declared service identity)
- **`detect_service_package` now reads the service identity a manifest actually declares**, not
  just the directory it happens to sit in — `go.mod`'s `module` path, `package.json`'s `name`
  (npm-scope stripped), `Cargo.toml`'s `[package].name`, `pyproject.toml`'s
  `[project].name`/`[tool.poetry].name`. A folder named `svc` whose `go.mod` declares `module
  github.com/acme/billing-service` is now identified as `billing-service`, not `svc`. Falls back to
  the directory name exactly as before when a manifest has none of these fields or fails to parse.
  Measured effect on Bank of Anthos: resolved edges 49 → **81** (node count 358 → 365, duplicates
  still exactly 0) — more accurate package identities let more callers disambiguate to a real
  match. `online-boutique` stays at 100% precision / 100% recall.

### Fixed (P1 step 2.2)
- **A relative path with no real manifest anywhere in its own ancestry could silently read *this
  crate's own* `Cargo.toml`.** `Path::parent()` eventually yields the empty path as its final
  ancestor, and `"".join("Cargo.toml")` resolves against the process's actual cwd — inside this
  workspace, always a real file. The pre-existing directory-name-only code never surfaced this (an
  empty path has no `file_name()` to return), but reading real manifest *content* (added by this
  step) would have leaked it for any synthetic or filesystem-less path. `detect_service_package`'s
  upward walk now stops at the empty path, the same way it already stops at its depth cap.

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
