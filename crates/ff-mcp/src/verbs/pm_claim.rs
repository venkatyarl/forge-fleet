//! Claim a project-management work item for an agent context.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::FromRow;
use uuid::Uuid;

use crate::handlers::HandlerResult;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimParams {
    work_item_id: Uuid,
    agent: String,
    #[serde(default)]
    context: Map<String, Value>,
}

#[derive(Debug, FromRow, Serialize)]
struct ClaimedItem {
    id: Uuid,
    title: String,
    status: String,
    assigned_to: String,
    context: Value,
    started_at: Option<DateTime<Utc>>,
}

pub async fn pm_claim(params: Option<Value>) -> HandlerResult {
    let params = parse_params(params)?;
    let pool = crate::pool::shared_pg_pool().await?;
    let item = sqlx::query_as::<_, ClaimedItem>(
        "UPDATE work_items \
            SET assigned_to = $2, \
                status = CASE WHEN status = 'idea' THEN 'ready' ELSE status END, \
                context = COALESCE(context, '{}'::jsonb) || $3::jsonb, \
                started_at = COALESCE(started_at, NOW()) \
          WHERE id = $1 AND (assigned_to IS NULL OR assigned_to = $2) \
          RETURNING id, title, status, assigned_to, context, started_at",
    )
    .bind(params.work_item_id)
    .bind(&params.agent)
    .bind(Value::Object(params.context))
    .fetch_optional(&pool)
    .await
    .map_err(|error| format!("failed to claim work item: {error}"))?;

    match item {
        Some(item) => Ok(json!({ "claimed": true, "work_item": item })),
        None => {
            let owner = sqlx::query_scalar::<_, Option<String>>(
                "SELECT assigned_to FROM work_items WHERE id = $1",
            )
            .bind(params.work_item_id)
            .fetch_optional(&pool)
            .await
            .map_err(|error| format!("failed to inspect work item claim: {error}"))?;
            match owner {
                None => Err(format!("work item {} not found", params.work_item_id)),
                Some(owner) => Err(format!(
                    "work item {} is already claimed by {}",
                    params.work_item_id,
                    owner.unwrap_or_else(|| "another agent".to_string())
                )),
            }
        }
    }
}

fn parse_params(params: Option<Value>) -> Result<ClaimParams, String> {
    let params: ClaimParams =
        serde_json::from_value(params.ok_or_else(|| "missing pm_claim parameters".to_string())?)
            .map_err(|error| format!("invalid pm_claim parameters: {error}"))?;
    if params.agent.trim().is_empty() {
        return Err("agent must not be empty".to_string());
    }
    Ok(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claim_context() {
        let id = Uuid::new_v4();
        let params = parse_params(Some(json!({
            "work_item_id": id,
            "agent": "codex",
            "context": { "cwd": "/tmp/repo" }
        })))
        .unwrap();
        assert_eq!(params.work_item_id, id);
        assert_eq!(params.agent, "codex");
        assert_eq!(params.context["cwd"], "/tmp/repo");
    }

    #[test]
    fn rejects_missing_empty_and_unknown_parameters() {
        assert!(parse_params(None).is_err());
        assert!(
            parse_params(Some(json!({
                "work_item_id": Uuid::new_v4(),
                "agent": " "
            })))
            .is_err()
        );
        assert!(
            parse_params(Some(json!({
                "work_item_id": Uuid::new_v4(),
                "agent": "codex",
                "extra": true
            })))
            .is_err()
        );
    }
}
