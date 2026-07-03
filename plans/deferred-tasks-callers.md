# deferred_tasks caller inventory

Purpose: fold-prep inventory for moving `deferred_tasks` into `fleet_tasks`.
Scope checked: literal `deferred_tasks` grep across `crates/` and `src/`, plus
callers of the central `ff_db::pg_*_deferred` API because those are indirect
readers/writers of the same table. Live DB still has the 19-column
`deferred_tasks` shape (`id`, `created_at`, `created_by`, `title`, `kind`,
`payload`, `trigger_type`, `trigger_spec`, `preferred_node`, `required_caps`,
`status`, `attempts`, `max_attempts`, `next_attempt_at`, `claimed_by`,
`claimed_at`, `last_error`, `result`, `completed_at`).

## Direct SQL CRUD call sites

### `ff-db`

- `crates/ff-db/src/queries.rs:3259` - `pg_enqueue_deferred_delayed` inserts new queue rows; `pg_enqueue_deferred` delegates here.
- `crates/ff-db/src/queries.rs:3290` - `pg_list_deferred` selects rows filtered by `status`.
- `crates/ff-db/src/queries.rs:3297` - `pg_list_deferred` selects latest rows across all statuses.
- `crates/ff-db/src/queries.rs:3329` - `pg_deferred_stats` counts rows by status.
- `crates/ff-db/src/queries.rs:3343` - `pg_deferred_stats` groups recent failed rows by `last_error`.
- `crates/ff-db/src/queries.rs:3362` - `pg_deferred_stats` groups recent created rows by title prefix.
- `crates/ff-db/src/queries.rs:3379` - `pg_deferred_stats` finds the oldest pending row.
- `crates/ff-db/src/queries.rs:3408` - `pg_get_deferred` selects one row by id.
- `crates/ff-db/src/queries.rs:3421` - `pg_cancel_deferred` marks pending/dispatchable/failed rows cancelled.
- `crates/ff-db/src/queries.rs:3440` - `pg_force_cancel_deferred` marks pending/dispatchable/failed/running rows cancelled.
- `crates/ff-db/src/queries.rs:3464` - `pg_reap_stale_running` requeues or fails stale running rows.
- `crates/ff-db/src/queries.rs:3489` - `pg_retry_deferred` resets failed/cancelled rows to pending.
- `crates/ff-db/src/queries.rs:3528` - `pg_scheduler_pass` promotes pending `node_online` rows to dispatchable.
- `crates/ff-db/src/queries.rs:3543` - `pg_scheduler_pass` promotes due `at_time` rows to dispatchable.
- `crates/ff-db/src/queries.rs:3562` - `pg_scheduler_pass` promotes due `manual`/`now`/`operator` rows to dispatchable.
- `crates/ff-db/src/queries.rs:3591` - `pg_claim_deferred` selects one dispatchable row with `FOR UPDATE SKIP LOCKED`.
- `crates/ff-db/src/queries.rs:3617` - `pg_claim_deferred` marks the selected row running and records the claim.
- `crates/ff-db/src/queries.rs:3643` - `pg_promote_deferred` manually promotes one pending row to dispatchable.
- `crates/ff-db/src/queries.rs:3667` - `pg_finish_deferred` marks successful rows completed and stores result JSON.
- `crates/ff-db/src/queries.rs:3682` - `pg_finish_deferred` records failure, increments attempts, and either retries pending or fails terminally.

Notes: `crates/ff-db/src/schema.rs` and `crates/ff-db/src/migrations.rs` mention
`deferred_tasks` for DDL/comments, but they do not perform runtime
INSERT/UPDATE/SELECT/DELETE call sites.

### `ff-terminal`

- `crates/ff-terminal/src/self_heal_cmd.rs:130` - direct rollback enqueue into `deferred_tasks` for `ff self-heal revert`; this uses old-looking columns/status (`status='queued'`, `priority`, `meta`) instead of the central enqueue API.
- `crates/ff-terminal/src/status_cmd.rs:197` - `ff status` selects counts by `deferred_tasks.status`.

### `ff-agent`

- `crates/ff-agent/src/ha/backup.rs:624` - backup fan-out coalescing selects in-flight rsync tasks for a peer/kind before enqueueing another.
- `crates/ff-agent/src/ha/backup.rs:877` - backup reaper coalescing selects in-flight reap tasks for a peer/kind before enqueueing another.
- `crates/ff-agent/src/fleet_integrity.rs:320` - active integrity repair checks for an in-flight `revive_member` task before enqueueing another.
- `crates/ff-agent/src/leader_tick.rs:693` - leader revive scan checks for an in-flight `revive_member` task before enqueueing.
- `crates/ff-agent/src/leader_tick.rs:820` - self-heal writer scan checks for an in-flight writer task before enqueueing.
- `crates/ff-agent/src/job_sweeper.rs:157` - stale-job sweeper selects old `running` deferred rows.
- `crates/ff-agent/src/job_sweeper.rs:177` - stale-job sweeper resets selected rows to `pending` or `failed`.
- `crates/ff-agent/src/job_sweeper.rs:205` - retention pass deletes terminal rows older than the configured retention window.

### top-level `src`

- No direct SQL CRUD against `deferred_tasks`; `src/main.rs` wires the production worker/scheduler/reaper through `ff_db`/`ff_agent` APIs.

## Indirect API callers

### `ff-agent`

- `crates/ff-agent/src/training_orchestrator.rs:143` - enqueues a shell deferred task to start a queued training job.
- `crates/ff-agent/src/version_check.rs:238` - lists pending rows for upgrade de-dupe.
- `crates/ff-agent/src/version_check.rs:267` - enqueues per-node upgrade tasks.
- `crates/ff-agent/src/mesh_check.rs:382` - lists pending rows for mesh-check de-dupe.
- `crates/ff-agent/src/mesh_check.rs:399` - enqueues mesh propagation/repair work.
- `crates/ff-agent/src/disk_reconcile.rs:210` - enqueues remote model delete commands through the queue.
- `crates/ff-agent/src/auto_upgrade.rs:381` - enqueues auto-upgrade playbook tasks.
- `crates/ff-agent/src/ha/backup.rs:662` - enqueues delayed/staggered backup rsync tasks.
- `crates/ff-agent/src/ha/backup.rs:909` - enqueues backup reaper tasks.
- `crates/ff-agent/src/external_tools_installer.rs:233` - enqueues external-tool upgrade/install playbooks.
- `crates/ff-agent/src/fleet_integrity.rs:410` - enqueues `revive_member` active repair tasks.
- `crates/ff-agent/src/leader_tick.rs:715` - enqueues leader `revive_member` tasks.
- `crates/ff-agent/src/leader_tick.rs:842` - enqueues self-heal writer tasks.
- `crates/ff-agent/src/autoscaler.rs:919` - enqueues cross-node model load tasks.
- `crates/ff-agent/src/autoscaler.rs:958` - lists recent deferred rows to avoid retry floods.
- `crates/ff-agent/src/autoscaler.rs:1034` - enqueues model reprofile tasks.
- `crates/ff-agent/src/verify_computer.rs:217` - enqueues the `defer_end_to_end` verification task.
- `crates/ff-agent/src/verify_computer.rs:235` - gets that verification task while polling for completion.
- `crates/ff-agent/src/defer_worker.rs:124` - production forgefleetd worker claims rows.
- `crates/ff-agent/src/defer_worker.rs:147` - production worker finishes rows.
- `crates/ff-agent/src/disk_sampler.rs:119` - lists pending manual disk-quota notices for de-dupe.
- `crates/ff-agent/src/disk_sampler.rs:143` - enqueues manual disk-quota notice tasks.
- `crates/ff-agent/src/coverage_guard.rs:583` - enqueues deferred model-load tasks for coverage gaps.

### `ff-terminal`

- `crates/ff-terminal/src/defer_cmd.rs:53` - `ff defer get --watch` polls one row.
- `crates/ff-terminal/src/defer_cmd.rs:159` - `ff defer list` lists rows.
- `crates/ff-terminal/src/defer_cmd.rs:221` - `ff defer add-shell` enqueues a shell row.
- `crates/ff-terminal/src/defer_cmd.rs:258` - `ff defer get` reads one row.
- `crates/ff-terminal/src/defer_cmd.rs:321` - `ff defer cancel --force` force-cancels rows, including stuck `running`.
- `crates/ff-terminal/src/defer_cmd.rs:323` - `ff defer cancel` cancels pending/dispatchable/failed rows.
- `crates/ff-terminal/src/defer_cmd.rs:342` - `ff defer retry` resets failed/cancelled rows.
- `crates/ff-terminal/src/defer_cmd.rs:354` - `ff defer stats` reads queue rollups.
- `crates/ff-terminal/src/model_cmd.rs:555` - model command enqueues a deferred model operation.
- `crates/ff-terminal/src/model_cmd.rs:1489` - model command enqueues a deferred model operation.
- `crates/ff-terminal/src/model_cmd.rs:2439` - model command enqueues a deferred model operation.
- `crates/ff-terminal/src/fleet_cmd.rs:598` - fleet command enqueues a deferred fleet operation.
- `crates/ff-terminal/src/fleet_cmd.rs:1560` - fleet command enqueues a deferred fleet operation.
- `crates/ff-terminal/src/fleet_cmd.rs:1936` - fleet command enqueues a deferred fleet operation.
- `crates/ff-terminal/src/fleet_cmd.rs:3310` - fleet command enqueues a deferred fleet operation.
- `crates/ff-terminal/src/doctor_cmd.rs:190` - doctor command reads deferred queue stats.
- `crates/ff-terminal/src/daemon_cmd.rs:940` - legacy/standalone `ff defer-worker` scheduler promotes trigger-ready rows.
- `crates/ff-terminal/src/daemon_cmd.rs:966` - legacy/standalone worker claims rows.
- `crates/ff-terminal/src/daemon_cmd.rs:987` - legacy/standalone worker finishes rows.

### `ff-gateway`

- `crates/ff-gateway/src/onboard.rs:632` - onboarding API enqueues mesh propagation work.
- `crates/ff-gateway/src/onboard.rs:932` - onboarding/API endpoint lists deferred rows.
- `crates/ff-gateway/src/onboard.rs:988` - onboarding/API endpoint manually promotes one deferred row.

### top-level `src`

- `src/main.rs:843` - forgefleetd starts the production deferred-task worker.
- `src/main.rs:1644` - forgefleetd reaps stale `running` rows through `pg_reap_stale_running`.
- `src/main.rs:3163` - leader-gated forgefleetd scheduler runs `pg_scheduler_pass`.
- `src/main.rs:3471` - forgefleetd starts the stale-job sweeper that also handles `deferred_tasks`.

## CLI verbs that touch `deferred_tasks`

- `ff defer add-shell` (`crates/ff-terminal/src/main.rs:1289`, handler at `crates/ff-terminal/src/defer_cmd.rs:221`) inserts shell tasks with `node_online` or `at_time` triggers.
- `ff defer list` / `ff defer ls` (`crates/ff-terminal/src/main.rs:1278`, handler at `crates/ff-terminal/src/defer_cmd.rs:159`) selects rows by optional status.
- `ff defer get [--watch]` (`crates/ff-terminal/src/main.rs:1314`, handler at `crates/ff-terminal/src/defer_cmd.rs:53` and `:258`) selects one row, optionally polling until terminal.
- `ff defer cancel [--force]` (`crates/ff-terminal/src/main.rs:1324`, handler at `crates/ff-terminal/src/defer_cmd.rs:321` and `:323`) updates status to `cancelled`.
- `ff defer retry` (`crates/ff-terminal/src/main.rs:1331`, handler at `crates/ff-terminal/src/defer_cmd.rs:342`) updates failed/cancelled rows back to pending.
- `ff defer stats` (`crates/ff-terminal/src/main.rs:1334`, handler at `crates/ff-terminal/src/defer_cmd.rs:354`) reads aggregate queue health.
- `ff defer-worker` (`crates/ff-terminal/src/main.rs:548`, worker path at `crates/ff-terminal/src/daemon_cmd.rs:940`, `:966`, `:987`) runs the legacy/standalone scheduler + worker loop.
- `ff daemon --scheduler/--defer-interval` (`crates/ff-terminal/src/main.rs:781`) also uses the legacy daemon path, though comments now steer operators to forgefleetd or `ff defer-worker`.
- `forgefleetd` (`src/main.rs:843`, `:3163`, `:3471`) is the production daemon path for worker, scheduler, and stale-row recovery.

## Fold risk notes

- Single-flight claim: `pg_claim_deferred` depends on a transaction with
  `SELECT ... FOR UPDATE SKIP LOCKED LIMIT 1` followed by an update to
  `status='running'` (`crates/ff-db/src/queries.rs:3591`, `:3617`). The fold must
  preserve this atomic claim behavior and the current precedence for
  `preferred_node`, unassigned work, and the 2-minute fallback for work whose
  preferred node is not claiming.
- Scheduling semantics: pending rows are not claimable until `pg_scheduler_pass`
  promotes them to `dispatchable`. Current trigger behavior is `node_online`
  matching `trigger_spec.node`, `at_time` comparing `trigger_spec.at`, and
  immediate promotion for `manual`/`now`/`operator`; `next_attempt_at` gates
  delayed or backoff retries and must not be overwritten incorrectly.
- Running orphan recovery: there are two recovery paths. `pg_reap_stale_running`
  requeues/fails old `running` rows from forgefleetd, while
  `job_sweeper.rs` also selects old `running` rows and updates/deletes terminal
  rows. The fold must keep the worker max-duration vs stale-sweep threshold
  invariant so healthy long-running shell tasks are not reaped early.
- Legacy writer risk: `crates/ff-terminal/src/self_heal_cmd.rs:130` directly
  inserts a row with `status='queued'`, `priority`, and `meta`, which do not
  match the live 19-column queue shape checked during this inventory. That path
  needs special handling before or during the fold.
- Executor split: production forgefleetd uses `crates/ff-agent/src/defer_worker.rs`,
  while `ff defer-worker`/legacy daemon uses `crates/ff-terminal/src/daemon_cmd.rs`.
  Both claim and finish the same queue rows, so any folded representation must
  keep behavior compatible until the legacy path is removed.
