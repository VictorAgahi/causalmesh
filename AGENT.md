# AGENT.md — Operating Directives for AI Agents Working on MeshMCP

> **Repository**: `VictorAgahi/causalmesh` (MeshMCP)
> **Applicability**: any AI coding agent (Claude Code, Cursor, Windsurf, Copilot, ...)
> **Authoritative rules**: the 7 Commandments and code style in [`CLAUDE.md`](CLAUDE.md). This
> file summarises them and adds navigation help; if the two disagree, `CLAUDE.md` wins.

---

## 0. Dogfood MeshMCP for navigation

This project is a code-navigation tool; use it on itself. Before grepping or reading a large
source file (`java.rs`, `go.rs`, `python.rs`, `typescript.rs`, `contracts.rs`, `indexer.rs`, or
anything over ~1,000 lines), prefer the running MeshMCP server's tools: `smart_search` to locate a
symbol, `find_dependents` before changing a shared type, `analyze_impact` / `analyze_grpc` for
cross-service effects, `search_docs` for the docs. Reading a whole large file is the fallback,
not the first move. If the tools are not good enough to navigate this repository, that is a bug
worth reporting.

This repository's own config is `.agents/mesh-mcp.toml`
(`roots = ["../crates/*", "../deploy*", "../docs", "../k8s*"]`). The repository root is not a
root itself, but `smart_search` treats an omitted `scope`, `"."`, `"*"` or the exact workspace
root as "all configured roots". Any other path outside the roots is rejected by the
`ValidatedScope` jail ("Sandbox escape attempt detected"); that is intended behaviour.

Behaviours worth knowing:

- `smart_search`'s `query` matches **declared symbol names** (case-insensitive substring), not
  free text. A keyword like `fn` returns nothing by design; pass `fuzzy: true` for a full-text
  scan of the scope.
- A page stops at about 8 KB of results (`limit` default 20); follow the `offset` given in the
  footer instead of raising `limit`. A single large `include_body: true` result can still hit the
  48 KB cap; then read the exact line range `smart_search` reported.
- The project's `PreToolUse:Read` hook blocks whole-file reads of files over ~300 lines and points
  back at `smart_search`; read with `offset`/`limit` instead.

---

## 1. Engineering rules

1. No mock code, pseudo-code or stubs (`todo!()`, `unimplemented!()`).
2. Every change compiles: `cargo build --workspace`.
3. Clippy is clean: `cargo clippy --workspace --all-targets -- -D warnings`.
4. New behaviour comes with tests: `cargo test --workspace`.
5. No `unwrap()` / `panic!()` in production code (workspace lints deny them); use `?` and
   `thiserror`. Tests may `unwrap`.
6. Numbers in documentation come from a measurement recorded in `docs/quality.md` (date,
   platform, command), or they are not written.

---

## 2. The 7 Commandments (summary of `CLAUDE.md`)

```
[1] NO DYNAMIC ALLOCATION IN THE HOT LOOP
    - mimalloc global allocator; CompactStr (<= 24 bytes inline) for names and IDs
    - RepoId = u16 for roots; FilePath = Arc<Path> interned once per file
    - ArcSwap<MeshSnapshot>: lock-free reads, one atomic snapshot swap per reload

[2] BOUNDED TREE-SITTER (AstGuard)
    - files <= 384 KB (1.5 MB for contracts/schemas); lines <= 1,024 bytes
    - null-byte sniff over the first 4,096 bytes; nesting depth <= 64 before the C parser
    - parse timeouts: indexing 2 s (INDEX_PARSE_TIMEOUT_MICROS, a hang guard),
      smart_search decapitation 500 ms (QUERY_PARSE_TIMEOUT_MICROS); never share one constant
    - indexing parse failures are counted in IndexHealth and retried once, never folded as empty
    - query cursor: match limit 500, 10,000 iteration steps

[3] STDIO ISOLATION & OUTPUT BOUNDS
    - stdout belongs to the StdioFramingActor (BufWriter<Stdout>); no println!/print!/dbg!
      on the server path; logs go to stderr through tracing
    - responses capped at 48 KB with narrowing guidance; smart_search pages ~8 KB

[4] SECURITY JAIL (ValidatedScope)
    - no raw PathBuf/&str paths in query engines; ValidatedScope::resolve*
    - path_clean + dunce::canonicalize; case-folded comparison on macOS/Windows only
    - follow_links(false) on every walk; an escape is classified -32602 and returned to the
      client as an isError tool result

[5] STRICT SCHEMAS & NEGATIVE PROMPTING
    - schemars::JsonSchema + #[serde(deny_unknown_fields)] on every args struct
    - every tool description has a "DO NOT USE ... (use <other tool>)" clause
    - secrets masked as [REDACTED_SECRET: USE_ENV_OR_LOCAL_FALLBACK]

[6] ACTIVE GOVERNANCE (RSAH)
    - stop rules refuse mutating calls on guarded paths, and read calls when
      read_governance_mode = "enforce_refusal"; structured handoff message for the human
    - Git pre-commit hook via `mesh-mcp install-hooks` (contract-first, from proto_dirs)

[7] OS POLITENESS, TRACING & AUDIT
    - reload pool at QOS_CLASS_BACKGROUND (macOS) / nice 10 (Linux) / below-normal (Windows)
    - W3C traceparent trace id recorded in the audit row
    - append-only SQLite audit DB (~/.cache/mesh-mcp/audit.db, 0600), chained SHA-256
```

---

## 3. Codebase map

```
causalmesh/
├── Cargo.toml                 # workspace manifest, release profile (thin LTO), lints
├── mesh-mcp.toml              # annotated example configuration
├── .agents/                   # this repo's own mesh-mcp.toml and coding-agent skills
├── install.sh                 # prebuilt-binary installer (GitHub releases)
├── scripts/                   # install_pilot.sh, determinism.sh, golden/, bench/, test_git_storm.sh
├── deploy/                    # systemd unit, launchd plist
├── bin/mesh-mcp.js            # npm runner (downloads a release binary)
├── docs/                      # architecture, tools, governance, quality (measurements), benchmarks
└── crates/
    ├── mesh-core/src/         # types, contracts (graph), state (snapshot), security (jail),
    │                          # crawler, vfs, watcher (+ watcher/git.rs), index_cache, audit,
    │                          # governance, properties, docs, yaml_stream, health, socket, rescan
    ├── mesh-parsers/src/      # guard (AstGuard), decapitate (LanguageKind), languages/*,
    │                          # markdown (formatter, 48 KB), graph + topology (rendering)
    ├── mesh-server/src/       # main (CLI, proxy), lib (JSON-RPC loop), framing, protocol,
    │                          # indexer (WorkspaceIndexer), watcher, tools/ (6 MCP tools),
    │                          # cli/ (doctor, init, graph, hooks, stats)
    └── mesh-daemon/src/       # main, server (socket / named pipe), idle, sandbox (Linux seccomp)
```

Task-specific guides: [`.agents/skills/README.md`](.agents/skills/README.md).

---

## 4. Verification workflow

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p mesh-server -- doctor
```

If a tool argument changes, update the schema in `docs/mcp-tools.md`
(`test_mcp_tools_md_schema_drift_check` enforces it). A change that can move precision/recall is
checked by `.github/workflows/golden.yml`; a change to indexing order or concurrency by
`scripts/determinism.sh`.

---

## 5. Which MCP tool to use

| Need | Tool | Do not |
| :--- | :--- | :--- |
| Find a function, class or method declaration | `smart_search` | use it for docs or whole files; pass generic keywords expecting a text match (use the symbol name, or `fuzzy: true`) |
| Find who imports or depends on a type, package or contract | `find_dependents` | pass very short generic names (`id`, `err`) |
| Trace a gRPC method from `.proto` to handlers and clients; check wire-format breaks | `analyze_grpc` | use it for Kafka or other async messaging |
| Impact of changing an RPC, service, topic or event | `analyze_impact` | expect test coverage (the graph does not link tests) |
| Read architecture docs and ADRs | `search_docs` | search source code with it |
| See the service topology | `visualize_mesh` | raise `max_services` to see everything; zoom with `service` |
