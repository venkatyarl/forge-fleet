//! Canonical model availability route.

use std::{collections::BTreeMap, sync::Arc};

use axum::{Json, extract::State};
use serde::Serialize;
use serde_json::Value;
use sqlx::{FromRow, PgPool};

use crate::{error::ApiError, server::AppState};

/// Canonical catalog metadata enriched with its current fleet deployments.
#[derive(Debug, Clone, Serialize)]
pub struct AvailableModel {
    pub id: String,
    pub name: String,
    pub params: Option<String>,
    pub tier: Option<i32>,
    pub where_running: Vec<String>,
    pub healthy: bool,
    pub workloads: Vec<String>,
}

#[derive(Debug, FromRow)]
struct AvailableModelRow {
    model_id: String,
    name: String,
    parameters: Option<String>,
    tier: Option<i32>,
    preferred_workloads: Value,
    worker_name: String,
    health_status: String,
}

const AVAILABLE_MODELS_SQL: &str = "\
    SELECT c.id AS model_id, \
           COALESCE(c.display_name, c.name, c.id) AS name, \
           c.parameters, \
           c.tier, \
           c.preferred_workloads, \
           d.worker_name, \
           d.health_status \
      FROM fleet_model_deployments d \
      JOIN fleet_model_catalog c ON c.id = d.catalog_id \
     WHERE d.desired_state = 'active' \
     ORDER BY c.id, d.worker_name";

fn db_pool(state: &AppState) -> Result<&PgPool, ApiError> {
    state
        .db_pool
        .as_ref()
        .ok_or_else(|| ApiError::BackendUnavailable("database not configured".to_string()))
}

/// List canonical models that have an active deployment in the fleet.
pub async fn available(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<AvailableModel>>, ApiError> {
    let rows = sqlx::query_as::<_, AvailableModelRow>(AVAILABLE_MODELS_SQL)
        .fetch_all(db_pool(&state)?)
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?;

    Ok(Json(group_rows(rows)))
}

fn group_rows(rows: Vec<AvailableModelRow>) -> Vec<AvailableModel> {
    let mut models = BTreeMap::<String, AvailableModel>::new();
    for row in rows {
        let is_healthy = row.health_status == "healthy";
        let model = models
            .entry(row.model_id.clone())
            .or_insert_with(|| AvailableModel {
                id: row.model_id,
                name: row.name,
                params: row.parameters,
                tier: row.tier,
                where_running: Vec::new(),
                healthy: false,
                workloads: parse_workloads(&row.preferred_workloads),
            });
        model.where_running.push(row.worker_name);
        model.healthy |= is_healthy;
    }
    models.into_values().collect()
}

fn parse_workloads(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(model_id: &str, worker: &str, health_status: &str) -> AvailableModelRow {
        AvailableModelRow {
            model_id: model_id.to_string(),
            name: model_id.to_string(),
            parameters: Some("7B".to_string()),
            tier: Some(1),
            preferred_workloads: serde_json::json!(["code", "tool_calling"]),
            worker_name: worker.to_string(),
            health_status: health_status.to_string(),
        }
    }

    #[test]
    fn groups_locations_and_health_by_canonical_model() {
        let grouped = group_rows(vec![
            row("qwen-7b", "node-a", "healthy"),
            row("qwen-7b", "node-b", "unhealthy"),
            row("llama-70b", "node-c", "unhealthy"),
        ]);

        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].id, "llama-70b");
        assert!(!grouped[0].healthy);
        assert_eq!(grouped[1].where_running, ["node-a", "node-b"]);
        assert!(grouped[1].healthy);
        assert_eq!(grouped[1].workloads, ["code", "tool_calling"]);
    }

    #[tokio::test]
    async fn available_query_matches_live_schema_when_configured() {
        let Ok(url) = std::env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
        else {
            return;
        };
        let pool = PgPool::connect(&url).await.unwrap();
        sqlx::query_as::<_, AvailableModelRow>(AVAILABLE_MODELS_SQL)
            .fetch_all(&pool)
            .await
            .unwrap();
    }
}
