# Governance: stop rules, RSAH refusals, project skills and the pre-commit hook

MeshMCP has three governance mechanisms, configured under `[engines.policy]`:

| Mechanism | Where it acts | Effect | Default |
| :--- | :--- | :--- | :--- |
| **Project skills** (`[engines.policy.skills]`) | every matching MCP tool call | a note pointing the agent at your playbook for that area | none configured |
| **Stop rules** (`[engines.policy.stop_rules]`) + RSAH | MCP tool calls, depending on `read_governance_mode` | a structured refusal, or a logged warning | rules apply only to mutating tools, and no shipped tool mutates |
| **Pre-commit hook** (`mesh-mcp install-hooks`) | `git commit` | rejects commits that mix contract and implementation changes | not installed until you run the command |

`[engines.policy] enabled = false` turns off stop rules and skills.

---

## 1. Why

In a workspace where contracts (`.proto`, schemas) are published and consumed by several
services, an agent given a cross-cutting task tends to edit the contract and every consumer in
one change. Downstream builds then fail until the regenerated stubs are published, and nobody
reviewed the contract change on its own. The mechanisms below push the agent toward the team's
process (skills), let a team stop an agent at a guarded boundary (stop rules), and catch the
mixed commit if it happens anyway (hook).

---

## 2. Stop rules and RSAH refusals

```toml
[engines.policy]
enabled = true
read_governance_mode = "allow_all"   # allow_all (default) | audit_warn | enforce_refusal

[engines.policy.stop_rules]
"proto-registry" = "proto-registry generates the TS/Go/Java stubs. Land the contract PR first."
"k8s"            = "Manifest changes require DevOps review."
```

### Matching
`GovernanceEngine::evaluate_guard` (`crates/mesh-core/src/governance.rs`) lowercases the call's
subject and matches each key as a **whole path segment**: `proto` matches `proto/x.proto` and
`src/proto/x`, not `internal/prototype/x.go`. Both `/` and `\` separate segments.

The subject is the `scope` of `smart_search`, the `target` of `find_dependents`, `analyze_grpc`
and `analyze_impact`, and the `query` of `search_docs`. `visualize_mesh` has no subject.

### When a rule applies
`ToolRegistry::invoke` (`crates/mesh-server/src/tools/mod.rs`) consults the rules before the tool
runs:

| Call | `allow_all` | `audit_warn` | `enforce_refusal` |
| :--- | :--- | :--- | :--- |
| Read-only tool (all six shipped tools) | runs | runs, a warning is logged on stderr | refused with RSAH |
| Tool that declares `mutates() == true` (none shipped) | refused with RSAH | refused | refused |

So with the default configuration no call is ever refused: stop rules only take effect if you
set `read_governance_mode = "enforce_refusal"` (or `audit_warn` for logging only), or if a future
tool declares that it mutates something.

### The refusal payload
A refusal is returned as an MCP tool result with `isError: true` (not a JSON-RPC error), so the
client hands it to the model, whose next step is spelled out. The text is a serialised
`RsahResponse`:

```json
{
  "status": "GOVERNANCE_BLOCKED",
  "policy": "CONTRACT_FIRST_CASCADE_CI",
  "violation": "Guarded contract boundary 'proto-registry' accessed. <your rule text>",
  "required_workflow": {
    "step_1": "Validate schema syntax and breaking changes in 'proto-registry'",
    "step_2": "Commit changes exclusively inside 'proto-registry'",
    "step_3": "Submit contract review and verify CI schema generation for 'proto-registry'",
    "step_4": "Do not modify downstream services until the generated contract packages are published."
  },
  "agent_next_action": "STOP_AND_REPORT_TO_USER",
  "message_to_user": "Detected mutation targeting contract in 'proto-registry'. ..."
}
```

The policy name and workflow depend on the key: a key containing `proto` gives
`CONTRACT_FIRST_CASCADE_CI`; one containing `k8s`, `infra` or `deploy` gives
`INFRASTRUCTURE_AS_CODE_REVIEW`; anything else gives a generic `ACTIVE_GOVERNANCE_POLICY`.
Refused calls are written to the audit log with status `ERROR`.

The in-server check is exercised by `test_invoke_blocks_mutating_call_on_guarded_subject` in
`crates/mesh-server/src/tools/mod.rs`.

---

## 3. Pre-commit hook

```bash
mesh-mcp install-hooks        # run at the repository root
```

writes `.git/hooks/pre-commit` (mode `0755` on Unix), unless
`[engines.policy] enforce_git_hooks = false`. The hook is generated from
`[engines.contracts.grpc] proto_dirs` (`crates/mesh-server/src/cli/hooks.rs`), not from the stop
rules. It rejects a commit that stages both:

- a `.proto` file under one of the configured `proto_dirs`, and
- a source file (`.go`, `.rs`, `.java`, `.kt`, `.ts`, `.tsx`, `.py`, `.cpp`, `.cs`, `.php`,
  `.rb`, `.swift`, `.scala`),

with a message asking to commit the contract change alone first. With no `proto_dirs`
configured the hook contains no check (there is no reliable contract boundary to enforce).

Limits: it is a client-side hook, so `git commit --no-verify` skips it, and it overwrites an
existing `pre-commit` hook file. It checks this one rule only.

---

## 4. Project skills: guidance before refusal

Stop rules refuse. Skills brief: they point the agent at the team's own playbook for an area
before it proposes a change.

### Configuration

```toml
[engines.policy.skills]
# Key = an MCP tool name, or any fragment of the scope/target being queried.
"proto-registry"   = ".agents/skills/proto-contract-evolution.md"
"services/billing" = ".agents/skills/billing-invariants.md"
"smart_search"     = ".agents/skills/how-we-search.md"
```

The value is a path to a Markdown file. If the file starts with YAML frontmatter containing
`description:`, or with an `# H1`, that line (up to 200 bytes) is quoted so the agent can judge
relevance without opening the file. Relative paths are resolved against the config file's
directory.

### Matching
Implemented by `GovernanceEngine::recommend_skill`:

1. An exact MCP tool name key fires on every call to that tool and takes precedence.
2. Otherwise each key is matched as a case-insensitive **substring** of the call's subject (same
   subjects as stop rules above). Note the difference with stop rules, which match whole path
   segments.
3. Among several matching keys, the longest wins, so `services/billing` beats `services`.

### Effect
On a successful call (and not for `visualize_mesh`'s JSON/HTML documents), the response starts
with:

```
---
**Project skill for this area**: `.agents/skills/proto-contract-evolution.md` — How to evolve a proto contract without breaking consumers.
Read it before proposing changes here.
```

### Validation
A mistyped path would never fire, so `mesh-mcp doctor` checks every configured skill file:

```
✔ Project skills: 3 configured, all files found
```
```
✖ Project skills: 1 of 3 file(s) missing — these keys will never recommend anything:
    proto-registry -> .agents/skills/typo.md
```

---

## 5. What this does and does not provide

- It gives the agent the team's process at the right moment (skills), an explicit stop signal it
  can relay to a human (RSAH, when enabled), and a last check at commit time (hook).
- It is not an access-control system. An agent with shell access can still edit files directly
  and bypass the hook; the MCP tools themselves only read. The path jail
  ([architecture.md](architecture.md) §4) limits what the tools can read, and the audit trail
  (§9) records what they were asked.
- No regulatory certification is claimed. The audit log and the human-handoff refusal can be
  part of an organisation's own oversight process; whether they satisfy a given framework is for
  that organisation to assess.
