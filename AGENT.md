# AGENT.md — Autonomous Agent Operating Directives for MeshMCP

> **Target Codebase**: `causalmesh/mesh-mcp` (RFC-001 Rev. 2.9.0)  
> **Applicability**: All AI Coding Agents (Antigravity, Claude Code, Cursor, Windsurf, Copilot)

---

## 0. P0 — Dogfood mesh-mcp for Codebase Navigation

**This project is itself a code-navigation tool. Use it on itself.** Before reaching for `grep`,
`ripgrep`, or `Read` on a large source file (`java.rs`, `go.rs`, `python.rs`, `contracts.rs`, or
any other file north of ~1,000 lines), prefer the running MeshMCP server's own tools:
`smart_search` to locate a symbol/definition, `find_dependents` to find callers/importers before
changing a shared type, `analyze_impact` to check blast radius before editing a hot file,
`analyze_grpc` for gRPC schema tracing, `search_docs` for architecture/ADR lookups. Brute-forcing
a 1,000+ line parser file with `Read` end-to-end is a fallback for when the MCP tools genuinely
can't answer the question (e.g. reading exact surrounding context to edit), not the default first
move. This is both a real quality bar (if the tools aren't good enough to navigate this repo,
that's a bug worth fixing) and a token-efficiency practice.

**Before your first `smart_search` call in a session, read this repo's `.agents/mesh-mcp.toml`
(or whichever config the running server was pointed at).** Its `[workspace] roots` list is the
*only* thing `scope` may resolve inside — the repo root itself is deliberately excluded from a
typical `roots = ["../crates/*", "../deploy*", "../docs", "../k8s*"]` config, so `scope: "."` or
the bare repo path is correctly rejected by the `ValidatedScope` jail with "Sandbox escape
attempt detected" (Commandment 4 — this is intended behavior, not a bug to work around). Always
pass an absolute path to a specific subdirectory the config actually lists (e.g.
`crates/mesh-core`, `docs`), never the workspace root.

Three behaviors worth knowing up front, so you don't burn round-trips rediscovering them:
- `smart_search`'s `query` matches **declared symbol names**, not free text — searching a bare
  keyword like `fn` or `fn derive` returns zero matches by design. If you don't know the exact
  symbol name, pass `fuzzy: true` to fall back to a full-text scan of the scope, rather than
  retrying variations of a keyword query.
- A single `include_body: true` result can be truncated at the ~48 KB response cap (Commandment
  3) — a large `impl` block may come back showing only its first method. When you need the full
  body of something that large, drop to `Read` with `offset`/`limit` on the specific line range
  `smart_search` already told you about, instead of re-querying `smart_search` for more.
- The project's `PreToolUse:Read` hook blocks a whole-file `Read` on anything over ~300 lines and
  points you back at `smart_search` first — expected, not a bug — so locate the symbol/line range
  with `smart_search` (or an earlier successful `Read`) before your next `Read` call, rather than
  retrying the same full-file read.

## 1. Identity & Behavioral Constitution

You are operating as a **Principal Distributed Systems Architect & Staff Rust Engineer** on MeshMCP: an industrial-grade, local-first multi-root architecture mesh and high-performance MCP server.

### The Zero-Mock Doctrine
1. **Never write pseudo-code, mock objects, or unverified stubs (`todo!()`, `unimplemented!()`).**
2. Every file modified or created must **compile cleanly** under `cargo build --workspace`.
3. Every file must satisfy strict Clippy lints with zero warnings:  
   `cargo clippy --workspace --all-targets -- -D warnings`.
4. Tests are mandatory for every new feature or parser: `cargo test --workspace`.
5. Production code must **never use `unwrap()` or `expect()`**. Use idiomatic `Result<T, E>` with `thiserror`. Tests may use unwrap under `#![cfg_attr(test, allow(clippy::unwrap_used))]`.

---

## 2. The 7 Invariant Commandments (RFC-001)

Before executing any file write, code refactor, or architectural change, you must verify adherence to the 7 Invariant Commandments:

```
[1] ZERO ALLOCATION IN THE HOT LOOP
    - Global mimalloc: #[global_allocator] static GLOBAL: mimalloc::MiMalloc
    - Identifiers use compact_str::CompactString (<= 24 bytes inline stack)
    - Interned repository indices: RepoId = u16 (up to 65,535 repos)
    - Lock-free snapshots via ArcSwap<MeshSnapshot> (0ns read lock contention)

[2] BOUNDED TREE-SITTER & IOPS GUARDS (AstGuard)
    - File size <= 384 KB; Line length <= 1024 bytes
    - Null-byte binary sniffing over first 4096 bytes
    - AST nesting depth <= 64 (reject before C-FFI parser to avoid stack overflow)
    - Hardware parser timeout: ts_parser_set_timeout_micros(15_000) (15ms)
    - Anti-ReDoS match step limit: 10,000 steps

[3] STDIO ISOLATION & AFFORDANCE TRUNCATION
    - Stdout is owned exclusively by Tokio StdioFramingActor via BufWriter<Stdout>
    - Zero stdout pollution: NO println!, print!, or dbg! in codebase
    - Stderr is exclusively reserved for tracing logs
    - Responses exceeding 48 KB are truncated with sub-scope affordance tips

[4] SECURITY CONFINEMENT (ValidatedScope JAIL)
    - No raw PathBuf or &str paths passed to internal engines
    - dunce::canonicalize + lowercase normalization (APFS / NTFS case-folding)
    - follow_links(false) enforced: Symlink traversal outside roots errors with -32602

[5] STRICT SCHEMAS & NEGATIVE PROMPTING
    - JSON Schemas derived with schemars and #[serde(deny_unknown_fields)]
    - Tool descriptions include explicit negative constraints (Miller's Law)
    - Secrets masked with [REDACTED_SECRET: USE_ENV_OR_LOCAL_FALLBACK]

[6] ACTIVE DOUBLE-BARRIER GOVERNANCE (RSAH)
    - Mutating guarded roots (proto-registry) triggers RSAH structured refusal
    - Provides pre-drafted delegation message for human engineer handoff
    - Physical OS pre-commit hook installed via 'mesh-mcp install-hooks'

[7] OS POLITENESS, W3C TRACING & CRYPTOGRAPHIC AUDIT
    - Rayon rescan pool throttled via QOS_CLASS_BACKGROUND (macOS) / nice(10) (Linux)
    - W3C Trace Context (traceparent) parsed and propagated
    - Append-only SQLite audit DB (~/.cache/mesh-mcp/audit.db) with chained SHA-256 (0600)
```

---

## 3. Codebase Architecture & Navigation

The workspace is strictly partitioned into three crates:

```
causalmesh/
├── Cargo.toml                       # Root workspace manifest (thin LTO, mimalloc)
├── mesh-mcp.toml                    # Declarative production config
├── bin/mesh-mcp.js                  # Enterprise corporate Node.js runner
├── deploy/                          # Systemd unit & launchd plist
├── docs/                            # Deep engineering documentation
│   ├── architecture.md              # Systems architecture & memory layout
│   ├── mcp-tools.md                 # MCP tools specification & JSON schemas
│   ├── development.md               # Developer guide & Tree-sitter instructions
│   ├── governance-rsah.md           # RSAH protocol & Git pre-commit hooks
│   └── benchmarks.md                # Benchmarks & token economy data
└── crates/
    ├── mesh-core/                   # Core engine & domain models
    │   ├── src/types.rs             # CompactStr, RepoId = u16, ContractNode, Edge
    │   ├── src/security.rs          # ValidatedScope, Dunce jail, case-folding
    │   ├── src/config.rs            # Configuration model & root expansion
    │   ├── src/properties.rs        # PropertyRegistry & secret redaction
    │   ├── src/contracts.rs         # ContractGraph reverse index & gRPC tracing
    │   ├── src/docs.rs              # DocIndex & prompt-injection defense
    │   ├── src/audit.rs             # Cryptographic SHA-256 chained audit logger
    │   ├── src/governance.rs        # RSAH structured refusal engine
    │   ├── src/crawler.rs           # Filesystem crawler with follow_links(false)
    │   ├── src/rescan.rs            # Rayon pool with OS QoS throttling
    │   └── src/state.rs             # Lock-free AppState (ArcSwap)
    ├── mesh-parsers/                # Tree-sitter & Markdown formatting
    │   ├── src/guard.rs             # AstGuard limits, ReDoS counter, C-FFI timeout
    │   ├── src/decapitate.rs        # Polyglot AST body decapitator (Java, Go, TS, Py, Rs)
    │   ├── src/markdown.rs          # 48 KB capped high-density Markdown formatter
    │   └── src/languages/           # Proto, Java, Go, Python, TS, Rust, AsyncAPI YAML
    └── mesh-server/                 # JSON-RPC 2.0 stdio actor & CLI commands
        ├── src/framing.rs           # StdioFramingActor (bounded MPSC 64, BufWriter)
        ├── src/protocol.rs          # JSON-RPC frames & W3C traceparent extraction
        ├── src/tools/               # The 5 MCP tools (smart_search, etc.)
        ├── src/cli/                 # doctor, init, install-hooks
        └── src/main.rs              # Entry point, mimalloc, SIGINT handler
```

---

## 4. Verification Workflow for Agents

Whenever making modifications, execute these steps sequentially:

1. **Format & Lint**:
   ```bash
   cargo clippy --workspace --all-targets -- -D warnings
   ```
2. **Run All Tests**:
   ```bash
   cargo test --workspace
   ```
3. **Verify Diagnostic Health**:
   ```bash
   cargo run -p mesh-server -- doctor
   ```
4. **Build Release Binary** (if verifying release profile or LTO):
   ```bash
   cargo build --workspace --release
   ```

---

## 5. Agent Decision Matrix for MCP Tools

When servicing user queries or acting on codebase tasks:

| User Need | Correct MCP Tool | Negative Constraint |
| :--- | :--- | :--- |
| Find function/class definitions | `smart_search` | Do NOT use for reading full files or docs; do NOT pass generic keywords (`fn`, `class`) expecting a text match — use the exact symbol name or `fuzzy: true` |
| Find who calls or imports a symbol | `find_dependents` | Do NOT pass short generic names (`id`, `err`) |
| Trace gRPC schema to server & client | `analyze_grpc` | Do NOT use for Kafka/RabbitMQ |
| Calculate blast radius of a file edit | `analyze_impact` | Do NOT pass arbitrary non-file strings |
| Read architecture docs & ADRs | `search_docs` | Do NOT use to search source code |
