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
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use sqlx::postgres::PgListener;
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
                  AND t.task_type IN ('shell', 'decomposed')
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
                  -- set, the referenced task must be terminal. This is
                  -- what lets wave-restart fire per-host instead of
                  -- waiting for every build sibling.
                  AND (
                    t.depends_on_task_id IS NULL
                    OR EXISTS (
                      SELECT 1 FROM fleet_tasks dep
                       WHERE dep.id = t.depends_on_task_id
                         AND dep.status IN ('completed', 'failed', 'cancelled')
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
        .fetch_optional(&self.pg)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let task_id: uuid::Uuid = row.get("id");
        let payload: Value = row.get("payload");
        let summary: String = row.get("summary");
        let task_type: String = row.get("task_type");
        let timeout_secs: Option<i32> = row.get("timeout_secs");

        info!(task_id = %task_id, summary = %summary, task_type = %task_type, "task claimed");

        // ── Decomposed task: create work items and return ──────────────────
        if task_type == "decomposed" {
            let items: Vec<Value> = payload
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let num_batches = payload
                .get("num_batches")
                .and_then(Value::as_u64)
                .unwrap_or(1) as usize;

            let mut weighted_items = Vec::new();
            for item in &items {
                let key = item
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let item_type = item
                    .get("item_type")
                    .and_then(Value::as_str)
                    .unwrap_or("shell")
                    .to_string();
                let weight = crate::batch_manager::ItemWeight {
                    base: item
                        .get("base_weight")
                        .and_then(Value::as_f64)
                        .unwrap_or(1.0),
                    pages: item.get("pages").and_then(Value::as_f64).unwrap_or(0.0),
                    words: item.get("words").and_then(Value::as_f64).unwrap_or(0.0),
                    has_images: item
                        .get("has_images")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0),
                    has_code: item.get("has_code").and_then(Value::as_f64).unwrap_or(0.0),
                };
                weighted_items.push((key, item_type, weight));
            }

            if weighted_items.is_empty() {
                warn!(task_id = %task_id, "decomposed task has no items — failing immediately");
                sqlx::query(
                    "UPDATE fleet_tasks SET status = 'failed', completed_at = NOW(), error = $1 WHERE id = $2"
                )
                .bind("decomposed task payload has no items")
                .bind(task_id)
                .execute(&self.pg)
                .await?;
                return Ok(Some(task_id));
            }

            match crate::batch_manager::create_work_items(
                &self.pg,
                task_id,
                &weighted_items,
                num_batches,
            )
            .await
            {
                Ok(batches) => {
                    info!(task_id = %task_id, batches = batches.len(), "decomposed task into work items");
                }
                Err(e) => {
                    warn!(task_id = %task_id, error = %e, "failed to create work items");
                    sqlx::query(
                        "UPDATE fleet_tasks SET status = 'failed', completed_at = NOW(), error = $1 WHERE id = $2"
                    )
                    .bind(format!("decomposition failed: {e}"))
                    .bind(task_id)
                    .execute(&self.pg)
                    .await?;
                    return Ok(Some(task_id));
                }
            }

            // Keep task in running state; completion watcher will mark it
            // done when all work_items finish. Heartbeat is bumped by the
            // watcher every 30s so the watchdog doesn't re-queue us.
            sqlx::query(
                "UPDATE fleet_tasks SET progress_message = 'waiting for work items', progress_pct = 0.0 WHERE id = $1"
            )
            .bind(task_id)
            .execute(&self.pg)
            .await?;

            return Ok(Some(task_id));
        }

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
        let outcome = match tokio::time::timeout(
            max_duration,
            run_shell_payload(&payload, &self.env),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(TaskRunnerError::BadPayload(format!(
                "task exceeded max duration of {}s",
                max_duration.as_secs()
            ))),
        };
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
    /// Phase 3 (V77): Uses PostgreSQL LISTEN/NOTIFY to wake immediately
    /// when a new task is inserted.  A 10s fallback interval remains for
    /// resilience if NOTIFY is lost or the DB connection drops.
    pub fn spawn(self, interval_secs: u64, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        let interval = Duration::from_secs(interval_secs.max(2));
        tokio::spawn(async move {
            loop {
                if let Err(e) = self.tick_once().await {
                    debug!(error = %e, computer = %self.my_name, "task tick error");
                }
                tokio::select! {
                    result = listen_for_tasks(&self.pg) => {
                        if let Err(e) = result {
                            debug!(error = %e, "task listener error, falling back to interval");
                        } else {
                            debug!("woken by fleet_task_inserted notification");
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

/// Wait for a `fleet_task_inserted` NOTIFY on a dedicated connection.
/// Returns when a notification is received so the caller can call
/// `tick_once` immediately.
async fn listen_for_tasks(pg: &PgPool) -> Result<(), sqlx::Error> {
    let mut listener = PgListener::connect_with(pg).await?;
    listener.listen("fleet_task_inserted").await?;
    // Block until at least one notification arrives.
    let _ = listener.recv().await?;
    Ok(())
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

    Ok(id)
}

/// Run a `task_type=shell` payload via `/bin/bash -lc <command>`.
/// `env` is injected on top of the inherited daemon env — these are
/// the `FF_*` values resolved from the DB at worker startup.
async fn run_shell_payload(
    payload: &Value,
    env: &[(String, String)],
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
            "shell '{}' is not in the allowed list: {:?}",
            shell, ALLOWED_SHELLS
        )));
    }

    // Use tokio::process so the child can actually be killed if the
    // outer Future is dropped (e.g. by tokio::time::timeout). The
    // `kill_on_drop(true)` flag makes the runtime SIGKILL the child
    // when the Child handle is dropped — closes the
    // heartbeat-fresh-but-stuck-forever class of bug documented in
    // feedback_priya_worker_hangs_with_fresh_heartbeat.md.
    let mut cmd = tokio::process::Command::new(&shell);
    cmd.arg("-lc").arg(&command);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.kill_on_drop(true);

    let out = cmd
        .output()
        .await
        .map_err(|e| TaskRunnerError::BadPayload(format!("spawn: {e}")))?;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let exit = out.status.code().unwrap_or(-1) as i64;

    Ok(json!({
        "exit": exit,
        "stdout": stdout.chars().take(8192).collect::<String>(),
        "stderr": stderr.chars().take(8192).collect::<String>(),
    }))
}

/// Compose the multi-step "bring `<target>` online" task graph atomically.
///
/// Every node-specific value (IPs, ssh user, role) and every fleet
/// constant (gateway port) is read from the DB at compose time — no
/// IPs, names, usernames, or port numbers in source. Ports come from
/// `fleet_secrets` (seeded by V50: `port.gateway`, `port.openclaw`, …)
/// so an operator can change one row to change a port fleet-wide
/// without a recompile.
///
/// One parent (compound, no shell) plus N children, one per
/// cooperative step. Children are sequenced by descending priority so
/// the leader picks them in order; for a true DAG with edges we'd add
/// a `depends_on` column to V44.
pub async fn compose_node_bootstrap(
    pg: &PgPool,
    target_name: &str,
    leader_computer_id: uuid::Uuid,
) -> Result<uuid::Uuid, sqlx::Error> {
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
    let parent: uuid::Uuid = sqlx::query_scalar(
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
    .await?;

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
        "set -e; for ip in {ip_list}; do \
           echo \"== $ip ==\"; \
           timeout 5 ssh{port_arg} -o BatchMode=yes -o StrictHostKeyChecking=accept-new \
             {ssh_target_for_iter} 'echo ok && uname -srvmo' || echo 'unreachable'; \
         done"
    );
    pg_enqueue_shell_task(
        pg,
        &format!("{target_name}/1: ssh-probe all known IPs"),
        &step1,
        &["leader".to_string()],
        None,
        Some(parent),
        90,
        Some(leader_computer_id),
    )
    .await?;

    // ── Step 2: leader runs the bootstrap script. ────────────────────────
    let step2 = format!(
        "ssh{port_arg} -o BatchMode=yes {ssh_target_primary} \
         \"curl -fsSL '{gateway_url}/onboard/bootstrap.sh\
?name={target_name}&ip={target_primary_ip}&ssh_user={target_ssh_user}&role=builder&runtime=auto' \
         | sudo bash\""
    );
    pg_enqueue_shell_task(
        pg,
        &format!("{target_name}/2: install forgefleetd via gateway bootstrap"),
        &step2,
        &["leader".to_string()],
        None,
        Some(parent),
        85,
        Some(leader_computer_id),
    )
    .await?;

    // ── Step 3: verify forgefleetd active. ───────────────────────────────
    // Unit names are project-fixed deploy artifacts (see deploy/linux/
    // forgefleet.service and revive.rs fallback chain) — keeping them
    // here is consistent with treating them as constants. `grep -qx`
    // (exact line match) avoids the bug where 'inactive' matches.
    // macOS uses launchctl rather than systemctl; gate the check.
    let step3 = if target_os_family == "macos" {
        format!(
            "ssh{port_arg} -o BatchMode=yes {ssh_target_primary} \
             'launchctl list | grep -E \"com\\.forgefleet\\.(forgefleetd|daemon)\" >/dev/null'"
        )
    } else {
        format!(
            "ssh{port_arg} -o BatchMode=yes {ssh_target_primary} \
             'systemctl --user is-active forgefleetd.service \
                || systemctl --user is-active forgefleet-node.service \
                || systemctl --user is-active forgefleet-daemon.service \
                || systemctl --user is-active forgefleet-agent.service' \
             | grep -qx active"
        )
    };
    pg_enqueue_shell_task(
        pg,
        &format!("{target_name}/3: verify forgefleetd running on {target_name}"),
        &step3,
        &["leader".to_string()],
        None,
        Some(parent),
        80,
        Some(leader_computer_id),
    )
    .await?;

    // ── Step 4: confirm online via ff (any peer with ff). ────────────────
    let step4 = format!(
        "for i in $(seq 1 30); do \
           if ff fleet health 2>/dev/null | awk '$1 == \"{target_name}\" {{print $3}}' \
              | grep -qx online; then \
             echo '{target_name} online in fleet health'; exit 0; \
           fi; sleep 2; \
         done; echo 'timeout: {target_name} still not online after 60s'; exit 1"
    );
    pg_enqueue_shell_task(
        pg,
        &format!("{target_name}/4: confirm {target_name} shows online in ff fleet health"),
        &step4,
        &["ff".to_string()],
        None,
        Some(parent),
        75,
        Some(leader_computer_id),
    )
    .await?;

    Ok(parent)
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
) -> Result<uuid::Uuid, sqlx::Error> {
    use crate::auto_upgrade::resolve_upgrade_plans_with_suffix;

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
    let wave_inflight: bool = sqlx::query_scalar(
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
    .await?;
    if wave_inflight {
        tracing::info!(
            software_id = %software_id,
            "fleet-upgrade-wave: refusing dispatch — existing wave for this software still in flight"
        );
        return Err(sqlx::Error::Configuration(
            format!(
                "wave already in flight for software_id='{software_id}' \
                 (V62 singleton — wait for current wave to drain)"
            )
            .into(),
        ));
    }

    let mut wave_targets: Vec<WaveTarget> = Vec::with_capacity(plans.len());
    for plan in plans {
        if plan.computer_name.eq_ignore_ascii_case(&leader_lower) {
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
    let parent: uuid::Uuid = sqlx::query_scalar(
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
    .await?;

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
            // barrier already in place. Keepalive at 15s with 120 retries
            // tolerates up to 30 minutes of pure compile silence.
            let command = format!(
                "set -e\n\
                 echo \"== upgrading {target} via ssh from $(hostname) ==\"\n\
                 ssh -T{port_arg} -o BatchMode=yes -o ServerAliveInterval=15 \
                     -o ServerAliveCountMax=120 -o StrictHostKeyChecking=accept-new \
                     {ssh_user}@{primary_ip} bash -l <<'FF_PLAYBOOK_EOF'\n\
                 {playbook}\n\
                 FF_PLAYBOOK_EOF\n",
                target = t.target_name,
                port_arg = port_arg,
                ssh_user = t.ssh_user,
                primary_ip = t.primary_ip,
                playbook = t.playbook_command,
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
            let build_id = pg_enqueue_shell_task_full(
                pg,
                &format!(
                    "fleet-upgrade-wave/wave{wave_idx}/build: {software_id} on {}",
                    t.target_name
                ),
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
            "export XDG_RUNTIME_DIR=\"${XDG_RUNTIME_DIR:-/run/user/$(id -u)}\"; \
             systemctl --user reset-failed forgefleetd.service forgefleet-node.service forgefleet-daemon.service 2>/dev/null; \
             systemctl --user restart forgefleetd.service || systemctl --user restart forgefleet-node.service || systemctl --user restart forgefleet-daemon.service"
                .to_string()
        };
        let restart_command = format!(
            "set -e\n\
             echo \"== restarting forgefleetd on {target} ({os}) via ssh from $(hostname) ==\"\n\
             ssh -T{port_arg} -o BatchMode=yes -o ServerAliveInterval=15 \
                 -o ServerAliveCountMax=20 -o StrictHostKeyChecking=accept-new \
                 {ssh_user}@{primary_ip} bash -l -c '{inner}'\n",
            target = t.target_name,
            os = t.os_family,
            port_arg = port_arg,
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
        let build_dep = build_ids_by_target.get(&t.target_id).copied();
        pg_enqueue_shell_task_full(
            pg,
            &format!(
                "fleet-upgrade-wave/restart: {software_id} on {}",
                t.target_name
            ),
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

    Ok(parent)
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
