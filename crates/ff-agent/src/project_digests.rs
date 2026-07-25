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

    // `logo_path` links each digest to the project's logo file on disk
    // (under ~/projects/<project>/...), so ff knows where the source logo is
    // and can re-render it. `logo_png` caches the rendered/resized bytes sent
    // to Telegram.
    sqlx::query("ALTER TABLE project_digest_configs ADD COLUMN IF NOT EXISTS logo_path text")
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
            crate::telegram::send_telegram_digest(pg, &title, &body, logo.as_deref()).await
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
    // The synthetic 'ff' project = the platform's own status, not project work.
    if project_id == "ff" {
        return build_system_digest(pg).await;
    }
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
            msg.push_str(&format!(
                "📦 Rolling deployment: {sha} · {up}/{tot} nodes\n\n"
            ));
        }
    }

    msg.push_str(&format!(
        "📊 Backlog: ready={ready}  failed={failed}  verified={verified}  ⛔blocked-on-you={blocked_op}"
    ));

    // Jira backlog — only for projects that have a Jira config (config_id ==
    // project_id). Shows tracked issues + how many are waiting on the operator
    // to reply so ff can move them forward. (Tables empty until the Jira
    // monitor populates them → reads 0, never errors.)
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
    let merged_24h: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM work_items WHERE status='merged' AND completed_at > now() - interval '24 hours'")
            .fetch_one(pg)
            .await
            .unwrap_or(0);
    let building_now: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM work_item_leases WHERE released_at IS NULL")
            .fetch_one(pg)
            .await
            .unwrap_or(0);
    msg.push_str(&format!(
        "🧠 Self-improvement: {merged_24h} merged (24h) · {building_now} building now\n"
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
    msg.push_str(&format!(
        "🤖 Local models: {catalog} in catalog · {deployed} deployed\n"
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
