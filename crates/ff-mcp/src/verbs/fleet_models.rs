//! Canonical model info — joins `fleet_model_deployments` with
//! `fleet_model_catalog` for enriched, real-time model data: parameters,
//! tier, deployment locations, and health status, grouped per catalog model.

use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

use crate::handlers::HandlerResult;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FleetModelsParams {
    #[serde(default)]
    catalog_id: Option<String>,
    #[serde(default)]
    node: Option<String>,
}

pub async fn fleet_models(params: Option<Value>) -> HandlerResult {
    let params = parse_params(params)?;
    let pool = crate::pool::shared_pg_pool().await?;

    let catalog = ff_db::pg_list_catalog(&pool)
        .await
        .map_err(|error| format!("failed to query model catalog: {error}"))?;
    let deployments = ff_db::pg_list_deployments(&pool, params.node.as_deref())
        .await
        .map_err(|error| format!("failed to query model deployments: {error}"))?;

    let mut by_catalog: BTreeMap<&str, Vec<&ff_db::ModelDeploymentRow>> = BTreeMap::new();
    for deployment in &deployments {
        if let Some(catalog_id) = deployment.catalog_id.as_deref() {
            by_catalog.entry(catalog_id).or_default().push(deployment);
        }
    }

    let models: Vec<Value> = catalog
        .iter()
        .filter(|entry| params.catalog_id.as_deref().is_none_or(|id| id == entry.id))
        .filter(|entry| params.node.is_none() || by_catalog.contains_key(entry.id.as_str()))
        .map(|entry| {
            let deploys = by_catalog
                .get(entry.id.as_str())
                .cloned()
                .unwrap_or_default();
            json!({
                "id": entry.id,
                "name": entry.name,
                "family": entry.family,
                "parameters": entry.parameters,
                "tier": entry.tier,
                "gated": entry.gated,
                "tool_calling": entry.tool_calling,
                "description": entry.description,
                "deployment_count": deploys.len(),
                "deployments": deploys.iter().map(|d| json!({
                    "worker_name": d.worker_name,
                    "port": d.port,
                    "runtime": d.runtime,
                    "pid": d.pid,
                    "health_status": d.health_status,
                    "started_at": d.started_at,
                    "last_health_at": d.last_health_at,
                    "context_window": d.context_window,
                    "parallel_slots": d.parallel_slots,
                    "usable_agent_ctx": d.usable_agent_ctx,
                    "tokens_used": d.tokens_used,
                    "request_count": d.request_count,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();

    Ok(json!({
        "count": models.len(),
        "catalog_id_filter": params.catalog_id,
        "node_filter": params.node,
        "models": models,
    }))
}

fn parse_params(params: Option<Value>) -> Result<FleetModelsParams, String> {
    let params: FleetModelsParams = serde_json::from_value(params.unwrap_or_else(|| json!({})))
        .map_err(|error| format!("invalid fleet_models parameters: {error}"))?;
    if params
        .catalog_id
        .as_ref()
        .is_some_and(|id| id.trim().is_empty())
    {
        return Err("catalog_id must not be empty".to_string());
    }
    if params
        .node
        .as_ref()
        .is_some_and(|node| node.trim().is_empty())
    {
        return Err("node must not be empty".to_string());
    }
    Ok(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_and_explicit_filters() {
        let params = parse_params(None).unwrap();
        assert!(params.catalog_id.is_none());
        assert!(params.node.is_none());

        let params = parse_params(Some(json!({
            "catalog_id": "qwen3-30b",
            "node": "fleet-node-1"
        })))
        .unwrap();
        assert_eq!(params.catalog_id.as_deref(), Some("qwen3-30b"));
        assert_eq!(params.node.as_deref(), Some("fleet-node-1"));
    }

    #[test]
    fn rejects_blank_and_unknown_parameters() {
        assert!(parse_params(Some(json!({ "catalog_id": " " }))).is_err());
        assert!(parse_params(Some(json!({ "node": " " }))).is_err());
        assert!(parse_params(Some(json!({ "unexpected": true }))).is_err());
    }
}
