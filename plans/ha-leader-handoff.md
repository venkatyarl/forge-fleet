# HA Leadership Handoff — Design

*Drafted 2026-06-08. Grounded in the current election code (leader_tick.rs,
leader_state.rs, auto_upgrade.rs, task_runner.rs, ha/pg_failover.rs). Status:
**design for review — do not implement blind.** The crux (§4) is a real
split-brain/data risk.*

## TL;DR — read this first

**The "leader can't self-upgrade" gap is already CLOSED** by the *in-place*
path, not by handoff:

- `auto_upgrade.rs::maybe_self_upgrade_leader` (#141, fixed #143/#145) rebuilds
  the leader's own `forgefleetd`/`ff` in a **detached `setsid` session**
  (`git reset --hard origin/main → cargo build → install + codesign →
  launchctl kickstart / systemctl --user restart`). `set -e` means a failed
  build never installs/restarts. Proven live 2026-06-01; gate now ON.
- #146 made `computers.source_tree_path` self-heal fleet-wide, so the in-place
  path works regardless of *which* node is leader.

So **HA leadership handoff is NOT required to close the self-upgrade gap.** It
is a *separate, larger* goal: zero-downtime leadership during maintenance, and
correct coupling with Postgres/Redis failover. Decide whether that goal is
worth the risk in §4 before building. Recommendation: **ship Phase 1 only**
(cheap, safe observability + a clean voluntary step-down), and treat Phases 2–3
as opt-in, gated, and separately reviewed.

## How leadership works today (ground truth)

- **State:** `fleet_leader_state` — a singleton row (`singleton_key='current'`,
  CHECK-enforced) with `computer_id`, `member_name`, `epoch` (monotonic),
  `heartbeat_at`. (schema.rs:883)
- **Election:** every daemon ticks ~15s (leader_tick.rs:312). Candidate pool =
  `fleet_workers ⋈ computers WHERE election_eligibility <> 'never_leader'`
  (leader_tick.rs:784). Winner = **lowest `election_priority`**, alphabetical
  tie-break (leader_tick.rs:327).
- **Hold:** leader refreshes `heartbeat_at` each tick
  (`pg_refresh_leader_heartbeat`); if the UPDATE matches 0 rows it has been
  taken over → `TickOutcome::Yielded`.
- **Takeover:** a follower claims iff `heartbeat_at` is stale (≥45s,
  `STALE_THRESHOLD_SECS`) and the observed `member_name` is unchanged since its
  last tick, with a strict `epoch` bump. A separate **pulse-silent challenge**
  (≥30s absent from the Redis alive-map) covers a hung-Redis leader (#91).
- **Voluntary yield (exists):** if a more-preferred peer comes alive, the
  current leader calls `pg_yield_leader()` which **DELETEs** the singleton and
  fires `on_lost_leader`. Next tick elects the preferred peer.
- **Wave excludes the leader:** `compose_fleet_upgrade_wave` skips the target
  whose name == leader (task_runner.rs:1489), erroring if no non-leader targets
  remain. This is why the leader needs the in-place path.
- **`going_offline`** (PulseBeatV2 LWT flag) is consumed by election as
  "not alive"; **`is_yielding`** is currently produced but unused.
- **Postgres failover is INDEPENDENT of fleet leadership**
  (ha/pg_failover.rs). The fleet leader promotes a local PG replica only if it
  *hosts one* and the primary is ODOWN. The leader is **not required** to host
  the PG primary — but in the real deployment Taylor tends to host both.

## The goal feature

Let the leader **temporarily** hand leadership to an eligible follower so the
follower's wave can rebuild the old leader, then **fail back** — with no fleet
downtime and no split-brain. (Even though in-place already solves upgrade, a
clean handoff is the foundation for graceful maintenance, kernel reboots, and
DB-primary moves.)

## §4 — The crux / risk: PG-primary coupling

If the current leader **also hosts the Postgres primary** (Taylor today), a
naive "yield fleet leadership, let the follower upgrade me, restart me" is
**unsafe**: restarting Taylor's `forgefleetd` is fine, but if the upgrade or a
reboot disrupts Taylor's Postgres, the whole fleet loses its DB. Handoff that
moves *fleet leadership* without considering *DB primary* buys nothing for the
dangerous case (a Taylor reboot) and adds a second moving part.

**Therefore the ordering for a safe maintenance handoff is:**
1. Confirm a follower (e.g. James) has a **caught-up** PG replica (`lag_bytes`
   ≈ 0 in `database_replicas`).
2. Demote Taylor PG primary → promote James PG replica → primary (and Redis).
3. Repoint the fleet's DB DSN to James (this is the hard part — connection
   strings are currently static per host env; see open question Q2).
4. Only then hand fleet leadership to James.
5. Reverse on fail-back.

Steps 2–3 are real distributed-systems work and the main reason **not** to
build the full feature casually.

## Phased plan

### Phase 1 — Observability + clean voluntary step-down *(safe, ship first)*
- Consume the already-published `going_offline` / add `is_yielding` to the
  election alive-map so a leader that *intends* to step down is treated as
  not-best-candidate **one tick early** (graceful, no 45s stale wait).
- Add `ff fleet leader status` (who/epoch/heartbeat age/candidates by priority)
  and `ff fleet leader step-down` → calls `pg_yield_leader()` + sets
  `is_yielding` on the next beat so the preferred follower takes over cleanly.
- **No PG involvement.** Pure fleet-leadership ergonomics. Low risk, immediately
  useful for operator-driven maintenance. ~1 PR.

### Phase 2 — Maintenance lease (not delete) *(gated, opt-in)*
- Replace the DELETE-on-yield with a **maintenance lease**: add
  `relinquishing_until TIMESTAMPTZ` + `standby_member` to `fleet_leader_state`.
  The old leader stays recorded but election prefers `standby_member` while the
  lease is active; auto-reverts (fail-back) when `relinquishing_until` passes or
  the old leader re-asserts health.
- Drain/refuse `forgefleetd_git`/`ff_git` waves during an active handoff (extend
  the V62 family-singleton) to avoid the wave self-kill race.
- Still **no PG move** — Phase 2 only covers the case where the leader does NOT
  host the PG primary (true for any non-Taylor leader).

### Phase 3 — DB-primary-aware handoff *(highest risk; separate review)*
- Implement the §4 ordering: replica-lag gate → PG/Redis primary move → DSN
  repoint → fleet-leadership move → fail-back. Requires solving dynamic DSN
  repoint (Q2) and a tested promote/demote runbook (`database_replicas` already
  models the roles).
- Only Phase 3 makes a **Taylor** maintenance/reboot truly zero-downtime.

## Open questions (resolve before Phase 2/3)
- **Q1.** Do we actually want zero-downtime Taylor maintenance, or is the
  in-place self-upgrade + a short manual window acceptable? (If the latter,
  stop at Phase 1.)
- **Q2.** How do workers learn the *current* DB DSN after a primary move? Today
  it's static per-host config. Options: a tiny "DSN of record" row workers read
  on connect-fail, NATS/Redis pub of the new primary, or a vip/proxy. This is
  the true blocker for Phase 3.
- **Q3.** Should `election_priority` auto-shift during a maintenance lease, or
  do we keep a separate `standby_member` field? (Separate field is cleaner —
  priority is a stable preference, lease is transient.)
- **Q4.** Interaction with `ha/pg_failover.rs` auto-failover — a planned handoff
  must suppress the reactive failover manager for its duration.

## Recommendation
Build **Phase 1 now** (safe, useful, ~1 PR). Hold Phase 2 behind a gate and a
follow-up review. Do **not** start Phase 3 until Q2 (dynamic DSN) has an agreed
design — that, not leadership mechanics, is the real HA blocker. The
self-upgrade gap that motivated this is already closed in-place; everything
here is about graceful *maintenance*, which is a genuine but lower-urgency goal.
