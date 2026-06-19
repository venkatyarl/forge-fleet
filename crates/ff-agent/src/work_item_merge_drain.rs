//! Pillar 4 — merge-queue drain.
//!
//! Leader-only, serial. Closes the loop: dispatch opens PRs and enqueues
//! `work_item_merge_queue`; this drains it one PR at a time per project — wait
//! for the PR's CI to go green, then `gh pr merge --squash --delete-branch`, and
//! mark the work_item `merged`. CI failure → the entry (and work_item) → `failed`.
//!
//! Serialization (one in-flight merge per project) is enforced by
//! [`ff_db::pg_next_merge_queue_item`], so merges land sequentially even though
//! builds ran in parallel across the fleet.
//!
//! Design: `.forgefleet/plans/DECISION-pillar4-canonical-home.md`.

use anyhow::{Context, Result};
use sqlx::PgPool;
use std::time::Duration;
use tracing::{info, warn};

/// One drain pass. Returns 1 if it merged something this tick, else 0.
pub async fn evaluate_merge_queue(pg: &PgPool) -> Result<usize> {
    let Some(item) = ff_db::pg_next_merge_queue_item(pg).await? else {
        return Ok(0);
    };
    let Some(pr_url) = item.pr_url.clone().filter(|u| !u.trim().is_empty()) else {
        ff_db::pg_mark_merge_failed(pg, item.id, item.work_item_id, "merge entry has no PR url")
            .await?;
        return Ok(0);
    };

    // Mark that we're watching this PR's CI (idempotent).
    ff_db::pg_mark_merge_ci_running(pg, item.id).await?;

    match pr_ci_state(&pr_url).await {
        CiState::Pending => {
            // Still running — leave it; we'll re-check next tick.
            Ok(0)
        }
        CiState::Failed(reason) => {
            warn!(pr = %pr_url, %reason, "merge_drain: PR CI failed — marking work_item failed");
            ff_db::pg_mark_merge_failed(pg, item.id, item.work_item_id, &reason).await?;
            Ok(0)
        }
        CiState::Success => match gh_merge_squash(&pr_url).await {
            Ok(()) => {
                ff_db::pg_mark_merge_merged(pg, item.id, item.work_item_id).await?;
                info!(pr = %pr_url, work_item = %item.work_item_id, "merge_drain: merged");
                Ok(1)
            }
            Err(e) => {
                let reason = format!("gh pr merge failed: {e}");
                warn!(pr = %pr_url, %reason, "merge_drain: merge failed");
                ff_db::pg_mark_merge_failed(pg, item.id, item.work_item_id, &reason).await?;
                Ok(0)
            }
        },
    }
}

enum CiState {
    Pending,
    Success,
    Failed(String),
}

/// Inspect a PR's checks via `gh pr checks <url> --json state`. No checks yet
/// (or gh transient error) is treated as Pending so we never merge prematurely.
async fn pr_ci_state(pr_url: &str) -> CiState {
    let out = match tokio::process::Command::new("gh")
        .args(["pr", "checks", pr_url, "--json", "state"])
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => return CiState::Failed(format!("gh pr checks spawn: {e}")),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    // gh exits non-zero when checks are failing OR still pending; rely on the
    // JSON states rather than the exit code.
    let states: Vec<String> = serde_json::from_str::<serde_json::Value>(&stdout)
        .ok()
        .and_then(|v| {
            v.as_array().map(|a| {
                a.iter()
                    .filter_map(|c| c.get("state").and_then(|s| s.as_str()).map(str::to_string))
                    .collect()
            })
        })
        .unwrap_or_default();

    if states.is_empty() {
        return CiState::Pending; // no checks reported yet
    }
    if states
        .iter()
        .any(|s| matches!(s.as_str(), "FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT"))
    {
        return CiState::Failed(format!("a check is {:?}", states));
    }
    if states
        .iter()
        .any(|s| matches!(s.as_str(), "IN_PROGRESS" | "QUEUED" | "PENDING"))
    {
        return CiState::Pending;
    }
    CiState::Success
}

/// `gh pr merge <url> --squash --delete-branch` (the project policy — always
/// delete the branch; see feedback_pr_merge_delete_branch.md).
async fn gh_merge_squash(pr_url: &str) -> Result<()> {
    let out = tokio::process::Command::new("gh")
        .args(["pr", "merge", pr_url, "--squash", "--delete-branch"])
        .output()
        .await
        .context("spawn gh pr merge")?;
    if out.status.success() {
        return Ok(());
    }
    anyhow::bail!(
        "{}",
        String::from_utf8_lossy(&out.stderr)
            .trim()
            .chars()
            .take(500)
            .collect::<String>()
    );
}

/// Spawn the leader-gated drain loop. Mirrors the scheduler's leader check.
pub fn spawn_work_item_merge_drain(
    pg: PgPool,
    worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let is_leader: bool = sqlx::query_scalar(
                        r#"SELECT EXISTS (
                               SELECT 1 FROM fleet_leader_state
                                WHERE member_name = $1
                                  AND heartbeat_at > NOW() - INTERVAL '60 seconds'
                           )"#,
                    )
                    .bind(&worker_name)
                    .fetch_one(&pg)
                    .await
                    .unwrap_or(false);
                    if !is_leader {
                        continue;
                    }
                    if let Err(e) = evaluate_merge_queue(&pg).await {
                        warn!(error = %e, "work_item_merge_drain tick failed");
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        info!("work_item_merge_drain loop stopped");
    })
}
