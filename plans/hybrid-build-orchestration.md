# Hybrid multi-LLM build orchestration — audit + hardening plan

> 2026-06-15 (autopilot iter-116, Supervisor+Vinny directive). ForgeFleet IS the
> hybrid build orchestrator: cloud CLIs (claude/codex/kimi) **+** local fleet
> LLMs, collision-safe via per-slot sub-agent workspaces + commit-back PRs. Goal:
> another project (HireFlow360) drives its ENTIRE build through `ff`, dogfooding
> HEAVILY and CONCURRENTLY — and ForgeFleet builds ITSELF the same way. This doc =
> what we have, the concurrency gaps, and the work to make it first-class +
> robust under concurrent multi-caller dispatch.

## What exists today (verb inventory)

| Verb | Role | Backend |
|------|------|---------|
| `ff cli <claude\|codex\|kimi> <prompt>` | one-shot headless cloud CLI pass-through (vendor handles auth) | cloud |
| `ff run --backend <b>` | single agent turn-loop on a member (b = local LLM **or** a cloud CLI tag) | hybrid |
| `ff offload <prompt>` | route a heavy task to the warm tool-capable local LLM (V111 router) | local |
| `ff agent dispatch` | coordinator: work_item → idle sub_agent slot → local LLM | local |
| `ff agent fanout N` | N copies of one prompt across the fleet via `fleet_tasks` (capability-tagged) | hybrid |
| `ff agent dispatch-each <backend>` | same prompt on every member with `<backend>`'s CLI | hybrid |
| `ff agent commit-back` | lift a worker's sub-agent-workspace changes → branch + PR on origin/main | — |
| `ff swarm run` | plan → fan out sub-tasks (`fleet_tasks`) → synthesize | hybrid |

**Collision-safety primitives that ARE sound:**
- `fleet_tasks` claim = `FOR UPDATE SKIP LOCKED` (atomic; many workers, no double-claim).
- `AgentCoordinator::claim_slot` = conditional `UPDATE … WHERE status='idle'` + `rows_affected==1` CAS (the loser gets `false`, never a double-claim).
- Per-slot workspaces `~/.forgefleet/sub-agent-{N}/` give each concurrent slot its own checkout (filesystem isolation).
- commit-back branch = `fleet/<worker>/<YYYYmmdd-HHMMSS>-<slug>` (second-granular — collides only on same-second/worker/title).

## Gaps found (this audit)

- **GAP-A — `dispatch_task` does NOT retry on slot-claim contention. [HIGH — fixing first]**
  `agent_coordinator.rs:220`: when `claim_slot` loses the CAS race it returns
  `Err(NoSlot("lost to another dispatcher"))` immediately instead of picking
  another idle slot. Under the concurrent multi-caller load HireFlow (and ff
  building itself) will create, callers spuriously fail **even when other slots
  are free**. Fix: loop pick→claim, re-pick on a lost CAS, bounded by attempts /
  until genuinely no idle slot remains. (`claim_slot` is already correct; only
  the orchestration around it needs the retry.)
- **GAP-B — commit-back branch collision (LOW).** Same-second + same-worker +
  same-title → `git checkout -b` fails. Add a short `work_item_id`/`session_id`
  suffix to the branch for full safety.
- **GAP-C — concurrency observability/limits (MEDIUM, to verify).** Is there a
  per-caller cap / fair-share so one project can't starve the slot pool? Does
  `ff swarm`/`fanout` degrade gracefully on partial sub-task failure (report
  which failed, not all-or-nothing)? Confirm + harden.
- **GAP-D — workspace freshness under concurrency (MEDIUM, to verify).** Confirm
  each dispatch that writes code resets its slot workspace to a clean
  `origin/main` (no leftover state from a prior caller's run) before working, so
  concurrent builds don't cross-contaminate. If not, add a clean-sync step.
- **GAP-E — queue-driven robustness (ONGOING #1).** Every failure HireFlow files
  in `ff-feature-requests.md` is a concurrency/robustness bug; servicing that
  queue is priority #1 over other backlog.

## Dogfood evidence (iter-116)

`ff offload --kind edits` (GAP-A fix) → routed to logan `qwen36-35b-a3b` in 6.2s,
$0 cloud. Output had the correct retry-loop SHAPE but detail bugs (wrong
`self.`/`.await?` receivers, `NoSlot(&str)` vs `Option<String>`) — confirming the
intended hybrid flow: **local generates, cloud reviews/cleans, PR**. Pattern holds
on ff's OWN code.

## First-class "drive an entire build through ff" flow (target)

1. **Plan** — `ff swarm plan` (or a project-supplied task list) decomposes the
   build into independent sub-tasks.
2. **Dispatch** — each sub-task → `ff offload` (local, cheap) or `ff run/agent
   dispatch --backend <cloud>` (frontier, for subtle work), capability-routed,
   each in its own slot workspace. Robust under concurrency (GAP-A).
3. **Commit-back** — `ff agent commit-back` lifts each worker's diff to a unique
   branch + PR; CI gates; review (cloud) merges.
4. **Observe** — `ff tasks list` result tables; partial failures visible/retryable.

## Status — concurrent-DISPATCH path hardened (iters 116-119)

`ff agent dispatch` is now robust under concurrent multi-caller load:
- **GAP-A (#374)** claim-retry on CAS contention — no spurious "lost to another dispatcher".
- **GAP-F (#375)** prefer computers with a healthy tool-capable LLM — no "no active LLM".
- **GAP-G (#376)** retry transient LLM-call failures on another LLM-capable slot.
- **GAP-H (#377)** randomize the idle-slot pick — no lockstep CAS contention ("pool
  contended"). LIVE: 12 concurrent dispatches → 12/12 clean (was 2/10 failing).

All four were found by dogfooding ff on its own dispatch path.

## Code-WRITING flow — deeper gaps (iter-120 audit) — DESIGN NEEDED

The concurrency work above hardens *dispatch* (a chat call → `work_outputs`). The
"drive an entire BUILD through ff" flow (write code in a workspace → commit-back →
PR) is **incompletely wired** and needs design before HireFlow can drive its whole
build through ff. Concrete gaps, traced this iteration:

- **GAP-D1 — fanout/dispatch-each don't target the sub-agent workspace.** Both build
  the member command as `ff run --backend <cli> '<prompt>'` (`agent_cmd.rs:54,134`)
  with **no `--cwd`**. `ff run` then executes in the defer-worker's cwd, NOT the
  per-slot workspace (`~/.forgefleet/sub-agent-{slot}/…`) that `commit-back` later
  reads — so produced code can land where commit-back never looks. Fix: pass
  `--cwd <slot-workspace>/<target-repo>` (and surface which slot/workspace a fanout
  task used, so commit-back can find it).
- **GAP-D2 — no clean-sync before a run.** Nothing resets the workspace to a clean
  `origin/main` (`git fetch && git reset --hard origin/main && git clean -fd`) before
  the LLM edits it. A prior run's leftover changes / stale HEAD contaminate the next
  run on that slot. Fix: a guarded clean-sync step at run start (the slot is claimed
  exclusively, so this is safe; respect the no-`git stash` rule — use reset/clean).
- **GAP-D3 — commit-back branches off the workspace's current HEAD, not fresh main.**
  `commit-back` does `git fetch origin main >/dev/null || true && git checkout -b
  <branch>` (`agent_cmd.rs:265`) — the fetch result is unused; the branch is cut from
  whatever HEAD the workspace is on. If the workspace is stale, the PR diff is huge /
  conflicting. Fix: branch from the freshly-fetched `origin/main` while preserving the
  run's working-tree changes (pairs with GAP-D2's clean base).
- **GAP-B (carried) — commit-back branch uniqueness.** `fleet/<worker>/<ts>-<slug>`
  is second-granular; add a `work_item_id` suffix for full safety.
- **GAP-C (carried) — swarm/fanout partial-failure reporting + per-caller fair-share.**

**Risk note:** GAP-D1-3 touch the build path; design + a real ff→commit-back dogfood
(small forge-fleet change end-to-end) should validate before wiring, rather than
rushing a half-fix into a data/build-critical path unattended.

## Prioritized work items

1. ✅ Concurrent-dispatch hardening (GAP-A/F/G/H) — DONE, iters 116-119.
2. Service `ff-feature-requests.md` as HireFlow files failures (standing #1).
3. **GAP-D1-3 code-writing flow** — design + dogfood-validate, then wire (workspace
   targeting + clean-sync + commit-back-from-main).
4. GAP-C fair-share cap + partial-failure reporting in swarm/fanout.
5. GAP-B commit-back branch uniqueness suffix.
