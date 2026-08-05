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
const JIRA_BLOCKED_HOLD: &str = "jira_blocked";
const JIRA_HOLD_PRIOR_STATUS: &str = "jira_hold_prior_status";
const JIRA_HOLD_PRIOR_PARKED: &str = "jira_hold_prior_parked";
const REPO_HOLD_REASON: &str = "jira_repo_resolution";
const REPO_UNRESOLVED_HOLD: &str = "repo_unresolved";
const REPO_MULTI_HOLD: &str = "repo_multi";
const JIRA_REPO_HOLD: &str = "jira_repo_hold";
const JIRA_REPO_HOLD_PRIOR_STATUS: &str = "jira_repo_hold_prior_status";

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

    // Repository rows are execution authority. Keep the exact-match snapshot
    // stable until every Jira binding derived from it is committed. This
    // avoids a rename/insert/delete changing cardinality between resolution
    // and binding without requiring a schema migration.
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("LOCK TABLE project_repos IN SHARE MODE")
        .execute(&mut *tx)
        .await?;
    let repos: Vec<ProjectRepo> = sqlx::query_as(
        "SELECT id, github_url, name, default_branch, local_path \
           FROM project_repos WHERE project_id = $1",
    )
    .bind(PROJECT_ID)
    .fetch_all(&mut *tx)
    .await?;

    for issue in issues.values() {
        upsert_issue(&mut tx, &base_url, &config, issue, &repos).await?;
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
    repos: &[ProjectRepo],
) -> Result<()> {
    let repo_resolution = resolve_repo(&issue.fields.labels, repos);
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

    let existing: Option<ExistingJiraParent> = sqlx::query_as(
        "SELECT id, status, metadata, started_at IS NOT NULL AS has_started, \
                completed_at IS NOT NULL AS has_completed FROM work_items \
          WHERE project_id = $1 AND kind = 'jira' \
            AND metadata->>'jira_issue_id' = $2 \
          ORDER BY created_at LIMIT 1 FOR UPDATE",
    )
    .bind(PROJECT_ID)
    .bind(&issue.id)
    .fetch_optional(&mut **tx)
    .await?;

    let work_item_id = if let Some(existing) = existing {
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
                )
                .await?;
            }
            RepoResolution::NoExactMatch => {
                hold_existing_for_repo_resolution(
                    tx,
                    &existing,
                    REPO_UNRESOLVED_HOLD,
                    "no_exact_match",
                    routable_repo_candidates(repos),
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
                )
                .await?;
            }
        }
        id
    } else {
        match repo_resolution {
            RepoResolution::Unique(repo) if repo_is_routable(repo) => {
                let repo_path =
                    routable_repo_path(repo).context("resolved Jira repo has no local path")?;
                sqlx::query_scalar(
                    "INSERT INTO work_items \
                        (project_id, kind, title, description, labels, status, priority, \
                         created_by, metadata, original_signal, repo_id, repo_url, \
                         repo_path, base_branch) \
                     VALUES ($1, 'jira', $2, $3, $4, 'ready', $5, \
                             'jira_ingest_tick', $6 || $7, $8, $9, $10, $11, $12) \
                     RETURNING id",
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
                .fetch_one(&mut **tx)
                .await?
            }
            RepoResolution::Unique(repo) => {
                let held_metadata = merge_metadata(
                    &metadata,
                    repo_hold_metadata(
                        REPO_UNRESOLVED_HOLD,
                        "incomplete_repo_binding",
                        repo_candidates(&[repo]),
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
                .await?
            }
            RepoResolution::NoExactMatch => {
                let held_metadata = merge_metadata(
                    &metadata,
                    repo_hold_metadata(
                        REPO_UNRESOLVED_HOLD,
                        "no_exact_match",
                        routable_repo_candidates(repos),
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
                .await?
            }
            RepoResolution::MultipleExactMatches(matches) => {
                let held_metadata = merge_metadata(
                    &metadata,
                    repo_hold_metadata(
                        REPO_MULTI_HOLD,
                        "multiple_exact_matches",
                        repo_candidates(&matches),
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
                .await?
            }
        }
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

fn routable_repo_candidates(repos: &[ProjectRepo]) -> Vec<Value> {
    let mut candidates: Vec<&ProjectRepo> =
        repos.iter().filter(|repo| repo_is_routable(repo)).collect();
    candidates.sort_by(|left, right| {
        canonical_repo_name(left)
            .cmp(&canonical_repo_name(right))
            .then_with(|| left.id.cmp(&right.id))
    });
    repo_candidates(&candidates)
}

fn canonical_repo_name(repo: &ProjectRepo) -> Option<&str> {
    repo.name.as_deref().filter(|name| !name.trim().is_empty())
}

fn routable_repo_path(repo: &ProjectRepo) -> Option<&str> {
    repo.local_path
        .as_deref()
        .filter(|path| !path.trim().is_empty())
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
    repo_hold: &str,
    detail: &str,
    allowed_repos: Vec<Value>,
    previous_status: &str,
) -> Value {
    json!({
        "jira_repo_hold": repo_hold,
        "jira_repo_hold_prior_status": previous_status,
        "jira_repo_resolution": {
            "reason": REPO_HOLD_REASON,
            "detail": detail,
            "allowed_repos": allowed_repos
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

fn repo_hold_previous_status(existing: &ExistingJiraParent) -> String {
    if let Some(status) = existing
        .metadata
        .get(JIRA_REPO_HOLD_PRIOR_STATUS)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|status| matches!(*status, "ready" | "idea" | "blocked" | "decomposed"))
    {
        return status.to_owned();
    }
    if existing
        .metadata
        .get("jira_execution_hold")
        .and_then(Value::as_str)
        == Some(JIRA_BLOCKED_HOLD)
        && let Some(status) = existing
            .metadata
            .get(JIRA_HOLD_PRIOR_STATUS)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|status| matches!(*status, "ready" | "idea" | "blocked" | "decomposed"))
    {
        return status.to_owned();
    }
    existing.status.clone()
}

fn repo_binding_may_change(existing: &ExistingJiraParent) -> bool {
    !existing.has_started
        && !existing.has_completed
        && matches!(existing.status.as_str(), "ready" | "idea" | "blocked")
}

async fn bind_existing_repo(
    tx: &mut Transaction<'_, Postgres>,
    existing: &ExistingJiraParent,
    repo: &ProjectRepo,
) -> Result<()> {
    if !repo_binding_may_change(existing) {
        return Ok(());
    }
    let repo_path = routable_repo_path(repo).context("resolved Jira repo has no local path")?;
    sqlx::query(
        "UPDATE work_items \
            SET status = CASE \
                  WHEN status = 'blocked' \
                   AND NULLIF(BTRIM(COALESCE(metadata->>'jira_execution_hold', '')), '') IS NULL \
                   AND metadata->>$7 IN ('ready', 'idea', 'decomposed') \
                  THEN metadata->>$7 ELSE status END, \
                repo_id = $2, repo_url = $3, repo_path = $4, base_branch = $5, \
                metadata = (metadata - $6 - $7 - 'jira_repo_resolution') || $8 \
          WHERE id = $1 AND started_at IS NULL AND completed_at IS NULL \
            AND status IN ('ready', 'blocked', 'idea')",
    )
    .bind(existing.id)
    .bind(repo.id)
    .bind(&repo.github_url)
    .bind(repo_path)
    .bind(&repo.default_branch)
    .bind(JIRA_REPO_HOLD)
    .bind(JIRA_REPO_HOLD_PRIOR_STATUS)
    .bind(repo_bound_metadata(repo))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn hold_existing_for_repo_resolution(
    tx: &mut Transaction<'_, Postgres>,
    existing: &ExistingJiraParent,
    repo_hold: &str,
    detail: &str,
    allowed_repos: Vec<Value>,
) -> Result<()> {
    if !repo_binding_may_change(existing) {
        return Ok(());
    }
    let hold_metadata = repo_hold_metadata(
        repo_hold,
        detail,
        allowed_repos,
        &repo_hold_previous_status(existing),
    );
    sqlx::query(
        "UPDATE work_items \
            SET status = CASE WHEN status = 'ready' THEN 'blocked' ELSE status END, \
                repo_id = NULL, repo_url = NULL, repo_path = NULL, base_branch = NULL, \
                metadata = (metadata - 'jira_repo_id' - 'jira_repo_name' \
                                      - 'jira_repo_resolution_state') || $2 \
          WHERE id = $1 AND started_at IS NULL AND completed_at IS NULL \
            AND status IN ('ready', 'blocked', 'idea')",
    )
    .bind(existing.id)
    .bind(hold_metadata)
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
) -> Result<Uuid> {
    Ok(sqlx::query_scalar(
        "INSERT INTO work_items \
            (project_id, kind, title, description, labels, status, priority, \
             created_by, metadata, original_signal) \
         VALUES ($1, 'jira', $2, $3, $4, 'blocked', $5, \
                 'jira_ingest_tick', $6, $7) \
         RETURNING id",
    )
    .bind(PROJECT_ID)
    .bind(title)
    .bind(description)
    .bind(labels)
    .bind(priority)
    .bind(metadata)
    .bind(original_signal)
    .fetch_one(&mut **tx)
    .await?)
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
            SET status = CASE \
                  WHEN NULLIF(BTRIM(COALESCE(metadata->>'jira_repo_hold', '')), '') IS NOT NULL \
                  THEN 'blocked' ELSE metadata->>$2 END, \
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
    fn exact_case_sensitive_repo_label_resolves_unique_repo() {
        let repos = vec![repo("api", 1), repo("web", 2)];
        assert_eq!(
            resolve_repo(&["web".to_owned()], &repos),
            RepoResolution::Unique(&repos[1])
        );
        for label in ["Web", "web ", "web-api"] {
            assert_eq!(
                resolve_repo(&[label.to_owned()], &repos),
                RepoResolution::NoExactMatch
            );
        }
        let stored_with_spaces = repo(" web ", 3);
        assert_eq!(
            resolve_repo(&["web".to_owned()], &[stored_with_spaces]),
            RepoResolution::NoExactMatch
        );
    }

    #[test]
    fn zero_and_multiple_repo_matches_fail_closed_with_candidates() {
        let repos = vec![repo("web", 2), repo("api", 1)];
        assert_eq!(
            resolve_repo(&["unknown".to_owned()], &repos),
            RepoResolution::NoExactMatch
        );
        assert_eq!(routable_repo_candidates(&repos).len(), 2);

        let RepoResolution::MultipleExactMatches(matches) =
            resolve_repo(&["web".to_owned(), "api".to_owned()], &repos)
        else {
            panic!("expected multiple exact matches");
        };
        let candidates = repo_candidates(&matches);
        assert_eq!(candidates[0]["name"], "api");
        assert_eq!(candidates[1]["name"], "web");
    }

    #[test]
    fn incomplete_exact_repo_is_not_routable() {
        let mut incomplete = repo("app", 1);
        incomplete.local_path = None;
        assert_eq!(
            resolve_repo(&["app".to_owned()], std::slice::from_ref(&incomplete)),
            RepoResolution::Unique(&incomplete)
        );
        assert!(!repo_is_routable(&incomplete));
    }

    #[test]
    fn repo_hold_state_is_independent_of_jira_execution_hold() {
        let held = repo_hold_metadata(REPO_UNRESOLVED_HOLD, "no_exact_match", Vec::new(), "ready");
        assert_eq!(held[JIRA_REPO_HOLD], REPO_UNRESOLVED_HOLD);
        assert_eq!(held[JIRA_REPO_HOLD_PRIOR_STATUS], "ready");
        assert!(held.get("jira_execution_hold").is_none());

        let unrelated = existing(
            "blocked",
            json!({"jira_execution_hold": "awaiting_council"}),
        );
        assert_eq!(repo_hold_previous_status(&unrelated), "blocked");

        let jira_blocked = existing(
            "blocked",
            json!({
                "jira_execution_hold": JIRA_BLOCKED_HOLD,
                "jira_hold_prior_status": "ready"
            }),
        );
        assert_eq!(repo_hold_previous_status(&jira_blocked), "ready");

        let jira_blocked_decomposed = existing(
            "blocked",
            json!({
                "jira_execution_hold": JIRA_BLOCKED_HOLD,
                "jira_hold_prior_status": "decomposed"
            }),
        );
        assert_eq!(
            repo_hold_previous_status(&jira_blocked_decomposed),
            "decomposed"
        );
    }

    #[test]
    fn repository_binding_never_rewrites_started_active_review_or_terminal_rows() {
        for status in [
            "building",
            "in_review",
            "decomposed",
            "done",
            "failed",
            "merged",
        ] {
            assert!(
                !repo_binding_may_change(&existing(status, json!({}))),
                "{status}"
            );
        }
        for status in ["ready", "idea", "blocked"] {
            assert!(
                repo_binding_may_change(&existing(status, json!({}))),
                "{status}"
            );
        }

        let mut started = existing("ready", json!({}));
        started.has_started = true;
        assert!(!repo_binding_may_change(&started));

        let mut completed = existing("blocked", json!({}));
        completed.has_completed = true;
        assert!(!repo_binding_may_change(&completed));
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
        let app_repo: ProjectRepo = sqlx::query_as(
            "INSERT INTO project_repos
                (project_id, github_url, name, default_branch, local_path)
             VALUES ($1, 'https://github.com/example/app.git', 'app', 'main', '/srv/app')
             RETURNING id, github_url, name, default_branch, local_path",
        )
        .bind(PROJECT_ID)
        .fetch_one(&mut *tx)
        .await
        .expect("insert test project repository");
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
            std::slice::from_ref(&app_repo),
        )
        .await
        .expect("upsert blocked issue");
        upsert_issue(
            &mut tx,
            "https://jira.example.test",
            &config,
            &jira_issue(&ready_issue_id, "HFPROD-2", "To Do"),
            std::slice::from_ref(&app_repo),
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
            std::slice::from_ref(&app_repo),
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
            std::slice::from_ref(&app_repo),
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

        let decomposed_issue_id = Uuid::new_v4().to_string();
        let decomposed_id: Uuid = sqlx::query_scalar(
            "INSERT INTO work_items
                (project_id, kind, title, status, priority, labels, created_by,
                 metadata, original_signal, repo_id, repo_url, repo_path, base_branch)
             VALUES ($1, 'jira', 'decomposed parent', 'decomposed', 'normal',
                     '[\"app\"]'::jsonb, 'test', $2, '{}'::jsonb,
                     $3, $4, $5, $6)
             RETURNING id",
        )
        .bind(PROJECT_ID)
        .bind(json!({
            "jira_issue_id": decomposed_issue_id.clone(),
            "jira_issue_key": "HFPROD-3",
            "jira_status": "In Progress",
            "jira_repo_resolution_state": "bound"
        }))
        .bind(app_repo.id)
        .bind(&app_repo.github_url)
        .bind(app_repo.local_path.as_deref())
        .bind(&app_repo.default_branch)
        .fetch_one(&mut *tx)
        .await
        .expect("insert bound decomposed parent");

        hold_jira_parent_and_descendants(&mut tx, decomposed_id)
            .await
            .expect("Jira-block decomposed parent");
        upsert_issue(
            &mut tx,
            "https://jira.example.test",
            &config,
            &jira_issue(&decomposed_issue_id, "HFPROD-3", "Blocked"),
            &[],
        )
        .await
        .expect("make blocked parent repository-unresolved");
        upsert_issue(
            &mut tx,
            "https://jira.example.test",
            &config,
            &jira_issue(&decomposed_issue_id, "HFPROD-3", "In Progress"),
            &[],
        )
        .await
        .expect("resume Jira while repository remains unresolved");
        let (held_status, jira_hold, repo_hold, repo_prior): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT status, metadata->>'jira_execution_hold',
                    metadata->>'jira_repo_hold',
                    metadata->>'jira_repo_hold_prior_status'
               FROM work_items WHERE id = $1",
        )
        .bind(decomposed_id)
        .fetch_one(&mut *tx)
        .await
        .expect("read repository-held decomposed parent");
        assert_eq!(held_status, "blocked");
        assert_eq!(jira_hold, None);
        assert_eq!(repo_hold.as_deref(), Some(REPO_UNRESOLVED_HOLD));
        assert_eq!(repo_prior.as_deref(), Some("decomposed"));

        upsert_issue(
            &mut tx,
            "https://jira.example.test",
            &config,
            &jira_issue(&decomposed_issue_id, "HFPROD-3", "In Progress"),
            std::slice::from_ref(&app_repo),
        )
        .await
        .expect("rebind resumed decomposed parent");
        let (status, repo_hold, resolution_state, repo_id): (
            String,
            Option<String>,
            Option<String>,
            Option<Uuid>,
        ) = sqlx::query_as(
            "SELECT status, metadata->>'jira_repo_hold',
                    metadata->>'jira_repo_resolution_state', repo_id
               FROM work_items WHERE id = $1",
        )
        .bind(decomposed_id)
        .fetch_one(&mut *tx)
        .await
        .expect("read rebound decomposed parent");
        assert_eq!(status, "decomposed");
        assert_eq!(repo_hold, None);
        assert_eq!(resolution_state.as_deref(), Some("bound"));
        assert_eq!(repo_id, Some(app_repo.id));

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
                labels: vec!["app".to_owned()],
                status: Some(NamedField {
                    name: status.to_owned(),
                }),
                ..JiraFields::default()
            },
        }
    }
}
