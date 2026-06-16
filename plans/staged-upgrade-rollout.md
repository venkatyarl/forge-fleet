# Staged Upgrade Rollout + Auto-Halt — Design

*Drafted 2026-06-16. Grounded in the current wave dispatcher
(`task_runner.rs::compose_fleet_upgrade_wave`), the V52 two-phase build/restart,
the V62 family-wide wave singleton, and the unwired rollback recommender
(`ff_evolution::VerificationModel::should_rollback`). Status: **design for
review — do not implement blind.** This subsystem has a documented self-kill
history (`feedback_wave_dispatcher_self_kill_race`, `cross_family_wave_self_kill`);
a half-built halt path is worse than none.*

Closes **PROD_READINESS GAP item 26**: "one bad build can roll the whole fleet
before failures surface."

## TL;DR

Today a fleet upgrade composes **every** target into priority-ordered waves and
inserts them **all at once**. Priority only controls *order*, not *gating*:
nothing stops wave N+1 from running after wave N **failed**. A bad build
therefore rolls all 14 non-leader hosts before anyone notices. The rollback
recommender exists but is wired to the *evolution* loop, not the *upgrade wave*.

The fix is two coupled mechanisms:
1. **Stage gate** — withhold the next stage until the current stage's tasks have
   *completed successfully* (not just "dispatched").
2. **Auto-halt** — when a stage's failure rate crosses a threshold, stop
   progressing, mark the rollout `halted`, alert, and surface the rollback
   recommendation.

Recommendation: **ship the gate + halt for a single canary stage first**
(highest value, smallest surface), behind a `fleet_secrets.staged_rollout_mode`
gate, before building full percentage staging.

## Ground truth — how the wave works today

- **Composer:** `compose_fleet_upgrade_wave(pg, software_id, fanout, leader_id,
  dry_run)` (`task_runner.rs:1561`). Resolves a per-target playbook for every
  member with the software (`resolve_upgrade_plans_with_suffix`, build-only
  variant for Phase-1), excludes the leader, chunks the rest into waves of
  `fanout`, and inserts `PlannedTask` rows into `fleet_tasks`.
- **"Waves" today are priority tiers, not gates.** Wave 0 gets the highest
  priority and descends by 3 per wave (`task_runner.rs:1547-1552`); **all tasks
  are inserted in one call.** Workers drain them fastest-first. There is no
  barrier that blocks wave N+1 on wave N's *success*.
- **Two-phase per target (V52):** Phase-1 = parallel build/install
  (`requires_capability=[]`, any worker), Phase-2 = serialized restart
  (`requires_capability=[leader]`), coupled by `parent_task_id` +
  `wait_for_siblings` (`task_runner.rs:262`, `:802`). Prevents a worker from
  restarting a sibling mid-build.
- **V62 family singleton:** refuses a new wave while any wave for the same
  software *family* (`ff_git`/`forgefleetd_git`/`forgefleet`) is pending/running
  (`task_runner.rs:1604-1640`). One wave per family in flight at a time.
- **Task outcome:** `fleet_tasks.status` ∈ `pending|running|completed|failed`
  (`task_runner.rs:470,493` set `failed`; deps gate on `status='completed'`,
  `:340`). This is the signal a stage gate / halt would read.
- **Leader excluded** from the wave (self-restart suicide); upgraded in place by
  `auto_upgrade::maybe_self_upgrade_leader` or by hand.
- **Rollback recommender exists but is unwired to upgrades:**
  `VerificationModel::should_rollback` (`verification.rs:168`) only feeds the
  evolution loop. `version_mgmt.rs` sketches a canary→stable promotion narrative
  but there is **no dispatcher** that enforces it.

## The goal

Replace "dispatch all waves at once" with a **gated progression**:

    canary (1 host) → stage 1 (~10%) → stage 2 (~50%) → stage 3 (100%)

Advance to the next stage **only** when the current stage's tasks all reached
`completed`. If a stage's failure rate exceeds a threshold, **halt**: withhold
all remaining stages, record the rollout as `halted`, alert (Telegram), and
attach the rollback recommendation.

## Design

### Schema (V133) — `upgrade_rollouts`
One row per rollout; stage tasks reference it.

    upgrade_rollouts(
      id UUID PK, software_id TEXT, started_by TEXT,
      stages JSONB,            -- ordered list of {stage_idx, target_names[]}
      current_stage INT,
      status TEXT,             -- in_progress | halted | completed | aborted
      failure_threshold_pct INT,   -- e.g. 25; canary uses ">=1 fail"
      halted_reason TEXT, created_at, updated_at)

`fleet_tasks` gains a nullable `rollout_id UUID` + `rollout_stage INT` so the
gate can count a stage's outcomes. (Add via the same migration; the columns are
inert for non-rollout tasks.)

### Stage gate — a leader-gated tick (preferred over task-deps)
A new leader tick `upgrade_rollout_tick` (~30-60s), gated by
`fleet_secrets.staged_rollout_mode` (off|dry-run|active, **default off**):

1. For each `in_progress` rollout, look at `current_stage`.
2. Count that stage's `fleet_tasks` by status (`completed` / `failed` /
   still-running).
3. **If any still running** → do nothing (stage in flight).
4. **If all terminal:** compute `failed / total`.
   - `> failure_threshold` (canary: `failed >= 1`) → set `status='halted'`,
     `halted_reason`, fire `upgrade_rollout_halted` alert, surface
     `should_rollback`. **Withhold all later stages.**
   - else → `current_stage += 1`. If more stages remain, **compose ONLY that
     stage's targets** (reuse `compose_fleet_upgrade_wave` semantics but for the
     stage's `target_names`, tagged with `rollout_id`/`rollout_stage`). If no
     stages remain → `status='completed'`.

Why a tick, not cross-wave task dependencies: the halt *decision* (failure-rate
threshold + alert + rollback) is stateful and policy-driven — it belongs in one
leader-elected place, not encoded as 14 task dependency edges. The tick also
composes the next stage lazily, so a halted rollout simply never creates the
remaining tasks (nothing to cancel).

### Entry point
`ff fleet upgrade <software> --staged [--canary 1 --stages 10,50,100]` builds the
`upgrade_rollouts` row + composes **only stage 0 (canary)**; the tick drives the
rest. Without `--staged`, today's all-at-once behaviour is unchanged (back-comp).

### Definition of "failure" (the crux)
A Phase-1/Phase-2 task that exits non-zero is the obvious signal. But the
*dangerous* failure is "build succeeded, daemon restarted, but the new binary is
unhealthy" — which a task-level `completed` would miss. Two options:
- **v1 (task-only):** halt on `fleet_tasks` failure. Simple, catches build/
  install/restart breakage. Does NOT catch a cleanly-installed-but-crashlooping
  binary.
- **v2 (health-aware):** after a stage's restarts, the tick waits one beat
  interval and checks each stage host's `heartbeat_v2 build_sha` actually flipped
  to the target AND the host is pulse-alive. A host that took the upgrade but
  stopped beating = failure. This is the real protection and reuses the
  materializer/heartbeat signal. Recommend v2 for the canary stage at minimum.

## Interaction / risk

- **Wave self-kill history.** Staging must compose **one stage at a time** so the
  family singleton (V62) still sees only one wave in flight. Composing all stages
  up front would reintroduce cross-wave races. The lazy per-stage compose in the
  tick preserves the singleton invariant.
- **Two-phase barrier** is per-target and unchanged; a stage is just a subset of
  targets. The gate waits for *both* phases (the parent restart task) to reach a
  terminal state before counting.
- **Leader** is still out-of-band (in-place self-upgrade). A staged rollout never
  includes the leader; document that the canary is a *follower*, so leader-only
  breakage is still caught by `maybe_self_upgrade_leader`'s `set -e` build, not
  here.
- **Don't auto-rollback.** Per the fleet "updates never auto-applied" rule, halt
  + alert + *recommend* rollback. The operator (or a future, separately-reviewed
  rollback dispatcher) executes it. Wiring an automatic binary downgrade is a
  separate, higher-risk design.

## Phased plan

1. **Phase 1 (ship first):** schema V133 + `upgrade_rollout_tick` (gate + halt) +
   `--staged` with a **single canary stage** (1 follower) then "the rest" as one
   stage. v2 health-aware failure check on the canary. Gate default off. This
   alone delivers "a bad build can't pass the canary." ~1 PR, leader-gated,
   reversible (gate off = today's behaviour).
2. **Phase 2 (gated, opt-in):** real percentage staging (10/50/100) + per-stage
   thresholds. Pure extension of the stage list; no new mechanism.
3. **Phase 3 (separate review):** wire an actual rollback dispatcher to the halt
   path (compose a wave back to the previous `installed_version`). Highest risk;
   needs the previous-good-SHA tracked per host and a tested downgrade playbook.

## Open questions
- **Q1.** Canary host selection — lowest `election_priority` follower? A
  designated `canary=true` flag on `computers`? (Recommend a flag so the operator
  picks a low-stakes box.)
- **Q2.** Health-check window — how many beat intervals to wait before declaring a
  stage healthy? (Start 2× the 15s beat = 30s, tunable via the rollout row.)
- **Q3.** Threshold semantics for tiny stages — a 1-host canary halts on 1 fail
  (100%); a 50% stage should tolerate 1 flaky 32GB box. Per-stage absolute +
  percentage floor.

## Recommendation
Build **Phase 1 only** (canary gate + health-aware halt), behind
`staged_rollout_mode` (default off), with `--staged` opt-in so the all-at-once
path is untouched. Hold Phases 2–3 for separate review. The single highest-value
outcome — *a bad build is caught on one host instead of fourteen* — lives
entirely in Phase 1.
