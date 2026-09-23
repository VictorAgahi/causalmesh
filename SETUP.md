# Setting up MeshMCP

This guide takes you from nothing to an AI agent that understands your architecture. Budget
about 20 minutes: five to install, fifteen to write a config that actually reflects your repo.

If you only want to see it work, jump to [Step 1](#step-1-install) then
[the demo](#try-it-on-the-bundled-demo).

**Contents**

1. [Install](#step-1-install)
2. [Generate a starting config](#step-2-generate-a-starting-config)
3. [Tell it where your code is](#step-3-tell-it-where-your-code-is)
4. [Check it actually indexed something](#step-4-check-it-actually-indexed-something)
5. [Connect your agent](#step-5-connect-your-agent)
6. [Teach it your conventions](#step-6-teach-it-your-conventions)
7. [Put your playbooks in front of the agent](#step-7-put-your-playbooks-in-front-of-the-agent)
8. [Guard critical paths](#step-8-guard-critical-paths-optional)
9. [Config reference](#config-reference)
10. [Troubleshooting](#troubleshooting)

---

## Step 1 — Install

### Prebuilt binary (no Rust needed)

```bash
curl -fsSL https://raw.githubusercontent.com/VictorAgahi/causalmesh/main/install.sh | bash
```

Installs `mesh-mcp` and `meshd` into `~/.local/bin/`. Make sure that's on your `PATH`:

```bash
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc   # or ~/.bashrc
exec $SHELL
mesh-mcp --version
```

Prebuilt targets: macOS (Apple Silicon and Intel), Linux x86-64 (gnu and musl), Windows x86-64.

### From source

Needs Rust 1.80+ (`rustup toolchain install stable`).

```bash
git clone https://github.com/VictorAgahi/causalmesh.git
cd causalmesh
cargo build --workspace --release
cp target/release/mesh-mcp target/release/meshd ~/.local/bin/
```

Both binaries are needed: `mesh-mcp` is the MCP server your agent talks to, `meshd` is the
shared background index it connects to.

### Try it on the bundled demo

Before touching your own repo, confirm the install works on the sample monorepo:

```bash
cd causalmesh   # only if you cloned the source
mesh-mcp graph --config examples/polyglot-shop/mesh-mcp.toml --format mermaid | head -30
```

You should see a Mermaid graph listing services and topics. `--open` renders it in a browser
instead.

---

## Step 2 — Generate a starting config

```bash
cd /path/to/your/monorepo
mesh-mcp init --auto
```

This writes `.agents/mesh-mcp.toml`, guessing your roots from directory names it recognises
(`services/`, `packages/`, `crates/`, `proto*/`, `docs/`, `k8s*/`, …).

**Treat the result as a draft.** It cannot know that your contracts live in `schemas/v2`, or
that `legacy/` should be ignored. Step 3 is where the real work happens.

MeshMCP looks for its config in this order:

1. the path given to `--config`
2. `.agents/mesh-mcp.toml`
3. `mesh-mcp.toml` in the current directory

---

## Step 3 — Tell it where your code is

Open `.agents/mesh-mcp.toml`. The `roots` list is the single most important setting: it defines
both **what gets indexed** and **what the agent is allowed to read at all**. Anything outside is
refused.

```toml
[workspace]
name = "my-mesh"
version = "1.0.0"

# Every root below is resolved against this. Overridable per-machine via $WORKSPACE_ROOT.
workspace_root = "${WORKSPACE_ROOT:-..}"

roots = [
  "${workspace_root}/proto-registry",
  "${workspace_root}/api-gateway",
  "${workspace_root}/services/*",      # a glob becomes one root per match
  "${workspace_root}/docs",
]
```

### The mistake everyone makes

**Paths resolve relative to the config file, not to where you run the command.**

`init --auto` writes the config into `.agents/`, one level below your repo root. So a root
written `./services` would resolve to `.agents/services` — which doesn't exist, and you'd get an
empty index with no error. That's why the generated file starts from `..`.

| Config file location | To reach `<repo>/services` |
| :--- | :--- |
| `<repo>/.agents/mesh-mcp.toml` | `../services`, or `${workspace_root}/services` with `workspace_root = "${WORKSPACE_ROOT:-..}"` |
| `<repo>/mesh-mcp.toml` | `./services`, or `${workspace_root}/services` with `workspace_root = "${WORKSPACE_ROOT:-.}"` |

### Exclusions

Gitignore-style globs. Excluded directories are pruned from the walk, so listing
`node_modules` costs nothing rather than scanning and discarding it.

```toml
exclude_patterns = [
  "**/node_modules/**", "**/target/**", "**/dist/**", "**/.venv/**",
  "**/*.pem", "**/*.key", "**/*.p12", "**/.env*", "**/secrets/**",
  "**/generated/**",   # add yours
]
```

A pattern without a slash matches a path *component*, so `build` excludes any `build/`
directory but leaves `build.rs` and `build_tools/` alone.

> **The crawler also always respects `.gitignore`**, independently of `exclude_patterns`. If a
> file is gitignored — a generated OpenAPI spec emitted by a build step and never committed, for
> example — it is never scanned, so `[engines.contracts.grpc].proto_dirs`,
> `[engines.contracts.openapi].spec_files`, and `[engines.docs].paths` can never match it either,
> no matter how the pattern is written. `mesh-mcp doctor`'s "matched 0 files" warning for one of
> these fields is often this, not a syntax mistake: check whether the file you expect to be
> indexed is gitignored before re-writing the pattern. There is currently no config flag to
> disable this behavior.

### Running in containers

> **Not yet wired.** `[workspace.mount_aliases]` parses and the translation logic exists
> (`ValidatedScope::resolve_with_aliases`), but nothing passes the table to it yet, so container
> paths are not translated today. Until then, run the server with the same paths the agent uses,
> or bind-mount at an identical path on both sides.

---

## Step 4 — Check it actually indexed something

Two commands. Do not skip these — a misconfigured root fails quietly, by indexing nothing.

```bash
mesh-mcp doctor
```

```
🔍 Running MeshMCP Diagnostic Healthcheck (v2.9.1)...

✔ Config syntax: Valid (.agents/mesh-mcp.toml)
✔ Jailed roots verified (6/6 allowed roots, 0 escapes detected)
ℹ Project skills: none configured ([engines.policy.skills])
✔ Symlink invariants: follow_links=false verified across all engines
✔ Secret redaction engine: ACTIVE (Dev secrets masked with fallback hints)
✔ Host OS event subsystem: Native (APFS FSEvents/kqueue active)
✔ Stdio loopback latency: 0.43ms
✔ Tree-sitter parsers initialized (Java, Go, Python, TypeScript, Rust, C++)
✔ Memory baseline: < 20 MiB RSS (mimalloc + compact_str)
✔ Toolchain utilities: git & ripgrep detected

✔ All systems operational. Ready for AI agents.
```

`doctor` validates syntax and roots — it does not tell you whether the *content* was understood.
For that:

```bash
mesh-mcp graph --format mermaid | head -40
```

Read the output against your mental model:

| What you see | What it means |
| :--- | :--- |
| `Scanned 0 files` | Your roots point nowhere. Re-read Step 3. |
| Files scanned, few nodes | Indexed, but your conventions aren't recognised yet → Step 6. |
| Services and topics you recognise | Working. Go to Step 5. |

---

## Step 5 — Connect your agent

### Claude Code

```bash
claude mcp add mesh-mcp -- mesh-mcp run
```

### Cursor, VS Code, Windsurf

`.cursor/mcp.json` or `.vscode/mcp.json` at the repo root:

```json
{
  "mcpServers": {
    "mesh-mcp": {
      "command": "mesh-mcp",
      "args": ["run"]
    }
  }
}
```

`mesh-mcp init --auto --write-ide-config` writes both files for you.

### Verify by hand

The server speaks JSON-RPC on stdio, so you can test it without an agent:

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | mesh-mcp run --standalone
```

You should get six tools back. To try a real query:

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"find_dependents","arguments":{"target":"YourSharedType"}}}' \
  | mesh-mcp run --standalone
```

### Daemon vs standalone

`mesh-mcp run` (default) connects to a shared `meshd` daemon over a Unix socket, auto-spawning
it if needed. Several IDE windows then share one index. The daemon exits on its own once idle.

`mesh-mcp run --standalone` keeps everything in one process — use it in containers, in CI, or
anywhere a Unix socket isn't available.

To manage the daemon explicitly:

```bash
meshd --idle-timeout-minutes 30    # foreground
pkill meshd                        # stop it; the next run respawns it
```

---

## Step 6 — Teach it your conventions

gRPC, Spring, OpenAPI and AsyncAPI are recognised out of the box. Your in-house event bus is
not. If Step 4 showed services but no topics or events, this is the missing piece.

### Search vocabulary

```toml
[engines.docs]
enabled = true
paths = ["${workspace_root}/docs"]

# Your shorthand → the term actually written in your docs.
aliases = { "k8s" = "kubernetes", "dlq" = "dead-letter-queue" }
stop_words = ["the", "how", "what"]
exact_phrase_boost = 60
sanitize_prompt_injections = true
```

Without `aliases`, an agent asking about "the DLQ" finds nothing in a document that only says
"dead-letter-queue".

### Custom contract patterns

Describe how your codebase declares producers, consumers, sagas and RPC calls:

```toml
[[engines.contracts.patterns]]
name = "transactional-outbox"
kind = "topic_producer"        # topic_producer | topic_consumer | saga | rpc
file_pattern = "*.ts"          # optional; matched against the path
regex = 'createEvent<([^>]+)>'
target_group = 1               # capture group holding the event/topic name

[[engines.contracts.patterns]]
name = "event-post-processor"
kind = "topic_consumer"
file_pattern = "*.ts"
regex = 'class\s+(\w+)\s+extends\s+\w*PostProcessor<([^>]+)>'
target_group = 2               # the event name
consumer_group = 1             # the class consuming it
```

Producers and consumers naming the same topic get linked automatically, which is what makes
`analyze_impact` able to trace an event end to end.

Use single-quoted TOML strings for regexes so you don't have to double every backslash. Check
your work with `mesh-mcp graph --format mermaid`; an invalid regex is logged and skipped rather
than failing the run, so watch stderr.

[`examples/polyglot-shop/mesh-mcp.toml`](examples/polyglot-shop/mesh-mcp.toml) has working
patterns for TypeScript, Go, Rust, Python and Java — start from those.

### gRPC specifics

```toml
[engines.contracts.grpc]
proto_dirs = ["${workspace_root}/proto-registry/proto"]
controller_annotations = ["@GrpcMethod", "@GrpcService"]
canonical_fqcn_projection = true
```

### Spring properties

```toml
[engines.contracts.spring]
enabled = true
property_files = [
  "**/src/main/resources/application*.yml",
  "**/src/main/resources/application*.properties",
]
resolve_placeholders = true
auto_redact_secrets = true      # keep this on
```

Any key whose name looks like a secret (`password`, `token`, `secret`, `key`, …) is replaced
with `[REDACTED_SECRET: USE_ENV_OR_LOCAL_FALLBACK]` before it can reach a prompt.

---

## Step 7 — Put your playbooks in front of the agent

An indexed repo tells the agent what the code *is*. It doesn't tell it how your team works.
Skills close that gap: a Markdown file you already have (or write once), surfaced automatically
whenever the agent touches the area it covers.

### 1. Write the playbook

`.agents/skills/proto-contract-evolution.md`:

```markdown
---
name: proto-contract-evolution
description: How to evolve a proto contract without breaking consumers.
---

# Evolving a proto contract

1. Never renumber or reuse a field tag. Mark removed fields `reserved`.
2. Open the PR against `proto-registry` alone and wait for CI to publish the stubs.
3. Only then bump the dependency in consuming services.
```

The `description:` line is what the agent sees first, so make it a concrete trigger, not a
title. If there's no frontmatter, the first `# H1` is used instead.

### 2. Map it to an area

```toml
[engines.policy.skills]
# Key = an MCP tool name, or any fragment of the scope/target being queried.
"proto-registry"   = ".agents/skills/proto-contract-evolution.md"
"services/billing" = ".agents/skills/billing-invariants.md"
"smart_search"     = ".agents/skills/how-we-search.md"
```

Matching rules:

- An exact **tool name** key (`smart_search`, `find_dependents`, `analyze_grpc`,
  `analyze_impact`, `search_docs`) fires on every call to that tool.
- Otherwise the key is matched case-insensitively against the **scope or target** of the call —
  the same way `stop_rules` are matched.
- Among several matching path keys, the **longest** wins, so `services/billing` beats
  `services`.

### 3. See it work

Any matching tool call now comes back with a footer:

```
---
**Project skill for this area**: `.agents/skills/proto-contract-evolution.md` — How to evolve a proto contract without breaking consumers.
Read it before proposing changes here.
```

The agent reads the file and follows your process instead of inventing one.

### 4. Verify

```bash
mesh-mcp doctor
```

```
✔ Project skills: 3 configured, all files found
```

If a path is wrong, `doctor` names the offending key — without this check, a typo would just
silently never recommend anything:

```
✖ Project skills: 1 of 3 file(s) missing — these keys will never recommend anything:
    proto-registry -> .agents/skills/typo.md
```

Paths are resolved as given, or relative to the config file's directory.

---

## Step 8 — Guard critical paths (optional)

Skills advise. Stop rules refuse.

```toml
[engines.policy]
enabled = true
enforce_git_hooks = true
cryptographic_audit_trail = true

[engines.policy.stop_rules]
"proto-registry" = "STOP: proto-registry generates the TS/Go/Java stubs. Land the contract PR and let CI publish before touching consumers."
"k8s-infrastructure" = "STOP: manifest changes require DevOps review."
```

Install the hook that enforces them:

```bash
mesh-mcp install-hooks
```

A commit touching a guarded path is now rejected with a structured explanation of the required
workflow. Read-only queries are never blocked — inspecting a guarded contract is allowed and
expected; only mutations are.

Every tool call is appended to a SHA-256 hash-chained SQLite log at
`~/.cache/mesh-mcp/audit.db` (mode `0600`). Details in
[docs/governance-rsah.md](docs/governance-rsah.md).

---

## Config reference

Only these sections exist. The parser rejects unknown keys, so a typo or an invented section
fails loudly at startup rather than being ignored.

Keys marked **wired** change behaviour. Keys marked *accepted* parse without error but are not
read by anything yet — they are placeholders for planned engines, listed here so you know not to
rely on them.

| Section / key | Status |
| :--- | :--- |
| `[workspace]` `name`, `version`, `workspace_root`, `roots`, `exclude_patterns` | **wired** |
| `[workspace.mount_aliases]` | **wired** — translates container bind-mount paths (e.g. `/workspace`) to local host roots |
| `[engines.docs]` `enabled`, `paths`, `aliases`, `stop_words`, `exact_phrase_boost`, `fuzzy_fallback`, `sanitize_prompt_injections` | **wired** — `enabled` toggles doc indexing, `paths` restricts doc scope (`"docs/**"` or `"${workspace_root}/docs/**"`), `fuzzy_fallback` controls full-text fallback |
| `[[engines.contracts.patterns]]` `name`, `kind`, `file_pattern`, `regex`, `target_group`, `consumer_group` | **wired** |
| `[engines.contracts.grpc]` `proto_dirs`, `controller_annotations`, `canonical_fqcn_projection` | **wired** — scopes `.proto` dirs, configures controller annotations (e.g. `@GrpcMethod`), and controls canonical FQCN projection |
| `[engines.contracts.spring]` `enabled`, `property_files`, `resolve_placeholders`, `auto_redact_secrets` | **wired** — scopes property files, resolves `${...}` placeholders, and masks sensitive secrets |
| `[engines.contracts.openapi]` `enabled`, `spec_files` | **wired** — scopes OpenAPI spec detection |
| `[engines.contracts.asyncapi]` `enabled`, `spec_files`, `infer_string_topics` | **wired** — scopes AsyncAPI spec detection and topic string inference |
| `[engines.contracts.cpp]` `include_paths` | **wired** — resolves C++ `<header.h>` angle-bracket include targets |
| `[engines.policy.stop_rules]` | **wired** (evaluated in tool dispatch and git pre-commit hook) |
| `[engines.policy.skills]` | **wired** |
| `[engines.policy]` `enabled`, `enforce_git_hooks`, `cryptographic_audit_trail` | **wired** — `enabled=false` turns off stop rules/skills; `enforce_git_hooks` gates hook installation; `cryptographic_audit_trail` gates audit logging |

Practical consequence: **all configuration sections actively control their respective indexing and governance behavior**. Secret masking and prompt-injection sanitisation are enabled by default and can be configured per section.

There is no `[engines.watcher]` and no `[engines.audit]` section: the watcher is always on with
a 150 ms debounce, and the audit log path is fixed at `~/.cache/mesh-mcp/audit.db`.

A complete annotated example lives in [`mesh-mcp.toml`](mesh-mcp.toml) at the repo root.

---

## Troubleshooting

**`mesh-mcp: command not found`**
`~/.local/bin` isn't on your `PATH`. See Step 1.

**`doctor` says the config is valid but `graph` scans 0 files**
Your roots resolve to the wrong place. Almost always the `.agents/` relative-path trap —
see [Step 3](#the-mistake-everyone-makes). Print what's actually resolved:
```bash
RUST_LOG=info mesh-mcp graph --format json 2>&1 >/dev/null | grep -i root
```

**`unknown field` on startup**
A section or key that doesn't exist — check it against the [config reference](#config-reference).
Common culprits: `[engines.watcher]`, `[engines.audit]`.

**Files are scanned but produce no nodes**
The files parsed, but nothing matched a known contract shape. Either the language isn't
supported, or your conventions need [custom patterns](#custom-contract-patterns).

**A specific file is never indexed**
It probably tripped a guard: over 384 KB (1.5 MB for `.proto` and generated schema stubs), a
line longer than 1 KB (minified), a null byte, or nesting deeper than 64. Run with
`RUST_LOG=debug` to see the rejection. If it's a generated file (a build-emitted OpenAPI spec,
a compiled proto stub), also check whether it's gitignored — the crawler always respects
`.gitignore`, with no config flag to disable it, so a file that never gets committed never gets
scanned either.

**`doctor` warns a `proto_dirs`/`spec_files`/`docs.paths` pattern "matched 0 files"**
Either the pattern is wrong, or the target file is gitignored (see above) — `doctor` can't tell
these apart, it only knows nothing matched. Check `git check-ignore -v <path>` on the file you
expected to be indexed before assuming the pattern syntax is at fault.

**`smart_search` returns nothing for a name you can see in the code**
By design: it searches *declared symbols*, not raw text. For a full-text scan of the scope, pass
`fuzzy: true`.

**`Sandbox escape attempt detected`**
The requested path is outside every configured root. That's the security jail working — add the
directory to `roots` if it should be readable.

**Changes aren't picked up**
The watcher debounces 150 ms and rescans in the background. On Linux, a large workspace can
exhaust inotify watches:
```bash
echo fs.inotify.max_user_watches=524288 | sudo tee -a /etc/sysctl.conf && sudo sysctl -p
```

**Stale index after switching branches**
`pkill meshd` — the next `mesh-mcp run` respawns it with a fresh scan.

---

## Next

- [docs/mcp-tools.md](docs/mcp-tools.md) — every tool's arguments and output format
- [docs/architecture.md](docs/architecture.md) — how indexing and the security jail work
- [docs/development.md](docs/development.md) — building, testing, adding a language
- [README.md](README.md) — overview and CLI reference
