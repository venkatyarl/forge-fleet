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
use sqlx::PgPool;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// How long a temporary task-digest keeps sending after its task completes.
const TEMP_DIGEST_LINGER_SECS: i64 = 15 * 60;

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

    // Seed a standing digest for every active project (id 'proj:standing').
    // Title = an emoji + display_name so each project reads distinctly.
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

    // 3) Find configs that are due (never sent, or older than their interval).
    let due: Vec<(String, String, String, Option<Vec<u8>>)> = sqlx::query_as(
        "SELECT id, project_id, title, logo_png \
           FROM project_digest_configs \
          WHERE enabled \
            AND (last_sent_at IS NULL \
                 OR now() - last_sent_at >= make_interval(secs => interval_secs)) \
          ORDER BY kind, project_id",
    )
    .fetch_all(pg)
    .await
    .unwrap_or_default();

    for (id, project_id, title, logo) in due {
        let body = match build_project_digest(pg, &project_id).await {
            Ok(b) => b,
            Err(err) => {
                warn!(project = %project_id, error = %err, "project digest build failed");
                continue;
            }
        };
        if let Err(err) =
            crate::telegram::send_telegram_photo_from_secrets(pg, &title, &body, logo.as_deref())
                .await
        {
            warn!(project = %project_id, error = %err, "project digest send failed");
            continue;
        }
        let _ = sqlx::query(
            "UPDATE project_digest_configs SET last_sent_at = now(), updated_at = now() WHERE id = $1",
        )
        .bind(&id)
        .execute(pg)
        .await;
    }
    Ok(())
}

/// Build one project's digest body, scoped to that project's `work_items`.
/// Sections: what's building now (duration · heartbeat · eta, STUCK flag),
/// backlog/failed/verified counts, blocked-on-operator, and — for the project
/// that owns the fleet control plane — the last rolling deployment. Everything
/// is queried live, so it cannot show fabricated progress.
pub async fn build_project_digest(pg: &PgPool, project_id: &str) -> Result<String> {
    // Building items for THIS project (join leases → work_items on project).
    let building: Vec<(String, i32, Option<i32>)> = sqlx::query_as(
        "SELECT left(w.title, 34), \
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

    let mut msg = String::new();
    msg.push_str("🔨 Building now (duration · heartbeat · eta):\n");
    if building.is_empty() {
        msg.push_str("• (idle)\n");
    } else {
        for (title, mins, hb) in &building {
            let stuck = hb.map(|h| h > 300).unwrap_or(false);
            let eta = (15 - mins).max(1);
            let hbs = hb.map(|h| h.to_string()).unwrap_or_else(|| "?".into());
            msg.push_str(&format!(
                "• {}{} — {}m in, hb {}s (eta~{}m)\n",
                if stuck { "⚠STUCK " } else { "" },
                title,
                mins,
                hbs,
                eta
            ));
        }
    }
    msg.push('\n');

    // Rolling deployment only makes sense for the fleet's own control plane.
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
            msg.push_str(&format!("📦 Rolling deployment: {sha} · {up}/{tot} nodes\n\n"));
        }
    }

    msg.push_str(&format!(
        "📊 ready={ready}  failed={failed}  verified={verified}  ⛔blocked-on-you={blocked_op}"
    ));
    Ok(msg)
}
