---
name: mesh-tool-authoring
description: >-
  Use when adding, renaming or changing an MCP tool in MeshMCP: the McpTool trait,
  schemars argument schemas, negative prompting in DESCRIPTION, the 48 KB output budget,
  audit, skill hints and integration tests.
---

# MeshMCP Tool Authoring Skill

Six tools are exposed over MCP: `smart_search`, `find_dependents`, `analyze_grpc`,
`analyze_impact`, `search_docs`, `visualize_mesh`. They all implement one trait in
[`crates/mesh-server/src/tools/mod.rs`](../../../crates/mesh-server/src/tools/mod.rs).

---

## 1. Quick Navigation & Codebase References

- **Trait and registry**: [`crates/mesh-server/src/tools/mod.rs`](../../../crates/mesh-server/src/tools/mod.rs)
  - `McpTool { NAME, DESCRIPTION, Args, meta(), subject(), run() }`
  - `ToolOutput { text, files_accessed, secrets_redacted }`, `ToolOutput::text()`
  - `ToolError = (i32, String)`
  - `ToolRegistry::list_tools()` (built once in a `LazyLock`), `::describe::<T>()`
  - `ToolRegistry::call_tool()` (dispatch by name), `::invoke::<T>()` (parse, `spawn_blocking`, audit, skill hint)
- **Reference implementations**: [`smart_search.rs`](../../../crates/mesh-server/src/tools/smart_search.rs)
  (scope validation, index-first lookup, snippet budget), [`find_dependents.rs`](../../../crates/mesh-server/src/tools/find_dependents.rs)
  (the minimal shape), [`search_docs.rs`](../../../crates/mesh-server/src/tools/search_docs.rs),
  [`analyze_grpc.rs`](../../../crates/mesh-server/src/tools/analyze_grpc.rs),
  [`analyze_impact.rs`](../../../crates/mesh-server/src/tools/analyze_impact.rs),
  [`visualize_mesh.rs`](../../../crates/mesh-server/src/tools/visualize_mesh.rs)
- **Output rendering**: [`crates/mesh-parsers/src/markdown.rs`](../../../crates/mesh-parsers/src/markdown.rs)
  - `MAX_OUTPUT_BYTES = 48 * 1024`, `MarkdownFormatter::format_*`, `SearchResult`
- **JSON-RPC surface**: [`crates/mesh-server/src/lib.rs`](../../../crates/mesh-server/src/lib.rs)
  (`tools/list`, `tools/call`) and [`crates/mesh-server/src/protocol.rs`](../../../crates/mesh-server/src/protocol.rs) (`RequestMeta`)
- **Audit**: [`crates/mesh-core/src/audit.rs`](../../../crates/mesh-core/src/audit.rs) (`AuditLogger::record_entry`)
- **Skill hints**: [`crates/mesh-core/src/governance.rs`](../../../crates/mesh-core/src/governance.rs) (`GovernanceEngine::recommend_skill`)
- **Tests**: [`crates/mesh-server/tests/integration_tests.rs`](../../../crates/mesh-server/tests/integration_tests.rs)

---

## 2. The trait

```rust
pub trait McpTool {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    type Args: DeserializeOwned + Serialize + JsonSchema + Send + 'static;

    fn meta(args: &Self::Args) -> Option<&RequestMeta>;
    fn run(args: &Self::Args, state: &AppState) -> Result<ToolOutput, ToolError>;
}
```

`run` is **synchronous on purpose**: every tool touches the disk, tree-sitter or SQLite.
`ToolRegistry::invoke` runs it inside `tokio::task::spawn_blocking`, so the JSON-RPC event
loop (and, in daemon mode, the other clients) never stalls. Do not make `run` `async`, and
do not call `block_on` inside it.

`subject` has a default returning `None` and reports what the call is about, for skill
matching:

```rust
fn subject(args: &Self::Args) -> Option<&str> {
    Some(args.scope.as_str())   // smart_search
}
```

---

## 3. Adding a tool, end to end

### Step 1 — new file `crates/mesh-server/src/tools/<name>.rs`

Copy the shape of `find_dependents.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindDependentsArgs {
    #[schemars(
        with = "String",
        description = "Target contract name (ex: 'UserAuthRequest') or package identifier (ex: '@volontariapp/domain-user') to trace reverse dependencies for."
    )]
    pub target: CompactStr,

    #[serde(default)]
    pub _meta: Option<RequestMeta>,
}
```

Rules for the args type, all load-bearing:

- `#[serde(deny_unknown_fields)]` on every args struct (Commandment 5). A typo'd argument
  must be an error, not a silently ignored field.
- `CompactStr` fields need `#[schemars(with = "String")]`, otherwise the generated schema
  describes `compact_str`'s internals.
- Optional arguments get `#[serde(default)]`.
- Always carry `pub _meta: Option<RequestMeta>` and return it from `meta()`, or W3C
  `traceparent` propagation into the audit row breaks for your tool.
- Put a concrete example inside every `description` — these strings are the agent's only
  documentation.

### Step 2 — implement the trait

```rust
impl McpTool for FindDependentsTool {
    const NAME: &'static str = "find_dependents";
    const DESCRIPTION: &'static str = "Resolves in-memory O(1) reverse dependency graph across packages and shared modules. DO NOT USE to search freeform text or method signatures (use smart_search).";
    type Args = FindDependentsArgs;

    fn meta(args: &Self::Args) -> Option<&RequestMeta> { args._meta.as_ref() }

    fn run(args: &Self::Args, state: &AppState) -> Result<ToolOutput, ToolError> {
        let snapshot = state.snapshot();
        let dependents = snapshot.contract_graph.find_dependents(args.target.as_str());
        Ok(ToolOutput::text(MarkdownFormatter::format_dependents(
            args.target.as_str(),
            &dependents,
        )))
    }
}
```

**Negative prompting (Commandment 5).** Every `DESCRIPTION` states what the tool does and
then names the tool to use instead. All six shipped descriptions follow the pattern
`… DO NOT USE <wrong situation> (use <other_tool>).` — an agent that cannot tell two
tools apart will call both. Keep it to one sentence plus one `DO NOT USE` clause.

### Step 3 — register it

Three edits in `tools/mod.rs`, all required:

1. `pub mod <name>;` and the `use` for the tool type.
2. A `ToolRegistry::describe::<YourTool>()` line in the `list_tools()` `json!` array.
3. A `YourTool::NAME => Self::invoke::<YourTool>(arguments, state).await?` arm in
   `call_tool`. The fallthrough is `unknown => return Err((-32601, …))`.

Forgetting (2) makes the tool invisible to clients; forgetting (3) makes it advertised
and un-callable. `test_list_tools_contains_all_tools` asserts the array length, so it
will catch a missing entry in (2) — bump the expected count there.

### Step 4 — errors

`ToolError` is `(i32, String)` and the code goes on the wire. Use the JSON-RPC codes the
rest of the server uses: `-32602` for invalid params, which is what
`SecurityError::jsonrpc_code()` returns for every sandbox failure (Commandment 4), and
`-32601` for unknown methods. Propagate scope failures verbatim:

```rust
let validated_scope = ValidatedScope::resolve(args.scope.as_str(), &state.allowed_roots)
    .map_err(|e| (e.jsonrpc_code(), e.to_string()))?;
```

Any tool taking a path-like argument must resolve it through `ValidatedScope` before
touching the filesystem. Never pass a raw `&str` or `PathBuf` to a reader or crawler.

---

## 4. The 48 KB output budget

`MAX_OUTPUT_BYTES = 48 * 1024` in `crates/mesh-parsers/src/markdown.rs`. Render through a
`MarkdownFormatter::format_*` function rather than building Markdown in the tool: the
formatter is where the budget and the truncation affordance live.
`format_search_results` checks before appending each entry:

```rust
if header.len() + body.len() + entry_str.len() > MAX_OUTPUT_BYTES - 1024 {
    return Self::build_truncated_search_output(
        &header, &body, idx, results.len(), query, &scope_counts,
    );
}
```

The 1 KB headroom is the truncation footer itself, which reports how many matches were
displayed of how many total and lists the top sub-scopes with a concrete follow-up call.
Truncating without that guidance leaves the agent blind to the remainder — if you add a
new formatter, add the same affordance.

Also bound the per-item cost inside the tool. `smart_search` caps a snippet at
`SNIPPET_LINES` lines with `SNIPPET_LEAD` lines of lead-in, and each line at
`MAX_SNIPPET_LINE_BYTES`, backing off to a char boundary via `floor_char_boundary` —
slicing a multi-byte line at a fixed byte index panics.

---

## 5. What the registry does around `run`

You get these for free; do not reimplement them in the tool:

- **Argument parsing** into `T::Args`, with `-32602` and the tool name on failure.
- **`spawn_blocking`**, so `run` may block.
- **Skill hint**: on `Ok`, `state.governance.recommend_skill(T::NAME, T::subject(&args))`
  is consulted and, when it matches, a `Project skill for this area` footer naming the
  configured file is appended to `out.text`. Implement `subject()` so your tool
  participates; leave it defaulted if the tool has no meaningful subject.
- **Audit**: `state.audit.record_entry(...)` is called on the same blocking thread with
  the tool name, the serialized args, `SUCCESS`/`ERROR`, `files_accessed` and
  `secrets_redacted`. It is best-effort — a failed write is logged to `mesh::audit`, never
  returned. Populate `files_accessed` and `secrets_redacted` on `ToolOutput` yourself;
  the registry only forwards them. `ToolOutput::text()` leaves both empty.

Because the whole args struct is serialized into the audit row, never put a secret or a
file body in an argument.

---

## 6. Tests

Integration tests live in `crates/mesh-server/tests/integration_tests.rs` and drive the
real dispatcher:

```rust
let res = ToolRegistry::call_tool("smart_search", args, state).await;
```

`setup_test_environment()` builds a temp workspace (a Java service, a `proto-registry`)
and returns an `Arc<AppState>`. For a new tool add at least:

- a success case asserting on `val["content"][0]["text"]`;
- a sandbox-escape case, if the tool takes a path — see
  `test_smart_search_sandbox_escape_rejected`;
- a case for whatever the `DO NOT USE` clause warns against, so the boundary between your
  tool and its neighbour is pinned down.

Then run the mandatory gates:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

`unwrap()` is forbidden outside `#[cfg(test)]`; the test crates carry
`#![allow(clippy::unwrap_used, clippy::expect_used)]` at the top, production code does not.
