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

        // Resolve role config — model + capability tag. Per-step
        // `step_memory.model_override` wins over the role default,
        // letting `ff session vote` ship N voters that all share a
        // role but use different models.
        let (mut model, capability): (String, Vec<String>) = match role.as_deref() {
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
        if let Some(override_model) = step_memory
            .get("model_override")
            .and_then(Value::as_str)
        {
            model = override_model.to_string();
        }

        let raw_prompt = step_memory
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if raw_prompt.is_empty() {
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

        // Pre-session brain context injection: pull top-K relevant
        // vault entries for this prompt and prepend them as a system
        // preamble. Closes the "each role re-derives everything from
        // scratch" gap noted in PR-E's deferred work. Skipped silently
        // when the brain has no matches or the query fails.
        let brain_context = gather_brain_context(pool, &raw_prompt).await.unwrap_or_default();
        let prompt_with_context = if brain_context.is_empty() {
            raw_prompt.clone()
        } else {
            format!(
                "<system>\n## Relevant context from your shared brain\n\n{brain_context}\n</system>\n\n<user>\n{raw_prompt}\n</user>"
            )
        };

        // Encode for shell. Single-quote the prompt; replace any
        // embedded single-quote with `'\''`.
        let shell_safe = prompt_with_context.replace('\'', "'\\''");
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

        // Mirror this session's findings into the vault. Per V13's
        // "AI writes only to Inbox" design, all session artefacts go
        // under `Inbox/sessions/<session-id>/`. Operator promotes from
        // there. Errors are non-fatal — the session is already
        // finalised in the DB, the mirror is a nice-to-have.
        if let Err(e) = mirror_session_to_vault(pool, sid).await {
            warn!(session = %sid, error = %e, "session vault mirror failed (non-fatal)");
        }
    }

    Ok(stats)
}

/// Copy a finalised session's brain entries + step results into the
/// Obsidian vault as markdown files. Layout:
///
///   <vault>/Inbox/sessions/<session-id>/
///     ├── _summary.md           — session metadata + step list + outcome
///     ├── brain-<key>.md        — one per session_brain entry
///     └── step-<step-name>.md   — one per step's stdout
///
/// Each file has frontmatter with `source: "ff-session"`,
/// `session_id`, `role` etc. so future brain promotions know where
/// the entry came from.
async fn mirror_session_to_vault(pool: &PgPool, session_id: uuid::Uuid) -> Result<()> {
    let vault = match resolve_vault_root_for_session(pool).await {
        Some(v) => v,
        None => {
            debug!("no vault configured; skipping session mirror");
            return Ok(());
        }
    };

    let dir = vault
        .join("Inbox")
        .join("sessions")
        .join(session_id.to_string());
    std::fs::create_dir_all(&dir).context("create session inbox dir")?;

    // Pull session metadata + steps + brain entries.
    let session_row = sqlx::query(
        "SELECT goal, team, status, error, created_at, completed_at
           FROM agent_sessions WHERE id = $1",
    )
    .bind(session_id)
    .fetch_one(pool)
    .await
    .context("read session for mirror")?;
    let goal: String = session_row.get("goal");
    let status: String = session_row.get("status");
    let error: Option<String> = session_row.try_get("error").ok();

    // _summary.md — session-level overview.
    let summary = format!(
        "---\n\
         source: ff-session\n\
         session_id: {session_id}\n\
         status: {status}\n\
         finalized_at: {}\n\
         ---\n\n\
         # Session: {goal}\n\n\
         **Status**: {status}\n\n\
         {}",
        chrono::Utc::now().to_rfc3339(),
        error.map(|e| format!("**Error**: {e}\n\n")).unwrap_or_default()
    );
    std::fs::write(dir.join("_summary.md"), summary).context("write session summary")?;

    // Per-step files.
    let steps = sqlx::query(
        "SELECT s.id, s.name, s.role, s.status, t.result
           FROM agent_steps s
           LEFT JOIN fleet_tasks t ON t.id = s.fleet_task_id
          WHERE s.session_id = $1
          ORDER BY s.created_at",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await
    .context("read session steps for mirror")?;
    for s in steps {
        let name: String = s.get("name");
        let role: Option<String> = s.try_get("role").ok();
        let step_status: String = s.get("status");
        let result: Option<Value> = s.try_get("result").ok();
        let stdout = result
            .as_ref()
            .and_then(|r| r.get("stdout"))
            .and_then(Value::as_str)
            .unwrap_or("(no stdout captured)");
        let safe_name = sanitize_filename(&name);
        let body = format!(
            "---\n\
             source: ff-session\n\
             session_id: {session_id}\n\
             step_name: {name}\n\
             role: {}\n\
             status: {step_status}\n\
             ---\n\n\
             # Step: {name}\n\n\
             ```\n{stdout}\n```\n",
            role.as_deref().unwrap_or("-")
        );
        std::fs::write(dir.join(format!("step-{safe_name}.md")), body)
            .context("write step file")?;
    }

    // session_brain entries.
    let brain = sqlx::query(
        "SELECT key, value, written_by_role, written_at
           FROM session_brain WHERE session_id = $1
          ORDER BY written_at",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await
    .context("read session_brain for mirror")?;
    for b in brain {
        let key: String = b.get("key");
        let value: Value = b.try_get("value").unwrap_or_else(|_| json!(null));
        let by_role: Option<String> = b.try_get("written_by_role").ok();
        let safe_key = sanitize_filename(&key);
        let body = format!(
            "---\n\
             source: ff-session\n\
             session_id: {session_id}\n\
             brain_key: {key}\n\
             written_by_role: {}\n\
             ---\n\n\
             # Brain entry: {key}\n\n\
             ```json\n{}\n```\n",
            by_role.as_deref().unwrap_or("-"),
            serde_json::to_string_pretty(&value).unwrap_or_default(),
        );
        std::fs::write(dir.join(format!("brain-{safe_key}.md")), body)
            .context("write brain file")?;
    }

    info!(session = %session_id, dir = %dir.display(), "session mirrored to vault Inbox");
    Ok(())
}

/// Pull up to 5 relevant `brain_vault_nodes` for `prompt` and format
/// them as a markdown bullet list. Tokenises the prompt by whitespace
/// (simple but works for keyword-style retrieval), then matches each
/// token against `title`, `path`, and `tags` arrays. Returns empty
/// string if nothing matches — the caller skips the preamble in that
/// case.
async fn gather_brain_context(pool: &PgPool, prompt: &str) -> Result<String> {
    let tokens: Vec<String> = prompt
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 4)
        .map(|t| t.to_lowercase())
        .filter(|t| {
            // Stop-words a single-line stop list. Same idea as the
            // existing brain_search MCP tool — drops the most common
            // tokens to focus the match on substantive ones.
            !matches!(
                t.as_str(),
                "this" | "that" | "with" | "from" | "have" | "what" | "when"
                    | "where" | "would" | "should" | "could" | "their" | "there"
                    | "them" | "then" | "than" | "they" | "your" | "yours"
                    | "into" | "over" | "under" | "above" | "below" | "about"
                    | "after" | "before" | "between" | "during" | "while"
            )
        })
        .take(8)
        .collect();
    if tokens.is_empty() {
        return Ok(String::new());
    }

    // Build a query that matches any of the tokens against title or
    // path (case-insensitive) or tags. Limit 5; only currently-valid
    // entries (`valid_until IS NULL`).
    let pattern: Vec<String> = tokens.iter().map(|t| format!("%{t}%")).collect();
    let rows = sqlx::query(
        "SELECT path, title,
                COALESCE(array_to_string(tags, ', '), '') AS tagstr
           FROM brain_vault_nodes
          WHERE valid_until IS NULL
            AND (
              EXISTS (
                SELECT 1 FROM unnest($1::text[]) AS pat
                 WHERE LOWER(title) LIKE pat OR LOWER(path) LIKE pat
              )
              OR tags && $2::text[]
            )
          ORDER BY hits DESC, last_accessed DESC
          LIMIT 5",
    )
    .bind(&pattern)
    .bind(&tokens)
    .fetch_all(pool)
    .await
    .context("brain context query")?;

    if rows.is_empty() {
        return Ok(String::new());
    }
    let mut out = String::new();
    for r in rows {
        let path: String = r.get("path");
        let title: String = r.get("title");
        let tags: String = r.try_get("tagstr").unwrap_or_default();
        let snippet = if tags.is_empty() {
            format!("- **{title}** (`{path}`)\n")
        } else {
            format!("- **{title}** (`{path}`) — tags: {tags}\n")
        };
        out.push_str(&snippet);
    }
    Ok(out)
}

async fn resolve_vault_root_for_session(pool: &PgPool) -> Option<std::path::PathBuf> {
    let from_secrets = ff_db::pg_get_secret(pool, "brain.vault_path")
        .await
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    let raw = from_secrets.unwrap_or_else(|| "~/projects/Yarli_KnowledgeBase".into());
    if let Some(rest) = raw.strip_prefix("~/") {
        Some(dirs::home_dir()?.join(rest))
    } else {
        Some(std::path::PathBuf::from(raw))
    }
}

fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect::<String>()
        .chars()
        .take(120)
        .collect()
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

/// Add a parallel vote: N voter steps (each running the same prompt
/// on a different model) plus a tally step depending on all of them.
/// When the tally step runs, its LLM is asked to read the voter
/// answers and pick the consensus, writing the result into
/// `session_brain[vote_<step_name>]`.
///
/// Returns `(voter_ids, tally_id)`.
///
/// Each voter is a model name (`claude-opus-4-7`, `gpt-5`,
/// `gemini-2.5-pro`, `qwen2.5-coder-32b`, etc.). The orchestrator
/// reads `step_memory.model_override` to dispatch with that model
/// regardless of role.
pub async fn create_vote(
    pool: &PgPool,
    session_id: uuid::Uuid,
    step_name: &str,
    prompt: &str,
    voter_models: &[String],
    tally_role: Option<&str>,
) -> Result<(Vec<uuid::Uuid>, uuid::Uuid)> {
    if voter_models.len() < 2 {
        return Err(anyhow!("a vote needs at least 2 voters"));
    }

    // Insert voter steps. Each gets the same prompt but a distinct
    // model_override.
    let mut voter_ids = Vec::with_capacity(voter_models.len());
    for (i, model) in voter_models.iter().enumerate() {
        let voter_name = format!("{step_name}/voter-{i}-{model}");
        let memory = json!({
            "prompt": prompt,
            "model_override": model,
            "vote_step": step_name,
            "voter_index": i,
        });
        let id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO agent_steps (session_id, name, role, depends_on, step_memory)
             VALUES ($1, $2, NULL, '[]'::jsonb, $3)
             RETURNING id",
        )
        .bind(session_id)
        .bind(&voter_name)
        .bind(memory)
        .fetch_one(pool)
        .await
        .context("insert voter step")?;
        voter_ids.push(id);
    }

    // Tally step: depends on all voters; LLM reads their stdouts and
    // picks consensus. We embed the voter step IDs in step_memory so
    // the tally prompt can reference them at dispatch time.
    let tally_prompt = format!(
        "You are tallying a multi-LLM vote on the question:\n\n\
         '{prompt}'\n\n\
         {} voters answered. Read each answer below, identify the consensus, \
         and emit JSON of shape:\n\
         {{\n  \"chosen\": \"…\",\n  \"reasoning\": \"why you picked this consensus\",\n  \"agreement\": \"unanimous|majority|split\"\n}}\n\n\
         The orchestrator will fetch each voter's stdout from `agent_steps.result.stdout` \
         and present them in order. For now, output the JSON shape based on the prompt alone — \
         in a future PR the tally step's pre-context will inject the voter answers automatically.",
        voter_models.len()
    );
    let tally_role = tally_role.unwrap_or("synthesiser");
    let tally_id = add_step(
        pool,
        session_id,
        &format!("{step_name}/tally"),
        Some(tally_role),
        &tally_prompt,
        &voter_ids,
    )
    .await?;

    info!(
        session = %session_id,
        step_name = %step_name,
        voters = voter_models.len(),
        "created vote step graph"
    );
    Ok((voter_ids, tally_id))
}

/// Read the completed voter steps for a vote-style step group and
/// store the per-voter answers into session_brain under
/// `vote_<step_name>`. Useful as a follow-up after the tally step
/// runs — surfaces the raw voter answers for operator review.
pub async fn collect_vote_answers(
    pool: &PgPool,
    session_id: uuid::Uuid,
    step_name: &str,
) -> Result<Value> {
    let rows = sqlx::query(
        "SELECT s.id, s.name, s.step_memory, t.result
           FROM agent_steps s
           LEFT JOIN fleet_tasks t ON t.id = s.fleet_task_id
          WHERE s.session_id = $1
            AND s.name LIKE $2
            AND s.status = 'completed'
          ORDER BY (s.step_memory->>'voter_index')::int",
    )
    .bind(session_id)
    .bind(format!("{step_name}/voter-%"))
    .fetch_all(pool)
    .await
    .context("read voter step results")?;

    let answers: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            let mem: Value = r.try_get("step_memory").unwrap_or_else(|_| json!({}));
            let model = mem
                .get("model_override")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let result: Option<Value> = r.try_get("result").ok();
            let stdout = result
                .as_ref()
                .and_then(|v| v.get("stdout"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .take(8192)
                .collect::<String>();
            json!({
                "model": model,
                "stdout": stdout,
            })
        })
        .collect();

    let snapshot = json!({
        "step_name": step_name,
        "voter_count": answers.len(),
        "answers": answers,
        "collected_at": chrono::Utc::now().to_rfc3339(),
    });

    brain_set(
        pool,
        session_id,
        &format!("vote_{step_name}"),
        &snapshot,
        Some("system"),
        None,
    )
    .await?;

    Ok(snapshot)
}

/// Add a planner step to a session — the planner role decomposes the
/// session's goal into a concrete step DAG. The dispatched LLM is
/// asked to emit JSON; a follow-up `apply_plan` reads the completed
/// step's stdout and inserts children accordingly.
///
/// Two-step flow lets the operator inspect the plan before committing
/// (some plans are bad / missing constraints; review-before-apply
/// catches that).
pub async fn add_planner_step(
    pool: &PgPool,
    session_id: uuid::Uuid,
) -> Result<uuid::Uuid> {
    let goal: String = sqlx::query_scalar("SELECT goal FROM agent_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(pool)
        .await
        .context("read session goal")?;

    let prompt = format!(
        "You are decomposing a user goal into a concrete plan for a multi-LLM team \
         (planner / coder / reviewer / browser / synthesiser).\n\n\
         User goal: {goal}\n\n\
         Output ONLY a JSON object of this shape, with no commentary or markdown fences:\n\
         {{\n  \
           \"steps\": [\n    \
             {{\n      \"name\": \"step name\",\n      \"role\": \"coder|reviewer|browser|synthesiser|planner\",\n      \"prompt\": \"what this step's LLM should do\",\n      \"depends_on\": [\"name of an earlier step\", ...]\n    }}\n  ]\n\
         }}\n\n\
         Rules:\n- 3-7 steps total.\n- depends_on uses step names (not UUIDs); the orchestrator resolves them.\n- The last step should typically be a synthesiser that combines the team's findings.\n- Keep prompts specific and actionable."
    );

    add_step(pool, session_id, "plan", Some("planner"), &prompt, &[]).await
}

/// Read the most recent completed planner step in a session, parse its
/// stdout as the planner JSON, and insert the planned children as
/// agent_steps rows. Names in `depends_on` are resolved against
/// previously-added steps in this session.
///
/// If the planner output isn't valid JSON or doesn't match the
/// expected shape, returns an error rather than corrupting the DAG —
/// operator inspects via `ff session get`.
pub async fn apply_plan(
    pool: &PgPool,
    session_id: uuid::Uuid,
    planner_step_id: Option<uuid::Uuid>,
) -> Result<Vec<uuid::Uuid>> {
    // Resolve which planner step's output to consume.
    let step_id = match planner_step_id {
        Some(id) => id,
        None => sqlx::query_scalar(
            "SELECT id FROM agent_steps
              WHERE session_id = $1
                AND role = 'planner'
                AND status = 'completed'
              ORDER BY completed_at DESC NULLS LAST
              LIMIT 1",
        )
        .bind(session_id)
        .fetch_optional(pool)
        .await
        .context("find latest completed planner step")?
        .ok_or_else(|| anyhow!(
            "no completed planner step found for session {session_id}; run `ff session plan` first and wait for it to finish"
        ))?,
    };

    // The fleet_task result has shape {exit, stdout, stderr}. Parse the
    // stdout as JSON; that's the planner's plan.
    let result: Option<Value> = sqlx::query_scalar(
        "SELECT t.result
           FROM agent_steps s
           JOIN fleet_tasks t ON t.id = s.fleet_task_id
          WHERE s.id = $1",
    )
    .bind(step_id)
    .fetch_optional(pool)
    .await
    .context("read planner step result")?;
    let result = result.ok_or_else(|| anyhow!("planner step has no fleet_task result"))?;
    let stdout = result
        .get("stdout")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("planner step result missing stdout"))?;

    // Try strict JSON first; fall back to extracting from a fenced
    // code block (some LLMs ignore the no-fences instruction).
    let plan_json: Value = serde_json::from_str(stdout.trim())
        .or_else(|_| {
            // Look for the first {…} block.
            let s = stdout
                .find('{')
                .and_then(|start| {
                    let end = stdout.rfind('}')?;
                    if end > start {
                        Some(&stdout[start..=end])
                    } else {
                        None
                    }
                })
                .ok_or_else(|| anyhow!("no JSON object found in planner output"))?;
            serde_json::from_str::<Value>(s).context("parse fallback JSON")
        })?;

    let steps_arr = plan_json
        .get("steps")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("planner output missing 'steps' array"))?;

    // Two-pass: first insert without deps to get IDs, then update
    // deps once we know name → id mapping. Simpler: assume the
    // planner emits steps in a topo-sort-friendly order and insert
    // sequentially, resolving deps from prior names.
    let mut name_to_id: std::collections::HashMap<String, uuid::Uuid> =
        std::collections::HashMap::new();
    let mut inserted = Vec::with_capacity(steps_arr.len());
    for v in steps_arr {
        let name = v
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("planned step missing 'name'"))?;
        let role = v.get("role").and_then(Value::as_str);
        let prompt = v
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("planned step '{name}' missing 'prompt'"))?;
        let dep_names = v
            .get("depends_on")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let dep_ids: Vec<uuid::Uuid> = dep_names
            .iter()
            .filter_map(|n| n.as_str())
            .filter_map(|n| name_to_id.get(n).copied())
            .collect();
        let id = add_step(pool, session_id, name, role, prompt, &dep_ids).await?;
        name_to_id.insert(name.to_string(), id);
        inserted.push(id);
    }
    info!(session = %session_id, count = inserted.len(), "applied planner plan");
    Ok(inserted)
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
