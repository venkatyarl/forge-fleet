//! List Mission Control project-management work items.

use std::sync::LazyLock;

use ff_mc::work_item::WorkItem;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::handlers::HandlerResult;

const DEFAULT_MC_URL: &str = "http://127.0.0.1:60002";
static HTTP: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    assignee: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    epic_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sprint_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_group_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}

pub async fn pm_list(params: Option<Value>) -> HandlerResult {
    let params = parse_params(params)?;
    let endpoint = format!("{}/api/mc/work-items", mc_base_url().trim_end_matches('/'));
    let response = HTTP
        .get(&endpoint)
        .query(&params)
        .send()
        .await
        .map_err(|error| format!("failed to call PM list endpoint {endpoint}: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|error| format!("failed to read response body: {error}"));
        return Err(format!(
            "PM list endpoint returned HTTP {}: {body}",
            status.as_u16()
        ));
    }

    let work_items: Vec<WorkItem> = response
        .json()
        .await
        .map_err(|error| format!("failed to decode PM list response: {error}"))?;
    Ok(json!({ "count": work_items.len(), "work_items": work_items }))
}

fn parse_params(params: Option<Value>) -> Result<ListParams, String> {
    serde_json::from_value(params.unwrap_or_else(|| json!({})))
        .map_err(|error| format!("invalid pm_list parameters: {error}"))
}

fn mc_base_url() -> String {
    std::env::var("FORGEFLEET_MC_URL")
        .or_else(|_| std::env::var("FF_MC_URL"))
        .unwrap_or_else(|_| DEFAULT_MC_URL.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_empty_and_filtered_parameters() {
        assert!(parse_params(None).is_ok());
        let params = parse_params(Some(json!({
            "status": "backlog",
            "assignee": "codex",
            "label": "mcp"
        })))
        .unwrap();
        assert_eq!(params.status.as_deref(), Some("backlog"));
        assert_eq!(params.assignee.as_deref(), Some("codex"));
        assert_eq!(params.label.as_deref(), Some("mcp"));
    }

    #[test]
    fn rejects_unknown_parameters() {
        assert!(parse_params(Some(json!({ "project": "unknown" }))).is_err());
    }
}
