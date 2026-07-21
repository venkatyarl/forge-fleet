//! Leader-only, backpressured conversion of PM ideas into dispatchable tasks.

use anyhow::{Context, Result, bail};
use sqlx::{PgPool, Row};
use std::collections::HashSet;
use std::path::PathBuf;
use tokio::process::Command;
use tracing::{info, warn};
use uuid::Uuid;

pub const REVIEW_CEILING: i64 = 40;
pub const FEED_TARGET: i64 = 30;
pub const MAX_FEEDS_PER_TICK: usize = 1;
pub const READY_PARENT_MIN_AGE_MINUTES: i64 = 5;

#[derive(Debug)]
struct Idea {
    id: Uuid,
    project_id: String,
    kind: String,
    repo_path: Option<String>,
}

/// Run one feeder pass. Leadership is enforced by the daemon tick registry;
/// this function owns the feature gate and pipeline backpressure checks.
pub async fn run_auto_backlog_feeder_tick(pg: &PgPool) -> Result<usize> {
    let rescued = rescue_ready_parent(pg).await?;
    if !auto_feeder_enabled(pg).await? {
        return Ok(rescued);
    }
    if !has_headroom(pg).await? {
        return Ok(rescued);
    }

    let mut fed = 0;
    let mut attempted = HashSet::new();
    while fed < MAX_FEEDS_PER_TICK && has_headroom(pg).await? {
        let Some(idea) = next_idea(pg, &attempted).await? else {
            break;
        };
        attempted.insert(idea.id);

        match idea.kind.as_str() {
            "task" => {
                let changed = sqlx::query(
                    "UPDATE work_items SET status = 'ready', last_error = NULL \
                     WHERE id = $1 AND status = 'idea' AND parked = FALSE",
                )
                .bind(idea.id)
                .execute(pg)
                .await?
                .rows_affected();
                fed += usize::from(changed == 1);
            }
            "bug" | "feature" => match decompose_idea(&idea).await {
                Ok(()) => fed += 1,
                Err(error) => {
                    warn!(item = %idea.id, %error, "auto backlog feeder decomposition failed");
                    sqlx::query(
                        "UPDATE work_items SET last_error = $2 \
                         WHERE id = $1 AND status = 'idea'",
                    )
                    .bind(idea.id)
                    .bind(format!("auto feeder: {error:#}"))
                    .execute(pg)
                    .await?;
                }
            },
            other => {
                warn!(item = %idea.id, kind = other, "auto backlog feeder skipped unsupported kind");
            }
        }
    }

    if fed > 0 {
        info!(fed, "auto backlog feeder promoted ideas");
    }
    Ok(rescued + fed)
}

/// Convert one stale, scheduler-ineligible ready parent into dispatchable leaf
/// tasks. The transient status is an atomic claim across leader handoffs.
async fn rescue_ready_parent(pg: &PgPool) -> Result<usize> {
    // A parent that already has children was decomposed by an older CLI which
    // did not transition ready parents. Repair it without generating duplicates.
    sqlx::query(
        "UPDATE work_items p SET status = 'decomposed', last_error = NULL \
         WHERE p.status = 'ready' AND p.kind IN ('bug', 'feature') \
           AND EXISTS (SELECT 1 FROM work_items c WHERE c.parent_id = p.id)",
    )
    .execute(pg)
    .await?;

    let row = sqlx::query(
        "UPDATE work_items p SET status = 'decomposing', last_error = NULL \
         WHERE p.id = ( \
             SELECT w.id FROM work_items w \
              WHERE w.status = 'ready' AND w.kind IN ('bug', 'feature') \
                AND COALESCE(w.parked, FALSE) = FALSE \
                AND w.created_at <= NOW() - make_interval(mins => $1) \
                AND NOT EXISTS (SELECT 1 FROM work_items c WHERE c.parent_id = w.id) \
              ORDER BY w.created_at ASC LIMIT 1 FOR UPDATE SKIP LOCKED) \
         RETURNING p.id, p.project_id, p.kind, p.repo_path",
    )
    .bind(READY_PARENT_MIN_AGE_MINUTES as i32)
    .fetch_optional(pg)
    .await?;
    let Some(row) = row else { return Ok(0) };
    let parent = Idea {
        id: row.get("id"),
        project_id: row.get("project_id"),
        kind: row.get("kind"),
        repo_path: row.try_get("repo_path").ok().flatten(),
    };

    match decompose_idea(&parent).await {
        Ok(()) => {
            info!(item = %parent.id, "rescued unschedulable ready parent");
            Ok(1)
        }
        Err(error) => {
            warn!(item = %parent.id, %error, "ready parent decomposition failed");
            sqlx::query(
                "UPDATE work_items SET status = 'ready', last_error = $2 \
                 WHERE id = $1 AND status = 'decomposing'",
            )
            .bind(parent.id)
            .bind(format!("ready parent auto-decompose: {error:#}"))
            .execute(pg)
            .await?;
            Ok(0)
        }
    }
}

async fn auto_feeder_enabled(pg: &PgPool) -> Result<bool> {
    let value: Option<String> =
        sqlx::query_scalar("SELECT value FROM fleet_secrets WHERE key = 'auto_feeder_mode'")
            .fetch_optional(pg)
            .await?;
    Ok(
        value
            .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "on" | "true" | "1")),
    )
}

async fn has_headroom(pg: &PgPool) -> Result<bool> {
    let has_free_slot = !ff_db::pg_free_slots(pg, None, 1).await?.is_empty();
    let row = sqlx::query(
        "SELECT COUNT(*) FILTER (WHERE status = 'in_review')::bigint AS in_review, \
                COUNT(*) FILTER (WHERE status IN ('ready', 'claimed', 'building'))::bigint AS active \
           FROM work_items",
    )
    .fetch_one(pg)
    .await?;
    Ok(within_pipeline_limits(
        has_free_slot,
        row.get("in_review"),
        row.get("active"),
    ))
}

pub fn within_pipeline_limits(has_free_slot: bool, in_review: i64, active: i64) -> bool {
    has_free_slot && in_review < REVIEW_CEILING && active < FEED_TARGET
}

async fn next_idea(pg: &PgPool, attempted: &HashSet<Uuid>) -> Result<Option<Idea>> {
    let excluded: Vec<Uuid> = attempted.iter().copied().collect();
    let row = sqlx::query(
        "SELECT id, project_id, kind, repo_path \
           FROM work_items \
          WHERE status = 'idea' AND parked = FALSE \
            AND kind IN ('task', 'bug', 'feature') \
            AND NOT (id = ANY($1::uuid[])) \
          ORDER BY CASE priority \
                     WHEN 'critical' THEN 0 WHEN 'high' THEN 1 \
                     WHEN 'normal' THEN 2 WHEN 'low' THEN 3 \
                     WHEN 'nice-to-have' THEN 4 ELSE 5 END, \
                   created_at ASC \
          LIMIT 1",
    )
    .bind(&excluded)
    .fetch_optional(pg)
    .await?;
    Ok(row.map(|row| Idea {
        id: row.get("id"),
        project_id: row.get("project_id"),
        kind: row.get("kind"),
        repo_path: row.try_get("repo_path").ok().flatten(),
    }))
}

async fn decompose_idea(idea: &Idea) -> Result<()> {
    let ff = std::env::var_os("FORGEFLEET_FF_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("ff"));
    let mut command = Command::new(ff);
    if let Some(path) = &idea.repo_path {
        command.arg("--cwd").arg(path);
    }
    let idea_id = idea.id.to_string();
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(240),
        command
            .kill_on_drop(true)
            .env("FORGEFLEET_AUTO_FEEDER", "1")
            .args([
                "pm",
                "decompose",
                &idea_id,
                "--project",
                &idea.project_id,
                "--ready",
            ])
            .output(),
    )
    .await
    .context("ff pm decompose timed out")?
    .context("run ff pm decompose")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "ff pm decompose exited {}: {}",
            output.status,
            stderr.trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feeder_limits_are_deliberately_bounded() {
        assert_eq!(REVIEW_CEILING, 40);
        assert_eq!(FEED_TARGET, 30);
        assert!((1..=2).contains(&MAX_FEEDS_PER_TICK));
        assert_eq!(READY_PARENT_MIN_AGE_MINUTES, 5);
    }
}
