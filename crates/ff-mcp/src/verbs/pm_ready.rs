//! Flag a project-management work item ready for fleet scheduling.

use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::FromRow;
use uuid::Uuid;

use crate::handlers::HandlerResult;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadyParams {
    work_item_id: Uuid,
    #[serde(default)]
    on: Option<String>,
}

#[derive(Debug, FromRow, serde::Serialize)]
struct ReadyItem {
    id: Uuid,
    title: String,
    kind: String,
    status: String,
    assigned_computer: Option<String>,
}

pub async fn pm_ready(params: Option<Value>) -> HandlerResult {
    let params = parse_params(params)?;
    let pool = crate::pool::shared_pg_pool().await?;
    let item = sqlx::query_as::<_, ReadyItem>(
        "UPDATE work_items \
            SET status = 'ready', \
                assigned_computer = COALESCE($2, assigned_computer) \
          WHERE id = $1 \
          RETURNING id, title, kind, status, assigned_computer",
    )
    .bind(params.work_item_id)
    .bind(params.on)
    .fetch_optional(&pool)
    .await
    .map_err(|error| format!("failed to flag work item ready: {error}"))?
    .ok_or_else(|| format!("work item {} not found", params.work_item_id))?;

    Ok(json!({ "ready": true, "work_item": item }))
}

fn parse_params(params: Option<Value>) -> Result<ReadyParams, String> {
    let params: ReadyParams =
        serde_json::from_value(params.ok_or_else(|| "missing pm_ready parameters".to_string())?)
            .map_err(|error| format!("invalid pm_ready parameters: {error}"))?;
    if params.on.as_ref().is_some_and(|on| on.trim().is_empty()) {
        return Err("on must not be empty".to_string());
    }
    Ok(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ready_parameters() {
        let id = Uuid::new_v4();
        let params = parse_params(Some(json!({
            "work_item_id": id,
            "on": "fleet-node-1"
        })))
        .unwrap();
        assert_eq!(params.work_item_id, id);
        assert_eq!(params.on.as_deref(), Some("fleet-node-1"));
    }

    #[test]
    fn rejects_missing_empty_and_unknown_parameters() {
        assert!(parse_params(None).is_err());
        assert!(
            parse_params(Some(json!({
                "work_item_id": Uuid::new_v4(),
                "on": " "
            })))
            .is_err()
        );
        assert!(
            parse_params(Some(json!({
                "work_item_id": Uuid::new_v4(),
                "extra": true
            })))
            .is_err()
        );
    }
}
