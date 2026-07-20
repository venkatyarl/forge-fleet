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
        "bug" | "feature" => decompose(id).await?,
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
    use super::feed_decision;

    #[test]
    fn feed_requires_slot_and_pipeline_headroom() {
        assert!(feed_decision(1, 39, 29));
        assert!(!feed_decision(0, 39, 29));
        assert!(!feed_decision(-1, 39, 29));
        assert!(!feed_decision(1, 40, 29));
        assert!(!feed_decision(1, 39, 30));
    }
}
