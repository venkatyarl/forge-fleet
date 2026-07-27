//! Leader-gated Jira queue ingestion.
//!
//! The daemon registry supplies the leader gate. This tick fetches the
//! `hireflow360` queue, then atomically refreshes Jira-owned work-item fields
//! and the existing `jira_watch_state` rows without disturbing scheduler state.

use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

const CONFIG_NAME: &str = "hireflow360";
const PROJECT_ID: &str = "hireflow360";

#[derive(sqlx::FromRow)]
struct JiraConfig {
    name: String,
    project_key: String,
    jira_secret_ref: String,
    queue_jql: String,
}

#[derive(Debug, Deserialize)]
struct JiraSearchPage {
    #[serde(default)]
    issues: Vec<JiraIssue>,
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
    #[serde(default, rename = "isLast")]
    is_last: bool,
}

#[derive(Debug, Deserialize)]
struct JiraIssue {
    id: String,
    key: String,
    fields: JiraFields,
}

#[derive(Debug, Default, Deserialize)]
struct JiraFields {
    summary: String,
    #[serde(default)]
    description: Option<Value>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    priority: Option<NamedField>,
    #[serde(default)]
    status: Option<NamedField>,
    #[serde(default)]
    assignee: Option<JiraUser>,
}

#[derive(Debug, Deserialize)]
struct NamedField {
    name: String,
}

#[derive(Debug, Deserialize)]
struct JiraUser {
    #[serde(rename = "accountId")]
    account_id: String,
}

/// Poll and ingest the configured HireFlow360 Jira queue once.
///
/// This is registered as `LeaderOnly` by the daemon. A transaction advisory
/// lock additionally serializes writes across leadership handoff or overlapping
/// invocations.
pub async fn run_jira_ingest_tick(pg: &PgPool) -> Result<()> {
    let config: Option<JiraConfig> = sqlx::query_as(
        "SELECT name, project_key, jira_secret_ref, queue_jql \
           FROM jira_configs WHERE name = $1",
    )
    .bind(CONFIG_NAME)
    .fetch_optional(pg)
    .await?;
    let Some(config) = config else {
        tracing::debug!("Jira ingestion skipped: hireflow360 config is absent");
        return Ok(());
    };

    ensure!(
        !config.queue_jql.trim().is_empty(),
        "Jira queue_jql is empty"
    );
    ensure!(
        !config.project_key.trim().is_empty(),
        "Jira project_key is empty"
    );

    let base_url = required_secret(pg, &format!("jira.{}.base_url", config.name))
        .await?
        .trim_end_matches('/')
        .to_owned();
    let email = required_secret(pg, &format!("jira.{}.auth_email", config.name)).await?;
    let token = required_secret(pg, &config.jira_secret_ref).await?;
    let issues =
        fetch_all_issues(&reqwest::Client::new(), &base_url, &email, &token, &config).await?;

    let mut tx = pg.begin().await?;
    let locked: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtext('jira_ingest:hireflow360'))")
            .fetch_one(&mut *tx)
            .await?;
    if !locked {
        tracing::debug!("Jira ingestion skipped: another tick owns the write lock");
        return Ok(());
    }

    for issue in issues.values() {
        upsert_issue(&mut tx, &base_url, &config, issue).await?;
    }
    tx.commit().await?;

    tracing::info!(
        config = CONFIG_NAME,
        issue_count = issues.len(),
        "Jira ingestion tick completed"
    );
    Ok(())
}

async fn fetch_all_issues(
    client: &reqwest::Client,
    base_url: &str,
    email: &str,
    token: &str,
    config: &JiraConfig,
) -> Result<BTreeMap<String, JiraIssue>> {
    let mut issues = BTreeMap::new();
    let mut next_page_token: Option<String> = None;

    loop {
        let mut request = client
            .get(format!("{base_url}/rest/api/3/search/jql"))
            .basic_auth(email, Some(token))
            .query(&[
                ("jql", config.queue_jql.as_str()),
                (
                    "fields",
                    "summary,description,labels,priority,status,assignee",
                ),
                ("maxResults", "100"),
            ]);
        if let Some(token) = next_page_token.as_deref() {
            request = request.query(&[("nextPageToken", token)]);
        }

        let page = request
            .send()
            .await
            .context("query Jira queue")?
            .error_for_status()
            .context("Jira queue response")?
            .json::<JiraSearchPage>()
            .await
            .context("decode Jira queue response")?;

        for issue in page.issues {
            ensure!(
                issue
                    .key
                    .strip_prefix(&config.project_key)
                    .is_some_and(|suffix| suffix.starts_with('-')),
                "Jira returned issue {} outside project {}",
                issue.key,
                config.project_key
            );
            issues.insert(issue.id.clone(), issue);
        }

        let token = page
            .next_page_token
            .filter(|token| !token.trim().is_empty());
        if page.is_last || token.is_none() {
            break;
        }
        ensure!(
            token != next_page_token,
            "Jira pagination repeated nextPageToken"
        );
        next_page_token = token;
    }

    Ok(issues)
}

async fn upsert_issue(
    tx: &mut Transaction<'_, Postgres>,
    base_url: &str,
    config: &JiraConfig,
    issue: &JiraIssue,
) -> Result<()> {
    let description = issue
        .fields
        .description
        .as_ref()
        .map(adf_text)
        .filter(|text| !text.is_empty());
    let status = issue
        .fields
        .status
        .as_ref()
        .map(|field| field.name.as_str());
    let assignee = issue
        .fields
        .assignee
        .as_ref()
        .map(|user| user.account_id.as_str());
    let metadata = json!({
        "jira_url": format!("{base_url}/browse/{}", issue.key),
        "jira_issue_id": issue.id,
        "jira_issue_key": issue.key,
        "jira_project_key": config.project_key,
        "jira_status": status
    });
    let original_signal = json!({
        "kind": "jira",
        "signature": format!("jira:{}:{}", config.name, issue.id),
        "config_id": config.name,
        "issue_id": issue.id,
        "issue_key": issue.key,
        "status": status
    });
    let title = format!("{} {}", issue.key, issue.fields.summary);
    let labels = json!(issue.fields.labels);
    let priority = normalize_priority(
        issue
            .fields
            .priority
            .as_ref()
            .map(|field| field.name.as_str()),
    );

    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM work_items \
          WHERE project_id = $1 AND kind = 'jira' \
            AND metadata->>'jira_issue_id' = $2 \
          ORDER BY created_at LIMIT 1 FOR UPDATE",
    )
    .bind(PROJECT_ID)
    .bind(&issue.id)
    .fetch_optional(&mut **tx)
    .await?;

    if let Some(id) = existing {
        sqlx::query(
            "UPDATE work_items \
                SET title = $2, description = $3, labels = $4, priority = $5, \
                    metadata = metadata || $6, original_signal = $7 \
              WHERE id = $1",
        )
        .bind(id)
        .bind(title)
        .bind(description)
        .bind(labels)
        .bind(priority)
        .bind(metadata.clone())
        .bind(original_signal)
        .execute(&mut **tx)
        .await?;
    } else {
        sqlx::query(
            "INSERT INTO work_items \
                (project_id, kind, title, description, labels, status, priority, \
                 created_by, metadata, original_signal) \
             VALUES ($1, 'jira', $2, $3, $4, 'ready', $5, \
                     'jira_ingest_tick', $6, $7)",
        )
        .bind(PROJECT_ID)
        .bind(title)
        .bind(description)
        .bind(labels)
        .bind(priority)
        .bind(metadata.clone())
        .bind(original_signal)
        .execute(&mut **tx)
        .await?;
    }

    sqlx::query(
        "INSERT INTO jira_watch_state \
            (config_id, issue_id, last_seen_status, last_seen_assignee_id, state_json) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (config_id, issue_id) DO UPDATE SET \
            last_seen_status = EXCLUDED.last_seen_status, \
            last_seen_assignee_id = EXCLUDED.last_seen_assignee_id, \
            state_json = jira_watch_state.state_json || EXCLUDED.state_json",
    )
    .bind(&config.name)
    .bind(&issue.id)
    .bind(status)
    .bind(assignee)
    .bind(metadata)
    .execute(&mut **tx)
    .await?;

    Ok(())
}

async fn required_secret(pg: &PgPool, key: &str) -> Result<String> {
    ff_db::pg_get_secret(pg, key)
        .await?
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("missing Jira configuration value '{key}'"))
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

    let mut output = Vec::new();
    collect(value, &mut output);
    output.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_paginated_jira_response() {
        let page: JiraSearchPage = serde_json::from_value(json!({
            "issues": [{
                "id": "10001",
                "key": "HFPROD-42",
                "fields": {"summary": "Repair queue", "status": {"name": "To Do"}}
            }],
            "nextPageToken": "page-2",
            "isLast": false
        }))
        .unwrap();

        assert_eq!(page.issues[0].key, "HFPROD-42");
        assert_eq!(page.next_page_token.as_deref(), Some("page-2"));
        assert!(!page.is_last);
    }

    #[test]
    fn maps_jira_priorities() {
        assert_eq!(normalize_priority(Some("Highest")), "critical");
        assert_eq!(normalize_priority(Some("Major")), "high");
        assert_eq!(normalize_priority(Some("Minor")), "low");
        assert_eq!(normalize_priority(None), "normal");
    }

    #[test]
    fn extracts_adf_description_text() {
        let value = json!({
            "type": "doc",
            "content": [{
                "type": "paragraph",
                "content": [{"type": "text", "text": "first"}]
            }, {
                "type": "paragraph",
                "content": [{"type": "text", "text": "second"}]
            }]
        });

        assert_eq!(adf_text(&value), "first\nsecond");
    }
}
