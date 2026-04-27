//! Session orchestrator for outcome-driven multi-LLM work.
//!
//! Walks the DAG declared in `agent_sessions` + `agent_steps` (V54),
//! dispatches each runnable step as a `fleet_tasks` shell row (the
//! existing wave-dispatcher path), reconciles the result back into the
//! step row, and advances the session toward a terminal state.
//!
//! What this module owns:
//!   - `create_session` / `add_step` — operator/CLI helpers for
//!     constructing a session + its DAG manually. (LLM-driven
//!     decomposition by the `planner` role is a follow-up PR.)
//!   - `tick` — one pass: find runnable steps, dispatch them; find
//!     just-completed fleet_tasks for running steps, fold their
//!     results back; if every step in a session is terminal, finalise
//!     the session.
//!   - `spawn` — long-lived loop calling `tick` every
//!     `TICK_INTERVAL_SECS`. Wired into the daemon main alongside the
//!     other subsystems (auto-upgrade, defer worker, etc.).
//!
//! Each step's prompt + role drive the dispatched shell command. The
//! body is `ff agent --model <role.default_model> '<prompt>'`, so the
//! existing local-LLM path runs the LLM, captures stdout, and the step
//! reads `result.stdout` as its result. Future PRs swap the body for
//! `ff run --backend <vendor>` so role-tagged steps go through the CLI
//! layers (Layer 2/3) directly.
//!
//! `requires_capability` from the role gates which fleet member runs
//! the step (per PR-A3's cap-detect extension).

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::Row;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::task_runner::pg_enqueue_shell_task;

/// How often the runner ticks. 5s is responsive without burning CPU
/// when the session pool is idle.
const TICK_INTERVAL_SECS: u64 = 5;

/// Per-tick stats for observability + tests.
#[derive(Debug, Default, Clone)]
pub struct TickStats {
    pub steps_dispatched: usize,
    pub steps_completed: usize,
    pub steps_failed: usize,
    pub sessions_finalised: usize,
}

/// Insert a new session. `goal` is the user-stated outcome; `team`
/// optionally pins specific role→model overrides
/// (`{"planner":"gpt-5","reviewer":"gemini-2.5-pro"}` etc.). Returns
/// the new session id.
pub async fn create_session(
    pool: &PgPool,
    goal: &str,
    team: Option<Value>,
    budget_usd_cap: Option<f64>,
    created_by: Option<&str>,
) -> Result<uuid::Uuid> {
    let team_json = team.unwrap_or_else(|| json!({}));
    let id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO agent_sessions (goal, team, status, budget_usd_cap, created_by)
         VALUES ($1, $2, 'pending', $3, $4)
         RETURNING id",
    )
    .bind(goal)
    .bind(team_json)
    .bind(budget_usd_cap)
    .bind(created_by)
    .fetch_one(pool)
    .await
    .context("insert agent_session")?;
    info!(session = %id, %goal, "agent session created");
    Ok(id)
}

/// Append a step to an existing session.
///
/// `role` should match a row in `agent_roles` (planner / coder /
/// reviewer / browser / synthesiser). `prompt` is the LLM input —
/// stored under `step_memory.prompt` so future prompt-rewrite logic
/// can layer in role system_prompt + brain context. `depends_on` is a
/// list of sibling step IDs that must reach a terminal state before
/// this one becomes runnable.
pub async fn add_step(
    pool: &PgPool,
    session_id: uuid::Uuid,
    name: &str,
    role: Option<&str>,
    prompt: &str,
    depends_on: &[uuid::Uuid],
) -> Result<uuid::Uuid> {
    let depends_json = json!(depends_on
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>());
    let memory = json!({ "prompt": prompt });
    let id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO agent_steps (session_id, name, role, depends_on, step_memory)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING id",
    )
    .bind(session_id)
    .bind(name)
    .bind(role)
    .bind(depends_json)
    .bind(memory)
    .fetch_one(pool)
    .await
    .context("insert agent_step")?;
    debug!(session = %session_id, step = %id, name, "agent step added");
    Ok(id)
}

/// One orchestrator pass:
///   1. For every running step whose fleet_task has reached a terminal
///      state, fold the task result into the step and mark it
///      `completed` or `failed`.
///   2. For every pending step whose dependencies have all reached
///      `completed` (or `skipped`), dispatch a fleet_task and mark
///      the step `running`.
///   3. For every session whose every step is terminal, mark the
///      session `succeeded` (all completed) or `failed` (any failed).
pub async fn tick(pool: &PgPool) -> Result<TickStats> {
    let mut stats = TickStats::default();

    // ── 1. reconcile running steps with their fleet_task outcomes ──
    let running_steps = sqlx::query(
        "SELECT s.id          AS step_id,
                s.session_id  AS session_id,
                s.fleet_task_id AS fleet_task_id,
                t.status      AS task_status,
                t.result      AS task_result,
                t.error       AS task_error
           FROM agent_steps s
           JOIN fleet_tasks t ON t.id = s.fleet_task_id
          WHERE s.status = 'running'
            AND t.status IN ('completed', 'failed', 'cancelled')",
    )
    .fetch_all(pool)
    .await
    .context("reconcile running steps")?;

    for r in running_steps {
        let step_id: uuid::Uuid = r.get("step_id");
        let task_status: String = r.get("task_status");
        let new_status = if task_status == "completed" {
            "completed"
        } else {
            "failed"
        };
        let task_result: Option<Value> = r.try_get("task_result").ok();
        let task_error: Option<String> = r.try_get("task_error").ok();
        sqlx::query(
            "UPDATE agent_steps
                SET status = $1,
                    result = $2,
                    error  = $3,
                    completed_at = NOW()
              WHERE id = $4",
        )
        .bind(new_status)
        .bind(&task_result)
        .bind(&task_error)
        .bind(step_id)
        .execute(pool)
        .await
        .context("update step terminal")?;
        if new_status == "completed" {
            stats.steps_completed += 1;
        } else {
            stats.steps_failed += 1;
        }
    }

    // ── 2. dispatch newly-runnable steps ──
    // A step is runnable when:
    //   - its session is `pending` or `running`,
    //   - its own status is `pending`,
    //   - every entry in `depends_on` references a step in
    //     ('completed', 'skipped').
    let pending = sqlx::query(
        "SELECT id, session_id, name, role, depends_on, step_memory
           FROM agent_steps
          WHERE status = 'pending'",
    )
    .fetch_all(pool)
    .await
    .context("list pending steps")?;

    for r in pending {
        let step_id: uuid::Uuid = r.get("id");
        let session_id: uuid::Uuid = r.get("session_id");
        let name: String = r.get("name");
        let role: Option<String> = r.try_get("role").ok();
        let depends_on: Value = r.try_get("depends_on").unwrap_or_else(|_| json!([]));
        let step_memory: Value = r.try_get("step_memory").unwrap_or_else(|_| json!({}));

        // Verify the parent session is still active.
        let sess_status: Option<String> =
            sqlx::query_scalar("SELECT status FROM agent_sessions WHERE id = $1")
                .bind(session_id)
                .fetch_optional(pool)
                .await
                .context("read session status")?;
        if !matches!(sess_status.as_deref(), Some("pending") | Some("running")) {
            continue;
        }

        // Check all dependencies terminal.
        let dep_ids: Vec<uuid::Uuid> = depends_on
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|v| v.as_str().and_then(|s| uuid::Uuid::parse_str(s).ok()))
            .collect();
        if !dep_ids.is_empty() {
            let row: (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM agent_steps
                  WHERE id = ANY($1)
                    AND status NOT IN ('completed', 'skipped')",
            )
            .bind(&dep_ids)
            .fetch_one(pool)
            .await
            .context("count unsatisfied deps")?;
            if row.0 > 0 {
                continue; // deps not yet terminal
            }
        }

        // Resolve role config — model + capability tag.
        let (model, capability): (String, Vec<String>) = match role.as_deref() {
            Some(r_name) => sqlx::query_as::<_, (String, Value)>(
                "SELECT default_model, requires_capability
                   FROM agent_roles WHERE name = $1 AND enabled = true",
            )
            .bind(r_name)
            .fetch_optional(pool)
            .await
            .context("read agent_role")?
            .map(|(m, caps)| {
                let cap_strs = caps
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect();
                (m, cap_strs)
            })
            .unwrap_or_else(|| ("qwen2.5-coder-32b".into(), Vec::new())),
            None => ("qwen2.5-coder-32b".into(), Vec::new()),
        };

        let prompt = step_memory
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if prompt.is_empty() {
            warn!(step = %step_id, "skipping step with empty step_memory.prompt");
            sqlx::query("UPDATE agent_steps SET status='failed', error=$1 WHERE id=$2")
                .bind("step_memory.prompt is empty")
                .bind(step_id)
                .execute(pool)
                .await
                .ok();
            stats.steps_failed += 1;
            continue;
        }

        // Encode for shell. Single-quote the prompt; replace any
        // embedded single-quote with `'\''`.
        let shell_safe = prompt.replace('\'', "'\\''");
        let cmd = format!("ff agent --model '{model}' '{shell_safe}'");

        let summary = format!(
            "agent_step: {name} (role={}, session={})",
            role.as_deref().unwrap_or("-"),
            session_id
        );

        let task_id = pg_enqueue_shell_task(
            pool,
            &summary,
            &cmd,
            &capability,
            None,
            None,
            70,
            None,
        )
        .await
        .with_context(|| format!("enqueue fleet_task for step {step_id}"))?;

        sqlx::query(
            "UPDATE agent_steps
                SET status = 'running',
                    fleet_task_id = $1,
                    started_at = NOW()
              WHERE id = $2",
        )
        .bind(task_id)
        .bind(step_id)
        .execute(pool)
        .await
        .context("mark step running")?;

        // If the session was still 'pending', flip to 'running' on
        // first dispatch.
        sqlx::query(
            "UPDATE agent_sessions
                SET status = 'running', started_at = COALESCE(started_at, NOW())
              WHERE id = $1 AND status = 'pending'",
        )
        .bind(session_id)
        .execute(pool)
        .await
        .ok();

        stats.steps_dispatched += 1;
        info!(session = %session_id, step = %step_id, %name, %model, "dispatched");
    }

    // ── 3. finalise sessions whose every step is terminal ──
    // A session is finalisable when no step is in `pending` or
    // `running`. Outcome = `succeeded` if every step is `completed` or
    // `skipped`; else `failed`.
    let finalisable = sqlx::query(
        "SELECT s.id
           FROM agent_sessions s
          WHERE s.status = 'running'
            AND NOT EXISTS (
                SELECT 1 FROM agent_steps st
                 WHERE st.session_id = s.id
                   AND st.status IN ('pending', 'running')
            )",
    )
    .fetch_all(pool)
    .await
    .context("list finalisable sessions")?;

    for r in finalisable {
        let sid: uuid::Uuid = r.get("id");
        let any_failed: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM agent_steps
              WHERE session_id = $1 AND status = 'failed'",
        )
        .bind(sid)
        .fetch_one(pool)
        .await
        .context("count failed steps")?;
        let outcome = if any_failed.0 > 0 { "failed" } else { "succeeded" };
        sqlx::query(
            "UPDATE agent_sessions
                SET status = $1, completed_at = NOW()
              WHERE id = $2",
        )
        .bind(outcome)
        .bind(sid)
        .execute(pool)
        .await
        .ok();
        info!(session = %sid, %outcome, "session finalised");
        stats.sessions_finalised += 1;
    }

    Ok(stats)
}

/// Spawn the long-lived runner. Idempotent — multiple processes may
/// call this; each one polls and races at the SQL UPDATE level (the
/// `WHERE status = 'pending'` claims are not yet atomic; concurrent
/// runners may double-dispatch a step. For now we run a single
/// instance on the leader, same as the watchdog).
pub fn spawn(pool: PgPool, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match tick(&pool).await {
                Ok(s) if s.steps_dispatched + s.steps_completed + s.steps_failed + s.sessions_finalised > 0 => {
                    info!(
                        dispatched = s.steps_dispatched,
                        completed = s.steps_completed,
                        failed = s.steps_failed,
                        finalised = s.sessions_finalised,
                        "session_runner tick"
                    );
                }
                Ok(_) => {}
                Err(e) => warn!(error = %e, "session_runner tick error"),
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(TICK_INTERVAL_SECS)) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
            }
        }
    })
}

/// Write a session-scoped memory entry. Roles within a session use
/// this to share structured findings without polluting each other's
/// context windows. JSONB value lets roles pass structured data
/// (lists, citations, code blocks) across steps.
///
/// Idempotent: if `key` already exists for the session, it's
/// overwritten (last-write-wins).
pub async fn brain_set(
    pool: &PgPool,
    session_id: uuid::Uuid,
    key: &str,
    value: &Value,
    written_by_role: Option<&str>,
    written_by_step: Option<uuid::Uuid>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO session_brain
            (session_id, key, value, written_by_role, written_by_step, written_at)
         VALUES ($1, $2, $3, $4, $5, NOW())
         ON CONFLICT (session_id, key) DO UPDATE SET
            value = EXCLUDED.value,
            written_by_role = EXCLUDED.written_by_role,
            written_by_step = EXCLUDED.written_by_step,
            written_at = NOW()",
    )
    .bind(session_id)
    .bind(key)
    .bind(value)
    .bind(written_by_role)
    .bind(written_by_step)
    .execute(pool)
    .await
    .context("session_brain set")?;
    Ok(())
}

/// Read a session-scoped memory entry. `None` if not set.
pub async fn brain_get(
    pool: &PgPool,
    session_id: uuid::Uuid,
    key: &str,
) -> Result<Option<Value>> {
    let row: Option<Value> = sqlx::query_scalar(
        "SELECT value FROM session_brain WHERE session_id = $1 AND key = $2",
    )
    .bind(session_id)
    .bind(key)
    .fetch_optional(pool)
    .await
    .context("session_brain get")?;
    Ok(row)
}

/// List every session_brain entry for a session, newest first. Useful
/// when a role needs to see "what does the team know so far?" before
/// generating its own contribution.
pub async fn brain_list(
    pool: &PgPool,
    session_id: uuid::Uuid,
) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT key, value, written_by_role, written_by_step, written_at
           FROM session_brain
          WHERE session_id = $1
          ORDER BY written_at DESC",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await
    .context("session_brain list")?;
    Ok(rows
        .into_iter()
        .map(|r| {
            json!({
                "key":             r.get::<String, _>("key"),
                "value":           r.try_get::<Value, _>("value").unwrap_or(json!(null)),
                "written_by_role": r.try_get::<String, _>("written_by_role").ok(),
                "written_by_step": r.try_get::<uuid::Uuid, _>("written_by_step").ok(),
                "written_at":      r.get::<chrono::DateTime<chrono::Utc>, _>("written_at"),
            })
        })
        .collect())
}

/// Helper for `ff session list` — return one row per session with
/// progress counters.
pub async fn list_sessions(pool: &PgPool, limit: i64) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT s.id, s.goal, s.status, s.created_at, s.completed_at,
                COUNT(st.id) FILTER (WHERE st.status = 'completed') AS done,
                COUNT(st.id) FILTER (WHERE st.status = 'failed')    AS failed,
                COUNT(st.id)                                        AS total
           FROM agent_sessions s
           LEFT JOIN agent_steps st ON st.session_id = s.id
          GROUP BY s.id
          ORDER BY s.created_at DESC
          LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("list sessions")?;
    Ok(rows
        .into_iter()
        .map(|r| {
            json!({
                "id":            r.get::<uuid::Uuid, _>("id"),
                "goal":          r.get::<String, _>("goal"),
                "status":        r.get::<String, _>("status"),
                "created_at":    r.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
                "completed_at":  r.try_get::<chrono::DateTime<chrono::Utc>, _>("completed_at").ok(),
                "steps_done":    r.get::<i64, _>("done"),
                "steps_failed":  r.get::<i64, _>("failed"),
                "steps_total":   r.get::<i64, _>("total"),
            })
        })
        .collect())
}

/// Helper for `ff session get <id>` — full session + step list.
pub async fn get_session(pool: &PgPool, id: uuid::Uuid) -> Result<Value> {
    let s_row = sqlx::query(
        "SELECT id, goal, team, status, budget_usd_cap, cost_usd_so_far,
                final_result, error, created_at, started_at, completed_at, created_by
           FROM agent_sessions WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("read session")?
    .ok_or_else(|| anyhow!("session not found: {id}"))?;

    let steps = sqlx::query(
        "SELECT id, name, role, depends_on, step_memory, status,
                fleet_task_id, result, error, retry_count,
                created_at, started_at, completed_at
           FROM agent_steps
          WHERE session_id = $1
          ORDER BY created_at",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .context("read session steps")?;

    Ok(json!({
        "id":            s_row.get::<uuid::Uuid, _>("id"),
        "goal":          s_row.get::<String, _>("goal"),
        "team":          s_row.try_get::<Value, _>("team").unwrap_or(json!({})),
        "status":        s_row.get::<String, _>("status"),
        "final_result":  s_row.try_get::<Value, _>("final_result").ok(),
        "error":         s_row.try_get::<String, _>("error").ok(),
        "created_at":    s_row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
        "started_at":    s_row.try_get::<chrono::DateTime<chrono::Utc>, _>("started_at").ok(),
        "completed_at":  s_row.try_get::<chrono::DateTime<chrono::Utc>, _>("completed_at").ok(),
        "steps": steps.into_iter().map(|r| json!({
            "id":           r.get::<uuid::Uuid, _>("id"),
            "name":         r.get::<String, _>("name"),
            "role":         r.try_get::<String, _>("role").ok(),
            "depends_on":   r.try_get::<Value, _>("depends_on").unwrap_or(json!([])),
            "step_memory":  r.try_get::<Value, _>("step_memory").unwrap_or(json!({})),
            "status":       r.get::<String, _>("status"),
            "fleet_task_id":r.try_get::<uuid::Uuid, _>("fleet_task_id").ok(),
            "result":       r.try_get::<Value, _>("result").ok(),
            "error":        r.try_get::<String, _>("error").ok(),
            "retry_count":  r.get::<i32, _>("retry_count"),
            "created_at":   r.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
            "started_at":   r.try_get::<chrono::DateTime<chrono::Utc>, _>("started_at").ok(),
            "completed_at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("completed_at").ok(),
        })).collect::<Vec<_>>(),
    }))
}
