//! Leader-gated promotion of parked-safe backlog ideas into ready work.

use anyhow::{Context, Result, bail};
use sqlx::PgPool;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};
use uuid::Uuid;

const AUTO_FEEDER_MODE: &str = "auto_feeder_mode";

pub(crate) fn jira_parent_eligibility_sql(alias: &str) -> String {
    format!(
        "({alias}.kind <> 'jira' OR (\
         {alias}.status = 'ready' \
         AND LOWER(BTRIM(COALESCE({alias}.metadata->>'jira_status', ''))) \
             NOT IN ('blocked', 'blocked on vinny') \
         AND NULLIF(BTRIM(COALESCE({alias}.metadata->>'jira_execution_hold', '')), '') IS NULL))"
    )
}

/// Return whether pipeline capacity permits promoting one more idea.
pub fn feed_decision(free_slots: i64, in_review: i64, active: i64) -> bool {
    free_slots > 0 && in_review < 40 && active < 30
}

async fn db_confirms_leader(pg: &PgPool, worker_name: &str) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM fleet_leader_state \
         WHERE member_name = $1 AND heartbeat_at > NOW() - INTERVAL '60 seconds')",
    )
    .bind(worker_name)
    .fetch_one(pg)
    .await
    .unwrap_or(false)
}

async fn feed_once(pg: &PgPool) -> Result<()> {
    if !ff_db::pg_read_safety_gate(pg, AUTO_FEEDER_MODE, false, false).await? {
        return Ok(());
    }

    // INVARIANT: a work_item that is `ready` but NOT a schedulable `task` is a
    // silent dead zone. The scheduler only dispatches kind='task'
    // (`ff_db::pg_ready_work_items` filters `AND w.kind = 'task'`), so a `ready`
    // feature/epic/bug/jira sits forever — unschedulable AND invisible to the
    // idea-promoter below (which only scans `status='idea'`). On 2026-07-28 this
    // starved the fleet to ZERO completions for ~6h: 52 `jira` + 2 `feature` were
    // `ready` with 0 `task` rows, so `pg_ready_work_items` returned nothing and the
    // scheduler correctly assigned nothing — with no alarm anywhere. Auto-decompose
    // such items into leaf tasks BEFORE anything else so a full-but-unschedulable
    // backlog can never silently stall the pipeline again. Runs ahead of the
    // capacity gate: a stuck backlog must be healed even when the pipeline looks
    // busy. Bounded (LIMIT 1/tick) and self-terminating (the `NOT EXISTS task
    // child` guard skips an item once it has been decomposed).
    let ready_parent_sql = format!(
        "UPDATE work_items p SET status = 'decomposing', last_error = NULL \
         WHERE p.id = ( \
           SELECT w.id FROM work_items w \
           WHERE w.status = 'ready' AND w.kind <> 'task' AND NOT w.parked \
             AND {} \
             AND NOT EXISTS (SELECT 1 FROM work_items c \
                             WHERE c.parent_id = w.id AND c.kind = 'task') \
           ORDER BY CASE w.priority \
             WHEN 'critical' THEN 0 WHEN 'high' THEN 1 \
             WHEN 'medium' THEN 2 ELSE 3 END, w.created_at \
           LIMIT 1 FOR UPDATE SKIP LOCKED) \
         RETURNING p.id, p.kind",
        jira_parent_eligibility_sql("w")
    );
    if let Some((id, kind)) = sqlx::query_as::<_, (Uuid, String)>(&ready_parent_sql)
        .fetch_optional(pg)
        .await?
    {
        warn!(
            work_item_id = %id,
            kind = %kind,
            "feeder: healing unschedulable READY non-task item — auto-decomposing into leaf tasks"
        );
        if let Err(error) = decompose(id).await {
            sqlx::query(
                "UPDATE work_items SET status = 'ready', last_error = $2 \
                 WHERE id = $1 AND status = 'decomposing'",
            )
            .bind(id)
            .bind(format!("ready parent auto-decompose: {error:#}"))
            .execute(pg)
            .await?;
            return Err(error);
        }
        sqlx::query(
            "UPDATE work_items SET status = 'decomposed' \
             WHERE id = $1 AND status = 'decomposing'",
        )
        .bind(id)
        .execute(pg)
        .await?;
        return Ok(());
    }

    let (free_slots, in_review, active) = sqlx::query_as::<_, (i64, i64, i64)>(
        "SELECT \
           (SELECT COUNT(*) FROM sub_agents WHERE status <> 'disabled')::bigint \
             - (SELECT COUNT(*) FROM work_item_leases WHERE released_at IS NULL)::bigint, \
           (SELECT COUNT(*) FROM work_items WHERE status = 'in_review')::bigint, \
           (SELECT COUNT(*) FROM work_items \
             WHERE status IN ('ready', 'claimed', 'building'))::bigint",
    )
    .fetch_one(pg)
    .await?;

    if !feed_decision(free_slots, in_review, active) {
        return Ok(());
    }

    let idea = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id, kind FROM work_items \
         WHERE status = 'idea' AND NOT parked \
           AND kind <> 'jira' \
         ORDER BY CASE priority \
           WHEN 'critical' THEN 0 WHEN 'high' THEN 1 \
           WHEN 'medium' THEN 2 ELSE 3 END, created_at \
         LIMIT 1",
    )
    .fetch_optional(pg)
    .await?;

    let Some((id, kind)) = idea else {
        return Ok(());
    };

    match kind.as_str() {
        "task" => {
            let promoted = sqlx::query(
                "UPDATE work_items SET status = 'ready' \
                 WHERE id = $1 AND status = 'idea' AND NOT parked",
            )
            .bind(id)
            .execute(pg)
            .await?
            .rows_affected();
            if promoted == 1 {
                info!(work_item_id = %id, "work item feeder promoted task");
            }
        }
        // Epics decompose exactly like bugs/features. Jira parents intentionally
        // reach decomposition only through the persisted-ready selector above.
        "bug" | "feature" | "epic" => decompose(id).await?,
        other => {
            warn!(work_item_id = %id, kind = other, "work item feeder skipped unsupported kind")
        }
    }

    Ok(())
}

fn ff_binary() -> PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        let sibling = parent.join("ff");
        if sibling.is_file() {
            return sibling;
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let installed = PathBuf::from(home).join(".local/bin/ff");
        if installed.is_file() {
            return installed;
        }
    }
    PathBuf::from("ff")
}

async fn decompose(id: Uuid) -> Result<()> {
    let output = Command::new(ff_binary())
        .args(["pm", "decompose", &id.to_string(), "--ready"])
        .output()
        .await
        .context("run ff pm decompose")?;
    if !output.status.success() {
        bail!(
            "ff pm decompose exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    info!(work_item_id = %id, "work item feeder decomposed idea");
    Ok(())
}

/// Spawn the leader-gated work-item feeder loop.
pub fn spawn_work_item_feeder(
    pg: PgPool,
    worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        // Skip the immediate fire so pulse/election settle first.
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !crate::leader_cache::is_current_leader()
                        && !db_confirms_leader(&pg, &worker_name).await
                    {
                        continue;
                    }
                    if let Err(error) = feed_once(&pg).await {
                        warn!(%error, "work item feeder tick failed");
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        info!("work item feeder loop stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::{feed_decision, jira_parent_eligibility_sql};
    use sqlx::PgPool;

    #[test]
    fn feed_requires_slot_and_pipeline_headroom() {
        assert!(feed_decision(1, 39, 29));
        assert!(!feed_decision(0, 39, 29));
        assert!(!feed_decision(-1, 39, 29));
        assert!(!feed_decision(1, 40, 29));
        assert!(!feed_decision(1, 39, 30));
    }

    #[tokio::test]
    async fn persisted_jira_parent_eligibility_skips_blocked_and_allows_active_statuses() {
        let Some(database_url) = std::env::var("FORGEFLEET_POSTGRES_URL")
            .ok()
            .or_else(|| std::env::var("FORGEFLEET_DATABASE_URL").ok())
        else {
            return;
        };
        let pg = PgPool::connect(&database_url)
            .await
            .expect("connect test db");
        let sql = format!(
            "SELECT label FROM (VALUES \
             ('stale blocked', 'jira', 'ready', '{{\"jira_status\":\"Blocked\"}}'::jsonb), \
             ('vinny blocked', 'jira', 'ready', '{{\"jira_status\":\"Blocked on Vinny\"}}'::jsonb), \
             ('to do', 'jira', 'ready', '{{\"jira_status\":\"To Do\"}}'::jsonb), \
             ('in progress', 'jira', 'ready', '{{\"jira_status\":\"In Progress\"}}'::jsonb), \
             ('held', 'jira', 'ready', '{{\"jira_status\":\"To Do\",\"jira_execution_hold\":\"awaiting_council\"}}'::jsonb), \
             ('idea jira', 'jira', 'idea', '{{\"jira_status\":\"To Do\"}}'::jsonb), \
             ('non-jira idea', 'feature', 'idea', '{{}}'::jsonb)) \
             AS candidate(label, kind, status, metadata) \
             WHERE {} ORDER BY label",
            jira_parent_eligibility_sql("candidate")
        );
        let eligible: Vec<String> = sqlx::query_scalar(&sql)
            .fetch_all(&pg)
            .await
            .expect("evaluate Jira eligibility");
        assert_eq!(eligible, vec!["in progress", "non-jira idea", "to do"]);
    }
}
