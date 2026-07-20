# Audit: Ephemeral/live-state data to Redis

**Date:** 2026-07-20  
**Status:** Audit complete; implementation split into ordered, dispatchable changes below.  
**Scope:** Live Postgres write amplification caused by heartbeat, presence, inventory, and
recurrence state. This report is the requested deliverable; it intentionally adds no schema
migration.

## Executive finding

ForgeFleet already has the right two-tier boundary in Pulse v2: a full live beat is published to
Redis, while a persistent-only snapshot is materialized to Postgres
(`crates/ff-pulse/src/materializer.rs:1-15,95-263`). The boundary is not being enforced in the
remaining write paths. A 15-second production sample showed about **992 Postgres updates/second**
across the eight hottest audited tables.

Do **not** move whole control-plane rows to Redis. The safe change is:

1. make Redis authoritative for freshness and currently-observed runtime state;
2. keep identity, desired state, transitions, outcomes, and audit history in Postgres; and
3. stop rewriting durable rows when their durable fields did not change.

The highest-volume offender, `evolution_backlog`, is durable work/audit state and should **not**
move to Redis. Its full-map write pattern must be replaced with dirty-item persistence and a
Postgres `IS DISTINCT FROM` guard. The first implementation batch should fix that write storm and
the Pulse snapshot miss before changing reader authority.

## Live evidence

The following values came from read-only `ff db query` calls against the live database. PostgreSQL
started at `2026-07-17 18:59:00 UTC`; `pg_stat_database.stats_reset` is null, so the cumulative
counters cover at most that server lifetime. Relation sizes include indexes and TOAST.

| Table | Live rows | Updates | HOT updates | Total size |
|---|---:|---:|---:|---:|
| `evolution_backlog` | 13,745 | 86,405,385 | 84,868,383 | 58 MB |
| `computer_software` | 252 | 52,125,594 | 52,095,105 | 4,480 kB |
| `computer_models` | 86 | 19,183,209 | 19,172,978 | 1,608 kB |
| `computer_model_deployments` | 34 | 6,861,340 | 6,856,483 | 1,056 kB |
| `computers` | 18 | 5,950,846 | 20,143 | 624 kB |
| `computer_docker_containers` | 17 | 1,247,564 | 1,245,689 | 328 kB |
| `fabric_pairs` | 5 | 1,212,278 | 1,212,153 | 168 kB |
| `fleet_leader_state` | 1 | 20,352 | 20,352 | 64 kB |

The tables are not currently enormous because autovacuum is working aggressively, but that does
not make the churn free: these eight tables recorded thousands of autovacuum/autoanalyze runs,
generate WAL, dirty buffers, and repeatedly create dead tuples. `computers` is especially costly:
only 0.34% of its updates were HOT, so nearly every freshness update also maintained indexes.

A second snapshot 15.02 seconds later proved the counters are still rising quickly:

| Table | Updates in sample | Approx. updates/s |
|---|---:|---:|
| `evolution_backlog` | 11,272 | 750.5 |
| `computer_software` | 2,089 | 139.1 |
| `computer_models` | 882 | 58.7 |
| `computer_model_deployments` | 357 | 23.8 |
| `computers` | 210 | 14.0 |
| `computer_docker_containers` | 75 | 5.0 |
| `fabric_pairs` | 17 | 1.1 |
| `fleet_leader_state` | 0 | 0.0 |

The short sample is diagnostic, not a capacity forecast. It is sufficient to reject the theory
that these are only old counters.

## Existing architecture and gaps

### Pulse already stores ephemeral state in Redis

`PersistedSnapshot` deliberately omits CPU/GPU utilization, queue depth, tokens/sec, and
per-container metrics (`materializer.rs:95-263`). It is cached at
`pulse:persisted:{computer_name}` for one hour. If both Redis and the durable computer row match,
the fast path should only touch `computers.last_seen_at` (`materializer.rs:483-601`).

The observed per-row update rate proves that delta paths are still executing almost continuously.
Each delta path fans out into unconditional upserts:

- software always rewrites `last_checked_at` (`materializer.rs:1015-1072`);
- model presence always rewrites `last_seen_at` (`materializer.rs:1077-1102`);
- deployments always rewrite `last_status_change` (`materializer.rs:1107-1245`);
- containers always rewrite `last_seen_at` (`materializer.rs:1251-1283`); and
- fabric identity is upserted without a distinctness predicate
  (`crates/ff-pulse/src/fabric_upsert.rs:12-72`).

The root snapshot miss must be measured before assuming a Redis capacity problem. Likely causes to
test are unstable ordering in the snapshot vectors, a field incorrectly classified as persistent,
multiple materializers using different Redis instances/prefixes, Redis GET/SET failures being
silently treated as misses, or persistent Postgres drift forcing `persistent_row_differ`. Add a
reason-labelled fast-path miss counter and Redis error counter; do not guess from aggregate SQL
statistics.

### The evolution backlog is a write-amplification bug, not ephemeral state

`BacklogService` correctly hydrates recurrence counters from Postgres because they must survive a
restart (`crates/ff-evolution/src/backlog.rs:109-130`). However, `persist_all` clones and UPSERTs
every in-memory item, and `persist_item` always replaces the JSON document and `updated_at`
(`backlog.rs:133-161`). With 13,745 rows, any recurring flush becomes a large write storm even when
most items are unchanged.

The checkout contains the persistence helpers but no non-test caller to `persist_all`, while the
live table is changing by roughly 750 updates/s. Before patching the caller, reconcile the deployed
binary/version with this checkout and identify the live call site. This source/runtime discrepancy
is itself an operational finding.

### Leases are not ordinary cache entries

`work_item_leases.heartbeat_at`, `fleet_tasks.last_heartbeat_at`, sub-agent heartbeats, and leader
heartbeats are live fields, but they participate in ownership and takeover decisions. For example,
the work-item dispatcher uses a dedicated Postgres heartbeat pool and retries specifically to
avoid reaping a live build (`crates/ff-agent/src/work_item_dispatch.rs:1171-1220`). Moving these
fields directly to Redis would turn Redis eviction or failover into duplicate-work risk.

They may move only after fencing tokens/epochs are durable in Postgres and every takeover performs
a transactional Postgres compare-and-set. Until then, retain them in Postgres and reduce their
frequency or checkpoint them separately.

## Storage classification

| State | Authority | Rule |
|---|---|---|
| Heartbeat freshness, reachability, live utilization, queue depth | Redis | TTL key; expiry means **unknown/stale**, not durable `offline` |
| Observed running containers, processes, loaded models, runtime health | Redis | Versioned snapshot per computer; replace atomically |
| Software/model presence observation time | Redis | Keep current observation in Redis; persist only appearance/disappearance/version transitions |
| Fabric latency, bandwidth, probe health, last probe | Redis | Keep pair identity/topology in Postgres |
| Computer identity and stable hardware/network inventory | Postgres | Update only with `IS DISTINCT FROM`; keep transition events durable |
| Desired deployment/container/model state | Postgres | Transactional source of truth |
| Job/lease ownership, attempts, terminal outcomes | Postgres | Never infer success or ownership solely from Redis |
| Backlog fingerprints, recurrence counts, priority/status | Postgres | Dirty-item writes only; Redis may cache read views |
| Historical metrics needed for trends | Postgres | Append downsampled points, as `computer_metrics_history` already does |

## Implementation sequence

### 1. Stop the two proven write storms

- Add dirty fingerprint tracking to `BacklogService`; persist only items changed by
  `ingest_report`. Add `WHERE evolution_backlog.item IS DISTINCT FROM EXCLUDED.item OR
  evolution_backlog.durable IS DISTINCT FROM EXCLUDED.durable` to the conflict update.
- Instrument Pulse fast-path misses by reason (`redis_miss`, `redis_error`, `snapshot_changed`,
  `status_changed`, `postgres_drift`) and count rows affected per materialized table.
- Canonicalize every collection in `PersistedSnapshot` before comparison (sort by stable keys such
  as software id, model id, deployment id, and container name).
- Stop swallowing Redis GET/SET errors for observability; continue processing the beat, but emit a
  rate-limited warning and counter.

Acceptance: during a 10-minute unchanged-fleet soak, `evolution_backlog` performs zero updates,
and inventory tables perform zero updates after the first converging beat. Only the explicitly
retained liveness checkpoint may advance.

### 2. Make live node state Redis-authoritative

- Define one versioned `pulse:live:v1:{computer}` snapshot with a TTL greater than two beat
  intervals. Include `observed_at`, publisher boot id, and a monotonic sequence so late beats
  cannot overwrite newer state.
- Convert fleet/dashboard readers of current presence and utilization to Redis-first. During the
  rollout, fall back to the last Postgres checkpoint and label it stale; never silently present a
  checkpoint as live.
- Change `computers.last_seen_at` to a coarse checkpoint (recommended: every 5 minutes with jitter)
  plus immediate writes on online/offline transitions. Durable downtime events remain unchanged.

Acceptance: killing Redis causes live state to become `unknown` while identity, desired state, and
history remain usable; a fresh beat repopulates Redis without a database rebuild.

### 3. Split observed state from durable inventory

- For software, models, deployments, containers, and fabric, add distinctness guards so stable
  fields are only persisted on transitions. Do not update `last_checked_at`, `last_seen_at`, or
  `last_status_change` merely because another identical beat arrived.
- Serve current `present`/`running`/`healthy` from the Redis snapshot. Preserve durable first-seen,
  version-change, desired-state, stopped/removed transition, and audit fields in Postgres.
- Shadow-read Redis and Postgres for one release and report divergence before removing legacy live
  reads.

Acceptance: unchanged heartbeats create no inventory row versions; start/stop, install/remove,
version change, and topology change each create exactly one durable transition.

### 4. Consider lease freshness only after durable fencing exists

- Keep durable lease identity, owner, attempt, expiry policy, and terminal release in Postgres.
- If heartbeat load becomes material, add Redis lease-liveness keys keyed by lease id and fencing
  epoch, but require a Postgres compare-and-set before assignment or takeover.
- Run Redis/Postgres dual reads through failover, clock-skew, delayed-heartbeat, and eviction tests
  before removing Postgres heartbeat writes.

This phase is lower priority: the live counters show inventory/backlog churn is orders of magnitude
larger, and incorrect lease migration has a much higher correctness cost.

## Rollout and rollback guardrails

- Every field must have one documented authority; avoid bidirectional reconciliation.
- Redis snapshots need TTL, schema version, boot id, sequence, and maximum accepted clock skew.
- Use feature flags independently for dual-write, shadow-read, Redis-primary read, and Postgres
  write suppression. Rollback is switching reads back to Postgres checkpoints, not replaying Redis
  into durable rows.
- Alert on Redis eviction, snapshot age, snapshot write failures, fast-path miss reason, Postgres
  rows affected per beat, and Redis/Postgres shadow divergence.
- After write suppression is proven, use normal vacuum/reindex maintenance based on measured bloat;
  do not schedule destructive table rewrites as part of the functional rollout.

## Migration decision

No migration belongs in this audit diff. The first two implementation tasks change write behavior
and add telemetry without changing schema. If later cleanup removes or renames legacy freshness
columns, that must be a separate forward-only migration using the next unused integer version; no
existing migration may be edited.

## Council note

The required `ff council --members codex,kimi` review was run. Codex agreed on Redis for “now” and
Postgres for identity/intent/outcomes, with phased dual-write, shadow reads, TTL semantics, and
fencing. Kimi was unavailable on this node (`kimi` executable missing), so there was no dissenting
second response to incorporate.
