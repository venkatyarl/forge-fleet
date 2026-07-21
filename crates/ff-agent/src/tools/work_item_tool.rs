//! Read-only access to the canonical fleet work-item queue.

use async_trait::async_trait;
use ff_db::WorkItem;
use serde_json::{Value, json};
use sqlx::PgPool;

use super::{AgentTool, AgentToolContext, AgentToolResult};

pub struct ListWorkItemsTool;

#[async_trait]
impl AgentTool for ListWorkItemsTool {
    fn name(&self) -> &str {
        "list_work_items"
    }

    fn description(&self) -> &str {
        "List fleet work items from the canonical database. Optionally filter by title, description, project, kind, status, or assignee. Returns actionable JSON for choosing work to claim or investigate."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Optional case-insensitive text filter"
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let query = input
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);

        let Some(pool) = ctx.pg_pool.as_ref() else {
            return AgentToolResult::err(error_json("database_unavailable"));
        };

        let result = list_work_items(pool, query).await;
        match serde_json::from_str::<Value>(&result) {
            Ok(value) if value.get("ok").and_then(Value::as_bool) == Some(true) => {
                AgentToolResult::ok(result)
            }
            _ => AgentToolResult::err(result),
        }
    }
}

/// Query the canonical DB model and serialize a bounded, agent-oriented result.
pub async fn list_work_items(pool: &PgPool, query: Option<String>) -> String {
    let pattern = query
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| format!("%{value}%"));

    let result = sqlx::query_as::<_, WorkItem>(
        r#"
        SELECT *
        FROM work_items
        WHERE $1::text IS NULL
           OR title ILIKE $1
           OR COALESCE(description, '') ILIKE $1
           OR project_id ILIKE $1
           OR kind ILIKE $1
           OR status ILIKE $1
           OR COALESCE(assigned_to, '') ILIKE $1
        ORDER BY
            CASE status
                WHEN 'ready' THEN 0
                WHEN 'claimed' THEN 1
                WHEN 'building' THEN 2
                WHEN 'in_progress' THEN 3
                WHEN 'blocked' THEN 4
                ELSE 5
            END,
            created_at DESC
        LIMIT 50
        "#,
    )
    .bind(pattern)
    .fetch_all(pool)
    .await;

    match result {
        Ok(items) => format_items(query, &items),
        Err(error) => error_json(&format!("database_query_failed: {error}")),
    }
}

fn format_items(query: Option<String>, items: &[WorkItem]) -> String {
    let items: Vec<Value> = items
        .iter()
        .map(|item| {
            json!({
                "id": item.id,
                "project_id": item.project_id,
                "kind": item.kind,
                "title": item.title,
                "description": item.description,
                "status": item.status,
                "priority": item.priority,
                "assigned_to": item.assigned_to,
                "assigned_computer": item.assigned_computer,
                "repository": {
                    "url": item.repo_url,
                    "path": item.repo_path,
                    "base_branch": item.base_branch,
                    "work_branch": item.branch_name
                },
                "required_capabilities": item.required_capabilities,
                "predicted_paths": item.predicted_paths,
                "attempts": item.attempts,
                "last_error": item.last_error,
                "action": action_for_status(&item.status)
            })
        })
        .collect();

    json!({
        "ok": true,
        "query": query,
        "count": items.len(),
        "truncated": items.len() == 50,
        "items": items
    })
    .to_string()
}

fn action_for_status(status: &str) -> &'static str {
    match status {
        "idea" => "review_and_mark_ready",
        "ready" => "claim_or_schedule",
        "claimed" => "start_work",
        "building" | "in_progress" => "continue_work",
        "in_review" => "review_changes",
        "blocked" => "inspect_last_error_and_unblock",
        "failed" => "inspect_last_error_and_retry",
        "done" | "cancelled" => "no_action",
        _ => "inspect_status",
    }
}

fn error_json(error: &str) -> String {
    json!({ "ok": false, "error": error, "items": [] }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_actions_are_agent_actionable() {
        assert_eq!(action_for_status("ready"), "claim_or_schedule");
        assert_eq!(
            action_for_status("blocked"),
            "inspect_last_error_and_unblock"
        );
        assert_eq!(action_for_status("done"), "no_action");
    }

    #[test]
    fn errors_are_valid_json() {
        let value: Value = serde_json::from_str(&error_json("database unavailable")).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["items"], json!([]));
    }
}
