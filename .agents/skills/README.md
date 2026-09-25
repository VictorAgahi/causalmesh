# MeshMCP Agent Skills

Skills for a coding agent working **on the MeshMCP codebase itself** (`crates/mesh-core`,
`crates/mesh-parsers`, `crates/mesh-server`, `crates/mesh-daemon`).

Each skill is `<name>/SKILL.md` with YAML frontmatter (`name`, `description`) and a
`Quick Navigation & Codebase References` section linking the exact files and symbols it
describes. Some carry a `DEEPENING.md` for the mechanics you only need when debugging.

Authoritative law lives elsewhere and is not restated here: the 7 Commandments in
[`CLAUDE.md`](../../CLAUDE.md).

## Index

| Skill | Scope | Load it when |
| --- | --- | --- |
| [`mesh-indexing-pipeline`](mesh-indexing-pipeline/SKILL.md) | crawl → guard → extract → `FileIndex` → fold → `reconcile_edges` → `install_snapshot`, in `crates/mesh-server/src/indexer.rs` | You touch indexing, reload, the VFS, the watcher, or add a file type / language to the scan |
| [`mesh-tool-authoring`](mesh-tool-authoring/SKILL.md) | The `McpTool` trait, `ToolRegistry`, schemas, output budget, audit, skill hints | You add, rename or change an MCP tool or its arguments |
| [`mesh-performance-invariants`](mesh-performance-invariants/SKILL.md) | Allocation, interning, index usage, blocking work, benchmarking | You write code on any hot path, or a review flags a regression |
| [`mesh-graph-reconciliation`](mesh-graph-reconciliation/SKILL.md) | `ContractGraph`, node/edge indices, `reconcile_edges`, query methods, `MeshSnapshot` | You change graph structure, edge semantics, or a graph query |
| [`mesh-parser-engineering`](mesh-parser-engineering/SKILL.md) | `AstGuard`, `AstDecapitator`, `LanguageKind`, language extractors | You add a language, change a grammar query, or change decapitation |
| [`mesh-security-governance`](mesh-security-governance/SKILL.md) | `ValidatedScope`, secret redaction, `GovernanceEngine` / RSAH, SQLite audit chain, git hook | You touch path handling, governance, redaction or audit |
| [`mesh-stdio-protocol`](mesh-stdio-protocol/SKILL.md) | `StdioFramingActor`, JSON-RPC shapes, `MarkdownFormatter`, 48 KB cap | You touch the transport, the JSON-RPC loop, or output formatting |

Adding an MCP tool touches at least three of these: `mesh-tool-authoring` for the trait
and registry, `mesh-graph-reconciliation` for the query it runs, `mesh-stdio-protocol`
for the rendered output.

## Referencing a skill from `mesh-mcp.toml`

`[engines.policy.skills]` maps a key to a skill file path. `ToolRegistry::invoke`
(in [`crates/mesh-server/src/tools/mod.rs`](../../crates/mesh-server/src/tools/mod.rs))
appends a `Project skill for this area` footer to the tool's output when
`GovernanceEngine::recommend_skill(tool, subject)` returns a path, so the agent calling
the tool is pointed at the playbook before it acts.

```toml
[engines.policy.skills]
# Key = exact MCP tool name -> fires on every call to that tool.
"smart_search"    = ".agents/skills/mesh-performance-invariants/SKILL.md"
# Key = path fragment -> matched case-insensitively against the call's subject
# (smart_search: `scope`; find_dependents / analyze_grpc / analyze_impact: `target`;
#  search_docs: `query`). Longest matching key wins.
"proto-registry"  = ".agents/skills/mesh-graph-reconciliation/SKILL.md"
```

Matching order, from `GovernanceEngine::recommend_skill` in
[`crates/mesh-core/src/governance.rs`](../../crates/mesh-core/src/governance.rs):

1. Exact match on the MCP tool name (`McpTool::NAME`) wins outright.
2. Otherwise the lowercased subject (`McpTool::subject(args)`) is searched for each key
   as a substring, and the longest matching key wins — the same shape as
   `[engines.policy.stop_rules]`.
3. No key matches, or `subject()` returns `None`: no footer.

The footer prints the path plus, when the file's frontmatter has one, its `description`
line (falling back to the first `# ` heading). Write descriptions that read as a
one-line trigger, because that string is what the calling agent sees first.

Paths may be relative: `WorkspaceIndexer::discover_config` calls
`Config::resolve_skill_paths(base_dir)`, which rewrites a relative path that exists under
the config file's own directory into one that resolves from any working directory — the
server is spawned by an IDE with an arbitrary cwd. A relative path that resolves nowhere is
left verbatim so `doctor` can report exactly what was configured.

A configured path that does not exist silently never fires. `mesh-mcp doctor` validates
every `[engines.policy.skills]` entry against disk and lists the missing ones — run it
after editing the table.

## House rules for editing these skills

- Every technical claim must be checkable in the code as it stands. No remembered APIs.
- Code blocks are copied from the source, not reconstructed.
- No invented numbers. Only constants that exist (`MAX_FILE_SIZE_BYTES`,
  `PARSER_TIMEOUT_MICROS`, `MAX_OUTPUT_BYTES`, `DEBOUNCE_INTERVAL`, …) or a measurement
  you took yourself and labelled as such.
- Every relative link must resolve, and every symbol named must still exist.
