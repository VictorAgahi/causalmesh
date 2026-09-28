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

`install.sh` downloads the rolling `latest` release, rebuilt from `main` on every merge, for macOS
(Apple Silicon and Intel), Linux x86-64 and Windows x86-64. On other platforms (Linux arm64, for
example) it falls back to `cargo install` from the repository, which needs a Rust toolchain.

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

### Pilot install (one command, from a checkout)

For a pilot, `scripts/install_pilot.sh` does Step 1 and the IDE part of Step 5 in one go, and
can be re-run safely (a second run changes nothing):

```bash
cd /path/to/your/workspace
/path/to/causalmesh/scripts/install_pilot.sh            # builds with cargo --release, asks before init
/path/to/causalmesh/scripts/install_pilot.sh --bin-dir ~/Downloads/mesh-mcp --yes   # prebuilt binaries, no prompt
```

It detects the OS and architecture (macOS and Linux, x86-64 and arm64), copies `mesh-mcp` and
`meshd` into `~/.local/bin` (`--prefix` to change) only when they differ, runs
`mesh-mcp init --auto --write-ide-config` in the workspace (`--workspace` to change) after
confirmation — merging into `.cursor/mcp.json` and `.vscode/mcp.json` without dropping your other
servers, and keeping an existing `.agents/mesh-mcp.toml` byte-for-byte — then checks the result
with `mesh-mcp doctor --json` and fails loudly on any `error` check. It never downloads anything.

During the pilot, `mesh-mcp stats --since all` reports calls per session (one per agent
connection; `--session <id>` keeps one), per-tool latency p50/p95 (p95 from 20 timed calls on),
the `isError` rate, the index cache hit rate and `meshd` restarts from the local audit database;
`--json` prints the same summary on stdout. Point `MESH_AUDIT_DB` (or `stats --db`) at a
dedicated file to keep pilot calls apart from any other use of MeshMCP on the machine. The A/B
measurement protocol is in [`docs/pilot-scorecard.md`](docs/pilot-scorecard.md).

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

If the agent sees the workspace under a different path than the server (a Docker bind mount, a
devcontainer), map the container path to the host path:

```toml
[workspace.mount_aliases]
"/workspace" = "${workspace_root}"
```

A path argument starting with `/workspace` is translated before the jail check instead of being
rejected. The translation applies to the path arguments tools accept (`smart_search`'s `scope`,
a `.proto` path given to `analyze_grpc`).

---

## Step 4 — Check it actually indexed something

Two commands. Do not skip these — a misconfigured root fails quietly, by indexing nothing.

```bash
mesh-mcp doctor
```

Abridged output (the exact lines depend on the platform and configuration):

```
🔍 Running MeshMCP Diagnostic Healthcheck (v7.0.0, commit: ...)...

✔ Config syntax: valid (.agents/mesh-mcp.toml)
ℹ Project skills: none configured ([engines.policy.skills])
✔ Jailed roots: 6 allowed root(s) resolved
✔ Root overlap: 6 root(s), none overlapping
✔ Symlink invariants: follow_links=false verified (crawler rejects symlink traversal)
✔ Secret redaction engine: active (test secrets masked)
✔ Tree-sitter parsers: initialized (Java, Go, Python, TypeScript, Rust, C++, Kotlin, C#, Ruby, PHP, Swift, Scala, Protobuf)
✔ Toolchain utilities: git and ripgrep detected
⚠ Index health: ... file(s) scanned, ... indexed, ... not indexed, first 10 listed
    (... non-source file(s) skipped: images, fonts, archives, lockfiles, bundles)
    Searches cannot return these files; read them directly.

✔ Socket permissions: socket 0600, directory 0700 (owner-only)
✔ Audit trail: quick_check and hash chain: ok
```

`mesh-mcp doctor --json` prints every one of these checks as a JSON array on stdout; the last
block (socket, daemon version, Linux sandbox, caches, audit chain) is what `mesh-mcp doctor --fix`
repairs. Some lines are
informational only; see [docs/development.md](docs/development.md#6-mesh-mcp-doctor) for which.

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

### Cursor, Windsurf

`.cursor/mcp.json` at the repo root:

```json
{ "mcpServers": { "mesh-mcp": { "command": "mesh-mcp", "args": ["run"] } } }
```

### VS Code

`.vscode/mcp.json` (VS Code keys servers under `servers`):

```json
{ "servers": { "mesh-mcp": { "type": "stdio", "command": "mesh-mcp", "args": ["run"] } } }
```

`mesh-mcp init --auto --write-ide-config` writes both files for you. It only adds or replaces the
`mesh-mcp` entry: other servers are kept, and a file that is not valid JSON is left untouched.

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

`mesh-mcp run` (default) connects to a `meshd` daemon over a Unix socket scoped to *this
workspace* (a hash of its canonical path + the binary version), auto-spawning it if needed.
Several IDE windows open on the *same* repo then share one index; a different repo always gets
its own daemon and socket — they never mix, even if both happen to be running at once. The
daemon exits on its own once idle.

`mesh-mcp run --standalone` keeps everything in one process — use it in containers, in CI, or
anywhere a Unix socket isn't available.

To manage a workspace's daemon explicitly:

```bash
meshd --idle-timeout-minutes 30    # foreground, indexes the cwd's workspace
pkill meshd                        # stop all daemons; the next `mesh-mcp run` respawns per-workspace
```

If you're upgrading from a version before P0 step 1.8 and see more than one `meshd` process, or
a socket at the old shared path (`~/.cache/mesh/meshd.sock`), it's safe to `pkill meshd` once —
every `mesh-mcp run` afterward resolves and (re)spawns the correct per-workspace daemon on its
own.

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
- Otherwise the key is matched case-insensitively as a substring of the **scope, target or
  query** of the call. (Stop rules, by contrast, match whole path segments.)
- Among several matching path keys, the **longest** wins, so `services/billing` beats
  `services`.

### 3. See it work

Any matching tool call now starts with this note:

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

Skills advise. Stop rules can refuse, and a Git hook can reject mixed commits.

```toml
[engines.policy]
enabled = true
enforce_git_hooks = true
cryptographic_audit_trail = true
read_governance_mode = "enforce_refusal"   # allow_all (default) | audit_warn | enforce_refusal

[engines.policy.stop_rules]
"proto-registry" = "proto-registry generates the TS/Go/Java stubs. Land the contract PR and let CI publish before touching consumers."
"k8s" = "Manifest changes require DevOps review."
```

A stop rule key is matched as a whole path segment of the call's scope or target. With
`read_governance_mode = "enforce_refusal"`, a tool call about a guarded path returns a structured
refusal (RSAH) telling the agent to stop and report to you; `audit_warn` only logs a warning;
the default `allow_all` never refuses a read. No shipped tool writes anything.

The Git hook is separate from stop rules:

```bash
mesh-mcp install-hooks
```

It rejects a commit that stages both a `.proto` under `[engines.contracts.grpc] proto_dirs` and
service source files, so contract changes land first. Without `proto_dirs` it checks nothing.

Every tool call is appended to a SHA-256 hash-chained SQLite log at
`~/.cache/mesh-mcp/audit.db` (mode `0600`); `mesh-mcp stats` summarises it. Details in
[docs/governance-rsah.md](docs/governance-rsah.md).

---

## Config reference

Only these sections exist. The parser rejects unknown keys, so a typo or an invented section
fails loudly at startup rather than being ignored.

Every key below changes behaviour.

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
| `[engines.policy.stop_rules]` | **wired** — evaluated in tool dispatch according to `read_governance_mode` (not used by the Git hook) |
| `[engines.policy.skills]` | **wired** |
| `[engines.policy]` `enabled`, `enforce_git_hooks`, `cryptographic_audit_trail`, `read_governance_mode` | **wired** — `enabled=false` turns off stop rules/skills; `enforce_git_hooks` gates hook installation; `cryptographic_audit_trail` gates audit logging; `read_governance_mode` (`allow_all`, `audit_warn`, `enforce_refusal`) decides whether stop rules refuse read-only calls |
| `[cache]` `max_size_mb` | **wired** — quota (MiB, default `2048`) of this workspace's parse cache `~/.cache/mesh-mcp/workspaces/<workspace_id>/index-cache.db`, database + WAL. Checked on open and after every scan writing more than 100 entries; over quota, least recently used entries are evicted down to 80 % of it |

Secret masking and prompt-injection sanitisation are on by default.

There is no `[engines.watcher]` and no `[engines.audit]` section: the watcher is always on with
a 150 ms debounce, and the audit log path is fixed at `~/.cache/mesh-mcp/audit.db`.

The parse cache is a disposable performance cache, one SQLite file per workspace (same
`workspace_id` as the daemon socket, so it also changes with the binary version). Deleting it is
always safe: the next boot re-parses everything. Versions up to 6.0.1 used a single
machine-wide `~/.cache/mesh-mcp/index-cache.db`; it is no longer read, and
`mesh-mcp doctor --fix` removes it.

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
line longer than 1 KB (minified), a null byte, or nesting deeper than 64. `mesh-mcp doctor`
lists rejected files with the reason, and `smart_search` / `find_dependents` name the rejected
source files inside the scope of a query. If it's a generated file (a build-emitted OpenAPI spec,
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
The watcher holds reloads while a Git operation is in progress and reindexes once it settles;
answers given meanwhile start with a "Git operation in progress" note. If the index still looks
stale, `pkill meshd` — the next `mesh-mcp run` respawns it with a fresh scan.

---

## Next

- [docs/mcp-tools.md](docs/mcp-tools.md) — every tool's arguments and output format
- [docs/architecture.md](docs/architecture.md) — how indexing and the security jail work
- [docs/development.md](docs/development.md) — building, testing, adding a language
- [README.md](README.md) — overview and CLI reference
