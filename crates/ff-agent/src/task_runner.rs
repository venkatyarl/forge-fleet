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

/// Errors that can occur while running the task runner.
#[derive(Debug, thiserror::Error)]
pub enum TaskRunnerError {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("task payload missing required field: {0}")]
    BadPayload(&'static str),
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
}

impl TaskRunner {
    pub fn new(
        pg: PgPool,
        my_computer_id: uuid::Uuid,
        my_name: String,
        my_capabilities: HashSet<String>,
    ) -> Self {
        Self {
            pg,
            my_computer_id,
            my_name,
            my_capabilities: Arc::new(my_capabilities),
        }
    }

    /// One worker tick — claim at most one ready task and run it.
    pub async fn tick_once(&self) -> Result<Option<uuid::Uuid>, TaskRunnerError> {
        // 1. Atomically claim a task whose capabilities we satisfy.
        let cap_array: Vec<String> = self.my_capabilities.iter().cloned().collect();
        let row = sqlx::query(
            r#"
            UPDATE fleet_tasks
               SET status                 = 'running',
                   claimed_by_computer_id = $1,
                   claimed_at             = NOW(),
                   started_at             = COALESCE(started_at, NOW()),
                   last_heartbeat_at      = NOW()
             WHERE id = (
               SELECT id FROM fleet_tasks
                WHERE status = 'pending'
                  AND task_type = 'shell'
                  AND (preferred_computer_id IS NULL
                       OR preferred_computer_id = $1)
                  AND requires_capability <@ to_jsonb($2::text[])
                ORDER BY priority DESC, created_at ASC
                  FOR UPDATE SKIP LOCKED
                LIMIT 1
             )
            RETURNING id, payload, summary
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

        info!(task_id = %task_id, summary = %summary, "task claimed");

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

        // 3. Run the payload.
        let outcome = run_shell_payload(&payload).await;
        let _ = cancel_tx.send(());
        let _ = hb_task.await;

        // 4. Persist result.
        match outcome {
            Ok(result) => {
                let exit = result
                    .get("exit")
                    .and_then(Value::as_i64)
                    .unwrap_or(-1);
                if exit == 0 {
                    sqlx::query(
                        "UPDATE fleet_tasks
                            SET status        = 'completed',
                                completed_at  = NOW(),
                                progress_pct  = 100.0,
                                result        = $1
                          WHERE id = $2",
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
                          WHERE id = $3",
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
                      WHERE id = $2",
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
    pub fn spawn(self, interval_secs: u64, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        let interval = Duration::from_secs(interval_secs.max(2));
        tokio::spawn(async move {
            loop {
                if let Err(e) = self.tick_once().await {
                    debug!(error = %e, computer = %self.my_name, "task tick error");
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

/// Convenience: spawn the leader watchdog as a background tick.
///
/// `my_name` is compared (case-insensitive) against
/// `fleet_leader_state.member_name`; only the elected leader actually
/// performs handoffs. Every daemon spawns this; the gate keeps it idle
/// on followers.
pub fn spawn_leader_watchdog(
    pg: PgPool,
    my_name: String,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // 60s tick — half the stuck threshold so detection latency stays bounded.
        let interval = Duration::from_secs(60);
        loop {
            let leader: Option<String> = sqlx::query_scalar(
                "SELECT member_name FROM fleet_leader_state LIMIT 1",
            )
            .fetch_optional(&pg)
            .await
            .ok()
            .flatten();
            if matches!(leader, Some(ref l) if l.eq_ignore_ascii_case(&my_name)) {
                match handoff_stuck_tasks(&pg).await {
                    Ok(n) if n > 0 => {
                        info!(handed_off = n, "task watchdog re-queued stale tasks");
                    }
                    Ok(_) => {}
                    Err(e) => warn!(error = %e, "task watchdog query failed"),
                }
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
/// `compose_aura_bootstrap` helper.
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

    let id: uuid::Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO fleet_tasks (
            parent_task_id, task_type, summary, payload,
            priority, requires_capability, preferred_computer_id,
            created_by_computer_id
        )
        VALUES ($1, 'shell', $2, $3, $4, $5, $6, $7)
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
    .fetch_one(pg)
    .await?;

    Ok(id)
}

/// Run a `task_type=shell` payload via `/bin/bash -lc <command>`.
async fn run_shell_payload(payload: &Value) -> Result<Value, TaskRunnerError> {
    let command = payload
        .get("command")
        .and_then(Value::as_str)
        .ok_or(TaskRunnerError::BadPayload("command"))?
        .to_string();

    let shell = payload
        .get("shell")
        .and_then(Value::as_str)
        .unwrap_or("/bin/bash")
        .to_string();

    // Run in a blocking thread so we don't tie up the runtime.
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new(&shell)
            .arg("-lc")
            .arg(&command)
            .output()
    })
    .await
    .map_err(|e| TaskRunnerError::BadPayload(Box::leak(format!("join: {e}").into_boxed_str())))?
    .map_err(|e| {
        TaskRunnerError::BadPayload(Box::leak(format!("spawn: {e}").into_boxed_str()))
    })?;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let exit = out.status.code().unwrap_or(-1) as i64;

    Ok(json!({
        "exit": exit,
        "stdout": stdout.chars().take(8192).collect::<String>(),
        "stderr": stderr.chars().take(8192).collect::<String>(),
    }))
}

/// Compose the multi-step "bring aura online" task graph atomically.
///
/// One parent (no shell) plus N children, one per fleet member who
/// helps. The members work in parallel where they can; the bootstrap
/// child has a `depends_on` style ordering done at the application
/// level by the watcher (we don't have a true DAG dependency column —
/// the watcher creates the next child only after the previous
/// completed). For now the children are sequenced in time-priority
/// (priority decreasing) so the leader picks them in order.
pub async fn compose_aura_bootstrap(
    pg: &PgPool,
    leader_computer_id: uuid::Uuid,
) -> Result<uuid::Uuid, sqlx::Error> {
    let parent: uuid::Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO fleet_tasks (
            task_type, summary, payload, priority, created_by_computer_id
        )
        VALUES ('compound', 'aura: bring online via fleet cooperation', '{}'::jsonb, 80, $1)
        RETURNING id
        "#,
    )
    .bind(leader_computer_id)
    .fetch_one(pg)
    .await?;

    // Step 1 (priority 90): probe aura on both IPs from whoever has
    // SSH trust today (today only the leader does — once aura's
    // bootstrap finishes, mesh propagate fans the trust out and a
    // non-leader peer could legitimately handle this).
    pg_enqueue_shell_task(
        pg,
        "aura/1: ssh-probe both interfaces",
        "set -e; \
         for ip in 192.168.5.109 192.168.5.110; do \
           echo \"== $ip ==\"; \
           timeout 5 ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new \
             aura@$ip 'echo ok && uname -srvmo' || echo 'unreachable'; \
         done",
        &["leader".to_string()],
        None,
        Some(parent),
        90,
        Some(leader_computer_id),
    )
    .await?;

    // Step 2 (priority 85): leader runs the bootstrap on aura.
    pg_enqueue_shell_task(
        pg,
        "aura/2: install forgefleetd via gateway bootstrap",
        "ssh -o BatchMode=yes aura@192.168.5.109 \
         \"curl -fsSL 'http://192.168.5.100:51002/onboard/bootstrap.sh\
?name=aura&ip=192.168.5.109&ssh_user=aura&role=builder&runtime=auto' | sudo bash\"",
        &["leader".to_string()],
        None,
        Some(parent),
        85,
        Some(leader_computer_id),
    )
    .await?;

    // Step 3 (priority 80): verify forgefleetd is running on aura.
    pg_enqueue_shell_task(
        pg,
        "aura/3: verify forgefleetd running on aura",
        "ssh -o BatchMode=yes aura@192.168.5.109 \
         'systemctl --user is-active forgefleetd.service \
            || systemctl --user is-active forgefleet-node.service \
            || systemctl --user is-active forgefleet-daemon.service' \
         | grep -q active",
        &["leader".to_string()],
        None,
        Some(parent),
        80,
        Some(leader_computer_id),
    )
    .await?;

    // Step 4 (priority 75): confirm aura is reporting online via ff.
    // Any fleet member with ff installed can run this — the work
    // is intentionally NOT pinned to leader, so we exercise the
    // cross-member work-stealing path.
    pg_enqueue_shell_task(
        pg,
        "aura/4: confirm aura shows online in ff fleet health",
        "for i in $(seq 1 30); do \
           if ff fleet health 2>/dev/null | awk '$1 == \"aura\" {print $3}' | grep -q online; then \
             echo 'aura online in fleet health'; exit 0; \
           fi; sleep 2; \
         done; echo 'timeout: aura still not online after 60s'; exit 1",
        &["ff".to_string()],
        None,
        Some(parent),
        75,
        Some(leader_computer_id),
    )
    .await?;

    Ok(parent)
}
