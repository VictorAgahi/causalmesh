# MeshMCP

**An MCP server that gives your AI coding agent a map of your polyglot codebase.**

[![Rust](https://img.shields.io/badge/rust-1.80%2B-blue.svg)](https://www.rust-lang.org)
[![License: MIT/Apache-2.0](https://img.shields.io/badge/license-MIT%2FApache--2.0-green.svg)](LICENSE)

Your agent reads code the way a newcomer does: one file at a time, guessing what calls what.
On a monorepo with a proto registry, a TypeScript gateway, three Go workers and a Java saga,
that means burnt context, missed callers, and confident answers that are wrong.

MeshMCP indexes your workspace once, keeps it in memory, and answers four questions your agent
can't answer alone:

- **Who breaks if I change this?** — reverse dependency graph.
- **Where does this RPC actually get implemented?** — `.proto` → generated stubs → handlers.
- **Who produces and consumes this event?** — Kafka/stream topics across services.
- **What do our own docs say about this?** — Markdown/ADR/RFC search.

It's a single Rust binary, runs locally, reads only the directories you list, and never talks
to the network.

**Languages indexed**: Java · Go · Python · TypeScript/JavaScript · Rust · C++ · Protobuf · OpenAPI/AsyncAPI YAML · Markdown

---

## Quick start

### 1. Install

```bash
curl -fsSL https://raw.githubusercontent.com/VictorAgahi/causalmesh/main/install.sh | bash
```

Installs a prebuilt binary to `~/.local/bin/`. No Rust toolchain needed.

<details>
<summary>Or build from source (needs Rust 1.80+)</summary>

```bash
git clone https://github.com/VictorAgahi/causalmesh.git
cd causalmesh
cargo build --workspace --release
cp target/release/mesh-mcp target/release/meshd ~/.local/bin/
```
</details>

### 2. Point it at your repo

```bash
cd /path/to/your/monorepo
mesh-mcp init --auto      # writes .agents/mesh-mcp.toml by detecting your layout
mesh-mcp doctor           # checks the config resolves and the parsers load
```

`init --auto` is a starting point, not a final answer — it guesses your roots from common
directory names. Read [Configuration](#configuration) next; ten minutes there is what makes
the difference between "it works" and "it understands our architecture".

### 3. Connect your agent

**Claude Code**
```bash
claude mcp add mesh-mcp -- mesh-mcp run
```

**Cursor / VS Code / Windsurf** — `.cursor/mcp.json` or `.vscode/mcp.json`:
```json
{ "mcpServers": { "mesh-mcp": { "command": "mesh-mcp", "args": ["run"] } } }
```

(`mesh-mcp init --auto --write-ide-config` writes these two files for you.)

### 4. Ask it something

> "Which services break if I change the `CreateOrder` RPC?"

Your agent now calls `analyze_grpc` and `find_dependents` instead of grepping.

---

## Try it in 30 seconds

A small polyglot monorepo ships with the repo — one proto file and five services in
TypeScript, Go, Rust, Python and Java, wired together by gRPC and Kafka topics:

```bash
mesh-mcp graph --config examples/polyglot-shop/mesh-mcp.toml --open
```

That opens an interactive topology in your browser. `--format mermaid` prints a diagram you
can paste into a Markdown file instead.

---

## What your agent gets

Six tools. Each description tells the model when *not* to use it, which is most of what keeps
an agent from flailing.

| Tool | Answers | Notes |
| :--- | :--- | :--- |
| `smart_search` | "Where is `UserAuthRequest` declared?" | Returns signatures with bodies stripped. Searches the in-memory symbol index; pass `fuzzy: true` for a full-text scan of the scope. |
| `find_dependents` | "Who imports this contract?" | Reverse dependency lookup. Import edges are currently extracted from **TypeScript/JavaScript only**; for other languages use a custom pattern or `smart_search`. |
| `analyze_grpc` | "Where is this RPC implemented and called?" | Links `.proto` definitions to handlers and client stubs. |
| `analyze_impact` | "Who produces/consumes this event?" | Kafka topics, streams, queues, sagas, post-processors. Detected natively for Java (`@KafkaListener`) and AsyncAPI channels; for other languages declare a [custom pattern](#layer-5--custom-patterns-teach-it-your-conventions). |
| `search_docs` | "What did we decide about idempotency?" | Keyword search over your Markdown docs, with alias and stop-word support. |
| `visualize_mesh` | "Show me the topology." | Mermaid or standalone HTML. |

Two things every tool does:

- **Strips function bodies.** You get `fn charge(order: &Order) -> Result<Receipt> { /* stripped */ }`,
  not 80 lines of retry logic. Measured on this repo's benchmark corpus, that removes 55–69% of
  the tokens depending on language (`cargo bench -p mesh-server` prints the number for yours).
- **Caps output at 48 KB**, with a note telling the agent how to narrow the query instead of
  silently truncating.

---

## Configuration

Everything lives in one TOML file: `.agents/mesh-mcp.toml` (or `mesh-mcp.toml` at the root).
Build it up in layers — each section below is independently useful.

### Layer 1 — Roots: what may be read at all

This is also the security boundary. Any path outside these roots is refused with a JSON-RPC
`-32602`, symlinks pointing outside are dropped, and `..` traversal is resolved before the check.

```toml
[workspace]
name = "my-mesh"
version = "1.0.0"

# Optional indirection so the same file works on every machine and in CI.
workspace_root = "${WORKSPACE_ROOT:-..}"

roots = [
  "${workspace_root}/proto-registry",
  "${workspace_root}/api-gateway",
  "${workspace_root}/services/*",      # globs expand to one root per match
  "${workspace_root}/docs",
]
```

> **Paths are relative to the config file, not to your shell.** A config in `.agents/` needs
> `..` to reach the repo root — that's why `init --auto` writes `${WORKSPACE_ROOT:-..}`.

Exclusions are gitignore-style globs. The defaults already cover secrets and build output; add
yours:

```toml
exclude_patterns = [
  "**/node_modules/**", "**/target/**", "**/.venv/**",
  "**/*.pem", "**/*.key", "**/.env*",
  "**/generated/**",
]
```

Excluded directories are pruned from the walk, so a large `node_modules/` costs nothing.

### Layer 2 — Docs: make `search_docs` speak your vocabulary

```toml
[engines.docs]
enabled = true
paths = ["${workspace_root}/docs", "${workspace_root}/architecture"]

# Your team's shorthand → the word actually written in the docs.
aliases = { "k8s" = "kubernetes", "dlq" = "dead-letter-queue", "ws" = "websocket" }

# Words that add noise to a query.
stop_words = ["the", "how", "what", "which"]

exact_phrase_boost = 60          # weight of a full-phrase hit in a section title
sanitize_prompt_injections = true # neutralise "ignore previous instructions" in indexed docs
```

> `paths` and `enabled` are accepted but not yet read: Markdown is indexed because it sits under
> `roots`. See the [config reference](SETUP.md#config-reference) for which keys are wired.

Aliases are the highest-leverage setting here: without them, an agent asking about "the DLQ"
finds nothing in a doc that only ever says "dead-letter-queue".

### Layer 3 — Skills: make the agent read your playbook first

This is how you get *your* process in front of the agent before it edits anything.

Write a Markdown file describing how work is done in some area of the codebase:

```markdown
---
name: proto-contract-evolution
description: How to evolve a proto contract without breaking consumers.
---

# Evolving a proto contract

1. Never renumber or reuse a field tag. Mark removed fields `reserved`.
2. Open the PR against `proto-registry` alone and wait for CI to publish stubs.
3. Only then bump the dependency in the consuming services.
```

Then map it to the area it covers:

```toml
[engines.policy.skills]
# Key = an MCP tool name, or any fragment of the scope/target being queried.
"proto-registry" = ".agents/skills/proto-contract-evolution.md"
"services/billing" = ".agents/skills/billing-invariants.md"
"smart_search" = ".agents/skills/how-we-search.md"
```

Now any tool call whose target or scope contains `proto-registry` comes back with a footer:

```
---
**Project skill for this area**: `.agents/skills/proto-contract-evolution.md` — How to evolve a proto contract without breaking consumers.
Read it before proposing changes here.
```

The agent reads the file and follows your rules instead of inventing its own. Matching is
case-insensitive; an exact tool-name key wins over a path match, and the longest matching key
wins among path matches. `mesh-mcp doctor` fails loudly if a configured skill file is missing —
a typo here would otherwise just silently never fire.

This repo's own skills live in [`.agents/skills/`](.agents/skills/) if you want examples.

### Layer 4 — Stop rules: hard boundaries

Where a skill is advice, a stop rule is a refusal. Used by the git pre-commit hook installed by
`mesh-mcp install-hooks`:

```toml
[engines.policy]
enabled = true
enforce_git_hooks = true
cryptographic_audit_trail = true

[engines.policy.stop_rules]
"proto-registry" = "STOP: proto-registry generates the TS/Go/Java stubs. Land the contract PR first."
"k8s-infrastructure" = "STOP: manifest changes require DevOps review."
```

A commit touching a guarded path is rejected with a structured explanation of what to do
instead. See [docs/governance-rsah.md](docs/governance-rsah.md).

### Layer 5 — Custom patterns: teach it your conventions

MeshMCP recognises gRPC, Spring, OpenAPI and AsyncAPI shapes by file extension and content —
the `[engines.contracts.grpc]` / `.spring` / `.openapi` / `.asyncapi` sections are accepted but
not yet read, so there is nothing to configure there today. Your in-house event bus, outbox
table or job queue it cannot guess — describe it with a regex:

```toml
[[engines.contracts.patterns]]
name = "transactional-outbox"
kind = "topic_producer"        # topic_producer | topic_consumer | saga | rpc
file_pattern = "*.ts"
regex = 'createEvent<([^>]+)>'
target_group = 1               # capture group holding the topic/event name

[[engines.contracts.patterns]]
name = "event-post-processor"
kind = "topic_consumer"
file_pattern = "*.ts"
regex = 'class\s+(\w+)\s+extends\s+\w*PostProcessor<([^>]+)>'
target_group = 2               # the event
consumer_group = 1             # the class consuming it
```

Producers and consumers of the same name are then linked automatically, and `analyze_impact`
can trace an event end to end. [`examples/polyglot-shop/mesh-mcp.toml`](examples/polyglot-shop/mesh-mcp.toml)
has working patterns for five languages.

After any config change:

```bash
mesh-mcp doctor                                    # config resolves, skills exist, parsers load
mesh-mcp graph --format mermaid | head -40         # does the topology look like your architecture?
```

---

## How it works

```
crawl roots ──▶ size/binary guard ──▶ tree-sitter parse ──▶ per-file extract
                                                                   │
        agent query ◀── one atomic snapshot ◀── reconcile edges ◀── merge
                                     ▲
                          file watcher ──▶ differential rescan (changed files only)
```

- Parsing runs in parallel; the resulting graph is published as a single immutable snapshot, so
  a query never sees a half-updated index.
- A file watcher re-indexes only what changed, on background-priority threads, so it doesn't
  compete with your editor.
- Tree-sitter runs behind hard bounds: 15 ms parse timeout, 384 KB file budget (1.5 MB for
  schemas), 1 KB max line length, depth limit. A file that trips a bound is skipped, never
  dumped raw into your context.
- Secrets in indexed YAML/properties are masked before they can reach a prompt.

Details: [docs/architecture.md](docs/architecture.md).

### Two ways to run

**Daemon (default)** — `mesh-mcp run` is a thin proxy over a Unix socket to a shared `meshd`
process that holds the index. Five IDE windows share one index instead of building five.
`meshd` is auto-spawned and shuts down when idle.

**Standalone** — `mesh-mcp run --standalone` keeps everything in one process. Use it in
containers, in CI, or when a Unix socket isn't available.

---

## Performance

Measured on this machine (Apple Silicon, 8 cores, macOS) with `cargo bench -p mesh-server`.
Run it yourself — these are the numbers that come out, not a marketing claim:

| Operation | Average | Notes |
| :--- | ---: | :--- |
| `find_dependents` on a 1,000-node graph | 3.95 µs | in-memory index lookup |
| `analyze_grpc` pipeline trace | 9.12 µs | |
| Audit log write (SQLite WAL + SHA-256 chain) | 12.22 µs | |
| Markdown formatting with 48 KB bound | 34.40 µs | |
| Lexical nesting guard | 1.20 µs | ~989 MB/s |
| AST decapitation, TypeScript | 150 µs | −54.5% tokens |
| AST decapitation, Rust | 107 µs | −57.4% tokens |
| AST decapitation, Go | 110 µs | −68.9% tokens |

Release binary: 12.3 MB (`mesh-mcp`) / 12.1 MB (`meshd`). Peak RSS indexing this repo (60
files): 20.5 MiB. Index size scales with your workspace — measure on yours.

`cargo test --workspace` includes a regression test asserting the benchmark budgets still hold.

---

## CLI

| Command | What it does |
| :--- | :--- |
| `mesh-mcp run` | Start the MCP server on stdio (daemon-backed). |
| `mesh-mcp run --standalone` | Same, single process, no daemon. |
| `mesh-mcp init --auto` | Detect the layout and write `.agents/mesh-mcp.toml`. |
| `mesh-mcp init --auto --write-ide-config` | Also write `.cursor/mcp.json` and `.vscode/mcp.json`. |
| `mesh-mcp doctor` | Validate config, roots, skill files, parsers, secret masking. |
| `mesh-mcp graph [--format html\|mermaid\|json] [--open]` | Render the topology. |
| `mesh-mcp install-hooks` | Install the git pre-commit hook enforcing stop rules. |

All commands accept `--config <path>`. Logs go to stderr; stdout carries JSON-RPC only.

---

## Documentation

| Document | Contents |
| :--- | :--- |
| [SETUP.md](SETUP.md) | Full installation and configuration walkthrough. |
| [docs/mcp-tools.md](docs/mcp-tools.md) | Tool schemas, arguments, output formats. |
| [docs/architecture.md](docs/architecture.md) | Internals: snapshots, indexing, security jail. |
| [docs/development.md](docs/development.md) | Building, testing, adding a language. |
| [docs/governance-rsah.md](docs/governance-rsah.md) | Stop rules, skills, pre-commit enforcement. |
| [docs/benchmarks.md](docs/benchmarks.md) | How to measure, and what the numbers mean. |
| [RFC-001-CAUSAL-MCP.md](RFC-001-CAUSAL-MCP.md) | The specification this implements. |

---

## Development

```bash
cargo build --workspace                                  # debug build
cargo test --workspace                                   # unit + integration tests
cargo clippy --workspace --all-targets -- -D warnings    # must be clean
cargo fmt --all                                          # before committing
cargo bench -p mesh-server                               # benchmark suite
```

Workspace layout:

| Crate | Responsibility |
| :--- | :--- |
| `mesh-core` | Graph, snapshot state, config, security jail, audit, watcher. |
| `mesh-parsers` | Tree-sitter extractors, AST decapitation, Markdown output. |
| `mesh-server` | MCP server, tools, indexing pipeline, CLI. |
| `mesh-daemon` | Shared `meshd` daemon over a Unix socket. |

`unwrap()` and `panic!()` are denied outside tests. See [docs/development.md](docs/development.md)
and [CLAUDE.md](CLAUDE.md) for the architectural invariants a change must preserve.

---

## License

MIT OR Apache-2.0.
