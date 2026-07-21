//! Create a Mission Control project-management work item.

use std::sync::LazyLock;

use ff_mc::work_item::{CreateWorkItem, WorkItem};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::handlers::HandlerResult;

const DEFAULT_MC_URL: &str = "http://127.0.0.1:60002";
static HTTP: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateParams {
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    priority: Option<i32>,
}

pub async fn pm_create(params: Option<Value>) -> HandlerResult {
    let params = parse_params(params)?;
    let endpoint = format!("{}/api/mc/work-items", mc_base_url().trim_end_matches('/'));
    let request = CreateWorkItem {
        title: params.title,
        description: params.description,
        priority: params.priority,
        ..CreateWorkItem::default()
    };

    let response = HTTP
        .post(&endpoint)
        .json(&request)
        .send()
        .await
        .map_err(|error| format!("failed to call PM creation endpoint {endpoint}: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|error| format!("failed to read response body: {error}"));
        return Err(format!(
            "PM creation endpoint returned HTTP {}: {body}",
            status.as_u16()
        ));
    }

    let item: WorkItem = response
        .json()
        .await
        .map_err(|error| format!("failed to decode PM creation response: {error}"))?;
    Ok(json!({ "created": true, "work_item": item }))
}

fn parse_params(params: Option<Value>) -> Result<CreateParams, String> {
    let params: CreateParams =
        serde_json::from_value(params.ok_or_else(|| "missing pm_create parameters".to_string())?)
            .map_err(|error| format!("invalid pm_create parameters: {error}"))?;
    if params.title.trim().is_empty() {
        return Err("title must not be empty".to_string());
    }
    if let Some(priority) = params.priority
        && !(1..=5).contains(&priority)
    {
        return Err("priority must be between 1 and 5".to_string());
    }
    Ok(params)
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
    fn parses_create_parameters() {
        let params = parse_params(Some(json!({
            "title": "Add pm_create",
            "description": "Expose work-item creation over MCP",
            "priority": 2
        })))
        .unwrap();
        assert_eq!(params.title, "Add pm_create");
        assert_eq!(params.description, "Expose work-item creation over MCP");
        assert_eq!(params.priority, Some(2));
    }

    #[test]
    fn rejects_missing_empty_invalid_and_unknown_parameters() {
        assert!(parse_params(None).is_err());
        assert!(parse_params(Some(json!({ "title": " " }))).is_err());
        assert!(parse_params(Some(json!({ "title": "task", "priority": 0 }))).is_err());
        assert!(parse_params(Some(json!({ "title": "task", "extra": true }))).is_err());
    }
}
