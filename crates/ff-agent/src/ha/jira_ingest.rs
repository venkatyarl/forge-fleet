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
const JIRA_BLOCKED_HOLD: &str = "jira_blocked";
const JIRA_HOLD_PRIOR_STATUS: &str = "jira_hold_prior_status";
const JIRA_HOLD_PRIOR_PARKED: &str = "jira_hold_prior_parked";

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
    let blocked_status = is_jira_blocked_status(status);
    let resumable_status = is_jira_resumable_status(status);
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

    let work_item_id = if let Some(id) = existing {
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
        id
    } else {
        sqlx::query_scalar(
            "INSERT INTO work_items \
                (project_id, kind, title, description, labels, status, priority, \
                 created_by, metadata, original_signal) \
             VALUES ($1, 'jira', $2, $3, $4, 'ready', $5, \
                     'jira_ingest_tick', $6, $7) \
             RETURNING id",
        )
        .bind(PROJECT_ID)
        .bind(title)
        .bind(description)
        .bind(labels)
        .bind(priority)
        .bind(metadata.clone())
        .bind(original_signal)
        .fetch_one(&mut **tx)
        .await?
    };

    if blocked_status {
        hold_jira_parent_and_descendants(tx, work_item_id).await?;
    } else if resumable_status {
        restore_jira_parent_and_descendants(tx, work_item_id).await?;
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

async fn hold_jira_parent_and_descendants(
    tx: &mut Transaction<'_, Postgres>,
    parent_id: Uuid,
) -> Result<()> {
    sqlx::query(
        "UPDATE work_items \
            SET status = 'blocked', \
                metadata = metadata \
                    || jsonb_build_object( \
                        'jira_execution_hold', $2::text, \
                        'jira_held_at', NOW(), \
                        'jira_hold_prior_status', \
                            COALESCE(metadata->'jira_hold_prior_status', to_jsonb(status)), \
                        'jira_hold_prior_parked', \
                            COALESCE(metadata->'jira_hold_prior_parked', to_jsonb(parked))) \
          WHERE id = $1 \
            AND status IN ('ready', 'decomposed') \
            AND started_at IS NULL \
            AND completed_at IS NULL \
            AND NULLIF(BTRIM(COALESCE(metadata->>'jira_execution_hold', '')), '') IS NULL",
    )
    .bind(parent_id)
    .bind(JIRA_BLOCKED_HOLD)
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        "WITH RECURSIVE descendants AS ( \
             SELECT id, parent_id FROM work_items WHERE parent_id = $1 \
             UNION ALL \
             SELECT child.id, child.parent_id \
               FROM work_items child \
               JOIN descendants parent ON child.parent_id = parent.id \
          ) \
          UPDATE work_items child \
             SET parked = true, \
                 metadata = child.metadata \
                    || jsonb_build_object( \
                        'jira_execution_hold', $2::text, \
                        'jira_held_at', NOW(), \
                        'jira_hold_prior_status', \
                            COALESCE(child.metadata->'jira_hold_prior_status', to_jsonb(child.status)), \
                        'jira_hold_prior_parked', \
                            COALESCE(child.metadata->'jira_hold_prior_parked', to_jsonb(child.parked))) \
            FROM descendants d \
           WHERE child.id = d.id \
             AND child.status IN ('idea', 'backlog', 'ready', 'decomposed') \
             AND child.started_at IS NULL \
             AND child.completed_at IS NULL \
             AND NULLIF(BTRIM(COALESCE(child.metadata->>'jira_execution_hold', '')), '') IS NULL",
    )
    .bind(parent_id)
    .bind(JIRA_BLOCKED_HOLD)
    .execute(&mut **tx)
    .await?;

    // Reconcile and heartbeat rows already owned by this Jira hold.  These
    // updates deliberately do not rebuild the prior-state keys: replaying a
    // blocked poll must never replace the first transition's restore point.
    // Reasserting the blocked/parked state also closes a dispatch window if an
    // eligible held row was incorrectly made ready between polls.  The exact
    // Jira hold remains authoritative until Jira reports a resumable status;
    // manual eligibility changes made while that key remains are drift.
    sqlx::query(
        "UPDATE work_items \
            SET status = 'blocked', \
                metadata = jsonb_set( \
                    metadata, '{jira_held_at}', to_jsonb(NOW()), true) \
          WHERE id = $1 \
            AND status IN ('blocked', 'ready', 'decomposed') \
            AND started_at IS NULL \
            AND completed_at IS NULL \
            AND metadata->>'jira_execution_hold' = $2",
    )
    .bind(parent_id)
    .bind(JIRA_BLOCKED_HOLD)
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        "WITH RECURSIVE descendants AS ( \
             SELECT id, parent_id FROM work_items WHERE parent_id = $1 \
             UNION ALL \
             SELECT child.id, child.parent_id \
               FROM work_items child \
               JOIN descendants parent ON child.parent_id = parent.id \
          ) \
          UPDATE work_items child \
             SET parked = true, \
                 metadata = jsonb_set( \
                     child.metadata, '{jira_held_at}', to_jsonb(NOW()), true) \
            FROM descendants d \
           WHERE child.id = d.id \
             AND child.status IN ('idea', 'backlog', 'ready', 'decomposed') \
             AND child.started_at IS NULL \
             AND child.completed_at IS NULL \
             AND child.metadata->>'jira_execution_hold' = $2",
    )
    .bind(parent_id)
    .bind(JIRA_BLOCKED_HOLD)
    .execute(&mut **tx)
    .await?;

    Ok(())
}

async fn restore_jira_parent_and_descendants(
    tx: &mut Transaction<'_, Postgres>,
    parent_id: Uuid,
) -> Result<()> {
    sqlx::query(
        "UPDATE work_items \
            SET status = metadata->>$2, \
                parked = CASE metadata->>$3 \
                    WHEN 'true' THEN true \
                    WHEN 'false' THEN false \
                    ELSE parked \
                END, \
                metadata = metadata - 'jira_execution_hold' - 'jira_held_at' - $2 - $3 \
          WHERE id = $1 \
            AND status = 'blocked' \
            AND metadata->>'jira_execution_hold' = $4 \
            AND metadata->>$2 IN ('ready', 'decomposed') \
            AND metadata->>$3 IN ('true', 'false') \
            AND started_at IS NULL \
            AND completed_at IS NULL",
    )
    .bind(parent_id)
    .bind(JIRA_HOLD_PRIOR_STATUS)
    .bind(JIRA_HOLD_PRIOR_PARKED)
    .bind(JIRA_BLOCKED_HOLD)
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        "WITH RECURSIVE descendants AS ( \
             SELECT id, parent_id FROM work_items WHERE parent_id = $1 \
             UNION ALL \
             SELECT child.id, child.parent_id \
               FROM work_items child \
               JOIN descendants parent ON child.parent_id = parent.id \
          ) \
          UPDATE work_items child \
             SET parked = CASE child.metadata->>$3 \
                    WHEN 'true' THEN true \
                    WHEN 'false' THEN false \
                    ELSE child.parked \
                 END, \
                 status = child.metadata->>$2, \
                 metadata = child.metadata - 'jira_execution_hold' - 'jira_held_at' - $2 - $3 \
            FROM descendants d \
           WHERE child.id = d.id \
             AND child.metadata->>'jira_execution_hold' = $4 \
             AND child.status IN ('idea', 'backlog', 'ready', 'decomposed') \
             AND child.metadata->>$2 IN ('idea', 'backlog', 'ready', 'decomposed') \
             AND child.metadata->>$3 IN ('true', 'false') \
             AND child.started_at IS NULL \
             AND child.completed_at IS NULL",
    )
    .bind(parent_id)
    .bind(JIRA_HOLD_PRIOR_STATUS)
    .bind(JIRA_HOLD_PRIOR_PARKED)
    .bind(JIRA_BLOCKED_HOLD)
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

fn normalize_jira_status(status: Option<&str>) -> Option<String> {
    status
        .map(str::trim)
        .filter(|status| !status.is_empty())
        .map(str::to_ascii_lowercase)
}

fn is_jira_blocked_status(status: Option<&str>) -> bool {
    matches!(
        normalize_jira_status(status).as_deref(),
        Some("blocked" | "blocked on vinny")
    )
}

fn is_jira_resumable_status(status: Option<&str>) -> bool {
    matches!(
        normalize_jira_status(status).as_deref(),
        Some("to do" | "in progress")
    )
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
    fn normalizes_jira_blocked_transition_matrix() {
        let cases = [
            (Some("Blocked"), true, false),
            (Some(" blocked "), true, false),
            (Some("Blocked on Vinny"), true, false),
            (Some("BLOCKED ON VINNY"), true, false),
            (Some("To Do"), false, true),
            (Some(" in progress "), false, true),
            (Some("In Review"), false, false),
            (Some("Done"), false, false),
            (None, false, false),
        ];

        for (status, blocked, resumable) in cases {
            assert_eq!(is_jira_blocked_status(status), blocked, "{status:?}");
            assert_eq!(is_jira_resumable_status(status), resumable, "{status:?}");
        }
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

    #[tokio::test]
    async fn blocked_polls_heartbeat_recursively_and_restore_exact_first_snapshot() {
        let Some(pg) = jira_ingest_test_pool().await else {
            return;
        };
        let mut tx = pg.begin().await.expect("begin test transaction");
        let project_id = format!("jira-ingest-test-{}", Uuid::new_v4());
        ensure_test_project(&mut tx, &project_id).await;

        let parent: Uuid = sqlx::query_scalar(
            "INSERT INTO work_items \
                (project_id, kind, title, status, priority, labels, created_by, parked, metadata, original_signal) \
             VALUES ($1, 'jira', 'parent', 'decomposed', 'normal', '[]'::jsonb, 'test', true, \
                     '{\"jira_status\":\"Blocked\",\"sentinel\":\"parent\"}'::jsonb, '{}'::jsonb) \
             RETURNING id",
        )
        .bind(&project_id)
        .fetch_one(&mut *tx)
        .await
        .expect("insert parent");
        let ready_child = insert_child(&mut tx, &project_id, parent, "ready", false, json!({}))
            .await
            .expect("insert ready child");
        let grandchild = insert_child(
            &mut tx,
            &project_id,
            ready_child,
            "backlog",
            true,
            json!({"sentinel": "grandchild"}),
        )
        .await
        .expect("insert grandchild");
        let unrelated_hold_child: Uuid = insert_child(
            &mut tx,
            &project_id,
            parent,
            "ready",
            false,
            json!({"jira_execution_hold": "awaiting_council", "sentinel": "unrelated"}),
        )
        .await
        .expect("insert unrelated hold child");
        let started_ready_child =
            insert_child(&mut tx, &project_id, parent, "ready", false, json!({}))
                .await
                .expect("insert started ready child");
        sqlx::query("UPDATE work_items SET started_at = NOW() WHERE id = $1")
            .bind(started_ready_child)
            .execute(&mut *tx)
            .await
            .expect("mark ready child started");
        let active_child = insert_child(&mut tx, &project_id, parent, "building", false, json!({}))
            .await
            .expect("insert active child");
        sqlx::query("UPDATE work_items SET started_at = NOW() WHERE id = $1")
            .bind(active_child)
            .execute(&mut *tx)
            .await
            .expect("mark active child started");
        let completed_ready_child =
            insert_child(&mut tx, &project_id, parent, "ready", false, json!({}))
                .await
                .expect("insert completed ready child");
        sqlx::query("UPDATE work_items SET completed_at = NOW() WHERE id = $1")
            .bind(completed_ready_child)
            .execute(&mut *tx)
            .await
            .expect("mark ready child completed");
        let failed_child = insert_child(&mut tx, &project_id, parent, "failed", false, json!({}))
            .await
            .expect("insert failed child");
        let malformed_held_child = insert_child(
            &mut tx,
            &project_id,
            parent,
            "ready",
            false,
            json!({
                "jira_execution_hold": JIRA_BLOCKED_HOLD,
                "jira_held_at": "2000-01-01T00:00:00Z",
                "jira_hold_prior_status": "not-a-status",
                "jira_hold_prior_parked": "not-a-boolean"
            }),
        )
        .await
        .expect("insert malformed held child");

        hold_jira_parent_and_descendants(&mut tx, parent)
            .await
            .expect("hold parent");

        assert_hold_snapshot(
            &mut tx,
            parent,
            "blocked",
            true,
            Some(JIRA_BLOCKED_HOLD),
            Some("decomposed"),
            Some("true"),
        )
        .await;
        assert_hold_snapshot(
            &mut tx,
            ready_child,
            "ready",
            true,
            Some(JIRA_BLOCKED_HOLD),
            Some("ready"),
            Some("false"),
        )
        .await;
        assert_hold_snapshot(
            &mut tx,
            grandchild,
            "backlog",
            true,
            Some(JIRA_BLOCKED_HOLD),
            Some("backlog"),
            Some("true"),
        )
        .await;
        assert_hold_snapshot(
            &mut tx,
            malformed_held_child,
            "ready",
            true,
            Some(JIRA_BLOCKED_HOLD),
            Some("not-a-status"),
            Some("not-a-boolean"),
        )
        .await;
        assert_child_state(
            &mut tx,
            unrelated_hold_child,
            "ready",
            false,
            Some("awaiting_council"),
        )
        .await;
        assert_child_state(&mut tx, started_ready_child, "ready", false, None).await;
        assert_child_state(&mut tx, active_child, "building", false, None).await;
        assert_child_state(&mut tx, completed_ready_child, "ready", false, None).await;
        assert_child_state(&mut tx, failed_child, "failed", false, None).await;

        sqlx::query(
            "UPDATE work_items \
                SET metadata = jsonb_set( \
                    metadata, '{jira_held_at}', to_jsonb(NOW() - INTERVAL '30 minutes'), true) \
              WHERE id IN ($1, $2, $3, $4)",
        )
        .bind(parent)
        .bind(ready_child)
        .bind(grandchild)
        .bind(malformed_held_child)
        .execute(&mut *tx)
        .await
        .expect("age Jira hold timestamps");
        sqlx::query("UPDATE work_items SET status = 'ready' WHERE id = $1")
            .bind(parent)
            .execute(&mut *tx)
            .await
            .expect("simulate parent eligibility drift");
        sqlx::query("UPDATE work_items SET parked = false WHERE id = $1")
            .bind(ready_child)
            .execute(&mut *tx)
            .await
            .expect("simulate child parking drift");
        let late_descendant = insert_child(
            &mut tx,
            &project_id,
            grandchild,
            "idea",
            false,
            json!({"sentinel": "late"}),
        )
        .await
        .expect("insert late descendant");

        hold_jira_parent_and_descendants(&mut tx, parent)
            .await
            .expect("replay blocked poll");

        let fresh_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM work_items \
              WHERE id IN ($1, $2, $3, $4, $5) \
                AND (metadata->>'jira_held_at')::timestamptz \
                    > NOW() - INTERVAL '15 minutes'",
        )
        .bind(parent)
        .bind(ready_child)
        .bind(grandchild)
        .bind(malformed_held_child)
        .bind(late_descendant)
        .fetch_one(&mut *tx)
        .await
        .expect("check refreshed Jira holds");
        assert_eq!(fresh_count, 5, "every Jira-owned eligible hold is fresh");
        assert_hold_snapshot(
            &mut tx,
            parent,
            "blocked",
            true,
            Some(JIRA_BLOCKED_HOLD),
            Some("decomposed"),
            Some("true"),
        )
        .await;
        assert_hold_snapshot(
            &mut tx,
            ready_child,
            "ready",
            true,
            Some(JIRA_BLOCKED_HOLD),
            Some("ready"),
            Some("false"),
        )
        .await;
        assert_hold_snapshot(
            &mut tx,
            late_descendant,
            "idea",
            true,
            Some(JIRA_BLOCKED_HOLD),
            Some("idea"),
            Some("false"),
        )
        .await;

        restore_jira_parent_and_descendants(&mut tx, parent)
            .await
            .expect("restore parent");

        assert_child_state(&mut tx, parent, "decomposed", true, None).await;
        assert_child_state(&mut tx, ready_child, "ready", false, None).await;
        assert_child_state(&mut tx, grandchild, "backlog", true, None).await;
        assert_child_state(&mut tx, late_descendant, "idea", false, None).await;
        assert_child_state(
            &mut tx,
            unrelated_hold_child,
            "ready",
            false,
            Some("awaiting_council"),
        )
        .await;
        assert_child_state(&mut tx, started_ready_child, "ready", false, None).await;
        assert_child_state(&mut tx, active_child, "building", false, None).await;
        assert_child_state(&mut tx, completed_ready_child, "ready", false, None).await;
        assert_child_state(&mut tx, failed_child, "failed", false, None).await;
        assert_hold_snapshot(
            &mut tx,
            malformed_held_child,
            "ready",
            true,
            Some(JIRA_BLOCKED_HOLD),
            Some("not-a-status"),
            Some("not-a-boolean"),
        )
        .await;
        let restored_metadata: Value =
            sqlx::query_scalar("SELECT metadata FROM work_items WHERE id = $1")
                .bind(parent)
                .fetch_one(&mut *tx)
                .await
                .expect("read restored parent metadata");
        assert_eq!(restored_metadata["sentinel"], json!("parent"));
        for key in [
            "jira_execution_hold",
            "jira_held_at",
            JIRA_HOLD_PRIOR_STATUS,
            JIRA_HOLD_PRIOR_PARKED,
        ] {
            assert!(restored_metadata.get(key).is_none(), "cleared {key}");
        }

        tx.rollback().await.expect("rollback test transaction");
    }

    #[tokio::test]
    async fn blocked_upsert_is_non_dispatchable_replay_fresh_and_resumable() {
        let Some(pg) = jira_ingest_test_pool().await else {
            return;
        };
        let mut tx = pg.begin().await.expect("begin test transaction");
        let config = JiraConfig {
            name: format!("test-config-{}", Uuid::new_v4()),
            project_key: "HFPROD".to_owned(),
            jira_secret_ref: "unused".to_owned(),
            queue_jql: "unused".to_owned(),
        };
        let ruleset_id = format!("test-ruleset-{}", Uuid::new_v4());
        ensure_test_project(&mut tx, PROJECT_ID).await;
        sqlx::query(
            "INSERT INTO jira_rulesets (id, name, version, content_hash) \
             VALUES ($1, 'test ruleset', 1, 'test-content-hash')",
        )
        .bind(&ruleset_id)
        .execute(&mut *tx)
        .await
        .expect("insert test Jira ruleset");
        sqlx::query(
            "INSERT INTO jira_configs \
                (name, project_key, owner_account_id, jira_secret_ref, queue_jql, ruleset_id) \
             VALUES ($1, $2, 'test-owner', $3, $4, $5)",
        )
        .bind(&config.name)
        .bind(&config.project_key)
        .bind(&config.jira_secret_ref)
        .bind(&config.queue_jql)
        .bind(&ruleset_id)
        .execute(&mut *tx)
        .await
        .expect("insert test Jira config");
        let blocked_issue_id = Uuid::new_v4().to_string();
        let ready_issue_id = Uuid::new_v4().to_string();

        upsert_issue(
            &mut tx,
            "https://jira.example.test",
            &config,
            &jira_issue(&blocked_issue_id, "HFPROD-1", "Blocked on Vinny"),
        )
        .await
        .expect("upsert blocked issue");
        upsert_issue(
            &mut tx,
            "https://jira.example.test",
            &config,
            &jira_issue(&ready_issue_id, "HFPROD-2", "To Do"),
        )
        .await
        .expect("upsert ready issue");

        let (blocked_status, blocked_parked, blocked_hold, blocked_prior, blocked_prior_parked): (
            String,
            bool,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT status, parked, metadata->>'jira_execution_hold', metadata->>$2, metadata->>$3 \
             FROM work_items WHERE kind = 'jira' AND metadata->>'jira_issue_id' = $1",
        )
        .bind(&blocked_issue_id)
        .bind(JIRA_HOLD_PRIOR_STATUS)
        .bind(JIRA_HOLD_PRIOR_PARKED)
        .fetch_one(&mut *tx)
        .await
        .expect("read blocked insert");
        assert_eq!(blocked_status, "blocked");
        assert!(!blocked_parked);
        assert_eq!(blocked_hold.as_deref(), Some(JIRA_BLOCKED_HOLD));
        assert_eq!(blocked_prior.as_deref(), Some("ready"));
        assert_eq!(blocked_prior_parked.as_deref(), Some("false"));

        sqlx::query(
            "UPDATE work_items \
                SET metadata = jsonb_set( \
                    metadata, '{jira_held_at}', to_jsonb(NOW() - INTERVAL '30 minutes'), true) \
              WHERE kind = 'jira' AND metadata->>'jira_issue_id' = $1",
        )
        .bind(&blocked_issue_id)
        .execute(&mut *tx)
        .await
        .expect("age blocked issue");
        upsert_issue(
            &mut tx,
            "https://jira.example.test",
            &config,
            &jira_issue(&blocked_issue_id, "HFPROD-1", "Blocked"),
        )
        .await
        .expect("replay blocked issue");
        let (fresh, prior, prior_parked): (bool, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT (metadata->>'jira_held_at')::timestamptz > NOW() - INTERVAL '15 minutes', \
                        metadata->>$2, metadata->>$3 \
                   FROM work_items \
                  WHERE kind = 'jira' AND metadata->>'jira_issue_id' = $1",
        )
        .bind(&blocked_issue_id)
        .bind(JIRA_HOLD_PRIOR_STATUS)
        .bind(JIRA_HOLD_PRIOR_PARKED)
        .fetch_one(&mut *tx)
        .await
        .expect("read replayed blocked issue");
        assert!(fresh);
        assert_eq!(prior.as_deref(), Some("ready"));
        assert_eq!(prior_parked.as_deref(), Some("false"));

        upsert_issue(
            &mut tx,
            "https://jira.example.test",
            &config,
            &jira_issue(&blocked_issue_id, "HFPROD-1", "In Progress"),
        )
        .await
        .expect("resume blocked issue");
        let (resumed_status, resumed_parked, resumed_hold): (String, bool, Option<String>) =
            sqlx::query_as(
                "SELECT status, parked, metadata->>'jira_execution_hold' \
                   FROM work_items \
                  WHERE kind = 'jira' AND metadata->>'jira_issue_id' = $1",
            )
            .bind(&blocked_issue_id)
            .fetch_one(&mut *tx)
            .await
            .expect("read resumed issue");
        assert_eq!(resumed_status, "ready");
        assert!(!resumed_parked);
        assert_eq!(resumed_hold, None);

        let (ready_status, ready_hold): (String, Option<String>) = sqlx::query_as(
            "SELECT status, metadata->>'jira_execution_hold' \
             FROM work_items WHERE kind = 'jira' AND metadata->>'jira_issue_id' = $1",
        )
        .bind(&ready_issue_id)
        .fetch_one(&mut *tx)
        .await
        .expect("read ready insert");
        assert_eq!(ready_status, "ready");
        assert_eq!(ready_hold, None);

        tx.rollback().await.expect("rollback test transaction");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_blocked_replays_preserve_one_restore_snapshot() {
        let Some(pg) = jira_ingest_test_pool().await else {
            return;
        };
        let project_id = format!("jira-ingest-concurrency-test-{}", Uuid::new_v4());
        let mut setup = pg.begin().await.expect("begin setup transaction");
        ensure_test_project(&mut setup, &project_id).await;
        let parent: Uuid = sqlx::query_scalar(
            "INSERT INTO work_items \
                (project_id, kind, title, status, priority, labels, created_by, parked, metadata, original_signal) \
             VALUES ($1, 'jira', 'parent', 'decomposed', 'normal', '[]'::jsonb, 'test', true, \
                     '{\"jira_status\":\"Blocked\"}'::jsonb, '{}'::jsonb) \
             RETURNING id",
        )
        .bind(&project_id)
        .fetch_one(&mut *setup)
        .await
        .expect("insert concurrent parent");
        let child = insert_child(&mut setup, &project_id, parent, "ready", false, json!({}))
            .await
            .expect("insert concurrent child");
        setup.commit().await.expect("commit concurrent setup");

        let mut first = pg.begin().await.expect("begin first replay");
        sqlx::query("SELECT id FROM work_items WHERE id = $1 FOR UPDATE")
            .bind(parent)
            .execute(&mut *first)
            .await
            .expect("lock parent for first replay");
        let second_pool = pg.clone();
        let second = tokio::spawn(async move {
            let mut tx = second_pool.begin().await?;
            hold_jira_parent_and_descendants(&mut tx, parent).await?;
            tx.commit().await?;
            Ok::<(), anyhow::Error>(())
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        hold_jira_parent_and_descendants(&mut first, parent)
            .await
            .expect("first blocked replay");
        first.commit().await.expect("commit first replay");
        second
            .await
            .expect("join second replay")
            .expect("second blocked replay");

        let parent_state: (String, bool, Option<String>, Option<String>, Option<String>) =
            sqlx::query_as(
                "SELECT status, parked, metadata->>'jira_execution_hold', \
                        metadata->>'jira_hold_prior_status', metadata->>'jira_hold_prior_parked' \
                   FROM work_items WHERE id = $1",
            )
            .bind(parent)
            .fetch_one(&pg)
            .await
            .expect("read concurrent parent");
        assert_eq!(
            parent_state,
            (
                "blocked".to_owned(),
                true,
                Some(JIRA_BLOCKED_HOLD.to_owned()),
                Some("decomposed".to_owned()),
                Some("true".to_owned()),
            )
        );
        let child_state: (String, bool, Option<String>, Option<String>, Option<String>) =
            sqlx::query_as(
                "SELECT status, parked, metadata->>'jira_execution_hold', \
                        metadata->>'jira_hold_prior_status', metadata->>'jira_hold_prior_parked' \
                   FROM work_items WHERE id = $1",
            )
            .bind(child)
            .fetch_one(&pg)
            .await
            .expect("read concurrent child");
        assert_eq!(
            child_state,
            (
                "ready".to_owned(),
                true,
                Some(JIRA_BLOCKED_HOLD.to_owned()),
                Some("ready".to_owned()),
                Some("false".to_owned()),
            )
        );

        sqlx::query("DELETE FROM work_items WHERE project_id = $1")
            .bind(&project_id)
            .execute(&pg)
            .await
            .expect("clean concurrent fixtures");
        sqlx::query("DELETE FROM projects WHERE id = $1")
            .bind(&project_id)
            .execute(&pg)
            .await
            .expect("clean concurrent project");
    }

    async fn jira_ingest_test_pool() -> Option<PgPool> {
        let Ok(database_url) = std::env::var("FORGEFLEET_JIRA_INGEST_TEST_DATABASE_URL") else {
            eprintln!("FORGEFLEET_JIRA_INGEST_TEST_DATABASE_URL is unset; skipping DB test");
            return None;
        };
        let pg = PgPool::connect(&database_url)
            .await
            .expect("connect Jira ingest test database");
        ff_db::run_postgres_migrations(&pg)
            .await
            .expect("migrate Jira ingest test database");
        Some(pg)
    }

    async fn ensure_test_project(tx: &mut Transaction<'_, Postgres>, project_id: &str) {
        sqlx::query(
            "INSERT INTO projects (id, display_name) VALUES ($1, $1) \
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(project_id)
        .execute(&mut **tx)
        .await
        .expect("insert test project");
    }

    async fn insert_child(
        tx: &mut Transaction<'_, Postgres>,
        project_id: &str,
        parent_id: Uuid,
        status: &str,
        parked: bool,
        metadata: Value,
    ) -> Result<Uuid, sqlx::Error> {
        sqlx::query_scalar(
            "INSERT INTO work_items \
                (project_id, parent_id, kind, title, status, priority, labels, created_by, parked, metadata, original_signal) \
             VALUES ($1, $2, 'task', 'child', $3, 'normal', '[]'::jsonb, 'test', $4, $5, '{}'::jsonb) \
             RETURNING id",
        )
        .bind(project_id)
        .bind(parent_id)
        .bind(status)
        .bind(parked)
        .bind(metadata)
        .fetch_one(&mut **tx)
        .await
    }

    async fn assert_child_state(
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
        expected_status: &str,
        expected_parked: bool,
        expected_hold: Option<&str>,
    ) {
        let (status, parked, hold): (String, bool, Option<String>) = sqlx::query_as(
            "SELECT status, parked, metadata->>'jira_execution_hold' FROM work_items WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&mut **tx)
        .await
        .expect("read child");
        assert_eq!(status, expected_status);
        assert_eq!(parked, expected_parked);
        assert_eq!(hold.as_deref(), expected_hold);
    }

    async fn assert_hold_snapshot(
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
        expected_status: &str,
        expected_parked: bool,
        expected_hold: Option<&str>,
        expected_prior_status: Option<&str>,
        expected_prior_parked: Option<&str>,
    ) {
        let (status, parked, hold, prior_status, prior_parked): (
            String,
            bool,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT status, parked, metadata->>'jira_execution_hold', \
                    metadata->>'jira_hold_prior_status', metadata->>'jira_hold_prior_parked' \
               FROM work_items WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&mut **tx)
        .await
        .expect("read Jira hold snapshot");
        assert_eq!(status, expected_status);
        assert_eq!(parked, expected_parked);
        assert_eq!(hold.as_deref(), expected_hold);
        assert_eq!(prior_status.as_deref(), expected_prior_status);
        assert_eq!(prior_parked.as_deref(), expected_prior_parked);
    }

    fn jira_issue(id: &str, key: &str, status: &str) -> JiraIssue {
        JiraIssue {
            id: id.to_owned(),
            key: key.to_owned(),
            fields: JiraFields {
                summary: "summary".to_owned(),
                status: Some(NamedField {
                    name: status.to_owned(),
                }),
                ..JiraFields::default()
            },
        }
    }
}
