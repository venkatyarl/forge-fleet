//! Jira transition tool for completing a work item after its change is merged.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{AgentTool, AgentToolContext, AgentToolResult, shared_http_client};

pub struct JiraTransitionTool {
    client: reqwest::Client,
}

impl Default for JiraTransitionTool {
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
}

#[derive(Deserialize)]
struct JiraTransitions {
    #[serde(default)]
    transitions: Vec<JiraTransition>,
}

#[derive(Deserialize)]
struct JiraTransition {
    id: String,
    name: String,
    to: JiraStatus,
}

#[derive(Deserialize)]
struct JiraStatus {
    name: String,
}

#[async_trait]
impl AgentTool for JiraTransitionTool {
    fn name(&self) -> &str {
        "jira_transition"
    }

    fn description(&self) -> &str {
        "Transition the Jira issue associated with a merged fleet work item and add a comment."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "work_item_id": {
                    "type": "string",
                    "format": "uuid",
                    "description": "Fleet work item UUID"
                },
                "target_status": {
                    "type": "string",
                    "description": "Jira destination status name"
                },
                "comment": {
                    "type": "string",
                    "description": "Comment to add to the Jira issue"
                }
            },
            "required": ["work_item_id", "target_status", "comment"],
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

impl JiraTransitionTool {
    async fn execute_inner(&self, input: &Value, ctx: &AgentToolContext) -> anyhow::Result<Value> {
        let work_item_id = required_string(input, "work_item_id")?
            .parse::<Uuid>()
            .map_err(|_| anyhow::anyhow!("work_item_id must be a valid UUID"))?;
        let target_status = required_string(input, "target_status")?;
        let comment = required_string(input, "comment")?;
        let pool = ctx
            .pg_pool
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("database_unavailable"))?;

        let (project_id, title): (String, String) =
            sqlx::query_as("SELECT project_id, title FROM work_items WHERE id = $1")
                .bind(work_item_id)
                .fetch_optional(pool)
                .await?
                .ok_or_else(|| anyhow::anyhow!("work item {work_item_id} was not found"))?;
        let issue_key = jira_issue_key(&title).ok_or_else(|| {
            anyhow::anyhow!("work item title does not start with a Jira issue key")
        })?;

        let config: JiraConfig =
            sqlx::query_as("SELECT name, jira_secret_ref FROM jira_configs WHERE name = $1")
                .bind(&project_id)
                .fetch_optional(pool)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!("no Jira configuration found for project {project_id}")
                })?;
        let base_url = required_secret(pool, &format!("jira.{}.base_url", config.name))
            .await?
            .trim_end_matches('/')
            .to_owned();
        let email = required_secret(pool, &format!("jira.{}.auth_email", config.name)).await?;
        let token = required_secret(pool, &config.jira_secret_ref).await?;
        let transitions_url = format!("{base_url}/rest/api/3/issue/{issue_key}/transitions");

        let response = self
            .client
            .get(&transitions_url)
            .basic_auth(&email, Some(&token))
            .send()
            .await?
            .error_for_status()?
            .json::<JiraTransitions>()
            .await?;
        let transition = find_transition(&response.transitions, target_status).ok_or_else(|| {
            let available = response
                .transitions
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::anyhow!(
                "Jira status '{target_status}' is not available for {issue_key}; available transitions: {available}"
            )
        })?;

        self.client
            .post(&transitions_url)
            .basic_auth(&email, Some(&token))
            .json(&json!({ "transition": { "id": transition.id } }))
            .send()
            .await?
            .error_for_status()?;

        self.client
            .post(format!("{base_url}/rest/api/3/issue/{issue_key}/comment"))
            .basic_auth(&email, Some(&token))
            .json(&json!({
                "body": {
                    "type": "doc",
                    "version": 1,
                    "content": [{
                        "type": "paragraph",
                        "content": [{ "type": "text", "text": comment }]
                    }]
                }
            }))
            .send()
            .await?
            .error_for_status()?;

        Ok(json!({
            "ok": true,
            "work_item_id": work_item_id,
            "issue_key": issue_key,
            "target_status": transition.to.name,
            "comment_added": true
        }))
    }
}

fn required_string<'a>(input: &'a Value, field: &str) -> anyhow::Result<&'a str> {
    input
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing or empty '{field}'"))
}

async fn required_secret(pool: &sqlx::PgPool, key: &str) -> anyhow::Result<String> {
    ff_db::pg_get_secret(pool, key)
        .await?
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing Jira configuration value '{key}'"))
}

fn jira_issue_key(title: &str) -> Option<&str> {
    let candidate = title.split_whitespace().next()?;
    let (project, number) = candidate.rsplit_once('-')?;
    (!project.is_empty()
        && project
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        && !number.is_empty()
        && number.bytes().all(|byte| byte.is_ascii_digit()))
    .then_some(candidate)
}

fn find_transition<'a>(
    transitions: &'a [JiraTransition],
    target_status: &str,
) -> Option<&'a JiraTransition> {
    transitions.iter().find(|transition| {
        transition.name.eq_ignore_ascii_case(target_status)
            || transition.to.name.eq_ignore_ascii_case(target_status)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_issue_key_from_work_item_title() {
        assert_eq!(
            jira_issue_key("HFPROD-268 cycle allocation APIs"),
            Some("HFPROD-268")
        );
        assert_eq!(jira_issue_key("cycle allocation APIs"), None);
        assert_eq!(jira_issue_key("hfprod-268 cycle allocation APIs"), None);
    }

    #[test]
    fn matches_transition_or_destination_name_case_insensitively() {
        let transitions = vec![JiraTransition {
            id: "31".into(),
            name: "Complete work".into(),
            to: JiraStatus {
                name: "Done".into(),
            },
        }];
        assert_eq!(
            find_transition(&transitions, "done").map(|item| item.id.as_str()),
            Some("31")
        );
        assert_eq!(
            find_transition(&transitions, "COMPLETE WORK").map(|item| item.id.as_str()),
            Some("31")
        );
    }
}
