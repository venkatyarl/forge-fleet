# forgefleetd Tick Registry + Scheduler (ff council: codex + kimi, 2026-07-01)

## Verdict
The "30 independent `tokio::time::interval` loops in one daemon" pattern is no
longer operationally defensible. The problem is NOT monolith-vs-subprocess — it's
uncoordinated scheduling, ~15 redundant per-tick leader checks, invisible
failures, and no backpressure. **Keep one forgefleetd binary; replace the ad-hoc
loops with an in-process tick registry/scheduler + supervision.** Use Postgres
LISTEN/NOTIFY selectively for hot queues (polling stays as reconciliation). Do
NOT split into subprocesses yet; do NOT do a wholesale event-driven rewrite.

## Top improvements (ranked)
1. **Unified tick registry + ONE cached leader gate** (S/M) — register ticks by
   name/interval/scope(leader|per-host)/gate-secret/jitter/timeout/priority/
   max-concurrency; replace per-loop `am_i_leader()` queries with one
   scheduler-owned cached leadership state (kills ~15 DB round-trips/cycle).
2. **Per-tick observability** (M) — track last_started/last_success/last_error/
   duration/lag/next_run/in_flight/failure_count → CLI (`ff daemon ticks`) +
   `daemon_ticks` table + alert on stale/failing/slow/spinning.
3. **Panic-isolating supervision** (M) — wrap each tick so panics/exits are
   visible; restart/backoff/disable-after-N-failures. Silent task death becomes
   impossible.
4. **Hybrid LISTEN/NOTIFY for hot pollers** (M) — work_item dispatch, deferred
   tasks, lease/merge events; keep periodic polling as correctness reconciliation.

## Migration path (incremental, no big-bang)
1. Add `TickRegistry`/`Scheduler` (new module in ff-agent). Wrap 2-3 low-risk
   PER-HOST ticks first: version-check, disk-sampler, backend-detector.
2. One scheduler-owned leadership refresh/cache; leader-gated ticks read that
   state instead of querying independently.
3. Migrate leader-gated ticks into the registry — behavior unchanged except
   jitter + timeout + skip-overlap protection.
4. Add metrics/health reporting + alerts (stale/failing/slow/spinning).
5. Bounded concurrency/backpressure by class: scheduler, cortex, deployment,
   maintenance.
6. LISTEN/NOTIFY only for the hot queues; scheduled reconciliation stays.
7. Revisit subprocess isolation later, only for domains needing OS-level
   isolation/independent scaling (likely cortex indexing/embedding,
   evolution/self-heal, or gateway/API).

## Phase 1 (building now, via `ff pm decompose` → fleet)
TickRegistry + Scheduler skeleton + shared leader cache + wrap 3 per-host ticks.
