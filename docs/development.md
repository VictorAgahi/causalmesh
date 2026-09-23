# MeshMCP Developer & Contributor Guide

This guide covers building, testing, extending, and maintaining MeshMCP (RFC-001 Rev. 2.9.0).

---

## 1. Prerequisites & Toolchain

MeshMCP requires a modern Rust toolchain:
- **Rust**: 1.80.0 or later (stable)
- **Cargo**: Included with Rust
- **Operating Systems**: macOS (Apple Silicon / Intel), Linux (x86_64, aarch64), Windows (WSL2)
- **C Compiler**: `clang` or `gcc` (required to build Tree-sitter C grammar runtimes)

---

## 2. Building the Project

### Development Build (Fast Compilation)
```bash
cargo build --workspace
```

### Production Release Build (Thin LTO & mimalloc)
```bash
cargo build --workspace --release
```
The workspace root [`Cargo.toml`](../Cargo.toml) is pre-configured with:
```toml
[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 1
panic = "abort"
strip = true
```
This produces self-contained binaries in `target/release/`:
- `mesh-mcp`: Stdio MCP server and transparent UDS client proxy (~6.8 MB)
- `meshd`: Background architecture daemon with UDS multiplexing (~6.5 MB)

For comprehensive build and configuration walkthroughs, see [SETUP.md](../SETUP.md).

---

## 3. Testing & Code Quality Invariants

MeshMCP adheres to a zero-warning, zero-compromise quality standard.

### Run All Workspace Tests
```bash
cargo test --workspace
```
Expected output:
```
test result: ok. 22 passed (mesh-core)
test result: ok. 8 passed (mesh-daemon)
test result: ok. 24 passed (mesh-parsers)
test result: ok. 4 passed (mesh-server unit)
test result: ok. 9 passed (mesh-server integration)
Total: 67 passed; 0 failed
```

### Run Strict Clippy
Every pull request must pass Clippy with `-D warnings`:
```bash
cargo clippy --workspace --all-targets -- -D warnings
```

### Check Code Formatting
```bash
cargo fmt --all -- --check
```

---

## 4. How to Add a New Tree-Sitter Language Parser

MeshMCP's [`crates/mesh-parsers`](../crates/mesh-parsers) crate uses Tree-sitter grammars to
decapitate function bodies and extract signatures. Supported languages today: **Java, Go,
Python, TypeScript (also `.tsx`, `.js`), Rust, C++, Kotlin, C#**, plus Protobuf and YAML handled
without tree-sitter. The full walkthrough — with `cpp.rs` as the worked example — lives in
[`.agents/skills/mesh-parser-engineering/SKILL.md`](../.agents/skills/mesh-parser-engineering/SKILL.md).
In short, adding a language (e.g. Ruby or Swift) touches these places:

### Step 1: Add the grammar crate
Root `Cargo.toml` (`[workspace.dependencies]`) and `crates/mesh-parsers/Cargo.toml`:
```toml
tree-sitter-ruby = "0.23"
```

### Step 2: Extend `LanguageKind` in `crates/mesh-parsers/src/decapitate.rs`
```rust
pub enum LanguageKind {
    Java, Go, Python, TypeScript, Rust, Cpp, Kotlin, CSharp,
    Ruby, // <-- new variant
    Protobuf, Yaml, Unknown,
}
```
Bump `TREE_SITTER_COUNT` and add the new variant's slot to `tree_sitter_slot()`, its grammar to
`language()`, and its extensions to `from_path()` — all four must move together, and the
thread-local parser array in `guard.rs::with_parser` must be resized to match `TREE_SITTER_COUNT`.

### Step 3: Write the extractor
`crates/mesh-parsers/src/languages/ruby.rs`, following the contract in the skill doc (`extract`
returning `Vec<ContractNode>`), and register `pub mod ruby;` plus the `PolyglotIndexer::extract`
dispatch arm in `languages/mod.rs`.

### Step 4: Add Decapitation Grammar Rules
In `crates/mesh-parsers/src/decapitate.rs`, update `AstDecapitator::collect_body_replacements`:
```rust
match lang_kind {
    // ... existing match arms ...
    LanguageKind::Ruby if kind == "method" => {
        if let Some(body) = node.child_by_field_name("body") {
            replacements.push((body.start_byte(), body.end_byte(), Cow::Borrowed("# stripped")));
            return;
        }
    }
    _ => {}
}
```

### Step 5: Wire the file watcher and `doctor`
Add the extension(s) to `FileWatcherService::is_relevant_path`
(`crates/mesh-core/src/watcher.rs`) — a language missing from this list parses correctly at boot
but never hot-reloads on edit — and to `AstGuard::verify_all_parsers`, which backs the
`mesh-mcp doctor` parser-initialization line.

### Step 6: Add tests
A unit test in the new extractor module asserting the expected `NodeKind`s, and a decapitation
test in `crates/mesh-parsers/src/decapitate.rs` verifying that method bodies are stripped while
the signature, annotations/attributes and docstrings survive. See `test_kotlin_decapitation` and
`test_csharp_decapitation` for worked examples of both a fields-less grammar (Kotlin) and a
grammar with named fields (C#).

---

## 5. Debugging JSON-RPC Over Stdio

Because MeshMCP communicates over `stdin`/`stdout`, you cannot test it using standard interactive terminal typing without proper JSON-RPC envelopes.

### Testing Stdio Interaction via Python or Shell Script

You can send a formatted JSON-RPC payload directly into the binary:

```bash
cat << 'EOF' | ./target/release/mesh-mcp run
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test-client","version":"1.0.0"}}}
{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
EOF
```

Expected output on `stdout`:
```json
{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"mesh-mcp","version":"2.9.0"}}}
{"jsonrpc":"2.0","id":2,"result":{"tools":[...]}}
```

All diagnostic logs appear on `stderr`:
```
2026-09-20T20:22:45.123Z INFO mesh::framing: Initializing StdioFramingActor (MPSC bounded capacity=64)
2026-09-20T20:22:45.124Z INFO mesh::server: Handling initialize handshake
```

---

## 6. Diagnostic Doctor Architecture

MeshMCP provides an integrated diagnostic tool (`mesh-mcp doctor`) located in `crates/mesh-server/src/cli/doctor.rs`.

It inspects:
1. **Configuration Syntax**: Validates `mesh-mcp.toml` parsing and checks for missing sections.
2. **Jailed Roots**: Resolves environment variables and confirms that target directories exist on disk.
3. **Symlink Boundary Checks**: Tests whether symlink protection is active.
4. **Secret Redaction Filters**: Verifies that regex patterns mask test keys (`AKIA...`, `sk-ant-...`).
5. **OS Kernel Event Subsystems**: Checks for APFS FSEvents/kqueue (macOS) or `inotify` watch limits (Linux).
6. **Stdio Loopback Latency**: Measures the internal round-trip time between bounded MPSC queues.
7. **Tree-Sitter Grammars**: Validates that all C-FFI grammar symbol tables initialize without faults.
8. **Memory RSS Baseline**: Asserts that resident memory remains below the 20 MiB ceiling.
