//! Leader-gated Jira queue ingestion.
//!
//! The daemon registry supplies the leader gate. This tick fetches the
//! `hireflow360` queue, then atomically refreshes Jira-owned work-item fields
//! and the existing `jira_watch_state` rows without disturbing scheduler state.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

const CONFIG_NAME: &str = "hireflow360";
const PROJECT_ID: &str = "hireflow360";
const REPO_HOLD_REASON: &str = "jira_repo_resolution";
const REPO_UNRESOLVED_HOLD: &str = "repo_unresolved";
const REPO_MULTI_HOLD: &str = "repo_multi";

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
struct ProjectRepo {
    id: Uuid,
    github_url: String,
    name: Option<String>,
    default_branch: String,
    local_path: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum RepoResolution<'a> {
    Unique(&'a ProjectRepo),
    NoExactMatch,
    MultipleExactMatches(Vec<&'a ProjectRepo>),
}

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

#[derive(sqlx::FromRow)]
struct ExistingJiraParent {
    id: Uuid,
    status: String,
    metadata: Value,
    has_started: bool,
    has_completed: bool,
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

    let repos: Vec<ProjectRepo> = sqlx::query_as(
        "SELECT id, github_url, name, default_branch, local_path \
           FROM project_repos WHERE project_id = $1",
    )
    .bind(PROJECT_ID)
    .fetch_all(&mut *tx)
    .await?;

    for (queue_rank, issue) in issues.values() {
        upsert_issue(&mut tx, &base_url, &config, issue, *queue_rank, &repos).await?;
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
) -> Result<BTreeMap<String, (u64, JiraIssue)>> {
    let mut issues = BTreeMap::new();
    let mut next_page_token: Option<String> = None;
    let mut next_queue_rank = 1_u64;

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
            if !issues.contains_key(&issue.id) {
                issues.insert(issue.id.clone(), (next_queue_rank, issue));
                next_queue_rank += 1;
            }
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
    queue_rank: u64,
    repos: &[ProjectRepo],
) -> Result<()> {
    let repo_resolution = resolve_repo(&issue.fields.labels, &repos);

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
        "jira_status": status,
        "jira_queue_rank": queue_rank
    });
    let original_signal = json!({
        "kind": "jira",
        "signature": format!("jira:{}:{}", config.name, issue.id),
        "config_id": config.name,
        "issue_id": issue.id,
        "issue_key": issue.key,
        "status": status,
        "queue_rank": queue_rank
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

    let existing: Option<ExistingJiraParent> = sqlx::query_as(
        "SELECT id, status, metadata, started_at IS NOT NULL AS has_started, \
                completed_at IS NOT NULL AS has_completed \
           FROM work_items \
          WHERE project_id = $1 AND kind = 'jira' \
            AND metadata->>'jira_issue_id' = $2 \
          ORDER BY created_at LIMIT 1 FOR UPDATE",
    )
    .bind(PROJECT_ID)
    .bind(&issue.id)
    .fetch_optional(&mut **tx)
    .await?;

    if let Some(existing) = existing {
        let id = existing.id;
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

        match repo_resolution {
            RepoResolution::Unique(repo) if repo_is_routable(repo) => {
                bind_existing_repo(tx, &existing, repo).await?;
            }
            RepoResolution::Unique(repo) => {
                hold_existing_for_repo_resolution(
                    tx,
                    &existing,
                    REPO_UNRESOLVED_HOLD,
                    "incomplete_repo_binding",
                    repo_candidates(&[repo]),
                    false,
                )
                .await?;
            }
            RepoResolution::NoExactMatch => {
                hold_existing_for_repo_resolution(
                    tx,
                    &existing,
                    REPO_UNRESOLVED_HOLD,
                    "no_exact_match",
                    Vec::new(),
                    false,
                )
                .await?;
            }
            RepoResolution::MultipleExactMatches(matches) => {
                hold_existing_for_repo_resolution(
                    tx,
                    &existing,
                    REPO_MULTI_HOLD,
                    "multiple_exact_matches",
                    repo_candidates(&matches),
                    true,
                )
                .await?;
            }
        }
    } else {
        match repo_resolution {
            RepoResolution::Unique(repo) if repo_is_routable(repo) => {
                let repo_path =
                    routable_repo_path(repo).context("resolved Jira repo has no local path")?;
                sqlx::query(
                    "INSERT INTO work_items \
                        (project_id, kind, title, description, labels, status, priority, \
                         created_by, metadata, original_signal, repo_id, repo_url, \
                         repo_path, base_branch) \
                     VALUES ($1, 'jira', $2, $3, $4, 'ready', $5, \
                             'jira_ingest_tick', $6 || $7, $8, $9, $10, $11, $12)",
                )
                .bind(PROJECT_ID)
                .bind(title)
                .bind(description)
                .bind(labels)
                .bind(priority)
                .bind(metadata.clone())
                .bind(repo_bound_metadata(repo))
                .bind(original_signal)
                .bind(repo.id)
                .bind(&repo.github_url)
                .bind(repo_path)
                .bind(&repo.default_branch)
                .execute(&mut **tx)
                .await?;
            }
            RepoResolution::Unique(repo) => {
                let held_metadata = merge_metadata(
                    &metadata,
                    repo_hold_metadata(
                        REPO_UNRESOLVED_HOLD,
                        "incomplete_repo_binding",
                        repo_candidates(&[repo]),
                        false,
                        "ready",
                    ),
                );
                insert_held_issue(
                    tx,
                    &title,
                    description.as_deref(),
                    &labels,
                    priority,
                    &held_metadata,
                    &original_signal,
                )
                .await?;
            }
            RepoResolution::NoExactMatch => {
                let held_metadata = merge_metadata(
                    &metadata,
                    repo_hold_metadata(
                        REPO_UNRESOLVED_HOLD,
                        "no_exact_match",
                        Vec::new(),
                        false,
                        "ready",
                    ),
                );
                insert_held_issue(
                    tx,
                    &title,
                    description.as_deref(),
                    &labels,
                    priority,
                    &held_metadata,
                    &original_signal,
                )
                .await?;
            }
            RepoResolution::MultipleExactMatches(matches) => {
                let held_metadata = merge_metadata(
                    &metadata,
                    repo_hold_metadata(
                        REPO_MULTI_HOLD,
                        "multiple_exact_matches",
                        repo_candidates(&matches),
                        true,
                        "ready",
                    ),
                );
                insert_held_issue(
                    tx,
                    &title,
                    description.as_deref(),
                    &labels,
                    priority,
                    &held_metadata,
                    &original_signal,
                )
                .await?;
            }
        }
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

fn resolve_repo<'a>(labels: &[String], repos: &'a [ProjectRepo]) -> RepoResolution<'a> {
    let labels: BTreeSet<&str> = labels.iter().map(String::as_str).collect();
    let mut matches: Vec<&ProjectRepo> = repos
        .iter()
        .filter(|repo| canonical_repo_name(repo).is_some_and(|name| labels.contains(name)))
        .collect();
    matches.sort_by(|left, right| {
        canonical_repo_name(left)
            .cmp(&canonical_repo_name(right))
            .then_with(|| left.id.cmp(&right.id))
    });

    match matches.len() {
        0 => RepoResolution::NoExactMatch,
        1 => RepoResolution::Unique(matches[0]),
        _ => RepoResolution::MultipleExactMatches(matches),
    }
}

fn repo_candidates(repos: &[&ProjectRepo]) -> Vec<Value> {
    repos
        .iter()
        .map(|repo| {
            json!({
                "id": repo.id,
                "name": canonical_repo_name(repo),
                "repo_url": repo.github_url,
                "repo_path": routable_repo_path(repo),
                "base_branch": repo.default_branch
            })
        })
        .collect()
}

fn canonical_repo_name(repo: &ProjectRepo) -> Option<&str> {
    repo.name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
}

fn routable_repo_path(repo: &ProjectRepo) -> Option<&str> {
    repo.local_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
}

fn repo_is_routable(repo: &ProjectRepo) -> bool {
    canonical_repo_name(repo).is_some()
        && routable_repo_path(repo).is_some()
        && !repo.github_url.trim().is_empty()
        && !repo.default_branch.trim().is_empty()
}

fn repo_bound_metadata(repo: &ProjectRepo) -> Value {
    json!({
        "jira_repo_id": repo.id,
        "jira_repo_name": canonical_repo_name(repo),
        "jira_repo_resolution_state": "bound"
    })
}

fn repo_hold_metadata(
    execution_hold: &str,
    detail: &str,
    allowed_repos: Vec<Value>,
    awaiting_partition: bool,
    previous_status: &str,
) -> Value {
    json!({
        "jira_execution_hold": execution_hold,
        "jira_repo_resolution": {
            "reason": REPO_HOLD_REASON,
            "detail": detail,
            "allowed_repos": allowed_repos,
            "awaiting_per_repo_partition": awaiting_partition,
            "previous_status": previous_status
        }
    })
}

fn merge_metadata(base: &Value, extra: Value) -> Value {
    let mut merged = base.clone();
    if let (Some(merged), Some(extra)) = (merged.as_object_mut(), extra.as_object()) {
        merged.extend(extra.clone());
    }
    merged
}

fn is_repo_execution_hold(metadata: &Value) -> bool {
    matches!(
        metadata.get("jira_execution_hold").and_then(Value::as_str),
        Some(REPO_UNRESOLVED_HOLD) | Some(REPO_MULTI_HOLD)
    )
}

fn repo_hold_previous_status(existing: &ExistingJiraParent) -> String {
    if existing
        .metadata
        .pointer("/jira_repo_resolution/reason")
        .and_then(Value::as_str)
        == Some(REPO_HOLD_REASON)
        && let Some(previous) = existing
            .metadata
            .pointer("/jira_repo_resolution/previous_status")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|status| !status.is_empty())
    {
        return previous.to_owned();
    }
    if is_repo_execution_hold(&existing.metadata) {
        return "ready".to_owned();
    }
    existing.status.clone()
}

async fn bind_existing_repo(
    tx: &mut Transaction<'_, Postgres>,
    existing: &ExistingJiraParent,
    repo: &ProjectRepo,
) -> Result<()> {
    let repo_path = routable_repo_path(repo).context("resolved Jira repo has no local path")?;
    sqlx::query(
        "UPDATE work_items \
            SET status = CASE \
                  WHEN status IN ('blocked', 'idea') \
                   AND (metadata->>'jira_execution_hold' IN ($7, $8) \
                        OR (NULLIF(BTRIM(COALESCE(metadata->>'jira_execution_hold', '')), '') \
                              IS NULL \
                            AND metadata->'jira_repo_resolution'->>'reason' = $9)) \
                  THEN CASE COALESCE(NULLIF(BTRIM(metadata->'jira_repo_resolution' \
                                                      ->>'previous_status'), ''), 'ready') \
                         WHEN 'ready' THEN 'ready' \
                         WHEN 'idea' THEN 'idea' \
                         ELSE status END \
                  ELSE status END, \
                repo_id = $2, repo_url = $3, repo_path = $4, base_branch = $5, \
                metadata = ((CASE \
                    WHEN metadata->>'jira_execution_hold' IN ($7, $8) \
                    THEN metadata - 'jira_execution_hold' ELSE metadata END) \
                    - 'jira_repo_resolution') || $6 \
          WHERE id = $1 AND started_at IS NULL AND completed_at IS NULL \
            AND status IN ('ready', 'blocked', 'idea')",
    )
    .bind(existing.id)
    .bind(repo.id)
    .bind(&repo.github_url)
    .bind(repo_path)
    .bind(&repo.default_branch)
    .bind(repo_bound_metadata(repo))
    .bind(REPO_UNRESOLVED_HOLD)
    .bind(REPO_MULTI_HOLD)
    .bind(REPO_HOLD_REASON)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn hold_existing_for_repo_resolution(
    tx: &mut Transaction<'_, Postgres>,
    existing: &ExistingJiraParent,
    execution_hold: &str,
    detail: &str,
    allowed_repos: Vec<Value>,
    awaiting_partition: bool,
) -> Result<()> {
    if existing.has_started || existing.has_completed {
        return Ok(());
    }
    let hold_metadata = repo_hold_metadata(
        execution_hold,
        detail,
        allowed_repos,
        awaiting_partition,
        &repo_hold_previous_status(existing),
    );
    sqlx::query(
        "UPDATE work_items \
            SET status = CASE \
                  WHEN status = 'ready' \
                   AND (NULLIF(BTRIM(COALESCE(metadata->>'jira_execution_hold', '')), '') \
                          IS NULL \
                        OR metadata->>'jira_execution_hold' IN ($3, $4)) \
                  THEN 'blocked' ELSE status END, \
                repo_id = NULL, repo_url = NULL, \
                repo_path = NULL, base_branch = NULL, \
                metadata = CASE \
                  WHEN NULLIF(BTRIM(COALESCE(metadata->>'jira_execution_hold', '')), '') \
                         IS NULL \
                    OR metadata->>'jira_execution_hold' IN ($3, $4) \
                  THEN (metadata - 'jira_repo_id' - 'jira_repo_name' \
                                 - 'jira_repo_resolution_state') || $2 \
                  ELSE (metadata - 'jira_repo_id' - 'jira_repo_name' \
                                 - 'jira_repo_resolution_state') \
                       || jsonb_build_object( \
                            'jira_repo_resolution', $2->'jira_repo_resolution') END \
          WHERE id = $1 AND started_at IS NULL AND completed_at IS NULL \
            AND status IN ('ready', 'blocked', 'idea')",
    )
    .bind(existing.id)
    .bind(hold_metadata)
    .bind(REPO_UNRESOLVED_HOLD)
    .bind(REPO_MULTI_HOLD)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn insert_held_issue(
    tx: &mut Transaction<'_, Postgres>,
    title: &str,
    description: Option<&str>,
    labels: &Value,
    priority: &str,
    metadata: &Value,
    original_signal: &Value,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO work_items \
            (project_id, kind, title, description, labels, status, priority, \
             created_by, metadata, original_signal) \
         VALUES ($1, 'jira', $2, $3, $4, 'blocked', $5, \
                 'jira_ingest_tick', $6, $7)",
    )
    .bind(PROJECT_ID)
    .bind(title)
    .bind(description)
    .bind(labels)
    .bind(priority)
    .bind(metadata)
    .bind(original_signal)
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

    fn repo(name: &str, id: u128) -> ProjectRepo {
        ProjectRepo {
            id: Uuid::from_u128(id),
            github_url: format!("https://github.com/example/{name}.git"),
            name: Some(name.to_owned()),
            default_branch: "main".to_owned(),
            local_path: Some(format!("/srv/{name}")),
        }
    }

    fn existing(status: &str, metadata: Value) -> ExistingJiraParent {
        ExistingJiraParent {
            id: Uuid::nil(),
            status: status.to_owned(),
            metadata,
            has_started: false,
            has_completed: false,
        }
    }

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

    #[test]
    fn exact_repo_label_resolves_unique_repo() {
        let repos = vec![repo("api", 1), repo("web", 2)];
        let labels = vec!["web".to_owned()];

        assert_eq!(
            resolve_repo(&labels, &repos),
            RepoResolution::Unique(&repos[1])
        );
    }

    #[test]
    fn incidental_label_does_not_invalidate_unique_repo() {
        let repos = vec![repo("api", 1), repo("web", 2)];
        let labels = vec!["urgent".to_owned(), "api".to_owned(), "backend".to_owned()];

        assert_eq!(
            resolve_repo(&labels, &repos),
            RepoResolution::Unique(&repos[0])
        );
    }

    #[test]
    fn multiple_exact_matches_have_deterministic_allowed_repo_set() {
        let repos = vec![repo("web", 2), repo("api", 1), repo("worker", 3)];
        let labels = vec!["web".to_owned(), "api".to_owned()];

        let RepoResolution::MultipleExactMatches(matches) = resolve_repo(&labels, &repos) else {
            panic!("expected multiple exact matches");
        };
        let candidates = repo_candidates(&matches);
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["api", "web"]
        );
        assert_eq!(candidates[0]["id"], json!(repos[1].id));
        assert_eq!(candidates[1]["id"], json!(repos[0].id));
        assert_eq!(
            repo_hold_metadata(
                REPO_MULTI_HOLD,
                "multiple_exact_matches",
                candidates,
                true,
                "ready"
            )
            .pointer("/jira_repo_resolution/awaiting_per_repo_partition"),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn duplicate_canonical_names_remain_distinguishable_by_stable_id() {
        let repos = vec![repo("api", 2), repo("api", 1)];
        let RepoResolution::MultipleExactMatches(matches) =
            resolve_repo(&["api".to_owned()], &repos)
        else {
            panic!("duplicate canonical rows must remain ambiguous");
        };
        let candidates = repo_candidates(&matches);
        assert_eq!(candidates[0]["id"], json!(repos[1].id));
        assert_eq!(candidates[1]["id"], json!(repos[0].id));
        assert_eq!(candidates[0]["name"], "api");
        assert_eq!(candidates[1]["name"], "api");
    }

    #[test]
    fn zero_exact_matches_fail_closed_without_primary_fallback() {
        let repos = vec![repo("primary", 1)];
        let labels = vec!["urgent".to_owned()];

        assert_eq!(resolve_repo(&labels, &repos), RepoResolution::NoExactMatch);
    }

    #[test]
    fn near_and_case_variant_labels_do_not_match() {
        let repos = vec![repo("hireflow360", 1)];

        for label in ["HireFlow360", "hireflow-360", "hireflow360-api"] {
            assert_eq!(
                resolve_repo(&[label.to_owned()], &repos),
                RepoResolution::NoExactMatch
            );
        }
    }

    #[test]
    fn incomplete_repo_binding_is_detected_after_exact_match() {
        let mut incomplete = repo("app", 1);
        incomplete.local_path = None;
        assert_eq!(
            resolve_repo(&["app".to_owned()], std::slice::from_ref(&incomplete)),
            RepoResolution::Unique(&incomplete)
        );
        assert!(!repo_is_routable(&incomplete));
    }

    #[test]
    fn repo_hold_metadata_is_fail_closed_and_reason_scoped() {
        let hold = repo_hold_metadata(
            REPO_UNRESOLVED_HOLD,
            "no_exact_match",
            Vec::new(),
            false,
            "ready",
        );
        assert_eq!(
            hold.get("jira_execution_hold"),
            Some(&Value::String(REPO_UNRESOLVED_HOLD.to_owned()))
        );
        assert_eq!(
            hold.pointer("/jira_repo_resolution/reason"),
            Some(&Value::String(REPO_HOLD_REASON.to_owned()))
        );
        assert_eq!(
            hold.pointer("/jira_repo_resolution/previous_status"),
            Some(&Value::String("ready".to_owned()))
        );
    }

    #[test]
    fn restoration_state_preserves_unrelated_holds() {
        let manual_repo_hold = existing(
            "blocked",
            json!({"jira_execution_hold": REPO_UNRESOLVED_HOLD}),
        );
        assert!(is_repo_execution_hold(&manual_repo_hold.metadata));
        assert_eq!(repo_hold_previous_status(&manual_repo_hold), "ready");

        let unrelated = existing(
            "blocked",
            json!({"jira_execution_hold": "awaiting_council"}),
        );
        assert!(!is_repo_execution_hold(&unrelated.metadata));
        assert_eq!(repo_hold_previous_status(&unrelated), "blocked");

        let reason_scoped = existing(
            "blocked",
            repo_hold_metadata(
                REPO_MULTI_HOLD,
                "multiple_exact_matches",
                repo_candidates(&[&repo("api", 1), &repo("web", 2)]),
                true,
                "ready",
            ),
        );
        assert_eq!(repo_hold_previous_status(&reason_scoped), "ready");
    }

    #[tokio::test]
    async fn repo_hold_and_bind_sql_are_fail_closed_and_reason_scoped() {
        let Some(database_url) = std::env::var("FORGEFLEET_POSTGRES_URL")
            .ok()
            .or_else(|| std::env::var("FORGEFLEET_DATABASE_URL").ok())
        else {
            eprintln!("skipping repo hold/bind SQL test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL");
            return;
        };
        let pg = PgPool::connect(&database_url)
            .await
            .expect("connect Jira repo resolution test database");
        let mut tx = pg.begin().await.expect("begin Jira repo resolution test");
        sqlx::query(
            "CREATE TEMP TABLE work_items ( \
                 id UUID PRIMARY KEY, status TEXT NOT NULL, \
                 metadata JSONB NOT NULL DEFAULT '{}', \
                 repo_id UUID, repo_url TEXT, repo_path TEXT, base_branch TEXT, \
                 started_at TIMESTAMPTZ, completed_at TIMESTAMPTZ \
             ) ON COMMIT DROP",
        )
        .execute(&mut *tx)
        .await
        .expect("create temporary work_items table");

        let app_repo = repo("app", 1);
        let repo_hold_id = Uuid::new_v4();
        sqlx::query("INSERT INTO work_items (id, status, metadata) VALUES ($1, 'ready', '{}')")
            .bind(repo_hold_id)
            .execute(&mut *tx)
            .await
            .expect("insert ready repo-hold candidate");
        let ready = ExistingJiraParent {
            id: repo_hold_id,
            ..existing("ready", json!({}))
        };
        hold_existing_for_repo_resolution(
            &mut tx,
            &ready,
            REPO_MULTI_HOLD,
            "multiple_exact_matches",
            repo_candidates(&[&repo("api", 2), &app_repo]),
            true,
        )
        .await
        .expect("hold ambiguous Jira parent");
        let (status, metadata, repo_id): (String, Value, Option<Uuid>) =
            sqlx::query_as("SELECT status, metadata, repo_id FROM work_items WHERE id = $1")
                .bind(repo_hold_id)
                .fetch_one(&mut *tx)
                .await
                .expect("read held Jira parent");
        assert_eq!(status, "blocked");
        assert_eq!(
            metadata.get("jira_execution_hold"),
            Some(&Value::String(REPO_MULTI_HOLD.to_owned()))
        );
        assert!(repo_id.is_none());

        let held = ExistingJiraParent {
            id: repo_hold_id,
            status,
            metadata,
            has_started: false,
            has_completed: false,
        };
        bind_existing_repo(&mut tx, &held, &app_repo)
            .await
            .expect("restore reason-scoped repo hold");
        let (status, metadata, repo_id, repo_path): (String, Value, Option<Uuid>, Option<String>) =
            sqlx::query_as(
                "SELECT status, metadata, repo_id, repo_path FROM work_items WHERE id = $1",
            )
            .bind(repo_hold_id)
            .fetch_one(&mut *tx)
            .await
            .expect("read restored Jira parent");
        assert_eq!(status, "ready");
        assert_eq!(repo_id, Some(app_repo.id));
        assert_eq!(repo_path.as_deref(), Some("/srv/app"));
        assert!(metadata.get("jira_execution_hold").is_none());
        assert!(metadata.get("jira_repo_resolution").is_none());

        let idea_id = Uuid::new_v4();
        sqlx::query("INSERT INTO work_items (id, status, metadata) VALUES ($1, 'idea', '{}')")
            .bind(idea_id)
            .execute(&mut *tx)
            .await
            .expect("insert idea repo-hold candidate");
        let idea = ExistingJiraParent {
            id: idea_id,
            ..existing("idea", json!({}))
        };
        hold_existing_for_repo_resolution(
            &mut tx,
            &idea,
            REPO_UNRESOLVED_HOLD,
            "no_exact_match",
            Vec::new(),
            false,
        )
        .await
        .expect("hold idea Jira parent without promoting it");
        let (status, metadata): (String, Value) =
            sqlx::query_as("SELECT status, metadata FROM work_items WHERE id = $1")
                .bind(idea_id)
                .fetch_one(&mut *tx)
                .await
                .expect("read held idea Jira parent");
        assert_eq!(status, "idea");
        assert_eq!(
            metadata.pointer("/jira_repo_resolution/previous_status"),
            Some(&Value::String("idea".to_owned()))
        );
        let held_idea = ExistingJiraParent {
            id: idea_id,
            status,
            metadata,
            has_started: false,
            has_completed: false,
        };
        bind_existing_repo(&mut tx, &held_idea, &app_repo)
            .await
            .expect("bind idea Jira parent without promoting it");
        let (status, metadata, repo_id): (String, Value, Option<Uuid>) =
            sqlx::query_as("SELECT status, metadata, repo_id FROM work_items WHERE id = $1")
                .bind(idea_id)
                .fetch_one(&mut *tx)
                .await
                .expect("read bound idea Jira parent");
        assert_eq!(status, "idea");
        assert!(metadata.get("jira_execution_hold").is_none());
        assert!(metadata.get("jira_repo_resolution").is_none());
        assert_eq!(repo_id, Some(app_repo.id));

        let council_id = Uuid::new_v4();
        let council_metadata = json!({"jira_execution_hold": "awaiting_council"});
        sqlx::query(
            "INSERT INTO work_items (id, status, metadata, repo_id, repo_url, repo_path) \
             VALUES ($1, 'blocked', $2, $3, 'stale', '/stale')",
        )
        .bind(council_id)
        .bind(&council_metadata)
        .bind(app_repo.id)
        .execute(&mut *tx)
        .await
        .expect("insert council-held Jira parent");
        let council = ExistingJiraParent {
            id: council_id,
            ..existing("blocked", council_metadata)
        };
        hold_existing_for_repo_resolution(
            &mut tx,
            &council,
            REPO_UNRESOLVED_HOLD,
            "no_exact_match",
            Vec::new(),
            false,
        )
        .await
        .expect("record repo hold without replacing Council hold");
        let (status, metadata, repo_id): (String, Value, Option<Uuid>) =
            sqlx::query_as("SELECT status, metadata, repo_id FROM work_items WHERE id = $1")
                .bind(council_id)
                .fetch_one(&mut *tx)
                .await
                .expect("read council-held Jira parent");
        assert_eq!(status, "blocked");
        assert_eq!(
            metadata.get("jira_execution_hold"),
            Some(&Value::String("awaiting_council".to_owned()))
        );
        assert_eq!(
            metadata.pointer("/jira_repo_resolution/reason"),
            Some(&Value::String(REPO_HOLD_REASON.to_owned()))
        );
        assert!(repo_id.is_none());

        let council_with_repo_hold = ExistingJiraParent {
            id: council_id,
            status,
            metadata,
            has_started: false,
            has_completed: false,
        };
        bind_existing_repo(&mut tx, &council_with_repo_hold, &app_repo)
            .await
            .expect("bind repo without releasing Council hold");
        let (status, metadata, repo_id): (String, Value, Option<Uuid>) =
            sqlx::query_as("SELECT status, metadata, repo_id FROM work_items WHERE id = $1")
                .bind(council_id)
                .fetch_one(&mut *tx)
                .await
                .expect("read bound Council-held Jira parent");
        assert_eq!(status, "blocked");
        assert_eq!(
            metadata.get("jira_execution_hold"),
            Some(&Value::String("awaiting_council".to_owned()))
        );
        assert!(metadata.get("jira_repo_resolution").is_none());
        assert_eq!(repo_id, Some(app_repo.id));

        for (protected_status, has_started, has_completed) in
            [("building", true, false), ("done", false, true)]
        {
            let protected_id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO work_items \
                     (id, status, metadata, repo_url, repo_path, started_at, completed_at) \
                 VALUES ($1, $2, '{}', 'keep-url', '/keep-path', \
                         CASE WHEN $3 THEN NOW() ELSE NULL END, \
                         CASE WHEN $4 THEN NOW() ELSE NULL END)",
            )
            .bind(protected_id)
            .bind(protected_status)
            .bind(has_started)
            .bind(has_completed)
            .execute(&mut *tx)
            .await
            .expect("insert protected Jira parent");
            let protected = ExistingJiraParent {
                id: protected_id,
                status: protected_status.to_owned(),
                metadata: json!({}),
                has_started,
                has_completed,
            };
            hold_existing_for_repo_resolution(
                &mut tx,
                &protected,
                REPO_UNRESOLVED_HOLD,
                "no_exact_match",
                Vec::new(),
                false,
            )
            .await
            .expect("leave protected Jira parent unchanged while holding");
            bind_existing_repo(&mut tx, &protected, &app_repo)
                .await
                .expect("leave protected Jira parent unchanged while binding");
            let (status, metadata, repo_url, repo_path): (
                String,
                Value,
                Option<String>,
                Option<String>,
            ) = sqlx::query_as(
                "SELECT status, metadata, repo_url, repo_path \
                   FROM work_items WHERE id = $1",
            )
            .bind(protected_id)
            .fetch_one(&mut *tx)
            .await
            .expect("read protected Jira parent");
            assert_eq!(status, protected_status);
            assert_eq!(metadata, json!({}));
            assert_eq!(repo_url.as_deref(), Some("keep-url"));
            assert_eq!(repo_path.as_deref(), Some("/keep-path"));
        }

        tx.rollback()
            .await
            .expect("rollback Jira repo resolution test");
        pg.close().await;
    }
}
