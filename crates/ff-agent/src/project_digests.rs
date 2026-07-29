//! Per-project Telegram digest framework (native ff, 2026-07-25).
//!
//! Replaces the single hardcoded ForgeFleet status tick with a data-driven
//! framework: one **updatable** config row per project in
//! `project_digest_configs`. A single leader-gated tick walks the enabled
//! configs and sends each project its own digest — scoped to that project's
//! `work_items` — carrying the project's logo (via `sendPhoto`).
//!
//! Design goals from the operator (2026-07-25):
//!   * ONE framework, not a new work-item/tick per project. Update the row,
//!     the operator gets the latest — old duplicate senders are removed.
//!   * Every registered project gets its own native digest (ForgeFleet,
//!     HireFlow360, ...), each with its own logo.
//!   * TEMPORARY task-scoped digests may be added; they auto-expire 15 minutes
//!     after the task they track completes, then stop sending on their own.
//!
//! The config table IS the source of truth: to change what the operator
//! receives, `UPDATE project_digest_configs` — no code change, no new tick.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};
use uuid::Uuid;

/// How long a temporary task-digest keeps sending after its task completes.
const TEMP_DIGEST_LINGER_SECS: i64 = 15 * 60;

#[async_trait]
trait DigestSender: Send + Sync {
    async fn send(
        &self,
        pg: &PgPool,
        title: &str,
        body: &str,
        logo: Option<&[u8]>,
    ) -> crate::telegram::TelegramDigestOutcome;
}

struct TelegramDigestSender;

#[async_trait]
impl DigestSender for TelegramDigestSender {
    async fn send(
        &self,
        pg: &PgPool,
        title: &str,
        body: &str,
        logo: Option<&[u8]>,
    ) -> crate::telegram::TelegramDigestOutcome {
        crate::telegram::send_telegram_digest_classified(pg, title, body, logo).await
    }
}

/// Idempotent schema + seed. Creates `project_digest_configs` if absent and
/// seeds a standing digest for each active project that doesn't already have
/// one. `ON CONFLICT DO NOTHING` means operator edits (title, interval,
/// enabled, logo) are never clobbered on restart. Logos are set separately
/// (operator-updatable), so a fresh seed has `logo_png = NULL` and the digest
/// still sends as text until a logo is attached.
pub async fn ensure_schema(pg: &PgPool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS project_digest_configs (\
            id                text PRIMARY KEY,\
            project_id        text NOT NULL,\
            kind              text NOT NULL DEFAULT 'standing',\
            title             text NOT NULL,\
            enabled           boolean NOT NULL DEFAULT true,\
            interval_secs     integer NOT NULL DEFAULT 900,\
            logo_png          bytea,\
            task_work_item_id text,\
            expires_at        timestamptz,\
            last_sent_at      timestamptz,\
            created_at        timestamptz NOT NULL DEFAULT now(),\
            updated_at        timestamptz NOT NULL DEFAULT now()\
        )",
    )
    .execute(pg)
    .await?;

    // `logo_path` links each digest to the project's logo file on disk
    // (under ~/projects/<project>/...), so ff knows where the source logo is
    // and can re-render it. `logo_png` caches the rendered/resized bytes sent
    // to Telegram.
    sqlx::query("ALTER TABLE project_digest_configs ADD COLUMN IF NOT EXISTS logo_path text")
        .execute(pg)
        .await?;

    // Bootstrap parity with ff-db V283.  Daemons historically owned this
    // feature's bootstrap, so an upgraded agent must be safe even when it
    // starts before the standalone migration runner.
    sqlx::raw_sql(ff_db::schema::SCHEMA_V283_PROJECT_DIGEST_ATTEMPTS)
        .execute(pg)
        .await?;

    // Seed a standing PROJECT digest for every active project (id
    // 'proj:standing'). These report that project's work_items/tasks. Title =
    // an emoji + display_name so each project reads distinctly.
    sqlx::query(
        "INSERT INTO project_digest_configs (id, project_id, kind, title, interval_secs) \
         SELECT p.id || ':standing', p.id, 'standing', \
                '🚀 ' || coalesce(nullif(p.display_name, ''), p.id), 900 \
           FROM projects p \
          WHERE p.status = 'active' \
            AND EXISTS (SELECT 1 FROM work_items w WHERE w.project_id = p.id) \
         ON CONFLICT (id) DO NOTHING",
    )
    .execute(pg)
    .await?;

    // Seed the SYSTEM digest ('ff:system'): the status of ForgeFleet-the-
    // platform ITSELF running — new models discovered, self-improvement, fleet
    // health, deployments, cloud headroom — as opposed to project work. This is
    // the distinction the operator drew: "ForgeFleet" = the project & its tasks;
    // "ff" = the platform's own operational status.
    sqlx::query(
        "INSERT INTO project_digest_configs (id, project_id, kind, title, interval_secs) \
         VALUES ('ff:system', 'ff', 'system', '⚙️ ff — platform status', 900) \
         ON CONFLICT (id) DO NOTHING",
    )
    .execute(pg)
    .await?;

    // Retire the stray internal 'ff-agent-dispatch' project digest — its role
    // is subsumed by the 'ff' system digest above.
    sqlx::query(
        "UPDATE project_digest_configs SET enabled = false, updated_at = now() \
          WHERE id = 'ff-agent-dispatch:standing'",
    )
    .execute(pg)
    .await?;

    Ok(())
}

/// Register (or refresh) a TEMPORARY digest that tracks a single task. It sends
/// on the same interval as standing digests but automatically expires
/// [`TEMP_DIGEST_LINGER_SECS`] after the tracked work item completes. Safe to
/// call repeatedly for the same task (idempotent upsert).
pub async fn upsert_temporary_task_digest(
    pg: &PgPool,
    project_id: &str,
    work_item_id: &str,
    title: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO project_digest_configs \
            (id, project_id, kind, title, interval_secs, task_work_item_id) \
         VALUES ($1, $2, 'temporary', $3, 900, $4) \
         ON CONFLICT (id) DO UPDATE SET title = EXCLUDED.title, \
            enabled = true, expires_at = NULL, updated_at = now()",
    )
    .bind(format!("task:{work_item_id}"))
    .bind(project_id)
    .bind(title)
    .bind(work_item_id)
    .execute(pg)
    .await?;
    Ok(())
}

/// Spawn the per-project digest tick. Runs on every daemon but leader-gates
/// itself so exactly one digest per project per interval goes out. `check_secs`
/// is how often the tick wakes to look for due/expiring digests (e.g. 60s);
/// each config's own `interval_secs` controls its send cadence.
pub fn spawn_project_digests_tick(
    pg: PgPool,
    check_secs: u64,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(err) = ensure_schema(&pg).await {
            warn!(error = %err, "project digests: ensure_schema failed");
        }
        let mut ticker = tokio::time::interval(Duration::from_secs(check_secs.max(1)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !crate::leader_cache::is_current_leader() {
                        continue;
                    }
                    if let Err(err) = run_once(&pg).await {
                        warn!(error = %err, "project digests tick failed");
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        info!("project digests tick shutting down");
                        break;
                    }
                }
            }
        }
    })
}

/// One pass: age temporary digests toward expiry, disable expired ones, then
/// send every enabled config that is due.
async fn run_once(pg: &PgPool) -> Result<()> {
    run_once_with_sender(pg, &TelegramDigestSender).await
}

async fn run_once_with_sender(pg: &PgPool, sender: &dyn DigestSender) -> Result<()> {
    // Keep the per-project session-of-record ("workstream") seeded — one row per
    // active project. Leader-gated (we're inside the leader-only tick), idempotent
    // and cheap. This is the foundation for ff-owns-the-session: clients attach to
    // their project's workstream while ff does the backend work.
    if let Err(err) = crate::workstreams::ensure_all_workstreams(pg).await {
        warn!(error = %err, "workstreams: ensure_all failed");
    }
    // Auto-derive each UNATTENDED project's working_summary from live work_item
    // activity, so the session-of-record reflects reality without a session
    // manually reporting. A live session's own report (last 15 min) always wins.
    match crate::workstreams::derive_working_summaries(pg).await {
        Ok(n) if n > 0 => info!(updated = n, "workstreams: auto-derived working summaries"),
        Ok(_) => {}
        Err(err) => warn!(error = %err, "workstreams: derive summaries failed"),
    }

    // 1) Start the death-clock: a temporary digest whose tracked task has
    //    completed and that has no expiry yet gets `expires_at = now() +
    //    linger`. It keeps sending until then, so the operator sees the
    //    finished-task summary for a while, then it stops on its own.
    sqlx::query(
        "UPDATE project_digest_configs c \
            SET expires_at = now() + make_interval(secs => $1), updated_at = now() \
          FROM work_items w \
         WHERE c.kind = 'temporary' AND c.enabled AND c.expires_at IS NULL \
           AND c.task_work_item_id = w.id::text \
           AND w.status IN ('done','merged','failed','cancelled')",
    )
    .bind(TEMP_DIGEST_LINGER_SECS as f64)
    .execute(pg)
    .await?;

    // 2) Disable temporaries past their expiry (they've lingered long enough).
    sqlx::query(
        "UPDATE project_digest_configs \
            SET enabled = false, updated_at = now() \
          WHERE kind = 'temporary' AND enabled \
            AND expires_at IS NOT NULL AND expires_at < now()",
    )
    .execute(pg)
    .await?;

    // 3) Find due configs plus every unfinished attempt. The latter is
    // independent of the mutable cursor: retryability belongs to the frozen
    // attempt, not to project_digest_configs.last_sent_at. `sending` is itself
    // permanently fail-closed after a crash; there is intentionally no timer
    // that can race an active multi-message Telegram delivery.
    let due: Vec<(
        String,
        String,
        String,
        Option<Vec<u8>>,
        Option<DateTime<Utc>>,
        DateTime<Utc>,
    )> = sqlx::query_as(
        "SELECT id, project_id, title, logo_png, last_sent_at, clock_timestamp() \
           FROM project_digest_configs c \
          WHERE (c.enabled AND (c.last_sent_at IS NULL \
                 OR now() - c.last_sent_at >= make_interval(secs => c.interval_secs))) \
             OR EXISTS (SELECT 1 FROM project_digest_attempts a \
                         WHERE a.config_id=c.id AND a.delivery_status<>'delivered') \
          ORDER BY kind, project_id",
    )
    .fetch_all(pg)
    .await
    .unwrap_or_default();

    for (id, project_id, title, logo, prior_cursor, window_end) in due {
        type Attempt = (
            Option<DateTime<Utc>>,
            DateTime<Utc>,
            DateTime<Utc>,
            String,
            String,
            Option<Vec<u8>>,
            String,
            String,
        );
        // An unfinished attempt owns the config until it is confirmed. This is
        // what freezes both the payload/key and window across every retry.
        let attempt: Attempt = if let Some(existing) = sqlx::query_as(
            "SELECT prior_cursor,cursor_at,window_end,title,body,logo_png,delivery_key,delivery_status \
               FROM project_digest_attempts WHERE config_id=$1 AND delivery_status<>'delivered' \
              ORDER BY created_at LIMIT 1",
        )
        .bind(&id)
        .fetch_optional(pg)
        .await?
        {
            existing
        } else {
            let cursor_at = prior_cursor.unwrap_or(window_end - chrono::Duration::hours(24));
            let body =
                match build_project_digest_window(pg, &project_id, cursor_at, window_end).await {
                    Ok(b) => b,
                    Err(err) => {
                        warn!(project = %project_id, error = %err, "project digest build failed");
                        continue;
                    }
                };
            let delivery_key = digest_delivery_key(&id, cursor_at, window_end);
            let inserted: Option<Attempt> = sqlx::query_as(
                "INSERT INTO project_digest_attempts \
                (config_id, prior_cursor, cursor_at, window_end, title, body, logo_png, delivery_key) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8) \
                 ON CONFLICT DO NOTHING \
                 RETURNING prior_cursor,cursor_at,window_end,title,body,logo_png,delivery_key,delivery_status",
            )
            .bind(&id)
            .bind(prior_cursor)
            .bind(cursor_at)
            .bind(window_end)
            .bind(&title)
            .bind(&body)
            .bind(&logo)
            .bind(&delivery_key)
            .fetch_optional(pg)
            .await?;
            if let Some(inserted) = inserted {
                inserted
            } else {
                // A concurrent leader/handoff may have frozen a different
                // window first. Reuse that exact payload/key instead of
                // failing the whole tick or creating a replacement attempt.
                sqlx::query_as(
                    "SELECT prior_cursor,cursor_at,window_end,title,body,logo_png,delivery_key,delivery_status \
                       FROM project_digest_attempts \
                      WHERE config_id=$1 AND delivery_status<>'delivered' \
                      ORDER BY created_at LIMIT 1",
                )
                .bind(&id)
                .fetch_one(pg)
                .await?
            }
        };

        if !matches!(attempt.7.as_str(), "prepared" | "retryable") {
            continue;
        }

        let fence = Uuid::new_v4();
        let claimed_attempt: Option<i64> = sqlx::query_scalar(
            "UPDATE project_digest_attempts \
                SET delivery_status='sending', attempt=attempt+1, fence=$4, \
                    last_error=NULL, error_at=NULL, updated_at=now() \
              WHERE config_id=$1 AND cursor_at=$2 AND window_end=$3 \
                AND delivery_status IN ('prepared','retryable') \
             RETURNING attempt",
        )
        .bind(&id)
        .bind(attempt.1)
        .bind(attempt.2)
        .bind(fence)
        .fetch_optional(pg)
        .await?;
        let Some(claimed_attempt) = claimed_attempt else {
            continue;
        };

        let outcome = match sender
            .send(pg, &attempt.3, &attempt.4, attempt.5.as_deref())
            .await
        {
            crate::telegram::TelegramDigestOutcome::Acknowledged { messages }
                if messages.is_empty() =>
            {
                crate::telegram::TelegramDigestOutcome::Ambiguous {
                    error: "Telegram reported success without a message identity".into(),
                }
            }
            outcome => outcome,
        };
        match outcome {
            crate::telegram::TelegramDigestOutcome::Acknowledged { messages } => {
                let acknowledgement = serde_json::to_value(
                    &messages
                        .iter()
                        .map(|message| {
                            serde_json::json!({
                                "chat_id": message.chat_id,
                                "message_id": message.message_id,
                            })
                        })
                        .collect::<Vec<_>>(),
                )?;
                let first = messages.first();
                let mut tx = pg.begin().await?;
                let delivered = sqlx::query(
                    "UPDATE project_digest_attempts \
                        SET delivery_status='delivered', delivered_at=now(), \
                            acknowledgement=$6, ack_chat_id=$7, ack_message_id=$8, \
                            last_error=NULL, error_at=NULL, updated_at=now() \
                      WHERE config_id=$1 AND cursor_at=$2 AND window_end=$3 \
                        AND delivery_status='sending' AND attempt=$4 AND fence=$5",
                )
                .bind(&id)
                .bind(attempt.1)
                .bind(attempt.2)
                .bind(claimed_attempt)
                .bind(fence)
                .bind(acknowledgement)
                .bind(first.map(|message| message.chat_id.as_str()))
                .bind(first.map(|message| message.message_id))
                .execute(&mut *tx)
                .await?
                .rows_affected();
                if delivered == 1 {
                    // A later legacy cursor wins, but can never leave this
                    // acknowledged attempt unfinished or regress the cursor.
                    sqlx::query(
                        "UPDATE project_digest_configs \
                            SET last_sent_at=greatest(coalesce(last_sent_at,$2),$2), updated_at=now() \
                          WHERE id=$1",
                    )
                    .bind(&id)
                    .bind(attempt.2)
                    .execute(&mut *tx)
                    .await?;
                }
                tx.commit().await?;
            }
            crate::telegram::TelegramDigestOutcome::DefinitelyNotDelivered { error } => {
                let changed = sqlx::query(
                    "UPDATE project_digest_attempts \
                        SET delivery_status='retryable', last_error=$6, error_at=now(), updated_at=now() \
                      WHERE config_id=$1 AND cursor_at=$2 AND window_end=$3 \
                        AND delivery_status='sending' AND attempt=$4 AND fence=$5",
                )
                .bind(&id)
                .bind(attempt.1)
                .bind(attempt.2)
                .bind(claimed_attempt)
                .bind(fence)
                .bind(&error)
                .execute(pg)
                .await?
                .rows_affected();
                if changed == 1 {
                    warn!(project = %project_id, %error, "project digest definitely not delivered; retryable");
                }
            }
            crate::telegram::TelegramDigestOutcome::Ambiguous { error } => {
                let changed = sqlx::query(
                    "UPDATE project_digest_attempts \
                        SET delivery_status='ambiguous', last_error=$6, error_at=now(), updated_at=now() \
                      WHERE config_id=$1 AND cursor_at=$2 AND window_end=$3 \
                        AND delivery_status='sending' AND attempt=$4 AND fence=$5",
                )
                .bind(&id)
                .bind(attempt.1)
                .bind(attempt.2)
                .bind(claimed_attempt)
                .bind(fence)
                .bind(&error)
                .execute(pg)
                .await?
                .rows_affected();
                if changed == 1 {
                    warn!(project = %project_id, %error, "project digest delivery ambiguous; failing closed");
                }
            }
        }
    }
    Ok(())
}

/// Extract a human LLM/backend name from a lease endpoint like
/// "lane1.5:local:qwen3-coder-480b" → "qwen3-coder-480b", "codex" → "codex".
/// Empty endpoint (not yet dispatched to a backend) → "pending".
#[allow(dead_code)] // superseded by the in-query {node}:{model} resolution
fn llm_from_endpoint(ep: &str) -> String {
    let ep = ep.trim();
    // Empty endpoint = a local Lane-1 codegen build that didn't record its exact
    // model on the lease (telemetry gap). The item IS building locally — label it
    // "local", not "pending" (which read as stuck).
    if ep.is_empty() {
        return "local".to_string();
    }
    ep.rsplit(':').next().unwrap_or(ep).to_string()
}

/// Compact minutes → "Xm" / "Yh" / "Yh Zm" for readable durations.
fn fmt_mins(m: i64) -> String {
    let m = m.max(0);
    if m < 60 {
        format!("{m}m")
    } else if m % 60 == 0 {
        format!("{}h", m / 60)
    } else {
        format!("{}h {}m", m / 60, m % 60)
    }
}

fn digest_delivery_key(
    config_id: &str,
    cursor_at: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> String {
    let mut hash = Sha256::new();
    hash.update(config_id.as_bytes());
    hash.update([0]);
    hash.update(cursor_at.timestamp_micros().to_be_bytes());
    hash.update(window_end.timestamp_micros().to_be_bytes());
    format!("project-digest:{:x}", hash.finalize())
}

/// Build one project's digest body, scoped to that project's `work_items`.
/// Sections (blank-line separated): building now (computer · LLM · duration ·
/// heartbeat · eta, STUCK flag), recently completed (with build time), recent
/// failures (with reason), rolling deployment (fleet only), backlog counts +
/// items-still-to-build, Jira (if configured), and a final ETA to clear the
/// backlog at the current merge pace. Everything is queried live.
pub async fn build_project_digest(pg: &PgPool, project_id: &str) -> Result<String> {
    // The synthetic 'ff' project = the platform's own status, not project work.
    if project_id == "ff" {
        return build_system_digest(pg).await;
    }

    let window_end: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(pg)
        .await?;
    let since: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT last_sent_at FROM project_digest_configs \
          WHERE project_id = $1 ORDER BY last_sent_at DESC NULLS LAST LIMIT 1",
    )
    .bind(project_id)
    .fetch_optional(pg)
    .await?
    .flatten();
    build_project_digest_window(
        pg,
        project_id,
        since.unwrap_or(window_end - chrono::Duration::hours(24)),
        window_end,
    )
    .await
}

async fn build_project_digest_window(
    pg: &PgPool,
    project_id: &str,
    cursor_at: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> Result<String> {
    if project_id == "ff" {
        return build_system_digest(pg).await;
    }
    // Building now — with the COMPUTER (assigned_computer) and the LLM (parsed
    // from the lease endpoint) building each item.
    // The LLM label is {serving-node}:{model} — the build runs in a slot on
    // `assigned_computer`, but the router may serve it from a DIFFERENT node's
    // model (a slot on lily can build via glm on adele). Resolve the lease's
    // endpoint (http://IP:port) to that node+model via fleet_model_deployments,
    // so the digest shows `lily:glm-4.5-air` / `cloud:codex`, not a bare "local"
    // or a port number (operator 2026-07-26). Empty endpoint = local codegen that
    // didn't record its model (telemetry gap) → "local:building".
    let building: Vec<(String, String, String, i32, Option<i32>)> = sqlx::query_as(
        "SELECT left(w.title, 30), \
                coalesce(w.assigned_computer, ''), \
                coalesce( \
                  -- an http://IP:port endpoint → resolve to {serving-node}:{model}
                  (SELECT w2.name || ':' || d.catalog_id \
                     FROM fleet_model_deployments d \
                     JOIN fleet_workers w2 ON w2.name = d.worker_name \
                    WHERE l.endpoint LIKE 'http%' \
                      AND l.endpoint LIKE '%' || w2.ip || ':' || d.port || '%' \
                    LIMIT 1), \
                  -- a 'local:model' / 'lane1.5:local:model' / 'cloud:backend' label
                  -- recorded by the dispatcher → use it verbatim; empty → 'local'
                  CASE WHEN coalesce(l.endpoint,'') = '' THEN 'local' \
                       ELSE l.endpoint END \
                ), \
                (EXTRACT(EPOCH FROM (now() - l.created_at)) / 60)::int, \
                (EXTRACT(EPOCH FROM (now() - l.heartbeat_at)))::int \
           FROM work_item_leases l JOIN work_items w ON w.id = l.work_item_id \
          WHERE l.released_at IS NULL AND w.project_id = $1 \
          ORDER BY l.created_at",
    )
    .bind(project_id)
    .fetch_all(pg)
    .await
    .unwrap_or_default();

    // Event history is authoritative per item. Legacy completed_at is eligible
    // only for items with no terminal event history at all.
    let completed: Vec<(String, Option<i32>)> = sqlx::query_as(
        "WITH terminal_any AS ( \
             SELECT DISTINCT e.work_item_id \
               FROM work_item_events e \
              WHERE e.to_status IN ('done','merged','failed','cancelled') \
         ), completion_history AS ( \
             SELECT DISTINCT ON (e.work_item_id) e.work_item_id, e.occurred_at \
               FROM work_item_events e \
              WHERE e.to_status IN ('done','merged') AND e.occurred_at <= $3 \
              ORDER BY e.work_item_id, e.occurred_at DESC, e.id DESC \
         ), selected AS ( \
             SELECT w.title, w.started_at, \
                    CASE WHEN c.work_item_id IS NOT NULL THEN c.occurred_at \
                         WHEN a.work_item_id IS NULL THEN w.completed_at END event_at \
               FROM work_items w \
               LEFT JOIN terminal_any a ON a.work_item_id=w.id \
               LEFT JOIN completion_history c ON c.work_item_id=w.id \
              WHERE w.project_id=$1 AND w.status IN ('done','merged') \
                AND CASE WHEN c.work_item_id IS NOT NULL \
                         THEN c.occurred_at > $2 AND c.occurred_at <= $3 \
                         WHEN a.work_item_id IS NULL \
                         THEN w.completed_at > $2 AND w.completed_at <= $3 \
                         ELSE false END \
         ) SELECT left(title,32), \
                  CASE WHEN started_at IS NOT NULL AND event_at >= started_at \
                       THEN round(EXTRACT(EPOCH FROM (event_at-started_at))/60)::int END \
             FROM selected ORDER BY event_at DESC LIMIT 8",
    )
    .bind(project_id)
    .bind(cursor_at)
    .bind(window_end)
    .fetch_all(pg)
    .await
    .unwrap_or_default();

    // Newly failed in the same frozen window, fenced by current status.
    let failures: Vec<(String, String)> = sqlx::query_as(
        "WITH failed_any AS ( \
             SELECT DISTINCT e.work_item_id FROM work_item_events e WHERE e.to_status='failed' \
         ), latest AS ( \
             SELECT DISTINCT ON (e.work_item_id) e.work_item_id, e.occurred_at \
               FROM work_item_events e \
              WHERE e.to_status='failed' AND e.occurred_at <= $3 \
              ORDER BY e.work_item_id, e.occurred_at DESC, e.id DESC \
         ), selected AS ( \
             SELECT w.title,w.last_error, \
                    CASE WHEN e.work_item_id IS NOT NULL THEN e.occurred_at \
                         WHEN a.work_item_id IS NULL THEN w.completed_at END event_at \
               FROM work_items w \
               LEFT JOIN failed_any a ON a.work_item_id=w.id \
               LEFT JOIN latest e ON e.work_item_id=w.id \
            WHERE w.project_id=$1 AND w.status='failed' \
         ) SELECT left(title,28),left(coalesce(last_error,'unknown'),52) \
             FROM selected WHERE event_at > $2 AND event_at <= $3 \
            ORDER BY event_at DESC LIMIT 5",
    )
    .bind(project_id)
    .bind(cursor_at)
    .bind(window_end)
    .fetch_all(pg)
    .await
    .unwrap_or_default();

    let (ready, failed, verified, blocked_op): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*) FILTER (WHERE status='ready'), \
                COUNT(*) FILTER (WHERE status='failed'), \
                COUNT(*) FILTER (WHERE verified=1), \
                COUNT(*) FILTER (WHERE status='blocked' AND coalesce(last_error,'') ILIKE '%operator%') \
           FROM work_items WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_one(pg)
    .await
    .unwrap_or((0, 0, 0, 0));

    // Merge throughput (last 24h) → ETA to clear the backlog at current pace.
    let merged_24h: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_items \
          WHERE project_id = $1 AND status='merged' \
            AND completed_at > now() - interval '24 hours'",
    )
    .bind(project_id)
    .fetch_one(pg)
    .await
    .unwrap_or(0);

    let mut msg = String::new();

    // §1 Completed since last update (FIRST — operator wants completions up top;
    // always shown, "(none)" when the window had no completions so the section is
    // never silently missing).
    msg.push_str("✅ Completed since last update:\n");
    if completed.is_empty() {
        msg.push_str("• (none)\n");
    } else {
        for (title, mins) in &completed {
            let duration = mins
                .map(|m| fmt_mins(m as i64))
                .unwrap_or_else(|| "duration unavailable".into());
            msg.push_str(&format!("• {title} — took {duration}\n"));
        }
    }
    msg.push('\n');

    // §2 Building now
    msg.push_str("🔨 Building now (computer · LLM · duration · heartbeat · eta):\n");
    if building.is_empty() {
        msg.push_str("• (idle)\n");
    } else {
        for (title, computer, llm_label, mins, hb) in &building {
            let stuck = hb.map(|h| h > 300).unwrap_or(false);
            let eta = (15 - mins).max(1);
            let hbs = hb.map(|h| h.to_string()).unwrap_or_else(|| "?".into());
            let comp = if computer.is_empty() { "?" } else { computer };
            msg.push_str(&format!(
                "• {}{} — {} · {} — {}m in, hb {}s (eta~{}m)\n",
                if stuck { "⚠STUCK " } else { "" },
                title,
                comp,
                llm_label,
                mins,
                hbs,
                eta
            ));
        }
    }

    // §3 Failures
    if !failures.is_empty() {
        msg.push_str("\n❌ Newly failed since last update (still failing):\n");
        for (title, reason) in &failures {
            msg.push_str(&format!("• {} — {}\n", title, reason));
        }
    }

    // §4 Rolling deployment (fleet control plane only)
    if project_id == "forge-fleet" {
        let deploy: Option<(String, i32, i32)> = sqlx::query_as(
            "SELECT commit_sha, nodes_updated, nodes_total FROM fleet_deploy_events \
              ORDER BY deployed_at DESC LIMIT 1",
        )
        .fetch_optional(pg)
        .await
        .ok()
        .flatten();
        if let Some((sha, up, tot)) = deploy {
            msg.push_str(&format!(
                "\n📦 Rolling deployment: {sha} · {up}/{tot} nodes\n"
            ));
        }
    }

    // §5 Backlog counts + items still to build
    let to_build = ready + building.len() as i64;
    msg.push_str(&format!(
        "\n📊 Backlog: {to_build} still to build ({ready} ready · {} building) · {failed} failed · {verified} verified · ⛔{blocked_op} blocked-on-you",
        building.len()
    ));

    // §6 Jira (only for projects with a jira_config: config_id == project_id)
    let has_jira: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM jira_configs WHERE name = $1)")
            .bind(project_id)
            .fetch_one(pg)
            .await
            .unwrap_or(false);
    if has_jira {
        let (tracked, waiting_you): (i64, i64) = sqlx::query_as(
            "SELECT \
               (SELECT COUNT(*) FROM jira_watch_state WHERE config_id = $1), \
               (SELECT COUNT(*) FROM jira_watch_state WHERE config_id = $1 \
                  AND awaiting_party IS NOT NULL \
                  AND (awaiting_party ILIKE '%operator%' OR awaiting_party ILIKE '%report%' \
                       OR awaiting_party ILIKE '%owner%' OR awaiting_party ILIKE '%you%'))",
        )
        .bind(project_id)
        .fetch_one(pg)
        .await
        .unwrap_or((0, 0));
        let in_flight: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM jira_issue_leases WHERE config_id = $1")
                .bind(project_id)
                .fetch_one(pg)
                .await
                .unwrap_or(0);
        msg.push_str(&format!(
            "\n🎫 Jira: {tracked} tracked · {in_flight} in-flight · ⛔{waiting_you} waiting-on-you"
        ));
    }

    // §7 ETA to clear the backlog at the current merge pace
    let eta = if to_build == 0 {
        "backlog clear ✅".to_string()
    } else if merged_24h == 0 {
        "n/a (no merges in 24h — pace unknown)".to_string()
    } else {
        let per_hr = merged_24h as f64 / 24.0;
        let hours = to_build as f64 / per_hr;
        if hours >= 48.0 {
            format!(
                "~{:.0}d at current pace ({merged_24h} merged/24h)",
                hours / 24.0
            )
        } else {
            format!("~{hours:.0}h at current pace ({merged_24h} merged/24h)")
        }
    };
    msg.push_str(&format!("\n\n⏱ ETA to clear backlog: {eta}"));

    Ok(msg)
}

/// Build the SYSTEM digest — the status of ForgeFleet-the-platform itself:
/// self-improvement throughput, the local-model catalog, cloud headroom, the
/// leader, and the last rolling deployment. This answers "what is ff doing
/// outside the projects?" All queries are defensive (a missing table/column
/// degrades that one line, never the whole digest).
pub async fn build_system_digest(pg: &PgPool) -> Result<String> {
    let mut msg = String::new();

    // Self-improvement: work items ff merged into itself in the last 24h, and
    // what's building across ALL projects right now.
    // Window = SINCE THE LAST ff:system digest (operator: "self-improvement only
    // since last message"), matching the project digests. NULL (first send) →
    // 24h fallback.
    let since: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT last_sent_at FROM project_digest_configs WHERE id = 'ff:system'",
    )
    .fetch_optional(pg)
    .await
    .ok()
    .flatten();
    let merged_since: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_items WHERE status='merged' \
           AND completed_at > coalesce($1, now() - interval '24 hours')",
    )
    .bind(since)
    .fetch_one(pg)
    .await
    .unwrap_or(0);
    let building_now: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM work_item_leases WHERE released_at IS NULL")
            .fetch_one(pg)
            .await
            .unwrap_or(0);
    msg.push_str(&format!(
        "🧠 Self-improvement: {merged_since} merged (since last update) · {building_now} building now\n"
    ));

    // Local model catalog — how many models ff can run locally (local-first).
    let (catalog, deployed): (i64, i64) = (
        sqlx::query_scalar("SELECT COUNT(*) FROM fleet_model_catalog")
            .fetch_one(pg)
            .await
            .unwrap_or(0),
        sqlx::query_scalar("SELECT COUNT(*) FROM fleet_model_deployments")
            .fetch_one(pg)
            .await
            .unwrap_or(0),
    );
    let stale_deploys: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM fleet_model_deployments \
          WHERE desired_state='active' AND health_status IS DISTINCT FROM 'healthy'",
    )
    .fetch_one(pg)
    .await
    .unwrap_or(0);
    msg.push_str(&format!(
        "🤖 Local models: {catalog} in catalog · {deployed} deployed{}\n",
        if stale_deploys > 0 {
            format!(" · ⚠{stale_deploys} unhealthy")
        } else {
            String::new()
        }
    ));

    // Self-heal: fix tasks ff completed for its own errors (last 24h) — the
    // self-improvement loop (scan_interaction_errors → self_heal_writer).
    let self_heal_24h: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM fleet_tasks \
          WHERE task_type = 'self_heal_writer' AND status = 'completed' \
            AND completed_at > now() - interval '24 hours'",
    )
    .fetch_one(pg)
    .await
    .unwrap_or(0);

    // Health at a glance: the no-diff + stall failure classes ff doctor tracks,
    // plus nodes over disk quota. So the platform digest surfaces the problems ff
    // is (or should be) fixing, not just throughput.
    let (nodiff, stall, disk_over): (i64, i64, i64) = sqlx::query_as(
        "SELECT \
           (SELECT COUNT(*) FROM work_items WHERE status='failed' \
              AND (last_error ILIKE '%no diff%' OR last_error ILIKE '%no commits%')), \
           (SELECT COUNT(*) FROM work_items WHERE status='failed' \
              AND (last_error ILIKE '%stalled%' OR last_error ILIKE '%no dispatchable backend%' \
                   OR last_error ILIKE '%index.lock%')) \
           + (SELECT COUNT(*) FROM work_item_leases WHERE released_at IS NULL \
                AND heartbeat_at < now() - interval '300 seconds'), \
           (SELECT COUNT(*) FROM fleet_workers w \
              JOIN (SELECT DISTINCT ON (worker_name) worker_name, used_bytes, total_bytes \
                      FROM fleet_disk_usage WHERE sampled_at > now()-interval '24h' \
                     ORDER BY worker_name, sampled_at DESC) l ON l.worker_name = w.name \
             WHERE l.total_bytes > 0 AND l.used_bytes*100.0/l.total_bytes >= w.disk_quota_pct)",
    )
    .fetch_one(pg)
    .await
    .unwrap_or((0, 0, 0));
    msg.push_str(&format!(
        "🩺 Health: {nodiff} no-diff · {stall} stalled · {disk_over} over-disk · 🔧 {self_heal_24h} self-heal fixes (24h)\n"
    ));

    // Cloud headroom per provider (weekly_pct used; flag exhausted windows).
    let providers: Vec<(String, Option<i16>, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
        "SELECT provider, weekly_pct, window_exhausted_until FROM cloud_budget_buckets ORDER BY provider",
    )
    .fetch_all(pg)
    .await
    .unwrap_or_default();
    if !providers.is_empty() {
        msg.push_str("☁️ Cloud headroom: ");
        let parts: Vec<String> = providers
            .iter()
            .map(|(p, pct, exh)| {
                let used = pct.unwrap_or(0);
                let exhausted = exh.map(|t| t > chrono::Utc::now()).unwrap_or(false);
                if exhausted {
                    format!("{p}=RATE-LIMITED")
                } else {
                    format!("{p}={}%left", 100 - used.min(100))
                }
            })
            .collect();
        msg.push_str(&parts.join("  "));
        msg.push('\n');
    }

    msg.push('\n');

    // Leader + last rolling deployment (fleet operational status).
    if let Ok(Some((leader, epoch))) = sqlx::query_as::<_, (String, i64)>(
        "SELECT member_name, epoch::bigint FROM fleet_leader_state WHERE singleton_key='current'",
    )
    .fetch_optional(pg)
    .await
    {
        msg.push_str(&format!("👑 Leader: {leader} (epoch {epoch})\n"));
    }
    let deploy: Option<(String, i32, i32)> = sqlx::query_as(
        "SELECT commit_sha, nodes_updated, nodes_total FROM fleet_deploy_events \
          ORDER BY deployed_at DESC LIMIT 1",
    )
    .fetch_optional(pg)
    .await
    .ok()
    .flatten();
    if let Some((sha, up, tot)) = deploy {
        msg.push_str(&format!("📦 Last deploy: {sha} · {up}/{tot} nodes\n"));
    }

    Ok(msg.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use uuid::Uuid;

    struct MockSender {
        outcomes: Mutex<VecDeque<crate::telegram::TelegramDigestOutcome>>,
        calls: Mutex<Vec<(String, String, Option<Vec<u8>>)>>,
        delay: Duration,
        interfere: bool,
        later_cursor: bool,
    }

    impl MockSender {
        fn new(outcomes: Vec<crate::telegram::TelegramDigestOutcome>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes.into()),
                calls: Mutex::new(Vec::new()),
                delay: Duration::ZERO,
                interfere: false,
                later_cursor: false,
            }
        }
    }

    #[async_trait]
    impl DigestSender for MockSender {
        async fn send(
            &self,
            pg: &PgPool,
            title: &str,
            body: &str,
            logo: Option<&[u8]>,
        ) -> crate::telegram::TelegramDigestOutcome {
            self.calls.lock().unwrap().push((
                title.to_string(),
                body.to_string(),
                logo.map(<[u8]>::to_vec),
            ));
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            if self.interfere {
                sqlx::query(
                    "UPDATE project_digest_attempts \
                        SET attempt=attempt+1,fence=$1 WHERE delivery_status='sending'",
                )
                .bind(Uuid::new_v4())
                .execute(pg)
                .await
                .unwrap();
            }
            if self.later_cursor {
                sqlx::query(
                    "UPDATE project_digest_configs \
                        SET last_sent_at=now()+interval '1 hour' WHERE id='cfg'",
                )
                .execute(pg)
                .await
                .unwrap();
            }
            self.outcomes.lock().unwrap().pop_front().unwrap()
        }
    }

    fn acknowledged(message_id: i64) -> crate::telegram::TelegramDigestOutcome {
        crate::telegram::TelegramDigestOutcome::Acknowledged {
            messages: vec![crate::telegram::TelegramMessageIdentity {
                chat_id: "-1001".into(),
                message_id,
            }],
        }
    }

    async fn test_pool() -> Option<PgPool> {
        let url = std::env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
            .ok()?;
        Some(
            PgPool::connect(&url)
                .await
                .expect("connect to test database"),
        )
    }

    async fn run_once_fixture(pool: &PgPool) -> DateTime<Utc> {
        sqlx::raw_sql(ff_db::schema::SCHEMA_V283_PROJECT_DIGEST_ATTEMPTS)
            .execute(pool)
            .await
            .unwrap();
        sqlx::raw_sql(
            "CREATE TABLE work_items (\
                id uuid PRIMARY KEY, project_id text NOT NULL, title text NOT NULL,\
                status text NOT NULL, started_at timestamptz, completed_at timestamptz,\
                last_error text, verified smallint NOT NULL DEFAULT 0,\
                assigned_computer text);\
             CREATE TABLE work_item_events (\
                id bigserial PRIMARY KEY, work_item_id uuid NOT NULL,\
                from_status text, to_status text NOT NULL, occurred_at timestamptz NOT NULL);\
             CREATE TABLE work_item_leases (\
                work_item_id uuid, endpoint text, created_at timestamptz,\
                heartbeat_at timestamptz, released_at timestamptz);",
        )
        .execute(pool)
        .await
        .unwrap();
        let cursor = Utc::now() - chrono::Duration::hours(2);
        sqlx::query(
            "INSERT INTO project_digest_configs \
                (id,project_id,kind,title,interval_secs,last_sent_at,logo_png) \
             VALUES ('cfg','p','standing','Project',1,$1,$2)",
        )
        .bind(cursor)
        .bind(vec![1_u8, 2, 3])
        .execute(pool)
        .await
        .unwrap();
        let item = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO work_items \
                (id,project_id,title,status,started_at,completed_at) \
             VALUES ($1,'p','finished','done',$2,$3)",
        )
        .bind(item)
        .bind(cursor)
        .bind(cursor + chrono::Duration::minutes(30))
        .execute(pool)
        .await
        .unwrap();
        sqlx::query_scalar("SELECT last_sent_at FROM project_digest_configs WHERE id='cfg'")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[test]
    fn delivery_key_is_stable_and_window_specific() {
        let start = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let end = start + chrono::Duration::minutes(15);
        assert_eq!(
            digest_delivery_key("cfg", start, end),
            digest_delivery_key("cfg", start, end)
        );
        assert_ne!(
            digest_delivery_key("cfg", start, end),
            digest_delivery_key("cfg", start, end + chrono::Duration::seconds(1))
        );
    }

    #[tokio::test]
    async fn event_windows_fallback_status_fences_and_durations() {
        let Some(pool) = test_pool().await else {
            eprintln!("skipping digest DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL");
            return;
        };
        sqlx::raw_sql(
            "CREATE TABLE project_digest_configs (id text PRIMARY KEY, project_id text NOT NULL, last_sent_at timestamptz);\
             CREATE TABLE work_items (id uuid PRIMARY KEY, project_id text NOT NULL, title text NOT NULL, status text NOT NULL, started_at timestamptz, completed_at timestamptz, last_error text, verified smallint NOT NULL DEFAULT 0, assigned_computer text);\
             CREATE TABLE work_item_events (id bigserial PRIMARY KEY, work_item_id uuid NOT NULL, from_status text, to_status text NOT NULL, occurred_at timestamptz NOT NULL);\
             CREATE TABLE work_item_leases (work_item_id uuid, endpoint text, created_at timestamptz, heartbeat_at timestamptz, released_at timestamptz);",
        )
        .execute(&pool).await.unwrap();
        let start = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let end = start + chrono::Duration::hours(1);
        let rows = [
            (
                "reclosed",
                "done",
                Some(start),
                Some(start + chrono::Duration::minutes(5)),
                None,
            ),
            (
                "reopened",
                "ready",
                Some(start),
                Some(start + chrono::Duration::minutes(10)),
                None,
            ),
            (
                "legacy",
                "merged",
                None,
                Some(start + chrono::Duration::minutes(20)),
                None,
            ),
            (
                "invalid-duration",
                "done",
                Some(end),
                Some(start + chrono::Duration::minutes(30)),
                None,
            ),
            ("open-boundary", "done", Some(start), Some(start), None),
            ("closed-boundary", "done", Some(start), Some(end), None),
            (
                "stale-fallback",
                "done",
                Some(start),
                Some(start + chrono::Duration::minutes(40)),
                None,
            ),
            (
                "new-failure",
                "failed",
                Some(start),
                Some(start + chrono::Duration::minutes(15)),
                Some("boom"),
            ),
            (
                "recovered-failure",
                "ready",
                Some(start),
                Some(start + chrono::Duration::minutes(16)),
                Some("old"),
            ),
            (
                "terminal-other",
                "done",
                Some(start),
                Some(start + chrono::Duration::minutes(22)),
                None,
            ),
            (
                "legacy-failure",
                "failed",
                Some(start),
                Some(start + chrono::Duration::minutes(23)),
                Some("legacy boom"),
            ),
        ];
        let mut ids = std::collections::HashMap::new();
        for (title, status, started, completed, error) in rows {
            let id = Uuid::new_v4();
            ids.insert(title, id);
            sqlx::query("INSERT INTO work_items(id,project_id,title,status,started_at,completed_at,last_error) VALUES($1,'p',$2,$3,$4,$5,$6)")
                .bind(id).bind(title).bind(status).bind(started).bind(completed).bind(error)
                .execute(&pool).await.unwrap();
        }
        async fn event(pool: &PgPool, id: Uuid, status: &str, at: DateTime<Utc>) {
            sqlx::query(
                "INSERT INTO work_item_events(work_item_id,to_status,occurred_at) VALUES($1,$2,$3)",
            )
            .bind(id)
            .bind(status)
            .bind(at)
            .execute(pool)
            .await
            .unwrap();
        }
        event(
            &pool,
            ids["reclosed"],
            "done",
            start + chrono::Duration::minutes(15),
        )
        .await;
        event(
            &pool,
            ids["reclosed"],
            "merged",
            start + chrono::Duration::minutes(45),
        )
        .await;
        event(
            &pool,
            ids["reclosed"],
            "merged",
            end + chrono::Duration::minutes(5),
        )
        .await;
        event(
            &pool,
            ids["reclosed"],
            "merged",
            start + chrono::Duration::minutes(45),
        )
        .await;
        event(
            &pool,
            ids["reopened"],
            "done",
            start + chrono::Duration::minutes(25),
        )
        .await;
        event(
            &pool,
            ids["invalid-duration"],
            "done",
            start + chrono::Duration::minutes(30),
        )
        .await;
        event(&pool, ids["open-boundary"], "done", start).await;
        event(&pool, ids["closed-boundary"], "done", end).await;
        event(
            &pool,
            ids["stale-fallback"],
            "done",
            start - chrono::Duration::minutes(1),
        )
        .await;
        event(
            &pool,
            ids["new-failure"],
            "failed",
            start + chrono::Duration::minutes(35),
        )
        .await;
        event(
            &pool,
            ids["new-failure"],
            "failed",
            end + chrono::Duration::minutes(5),
        )
        .await;
        event(
            &pool,
            ids["recovered-failure"],
            "failed",
            start + chrono::Duration::minutes(36),
        )
        .await;
        event(
            &pool,
            ids["terminal-other"],
            "failed",
            start - chrono::Duration::minutes(1),
        )
        .await;

        let body = build_project_digest_window(&pool, "p", start, end)
            .await
            .unwrap();
        assert!(body.contains("reclosed"));
        assert!(body.contains("legacy — took duration unavailable"));
        assert!(body.contains("invalid-duration — took duration unavailable"));
        assert!(body.contains("closed-boundary"));
        assert!(!body.contains("reopened"));
        assert!(!body.contains("open-boundary"));
        assert!(!body.contains("stale-fallback"));
        assert!(!body.contains("terminal-other"));
        assert!(body.contains("Newly failed since last update (still failing)"));
        assert!(body.contains("new-failure — boom"));
        assert!(body.contains("legacy-failure — legacy boom"));
        assert!(!body.contains("recovered-failure"));
    }

    #[tokio::test]
    async fn run_once_success_atomically_records_ack_and_cursor() {
        let Some(pool) = test_pool().await else {
            eprintln!("skipping digest DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL");
            return;
        };
        let old_cursor = run_once_fixture(&pool).await;
        let sender = MockSender::new(vec![acknowledged(42)]);

        run_once_with_sender(&pool, &sender).await.unwrap();

        let row: (
            String,
            i64,
            String,
            i64,
            serde_json::Value,
            DateTime<Utc>,
            DateTime<Utc>,
        ) = sqlx::query_as(
            "SELECT delivery_status,attempt,ack_chat_id,ack_message_id,\
                    acknowledgement,window_end,c.last_sent_at \
               FROM project_digest_attempts a \
               JOIN project_digest_configs c ON c.id=a.config_id \
              WHERE a.config_id='cfg'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "delivered");
        assert_eq!(row.1, 1);
        assert_eq!(row.2, "-1001");
        assert_eq!(row.3, 42);
        assert_eq!(row.4[0]["message_id"], 42);
        assert_eq!(row.5, row.6);
        assert!(row.6 > old_cursor);
        let calls = sender.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].2.as_deref(), Some(&[1, 2, 3][..]));
    }

    #[tokio::test]
    async fn run_once_definite_failure_retries_same_frozen_payload() {
        let Some(pool) = test_pool().await else {
            eprintln!("skipping digest DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL");
            return;
        };
        let old_cursor = run_once_fixture(&pool).await;
        let sender = MockSender::new(vec![
            crate::telegram::TelegramDigestOutcome::DefinitelyNotDelivered {
                error: "telegram HTTP 503".into(),
            },
            acknowledged(43),
        ]);

        run_once_with_sender(&pool, &sender).await.unwrap();
        let retryable: (String, i64, String, DateTime<Utc>) = sqlx::query_as(
            "SELECT delivery_status,attempt,last_error,c.last_sent_at \
               FROM project_digest_attempts a \
               JOIN project_digest_configs c ON c.id=a.config_id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(retryable.0, "retryable");
        assert_eq!(retryable.1, 1);
        assert!(retryable.2.contains("503"));
        assert_eq!(retryable.3, old_cursor);

        run_once_with_sender(&pool, &sender).await.unwrap();
        let delivered: (String, i64) =
            sqlx::query_as("SELECT delivery_status,attempt FROM project_digest_attempts")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(delivered, ("delivered".into(), 2));
        let calls = sender.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], calls[1], "retry must reuse title/body/logo");
    }

    #[tokio::test]
    async fn run_once_ambiguous_and_stranded_sending_never_resend() {
        let Some(pool) = test_pool().await else {
            eprintln!("skipping digest DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL");
            return;
        };
        let old_cursor = run_once_fixture(&pool).await;
        let sender = MockSender::new(vec![crate::telegram::TelegramDigestOutcome::Ambiguous {
            error: "response parse loss".into(),
        }]);
        run_once_with_sender(&pool, &sender).await.unwrap();
        run_once_with_sender(&pool, &sender).await.unwrap();
        assert_eq!(sender.calls.lock().unwrap().len(), 1);
        let ambiguous: (String, DateTime<Utc>) = sqlx::query_as(
            "SELECT delivery_status,c.last_sent_at \
               FROM project_digest_attempts a \
               JOIN project_digest_configs c ON c.id=a.config_id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(ambiguous, ("ambiguous".into(), old_cursor));

        sqlx::query(
            "UPDATE project_digest_attempts \
                SET delivery_status='sending',updated_at=now()-interval '1 day'",
        )
        .execute(&pool)
        .await
        .unwrap();
        run_once_with_sender(&pool, &sender).await.unwrap();
        let state: String =
            sqlx::query_scalar("SELECT delivery_status FROM project_digest_attempts")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state, "sending");
        assert_eq!(sender.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn run_once_success_without_message_identity_fails_closed() {
        let Some(pool) = test_pool().await else {
            eprintln!("skipping digest DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL");
            return;
        };
        let old_cursor = run_once_fixture(&pool).await;
        let sender = MockSender::new(vec![crate::telegram::TelegramDigestOutcome::Acknowledged {
            messages: Vec::new(),
        }]);

        run_once_with_sender(&pool, &sender).await.unwrap();
        run_once_with_sender(&pool, &sender).await.unwrap();

        let state: (String, Option<serde_json::Value>, DateTime<Utc>) = sqlx::query_as(
            "SELECT delivery_status,acknowledgement,c.last_sent_at \
               FROM project_digest_attempts a \
               JOIN project_digest_configs c ON c.id=a.config_id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(state.0, "ambiguous");
        assert_eq!(state.1, None);
        assert_eq!(state.2, old_cursor);
        assert_eq!(sender.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn run_once_concurrent_claim_sends_once() {
        let Some(pool) = test_pool().await else {
            eprintln!("skipping digest DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL");
            return;
        };
        run_once_fixture(&pool).await;
        let mut sender = MockSender::new(vec![acknowledged(44)]);
        sender.delay = Duration::from_millis(100);
        let sender = std::sync::Arc::new(sender);
        let (first, second) = tokio::join!(
            run_once_with_sender(&pool, sender.as_ref()),
            run_once_with_sender(&pool, sender.as_ref())
        );
        first.unwrap();
        second.unwrap();
        assert_eq!(sender.calls.lock().unwrap().len(), 1);
        let state: String =
            sqlx::query_scalar("SELECT delivery_status FROM project_digest_attempts")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state, "delivered");
    }

    #[tokio::test]
    async fn run_once_stale_fence_cannot_commit_ack_or_cursor() {
        let Some(pool) = test_pool().await else {
            eprintln!("skipping digest DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL");
            return;
        };
        let old_cursor = run_once_fixture(&pool).await;
        let mut sender = MockSender::new(vec![acknowledged(45)]);
        sender.interfere = true;
        run_once_with_sender(&pool, &sender).await.unwrap();
        let row: (String, i64, Option<serde_json::Value>, DateTime<Utc>) = sqlx::query_as(
            "SELECT delivery_status,attempt,acknowledgement,c.last_sent_at \
               FROM project_digest_attempts a \
               JOIN project_digest_configs c ON c.id=a.config_id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "sending");
        assert_eq!(row.1, 2);
        assert_eq!(row.2, None);
        assert_eq!(row.3, old_cursor);
    }

    #[tokio::test]
    async fn run_once_ack_finishes_attempt_without_regressing_later_cursor() {
        let Some(pool) = test_pool().await else {
            eprintln!("skipping digest DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL");
            return;
        };
        run_once_fixture(&pool).await;
        let mut sender = MockSender::new(vec![acknowledged(46)]);
        sender.later_cursor = true;
        run_once_with_sender(&pool, &sender).await.unwrap();
        let row: (String, DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
            "SELECT delivery_status,window_end,c.last_sent_at \
               FROM project_digest_attempts a \
               JOIN project_digest_configs c ON c.id=a.config_id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "delivered");
        assert!(row.2 > row.1);
    }

    #[tokio::test]
    async fn attempt_payload_reuse_ambiguous_dedup_and_atomic_cursor() {
        let Some(pool) = test_pool().await else {
            eprintln!("skipping digest DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL");
            return;
        };
        sqlx::query("CREATE TABLE project_digest_configs (id text PRIMARY KEY, last_sent_at timestamptz, updated_at timestamptz DEFAULT now())").execute(&pool).await.unwrap();
        sqlx::raw_sql(ff_db::schema::SCHEMA_V283_PROJECT_DIGEST_ATTEMPTS)
            .execute(&pool)
            .await
            .unwrap();
        let start = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let end = start + chrono::Duration::minutes(15);
        sqlx::query("INSERT INTO project_digest_configs(id,last_sent_at) VALUES('cfg',$1)")
            .bind(start)
            .execute(&pool)
            .await
            .unwrap();
        let key = digest_delivery_key("cfg", start, end);
        for body in ["exact payload", "replacement must lose"] {
            sqlx::query("INSERT INTO project_digest_attempts(config_id,prior_cursor,cursor_at,window_end,title,body,delivery_key) VALUES('cfg',$1,$1,$2,'title',$3,$4) ON CONFLICT(config_id,cursor_at,window_end) DO NOTHING")
                .bind(start).bind(end).bind(body).bind(&key).execute(&pool).await.unwrap();
        }
        let stored: (String, String) = sqlx::query_as(
            "SELECT body,delivery_key FROM project_digest_attempts WHERE config_id='cfg'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(stored, ("exact payload".into(), key));
        let later_end = end + chrono::Duration::minutes(15);
        let later_key = digest_delivery_key("cfg", start, later_end);
        let concurrent: Option<String> = sqlx::query_scalar(
            "INSERT INTO project_digest_attempts \
                (config_id,prior_cursor,cursor_at,window_end,title,body,delivery_key) \
             VALUES ('cfg',$1,$1,$2,'later','replacement window',$3) \
             ON CONFLICT DO NOTHING RETURNING body",
        )
        .bind(start)
        .bind(later_end)
        .bind(later_key)
        .fetch_optional(&pool)
        .await
        .unwrap();
        assert_eq!(concurrent, None);
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT body FROM project_digest_attempts \
                  WHERE config_id='cfg' AND delivery_status<>'delivered'"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            "exact payload"
        );
        sqlx::query(
            "UPDATE project_digest_attempts SET delivery_status='sending' WHERE config_id='cfg'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let reclaimed: Option<bool> = sqlx::query_scalar("UPDATE project_digest_attempts SET delivery_status='sending' WHERE config_id='cfg' AND delivery_status='prepared' RETURNING true").fetch_optional(&pool).await.unwrap();
        assert_eq!(reclaimed, None);
        let cursor: DateTime<Utc> =
            sqlx::query_scalar("SELECT last_sent_at FROM project_digest_configs WHERE id='cfg'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(cursor, start);

        let mut tx = pool.begin().await.unwrap();
        sqlx::query(
            "UPDATE project_digest_attempts \
                SET delivery_status='delivered',delivered_at=now(), \
                    acknowledgement='[{\"chat_id\":\"-1001\",\"message_id\":47}]'::jsonb, \
                    ack_chat_id='-1001',ack_message_id=47 \
              WHERE config_id='cfg'",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("UPDATE project_digest_configs SET last_sent_at=$1 WHERE id='cfg' AND last_sent_at IS NOT DISTINCT FROM $2").bind(end).bind(start).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        let final_state: (DateTime<Utc>, String) = sqlx::query_as("SELECT c.last_sent_at,a.delivery_status FROM project_digest_configs c JOIN project_digest_attempts a ON a.config_id=c.id WHERE c.id='cfg'").fetch_one(&pool).await.unwrap();
        assert_eq!(final_state, (end, "delivered".into()));
    }
}
