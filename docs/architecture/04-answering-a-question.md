# 4. Answering a question

This is the query path: from a JSON line on standard input to a Markdown answer. Code:
`crates/mesh-server/src/lib.rs` (the loop), `framing.rs`, `protocol.rs`, `tools/` (the tools),
`crates/mesh-parsers/src/markdown.rs` (formatting).

## The protocol

MCP over standard input/output is **JSON-RPC 2.0**, one JSON object per line. MeshMCP answers four
methods: `initialize`, `ping`, `tools/list`, `tools/call`. Any other method is error `-32601`. A
message without an `id` is a notification and never gets a reply.

Two rules about the streams:

- **Standard output belongs to one writer** (`StdioFramingActor`). Nothing else prints to it, because
  a stray line would corrupt the protocol. Logs go to standard error.
- The channels between reader, loop and writer are bounded (64 frames), so a client that floods
  requests gets backpressure instead of unbounded memory use.

`initialize` returns, besides the version, a short `instructions` text: which tool answers which
question, written as a starting point to verify rather than an authority.

## A failure is a result, not a crash

If a tool fails (bad arguments, a path outside the roots, a governance refusal, "still indexing"),
the reply is a normal success response whose result has `isError: true` and the message as text. The
agent sees the message and can correct its next call. Only protocol faults (unparseable JSON, unknown
method) are JSON-RPC errors.

## The six tools

| Tool | Reads | Answers |
| :--- | :--- | :--- |
| `smart_search` | symbol index, HTTP route nodes | where is this declared (or this route handled); signatures with bodies stripped |
| `find_dependents` | import edges | who imports or uses this package, type or contract |
| `analyze_grpc` | proto, handler and client nodes | server handlers and clients of an RPC; wire-format check of the `.proto` against a Git base |
| `analyze_impact` | produce/consume/RPC/route edges | what is affected by changing this RPC, topic, event or route handler |
| `search_docs` | the Markdown section index | what the team's docs say about a term |
| `visualize_mesh` | the whole graph, aggregated per service | a bounded topology map as Mermaid, JSON or HTML |

Each tool is a Rust type that implements the `McpTool` trait: a name, a description, an argument
type, and a `run` function that takes the arguments and the shared state and returns text.
Arguments are checked against a strict schema; an unknown or misspelled field is an error that names
the field.

## What happens on `tools/call`

`ToolRegistry::invoke` (`tools/mod.rs`) does the same sequence for every tool:

1. **Parse the arguments** into the tool's argument type (strict, unknown fields rejected).
2. **Governance**: if the tool would mutate something, or `read_governance_mode` is
   `enforce_refusal`, check the call's subject (a scope or a target) against the stop rules. A match
   returns a structured refusal telling the agent to stop and report to a human. With the default
   `allow_all`, no read is ever refused.
3. **Run the tool on a blocking thread.** Tools read disk and SQLite and run tree-sitter, which would
   stall the message loop if run inline.
4. **Skill hint**: if a project skill is configured for this tool or subject, a note pointing at it
   is placed at the top of the answer.
5. **Cap the output** at 48 KB.
6. **Audit**: append a record of the call, success or not.

## Reading from one snapshot

At the start of `run`, a tool takes the current snapshot (an atomic pointer read, no lock) and uses
that single snapshot for the whole call. A reload that finishes meanwhile does not change what this
call sees. Chapter 5 explains how the snapshot is produced and swapped.

## Output rules

- **Markdown, not JSON**, so there is no escaping overhead inside the text.
- **Hard cap of 48 KB.** Paginated tools stop a page before the cap and print the exact `offset` of
  the next page. If something would still exceed the cap, it is cut on a line boundary, any open code
  fence is closed, and a note says how to narrow the query.
- `smart_search` pages also stop at about 8 KB by default, and each result's size is measured before
  it is accepted.
- **Bodies are stripped** (decapitation): function bodies become `{ /* stripped */ }` or `...`; line
  numbers still refer to the original file. `include_body: true` returns one full implementation.
- **Test files are left out** of dependent, route and impact answers and counted, unless
  `include_tests: true`. "Test file" is judged on the path relative to the workspace root, so a
  checkout living under a directory called `test` does not hide everything.
- **Zeros are worded carefully.** A zero says "no literal match in the indexed roots", and notes about
  names that may be resolved at runtime or live in unindexed roots accompany it. A claim of absence
  needs a query across all roots with the tool that matches the question.

## Interpreting rows: INTERNAL and EXTERNAL

The graph has no "service" concept, so a service is a root. For `analyze_impact`, a row is `INTERNAL`
when its root is one of the owner roots of the contract (the roots of the handlers that implement an
RPC, or of the producers of an event) and `EXTERNAL` otherwise. Rows are ordered `EXTERNAL` first,
then by role, path and line, and are stable between runs.

## Search caching

`smart_search` pages are cached per query and scope, dropped whenever a new snapshot is installed,
and re-validated against the size and modification time of every file they read, so an on-disk edit
the index has not yet seen is never served from the cache.

## The audit log

Every call, including refused and invalid ones, is appended to `~/.cache/mesh-mcp/audit.db`
(SQLite, mode `0600`). Each row's hash includes the previous row's hash, so editing or inserting a
row in the middle breaks every later link; `mesh-mcp doctor` verifies the chain. Raw arguments are
never stored, only their digest; an error row also keeps a short, sanitized message. It is
tamper-evident, not tamper-proof: a local user can delete the file, and removing the most recent rows
is not detectable from the file alone. `mesh-mcp stats` summarizes it locally.

Next: [5. Processes and time](05-processes-and-time.md).
