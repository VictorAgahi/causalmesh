# MeshMCP setup guide for AI agents

You are an AI coding agent (Claude Code, Cursor, Copilot, …) asked to install or configure
MeshMCP for a user. This guide is written for you. Read it entirely before running anything;
`mesh-mcp agent-guide` prints the copy matching the installed binary.

MeshMCP is a local MCP server that indexes several repositories at once (declared symbols,
imports, gRPC contracts, events/queues, docs) so an agent can answer cross-repository questions
without reading everything. Its answers are only as good as its configuration, and a wrong
configuration fails **quietly** (it indexes less, it does not error). Your job is to leave the
user with a configuration you have **verified**, and to tell them plainly what you could not verify.

---

## 0. Rules

1. **Ask before** any of these, and say why:
   - editing a *global* agent config (`~/.claude.json`, `~/.cursor/mcp.json`, user-scope servers);
   - `mesh-mcp install-hooks` — it **overwrites** `.git/hooks/pre-commit` (an existing husky,
     lefthook or custom hook is lost; check the file first);
   - `mesh-mcp doctor --fix` — it may stop a running `meshd` and delete caches (never the audit log).
2. **Never overwrite an existing `.agents/mesh-mcp.toml`.** It is the user's (or their team's)
   tuned config: read it, and edit it by hand if something is missing. `mesh-mcp init` keeps it;
   never pass `init --force` without the user's explicit agreement.
3. Everything runs locally. Nothing is uploaded. Do not invent network steps.
4. Do not tell the user it "works" until section 5 passed. Report what you checked, with numbers.

---

## 1. Check the install

```bash
command -v mesh-mcp meshd      # both must resolve
mesh-mcp --version
```

If `mesh-mcp` is not found, the installer put it in `~/.local/bin`: add that directory to the
user's `PATH` (ask which shell profile to edit) or call it by absolute path.

---

## 2. Choose the workspace root

The workspace root is the directory from which every repository the agent must reason across is
reachable, and where `.agents/mesh-mcp.toml` will live:

- **Monorepo**: its root.
- **Several sibling repositories** (`~/code/api-gateway`, `~/code/ms-user`, …): their **parent
  directory** (`~/code`). Cross-repository questions (who calls this RPC, who imports this
  package) only work across repositories listed in the *same* config.

Ask the user which repositories belong together if it is not obvious. Run every command below from
the workspace root.

---

## 3. Write the configuration

### 3.1 Generate a draft (skipped when a config exists)

```bash
mesh-mcp init --auto
```

It writes `.agents/mesh-mcp.toml` from directory heuristics (`proto*/`, `api-gateway/`,
`services/*`, `packages/*`, `crates/*`, `apps/*`, `src/*`, `docs/`, `deploy*`, `k8s*`, and
`package.json` / `go.mod` / `pom.xml` / `pyproject.toml` / `Cargo.toml` at the top). **It does not
detect arbitrary sibling repositories**: for a parent directory of separate repos you will almost
always have to write `roots` yourself.

### 3.2 Review and fix it

Paths in the file are relative to the file's own directory (`.agents/`), hence the `..`.
`${workspace_root}` defaults to that parent.

```toml
[workspace]
name = "my-platform"
version = "1.0.0"
workspace_root = "${WORKSPACE_ROOT:-..}"
roots = [                                   # one entry per repository or source tree
  "${workspace_root}/proto-registry",
  "${workspace_root}/api-gateway",
  "${workspace_root}/services/*",           # a glob expands to one root per directory
  "${workspace_root}/docs",
]
exclude_patterns = [                        # .gitignore is honored as well
  "**/node_modules/**", "**/dist/**", "**/build/**", "**/target/**", "**/coverage/**",
  "**/.env*", "**/secrets/**", "**/*.pem", "**/*.key",
]
```

Checklist, in this order:

- **`roots`**: every repository the user named, nothing else. Roots must not contain each other
  (`doctor` warns). Exclude mirrored or vendored copies of the same code (git submodules that
  duplicate another root, generated SDKs): duplicates make every answer ambiguous.
- **Secrets**: keep the `.env*` / key / certificate excludes. Values of secret-looking keys are
  masked in outputs, but excluded files are never read at all.
- **gRPC** (only if the workspace has `.proto` files):
  ```toml
  [engines.contracts.grpc]
  proto_dirs = ["${workspace_root}/proto-registry/proto"]
  controller_annotations = ["@GrpcMethod"]   # NestJS; add your own handler decorators
  ```
- **Events and queues** that are not a built-in pattern (custom outbox, stream wrappers, base
  classes): declare them with regex patterns. Rules that matter:
  - capture the **topic value** (`EVENT_CREATED`, `orders.created`), never a type expression
    (`typeof Foo.BAR`) or a whole call;
  - producers and consumers must yield the **same key**, or they are never linked;
  - test each regex against real lines (`grep -rnE '<regex>' <repo> --include='*.ts' | head`)
    before adding it.
  ```toml
  [[engines.contracts.patterns]]
  name = "outbox-producer"
  kind = "topic_producer"          # topic_producer | topic_consumer | saga | rpc
  file_pattern = "*.ts"
  regex = 'createEvent<\w+\.(\w+)>'
  target_group = 1                 # capture group holding the topic
  # consumer_group = 1             # topic_consumer only: capture group naming the consumer
  ```
- **Governance** (optional): `[engines.policy.stop_rules]` maps a path fragment to a message
  returned instead of letting an agent modify it, e.g. a contract registry that generates code
  for other repos. Propose it; do not invent rules the user did not ask for.
- **Docs**: `[engines.docs] paths` should point at real architecture/ADR folders.

The full annotated reference:
<https://github.com/VictorAgahi/causalmesh/blob/main/mesh-mcp.toml>.

---

## 4. Register the server with the agent

The server is started by the agent client, from the workspace root, as plain `mesh-mcp` (it
finds `.agents/mesh-mcp.toml` in its working directory and starts or reuses a background `meshd`).

- **Claude Code**, project scope (writes `.mcp.json`, shareable with the team):
  ```bash
  claude mcp add -s project mesh-mcp -- mesh-mcp
  ```
- **Claude Code, Cursor and VS Code at once**:
  ```bash
  mesh-mcp init --write-ide-config
  ```
  It merges a `mesh-mcp` entry into `.mcp.json`, `.cursor/mcp.json` and `.vscode/mcp.json`
  without touching other servers (a file that is not valid JSON is left untouched, with a
  warning), and generates `.agents/mesh-mcp.toml` only if none exists.

The client must be **restarted** (a new Claude Code session) before the tools appear. Tools:
`smart_search`, `find_dependents`, `analyze_grpc`, `analyze_impact`, `search_docs`,
`visualize_mesh`.

---

## 5. Verify (mandatory)

### 5.1 Health

```bash
mesh-mcp doctor          # human-readable; --json for the same checks as JSON
```

- No `✖` line may remain. Fix each one (the message says what is wrong) and re-run.
- Read **Index health**: `N file(s) scanned, M indexed, K not indexed`. Compare N with what you
  expect (for each root, `git -C <repo> ls-files | wc -l`, minus excluded paths). A root that
  resolves to nothing, or a scanned count far below expectations, means `roots` or
  `exclude_patterns` is wrong. Files listed as not indexed are oversized or binary: an agent must
  read those directly.
- `⚠ Root overlap`, `⚠ Duplicated submodule` (the same git submodule checked out in several roots,
  indexed once per copy) or `matched 0 files — likely dead config` lines point at config mistakes.

### 5.2 Answers against ground truth

After restarting the client, check three answers against `grep`, on the user's own code:

| Check | Tool call | Ground truth |
|---|---|---|
| A declared symbol is found | `smart_search` with a class name you know | `grep -rn "class <Name>"` |
| Cross-repo imports | `find_dependents` with a shared package name | `grep -rl "<package>" --include=*.ts` (excluding tests, `node_modules`, `dist`) |
| One gRPC method end to end (if any) | `analyze_grpc` with a method name | `grep -rn "rpc <Method>"` in the protos, and the client calls (`.<method>(`) |

Report each result to the user as *tool count vs grep count*. When the tool misses results grep
finds, say so explicitly and name the case: the user must know which questions MeshMCP answers
reliably on their code. Do not tune the config at random to make one number match.

### 5.3 Tell the user

A short summary: roots configured, files indexed / not indexed, which checks of 5.2 matched, which
did not, and anything you left for them to decide (stop rules, git hook, global config).

---

## 6. Using MeshMCP well afterwards

- `smart_search` matches **declared symbol names**, not free text; use `fuzzy: true` for a text
  scan. Follow the `offset` in a page footer instead of raising `limit`.
- `analyze_grpc`, `analyze_impact` and `visualize_mesh` label each link `exact`, `heuristic` or
  `ambiguous`, and say when a list may be incomplete ("may call", "No producer resolved").
  `smart_search` is a substring search: it lists exact declarations first and says how many other
  results only contain the query. `find_dependents` warns when its answer is a substring fallback.
  Treat anything `heuristic`, `ambiguous` or merely name-matched as a lead to confirm by reading
  the code, not as a fact.
- A tool answer that says a file is not indexed means: read that file directly.

---

## 7. Where things live

| What | Where |
|---|---|
| Config | `<workspace>/.agents/mesh-mcp.toml` (commit it so the team shares it) |
| Index cache | `~/.cache/mesh-mcp/workspaces/<id>/` (safe to delete; rebuilt on start) |
| Audit log | `~/.cache/mesh-mcp/audit.db` (mode `0600`, append-only, local); `MESH_AUDIT_DB` moves it; `mesh-mcp stats` summarizes it |
| Daemon socket | `~/.cache/mesh/` (Unix) or a named pipe (Windows) |

Uninstall: remove `mesh-mcp` and `meshd` from `~/.local/bin`, the `mesh-mcp` entries from the
agent configs, `.agents/mesh-mcp.toml`, and `~/.cache/mesh-mcp`.
