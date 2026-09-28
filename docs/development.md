# MeshMCP Developer Guide

Building, testing and extending MeshMCP. The invariants every change must preserve are in
[`CLAUDE.md`](../CLAUDE.md); task-specific guides for coding agents are in
[`.agents/skills/`](../.agents/skills/README.md).

---

## 1. Prerequisites

- **Rust**: a current stable toolchain. CI builds with the latest stable release
  (`dtolnay/rust-toolchain@stable`); no minimum supported version is pinned or tested.
- **C compiler** (`clang` or `gcc`, MSVC on Windows): the tree-sitter grammars are C code
  compiled by their build scripts.
- **Platforms exercised by CI**: `ubuntu-latest`, `macos-latest`, `windows-latest` (test suite);
  `ubuntu-latest` and `macos-latest` (determinism gate, release smoke test, pilot installer test).

---

## 2. Building

```bash
cargo build --workspace              # debug
cargo build --workspace --release    # release: thin LTO, codegen-units = 1, panic = abort, stripped
```

The release profile is in the root [`Cargo.toml`](../Cargo.toml). The build produces two
binaries in `target/release/`: `mesh-mcp` (MCP server, CLI, daemon proxy) and `meshd` (the
per-workspace daemon). Both are needed for the default daemon mode. Binary size depends on the
target and toolchain and is not tracked.

Installation for users and pilots is covered in [`SETUP.md`](../SETUP.md) and
`scripts/install_pilot.sh`.

---

## 3. Tests and quality gates

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

All three run in CI (`.github/workflows/ci.yml`) on every pull request. The workspace lints deny
`unwrap_used` and `panic` outside tests. Once per clone, enable the repository's pre-commit hook
so these run before each commit:

```bash
git config core.hooksPath .githooks
```

Other gates in CI:

- `scripts/determinism.sh`: one index fingerprint across sequential and concurrent runs of each
  example workspace (`ubuntu-latest`, `macos-latest`).
- `.github/workflows/golden.yml`: gRPC edge precision/recall on the pinned golden corpus, must
  stay at 1.0/1.0.
- `scripts/test_install_pilot.sh`: the pilot installer run twice under an isolated `HOME` must
  leave the same state.
- `crates/mesh-server/src/tools/mod.rs` `test_mcp_tools_md_schema_drift_check`: the JSON schemas
  in [`mcp-tools.md`](mcp-tools.md) must list exactly the properties each tool advertises. Edit
  that file whenever a tool argument changes.

Measurement harnesses (scale, memory, cache, payload) are described in
[`benchmarks.md`](benchmarks.md); their results go in [`quality.md`](quality.md).

---

## 4. Adding a tree-sitter language

Supported today: Java, Go, Python, TypeScript (`.ts`, `.mts`, `.cts`; `.tsx`, `.js`, `.jsx`,
`.mjs`, `.cjs` through the TSX grammar), Rust, C++, Kotlin, C#,
Ruby, PHP, Swift, Scala and Protobuf, all through tree-sitter; YAML (OpenAPI, AsyncAPI, Spring
properties), `.properties` and Markdown are parsed without it. The full walkthrough is
[`.agents/skills/mesh-parser-engineering/SKILL.md`](../.agents/skills/mesh-parser-engineering/SKILL.md).
In short:

### Step 1: grammar crate
Add it to the root `Cargo.toml` (`[workspace.dependencies]`) and to
`crates/mesh-parsers/Cargo.toml`.

**Grammar ABI.** The workspace pins `tree-sitter = "0.24"`. The newest release of a grammar crate
may target a newer ABI and fail to compile against it with a `Language` type mismatch; check the
grammar's `LANGUAGE_VERSION` in its generated `parser.c`, or try the version in a scratch build
first.

### Step 2: `LanguageKind` (`crates/mesh-parsers/src/decapitate.rs`)
Add the variant, then update together: `as_str()`, `language()`, `tree_sitter_slot()`,
`from_path()` (extensions) and `TREE_SITTER_COUNT`. The two thread-local parser arrays in
`guard.rs` (`with_parser`'s `PARSERS` and `parse_with`'s `INDEX_PARSERS`) are sized by
`TREE_SITTER_COUNT` and initialised with one `None` per slot; extend both initialisers.

### Step 3: extractor
Create `crates/mesh-parsers/src/languages/<lang>.rs`. An extractor receives an already-parsed
tree and returns nodes (and, for richer languages, relations), without touching the graph:

```rust
pub fn extract(file_path: &Path, content: &str, repo_id: RepoId, tree: &Tree) -> Vec<ContractNode>
```

Register `pub mod <lang>;` and add a match arm in `PolyglotIndexer::extract_with_config`
(`languages/mod.rs`) that calls it through the `parsed!` macro (which uses
`AstGuard::parse_with` with the indexing timeout and records parse failures). If the extractor
returns relations, copy them into `out.dependencies`, `out.producers`, `out.consumers` and
`out.rpc_calls`; a relation computed by the extractor but not copied there never reaches the graph.

### Step 4: decapitation rule
In `AstDecapitator::collect_body_replacements` (`decapitate.rs`), replace the body node of the
language's functions and methods, keeping signatures, annotations and docstrings.

### Step 5: watcher and doctor
Add the extension to `FileWatcherService::is_relevant_path` (`crates/mesh-core/src/watcher.rs`),
or edits to such files will never trigger a reload, and add the grammar to
`AstGuard::verify_all_parsers`, which backs the parser line of `mesh-mcp doctor`.

### Step 6: tests
A unit test in the extractor module asserting the expected `NodeKind`s; a decapitation test in
`decapitate.rs` (see `test_kotlin_decapitation` and `test_csharp_decapitation`); and, if the
language produces relations, a test that goes through `PolyglotIndexer::index_file` rather than
the extractor directly, so the dispatch wiring of step 3 is covered
(`test_polyglot_indexer_wires_java_import_dependency` is the model).

---

## 5. Talking to the server by hand

The server speaks newline-delimited JSON-RPC on stdio:

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  | ./target/release/mesh-mcp run --standalone
```

The `initialize` result has the shape:

```json
{"protocolVersion":"2024-11-05","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"mesh-mcp","version":"<crate version>"}}
```

Logs are written to stderr (`RUST_LOG=debug` for more detail). Nothing but JSON-RPC frames is
written to stdout.

---

## 6. `mesh-mcp doctor`

`crates/mesh-server/src/cli/doctor.rs`. Output goes to stderr, except `--json`, which prints every
check below as one JSON array on stdout (`name`, `status`, `message`, and `fixed` after `--fix`).
The JSON is identical between two runs on the same machine: volatile figures (the measured RSS)
appear in the text report only.

What it actually checks:

- **Configuration**: the config file parses, roots resolve, configured skill files exist,
  `[engines.docs] paths`, `proto_dirs` and OpenAPI `spec_files` patterns match at least one file,
  roots do not overlap.
- **Secret masking**: two sample keys are inserted into a `PropertyRegistry` and must come back
  masked.
- **Linux**: the `fs.inotify.max_user_watches` limit (warns below 524,288).
- **Parsers**: every tree-sitter grammar initialises (`AstGuard::verify_all_parsers`).
- **Tools on `PATH`**: `git`, and whether `ripgrep` is present.
- **Symlink invariants**: a symlink is created in a canonicalized temp directory and the crawler
  must not follow it (the probe reports `info` where symlinks cannot be created).
- **Host event subsystem**: an FSEvents / ReadDirectoryChangesW watcher can be created.
- **Index health**: a full scan of the workspace, then the count of files rejected per reason and
  the first 10 paths. Non-source files (images, fonts, archives, lockfiles, minified bundles:
  `mesh_core::health::is_non_source`) are only counted as skipped, never listed.
- **Memory**: peak RSS before the index-health scan, `warn` above 50 MiB.
- **Repairable checks** (also in `--json`, acted on by `--fix`): socket and socket-directory
  permissions, orphaned socket, daemon version drift, daemon seccomp confinement (Linux),
  legacy machine-wide cache, corrupt workspace cache, orphaned per-version cache directories, and
  audit-chain verification (reported, never repaired).

The last line counts the `error` and `warn` checks above it (a check repaired by `--fix` is not
counted).
