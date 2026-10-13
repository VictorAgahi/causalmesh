# 6. Safety and limits

MeshMCP reads a developer's source code and answers an AI agent. This chapter lists what protects
that, how, and, as important, what is not claimed. Reference version: sections 4, 5, 8.3 and 9 of
[`../architecture.md`](../architecture.md); governance in [`../governance-rsah.md`](../governance-rsah.md).

## What it can read: the path jail

Every path a tool receives goes through `ValidatedScope` (`crates/mesh-core/src/security.rs`):

1. normalize the Unicode,
2. translate container paths through `[workspace.mount_aliases]` if configured,
3. anchor a relative path on the workspace root,
4. clean the path (`..`, `.`) and resolve it to a real path, following symbolic links,
5. require the result to lie under one of the configured roots (case-folded on macOS and Windows,
   exact on Linux).

A failure is a tool error with code `-32602`. The crawler never follows symbolic links, so a link that
points outside a root is never indexed. One deliberate exception: `smart_search` treats an omitted
scope, `"."`, `"*"` or the exact workspace root as "every configured root"; any other ancestor of the
roots is still rejected.

Limit: the jail restricts what the *tools* read. It does not sandbox the process.

## What it will parse: guards

File size, binary content, line length and nesting are checked before the C parser runs (chapter 2).
Parsers have time budgets: 2 seconds when indexing (a hang guard, deliberately generous because a
short budget made results depend on CPU load) and 500 milliseconds when rendering a result on an
agent's request. Tree-sitter queries are bounded to 500 matches and 10,000 steps. YAML is read as a
stream with budgets so an alias bomb ingests nothing.

## What it prints: secrets and injected text

- Values of keys that look like secrets (`password`, `secret`, `token`, `credential`, `apikey`, ...)
  in YAML and `.properties` files are replaced with `[REDACTED_SECRET: USE_ENV_OR_LOCAL_FALLBACK]`.
  The same key-based masking applies to `key: value` lines in `smart_search` snippets.
  `.env*`, `*.pem` and `*.key` are excluded from indexing by default.
- Indexed Markdown is scanned for a fixed list of known injection phrases and chat-template markers,
  which are replaced before `search_docs` returns them.

Limits: both are name- and phrase-based. A secret assigned to an innocuous variable name, or embedded in
a string literal in code, is not detected. The injection filter is a fixed list, not a classifier; a
reworded instruction is not caught.

## No network code, and on Linux a kernel guarantee

The binaries contain no network client. On Linux, right after binding its socket, `meshd` installs a
seccomp filter (a Linux kernel feature that lets a process forbid itself certain system calls) on all of its threads that makes creating IPv4 and IPv6 sockets, and `io_uring` (a Linux interface that can create sockets without the usual call), fail.
If the kernel refuses the filter, the daemon logs a warning and keeps serving, unless
`MESH_DAEMON_SANDBOX=required` is set, in which case it exits. `mesh-mcp doctor` reports whether the
running daemon is confined.

Limits: only `meshd` is confined, only on Linux (x86_64, aarch64, riscv64). `mesh-mcp run --standalone`,
macOS and Windows are not kernel-confined. It is a network sandbox, not a filesystem one.

## What it records: the audit chain

Chapter 4 describes the log. The hash chain covers timestamp, session, tool, argument digest, status,
files accessed and redaction count. Error messages are bound to their row by a separate signature, so
older binaries still verify the chain. The chain uses plain SHA-256 with no secret key: it reveals an accidental or partial edit, but a
local user who rewrites the file and recomputes every hash is not detected. It is evidence for a
reviewer, not protection, and no regulatory certification is claimed.

## Optional governance

`[engines.policy.stop_rules]` names guarded paths. With `read_governance_mode = "enforce_refusal"`,
a tool call about a guarded path returns a structured refusal telling the agent to stop and report to
a human; `audit_warn` only logs; the default `allow_all` never refuses a read. `mesh-mcp install-hooks`
adds a Git pre-commit hook that rejects a commit staging both a contract (`.proto` under the
configured `proto_dirs`) and source files. No shipped tool writes anything, so refusals only occur if
you opt in. Both mechanisms are bypassable by an agent with a shell (`git commit --no-verify`, editing
files directly): this is guidance and evidence, not access control.

## Process politeness

Indexing yields to the editor (chapter 5). Output is capped at 48 KB so an answer cannot flood an
agent's context. Standard output is reserved for protocol frames.

## Things deliberately not promised

- Accuracy beyond what is measured. Extraction accuracy is measured against hand-written ground truth
  only for gRPC edges on three open-source demo applications.
- Behaviour on workspaces larger than those measured, or a daemon left running for weeks.
- Completeness of any answer. Zeros are "not statically resolved in the indexed roots".
- Protection from an agent that has a shell.

Next: [7. Rust for readers of this code](07-rust-for-readers.md).
