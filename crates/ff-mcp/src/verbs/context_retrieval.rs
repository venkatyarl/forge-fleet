//! Bounded, ready-to-inject context for a Mission Control work item.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ff_agent::work_item_dispatch::{parse_cli_tokens, run_git};
use ff_db::OperationalStore;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::handlers::HandlerResult;

const DEFAULT_MAX_COMMITS: usize = 10;
const MAX_COMMITS: usize = 50;
const MAX_GIT_OUTPUT_BYTES: usize = 32 * 1024;

#[derive(Debug, Deserialize)]
struct ContextParams {
    work_item_id: String,
    repo_path: Option<String>,
    #[serde(default = "default_max_commits")]
    max_commits: usize,
}

fn default_max_commits() -> usize {
    DEFAULT_MAX_COMMITS
}

pub async fn work_item_context(params: Option<Value>) -> HandlerResult {
    let params: ContextParams =
        serde_json::from_value(params.ok_or_else(|| "work_item_id is required".to_string())?)
            .map_err(|error| format!("invalid work_item_context parameters: {error}"))?;
    let work_item_id = params.work_item_id.trim();
    if work_item_id.is_empty() {
        return Err("work_item_id must not be empty".to_string());
    }
    if params.max_commits > MAX_COMMITS {
        return Err(format!("max_commits must not exceed {MAX_COMMITS}"));
    }

    let pool = crate::pool::shared_pg_pool().await?;
    let store = OperationalStore::postgres_with_pool(Arc::new(pool))
        .await
        .map_err(|error| format!("failed to open operational store: {error}"))?;
    let item = ff_mc::operational_api::get_work_item_from_store(&store, work_item_id)
        .await
        .map_err(|error| format!("failed to retrieve work item '{work_item_id}': {error}"))?;

    let repository = params
        .repo_path
        .as_deref()
        .map(|path| git_context(Path::new(path), params.max_commits))
        .transpose()?;
    let rendered_context = render_context(&item, repository.as_ref());

    Ok(json!({
        "work_item": item,
        "repository": repository,
        "reported_cli_tokens": parse_cli_tokens(&rendered_context),
        "rendered_context": rendered_context,
    }))
}

fn git_context(repo: &Path, max_commits: usize) -> Result<Value, String> {
    if !repo.is_dir() {
        return Err(format!("repo_path is not a directory: {}", repo.display()));
    }

    let branch = git_stdout(repo, ["rev-parse", "--abbrev-ref", "HEAD"])?;
    let head = git_stdout(repo, ["rev-parse", "HEAD"])?;
    let status = git_stdout(repo, ["status", "--short"])?;
    let log = if max_commits == 0 {
        String::new()
    } else {
        git_stdout(
            repo,
            [
                "log",
                "--oneline",
                "--decorate=no",
                &format!("-{max_commits}"),
            ],
        )?
    };

    Ok(json!({
        "path": repo,
        "branch": branch,
        "head": head,
        "status": status,
        "recent_commits": log.lines().collect::<Vec<_>>(),
    }))
}

fn git_stdout<const N: usize>(repo: &Path, args: [&str; N]) -> Result<String, String> {
    let output = run_git(repo, args, Duration::from_secs(10))
        .map_err(|error| format!("git command failed in {}: {error}", repo.display()))?;
    let stdout = String::from_utf8(output.stdout)
        .map_err(|error| format!("git returned non-UTF-8 output: {error}"))?;
    Ok(truncate(stdout.trim(), MAX_GIT_OUTPUT_BYTES))
}

fn truncate(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… truncated", &value[..end])
}

fn render_context(item: &ff_mc::work_item::WorkItem, repository: Option<&Value>) -> String {
    let mut rendered = format!(
        "Work item {}: {}\nStatus: {}\nPriority: {}\nAssignee: {}\n\n{}",
        item.id,
        item.title,
        item.status,
        item.priority.label(),
        item.assignee,
        item.description
    );
    if let Some(repository) = repository {
        rendered.push_str("\n\nRepository context:\n");
        rendered.push_str(&serde_json::to_string_pretty(repository).unwrap_or_default());
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_preserves_utf8_boundaries() {
        assert_eq!(truncate("aébc", 2), "a\n… truncated");
        assert_eq!(truncate("short", 32), "short");
    }

    #[tokio::test]
    async fn rejects_missing_parameters_without_opening_database() {
        assert_eq!(
            work_item_context(None).await.unwrap_err(),
            "work_item_id is required"
        );
        assert_eq!(
            work_item_context(Some(json!({ "work_item_id": " " })))
                .await
                .unwrap_err(),
            "work_item_id must not be empty"
        );
    }

    #[tokio::test]
    async fn rejects_excessive_commit_limit_without_opening_database() {
        let error = work_item_context(Some(json!({
            "work_item_id": "work-1",
            "max_commits": MAX_COMMITS + 1,
        })))
        .await
        .unwrap_err();
        assert_eq!(error, "max_commits must not exceed 50");
    }
}
