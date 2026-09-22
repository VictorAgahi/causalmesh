# CLAUDE.md — Claude Code Developer Directives for MeshMCP

> **Project**: MeshMCP (RFC-001 Rev. 2.9.0)  
> **Repository**: `causalmesh/mesh-mcp`  
> **Environment**: Rust 1.80+ (macOS, Linux, WSL2)

---

## 1. Quick Command Reference

All CLI commands are optimized through the **RTK (Rust Token Killer)** proxy hook for token savings.

### Essential Build & Test Commands
```bash
# Build workspace in debug mode
cargo build --workspace

# Build optimized release binary with Thin LTO and mimalloc (6.8 MB)
cargo build --workspace --release

# Run all unit and integration tests across workspace
cargo test --workspace

# Strict Clippy validation (MUST have 0 warnings and 0 errors)
cargo clippy --workspace --all-targets -- -D warnings

# Run MeshMCP diagnostic healthcheck
cargo run -p mesh-server -- doctor

# Run MCP server locally over stdio
cargo run -p mesh-server -- run
```

### RTK Meta Commands
```bash
rtk gain              # Display token savings analytics
rtk gain --history    # Show command usage history with savings
rtk discover          # Analyze command history for optimization opportunities
rtk proxy <cmd>       # Execute raw command without filtering (debugging only)
```

---

## 2. Core Architectural Invariants (The 7 Commandments)

Claude Code must strictly enforce these invariants on every edit:

1. **Zero Dynamic Allocation in Hot Loop**:
   - `mimalloc` is the global allocator.
   - Use `compact_str::CompactString` for symbols, paths, and IDs (<= 24 bytes inline stack).
   - Use `RepoId = u16` for repository indexing (up to 65,535 repos).
   - Use `ArcSwap<MeshSnapshot>` for lock-free state reads (0ns contention).
2. **Bounded Tree-Sitter & IOPS (`AstGuard`)**:
   - Files > 384 KB or lines > 1,024 bytes must be rejected.
   - Null-byte sniffing over first 4,096 bytes.
   - AST nesting depth <= 64 (pre-check before C-FFI parser).
   - C-FFI timeout: `ts_parser_set_timeout_micros(15_000)` (15ms).
   - Query cursor match limit: 10,000 steps.
3. **Stdio Isolation & Affordance Truncation**:
   - `stdout` is reserved exclusively for the `StdioFramingActor` via `BufWriter<Stdout>`.
   - **Never** write to `stdout` via `println!`, `print!`, or `dbg!`.
   - All logging must go to `stderr` via `tracing::info!`, `tracing::warn!`, etc.
   - Output cap: 48 KB with structured sub-scope truncation guidance.
4. **Security Jail (`ValidatedScope`)**:
   - Never accept raw `PathBuf` or string paths in query engines.
   - Use `ValidatedScope::resolve()` which applies `path_clean::clean()`, `dunce::canonicalize()`, and case-folding.
   - Enforce `follow_links(false)` on all filesystem walks.
   - Path traversal or symlink escape must return JSON-RPC error code `-32602`.
5. **Strict Schemas & Negative Prompting**:
   - Derive schemas using `schemars::JsonSchema` with `#[serde(deny_unknown_fields)]`.
   - Include negative constraints in tool descriptions.
   - Mask secrets with `[REDACTED_SECRET: USE_ENV_OR_LOCAL_FALLBACK]`.
6. **Active Double-Barrier Governance (RSAH)**:
   - Modifications targeting `proto-registry` or guarded repos return an RSAH structured refusal.
   - Install OS pre-commit hook via `mesh-mcp install-hooks`.
7. **OS Politeness & Cryptographic Audit**:
   - Rayon rescan thread pool runs with `QOS_CLASS_BACKGROUND` (macOS) / `nice(10)` (Linux).
   - Propagate W3C `traceparent` headers.
   - Append to `audit.log` (mode `0600`) with chained SHA-256 signatures.

---

## 3. Code Style & Rules

- **Pure Rust**: No mock code, no stubs (`todo!()`, `unimplemented!()`).
- **Zero Unwrap in Production**: Use `?` operator and `thiserror` for error management. `unwrap()` is strictly forbidden outside `#[cfg(test)]`.
- **AST Decapitation**: Strip function bodies into `{ /* stripped */ }` or `...` to minimize token consumption (measured 54-69% fewer tokens depending on language; run `cargo bench -p mesh-server`).
- **Markdown Payloads**: Format tool outputs in dense GitHub Flavored Markdown rather than raw JSON (avoids escaping overhead).

---

## 4. Key References & Documentation
- High-level architecture: [`docs/architecture.md`](docs/architecture.md)
- 6 MCP Tools & Schemas: [`docs/mcp-tools.md`](docs/mcp-tools.md)
- Developer & Tree-sitter guide: [`docs/development.md`](docs/development.md)
- Governance & RSAH: [`docs/governance-rsah.md`](docs/governance-rsah.md)
- Performance & Token benchmarks: [`docs/benchmarks.md`](docs/benchmarks.md)
- Authoritative Specification: [`RFC-001-CAUSAL-MCP.md`](RFC-001-CAUSAL-MCP.md)
