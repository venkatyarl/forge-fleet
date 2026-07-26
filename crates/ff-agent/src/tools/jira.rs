//! Poll Jira's configured queue and enqueue issues for fleet dispatch.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{AgentTool, AgentToolContext, AgentToolResult, shared_http_client};

const DEFAULT_CONFIG: &str = "hireflow360";

pub struct JiraQueueTool {
    client: reqwest::Client,
}

impl Default for JiraQueueTool {
    fn default() -> Self {
        Self {
            client: shared_http_client(),
        }
    }
}

#[derive(Deserialize, sqlx::FromRow)]
struct JiraConfig {
    name: String,
    jira_secret_ref: String,
    queue_jql: String,
    repo_map_json: Value,
}

#[derive(Deserialize)]
struct JiraSearchResult {
    #[serde(default)]
    issues: Vec<JiraIssue>,
}

#[derive(Deserialize)]
struct JiraIssue {
    id: String,
    key: String,
    fields: JiraFields,
}

#[derive(Default, Deserialize)]
struct JiraFields {
    summary: String,
    #[serde(default)]
    description: Option<Value>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    priority: Option<JiraNamedField>,
    #[serde(default)]
    status: Option<JiraNamedField>,
}

#[derive(Deserialize)]
struct JiraNamedField {
    name: String,
}

#[derive(sqlx::FromRow)]
struct ProjectRepo {
    id: Uuid,
    github_url: String,
    default_branch: String,
}

#[async_trait]
impl AgentTool for JiraQueueTool {
    fn name(&self) -> &str {
        "jira_queue_poll"
    }

    fn description(&self) -> &str {
        "Poll a configured Jira queue and enqueue mapped issues as ready fleet work items."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "config": {
                    "type": "string",
                    "description": "jira_configs name (defaults to hireflow360)"
                }
            },
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        match self.execute_inner(&input, ctx).await {
            Ok(value) => AgentToolResult::ok(value.to_string()),
            Err(error) => {
                AgentToolResult::err(json!({ "ok": false, "error": error.to_string() }).to_string())
            }
        }
    }
}

impl JiraQueueTool {
    async fn execute_inner(&self, input: &Value, ctx: &AgentToolContext) -> anyhow::Result<Value> {
        let config_name = input
            .get("config")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_CONFIG);
        let pool = ctx
            .pg_pool
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("database_unavailable"))?;

        let config: JiraConfig = sqlx::query_as(
            "SELECT name, jira_secret_ref, queue_jql, repo_map_json \
             FROM jira_configs WHERE name = $1",
        )
        .bind(config_name)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no Jira configuration found for {config_name}"))?;

        let base_url = required_secret(pool, &format!("jira.{}.base_url", config.name))
            .await?
            .trim_end_matches('/')
            .to_owned();
        let email = required_secret(pool, &format!("jira.{}.auth_email", config.name)).await?;
        let token = required_secret(pool, &config.jira_secret_ref).await?;

        let response = self
            .client
            .get(format!("{base_url}/rest/api/3/search/jql"))
            .basic_auth(email, Some(token))
            .query(&[
                ("jql", config.queue_jql.as_str()),
                ("fields", "summary,description,labels,priority,status"),
                ("maxResults", "100"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json::<JiraSearchResult>()
            .await?;

        let fetched = response.issues.len();
        let mut created_ids = Vec::new();
        let mut duplicates = 0usize;
        let mut unmapped = Vec::new();

        for issue in response.issues {
            let Some(repo_name) = resolve_repo_name(&issue.fields.labels, &config.repo_map_json)
            else {
                unmapped.push(issue.key);
                continue;
            };
            let repo: Option<ProjectRepo> = sqlx::query_as(
                "SELECT id, github_url, default_branch FROM project_repos \
                 WHERE project_id = $1 AND name = $2",
            )
            .bind(&config.name)
            .bind(repo_name)
            .fetch_optional(pool)
            .await?;
            let Some(repo) = repo else {
                unmapped.push(issue.key);
                continue;
            };

            let signature = jira_signature(&config.name, &issue.id);
            let original_signal = json!({
                "kind": "jira",
                "signature": signature,
                "config_id": config.name,
                "issue_id": issue.id,
                "issue_key": issue.key,
                "status": issue.fields.status.as_ref().map(|status| status.name.as_str())
            });
            let description = issue
                .fields
                .description
                .as_ref()
                .map(adf_text)
                .filter(|text| !text.is_empty());
            let metadata = json!({
                "jira_url": format!("{base_url}/browse/{}", issue.key),
                "jira_issue_id": issue.id,
                "jira_issue_key": issue.key
            });

            let inserted: Option<Uuid> = sqlx::query_scalar(
                "INSERT INTO work_items \
                 (project_id, kind, title, description, labels, status, priority, created_by, \
                  metadata, repo_id, repo_url, base_branch, original_signal) \
                 VALUES ($1, 'jira', $2, $3, $4, 'ready', $5, 'jira_queue_poll', \
                         $6, $7, $8, $9, $10) \
                 ON CONFLICT DO NOTHING RETURNING id",
            )
            .bind(&config.name)
            .bind(format!("{} {}", issue.key, issue.fields.summary))
            .bind(description)
            .bind(json!(issue.fields.labels))
            .bind(normalize_priority(
                issue
                    .fields
                    .priority
                    .as_ref()
                    .map(|priority| priority.name.as_str()),
            ))
            .bind(metadata)
            .bind(repo.id)
            .bind(repo.github_url)
            .bind(repo.default_branch)
            .bind(original_signal)
            .fetch_optional(pool)
            .await?;

            if let Some(id) = inserted {
                created_ids.push(id);
            } else {
                duplicates += 1;
            }
        }

        Ok(json!({
            "ok": true,
            "config": config.name,
            "fetched": fetched,
            "created": created_ids.len(),
            "duplicates": duplicates,
            "unmapped": unmapped,
            "created_work_item_ids": created_ids
        }))
    }
}

async fn required_secret(pool: &sqlx::PgPool, key: &str) -> anyhow::Result<String> {
    ff_db::pg_get_secret(pool, key)
        .await?
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing Jira configuration value '{key}'"))
}

fn resolve_repo_name<'a>(labels: &'a [String], repo_map: &'a Value) -> Option<&'a str> {
    let mappings = repo_map.as_object()?;
    labels
        .iter()
        .filter_map(|label| {
            mappings
                .get(label)
                .and_then(Value::as_str)
                .map(|repo| (label, repo))
        })
        .min_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, repo)| repo)
}

fn jira_signature(config: &str, issue_id: &str) -> String {
    format!("jira:{config}:{issue_id}")
}

fn normalize_priority(priority: Option<&str>) -> &'static str {
    match priority.map(str::to_ascii_lowercase).as_deref() {
        Some("highest" | "blocker" | "critical") => "critical",
        Some("high" | "major") => "high",
        Some("low" | "lowest" | "minor" | "trivial") => "low",
        _ => "normal",
    }
}

fn adf_text(value: &Value) -> String {
    fn collect(value: &Value, output: &mut Vec<String>) {
        if let Some(text) = value.get("text").and_then(Value::as_str) {
            output.push(text.to_owned());
        }
        if let Some(content) = value.get("content").and_then(Value::as_array) {
            for child in content {
                collect(child, output);
            }
        }
    }

    let mut parts = Vec::new();
    collect(value, &mut parts);
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jira_search_response() {
        let result: JiraSearchResult = serde_json::from_value(json!({
            "issues": [{
                "id": "10001",
                "key": "HFPROD-268",
                "fields": {
                    "summary": "Cycle allocation APIs",
                    "description": null,
                    "labels": ["hireflow360-api"],
                    "priority": {"name": "Highest"},
                    "status": {"name": "To Do"}
                }
            }]
        }))
        .unwrap();
        assert_eq!(result.issues[0].key, "HFPROD-268");
    }

    #[test]
    fn resolves_repo_deterministically_and_normalizes_priority() {
        let labels = vec!["web".to_owned(), "api".to_owned()];
        let map = json!({"web": "web-hireflow360", "api": "hireflow360-api"});
        assert_eq!(resolve_repo_name(&labels, &map), Some("hireflow360-api"));
        assert_eq!(normalize_priority(Some("Blocker")), "critical");
        assert_eq!(normalize_priority(Some("Medium")), "normal");
    }

    #[test]
    fn flattens_adf_and_builds_stable_signature() {
        let value = json!({
            "type": "doc",
            "content": [
                {"type": "paragraph", "content": [{"type": "text", "text": "First"}]},
                {"type": "paragraph", "content": [{"type": "text", "text": "second"}]}
            ]
        });
        assert_eq!(adf_text(&value), "First second");
        assert_eq!(
            jira_signature("hireflow360", "10001"),
            "jira:hireflow360:10001"
        );
    }

    #[test]
    fn queue_tool_is_registered_for_discovery() {
        let tools = crate::tools::all_tools();
        assert!(crate::tools::find_tool("jira_queue_poll", &tools).is_some());
    }
}
