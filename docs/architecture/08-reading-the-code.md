# 8. Reading the code

A suggested order for opening files, with what to look for in each, then exercises. Open the files
beside the chapters. Use MeshMCP on itself where you can (`smart_search` to find a symbol, then
read only that range).

## A first pass (about an hour)

| Order | File | Look for |
| :-: | :--- | :--- |
| 1 | `Cargo.toml` | the four crates, the release profile, workspace lints that forbid `unwrap` |
| 2 | `crates/mesh-core/src/types.rs` | `NodeKind`, `EdgeKind`, `EdgeConfidence`, `ContractNode`: the vocabulary of chapter 3 |
| 3 | `crates/mesh-parsers/src/guard.rs` | the guard constants and `parse_with`: chapter 2, step 2 |
| 4 | `crates/mesh-parsers/src/languages/mod.rs` | `extract_with_config`: the match on language that calls each extractor, and `FileIndex` |
| 5 | `crates/mesh-parsers/src/languages/swift.rs` | one of the smallest extractors (declarations plus one route idiom); shows the shape of "walk a tree, emit nodes" |
| 6 | `crates/mesh-core/src/contracts.rs` | `ContractGraph`, `add_node`, `reconcile_edges`: chapter 3 |
| 7 | `crates/mesh-core/src/state.rs` | `AppState`, `MeshSnapshot`, `install_snapshot`: chapter 5 |
| 8 | `crates/mesh-server/src/tools/mod.rs` | the `McpTool` trait and `ToolRegistry::invoke`: chapter 4 |
| 9 | `crates/mesh-server/src/tools/find_dependents.rs` | one small tool end to end: arguments, query, formatting |
| 10 | `crates/mesh-server/src/lib.rs` | the JSON-RPC loop and the `initialize` instructions |

## A second pass: the moving parts

| File | Chapter |
| :--- | :--- |
| `crates/mesh-server/src/indexer.rs` | 2, 5: `WorkspaceIndexer`, boot scan, `reload`, `reload_paths` |
| `crates/mesh-core/src/index_cache.rs` | 2: the parse cache and its key |
| `crates/mesh-core/src/watcher.rs`, `watcher/git.rs` | 5: debounce, Git gate |
| `crates/mesh-daemon/src/host.rs` | 5: `WorkspaceHost`, following the config |
| `crates/mesh-core/src/reload_kind.rs` | 5: the classifier |
| `crates/mesh-core/src/security.rs` | 6: `ValidatedScope` |
| `crates/mesh-core/src/audit.rs` | 4, 6: the hash chain |
| `crates/mesh-daemon/src/sandbox.rs` | 6: the Linux filter |
| `crates/mesh-parsers/src/languages/typescript.rs`, `js_frameworks/` | the deepest extractor, [parser-support](../parser-support.md) |

## Exercises

Each exercise has an observable result. Use a scratch checkout.

1. **See the facts.** Run `cargo run -p mesh-server -- graph --config examples/polyglot-shop/mesh-mcp.toml --format mermaid`.
   Find the node for the `ProcessPayment` RPC. Which file does it come from, and which extractor
   produced it?
2. **See a guard.** Create a file with a 2,000-character line inside an indexed root, then run
   `cargo run -p mesh-server -- doctor`. What reason does it print, and with what measurement?
3. **See confidence.** Run `analyze_impact` on `ProcessPayment` through the example workspace
   (the sample output is in [mcp-tools](../mcp-tools.md)). Why are the client rows `heuristic`?
4. **Follow a topic.** In `examples/unresolved-flow`, find the stream whose producer publishes to a
   name read from a config object. What does `analyze_impact` say, and why is that not "nobody
   produces it"?
5. **Break determinism on purpose** (read only, do not commit): in `reconcile_edges`, iterate a
   `HashMap` instead of a sorted collection, then run `scripts/determinism.sh`. What fails?
6. **Add a rule.** Add a single row to the broker rule table in `js_frameworks/messaging.rs` for a
   library you use, with a unit test. The table-driven design means this should not need new control
   flow.

## When something is surprising

- An answer is missing a file: `doctor` lists rejected files and their reason; check `.gitignore`.
- An answer is stale: check the snapshot `generation` note and whether a Git operation is in progress.
- A number in the docs: it should come from `docs/quality.md` with a date, platform and command, or it
  is not written (`AGENT.md`, rule 6).
- A behaviour with a version number in the docs (7.1.8, 7.1.9, ...): read that version's entry in
  `CHANGELOG.md` for the reason it exists.

Back to the [index](README.md).
