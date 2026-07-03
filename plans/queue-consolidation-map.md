# Queue Consolidation Map (2026-07-03)

Canonical decision: **`fleet_tasks` is the one execution queue.** `work_items`
stays as the PM/planning layer (decompose, prioritize, assign, lease, merge),
not a second execution queue.

Live counts below came from `ff db query` against the shared Postgres on
2026-07-03.

| Table | Live rows | Purpose | Status |
|-------|-----------|---------|--------|
| `fleet_tasks` | 26,522 | Canonical runnable execution queue for fleet-dispatched work. | canonical |
| `work_items` | 972 | PM/planning layer: scoped work records, readiness, assignment, leases, merge flow. | PM-layer |
| `deferred_tasks` | 25,161 | Delayed / condition-gated work (time-based, retryable, preferred-node/offline cases). | to-fold |
| `research_subtasks` | 64 | Persisted per-question research fanout state/results for `ff research`. | to-fold |
| `fleet_self_heal_queue` | 0 | Leader single-flight queue for self-heal intents keyed by bug signature. | to-fold |

Dropped/dead tables:

- `fleet_work_items` and `fleet_work_batches` are already dropped by V153 and
  are absent live.
- `work_item_fleet_tasks` is the old bridge table; V155 drops it. The branch
  already carries that migration, but the shared live DB is still on V154, so
  the table still exists there at **0 rows** until V155 is applied.

## Fold order

Use the compat-view migration playbook for every fold: add any missing
canonical columns to `fleet_tasks`, dual-write/backfill, replace the legacy
table with a compat view, cut callers, then remove the compat view in a later
cleanup migration.

1. **Drop dead tables first.** `fleet_work_items` / `fleet_work_batches` are
   already gone; apply V155 to remove the empty `work_item_fleet_tasks` bridge.
2. **Fold `fleet_self_heal_queue`.** Lowest risk: 0 live rows and narrow leader
   ownership.
3. **Fold `research_subtasks`.** Small live footprint (64 rows), isolated
   subsystem, straightforward backfill into `fleet_tasks`.
4. **Fold `deferred_tasks` last.** Biggest operational risk and data mass
   (25,161 live rows, ~25k), so do it only after the smaller queues prove the
   migration pattern.

Each fold should be its **own forward-only migration series**. Do not combine
multiple queue folds into one migration.
