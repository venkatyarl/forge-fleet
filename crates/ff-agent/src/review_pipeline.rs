//! Automated Review Pipeline — inserts `pending_review` tasks into the fleet queue.
//!
//! Triggered by:
//! - Git post-commit hooks
//! - `ff review` CLI command
//! - Periodic cron scan of unreviewed commits
//!
//! Each review task is a `fleet_task` with `task_type = 'code_review'`.
//! A fleet sub-agent claims it, runs `ff-pipeline::testing_pipeline`,
//! and posts the result back to the PR or commit comment.

use serde_json::json;
use sqlx::PgPool;
use tracing::{info, warn};
use uuid::Uuid;

/// Enqueue a code review task for a commit range.
pub async fn enqueue_review(
    pg: &PgPool,
    repo_path: &str,
    commit_sha: &str,
    branch: &str,
    author: &str,
    diff_text: &str,
) -> Result<Uuid, sqlx::Error> {
    let distributed = crate::fleet_info::distributed_review_mode_enabled();
    tracing::debug!(
        commit = %commit_sha,
        distributed_review_mode = distributed,
        "enqueue_review: fleet_secrets.distributed_review_mode read"
    );

    let payload = json!({
        "repo_path": repo_path,
        "commit_sha": commit_sha,
        "branch": branch,
        "author": author,
        "diff": diff_text,
        "pipeline": ["T1", "T2", "T3"],
    });

    let summary = format!("review {commit_sha:.8} on {branch} by {author}");

    let task_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO fleet_tasks (
            task_type, summary, payload, priority, status, created_at
        )
        VALUES ('code_review', $1, $2, 60, 'pending', NOW())
        RETURNING id
        "#,
    )
    .bind(&summary)
    .bind(&payload)
    .fetch_one(pg)
    .await?;

    info!(task_id = %task_id, commit = %commit_sha, "review task enqueued");
    Ok(task_id)
}

/// Scan a repo for commits since `last_reviewed_sha` and enqueue reviews.
pub async fn scan_and_enqueue(
    pg: &PgPool,
    repo_path: &str,
    last_reviewed_sha: Option<&str>,
) -> Result<usize, sqlx::Error> {
    let since = last_reviewed_sha.unwrap_or("HEAD~10");
    let output = tokio::process::Command::new("git")
        .args([
            "log",
            &format!("{since}..HEAD"),
            "--pretty=format:%H|%s|%an",
        ])
        .current_dir(repo_path)
        .output()
        .await;

    let mut enqueued = 0;
    match output {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            for line in text.lines() {
                let parts: Vec<&str> = line.splitn(3, '|').collect();
                if parts.len() != 3 {
                    continue;
                }
                let sha = parts[0];
                let _subject = parts[1];
                let author = parts[2];

                // Get diff for this commit
                let diff = tokio::process::Command::new("git")
                    .args(["show", sha, "--stat"])
                    .current_dir(repo_path)
                    .output()
                    .await
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                    .unwrap_or_default();

                match enqueue_review(pg, repo_path, sha, "main", author, &diff).await {
                    Ok(_) => enqueued += 1,
                    Err(e) => warn!(commit = %sha, error = %e, "failed to enqueue review"),
                }
            }
        }
        Ok(o) => {
            warn!(
                stderr = %String::from_utf8_lossy(&o.stderr),
                "git log failed"
            );
        }
        Err(e) => {
            warn!(error = %e, "git command failed");
        }
    }

    info!(enqueued, "review scan complete");
    Ok(enqueued)
}
