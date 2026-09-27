# Pilot scorecard — A/B measurement protocol (template)

> Plan 4 step 4.8. This is an **empty template**: it holds the protocol and the grids to fill in
> during the pilot (run by a human, after the 6.1.0 release). Every cell is filled from a
> measurement taken during the pilot; nothing is pre-filled, estimated or extrapolated. A value
> that could not be measured is written `not measured`, never guessed.

## 1. Question

On real tasks from the pilot team's own repositories, does an AI coding agent **with** MeshMCP
use fewer tokens and tool calls, take less time, and succeed as often or more often than the same
agent **without** MeshMCP?

## 2. Setup

| Item                                                     | Value (fill in) |
| ----------------------------------------------------------| -----------------|
| Pilot team / repositories (names, commit SHAs)           |                 |
| Workspace size (repos, files indexed: `mesh-mcp doctor`) |                 |
| Agent tool and version (e.g. Claude Code x.y.z)          |                 |
| Model id (identical in both conditions)                  |                 |
| MeshMCP version (`mesh-mcp --version`)                   |                 |
| Machine (OS, CPU, RAM)                                   |                 |
| Install command used (`scripts/install_pilot.sh …`)      |                 |
| Dates of the runs                                        |                 |

## 3. Tasks

Pick **at least 6** real tasks the team would have done anyway (tickets, bug fixes, "where is X
used" investigations, cross-service changes). Write each task's prompt and its **success criterion**
*before* any run, and freeze both; the success criterion is judged, not the agent's own claim.

| # | Task (frozen prompt, verbatim, or link) | Kind (lookup / cross-repo change / bug fix / …) | Success criterion (written before the runs) | Judged by (test command, reviewer) |
|---|---|---|---|---|
| T1 | | | | |
| T2 | | | | |
| T3 | | | | |
| T4 | | | | |
| T5 | | | | |
| T6 | | | | |

Success is judged by, in order of preference: a test or command that passes/fails (write the
command), otherwise a reviewer from the team who does not know which condition produced the
result (blind review), using the criterion above. Partial success counts as failure.

## 4. Conditions

- **A — without MeshMCP**: MeshMCP server removed from the agent's MCP config (check with the
  agent's own MCP listing before the run). Everything else identical.
- **B — with MeshMCP**: `mesh-mcp` configured by `install_pilot.sh` (`init --write-ide-config`),
  index warm (`mesh-mcp doctor` clean, daemon already running) before the first measured run.

## 5. What is measured, per run

| Measure | Source | Notes |
|---|---|---|
| Tokens consumed by the agent (input, output, cache-read, cache-write, separately) | The agent tool's own usage report for the session (e.g. Claude Code `/cost` or its session log) | Never computed from MeshMCP data. MeshMCP does not estimate "tokens saved". |
| Number of tool calls (all tools, and MeshMCP tools separately) | The agent's session transcript | |
| Wall-clock time to task end | Timestamps of first prompt and final answer | |
| Task success (yes/no) | Criterion of §3, judged as §3 says | |
| Human interventions (count) | Operator notes | A run needing an intervention is flagged, not dropped. |
| MeshMCP-side: latency p50/p95 per tool, isError rate, index cache hit rate, daemon restarts | `mesh-mcp stats --since <window>` (condition B only) | Attach the raw output (§8). |

## 6. Repetitions and bias control

- **Repetitions**: each task runs **at least 3 times per condition** (≥ 6 tasks × 2 conditions ×
  3 = ≥ 36 runs). Report every run, including failed and aborted ones.
- **Order**: alternate the condition that runs first per task (ABBA across repetitions:
  A,B,B,A,A,B…) so neither condition systematically benefits from the operator learning the task.
- **Same model, same prompt**: the identical frozen prompt and model id in both conditions; no
  prompt wording that names MeshMCP tools.
- **Fresh session per run**: a new agent session each time; no conversation history, no memory
  or notes file carried between runs (disable or clear agent memory features for the pilot).
- **Cache**: the provider-side prompt cache can make later runs cheaper regardless of condition.
  Record cache-read tokens separately (§5) and compare conditions on both total tokens and
  non-cached input tokens. Keep MeshMCP's own index warm in B (it is part of the product), and
  note any daemon restart shown by `mesh-mcp stats`.
- **Same repository state**: every run starts from the same commit (`git stash`/`git checkout`
  back to the recorded SHA; clean working tree).
- **Operator**: the same person operates both conditions of a task; they do not help the agent
  beyond what is written in the operator notes.

## 7. Results grid (fill in, one row per run)

| Run | Task | Condition (A/B) | Order in pair | Input tokens | Output tokens | Cache-read tokens | Cache-write tokens | Tool calls (all) | MeshMCP tool calls | Wall time (s) | Success (Y/N) | Interventions | Notes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | | | | | | | | | | | | | |
| 2 | | | | | | | | | | | | | |
| 3 | | | | | | | | | | | | | |

### Per-task summary (computed from the grid above, medians over repetitions)

| Task | Median tokens A | Median tokens B | Median tool calls A | Median tool calls B | Median time A | Median time B | Successes A (k/n) | Successes B (k/n) |
|---|---|---|---|---|---|---|---|---|
| T1 | | | | | | | | |

Report medians and the full per-run spread; with n = 3 per cell, do not report percentages of
"savings" beyond what the per-run data shows, and state when the two conditions overlap.

## 8. MeshMCP stats to attach

Run after the last condition-B run of the pilot, and attach the raw output unedited:

```bash
mesh-mcp --version
mesh-mcp stats --since all      # whole pilot
mesh-mcp stats --since 7d       # last week of the pilot
mesh-mcp doctor --json          # health at the end of the pilot
```

`stats` reports, from the local audit database only: calls, isError count and rate per tool,
latency p50/p95 per tool (nearest-rank over timed calls), index cache hit rate (hits / lookups
recorded per indexing pass), and `meshd` restarts. A line saying "not recorded" means the audit
database predates 6.1.0 for that metric; it is not a zero.

## 9. Conclusion (fill in after the runs)

- Outcome per measure (tokens, tool calls, time, success), with the per-run evidence:
- Tasks where B was worse, and why (transcript excerpts):
- Frictions met during installation and use:
- Decision (adopt / extend the pilot / stop):
