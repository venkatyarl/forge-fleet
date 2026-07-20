# Audit: Move Ephemeral / Live-State Data to Redis (2026-07-19)

Status: **idea / pre-dispatch** — findings and phased plan only; no code changes yet.

Migrated from DR session backlog 2026-07-17.

## Executive summary

Several Postgres tables hold live, high-churn, or TTL-bound state that is a
natural fit for Redis. Moving them would reduce row bloat, dead-tuple churn,
and write load on Postgres. The codebase already has a mature Redis layer in
`ff-pulse` for Pulse heartbeats, so the infrastructure is in place.

Live table sizes below came from `ff db query` against the shared Postgres on
2026-07-19. Row-count snapshots were taken at roughly the same time.

## Existing Redis context

| Component | Redis usage |
|-----------|-------------|
| `ff-pulse` | Pulse v2 beats (`pulse:computer:{name}`, 45s TTL), pub/sub (`pulse:events`, `pulse:updates`), materializer snapshot cache (`pulse:persisted:{name}`, 1h TTL). |
| `ff-brain` | Per-thread stacks (`brain:stack:{user}:{thread}`) and per-project backlogs (`brain:backlog:{user}:{project}`). |
| `ff-gateway` | Routing cache invalidation (`routing:invalidate`). |
| `ff-agent` | Fleet event pub/sub (`fleet:*`). |

Conventions observed:

- `redis` crate v0.27 with `tokio-comp` + `connection-manager`.
- `ConnectionManager` for shared/long-lived clients; `MultiplexedConnection` for one-shot publishes.
- JSON serialization everywhere; HMAC-signed Pulse beats when configured.
- Config precedence: `FORGEFLEET_REDIS_URL` → `fleet.toml [redis] url` → `redis://127.0.0.1:56379`.
- Docker Compose Redis is `redis:7-alpine` on port `56379`, `maxmemory 96mb`, `allkeys-lru`, AOF everysec.
- CI (`cargo test --lib`) has **no Redis service**; any new Redis-backed code must degrade gracefully or be skipped in unit tests.

## Candidate tables / columns

### Tier 1 — strongest live-state candidates

| # | Table / columns | Size | Rows (active) | Why Redis fits | Blast radius |
|---|-----------------|------|---------------|----------------|--------------|
| 1 | `work_item_leases` | 464 kB | 1,235 (53 active) | Distributed lock with TTL, heartbeat-bumped, high dead-tuple churn. Partial-unique active index is exactly a Redis lock. | **High** — core scheduling lock; takeover + reaper logic must move too. |
| 2 | `fleet_leader_state` | 64 kB | 1 singleton | Leader-election heartbeat updated every ~15s. Redis `SET NX/XX` with TTL is the textbook replacement. | **Medium-High** — ~15 read sites across gateway/terminal/brain. |
| 3 | `fleet_tasks` live columns | 4.8 MB | 3,153 (106 active) | Active execution state flips constantly (`status`, `claimed_by_*`, `last_heartbeat_at`, `progress_*`). Terminal history already pruned daily. | **Very High** — central ledger. Move only live execution state, keep durable history in Postgres. |
| 4 | `sub_agents` live columns | 128 kB | 68 (12 active) | Slot state (`status`, `current_work_item_id`, `last_heartbeat_at`) is a live lease pointer. | **Medium** — small data set, but joins with `work_items` need redesign. |
| 5 | `fleet_mesh_status` | 176 kB | 280 | Pairwise mesh liveness probes with TTL semantics. Read rarely (doctor/status). | **Low-Medium** — ideal Redis hash-per-pair with TTL. |
| 6 | `fleet_backend_health` | 80 kB | 34 (5 open) | Circuit-breaker state with explicit TTL (`breaker_open_until`), rolling 5-min windows. | **Low-Medium** — callers localized to `circuit_breaker.rs`. |
| 7 | `fleet_provider_usage` | 80 kB | 13 | Live headroom row overwritten per (computer, provider) on every outcome. | **Low-Medium** — natural Redis string/hash with TTL. |
| 8 | `host_circuit_status` | 64 kB | 0 currently | Host quarantine state with expiry (`opens_until`). | **Low** — only two functions in `circuit_breaker.rs`. |

### Tier 2 — metrics / time-series / probes

| # | Table / columns | Size | Rows | Why Redis fits | Blast radius |
|---|-----------------|------|------|----------------|--------------|
| 9 | `computer_metrics_history` | 11 MB | 54,631 | Downsampled per-computer metrics written every 60s from Redis Pulse beats; 90-day PG retention. Source of truth is already Redis. | **Medium** — dashboard/alert queries read Postgres today; Redis Streams or TSDB needed. |
| 10 | `deployment_metrics_scrapes` | 32 kB | 0 currently | Per-deployment `/metrics` scrapes every 30s, 24h PG retention. No other readers found. | **Low-Medium** — could become Redis time-series or in-memory. |
| 11 | `task_liveness_probes` | 64 kB | 0 currently | High-frequency OS probes per running task; only latest value matters to watchdog. | **Low** — two call sites; Redis hash per task with short TTL. |

### Tier 3 — queues / outbox / transient state

| # | Table / columns | Size | Rows | Why Redis fits | Blast radius |
|---|-----------------|------|------|----------------|--------------|
| 12 | `task_notification_outbox` | 2.4 MB | 6,563 unprocessed | Transactional outbox for `fleet_tasks` lifecycle events. Meant to be processed and deleted. | **Medium** — **no consumer found in the codebase**; backlog must be resolved first. Redis Streams is natural if/when a relay exists. |
| 13 | `work_queue` | 80 kB | 12 | General durable work queue with claim/ack semantics. | **Low** — isolated module, but durability requirements must be confirmed. |

### Tier 4 — membership tables with ephemeral columns

| # | Table / columns | Size | Rows | Why Redis fits | Blast radius |
|---|-----------------|------|------|----------------|--------------|
| 14 | `computers` liveness cols | 608 kB | 18 | `last_seen_at`, `status`, `offline_since` are derived from Redis Pulse beats. Durable enrollment data should stay in Postgres. | **Medium** — many readers join/filter on liveness; merge PG registry + Redis liveness. |
| 15 | `fleet_workers` `status`, `updated_at` | 136 kB | 18 | Worker online/offline status; could source from Pulse Redis. | **Low-Medium** |

### Tier 5 — borderline

| # | Table / columns | Size | Rows | Notes |
|---|-----------------|------|------|-------|
| 16 | `agent_memory` | 176 kB | 2 (24 dead tuples) | Scratchpad working memory. High update ratio. Could move live blocks to Redis, keep `agent_memory_evictions` audit in Postgres. |
| 17 | `agent_memory_evictions` | 736 kB | 592 | Append-only audit trail. **Keep in Postgres** (or archive to object storage later). |

### Not recommended for Redis

- `work_item_merge_queue` — serialized CI-gated merge queue; needs durability and ordering.
- `work_item_worktrees` — filesystem paths and git branch state; durable project metadata.
- `work_items` — PM work items; operator-meaningful durable records.
- `error_events` — new audit table for model runtime failures; keep for debugging.
- `brain_*`, `ff_interactions`, `evolution_backlog` — knowledge graph, training corpus, long-term backlog.

## Prioritized implementation phases

### Phase 0 — operational cleanup (must happen first)

1. **`task_notification_outbox` has 6,563 unprocessed rows and no visible consumer.**
   - Decide if the relay is planned; if so, design it (Redis Streams is the natural target).
   - If not planned, drain/delete stale rows and consider removing the trigger until the relay is built.
2. Confirm durability requirements for `work_queue` before moving it.
3. Confirm retention / query requirements for `computer_metrics_history` (Redis Streams vs. TSDB vs. keep in Postgres with reduced retention).

### Phase 1 — quick wins (low blast radius, immediate churn reduction)

Move these first; each is a small, isolated change:

1. `host_circuit_status` → Redis key per `(worker_name, failure_category)` with TTL.
2. `fleet_mesh_status` → Redis hash per `src:dst` pair with TTL, or pub/sub stream.
3. `task_liveness_probes` → Redis hash per `task_id` with short TTL.
4. `deployment_metrics_scrapes` → Redis time-series or in-memory buffer.
5. `fleet_provider_usage` → Redis string/hash per `(computer_id, provider)` with TTL.

### Phase 2 — natural Redis patterns with medium effort

1. `fleet_backend_health` — replace SQL rolling-window logic with Redis sorted-set sliding counters + TTL.
2. `fleet_leader_state` — replace singleton PG row with Redis `SET NX/XX` + TTL.
3. `sub_agents` live columns — keep durable slot enrollment in Postgres, move `status`, `current_work_item_id`, heartbeat to Redis.
4. `computers` / `fleet_workers` liveness columns — source from Pulse Redis instead of materializing to PG.

### Phase 3 — architectural moves

1. **Active `fleet_tasks` execution state in Redis**, keep terminal/durable history in Postgres.
   - Live: `status`, `claimed_by_computer_id`, `claimed_at`, `started_at`, `last_heartbeat_at`, `progress_pct`, `progress_message`, `result`, `error`, `handoff_*`.
   - History/archive: completed/failed terminal rows stay in Postgres for audit and CLI history.
2. **`work_item_leases` to Redis.**
   - This is the biggest payoff but highest risk. Needs Redis-backed lease acquisition, heartbeats, takeover, and stale-lease reaping.
   - Consider as part of a larger scheduler Redis refactor rather than a standalone migration.

## Testing / safety notes

- Any new Redis-backed library code must pass `cargo test --lib` without a live Redis (CI has none). Use graceful degradation or `#[ignore]` integration tests.
- For each migrated table, add a forward-only migration that either:
  - drops the PG table after dual-write/cutover, or
  - turns it into a compatibility view while callers are migrated.
- Do **not** edit existing migration consts; add one new const + register the next integer version per migration.
- DB tests must early-return when `FORGEFLEET_POSTGRES_URL` and `FORGEFLEET_DATABASE_URL` are unset.

## Files to touch (by phase)

| Phase | Files |
|-------|-------|
| 1 | `crates/ff-agent/src/circuit_breaker.rs`, `crates/ff-db/src/queries.rs`, `crates/ff-agent/src/task_probe.rs`, `crates/ff-agent/src/watchdog.rs`, `crates/ff-agent/src/metrics_scraper.rs`, `crates/ff-terminal/src/doctor_cmd.rs` |
| 2 | `crates/ff-agent/src/leader_tick.rs`, `crates/ff-db/src/leader_state.rs`, `crates/ff-agent/src/agent_coordinator.rs`, `crates/ff-pulse/src/materializer.rs` |
| 3 | `crates/ff-agent/src/work_item_dispatch.rs`, `crates/ff-agent/src/work_item_scheduler.rs`, `crates/ff-agent/src/lease_takeover.rs`, `crates/ff-agent/src/ha/mod.rs`, `crates/ff-db/src/queries.rs`, `crates/ff-agent/src/task_runner.rs`, `crates/ff-agent/src/scheduler_tick.rs`, `crates/ff-gateway/src/server.rs`, `crates/ff-terminal/src/tasks_cmd.rs` |

## Recommended next step

Flag Phase 0 + Phase 1 for dispatch first. They are small, safe, and will immediately reduce write churn while validating the Redis patterns before tackling the scheduler and task ledger in Phase 3.
