# Fleet Self-Build Autonomy Roadmap (ff council: codex + kimi, 2026-07-01)

Context: fleet EXECUTION works (scheduler → sub-agent worktrees → codex → salvage → PR;
real fleet-authored PRs #678-#682 landed). Gaps: planner disconnected from PM queue,
Lane-1 local codegen broken (everything falls to slow codex), no verify/review before
merge, no auto-retry, scheduler ramps 1/host/tick.

## Council consensus ranking (chairman: codex; both members strongly agreed)

Order of attack: **bridge (in progress) → verify → retry → execution-contract → Lane-1 → queue/host health → scheduler ramp → PR bot → feedback loop.**

0. **Planner→PM bridge** (IN PROGRESS) — `ff pm decompose <goal>`: decompose a goal/epic
   into leaf `task` work_items (parent_id set), flag ready → scheduler fans to fleet.
   Turns "hand-create every task" into "give it a goal."

1. **Automated verify/review gate before merge** — **M** — fmt + build + tests + clippy +
   an LLM code-review pass on every fleet PR; block merge or send failures back to retry.
   *The safety layer that turns "fleet wrote code" into "fleet produced MERGEABLE code."*
   (Non-negotiable — else the fleet ships its own bugs at scale.)

2. **Failure-aware retry loop** — **S/M** — capture compile/test/lint/apply/hang errors,
   attach to the work_item, increment attempt_count, requeue with the error context
   appended + caps/escalation. *Fleet repairs its own failed attempts vs returning to a human.*

3. **Deterministic codegen execution contract** — **S/M** — formalize states:
   `success | failed_no_diff | failed_with_diff | timeout_salvaged`; timeouts + heartbeat +
   diff capture + clean finalization. *Salvage proved valuable but must be a controlled
   protocol, not an emergency path.*

4. **Fix Lane-1 local LLM codegen** — **M/L** — make qwen3-coder-30b reliably produce
   applyable patches, strict cloud fallback. *Cloud codex ~15min/task is the cost+latency
   bottleneck; local success is the main unlock.* (Kimi ranked this #1.)

5. **Queue/claim/host health correctness** — **M** — invariant checks: stale/duplicate
   claims, stuck tasks, orphaned worktrees, dead agents, disk-full, degraded-host circuit
   breakers. *Once the planner emits many tasks, silent scheduler/host failures kill autonomy.*

6. **Adaptive scheduler ramp** — **M** — replace `1 dispatch/host/tick` with capacity-aware
   dispatch (slots/load/model-availability/task-size/recent-failure-rate + backpressure).
   *Unlocks throughput without flooding broken lanes.*

7. **PR integration bot w/ merge policy** — **M/L** — auto-merge green low-risk PRs, rebase
   stale branches, serialize conflicts, route risky PRs to review. *Not fully self-building
   until green code lands without a human traffic controller.*

8. **Outcome feedback loop** — **L** — log model/task-type/duration/retries/review/merge
   outcome from ff_interactions → improve decomposition, routing, model selection.
   *The long-term self-improvement loop; depends on the gates above producing clean labels.*

## Dissent (recorded)
Kimi: local Lane-1 first (codex is the bottleneck). Codex: verify-gate first (fast bad PRs
are worse than slow good ones). Chair ruling: verify gate first, Lane-1 immediately after
the execution-contract work.
