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

/// Build a `gh` invocation with the fleet GitHub token injected as `GH_TOKEN`.
///
/// The merge-drain runs on whichever node is currently leader, and that node's
/// *ambient* `gh` auth (`~/.config/gh`) may be unset or point at a retired
/// account (e.g. `taylor-oclaw`) — relying on it silently breaks every drain
/// call with `gh ... failed: authenticate with gh auth login`, stranding
/// CI-green PRs in the queue. Pulling `github_gh_token` from `fleet_secrets` at
/// call time makes the drain authenticate on ANY leader with no per-node `gh
/// auth` and without writing the token to disk. No secret → ambient-auth
/// fallback (unchanged behaviour).
async fn gh_cmd() -> tokio::process::Command {
    let mut c = tokio::process::Command::new("gh");
    if let Some(token) = crate::fleet_info::fetch_secret("github_gh_token").await {
        c.env("GH_TOKEN", token);
    }
    c
}

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
    let mut diff_cmd = gh_cmd().await;
    diff_cmd.args(["pr", "diff", pr_url]);
    let diff_out = diff_cmd
        .output()
        .await
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
    let mut cmd = gh_cmd().await;
    cmd.args(["pr", "checks", pr_url, "--json", "state"]);
    let out = match cmd.output().await {
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
    let mut cmd = gh_cmd().await;
    cmd.args(["pr", "merge", pr_url, "--squash", "--delete-branch"]);
    let out = cmd.output().await.context("spawn gh pr merge")?;
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
    let mut cmd = gh_cmd().await;
    cmd.args(["pr", "comment", pr_url, "--body", body]);
    let out = cmd.output().await.context("spawn gh pr comment")?;
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

/// Reconcile work_items stranded in `in_review` whose PR was merged or closed
/// out-of-band — hand-merged by the operator, or merged by any path other than
/// [`evaluate_merge_queue`] (e.g. a plain `gh pr merge` in a terminal, which
/// touches GitHub but never the DB). Without this, such items sit in `in_review`
/// forever and the queue/ETA never reflects that they actually shipped.
///
/// Bounded (25 rows/pass) and leader-gated by the caller. Best-effort: a `gh`
/// failure for one PR is logged and skipped — it never aborts the pass. Only
/// rows that are still `in_review` at UPDATE time are flipped (guards against a
/// racing drain that already claimed the row).
async fn reconcile_orphaned_reviews(pg: &PgPool) -> Result<usize> {
    let rows: Vec<(uuid::Uuid, String)> = sqlx::query_as(
        "SELECT id, pr_url FROM work_items \
         WHERE status = 'in_review' AND pr_url IS NOT NULL AND pr_url LIKE '%github.com%' \
         ORDER BY COALESCE(started_at, created_at) ASC LIMIT 25",
    )
    .fetch_all(pg)
    .await
    .context("query in_review work_items for reconcile")?;

    let mut reconciled = 0usize;
    for (id, pr_url) in rows {
        let mut cmd = gh_cmd().await;
        cmd.args(["pr", "view", &pr_url, "--json", "state,mergedAt"]);
        let out = match cmd.output().await {
            Ok(o) if o.status.success() => o,
            Ok(o) => {
                warn!(pr = %pr_url, stderr = %String::from_utf8_lossy(&o.stderr).trim(), "reconcile: gh pr view failed — leaving row");
                continue;
            }
            Err(e) => {
                warn!(pr = %pr_url, error = %e, "reconcile: gh spawn failed");
                continue;
            }
        };
        let v: serde_json::Value = match serde_json::from_slice(&out.stdout) {
            Ok(v) => v,
            Err(e) => {
                warn!(pr = %pr_url, error = %e, "reconcile: gh json parse failed");
                continue;
            }
        };
        let merged = v.get("mergedAt").and_then(|m| m.as_str()).is_some();
        let state = v.get("state").and_then(|s| s.as_str()).unwrap_or("");
        let new_status = if merged {
            "merged"
        } else if state.eq_ignore_ascii_case("CLOSED") {
            "cancelled"
        } else {
            continue; // still OPEN — nothing to reconcile
        };
        let affected = sqlx::query(
            "UPDATE work_items SET status = $2, \
             completed_at = COALESCE(completed_at, NOW()), last_error = NULL \
             WHERE id = $1 AND status = 'in_review'",
        )
        .bind(id)
        .bind(new_status)
        .execute(pg)
        .await
        .context("reconcile update work_item status")?
        .rows_affected();
        if affected > 0 {
            info!(work_item = %id, pr = %pr_url, status = new_status, "reconcile: flipped orphaned in_review");
            reconciled += 1;
        }
    }
    Ok(reconciled)
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
        let mut tick_n: u64 = 0;
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !crate::leader_cache::is_current_leader() {
                        continue;
                    }
                    if let Err(e) = evaluate_merge_queue(&pg).await {
                        warn!(error = %e, "work_item_merge_drain tick failed");
                    }
                    // Every ~20th tick, sweep for PRs merged/closed out-of-band
                    // (hand-merges) so their work_items don't rot in `in_review`.
                    tick_n = tick_n.wrapping_add(1);
                    if tick_n % 20 == 1 {
                        match reconcile_orphaned_reviews(&pg).await {
                            Ok(n) if n > 0 => info!(count = n, "work_item_merge_drain: reconciled orphaned in_review items"),
                            Ok(_) => {}
                            Err(e) => warn!(error = %e, "work_item_merge_drain reconcile failed"),
                        }
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
