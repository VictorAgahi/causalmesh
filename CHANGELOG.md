# Changelog

All notable changes to MeshMCP (`mesh-mcp` / `meshd`) are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). This file starts
at 3.0.0 — there is no reconstructed history before it.

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
