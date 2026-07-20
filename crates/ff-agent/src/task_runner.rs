//! Distributed shell-task worker for `fleet_tasks` (V44).
//!
//! Every daemon runs a [`TaskRunner`] tick. It looks for `pending`
//! rows in `fleet_tasks` whose `requires_capability` set is satisfied
//! by this computer and either has no preferred computer or names this
//! one. It atomically claims the row via `FOR UPDATE SKIP LOCKED`,
//! runs the shell payload, heartbeats every 30s while running, and
//! marks `completed` / `failed`.
//!
//! The leader runs an additional handoff watchdog that finds rows
//! whose claimed worker has gone stale (`last_heartbeat_at` older than
//! 120s) and re-puts them on the queue with `handoff_count` bumped.
//! After [`MAX_HANDOFFS`] re-tries the row is marked permanently
//! `failed` so we don't loop forever on a poison task.
//!
//! Today only `task_type = "shell"` is dispatched. The payload shape:
//!
//! ```json
//! { "command": "echo hi", "shell": "/bin/bash" }
//! ```
//!
//! `shell` is optional and defaults to `/bin/bash` (or `sh` on
//! systems where bash is absent). Stdout + stderr are captured into
//! `result.stdout` / `result.stderr`; exit code into `result.exit`.

use std::collections::HashSet;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// How long a `running` row may sit without a heartbeat before the
/// leader re-queues it.
const STUCK_AFTER_SECS: i64 = 120;
/// How often the worker bumps `last_heartbeat_at` while a task runs.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);
/// Max times a row can be handed off before we give up and fail it.
const MAX_HANDOFFS: i32 = 3;
/// Max wall-clock duration for a single shell task. Closes the
/// heartbeat-fresh-but-stuck-forever class of bug (worker's heartbeat
/// task keeps firing while the actual SSH/cargo child is wedged).
///
/// 10 min default — covers a cold cargo build (~3-5 min observed) with
/// 2x margin. Earlier 30 min default was too lenient: when priya hung,
/// the wave's barrier held Phase-2 for 15+ min waiting for the timeout.
/// Per-task override via `payload.max_duration_secs` for known-slow
/// jobs (e.g. model downloads).
const MAX_TASK_DURATION: Duration = Duration::from_secs(10 * 60);
/// Maximum number of worker ticks before returning, to prevent
/// over-processing in a long-running task runner.
const MAX_ITERATIONS: usize = 100;

/// GAP-C fair-share cap: the max number of `running` tasks a single caller
/// (one `parent_task_id` — a swarm/fanout/build invocation) may hold
/// fleet-wide *while another caller has pending work waiting*. Keeps one
/// large swarm from monopolizing every worker slot and starving a second
/// caller. The cap is work-conserving: with no contention a caller still
/// uses the whole fleet (see the claim query's `NOT EXISTS (other caller)`
/// branch). Default 6; override via `FF_FAIR_SHARE_MAX_RUNNING`.
const DEFAULT_FAIR_SHARE_MAX_RUNNING: i64 = 6;

/// Resolve the fair-share cap, honoring the `FF_FAIR_SHARE_MAX_RUNNING`
/// env override (must be a positive integer; anything else falls back to
/// the default).
fn fair_share_max_running() -> i64 {
    std::env::var("FF_FAIR_SHARE_MAX_RUNNING")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_FAIR_SHARE_MAX_RUNNING)
}

/// Read a port-shaped row from `fleet_secrets`. Panics loudly if the
/// row is missing — better than silently falling back to a hardcoded
/// default and drifting away from operational truth. Operators can
/// always re-seed via the V50 migration or `ff secrets set`.
async fn read_port_secret(pg: &PgPool, key: &str) -> Result<u16, sqlx::Error> {
    let val: Option<String> = sqlx::query_scalar("SELECT value FROM fleet_secrets WHERE key = $1")
        .bind(key)
        .fetch_optional(pg)
        .await?;
    let s = val.ok_or_else(|| {
        sqlx::Error::Configuration(
            format!(
                "fleet_secrets is missing required key '{key}' — \
                 run V50 migration or `ff secrets set {key} <port>`"
            )
            .into(),
        )
    })?;
    s.parse::<u16>().map_err(|e| {
        sqlx::Error::Configuration(
            format!("fleet_secrets['{key}']='{s}' is not a u16 port: {e}").into(),
        )
    })
}

/// Errors that can occur while running the task runner.
#[derive(Debug, thiserror::Error)]
pub enum TaskRunnerError {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("task payload missing required field: {0}")]
    BadPayload(String),
    #[error("unsupported task_type: {0}")]
    UnsupportedType(String),
    #[error("task exceeded max duration of {0}s")]
    Timeout(u64),
}

/// Per-computer worker that polls `fleet_tasks` and runs shell payloads.
#[derive(Clone)]
pub struct TaskRunner {
    pg: PgPool,
    my_computer_id: uuid::Uuid,
    my_name: String,
    /// Capabilities advertised by this computer. The runner will only
    /// claim a task whose `requires_capability` ⊆ this set.
    my_capabilities: Arc<HashSet<String>>,
    /// Per-computer environment exposed to every shell payload — the
    /// canonical "things that come from the DB, not from source code"
    /// surface. Tasks reference these as `$FF_SOURCE_TREE`, `$FF_NODE`,
    /// `$FF_LEADER_NAME`, etc. Resolved once at startup; if the leader
    /// or source_tree_path changes the daemon is restarted.
    env: Arc<Vec<(String, String)>>,
}

impl TaskRunner {
    pub fn new(
        pg: PgPool,
        my_computer_id: uuid::Uuid,
        my_name: String,
        my_capabilities: HashSet<String>,
        env: Vec<(String, String)>,
    ) -> Self {
        Self {
            pg,
            my_computer_id,
            my_name,
            my_capabilities: Arc::new(my_capabilities),
            env: Arc::new(env),
        }
    }

    /// Resolve the standard env-var bag from the DB. Reads both this
    /// computer's row (for `FF_SOURCE_TREE`, `FF_NODE`, `FF_PRIMARY_IP`)
    /// and the leader's row (for `FF_LEADER_NAME`, `FF_LEADER_IP`,
    /// `FF_GATEWAY_URL`). Falls back gracefully when columns are NULL.
    pub async fn resolve_env_from_db(
        pg: &PgPool,
        my_name: &str,
    ) -> Result<Vec<(String, String)>, sqlx::Error> {
        let mut env = vec![("FF_NODE".to_string(), my_name.to_string())];

        if let Some(row) = sqlx::query(
            "SELECT primary_ip, source_tree_path, ssh_user, os_family
               FROM computers WHERE name = $1",
        )
        .bind(my_name)
        .fetch_optional(pg)
        .await?
        {
            let primary_ip: String = row.get("primary_ip");
            let stp: Option<String> = row.try_get("source_tree_path").ok();
            let ssh_user: String = row.get("ssh_user");
            let os_family: String = row.get("os_family");
            env.push(("FF_PRIMARY_IP".to_string(), primary_ip));
            env.push(("FF_SSH_USER".to_string(), ssh_user));
            env.push(("FF_OS_FAMILY".to_string(), os_family));
            if let Some(s) = stp.filter(|s| !s.is_empty()) {
                env.push(("FF_SOURCE_TREE".to_string(), s));
            }
        }

        if let Some(row) = sqlx::query(
            "SELECT ls.member_name, c.primary_ip
               FROM fleet_leader_state ls
               JOIN computers c ON c.id = ls.computer_id
              LIMIT 1",
        )
        .fetch_optional(pg)
        .await?
        {
            let leader_name: String = row.get("member_name");
            let leader_ip: String = row.get("primary_ip");
            let gateway_port = read_port_secret(pg, "port.gateway").await?;
            let gateway_url = format!("http://{leader_ip}:{gateway_port}");
            env.push(("FF_LEADER_NAME".to_string(), leader_name));
            env.push(("FF_LEADER_IP".to_string(), leader_ip));
            env.push(("FF_GATEWAY_URL".to_string(), gateway_url));
            env.push(("FF_GATEWAY_PORT".to_string(), gateway_port.to_string()));
        }
        Ok(env)
    }

    /// Load-aware gate (P1.3). Returns `false` when this node is
    /// sufficiently overloaded that it should skip claiming new tasks.
    /// Thresholds are conservative — a node at 85% CPU, 90% RAM, or 95%
    /// GPU is considered saturated. Also gates on active task count (>10).
    async fn should_claim_by_load(&self) -> Result<bool, sqlx::Error> {
        // Latest metrics from Pulse (leader-gated downsampler writes these
        // once per minute; if stale >5 min we ignore them).
        let metrics = sqlx::query(
            r#"
            SELECT cpu_pct, ram_pct, gpu_pct
            FROM computer_metrics_history
            WHERE computer_id = $1
            ORDER BY recorded_at DESC
            LIMIT 1
            "#,
        )
        .bind(self.my_computer_id)
        .fetch_optional(&self.pg)
        .await?;

        if let Some(row) = metrics {
            let cpu: Option<f64> = row.try_get("cpu_pct").ok();
            let ram: Option<f64> = row.try_get("ram_pct").ok();
            let gpu: Option<f64> = row.try_get("gpu_pct").ok();

            if cpu.unwrap_or(0.0) > 85.0 {
                return Ok(false);
            }
            if ram.unwrap_or(0.0) > 90.0 {
                return Ok(false);
            }
            if gpu.unwrap_or(0.0) > 95.0 {
                return Ok(false);
            }
        }

        // Also gate on active task count — if we're already running a lot,
        // let other nodes help.
        let active_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM fleet_tasks WHERE claimed_by_computer_id = $1 AND status = 'running'",
        )
        .bind(self.my_computer_id)
        .fetch_one(&self.pg)
        .await?;

        if active_count > 10 {
            return Ok(false);
        }

        Ok(true)
    }

    /// One worker tick — claim at most one ready task and run it.
    pub async fn tick_once(&self) -> Result<Option<uuid::Uuid>, TaskRunnerError> {
        // 0. Distributed watchdog (V61): every tick on every worker scans
        // for stale-heartbeat tasks and demotes them back to `pending`.
        // FOR UPDATE SKIP LOCKED in handoff_stuck_tasks ensures only one
        // peer wins per stuck task. Replaces the leader-only watchdog
        // formerly run from BundledScheduler — closes the SPOF where the
        // leader was both the sole supervisor AND a possible victim.
        if let Err(e) = handoff_stuck_tasks(&self.pg).await {
            debug!(error = %e, "distributed watchdog: handoff sweep failed");
        }

        // 1. Load-aware gate (P1.3): skip claiming if this node is overloaded.
        // Other nodes will pick up the work. Thresholds are conservative to
        // prevent cascading overload while still utilizing fleet capacity.
        match self.should_claim_by_load().await {
            Ok(false) => {
                debug!(node = %self.my_name, "load-aware gate: node overloaded — skipping claim");
                return Ok(None);
            }
            Ok(true) => {}
            Err(e) => {
                debug!(error = %e, "load-aware gate query failed — proceeding with claim");
            }
        }

        // 2. Atomically claim a task whose capabilities we satisfy.
        let cap_array: Vec<String> = self.my_capabilities.iter().cloned().collect();
        // Two-phase / barrier note: tasks with `wait_for_siblings = true`
        // are only claimable when no non-barrier sibling under the same
        // `parent_task_id` is still `pending` or `running`. This lets the
        // dispatcher emit a Phase-1 (parallel build/install, capability=[])
        // and Phase-2 (serialized restart, capability=[leader]) under one
        // parent without scheduling state — Phase-2 rows literally can't
        // be claimed until Phase-1 drains. Closes the self-kill race
        // documented in feedback_wave_dispatcher_self_kill_race.md.
        //
        // V61 worker-exclusion: the claim refuses tasks whose
        // `excludes_computer_ids` array contains this worker's
        // computer_id. The wave dispatcher sets this to `[target_id]`
        // for `*_git` build/restart tasks so the target never claims its
        // own ff upgrade — a peer always does the ssh+build+restart.
        //
        // V62 target quarantine: the claim ALSO refuses ANY task while
        // this worker is itself the target of a wave-task currently
        // running. Reuses excludes_computer_ids as the "I'm the target"
        // marker (V61 only sets it on wave-tasks, so the semantics are
        // tight: a row with my_id in its excludes IS targeting me).
        // Effect: while peer C is upgrading me, my task tick stops
        // claiming new fleet_tasks. My in-flight upgrade can complete +
        // Phase-2 restart can fire without killing in-progress work.
        // Deferred-tasks queue is unaffected — non-`*_git` software
        // continues to flow through this node normally.
        let row = sqlx::query(
            r#"
            UPDATE fleet_tasks
               SET status                 = 'running',
                   claimed_by_computer_id = $1,
                   claimed_at             = NOW(),
                   started_at             = COALESCE(started_at, NOW()),
                   last_heartbeat_at      = NOW()
             WHERE id = (
               SELECT id FROM fleet_tasks t
                WHERE t.status = 'pending'
                  AND t.task_type = 'shell'
                  AND (t.preferred_computer_id IS NULL
                       OR t.preferred_computer_id = $1)
                  AND t.requires_capability <@ to_jsonb($2::text[])
                  AND NOT (t.excludes_computer_ids @> to_jsonb(ARRAY[$1::uuid]))
                  -- V74 local_only: only the creator node can claim
                  AND (t.routing_mode != 'local_only'
                       OR t.created_by_computer_id = $1)
                  AND NOT EXISTS (
                    -- V62 target quarantine: I am being upgraded by a peer.
                    SELECT 1 FROM fleet_tasks q
                     WHERE q.status = 'running'
                       AND q.summary LIKE 'fleet-upgrade-wave/%'
                       AND q.excludes_computer_ids @> to_jsonb(ARRAY[$1::uuid])
                  )
                  AND (
                    t.wait_for_siblings = false
                    OR t.parent_task_id IS NULL
                    OR NOT EXISTS (
                      SELECT 1 FROM fleet_tasks s
                       WHERE s.parent_task_id = t.parent_task_id
                         AND s.id != t.id
                         AND s.wait_for_siblings = false
                         AND s.status IN ('pending', 'running')
                    )
                  )
                  -- V108 per-task dependency: if depends_on_task_id is
                  -- set, the referenced task must have SUCCEEDED. This is
                  -- what lets wave-restart fire per-host instead of
                  -- waiting for every build sibling.
                  --
                  -- 2026-05-24 (fix B): the gate used to accept any
                  -- terminal status (completed/failed/cancelled). That
                  -- meant a host whose build *died* (e.g. killed mid-
                  -- compile by a cross-wave restart) still got its restart
                  -- task claimed — restarting onto stale or half-installed
                  -- binaries. Require 'completed' so a failed build never
                  -- triggers a restart. The orphaned restart (whose build
                  -- failed) is swept to 'cancelled' by the wave-reaper so
                  -- it doesn't sit pending forever.
                  AND (
                    t.depends_on_task_id IS NULL
                    OR EXISTS (
                      SELECT 1 FROM fleet_tasks dep
                       WHERE dep.id = t.depends_on_task_id
                         AND dep.status = 'completed'
                    )
                  )
                  -- V119 arbiter fence: if THIS computer (the claimer, $1) is
                  -- reserved/drained by the resource arbiter, it is off-limits
                  -- to general task claiming — EXCEPT for tasks belonging to the
                  -- reservation's own owning intent, so the reservation holder's
                  -- own work still runs on its reserved host. The owner-tag is
                  -- carried on computers.reserved_reason as 'arbiter:<intent_id>'
                  -- and the owner's tasks use a summary prefix
                  -- 'arbiter/<intent_id>/...' (same summary-prefix fencing
                  -- technique as the V62 quarantine conjunct above). Append-only:
                  -- one AND conjunct; no existing conjunct reordered or removed.
                  AND (
                    COALESCE(
                      (SELECT c.reservation_state FROM computers c WHERE c.id = $1),
                      'available'
                    ) NOT IN ('reserved','drained')
                    OR (SELECT c.reserved_reason FROM computers c WHERE c.id = $1)
                         = 'arbiter:' || split_part(t.summary, '/', 2)
                  )
                  -- 2026-05-26 (within-wave executor-kill guard): a wave
                  -- restart task SSHes into its TARGET host and restarts that
                  -- host's forgefleetd. V61 makes builds peer-driven, so the
                  -- target may itself be acting as the EXECUTOR of some other
                  -- host's build right now (its task_runner holds that build's
                  -- SSH session). Restarting it mid-flight tears the session
                  -- down and the peer's build dies exit=-1 mid-compile — the
                  -- ~8 random failures/pass seen at fanout>1. V62 quarantine
                  -- only stops the target from claiming NEW work while it's
                  -- being restarted; it does nothing for the build already
                  -- in flight ON the target. So hold the restart until its
                  -- target is no longer the claimer of any running wave build.
                  -- The restart's target is the single computer in
                  -- excludes_computer_ids (V61). Builds themselves are never
                  -- gated, so they always drain and the restart fires shortly
                  -- after — this is far more targeted than V108's old
                  -- "wait for every build sibling" global barrier, keeping
                  -- the per-host latency win.
                  AND NOT (
                    t.summary LIKE 'fleet-upgrade-wave/restart:%'
                    AND EXISTS (
                      SELECT 1 FROM fleet_tasks b
                       WHERE b.status = 'running'
                         AND b.summary LIKE 'fleet-upgrade-wave/%/build:%'
                         AND b.claimed_by_computer_id IS NOT NULL
                         AND t.excludes_computer_ids
                               @> to_jsonb(ARRAY[b.claimed_by_computer_id])
                    )
                  )
                  -- 2026-07-20 (drain-before-restart): a wave restart is not
                  -- claimable while its TARGET holds an ACTIVE work_item
                  -- build lease. The V114 no-active-lease gate only runs at
                  -- COMPOSE time; builds take up to 45 min, and by restart
                  -- time the target has claimed fresh leases — restarting
                  -- then orphans the build → stale-heartbeat reap → wasted
                  -- attempt (adele 01:14-01:35: 6 restarts in 21 min, item
                  -- e10adbeb killed twice mid-build). The target is the
                  -- single computer in excludes_computer_ids (V61). Bounded
                  -- wait, no starvation: once this restart's build dep is
                  -- completed, pg_free_slots stops granting the target NEW
                  -- work_item leases (IMMINENT_DAEMON_RESTART_DRAIN_SQL),
                  -- and existing leases are hard-ceilinged by the lease
                  -- reaper (~25 min max) — so the gate always lifts and the
                  -- restart then fires on an idle host.
                  AND NOT (
                    t.summary LIKE 'fleet-upgrade-wave/restart:%'
                    AND EXISTS (
                      SELECT 1 FROM work_item_leases wl
                       WHERE wl.released_at IS NULL
                         AND t.excludes_computer_ids
                               @> to_jsonb(ARRAY[wl.computer_id])
                    )
                  )
                  -- GAP-C per-caller fair-share cap: one caller (a single
                  -- parent_task_id — a swarm/fanout/build invocation) may not
                  -- hold more than $3 'running' tasks fleet-wide WHILE a
                  -- *different* caller has pending work waiting. This stops one
                  -- large swarm from grabbing every worker slot and starving a
                  -- second caller. Work-conserving: the cap only bites under
                  -- contention — with no other caller waiting, the last
                  -- disjunct (`NOT EXISTS other caller`) is true and the caller
                  -- uses the full fleet. Top-level tasks (parent_task_id IS
                  -- NULL) are uncapped — they ARE the work. The system upgrade
                  -- wave is exempt (it intentionally targets every host and has
                  -- its own two-phase concurrency control). Soft limit: a tiny
                  -- count race across concurrent claimers may exceed $3 by 1,
                  -- which is fine for a fairness cap. Append-only conjunct.
                  AND (
                    t.parent_task_id IS NULL
                    OR t.summary LIKE 'fleet-upgrade-wave/%'
                    OR (
                      SELECT COUNT(*) FROM fleet_tasks rs
                       WHERE rs.parent_task_id = t.parent_task_id
                         AND rs.status = 'running'
                    ) < $3
                    OR NOT EXISTS (
                      SELECT 1 FROM fleet_tasks other
                       WHERE other.status = 'pending'
                         AND other.task_type = 'shell'
                         AND COALESCE(other.parent_task_id, other.id)
                             != COALESCE(t.parent_task_id, t.id)
                    )
                  )
                -- V74 selfish routing: fleet_first deprioritizes local tasks;
                -- local_first/balanced prioritizes local tasks.
                ORDER BY
                  CASE t.routing_mode
                    WHEN 'fleet_first' THEN
                      CASE WHEN t.created_by_computer_id = $1 THEN 1 ELSE 0 END
                    WHEN 'local_first' THEN
                      CASE WHEN t.created_by_computer_id = $1 THEN 0 ELSE 1 END
                    WHEN 'balanced' THEN
                      CASE WHEN t.created_by_computer_id = $1 THEN 0 ELSE 1 END
                    ELSE 0
                  END ASC,
                  t.priority DESC,
                  t.created_at ASC
                  FOR UPDATE SKIP LOCKED
                LIMIT 1
             )
            RETURNING id, payload, summary, task_type, timeout_secs
            "#,
        )
        .bind(self.my_computer_id)
        .bind(&cap_array)
        .bind(fair_share_max_running())
        .fetch_optional(&self.pg)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let task_id: uuid::Uuid = row.get("id");
        let payload: Value = row.get("payload");
        // Deferred tasks (`pg_enqueue_deferred`) fold into `fleet_tasks` with the
        // caller's REAL payload nested under a `deferred_payload` key (V153-158
        // queue consolidation). Unwrap it so the shell fields (command / shell /
        // max_duration_secs) are read from the original payload — otherwise every
        // cross-node deferred shell task (notably `ff model download --node`)
        // fails "task payload missing required field: command". Non-deferred
        // fleet_tasks (command at the top level) pass straight through.
        let payload: Value = payload
            .get("deferred_payload")
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or(payload);
        let summary: String = row.get("summary");
        let task_type: String = row.get("task_type");
        let timeout_secs: Option<i32> = row.get("timeout_secs");

        info!(task_id = %task_id, summary = %summary, task_type = %task_type, "task claimed");

        // 2. Spawn a heartbeat ticker for the duration of the run.
        let pg_hb = self.pg.clone();
        let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let hb_task: JoinHandle<()> = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(HEARTBEAT_EVERY) => {
                        let _ = sqlx::query(
                            "UPDATE fleet_tasks SET last_heartbeat_at = NOW() WHERE id = $1",
                        )
                        .bind(task_id)
                        .execute(&pg_hb)
                        .await;
                    }
                    _ = &mut cancel_rx => break,
                }
            }
        });

        // 3. Run the payload — with FF_* env vars injected so tasks
        // never have to embed IPs, paths, or names in source. Per-task
        // override of MAX_TASK_DURATION via `payload.max_duration_secs`;
        // falls back to `fleet_tasks.timeout_secs` (V81) if present.
        let max_duration = payload
            .get("max_duration_secs")
            .and_then(Value::as_u64)
            .map(Duration::from_secs)
            .or_else(|| timeout_secs.map(|s| Duration::from_secs(s as u64)))
            .unwrap_or(MAX_TASK_DURATION);
        // The timeout lives INSIDE run_shell_payload now: on elapse it
        // SIGKILLs the whole process GROUP (shell + every descendant),
        // not just the direct child. A plain outer `tokio::time::timeout`
        // here would drop the future → kill_on_drop reaps only the direct
        // `bash`, orphaning any ssh/git/rsync/cargo grandchildren — the
        // exact leak that wedged priya/sophie (stuck rsync + days-old
        // `git fetch` processes) until every subsequent task hit the cap.
        let outcome = run_shell_payload(&payload, &self.env, max_duration).await;
        let _ = cancel_tx.send(());
        let _ = hb_task.await;

        // 4. Persist result. The `WHERE status = 'running'` guard makes
        // the completion update idempotent against operator
        // cancellation — `ff tasks cancel <id>` flips status to
        // `cancelled`, and a subsequent late-completing worker won't
        // overwrite that.
        match outcome {
            Ok(result) => {
                let exit = result.get("exit").and_then(Value::as_i64).unwrap_or(-1);
                if exit == 0 {
                    sqlx::query(
                        "UPDATE fleet_tasks
                            SET status        = 'completed',
                                completed_at  = NOW(),
                                progress_pct  = 100.0,
                                result        = $1
                          WHERE id = $2 AND status = 'running'",
                    )
                    .bind(&result)
                    .bind(task_id)
                    .execute(&self.pg)
                    .await?;
                    info!(task_id = %task_id, "task completed");
                } else {
                    sqlx::query(
                        "UPDATE fleet_tasks
                            SET status        = 'failed',
                                completed_at  = NOW(),
                                result        = $1,
                                error         = $2
                          WHERE id = $3 AND status = 'running'",
                    )
                    .bind(&result)
                    .bind(format!("non-zero exit: {exit}"))
                    .bind(task_id)
                    .execute(&self.pg)
                    .await?;
                    warn!(task_id = %task_id, exit, "task failed (non-zero exit)");
                }
            }
            Err(e) => {
                sqlx::query(
                    "UPDATE fleet_tasks
                        SET status       = 'failed',
                            completed_at = NOW(),
                            error        = $1
                      WHERE id = $2 AND status = 'running'",
                )
                .bind(format!("{e}"))
                .bind(task_id)
                .execute(&self.pg)
                .await?;
                warn!(task_id = %task_id, error = %e, "task failed (runner error)");
            }
        }

        Ok(Some(task_id))
    }

    /// Spawn the worker tick loop. Tries to claim one task every
    /// `interval_secs`. Exits when `shutdown` flips to true.
    ///
    /// Phase 3 (V77): Consumes the NATS `FF_TASKS` stream to wake
    /// immediately when a new task is inserted.  A 10s fallback interval
    /// remains for resilience if the NATS notification is lost or the
    /// NATS connection drops.  During rollout the PostgreSQL
    /// `fleet_task_inserted` NOTIFY remains active (dual-emission), so
    /// mixed-version workers still wake.
    pub fn spawn(self, interval_secs: u64, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        let interval = Duration::from_secs(interval_secs.max(2));
        tokio::spawn(async move {
            let mut iterations = 0;
            loop {
                if let Err(e) = self.tick_once().await {
                    debug!(error = %e, computer = %self.my_name, "task tick error");
                }
                iterations += 1;
                if iterations >= MAX_ITERATIONS {
                    warn!(
                        max_iterations = MAX_ITERATIONS,
                        computer = %self.my_name,
                        "task runner iteration cap reached; stopping worker"
                    );
                    break;
                }
                tokio::select! {
                    result = crate::ha::agent::listen_for_tasks() => {
                        if let Err(e) = result {
                            debug!(error = %e, "task NATS listener error, falling back to interval");
                        } else {
                            debug!("woken by NATS FF_TASKS notification");
                        }
                    }
                    _ = tokio::time::sleep(interval) => {}
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() { break; }
                    }
                }
            }
        })
    }
}

/// Leader-only watchdog: detect `running` tasks whose worker has gone
/// quiet and put them back on the queue (or fail them after
/// [`MAX_HANDOFFS`]).
pub async fn handoff_stuck_tasks(pg: &PgPool) -> Result<usize, sqlx::Error> {
    // Demote stale-claim tasks back to pending unless we've already
    // handed off too many times — those become permanent failures.
    let demoted = sqlx::query(
        r#"
        UPDATE fleet_tasks
           SET status                 = 'pending',
               claimed_by_computer_id = NULL,
               claimed_at             = NULL,
               last_heartbeat_at      = NULL,
               handoff_count          = handoff_count + 1,
               handoff_reason         = 'heartbeat_stale',
               original_computer_id   = COALESCE(original_computer_id, claimed_by_computer_id)
         WHERE status = 'running'
           AND last_heartbeat_at < NOW() - make_interval(secs => $1::int)
           AND handoff_count < $2
        RETURNING id
        "#,
    )
    .bind(STUCK_AFTER_SECS as i32)
    .bind(MAX_HANDOFFS)
    .fetch_all(pg)
    .await?;

    let _ = sqlx::query(
        r#"
        UPDATE fleet_tasks
           SET status       = 'failed',
               completed_at = NOW(),
               error        = 'exceeded MAX_HANDOFFS retries'
         WHERE status = 'running'
           AND last_heartbeat_at < NOW() - make_interval(secs => $1::int)
           AND handoff_count >= $2
        "#,
    )
    .bind(STUCK_AFTER_SECS as i32)
    .bind(MAX_HANDOFFS)
    .execute(pg)
    .await?;

    Ok(demoted.len())
}

/// Operator escape hatch — mark a task `cancelled` regardless of its
/// current state. Returns the previous status so the caller can warn
/// when there's nothing to cancel. The worker's completion UPDATE is
/// gated on `status = 'running'`, so a hung worker that finishes late
/// won't clobber the cancellation. The actual child process keeps
/// running on the worker until it exits or hits MAX_TASK_DURATION;
/// its row stays `cancelled` either way.
pub async fn pg_cancel_task(
    pg: &PgPool,
    task_id: uuid::Uuid,
    reason: &str,
) -> Result<Option<String>, sqlx::Error> {
    let prev_status: Option<String> = sqlx::query_scalar(
        "UPDATE fleet_tasks
            SET status       = 'cancelled',
                completed_at = COALESCE(completed_at, NOW()),
                error        = COALESCE(error, $1)
          WHERE id = $2
            AND status NOT IN ('completed', 'failed', 'cancelled')
        RETURNING (
          SELECT status FROM fleet_tasks WHERE id = $2
        )",
    )
    .bind(reason)
    .bind(task_id)
    .fetch_optional(pg)
    .await?;
    Ok(prev_status)
}

/// Spawn the distributed handoff watchdog as a background tick.
///
/// V61: every daemon runs this — no leader gate. `handoff_stuck_tasks`
/// uses `FOR UPDATE SKIP LOCKED`, so concurrent watchdogs across peers
/// race safely and only one wins per stuck task. Closes the SPOF where
/// the leader was both the sole watchdog AND a possible victim:
/// previously, if the leader died mid-task, the watchdog died with it
/// and stuck rows sat indefinitely until election + recovery.
///
/// `my_name` is kept for log context only.
pub fn spawn_leader_watchdog(
    pg: PgPool,
    my_name: String,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // 60s tick — half the stuck threshold so detection latency stays bounded.
        let interval = Duration::from_secs(60);
        loop {
            match handoff_stuck_tasks(&pg).await {
                Ok(n) if n > 0 => {
                    info!(
                        handed_off = n,
                        watchdog_node = %my_name,
                        "distributed task watchdog re-queued stale tasks"
                    );
                }
                Ok(_) => {}
                Err(e) => warn!(error = %e, "task watchdog query failed"),
            }
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
            }
        }
    })
}

/// Enqueue a single shell task. Used by the CLI and by the
/// `compose_aura_bootstrap` helper. Equivalent to
/// [`pg_enqueue_shell_task_ext`] with `wait_for_siblings = false`.
#[allow(clippy::too_many_arguments)]
pub async fn pg_enqueue_shell_task(
    pg: &PgPool,
    summary: &str,
    command: &str,
    capabilities: &[String],
    preferred_computer: Option<&str>,
    parent_task_id: Option<uuid::Uuid>,
    priority: i32,
    created_by_computer_id: Option<uuid::Uuid>,
) -> Result<uuid::Uuid, sqlx::Error> {
    pg_enqueue_shell_task_ext(
        pg,
        summary,
        command,
        capabilities,
        preferred_computer,
        parent_task_id,
        priority,
        created_by_computer_id,
        false,
    )
    .await
}

/// Like [`pg_enqueue_shell_task`] but with `wait_for_siblings`. When set,
/// the row is only claimable when no non-barrier sibling under the same
/// `parent_task_id` is still pending or running. Used by the two-phase
/// wave dispatcher to gate Phase-2 (restart) tasks behind Phase-1
/// (build/install) completion.
#[allow(clippy::too_many_arguments)]
pub async fn pg_enqueue_shell_task_ext(
    pg: &PgPool,
    summary: &str,
    command: &str,
    capabilities: &[String],
    preferred_computer: Option<&str>,
    parent_task_id: Option<uuid::Uuid>,
    priority: i32,
    created_by_computer_id: Option<uuid::Uuid>,
    wait_for_siblings: bool,
) -> Result<uuid::Uuid, sqlx::Error> {
    pg_enqueue_shell_task_with_options(
        pg,
        summary,
        command,
        capabilities,
        preferred_computer,
        parent_task_id,
        priority,
        created_by_computer_id,
        wait_for_siblings,
        &[],
    )
    .await
}

/// Most general enqueue: also accepts `excludes_computer_ids` (V61) so
/// a task can refuse to be claimed by named workers. Used by the wave
/// dispatcher to keep `*_git` upgrades peer-driven (target excluded
/// from claiming its own ff upgrade).
#[allow(clippy::too_many_arguments)]
pub async fn pg_enqueue_shell_task_with_options(
    pg: &PgPool,
    summary: &str,
    command: &str,
    capabilities: &[String],
    preferred_computer: Option<&str>,
    parent_task_id: Option<uuid::Uuid>,
    priority: i32,
    created_by_computer_id: Option<uuid::Uuid>,
    wait_for_siblings: bool,
    excludes_computer_ids: &[uuid::Uuid],
) -> Result<uuid::Uuid, sqlx::Error> {
    pg_enqueue_shell_task_full(
        pg,
        summary,
        command,
        capabilities,
        preferred_computer,
        parent_task_id,
        priority,
        created_by_computer_id,
        wait_for_siblings,
        excludes_computer_ids,
        None,
        None,
    )
    .await
}

/// Like [`pg_enqueue_shell_task_with_options`] but with an explicit
/// `max_duration_secs` override that pierces the per-task payload so
/// long-running shell commands (cold cargo builds, model downloads)
/// don't hit the default 10-min `MAX_TASK_DURATION`. Pass `None` to
/// keep the default.
#[allow(clippy::too_many_arguments)]
pub async fn pg_enqueue_shell_task_full(
    pg: &PgPool,
    summary: &str,
    command: &str,
    capabilities: &[String],
    preferred_computer: Option<&str>,
    parent_task_id: Option<uuid::Uuid>,
    priority: i32,
    created_by_computer_id: Option<uuid::Uuid>,
    wait_for_siblings: bool,
    excludes_computer_ids: &[uuid::Uuid],
    max_duration_secs: Option<u64>,
    depends_on_task_id: Option<uuid::Uuid>,
) -> Result<uuid::Uuid, sqlx::Error> {
    let preferred_id: Option<uuid::Uuid> = if let Some(name) = preferred_computer {
        sqlx::query_scalar("SELECT id FROM computers WHERE name = $1")
            .bind(name)
            .fetch_optional(pg)
            .await?
    } else {
        None
    };

    let payload = match max_duration_secs {
        Some(secs) => json!({ "command": command, "max_duration_secs": secs }),
        None => json!({ "command": command }),
    };
    let caps = serde_json::Value::Array(
        capabilities
            .iter()
            .map(|c| Value::String(c.clone()))
            .collect(),
    );
    let excludes_json = serde_json::Value::Array(
        excludes_computer_ids
            .iter()
            .map(|id| Value::String(id.to_string()))
            .collect(),
    );

    let id: uuid::Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO fleet_tasks (
            parent_task_id, task_type, summary, payload,
            priority, requires_capability, preferred_computer_id,
            created_by_computer_id, wait_for_siblings,
            excludes_computer_ids, depends_on_task_id
        )
        VALUES ($1, 'shell', $2, $3, $4, $5, $6, $7, $8, $9, $10)
        RETURNING id
        "#,
    )
    .bind(parent_task_id)
    .bind(summary)
    .bind(&payload)
    .bind(priority)
    .bind(&caps)
    .bind(preferred_id)
    .bind(created_by_computer_id)
    .bind(wait_for_siblings)
    .bind(&excludes_json)
    .bind(depends_on_task_id)
    .fetch_one(pg)
    .await?;

    // Dual-emission: notify NATS FF_TASKS stream in addition to the
    // PostgreSQL fleet_task_inserted NOTIFY during rollout.
    crate::nats_jetstream::publish_task_inserted(id).await;

    Ok(id)
}

/// Like [`pg_enqueue_shell_task_with_options`] but with explicit routing mode.
/// Use this when you need `fleet_first`, `local_only`, or `balanced` instead
/// of the default `fleet_first`.
#[allow(clippy::too_many_arguments)]
pub async fn pg_enqueue_shell_task_routed(
    pg: &PgPool,
    summary: &str,
    command: &str,
    capabilities: &[String],
    preferred_computer: Option<&str>,
    parent_task_id: Option<uuid::Uuid>,
    priority: i32,
    created_by_computer_id: Option<uuid::Uuid>,
    wait_for_siblings: bool,
    excludes_computer_ids: &[uuid::Uuid],
    routing_mode: &str,
) -> Result<uuid::Uuid, sqlx::Error> {
    let preferred_id: Option<uuid::Uuid> = if let Some(name) = preferred_computer {
        sqlx::query_scalar("SELECT id FROM computers WHERE name = $1")
            .bind(name)
            .fetch_optional(pg)
            .await?
    } else {
        None
    };

    let payload = json!({ "command": command });
    let caps = serde_json::Value::Array(
        capabilities
            .iter()
            .map(|c| Value::String(c.clone()))
            .collect(),
    );
    let excludes_json = serde_json::Value::Array(
        excludes_computer_ids
            .iter()
            .map(|id| Value::String(id.to_string()))
            .collect(),
    );

    let id: uuid::Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO fleet_tasks (
            parent_task_id, task_type, summary, payload,
            priority, requires_capability, preferred_computer_id,
            created_by_computer_id, wait_for_siblings,
            excludes_computer_ids, routing_mode
        )
        VALUES ($1, 'shell', $2, $3, $4, $5, $6, $7, $8, $9, $10)
        RETURNING id
        "#,
    )
    .bind(parent_task_id)
    .bind(summary)
    .bind(&payload)
    .bind(priority)
    .bind(&caps)
    .bind(preferred_id)
    .bind(created_by_computer_id)
    .bind(wait_for_siblings)
    .bind(&excludes_json)
    .bind(routing_mode)
    .fetch_one(pg)
    .await?;

    // Dual-emission: notify NATS FF_TASKS stream in addition to the
    // PostgreSQL fleet_task_inserted NOTIFY during rollout.
    crate::nats_jetstream::publish_task_inserted(id).await;

    Ok(id)
}

/// Method passed to [`pg_enqueue_pr_merge_task`].
#[derive(Debug, Clone, Copy)]
pub enum PrMergeMethod {
    Merge,
    Squash,
    Rebase,
}

impl PrMergeMethod {
    fn flag(self) -> &'static str {
        match self {
            PrMergeMethod::Merge => "--merge",
            PrMergeMethod::Squash => "--squash",
            PrMergeMethod::Rebase => "--rebase",
        }
    }
}

/// Enqueue a fleet task that watches `<pr_number>` for CI green and then
/// merges it via `gh pr merge --<method> --delete-branch`. The
/// `--delete-branch` flag is **always** included — branch cleanup is
/// project policy (see `feedback_pr_merge_delete_branch.md`). Use this
/// helper instead of inlining `gh pr merge` so the policy lives in one
/// place.
///
/// `ci_timeout_iters` × 30s = the maximum wait for CI to settle. Set to
/// 0 for no CI watch (merge unconditionally — useful when the operator
/// has already verified CI).
#[allow(clippy::too_many_arguments)]
pub async fn pg_enqueue_pr_merge_task(
    pg: &PgPool,
    pr_number: u32,
    method: PrMergeMethod,
    ci_timeout_iters: u32,
    parent_task_id: Option<uuid::Uuid>,
    priority: i32,
    created_by_computer_id: Option<uuid::Uuid>,
) -> Result<uuid::Uuid, sqlx::Error> {
    let summary = format!("ff: merge PR #{pr_number} ({method:?}) + delete branch");
    let merge_flag = method.flag();
    let command = if ci_timeout_iters > 0 {
        format!(
            "set -e; cd ${{FF_SOURCE_TREE/#\\~/$HOME}}\n\
             PR={pr_number}\n\
             echo \"watching PR #$PR CI ({iters} iters × 30s = {secs}s max)...\"\n\
             for i in $(seq 1 {iters}); do\n\
               out=$(gh pr checks $PR --json name,bucket 2>/dev/null || echo '[]')\n\
               pending=$(echo \"$out\" | jq '[.[] | select(.bucket==\"pending\")] | length')\n\
               fail=$(echo \"$out\" | jq '[.[] | select(.bucket==\"fail\")] | length')\n\
               pass=$(echo \"$out\" | jq '[.[] | select(.bucket==\"pass\")] | length')\n\
               echo \"iter=$i pending=$pending pass=$pass fail=$fail\"\n\
               if [ \"$fail\" != \"0\" ]; then echo 'CI red'; gh pr checks $PR; exit 1; fi\n\
               if [ \"$pending\" = \"0\" ] && [ \"$pass\" -gt 0 ]; then break; fi\n\
               sleep 30\n\
             done\n\
             echo 'merging...'\n\
             gh pr merge $PR {merge_flag} --delete-branch\n\
             gh pr view $PR --json state,mergedAt\n",
            iters = ci_timeout_iters,
            secs = ci_timeout_iters * 30,
        )
    } else {
        format!(
            "set -e; cd ${{FF_SOURCE_TREE/#\\~/$HOME}}\n\
             gh pr merge {pr_number} {merge_flag} --delete-branch\n\
             gh pr view {pr_number} --json state,mergedAt\n",
        )
    };

    pg_enqueue_shell_task(
        pg,
        &summary,
        &command,
        &["leader".to_string()],
        None,
        parent_task_id,
        priority,
        created_by_computer_id,
    )
    .await
}

/// Run a `task_type=shell` payload via `/bin/bash -lc <command>`.
/// `env` is injected on top of the inherited daemon env — these are
/// the `FF_*` values resolved from the DB at worker startup.
async fn run_shell_payload(
    payload: &Value,
    env: &[(String, String)],
    max_duration: Duration,
) -> Result<Value, TaskRunnerError> {
    let command = payload
        .get("command")
        .and_then(Value::as_str)
        .ok_or(TaskRunnerError::BadPayload("command".to_string()))?
        .to_string();

    // Security: block obviously destructive commands (same list as BashTool).
    if is_blocked_command(&command) {
        return Err(TaskRunnerError::BadPayload(
            "Command blocked for safety: potentially destructive operation".to_string(),
        ));
    }

    let shell = payload
        .get("shell")
        .and_then(Value::as_str)
        .unwrap_or("/bin/bash")
        .to_string();

    // Security: validate shell path against allowlist.
    const ALLOWED_SHELLS: &[&str] = &[
        "/bin/bash",
        "/bin/sh",
        "/bin/zsh",
        "/usr/bin/bash",
        "/usr/bin/sh",
        "/usr/bin/zsh",
    ];
    if !ALLOWED_SHELLS.contains(&shell.as_str()) {
        return Err(TaskRunnerError::BadPayload(format!(
            "shell '{shell}' is not in the allowed list: {ALLOWED_SHELLS:?}"
        )));
    }

    // Use tokio::process so the child can actually be killed when the
    // task times out. Two hardening measures work together here:
    //
    //  1. `process_group(0)` puts the shell into its OWN process group
    //     (it becomes the group leader, pgid == its pid). Every
    //     descendant it forks — ssh, git, rsync, cargo, a relaunched
    //     model server — joins that group unless it deliberately
    //     re-`setsid`s out (which legitimately-detached daemons do).
    //
    //  2. On timeout we SIGKILL the whole group (`kill(-pgid)`), not
    //     just the direct child. `kill_on_drop(true)` only reaps the
    //     immediate `bash`; a build playbook's grandchildren would be
    //     orphaned and run forever. That is the leak that wedged
    //     priya/sophie — `output()` blocks until the stdout/stderr pipe
    //     hits EOF, and an orphaned grandchild holding the pipe's
    //     write-end keeps it open until the full max-duration cap, after
    //     which the old code reaped only `bash` and left the grandchild
    //     (stuck rsync / days-old `git fetch`) alive to wedge the NEXT
    //     task too. Killing the group closes the pipe and clears the
    //     leak. See feedback_priya_worker_hangs_with_fresh_heartbeat.md.
    //
    // stdin is `/dev/null` so nothing the task spawns can block on a
    // read from the (closed) task channel.
    let mut cmd = tokio::process::Command::new(&shell);
    cmd.arg("-lc").arg(&command);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.process_group(0);
    cmd.kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| TaskRunnerError::BadPayload(format!("spawn: {e}")))?;
    // Captured before any await so it survives even if the child is
    // later reaped; it is the group id we target with `kill(-pid)`.
    let pgid = child.id();

    // Drain stdout and stderr CONCURRENTLY with the wait — reading one
    // fully before the other can deadlock a task that fills the second
    // pipe's 64 KiB buffer while blocked waiting for us to drain it.
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let collect = async {
        let read_out = async {
            let mut buf = Vec::new();
            if let Some(mut o) = stdout_pipe {
                let _ = tokio::io::AsyncReadExt::read_to_end(&mut o, &mut buf).await;
            }
            buf
        };
        let read_err = async {
            let mut buf = Vec::new();
            if let Some(mut e) = stderr_pipe {
                let _ = tokio::io::AsyncReadExt::read_to_end(&mut e, &mut buf).await;
            }
            buf
        };
        let (stdout_buf, stderr_buf, status) = tokio::join!(read_out, read_err, child.wait());
        (status, stdout_buf, stderr_buf)
    };

    match tokio::time::timeout(max_duration, collect).await {
        Ok((status, stdout_buf, stderr_buf)) => {
            let exit = status.ok().and_then(|s| s.code()).unwrap_or(-1) as i64;
            let stdout = String::from_utf8_lossy(&stdout_buf).to_string();
            let stderr = String::from_utf8_lossy(&stderr_buf).to_string();
            Ok(json!({
                "exit": exit,
                "stdout": stdout.chars().take(8192).collect::<String>(),
                "stderr": stderr.chars().take(8192).collect::<String>(),
            }))
        }
        Err(_) => {
            // Reap the ENTIRE group so no grandchild is left orphaned.
            if let Some(pid) = pgid {
                kill_process_group(pid);
            }
            Err(TaskRunnerError::Timeout(max_duration.as_secs()))
        }
    }
}

/// SIGKILL an entire process group by its leader pid.
///
/// We spawn shell tasks with `process_group(0)`, so the shell's pid IS
/// the group id and `-pid` reaches the shell plus every descendant that
/// did not deliberately `setsid` into its own session. Used on timeout
/// to guarantee no grandchild (ssh / git / rsync / cargo) leaks past the
/// task it belonged to.
///
/// `pub(crate)` so the deferred-task executor (`defer_worker`) reuses the
/// exact same group-kill semantics — both executors spawn with
/// `process_group(0)` and must reap the whole group on timeout.
#[cfg(unix)]
pub(crate) fn kill_process_group(pid: u32) {
    // SAFETY: `kill(2)` with a negative pid targets the process group;
    // SIGKILL cannot be caught or ignored. An already-dead group simply
    // returns ESRCH, which we ignore.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
pub(crate) fn kill_process_group(_pid: u32) {}

/// Compose the multi-step "bring `<target>` online" task graph atomically.
///
/// Every node-specific value (IPs, ssh user, role) and every fleet
/// constant (gateway port) is read from the DB at compose time — no
/// IPs, names, usernames, or port numbers in source. Ports come from
/// `fleet_secrets` (seeded by V50: `port.gateway`, …)
/// so an operator can change one row to change a port fleet-wide
/// without a recompile.
///
/// One task in a composed-but-not-yet-enqueued graph, returned by the
/// `compose_*` functions when `dry_run` is set. Mirrors the arguments
/// that would otherwise be passed to [`pg_enqueue_shell_task`] /
/// [`pg_enqueue_shell_task_full`] so the preview can't drift from what
/// the real path enqueues.
#[derive(Debug, Clone)]
pub struct PlannedTask {
    pub summary: String,
    pub command: String,
    pub capabilities: Vec<String>,
    pub priority: i32,
    pub timeout_secs: Option<i64>,
    /// Human-readable label of the in-graph task this one waits for
    /// (e.g. the matching wave build), or `None` when it only depends
    /// on the parent. Not a real task id — the deps aren't enqueued in
    /// dry-run, so this is for display only.
    pub depends_on: Option<String>,
}

/// Result of a `compose_*` call. In the real path `parent` is the
/// enqueued compound-task id and `tasks` is empty; in `dry_run` mode
/// `parent` is `None` and `tasks` holds the full graph for preview.
#[derive(Debug, Clone)]
pub struct ComposePlan {
    pub parent: Option<uuid::Uuid>,
    pub parent_summary: String,
    pub tasks: Vec<PlannedTask>,
}

/// One parent (compound, no shell) plus N children, one per
/// cooperative step. Children are sequenced by descending priority so
/// the leader picks them in order; for a true DAG with edges we'd add
/// a `depends_on` column to V44.
///
/// When `dry_run` is true NOTHING is written to the DB: the function
/// reads the same fleet data, composes the identical task graph, and
/// returns it in [`ComposePlan::tasks`] for preview (the parent is
/// `None`). When false the real path is byte-for-byte unchanged —
/// every task is enqueued and `tasks` is empty.
pub async fn compose_node_bootstrap(
    pg: &PgPool,
    target_name: &str,
    leader_computer_id: uuid::Uuid,
    dry_run: bool,
) -> Result<ComposePlan, sqlx::Error> {
    let mut planned: Vec<PlannedTask> = Vec::new();
    // These onboarding steps run on the leader, which after an HA failover can
    // be a headless Linux follower whose inherited ssh-agent may be wedged —
    // bypass it the same way the wave SSH does (see `crate::ssh_opts`).
    let ssh_bypass = SSH_AGENT_BYPASS;
    // ── 1. Pull everything we need from the DB. ──────────────────────────
    let target_row = sqlx::query(
        "SELECT name, primary_ip, all_ips, ssh_user, ssh_port, os_family
           FROM computers WHERE name = $1",
    )
    .bind(target_name)
    .fetch_optional(pg)
    .await?
    .ok_or_else(|| {
        sqlx::Error::RowNotFound // mapped by caller
    })?;

    let target_name: String = target_row.get("name");
    let target_primary_ip: String = target_row.get("primary_ip");
    let target_all_ips_json: serde_json::Value = target_row.get("all_ips");
    let target_ssh_user: String = target_row.get("ssh_user");
    let target_ssh_port: i32 = target_row.get("ssh_port");
    let target_os_family: String = target_row.get("os_family");

    // Flatten all_ips JSONB → Vec<String>. The column may be a list of
    // strings or a list of {ip,iface,kind,…} objects depending on the
    // writer's vintage; tolerate both.
    let target_ips: Vec<String> = match &target_all_ips_json {
        serde_json::Value::Array(arr) if !arr.is_empty() => arr
            .iter()
            .filter_map(|v| match v {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Object(o) => {
                    o.get("ip").and_then(|x| x.as_str()).map(String::from)
                }
                _ => None,
            })
            .collect(),
        _ => vec![target_primary_ip.clone()],
    };

    let leader_ip: String = sqlx::query_scalar("SELECT primary_ip FROM computers WHERE id = $1")
        .bind(leader_computer_id)
        .fetch_one(pg)
        .await?;
    // Single source of truth: fleet_secrets['port.gateway'] (V50).
    let gateway_port = read_port_secret(pg, "port.gateway").await?;
    let gateway_url = format!("http://{leader_ip}:{gateway_port}");

    // ── 2. Parent task. ──────────────────────────────────────────────────
    let parent_summary = format!("{target_name}: bring online via fleet cooperation");
    let parent: uuid::Uuid = if dry_run {
        uuid::Uuid::nil()
    } else {
        sqlx::query_scalar(
            r#"
        INSERT INTO fleet_tasks (
            task_type, summary, payload, priority, created_by_computer_id
        )
        VALUES ('compound', $1, '{}'::jsonb, 80, $2)
        RETURNING id
        "#,
        )
        .bind(&parent_summary)
        .bind(leader_computer_id)
        .fetch_one(pg)
        .await?
    };

    let port_arg = if target_ssh_port == 22 {
        String::new()
    } else {
        format!(" -p {target_ssh_port}")
    };
    let ssh_target_for_iter = format!("{target_ssh_user}@$ip");
    let ssh_target_primary = format!("{target_ssh_user}@{target_primary_ip}");

    // ── Step 1: probe every known IP. ────────────────────────────────────
    let ip_list = target_ips.join(" ");
    let step1 = format!(
        // ssh -o ConnectTimeout, NOT `timeout 5 ssh`: this step runs on the
        // LEADER (capability=["leader"]), which is often macOS (taylor/ace)
        // where `timeout` is not a default command (it's `gtimeout` via
        // coreutils) — so `timeout 5 ssh` silently breaks the probe on macOS.
        // ssh's built-in ConnectTimeout is portable and covers the hang case.
        "set -e; for ip in {ip_list}; do \
           echo \"== $ip ==\"; \
           ssh{port_arg} -o ConnectTimeout=5 {ssh_bypass} -o StrictHostKeyChecking=accept-new \
             {ssh_target_for_iter} 'echo ok && uname -srvmo' || echo 'unreachable'; \
         done"
    );
    let summary1 = format!("{target_name}/1: ssh-probe all known IPs");
    if dry_run {
        planned.push(PlannedTask {
            summary: summary1,
            command: step1,
            capabilities: vec!["leader".to_string()],
            priority: 90,
            timeout_secs: None,
            depends_on: None,
        });
    } else {
        pg_enqueue_shell_task(
            pg,
            &summary1,
            &step1,
            &["leader".to_string()],
            None,
            Some(parent),
            90,
            Some(leader_computer_id),
        )
        .await?;
    }

    // ── Step 2: leader runs the bootstrap script. ────────────────────────
    let step2 = format!(
        "ssh{port_arg} {ssh_bypass} {ssh_target_primary} \
         \"curl -fsSL '{gateway_url}/onboard/bootstrap.sh\
?name={target_name}&ip={target_primary_ip}&ssh_user={target_ssh_user}&role=builder&runtime=auto' \
         | sudo bash\""
    );
    let summary2 = format!("{target_name}/2: install forgefleetd via gateway bootstrap");
    if dry_run {
        planned.push(PlannedTask {
            summary: summary2,
            command: step2,
            capabilities: vec!["leader".to_string()],
            priority: 85,
            timeout_secs: None,
            depends_on: None,
        });
    } else {
        pg_enqueue_shell_task(
            pg,
            &summary2,
            &step2,
            &["leader".to_string()],
            None,
            Some(parent),
            85,
            Some(leader_computer_id),
        )
        .await?;
    }

    // ── Step 3: verify forgefleetd active. ───────────────────────────────
    // Unit names are project-fixed deploy artifacts (see deploy/linux/
    // forgefleet.service and revive.rs fallback chain) — keeping them
    // here is consistent with treating them as constants. `grep -qx`
    // (exact line match) avoids the bug where 'inactive' matches.
    // macOS uses launchctl rather than systemctl; gate the check.
    let step3 = if target_os_family == "macos" {
        format!(
            "ssh{port_arg} {ssh_bypass} {ssh_target_primary} \
             'launchctl list | grep -E \"com\\.forgefleet\\.(forgefleetd|daemon)\" >/dev/null'"
        )
    } else {
        format!(
            "ssh{port_arg} {ssh_bypass} {ssh_target_primary} \
             'systemctl --user is-active forgefleetd.service \
                || systemctl --user is-active forgefleet-node.service \
                || systemctl --user is-active forgefleet-daemon.service \
                || systemctl --user is-active forgefleet-agent.service' \
             | grep -qx active"
        )
    };
    let summary3 = format!("{target_name}/3: verify forgefleetd running on {target_name}");
    if dry_run {
        planned.push(PlannedTask {
            summary: summary3,
            command: step3,
            capabilities: vec!["leader".to_string()],
            priority: 80,
            timeout_secs: None,
            depends_on: None,
        });
    } else {
        pg_enqueue_shell_task(
            pg,
            &summary3,
            &step3,
            &["leader".to_string()],
            None,
            Some(parent),
            80,
            Some(leader_computer_id),
        )
        .await?;
    }

    // ── Step 4: confirm online via ff (any peer with ff). ────────────────
    let step4 = format!(
        "for i in $(seq 1 30); do \
           if ff fleet health 2>/dev/null | awk '$1 == \"{target_name}\" {{print $3}}' \
              | grep -qx online; then \
             echo '{target_name} online in fleet health'; exit 0; \
           fi; sleep 2; \
         done; echo 'timeout: {target_name} still not online after 60s'; exit 1"
    );
    let summary4 =
        format!("{target_name}/4: confirm {target_name} shows online in ff fleet health");
    if dry_run {
        planned.push(PlannedTask {
            summary: summary4,
            command: step4,
            capabilities: vec!["ff".to_string()],
            priority: 75,
            timeout_secs: None,
            depends_on: None,
        });
    } else {
        pg_enqueue_shell_task(
            pg,
            &summary4,
            &step4,
            &["ff".to_string()],
            None,
            Some(parent),
            75,
            Some(leader_computer_id),
        )
        .await?;
    }

    Ok(ComposePlan {
        parent: if dry_run { None } else { Some(parent) },
        parent_summary,
        tasks: planned,
    })
}

/// Compose a wave-based fleet-upgrade graph for `software_id`.
///
/// **Why waves.** A node cannot reliably restart its own daemon from a
/// task running inside that daemon — the supervisor (`launchd` on
/// macOS, `systemd --user` on Linux) sends `SIGKILL` to the whole
/// process group when it kills the unit, so the restart command takes
/// the task itself down with it; the task is left in `running` state,
/// the watchdog re-queues it 120s later, and the loop repeats until
/// `MAX_HANDOFFS` retries are exhausted.
///
/// The fix is structural: a peer SSHs into the target and runs the
/// upgrade remotely. The executor's daemon stays alive throughout (it
/// only spawns an `ssh` subshell); the target's daemon dies after its
/// session ends, but by then the executor has captured stdout / stderr
/// / exit and marked the row `completed`.
///
/// **Wave layout.** Every non-leader online member with a row in
/// `computer_software` for this `software_id` is a target. Targets are
/// chunked into waves of `fanout`. Wave 0 has the highest priority so
/// the workers pick them first; subsequent waves descend by 3 in
/// priority. Tasks have an empty `requires_capability` set, so any
/// online worker can claim them — the fleet self-parallelizes once
/// Wave 0 has expanded the worker pool from 1 (just the leader) to
/// `1 + fanout`. The leader is intentionally excluded from this graph
/// (cycles back into the suicide-restart problem); restart it
/// manually after the fleet is upgraded, e.g. by SSHing from any
/// upgraded peer.
///
/// **No hardcoded fleet data.** Every per-target value (ssh_user,
/// primary_ip, ssh_port, the playbook command itself) is read from
/// the DB at compose time via
/// [`crate::auto_upgrade::resolve_upgrade_plans`].
pub async fn compose_fleet_upgrade_wave(
    pg: &PgPool,
    software_id: &str,
    fanout: usize,
    leader_computer_id: uuid::Uuid,
    dry_run: bool,
) -> Result<ComposePlan, sqlx::Error> {
    compose_fleet_upgrade_wave_filtered(pg, software_id, fanout, leader_computer_id, dry_run, None)
        .await
}

/// Stage-scoped variant of [`compose_fleet_upgrade_wave`]. When
/// `target_filter` is `Some(names)`, only those (case-insensitive) member names
/// are composed into the wave — every other resolvable target is dropped. This
/// is how the staged-rollout tick composes ONE stage at a time (canary → the
/// rest) while preserving the V62 one-wave-per-family-in-flight invariant: the
/// tick never composes the next stage until the current one is terminal, so the
/// singleton always sees a single wave. `None` is the all-targets behaviour the
/// public wrapper passes through unchanged.
pub async fn compose_fleet_upgrade_wave_filtered(
    pg: &PgPool,
    software_id: &str,
    fanout: usize,
    leader_computer_id: uuid::Uuid,
    dry_run: bool,
    target_filter: Option<&[String]>,
) -> Result<ComposePlan, sqlx::Error> {
    use crate::auto_upgrade::resolve_upgrade_plans_with_suffix;

    let filter_lower: Option<std::collections::HashSet<String>> = target_filter.map(|names| {
        names
            .iter()
            .map(|n| n.trim().to_ascii_lowercase())
            .collect()
    });

    let mut planned: Vec<PlannedTask> = Vec::new();
    let fanout = fanout.max(1);

    // 1. Resolve playbook for every member that has this software.
    //    `upgrade_available_only=false` means we get ALL targets, not
    //    just drift candidates — the dispatcher itself is the trigger.
    //    `key_suffix=Some("build-only")` requests the V52 build-only
    //    playbook variant for Phase-1; resolver falls through to the
    //    plain key on older deployments where V52 hasn't run.
    let (plans, skipped) =
        resolve_upgrade_plans_with_suffix(pg, software_id, None, false, Some("build-only"))
            .await
            .map_err(|e| {
                sqlx::Error::Configuration(format!("resolve_upgrade_plans: {e}").into())
            })?;

    if !skipped.is_empty() {
        tracing::info!(
            count = skipped.len(),
            "fleet-upgrade-wave: members skipped (no playbook key matched)"
        );
    }

    // 2. Pull leader name; non-leader plans become wave targets.
    let leader_name: Option<String> =
        sqlx::query_scalar("SELECT name FROM computers WHERE id = $1")
            .bind(leader_computer_id)
            .fetch_optional(pg)
            .await?;
    let leader_lower = leader_name
        .as_deref()
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();

    // V62 wave-level singleton: refuse to dispatch a new wave for this
    // software if ANY wave-task for the same software is currently
    // pending or running. Tightens V61's per-target dedup to per-software.
    //
    // Why: per-target dedup leaves the door open to cross-wave races. A
    // worker B running Wave-N "build on D" can be Phase-2-restarted by
    // Wave-N+1 (different parent_task_id, so wait_for_siblings doesn't
    // see Wave-N's running task) — B's daemon dies mid-build on D and
    // the task fails. Wave-level singleton means only one wave per
    // software is in flight at any time, eliminating the cross-wave
    // race entirely. Operator running back-to-back ticks gets a no-op
    // on the second one until the first wave drains.
    //
    // 2026-05-24 (fix A): the per-software singleton is NOT enough for the
    // daemon-self family. ff_git and forgefleetd_git are distinct
    // software_ids, so the auto-upgrade tick composes a *separate* wave
    // for each in the same instant — both targeting all 14 hosts. They're
    // invisible to each other: forgefleetd_git's Phase-2 restart tears
    // down host X's task_runner subprocess while ff_git's wave is using
    // host X as a build worker, so ff_git's build dies with exit=-1 mid-
    // compile. This is feedback_wave_dispatcher_self_kill_race.md
    // resurfacing across wave *families*. Fix: for any daemon-self
    // software, the singleton spans the WHOLE family — refuse if any
    // ff_git / forgefleetd_git / forgefleet wave is in flight, not just
    // this exact software_id. The two waves now serialize across ticks.
    let wave_inflight: bool = if crate::auto_upgrade::is_daemon_self_software(software_id) {
        let family = crate::auto_upgrade::DAEMON_SELF_SOFTWARE;
        let patterns: Vec<String> = family
            .iter()
            .map(|id| format!("fleet-upgrade-wave/%: {id} on %"))
            .collect();
        sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM fleet_tasks
                 WHERE status IN ('pending', 'running')
                   AND summary LIKE ANY($1::text[])
            )
            "#,
        )
        .bind(&patterns)
        .fetch_one(pg)
        .await?
    } else {
        sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM fleet_tasks
                 WHERE status IN ('pending', 'running')
                   AND summary LIKE $1
            )
            "#,
        )
        .bind(format!("fleet-upgrade-wave/%: {software_id} on %"))
        .fetch_one(pg)
        .await?
    };
    if wave_inflight {
        tracing::info!(
            software_id = %software_id,
            "fleet-upgrade-wave: refusing dispatch — existing wave (this software or daemon-self family) still in flight"
        );
        return Err(sqlx::Error::Configuration(
            format!(
                "wave already in flight for software_id='{software_id}' \
                 (V62 singleton — wait for current wave to drain)"
            )
            .into(),
        ));
    }

    // HA Phase 2 drain: refuse a daemon-self upgrade wave while a leadership
    // maintenance lease is active. Restarting daemons mid-handoff is the wave
    // self-kill race the V62 singleton guards against, now spanning a leader
    // change — defer until the lease ends (it auto-fails-back), then re-compose.
    if crate::auto_upgrade::is_daemon_self_software(software_id)
        && ff_db::pg_get_active_maintenance_lease(pg)
            .await
            .unwrap_or(None)
            .is_some()
    {
        tracing::info!(
            software_id = %software_id,
            "fleet-upgrade-wave: refusing dispatch — leadership maintenance lease active (HA Phase 2 drain)"
        );
        return Err(sqlx::Error::Configuration(
            "leadership handoff in progress (maintenance lease) — daemon-self wave deferred".into(),
        ));
    }

    let mut wave_targets: Vec<WaveTarget> = Vec::with_capacity(plans.len());
    for plan in plans {
        if plan.computer_name.eq_ignore_ascii_case(&leader_lower) {
            continue;
        }
        // Stage scoping: when a filter is set, only the named members are
        // composed into this wave (the rest of the stage list is held back for
        // a later tick to compose).
        if let Some(allow) = filter_lower.as_ref()
            && !allow.contains(&plan.computer_name.to_ascii_lowercase())
        {
            continue;
        }
        let row: Option<(uuid::Uuid, String, String, i32, String)> = sqlx::query_as(
            "SELECT id, ssh_user, primary_ip, ssh_port, COALESCE(os_family, 'unknown') \
               FROM computers WHERE name = $1",
        )
        .bind(&plan.computer_name)
        .fetch_optional(pg)
        .await?;
        let Some((target_id, ssh_user, primary_ip, ssh_port, os_family)) = row else {
            tracing::warn!(
                computer = %plan.computer_name,
                "fleet-upgrade-wave: no computers row, skipping"
            );
            continue;
        };

        wave_targets.push(WaveTarget {
            target_id,
            target_name: plan.computer_name.clone(),
            ssh_user,
            primary_ip,
            ssh_port,
            os_family,
            playbook_command: plan.command,
        });
    }

    if wave_targets.is_empty() {
        return Err(sqlx::Error::Configuration(
            format!("no non-leader targets for software_id='{software_id}'").into(),
        ));
    }

    // 3. Parent task for the whole graph.
    let wave_count = wave_targets.len().div_ceil(fanout);
    let parent_summary = format!(
        "fleet-upgrade-wave: {software_id} ({n} target(s), fanout {fanout}, {wave_count} wave(s))",
        n = wave_targets.len()
    );
    let parent: uuid::Uuid = if dry_run {
        uuid::Uuid::nil()
    } else {
        sqlx::query_scalar(
            r#"
        INSERT INTO fleet_tasks (
            task_type, summary, payload, priority, created_by_computer_id
        )
        VALUES ('compound', $1, $2, 90, $3)
        RETURNING id
        "#,
        )
        .bind(&parent_summary)
        .bind(serde_json::json!({
            "software_id": software_id,
            "fanout": fanout,
            "wave_count": wave_count,
            "target_count": wave_targets.len(),
        }))
        .bind(leader_computer_id)
        .fetch_one(pg)
        .await?
    };

    // 4. Phase 1 — chunk targets into waves; one build/install shell task
    //    per (wave, target). Empty capability set = any worker may claim,
    //    so the fleet self-parallelizes as Wave 0 expands the worker
    //    pool. NO daemon restart in this phase — closes the self-kill
    //    race documented in feedback_wave_dispatcher_self_kill_race.md.
    //
    // V108 per-host dependency: capture each build's task id so the
    // corresponding restart task in Phase 2 can wait for THIS host's
    // build only, not for the slowest build in the whole batch.
    let mut build_ids_by_target: std::collections::HashMap<uuid::Uuid, uuid::Uuid> =
        std::collections::HashMap::with_capacity(wave_targets.len());
    for (wave_idx, chunk) in wave_targets.chunks(fanout).enumerate() {
        let priority = 95i32.saturating_sub((wave_idx as i32).saturating_mul(3));
        for t in chunk {
            let port_arg = if t.ssh_port == 22 {
                String::new()
            } else {
                format!(" -p {}", t.ssh_port)
            };
            // The playbook is sent over ssh stdin to a remote login
            // bash. Quoting `'FF_PLAYBOOK_EOF'` prevents the local
            // shell from expanding `$` / backticks in the playbook —
            // remote bash sees the script verbatim. `ssh -T`
            // suppresses pseudo-tty allocation so the heredoc flows
            // cleanly.
            // SSH keepalive flags are critical here — the playbook runs
            // `cargo build --release` which compiles silently for tens of
            // seconds at a stretch. With no client-side keepalive, an idle
            // SSH session can be killed by an intermediate router / firewall
            // / sshd's ClientAliveInterval, surfacing as exit=-1 to the
            // worker. Observed on 2026-05-20: 6 of 14 builds died with
            // exit=-1 at the 80-200s mark even with the V62 two-phase
            // barrier already in place. A 15s keepalive INTERVAL keeps an
            // arbitrarily long silent compile alive (the remote sshd answers
            // each probe, which resets the unanswered-probe counter); the
            // CountMax only bounds how fast ssh gives up on an UNRESPONSIVE
            // peer — see WAVE_BUILD_SSH_ALIVE_COUNT_MAX.
            // Memory-aware build (2026-05-26): self-built releases
            // (forgefleetd / ff) are heavy and OOM mid-link on memory-tight
            // hosts (≤ FREE_FOR_BUILD_RAM_GB) that have an LLM model resident —
            // sophie (32GB) and ace (16GB) failed the wave every pass and
            // couldn't self-heal. Wrap the playbook so the TARGET frees RAM
            // (snapshots + unloads its models) before the build and reloads
            // them after. `free-for-build` is a no-op on roomy hosts;
            // `resume-from-build` is on its own line (not in the playbook's
            // `&&` chain) so a failed build still restores the model. Both are
            // `|| true` so a missing `ff` on PATH degrades to current
            // behaviour rather than failing the upgrade. Only for daemon-self
            // software — package-manager upgrades are light and must not strip
            // a host's models.
            let playbook_body = build_wave_playbook_body(
                &t.playbook_command,
                crate::auto_upgrade::is_daemon_self_software(software_id),
            );
            // `-o ConnectTimeout=30`: an unreachable / packet-dropping peer
            // has NO default connect timeout, so the ssh client would hang at
            // TCP connect until the task cap. Bound it. The keepalive bound is
            // documented on WAVE_BUILD_SSH_ALIVE_COUNT_MAX.
            let command = wave_build_ssh_command(
                &t.target_name,
                &port_arg,
                &t.ssh_user,
                &t.primary_ip,
                &playbook_body,
            );

            // V61 worker-exclusion: target NEVER claims its own ff
            // upgrade. A peer always does the ssh+build. The exclusion
            // also closes the priya→priya self-ssh failure mode (worker
            // and target both being priya hits a stale known_hosts line).
            //
            // 45-min build timeout (vs default 10-min): cold cargo on the
            // slow Ubuntu hosts (beyonce, marcus) genuinely exceeds 25
            // minutes now. 2026-05-20 used 25min and 2 builds hit it;
            // 2026-05-21 wave still saw 4/14 hit the 25-min cap (ace,
            // lily, rihanna, priya) because ~15 crates landed this
            // month. 45min absorbs the new normal with headroom.
            let build_summary = format!(
                "fleet-upgrade-wave/wave{wave_idx}/build: {software_id} on {}",
                t.target_name
            );
            if dry_run {
                planned.push(PlannedTask {
                    summary: build_summary,
                    command,
                    capabilities: vec![],
                    priority,
                    timeout_secs: Some(45 * 60),
                    depends_on: None,
                });
                continue;
            }
            let build_id = pg_enqueue_shell_task_full(
                pg,
                &build_summary,
                &command,
                &[],
                None,
                Some(parent),
                priority,
                Some(leader_computer_id),
                false,
                &[t.target_id],
                Some(45 * 60),
                None,
            )
            .await?;
            build_ids_by_target.insert(t.target_id, build_id);
        }
    }

    // 5. Phase 2 — restart tasks. One per target, NO capability
    //    requirement (was `[leader]`; dropped 2026-05-22 to actually
    //    realize V108's per-host latency win).
    //
    //    V108 per-host dependency: each restart task points at its
    //    matching build via depends_on_task_id, so the restart is
    //    claimable as soon as THAT host's build is terminal.
    //
    //    Why no leader gate: leader-only restarts serialized the
    //    entire restart phase on one worker (Taylor processes one
    //    task at a time). With V108 making restarts per-host
    //    eligible the moment their build finishes, any peer can pick
    //    one up. V61 excludes the target from claiming its own
    //    restart; V62 quarantines the target while being restarted;
    //    FOR UPDATE SKIP LOCKED prevents double-claim. The leader
    //    gate was the last serialization bottleneck.
    let restart_priority = 30i32;
    for t in &wave_targets {
        let port_arg = if t.ssh_port == 22 {
            String::new()
        } else {
            format!(" -p {}", t.ssh_port)
        };
        // OS-aware restart body. Linux flavors use systemd --user;
        // macOS targets (e.g. ace) need launchctl because systemctl
        // doesn't exist there — closes the ace-restart gap that left
        // 1/14 daemons stuck on old code in the 2026-04-27 rollout.
        //
        // UID resolution on macOS: `id -u` over SSH was unreliable on
        // some hosts (returned 0 even for non-root SSH users — observed
        // on ace 2026-04-27). Derive the uid from $HOME ownership via
        // `stat -f %u`; HOME is always set correctly by sshd. Also try
        // the system domain as a last-resort fallback for daemons
        // loaded as system LaunchDaemons rather than user LaunchAgents.
        let inner_restart = if t.os_family.starts_with("macos") {
            // launchctl is the happy path when forgefleetd is registered
            // as a LaunchAgent. Some hosts (notably ace 2026-04-27)
            // never had the plist installed — bootstrap gap, same shape
            // as the missing-systemd-unit gap on Linux. Final fallback
            // is a manual pkill + nohup respawn so the restart works
            // regardless of how the daemon was originally started.
            "USER_ID=$(stat -f \"%u\" \"$HOME\" 2>/dev/null); \
             [ -z \"$USER_ID\" ] && USER_ID=$(id -u); \
             echo \"resolved USER_ID=$USER_ID HOME=$HOME\"; \
             launchctl kickstart -k \"gui/${USER_ID}/com.forgefleet.forgefleetd\" 2>/dev/null \
             || launchctl asuser \"${USER_ID}\" launchctl kickstart -k \"gui/${USER_ID}/com.forgefleet.forgefleetd\" 2>/dev/null \
             || launchctl kickstart -k \"user/${USER_ID}/com.forgefleet.forgefleetd\" 2>/dev/null \
             || launchctl kickstart -k \"system/com.forgefleet.forgefleetd\" 2>/dev/null \
             || (echo \"launchd has no registered service — falling back to pkill+nohup respawn\"; \
                 pkill -TERM -f \"$HOME/.local/bin/forgefleetd\" 2>/dev/null; \
                 sleep 1; \
                 nohup \"$HOME/.local/bin/forgefleetd\" >/tmp/forgefleetd.out 2>&1 </dev/null & \
                 disown; \
                 echo \"respawned via nohup, pid=$!\")"
                .to_string()
        } else {
            // 2026-05-24 (fix C): the standalone restart used a plain
            // blocking `systemctl --user restart ...` with stdout/stderr
            // still wired to the SSH channel. On 7 Linux hosts it never
            // returned and was killed at the 600s task cap — `restart`
            // waits for the unit to come back up, and the live SSH channel
            // keeps the session attached to the restarted daemon's output.
            // The build-embedded restart never hit this because it
            // detached. Match that pattern: `setsid` + `--no-block`
            // (enqueue the restart job and return immediately) +
            // </dev/null >/dev/null 2>&1 so nothing holds the SSH channel
            // open. The command returns as soon as the job is queued.
            "export XDG_RUNTIME_DIR=\"${XDG_RUNTIME_DIR:-/run/user/$(id -u)}\"; \
             systemctl --user reset-failed forgefleetd.service forgefleet-node.service forgefleet-daemon.service 2>/dev/null; \
             setsid systemctl --user restart --no-block forgefleetd.service </dev/null >/dev/null 2>&1 \
               || setsid systemctl --user restart --no-block forgefleet-node.service </dev/null >/dev/null 2>&1 \
               || setsid systemctl --user restart --no-block forgefleet-daemon.service </dev/null >/dev/null 2>&1; \
             echo \"restart job dispatched (--no-block, detached) on $(hostname)\""
                .to_string()
        };
        let restart_command = format!(
            "set -e\n\
             echo \"== restarting forgefleetd on {target} ({os}) via ssh from $(hostname) ==\"\n\
             ssh -T{port_arg} {ssh_bypass} -o ServerAliveInterval=15 \
                 -o ServerAliveCountMax=20 -o StrictHostKeyChecking=accept-new \
                 {ssh_user}@{primary_ip} bash -l -c '{inner}'\n",
            target = t.target_name,
            os = t.os_family,
            port_arg = port_arg,
            ssh_bypass = SSH_AGENT_BYPASS,
            ssh_user = t.ssh_user,
            primary_ip = t.primary_ip,
            inner = inner_restart.replace('\'', "'\\''"),
        );

        // V61: target excluded from claiming its own restart. With the
        // leader gate removed (2026-05-22), any peer may claim — V61's
        // target-exclusion is now the primary guard keeping a host from
        // restarting itself mid-task. V62 quarantine adds belt-and-
        // suspenders: while a peer is restarting this host, the host's
        // own task_runner refuses to claim anything.
        //
        // V108: depends_on_task_id points at THIS host's build task. The
        // restart becomes claimable as soon as that single build is
        // terminal (completed/failed/cancelled) — no longer waits for
        // every other host's build.
        let restart_summary = format!(
            "fleet-upgrade-wave/restart: {software_id} on {}",
            t.target_name
        );
        if dry_run {
            planned.push(PlannedTask {
                summary: restart_summary,
                command: restart_command,
                capabilities: vec![],
                priority: restart_priority,
                timeout_secs: None,
                depends_on: Some(format!("build: {software_id} on {}", t.target_name)),
            });
            continue;
        }
        let build_dep = build_ids_by_target.get(&t.target_id).copied();
        pg_enqueue_shell_task_full(
            pg,
            &restart_summary,
            &restart_command,
            &[],
            None,
            Some(parent),
            restart_priority,
            Some(leader_computer_id),
            false, // V108: per-host depends_on replaces global wait_for_siblings barrier
            &[t.target_id],
            None,
            build_dep,
        )
        .await?;
    }

    Ok(ComposePlan {
        parent: if dry_run { None } else { Some(parent) },
        parent_summary,
        tasks: planned,
    })
}

/// One row in [`compose_fleet_upgrade_wave`]'s working set.
struct WaveTarget {
    target_id: uuid::Uuid,
    target_name: String,
    ssh_user: String,
    primary_ip: String,
    ssh_port: i32,
    /// `linux-ubuntu`, `linux-dgx`, `macos`, etc. Used to pick the
    /// right Phase-2 restart command (systemctl vs launchctl).
    os_family: String,
    playbook_command: String,
}

/// Build the remote script the wave SSHes into each target to run
/// (`ssh ... target bash -l <<EOF <body> EOF`).
///
/// CRITICAL FD HYGIENE (2026-06-12): the body's stdout/stderr ARE the ssh
/// channel. Any process the body spawns that outlives the shell — notably a
/// model server relaunched by `resume-from-build` — inherits and HOLDS that
/// channel open, so the remote work completes (the new binary installs) but
/// `ssh` never sees EOF and the build task hangs to its 2700s cap. Its
/// dependent restart task (which keys off the build being SUCCESSFUL) then
/// never fires, so the host keeps running the old daemon. Observed live:
/// adele + lily drifted for 2+ weeks — the executor's ssh sat blocked at
/// 2287s while the remote `bash -l` was already gone and the binary already
/// installed at HEAD, with ~4 leaked `sshd@notty` sessions piled up over 14
/// days. The build itself is only ~40s; slowness was never the issue.
///
/// Fix: run the real work inside a brace group whose stdout/stderr go to a
/// remote LOG FILE and whose stdin is `/dev/null`, so nothing it spawns
/// inherits the ssh channel. The redirect is BLOCK-SCOPED (not `exec`), so
/// the parent `bash -l` keeps reading the rest of this heredoc from its own
/// stdin. The channel then carries only one trailing status line and closes
/// cleanly the instant the remote bash exits. We capture the BUILD block's
/// own exit code (not `resume-from-build`'s `|| true`, which previously
/// masked real build failures as success) and propagate it as the ssh exit
/// status so a genuine build failure fails the task instead of silently
/// "succeeding".
fn build_wave_playbook_body(playbook_command: &str, daemon_self: bool) -> String {
    let log_dir = "$HOME/.forgefleet/logs";
    let build_log = format!("{log_dir}/fleet-upgrade-wave-build.log");
    // Bound the playbook's git network transfer so a hung GitHub fetch fails
    // fast instead of leaking a stuck process — sophie accumulated 26-day-old
    // `git fetch`/`git-upload-pack` orphans from exactly this path pre-#215.
    // Covers BOTH transports the fleet uses, as plain env-var assignments so
    // it stays portable to macOS + Linux/dash with no `timeout(1)` dependency
    // (macOS ships no coreutils `timeout`):
    //   - SSH remotes (git@github.com-venkat:…): GIT_SSH_COMMAND adds a connect
    //     timeout + TCP keepalives so a silent peer is dropped in ~60s
    //     (ServerAliveInterval 15 × CountMax 4), not held forever. The added
    //     `-o` flags layer on top of the host's ssh-config alias (IdentityFile
    //     etc. still apply); BatchMode=yes turns a would-be auth prompt into a
    //     fast failure rather than a hang.
    //   - HTTPS remotes: GIT_HTTP_LOW_SPEED_LIMIT/TIME abort a transfer that
    //     trickles under 1 KB/s for 60s.
    // Build/compile steps that follow are unaffected (these only touch git).
    // `IdentityAgent=none` mirrors `crate::ssh_opts::SSH_AGENT_BYPASS`: a
    // daemon-self upgrade on a headless Linux peer inherits the wedged
    // gnome-keyring agent, which would hang `git fetch` over ssh at auth.
    let git_net_env = "export \
         GIT_SSH_COMMAND='ssh -o IdentityAgent=none -o BatchMode=yes -o ConnectTimeout=30 \
         -o ServerAliveInterval=15 -o ServerAliveCountMax=4' \
         GIT_HTTP_LOW_SPEED_LIMIT=1000 GIT_HTTP_LOW_SPEED_TIME=60; ";
    // For daemon-self upgrades, free RAM before the build and restore models
    // after — but each step is FD-isolated so a relaunched server can't hold
    // the channel. `free-for-build` runs inside the build block; `resume`
    // runs in its own redirected block AFTER we capture the build rc.
    let (free, resume) = if daemon_self {
        (
            "ff model free-for-build || true; ",
            format!(
                "{{ ff model resume-from-build || true; }} \
                 > {log_dir}/fleet-upgrade-wave-resume.log 2>&1 </dev/null\n"
            ),
        )
    } else {
        ("", String::new())
    };
    format!(
        "mkdir -p {log_dir}\n\
         {{ {git_net_env}{free}{playbook_command}; }} > {build_log} 2>&1 </dev/null\n\
         __build_rc=$?\n\
         {resume}\
         echo \"fleet-upgrade-wave: remote build rc=$__build_rc (log: {build_log})\"\n\
         exit $__build_rc"
    )
}

/// SSH `ServerAliveCountMax` for the wave's OUTER build ssh — the long-lived
/// `ssh … bash -l <<heredoc` a peer runs to drive a target's build.
///
/// With `ServerAliveInterval=15`, a *healthy* build stays connected through
/// arbitrarily long compile silence at ANY CountMax: the build's own output is
/// redirected off the channel (see [`build_wave_playbook_body`]), but the
/// remote sshd still ANSWERS every keepalive probe, and any answer resets the
/// unanswered-probe counter. So CountMax does **not** govern silence tolerance
/// (the old code set it to 120 ≈ 30 min believing it did); it only bounds how
/// fast ssh gives up on an **unresponsive / half-open** peer.
///
/// The 30-min value was harmful: when a target's build finished and its sshd
/// session tore down but the peer's TCP FIN never reached the builder (a LAN
/// blip leaving the connection half-open), the builder's ssh sat blocked for
/// the full 30 min. The build task stayed `running`, so its dependent restart
/// task (gated on the build being SUCCESSFUL) never fired within the hourly
/// wave — so the host kept the old daemon. Observed live 2026-06-13: duncan's
/// build `Finished in 50.27s`, the remote `bash -l` and sshd session were
/// gone, yet the builder's ssh was still sleeping 18 min later; duncan + ace
/// had drifted 3 releases behind for exactly this reason.
///
/// 8 × 15 s = 120 s detects a dead peer fast (unblocking the dependent restart
/// well inside the hourly wave) while never touching a healthy build, since a
/// live sshd keeps answering probes. The inner git transfer uses 4 (#219) and
/// the lighter restart ssh uses 20.
const WAVE_BUILD_SSH_ALIVE_COUNT_MAX: u32 = 8;

/// HA.2 (2026-06-14): the wave SSH runs from a daemon-spawned worker, so it
/// inherits forgefleetd's `SSH_AUTH_SOCK` — on headless Ubuntu peers that can
/// point at a wedged gnome-keyring agent that hangs ssh at auth. The bypass is
/// the canonical [`crate::ssh_opts::SSH_AGENT_BYPASS`] (see that module for the
/// full rationale and the other daemon SSH sites it covers).
use crate::ssh_opts::SSH_AGENT_BYPASS;

/// Build the OUTER ssh command the wave runs on a peer to drive `target`'s
/// build: `ssh -T … target bash -l <<EOF <playbook> EOF`. Pure (no I/O) so the
/// keepalive/timeout flags and heredoc framing are unit-testable.
fn wave_build_ssh_command(
    target_name: &str,
    port_arg: &str,
    ssh_user: &str,
    primary_ip: &str,
    playbook_body: &str,
) -> String {
    format!(
        "set -e\n\
         echo \"== upgrading {target_name} via ssh from $(hostname) ==\"\n\
         ssh -T{port_arg} {SSH_AGENT_BYPASS} -o ServerAliveInterval=15 \
             -o ServerAliveCountMax={WAVE_BUILD_SSH_ALIVE_COUNT_MAX} -o ConnectTimeout=30 \
             -o StrictHostKeyChecking=accept-new \
             {ssh_user}@{primary_ip} bash -l <<'FF_PLAYBOOK_EOF'\n\
         {playbook_body}\n\
         FF_PLAYBOOK_EOF\n",
    )
}

/// Block obviously destructive shell commands.
fn is_blocked_command(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    let blocked = [
        "rm -rf /",
        "rm -rf /*",
        "mkfs.",
        ":(){ :|:& };:",
        "dd if=/dev/zero of=/dev/sd",
        "dd if=/dev/random of=/dev/sd",
        "> /dev/sda",
        "shutdown -h",
        "shutdown now",
        "reboot",
        "halt",
        "init 0",
        "init 6",
    ];
    blocked.iter().any(|b| lower.contains(b))
}

#[cfg(test)]
mod wave_playbook_tests {
    use super::build_wave_playbook_body;

    #[test]
    fn build_block_isolates_fds_and_propagates_real_rc() {
        let body = build_wave_playbook_body("git pull && cargo build && install x", true);
        // The build work runs inside a brace group whose stdout/stderr go to
        // a log file and whose stdin is /dev/null — so no spawned process can
        // hold the ssh channel open.
        assert!(body.contains(
            "GIT_HTTP_LOW_SPEED_TIME=60; ff model free-for-build || true; \
             git pull && cargo build && install x; }"
        ));
        assert!(
            body.contains("> $HOME/.forgefleet/logs/fleet-upgrade-wave-build.log 2>&1 </dev/null")
        );
        // The git network step is bounded so a hung fetch fails fast (no leak),
        // and bypasses a wedged inherited ssh-agent on headless Linux peers.
        assert!(body.contains(
            "GIT_SSH_COMMAND='ssh -o IdentityAgent=none -o BatchMode=yes -o ConnectTimeout=30"
        ));
        assert!(body.contains("GIT_HTTP_LOW_SPEED_LIMIT=1000"));
        // It's inside the FD-isolated build block (before the playbook), so it
        // only affects this command group, not the trailing status line.
        let env_at = body.find("GIT_SSH_COMMAND=").expect("has git-net env");
        let log_at = body
            .find("fleet-upgrade-wave-build.log")
            .expect("has build log");
        assert!(env_at < log_at, "git-net env is inside the build block");
        // The redirect is block-scoped, NOT an `exec </dev/null` that would
        // truncate the heredoc the parent bash is still reading.
        assert!(!body.contains("exec </dev/null"));
        // The ssh exit status is the BUILD rc, captured BEFORE resume runs, so
        // a failed build fails the task (resume's `|| true` can't mask it).
        let rc_at = body.find("__build_rc=$?").expect("captures rc");
        let resume_at = body.find("resume-from-build").expect("has resume");
        assert!(rc_at < resume_at, "rc must be captured before resume runs");
        assert!(body.trim_end().ends_with("exit $__build_rc"));
    }

    #[test]
    fn fair_share_cap_defaults_and_env_override() {
        // Serialized via a single test so the process-global env var doesn't
        // race a sibling test. Default when unset / invalid / non-positive.
        // SAFETY: single-threaded test; no other thread reads the env here.
        unsafe {
            std::env::remove_var("FF_FAIR_SHARE_MAX_RUNNING");
            assert_eq!(
                super::fair_share_max_running(),
                super::DEFAULT_FAIR_SHARE_MAX_RUNNING
            );

            std::env::set_var("FF_FAIR_SHARE_MAX_RUNNING", "3");
            assert_eq!(super::fair_share_max_running(), 3);

            // Whitespace tolerated.
            std::env::set_var("FF_FAIR_SHARE_MAX_RUNNING", "  10 ");
            assert_eq!(super::fair_share_max_running(), 10);

            // Non-positive / garbage → default (a 0 cap would deadlock the queue).
            std::env::set_var("FF_FAIR_SHARE_MAX_RUNNING", "0");
            assert_eq!(
                super::fair_share_max_running(),
                super::DEFAULT_FAIR_SHARE_MAX_RUNNING
            );
            std::env::set_var("FF_FAIR_SHARE_MAX_RUNNING", "-4");
            assert_eq!(
                super::fair_share_max_running(),
                super::DEFAULT_FAIR_SHARE_MAX_RUNNING
            );
            std::env::set_var("FF_FAIR_SHARE_MAX_RUNNING", "lots");
            assert_eq!(
                super::fair_share_max_running(),
                super::DEFAULT_FAIR_SHARE_MAX_RUNNING
            );
            std::env::remove_var("FF_FAIR_SHARE_MAX_RUNNING");
        }
    }

    #[test]
    fn resume_is_also_fd_isolated_for_daemon_self() {
        let body = build_wave_playbook_body("true", true);
        // A model server relaunched by resume must inherit the resume log, not
        // the ssh channel.
        assert!(body.contains(
            "{ ff model resume-from-build || true; } \
                 > $HOME/.forgefleet/logs/fleet-upgrade-wave-resume.log 2>&1 </dev/null"
        ));
    }

    #[test]
    fn non_daemon_self_skips_model_pause_resume() {
        let body = build_wave_playbook_body("apt-get install -y foo", false);
        // Package upgrades are light: no model free/resume, but still
        // FD-isolated and rc-propagating.
        assert!(!body.contains("free-for-build"));
        assert!(!body.contains("resume-from-build"));
        assert!(body.contains("GIT_HTTP_LOW_SPEED_TIME=60; apt-get install -y foo; } > $HOME/.forgefleet/logs/fleet-upgrade-wave-build.log 2>&1 </dev/null"));
        assert!(body.trim_end().ends_with("exit $__build_rc"));
    }

    #[test]
    fn outer_ssh_uses_bounded_keepalive_and_frames_heredoc() {
        let cmd = super::wave_build_ssh_command(
            "duncan",
            " -p 2222",
            "duncan",
            "192.168.5.114",
            "mkdir -p x\nexit $__build_rc",
        );
        // Keepalive INTERVAL keeps a silent build alive; CountMax bounds
        // dead-peer detection. It must NOT be the old 30-min (120) value that
        // hung the build task — and thus the dependent restart — for half an
        // hour on a half-open channel.
        assert!(cmd.contains("-o ServerAliveInterval=15"));
        assert!(cmd.contains(&format!(
            "-o ServerAliveCountMax={}",
            super::WAVE_BUILD_SSH_ALIVE_COUNT_MAX
        )));
        assert!(!cmd.contains("ServerAliveCountMax=120"));
        // 15s interval × CountMax frees a half-open peer in ≤ a few minutes —
        // well inside the hourly wave — but stays > the inner git bound (4).
        assert!(super::WAVE_BUILD_SSH_ALIVE_COUNT_MAX > 4);
        assert!(super::WAVE_BUILD_SSH_ALIVE_COUNT_MAX * 15 <= 180);
        // Bounded TCP connect + non-interactive + the playbook body framed in
        // the quoted heredoc the remote login shell reads.
        assert!(cmd.contains("-o ConnectTimeout=30"));
        assert!(cmd.contains("ssh -T -p 2222 -o IdentityAgent=none -o BatchMode=yes"));
        // HA.2: ignore the inherited (possibly wedged) keyring agent + stay
        // non-interactive, else the wave hangs at auth on sophie/priya and
        // jams the whole upgrade singleton.
        assert!(cmd.contains("-o IdentityAgent=none"));
        assert!(cmd.contains("-o BatchMode=yes"));
        assert!(cmd.contains("duncan@192.168.5.114 bash -l <<'FF_PLAYBOOK_EOF'"));
        assert!(cmd.contains("\nmkdir -p x\nexit $__build_rc\n"));
        assert!(cmd.trim_end().ends_with("FF_PLAYBOOK_EOF"));
    }
}

#[cfg(test)]
mod claim_query_tests {
    /// DB-backed validation that the within-wave executor-kill guard clause
    /// (2026-05-26) parses and type-checks against the real `fleet_tasks`
    /// schema — the claim query uses runtime-checked `sqlx::query`, so a
    /// malformed clause wouldn't surface at compile time. Ignored so CI
    /// (which has no fleet pool) skips it; run on a host that has the pool:
    ///
    ///   cargo test -p ff-agent --lib -- --ignored explain_restart_executor_guard
    #[tokio::test]
    #[ignore]
    async fn explain_restart_executor_guard() {
        let pool = crate::fleet_info::get_fleet_pool()
            .await
            .expect("fleet pool");
        // EXPLAIN runs the planner = full parse + type-check, no mutation.
        sqlx::query(
            r#"
            EXPLAIN
            SELECT 1 FROM fleet_tasks t
             WHERE NOT (
               t.summary LIKE 'fleet-upgrade-wave/restart:%'
               AND EXISTS (
                 SELECT 1 FROM fleet_tasks b
                  WHERE b.status = 'running'
                    AND b.summary LIKE 'fleet-upgrade-wave/%/build:%'
                    AND b.claimed_by_computer_id IS NOT NULL
                    AND t.excludes_computer_ids
                          @> to_jsonb(ARRAY[b.claimed_by_computer_id])
               )
             )
            "#,
        )
        .fetch_all(&pool)
        .await
        .expect("restart executor-guard clause must parse + type-check");
    }
}

#[cfg(all(test, unix))]
mod shell_payload_tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn normal_command_returns_stdout_and_exit() {
        let payload = json!({ "command": "echo hello-fleet" });
        let res = run_shell_payload(&payload, &[], Duration::from_secs(10))
            .await
            .expect("ok");
        assert_eq!(res["exit"], 0);
        assert!(res["stdout"].as_str().unwrap().contains("hello-fleet"));
    }

    #[tokio::test]
    async fn injected_env_is_visible() {
        let payload = json!({ "command": "echo \"$FF_TEST_VAR\"" });
        let env = vec![("FF_TEST_VAR".to_string(), "vinny".to_string())];
        let res = run_shell_payload(&payload, &env, Duration::from_secs(10))
            .await
            .expect("ok");
        assert!(res["stdout"].as_str().unwrap().contains("vinny"));
    }

    #[tokio::test]
    async fn slow_command_times_out_promptly() {
        let payload = json!({ "command": "sleep 30" });
        let start = Instant::now();
        let res = run_shell_payload(&payload, &[], Duration::from_millis(300)).await;
        assert!(matches!(res, Err(TaskRunnerError::Timeout(_))));
        // Must return at ~max_duration, not run to completion.
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    /// THE regression test for the priya/sophie wedge: the direct shell
    /// exits immediately, but a backgrounded grandchild inherits the
    /// stdout pipe and sleeps. The old `output()` path blocked on pipe
    /// EOF for the FULL max-duration and then reaped only the (already
    /// dead) direct child, leaking the grandchild. Group-kill on timeout
    /// must (a) return promptly with Timeout and (b) leave no survivor.
    #[tokio::test]
    async fn timeout_reaps_grandchild_holding_pipe() {
        // The grandchild writes its pid to a temp file so we can probe it.
        let pidfile = std::env::temp_dir().join(format!("ff_pgkill_test_{}", std::process::id()));
        let _ = std::fs::remove_file(&pidfile);
        let cmd = format!(
            "( sleep 30 & echo $! > {p}; ) ; exit 0",
            p = pidfile.display()
        );
        let payload = json!({ "command": cmd });
        let start = Instant::now();
        let res = run_shell_payload(&payload, &[], Duration::from_millis(400)).await;
        assert!(matches!(res, Err(TaskRunnerError::Timeout(_))));
        assert!(start.elapsed() < Duration::from_secs(5));

        // The backgrounded grandchild must have been SIGKILLed with the
        // group. Give the kill a moment to land, then assert it's gone.
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Ok(pid_str) = std::fs::read_to_string(&pidfile) {
            if let Ok(pid) = pid_str.trim().parse::<i32>() {
                // signal 0 = existence probe; -1/ESRCH means it's gone.
                let alive = unsafe { libc::kill(pid, 0) } == 0;
                assert!(!alive, "grandchild pid {pid} survived group-kill");
            }
        }
        let _ = std::fs::remove_file(&pidfile);
    }
}

#[cfg(test)]
mod watchdog_threshold_tests {
    use super::*;

    /// REGRESSION GUARD (reaper bug class #589/#590): the fleet_tasks watchdog
    /// re-queues a `running` row whose `last_heartbeat_at` is older than
    /// STUCK_AFTER_SECS. That window MUST tolerate at least two missed
    /// heartbeats — otherwise a single late beat from a HEALTHY worker would
    /// hand its task to a peer, double-dispatching live work. Couple the two
    /// consts so a careless edit can't narrow the window under the cadence.
    #[test]
    fn watchdog_window_tolerates_missed_heartbeats() {
        let cadence = HEARTBEAT_EVERY.as_secs() as i64;
        assert!(
            STUCK_AFTER_SECS >= 2 * cadence,
            "STUCK_AFTER_SECS ({STUCK_AFTER_SECS}) must be >= 2x the heartbeat cadence ({cadence})"
        );
    }
}
