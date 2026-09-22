---
name: mesh-performance-invariants
description: >-
  Use before writing or reviewing code on any MeshMCP hot path: allocation in loops,
  CompactStr and Arc<Path> interning, index lookups instead of linear scans, per-file
  regex or parser construction, blocking work on the tokio executor, and how to measure.
---

# MeshMCP Performance Invariants Skill

Commandment 1 ("zero dynamic allocation in the hot loop") is not a slogan here — the
refactor on this branch rewrote state, extraction and reconciliation around it. This
skill lists the invariants a change must not break and how to check.

---

## 1. Quick Navigation & Codebase References

- **Types and interning**: [`crates/mesh-core/src/types.rs`](../../../crates/mesh-core/src/types.rs)
  - `CompactStr = compact_str::CompactString`, `RepoId = u16`, `NodeId = u32`
  - `FilePath = Arc<Path>` — one heap buffer per file, shared by every node declared in it
- **Graph indices**: [`crates/mesh-core/src/contracts.rs`](../../../crates/mesh-core/src/contracts.rs)
  - `name_to_nodes`, `package_to_nodes`, `file_to_nodes`, `fqcn_to_node`, `reverse_deps`,
    `topic_producers`, `topic_consumers`
  - `contains_ignore_ascii_case()` — allocation-free replacement for `to_lowercase().contains()`
- **Snapshot**: [`crates/mesh-core/src/state.rs`](../../../crates/mesh-core/src/state.rs)
  (`ArcSwap<MeshSnapshot>`, `snapshot()`, `install_snapshot()`)
- **Parser cache**: [`crates/mesh-parsers/src/guard.rs`](../../../crates/mesh-parsers/src/guard.rs)
  (`AstGuard::with_parser`, thread-local `[Option<Parser>; LanguageKind::TREE_SITTER_COUNT]`)
- **Precompiled patterns**: [`crates/mesh-parsers/src/languages/mod.rs`](../../../crates/mesh-parsers/src/languages/mod.rs)
  (`CompiledPattern::compile_all`)
- **Compiled excludes**: [`crates/mesh-core/src/crawler.rs`](../../../crates/mesh-core/src/crawler.rs) (`ExcludeMatcher`)
- **Stat-before-read**: [`crates/mesh-parsers/src/guard.rs`](../../../crates/mesh-parsers/src/guard.rs)
  (`within_size_budget`) and [`crates/mesh-core/src/vfs.rs`](../../../crates/mesh-core/src/vfs.rs) (`is_unchanged_fast`)
- **Blocking boundary**: [`crates/mesh-server/src/tools/mod.rs`](../../../crates/mesh-server/src/tools/mod.rs)
  (`spawn_blocking` in `ToolRegistry::invoke`) and [`crates/mesh-core/src/rescan.rs`](../../../crates/mesh-core/src/rescan.rs)
- **Measurement**: [`crates/mesh-server/benches/real_benchmarks.rs`](../../../crates/mesh-server/benches/real_benchmarks.rs)
  and `test_real_benchmarks_regression_budgets` in
  [`crates/mesh-server/tests/integration_tests.rs`](../../../crates/mesh-server/tests/integration_tests.rs)

---

## 2. Invariants

### 2.1 Strings: `CompactStr`, not `String`

Symbols, packages, paths-as-keys and metadata are `CompactStr`
(`compact_str::CompactString`), which keeps short values inline instead of heap-allocating.
Every field of `ContractNode` except `file_path` and the line numbers is a `CompactStr`.
A new field on a hot struct is `CompactStr` unless you can say why not.

### 2.2 Paths: intern once as `Arc<Path>`

`ContractNode.file_path` is `FilePath = Arc<Path>`, not `PathBuf`. Extractors build it
once per file and clone the `Arc` per symbol:

```rust
let file_path: FilePath = Arc::from(file_path);
```

`ContractGraph` keys `file_to_nodes` by `FilePath`, so the same buffer backs the index.
Introducing a `PathBuf` per node undoes this; so does `node.file_path.to_path_buf()` in a
loop. Borrow with `&*node.file_path` (as `smart_search` does when collecting candidate
files) or clone the `Arc`.

### 2.3 Never linear-scan the graph

`ContractGraph` carries six secondary indices precisely so no query walks `nodes`.
`nodes.values().find(...)` in a query or in `reconcile_edges` is a bug, not a style issue:
`reconcile_edges` is documented as linear in nodes + edges and used to be O(E²).

Use the index that matches the key you have:

| You have | Use |
| --- | --- |
| exact symbol name | `name_to_nodes` (`search_symbols` fast path) |
| package / module id | `package_to_nodes` |
| a file | `get_nodes_for_file()` / `file_to_nodes` |
| `package/Name` for an RPC | `fqcn_to_node` |
| an import string | `resolve_import_target()` |
| a topic (lowercased) | `topic_producers` / `topic_consumers` |

Edge de-duplication goes through `HashSet<(NodeId, NodeId, EdgeKind)>`, seeded once:

```rust
let mut edge_set: HashSet<(NodeId, NodeId, EdgeKind)> =
    self.edges.iter().map(|e| (e.from, e.to, e.kind)).collect();
```

Never scan `self.edges` to check whether an edge already exists.

### 2.4 No `to_lowercase()` per query

Case-insensitive matching on a hot path uses the allocation-free helper:

```rust
fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() { return true; }
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    h.len() >= n.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}
```

Where lowercase *keys* are genuinely needed (topics), they are lowercased once at
indexing time in `add_node` / `add_producer` / `add_consumer`, and lookups pass an
already-lowercase key. `LanguageKind::as_str()` exists for the same reason: it replaced a
per-file `format!("{:?}").to_lowercase()`. Precompute at index time, not per request.

### 2.5 Build expensive objects once

- **Tree-sitter parsers**: `AstGuard::with_parser(lang_kind, |parser| ...)` keeps one
  parser per language per thread in a `thread_local!`; `Parser::new` + `set_language` is
  C-FFI allocation and grammar binding. `create_bounded_parser` is the constructor behind
  it and is the right call only in `verify_all_parsers`-style checks — never per file.
  `with_parser` also calls `parser.reset()` afterwards, because a timed-out parse leaves
  the parser mid-state.
- **Regexes**: `CompiledPattern::compile_all(&[CustomPatternConfig])` once per run, then
  `PolyglotIndexer::extract_custom_patterns`. `apply_custom_patterns` recompiles on every
  call — its own doc comment says to prefer the compiled path.
- **Glob sets**: `ExcludeMatcher::compile` once per crawl, and the set is handed to
  `WalkBuilder::filter_entry` so excluded directories are pruned rather than matched file
  by file.
- **Tool schemas**: `ToolRegistry::list_tools()` builds the `json!` array inside a
  `LazyLock` because `schema_for!` walks the whole type graph and every client calls
  `tools/list` on connect.

### 2.6 Stat before you read

Two independent stat-only gates exist; use them, do not read first and check after.

```rust
if !AstGuard::within_size_budget(path, &metadata) { return None; }   // Commandment 2
```

```rust
Ok(m) => !vfs.is_unchanged_fast(p, &m),   // reload candidate filter
```

`is_unchanged_fast` compares mtime and size only; the SHA-256 in `compute_signature` is
paid only for files that already look changed.

### 2.7 Borrow query results, do not clone nodes

`GrpcTrace<'g>` and `ImpactFlow<'g>` hold `Vec<&'g ContractNode>`, and `find_dependents`
and `search_symbols` return `Vec<&ContractNode>`. The snapshot guard the caller holds
keeps them alive. Cloning a `ContractNode` per result per request is exactly what the
formatter never needed. If a lifetime fights you, hold the guard longer — do not `clone()`.

### 2.8 Reads are lock-free; writes are single-writer

`AppState.snapshot` is an `ArcSwap<MeshSnapshot>`. Read with `state.snapshot()` (a guard),
publish with `state.install_snapshot(snap)`. Never wrap the graph in a `RwLock`, never
hold a snapshot guard across an `.await`, and never mutate a published snapshot — clone it
with `snapshot_clone()`, mutate, install.

The three indices live in one `MeshSnapshot` so a reader cannot observe a new
`contract_graph` beside a stale `doc_index`. Do not split them back into separate
`ArcSwap`s.

### 2.9 Nothing blocking on the tokio executor

Disk, tree-sitter and SQLite work runs either in `spawn_blocking` (`ToolRegistry::invoke`)
or on the Rayon pools. `McpTool::run` is synchronous for that reason. Background rescans
go through `BackgroundRescanEngine`, whose threads are `QOS_CLASS_BACKGROUND` on macOS and
`nice 10` on Linux (Commandment 7); use `install` when the closure itself contains a
`par_iter`, `spawn` when it does not.

### 2.10 Parallel extraction, sequential fold

`PolyglotIndexer::extract` returns a `FileIndex` with relations in local indices so it can
run on the pool; `FileIndex::apply` inserts into the graph sequentially. Any new extractor
must be pure and `Send`, and must not take `&mut ContractGraph`. See
[`mesh-indexing-pipeline`](../mesh-indexing-pipeline/SKILL.md).

### 2.11 Bound every output

`MAX_OUTPUT_BYTES = 48 * 1024` is checked before appending each entry in
`MarkdownFormatter::format_search_results`, and snippets are additionally capped per line.
An unbounded `format!` into a response body is a context-window regression even when it is
fast.

---

## 3. Measuring

```bash
cargo bench -p mesh-server
```

`crates/mesh-server/benches/real_benchmarks.rs` is a `harness = false` binary with its own
`main`. It measures with `measure_latencies` (`WARMUP_ITERATIONS` warmup passes, then
`BENCH_ITERATIONS` timed passes, reporting avg / p95 / min / max in microseconds and
ops/sec) across AST decapitation, the contract graph, the audit logger and Markdown
formatting. It asserts two invariants rather than absolute speeds: the audit chain must
verify (`AuditLogger::verify_log_file`), and `format_search_results` output must be
`<= 48 * 1024` bytes.

Numbers are machine-specific: compare a before/after on the same machine in the same
state, and never copy a figure from a bench run into documentation as if it were a
property of the system.

The non-regression budgets that run in CI are in
`test_real_benchmarks_regression_budgets` (`crates/mesh-server/tests/integration_tests.rs`):

```rust
let budget_ms = if cfg!(debug_assertions) { 150 } else { 15 };
```

decapitating one TypeScript controller, tied to the 15 ms C-FFI timeout budget
(`AstGuard::PARSER_TIMEOUT_MICROS = 15_000`), plus an assertion that decapitation saves
more than 50% of the raw character count (`len() / 4` as a token proxy). Tighten those
budgets only with a measurement; loosening one is an admission of a regression and needs
saying out loud in the PR.

Always finish with:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```
