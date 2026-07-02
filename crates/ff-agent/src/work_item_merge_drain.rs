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
use std::process::Command;
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
        CiState::Success => {
            // SAFETY GATE: never auto-merge LLM-authored code to main unless the
            // operator has explicitly opted in. Default OFF → the PR is left
            // 'mergeable' (CI-green, awaiting approval); flip
            // `work_item_automerge_mode` on (or merge the PR by hand) to land it.
            if !automerge_enabled(pg).await {
                ff_db::pg_mark_merge_mergeable(pg, item.id).await?;
                info!(
                    pr = %pr_url,
                    "merge_drain: PR CI green — MERGEABLE, awaiting operator approval \
                     (work_item_automerge_mode off)"
                );
                return Ok(0);
            }
            match run_pr_review(pg, &pr_url, item.work_item_id).await {
                Ok((true, reason)) => {
                    info!(
                        pr = %pr_url,
                        work_item = %item.work_item_id,
                        %reason,
                        "merge_drain: autonomous review approved"
                    );
                }
                Ok((false, reason)) => {
                    let failure = format!("review rejected: {reason}");
                    warn!(
                        pr = %pr_url,
                        work_item = %item.work_item_id,
                        reason = %failure,
                        "merge_drain: autonomous review rejected PR"
                    );
                    ff_db::pg_mark_merge_failed(pg, item.id, item.work_item_id, &failure).await?;
                    if let Err(e) =
                        gh_pr_comment(&pr_url, &format!("Autonomous review REJECTED: {reason}"))
                            .await
                    {
                        warn!(pr = %pr_url, error = %e, "merge_drain: failed to comment review rejection");
                    }
                    return Ok(0);
                }
                Err(e) => {
                    warn!(
                        pr = %pr_url,
                        work_item = %item.work_item_id,
                        error = %e,
                        "merge_drain: review unavailable, deferring PR for manual review"
                    );
                    return Ok(0);
                }
            }
            match gh_merge_squash(&pr_url).await {
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
            }
        }
    }
}

async fn run_pr_review(
    pg: &PgPool,
    pr_url: &str,
    work_item_id: uuid::Uuid,
) -> anyhow::Result<(bool, String)> {
    let diff_out = Command::new("gh")
        .args(["pr", "diff", pr_url])
        .output()
        .context("spawn gh pr diff")?;
    if !diff_out.status.success() {
        anyhow::bail!(
            "gh pr diff failed: {}",
            String::from_utf8_lossy(&diff_out.stderr)
                .trim()
                .chars()
                .take(500)
                .collect::<String>()
        );
    }
    let diff = truncate_chars(&String::from_utf8_lossy(&diff_out.stdout), 40_000);

    let (title, description): (String, Option<String>) =
        sqlx::query_as("SELECT title, description FROM work_items WHERE id = $1")
            .bind(work_item_id)
            .fetch_one(pg)
            .await
            .context("fetch work_item intent for PR review")?;

    let prompt = format!(
        "You are reviewing a pull request opened by an autonomous coding fleet.\n\
         Judge whether the change correctly and cleanly implements the requested work item.\n\n\
         Work item title:\n{title}\n\n\
         Work item description:\n{description}\n\n\
         Requirements for approval:\n\
         - The diff matches the stated intent.\n\
         - The diff introduces no regressions.\n\
         - The diff does NOT DEGRADE existing code, documentation, comments, tests, or behavior; \
           for example, replacing a good detailed doc comment or working logic with something \
           worse, shorter, less clear, or less complete is a rejection.\n\
         - The diff is a real, complete change rather than a placeholder, superficial edit, or \
           partial implementation.\n\n\
         Answer with exactly APPROVE or REJECT on the first line. Put a one-line reason on the \
         next line.\n\n\
         Pull request diff (truncated to 40000 chars if needed):\n```diff\n{diff}\n```",
        description = description.unwrap_or_default(),
    );

    let response =
        crate::fleet_oneshot::fleet_oneshot(pg, &prompt, None, Some(Duration::from_secs(180)))
            .await
            .context("fleet PR review")?;
    Ok(parse_review_response(&response.text))
}

fn parse_review_response(response: &str) -> (bool, String) {
    let mut first_idx = None;
    let mut first_line = "";
    for (idx, line) in response.lines().enumerate() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            first_idx = Some(idx);
            first_line = trimmed;
            break;
        }
    }

    let Some(idx) = first_idx else {
        return (false, "empty review response".to_string());
    };

    let approved = first_line.to_uppercase().starts_with("APPROVE");
    let reason = response
        .lines()
        .skip(idx + 1)
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let reason = if reason.is_empty() {
        first_line.to_string()
    } else {
        reason
    };
    (approved, reason)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Whether the operator has opted into auto-merging work_item PRs to main.
/// Default OFF — the loop opens + queues PRs but a human approves the merge.
async fn automerge_enabled(pg: &PgPool) -> bool {
    matches!(
        ff_db::pg_read_gate_value(pg, "work_item_automerge_mode", "off", "off")
            .await
            .as_deref(),
        Ok("on") | Ok("true") | Ok("1")
    )
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

async fn gh_pr_comment(pr_url: &str, body: &str) -> Result<()> {
    let out = tokio::process::Command::new("gh")
        .args(["pr", "comment", pr_url, "--body", body])
        .output()
        .await
        .context("spawn gh pr comment")?;
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
    _worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !crate::leader_cache::is_current_leader() {
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
