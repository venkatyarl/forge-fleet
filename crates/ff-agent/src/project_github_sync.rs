//! GitHub sync for projects.
//!
//! For every row in `projects` that has a `repo_url`, this module:
//!   1. parses owner/repo from the URL
//!   2. GETs `/repos/{owner}/{repo}/commits/{default_branch}` to learn the
//!      current main-branch commit SHA + author + commit message
//!   3. GETs `/repos/{owner}/{repo}/branches` and upserts each into
//!      `project_branches` (status='active' for new rows; existing statuses
//!      are preserved)
//!   4. GETs `/repos/{owner}/{repo}/pulls?state=all&per_page=30` and decorates
//!      `project_branches` with `pr_number`, `pr_url`, `pr_state`
//!
//! A GitHub PAT can be provided via `fleet_secrets.github.venkat_pat`; without
//! a token calls still work (public API), just rate-limited. 404s are handled
//! gracefully — a project whose repo doesn't exist yet on GitHub is counted
//! under `missing_repos` without failing the whole pass.
//!
//! The CI-runs table (`project_ci_runs`) is intentionally NOT populated in v1
//! — GitHub Actions runs API is higher-cost and more complex.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use thiserror::Error;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

const USER_AGENT: &str = "ForgeFleet/1.0";
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const GITHUB_API: &str = "https://api.github.com";

/// Errors raised from constructing the sync (pure plumbing errors).
/// Per-project failures are captured in the [`SyncReport`].
#[derive(Debug, Error)]
pub enum GitHubError {
    #[error("reqwest client build failed: {0}")]
    Client(#[from] reqwest::Error),

    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
}

/// Report returned by [`GitHubSync::sync_all_projects`].
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SyncReport {
    /// Total projects considered.
    pub total: usize,
    /// Projects with no `repo_url` (skipped).
    pub skipped_no_repo: usize,
    /// Projects whose `repo_url` couldn't be parsed into owner/repo.
    pub skipped_bad_url: usize,
    /// Projects whose main-branch fetch returned 404.
    pub missing_repos: Vec<String>,
    /// Projects whose main commit row was refreshed (SHA changed OR first sync).
    pub updated_main: usize,
    /// Branches upserted across all projects.
    pub branches_upserted: usize,
    /// project_branches rows whose PR metadata was attached.
    pub prs_attached: usize,
    /// Per-project error messages (project_id, message).
    pub errors: Vec<(String, String)>,
}

/// GitHub sync worker. Owns a Postgres pool.
pub struct GitHubSync {
    pg: PgPool,
}

impl GitHubSync {
    /// Build a new sync worker.
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    /// Run one full pass: for every project with a `repo_url`, refresh main
    /// commit + branches + PR metadata. Errors on individual projects are
    /// captured in the returned [`SyncReport`] rather than returned as `Err`.
    pub async fn sync_all_projects(&self) -> Result<SyncReport, GitHubError> {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()?;

        let token = ff_db::pg_get_secret(&self.pg, "github.venkat_pat")
            .await
            .unwrap_or(None);

        let projects: Vec<(String, Option<String>, String)> =
            sqlx::query_as("SELECT id, repo_url, default_branch FROM projects ORDER BY id")
                .fetch_all(&self.pg)
                .await?;

        let mut report = SyncReport {
            total: projects.len(),
            ..SyncReport::default()
        };

        for (project_id, repo_url, default_branch) in projects {
            let Some(url) = repo_url.as_deref().filter(|s| !s.trim().is_empty()) else {
                report.skipped_no_repo += 1;
                continue;
            };

            let Some((owner, repo)) = parse_owner_repo(url) else {
                report.skipped_bad_url += 1;
                report
                    .errors
                    .push((project_id.clone(), format!("could not parse {url}")));
                continue;
            };

            // ─── 1. main commit ────────────────────────────────────────────
            match fetch_branch_commit(&http, &owner, &repo, &default_branch, token.as_deref()).await
            {
                Ok(Some(info)) => {
                    if let Err(e) = write_main_commit(&self.pg, &project_id, &info).await {
                        report.errors.push((project_id.clone(), e));
                        continue;
                    }
                    report.updated_main += 1;
                }
                Ok(None) => {
                    report.missing_repos.push(project_id.clone());
                    // Still bump last_synced so UIs know we tried.
                    let _ = sqlx::query(
                        "UPDATE projects SET main_last_synced_at = NOW() WHERE id = $1",
                    )
                    .bind(&project_id)
                    .execute(&self.pg)
                    .await;
                    continue;
                }
                Err(e) => {
                    report.errors.push((project_id.clone(), e));
                    continue;
                }
            }

            // ─── 2. branches ───────────────────────────────────────────────
            match fetch_branches(&http, &owner, &repo, token.as_deref()).await {
                Ok(branches) => {
                    for br in branches {
                        if let Err(e) = upsert_branch(&self.pg, &project_id, &br).await {
                            report.errors.push((project_id.clone(), e));
                            continue;
                        }
                        report.branches_upserted += 1;
                    }
                }
                Err(e) => {
                    report
                        .errors
                        .push((project_id.clone(), format!("branches: {e}")));
                }
            }

            // ─── 3. PR metadata on branches ────────────────────────────────
            match fetch_pulls(&http, &owner, &repo, token.as_deref()).await {
                Ok(pulls) => {
                    for pr in pulls {
                        match attach_pr(&self.pg, &project_id, &pr).await {
                            Ok(true) => report.prs_attached += 1,
                            Ok(false) => {}
                            Err(e) => report
                                .errors
                                .push((project_id.clone(), format!("attach pr: {e}"))),
                        }
                    }
                }
                Err(e) => {
                    report
                        .errors
                        .push((project_id.clone(), format!("pulls: {e}")));
                }
            }
        }

        info!(
            total = report.total,
            updated_main = report.updated_main,
            branches = report.branches_upserted,
            prs = report.prs_attached,
            missing = report.missing_repos.len(),
            errors = report.errors.len(),
            "project GitHub sync complete"
        );

        Ok(report)
    }

    /// Spawn a background tick that runs [`Self::sync_all_projects`] every
    /// `interval_mins`. Exits cleanly when `shutdown` flips to `true`.
    pub fn spawn(self, interval_mins: u64, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        let interval = Duration::from_secs(interval_mins.max(1) * 60);
        let kickoff = Duration::from_secs(30);

        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(kickoff) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return; }
                }
            }

            loop {
                match self.sync_all_projects().await {
                    Ok(report) => debug!(
                        total = report.total,
                        updated = report.updated_main,
                        errors = report.errors.len(),
                        "project github sync tick"
                    ),
                    Err(err) => warn!(error = %err, "project github sync tick failed"),
                }

                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() { break; }
                    }
                }
            }
        })
    }
}

// ─── DB writes ────────────────────────────────────────────────────────────

/// One commit's worth of metadata, already parsed from the GitHub JSON blob.
#[derive(Debug, Clone)]
struct CommitInfo {
    sha: String,
    message_first_line: String,
    author_name: Option<String>,
    author_date: Option<chrono::DateTime<chrono::Utc>>,
}

async fn write_main_commit(
    pool: &PgPool,
    project_id: &str,
    info: &CommitInfo,
) -> Result<(), String> {
    sqlx::query(
        "UPDATE projects
            SET main_commit_sha     = $2,
                main_commit_message = $3,
                main_committed_at   = $4,
                main_committed_by   = $5,
                main_last_synced_at = NOW()
          WHERE id = $1",
    )
    .bind(project_id)
    .bind(&info.sha)
    .bind(&info.message_first_line)
    .bind(info.author_date)
    .bind(info.author_name.as_deref())
    .execute(pool)
    .await
    .map_err(|e| format!("update projects: {e}"))?;
    Ok(())
}

#[derive(Debug, Clone)]
struct BranchInfo {
    name: String,
    last_commit_sha: Option<String>,
}

async fn upsert_branch(pool: &PgPool, project_id: &str, branch: &BranchInfo) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO project_branches (
            project_id, branch_name, created_by, last_commit_sha, status
         )
         VALUES ($1, $2, 'github', $3, 'active')
         ON CONFLICT (project_id, branch_name) DO UPDATE SET
            last_commit_sha = EXCLUDED.last_commit_sha",
    )
    .bind(project_id)
    .bind(&branch.name)
    .bind(branch.last_commit_sha.as_deref())
    .execute(pool)
    .await
    .map_err(|e| format!("upsert branch {}: {e}", branch.name))?;
    Ok(())
}

#[derive(Debug, Clone)]
struct PullInfo {
    number: i32,
    state: String,
    head_ref: String,
    url: String,
}

async fn attach_pr(pool: &PgPool, project_id: &str, pr: &PullInfo) -> Result<bool, String> {
    let rows = sqlx::query(
        "UPDATE project_branches
            SET pr_number = $3,
                pr_url    = $4,
                pr_state  = $5
          WHERE project_id = $1
            AND branch_name = $2",
    )
    .bind(project_id)
    .bind(&pr.head_ref)
    .bind(pr.number)
    .bind(&pr.url)
    .bind(&pr.state)
    .execute(pool)
    .await
    .map_err(|e| format!("attach pr #{}: {e}", pr.number))?;
    Ok(rows.rows_affected() > 0)
}

// ─── HTTP fetches ────────────────────────────────────────────────────────

/// Parse `https://github.com/owner/repo[.git]` → `(owner, repo)`.
/// Also handles URLs without a scheme and trailing slashes.
pub fn parse_owner_repo(url: &str) -> Option<(String, String)> {
    let trimmed = url.trim().trim_end_matches('/').trim_end_matches(".git");
    // Strip any known prefix; we only care about the last two path segments.
    let path = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
        .or_else(|| trimmed.strip_prefix("git@github.com:"))
        .or_else(|| trimmed.strip_prefix("github.com/"))
        .unwrap_or(trimmed);

    let mut parts = path.split('/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner, repo))
}

async fn fetch_branch_commit(
    http: &reqwest::Client,
    owner: &str,
    repo: &str,
    branch: &str,
    token: Option<&str>,
) -> Result<Option<CommitInfo>, String> {
    let url = format!("{GITHUB_API}/repos/{owner}/{repo}/commits/{branch}");
    let resp = send_gh(http, &url, token)
        .await
        .map_err(|e| format!("{url}: {e}"))?;

    let status = resp.status();
    if status.as_u16() == 404 {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(format!("{url} returned {status}"));
    }

    let json: serde_json::Value = resp.json().await.map_err(|e| format!("parse {url}: {e}"))?;

    let sha = json
        .get("sha")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing .sha".to_string())?
        .to_string();

    let message_first_line = json
        .pointer("/commit/message")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();

    let author_name = json
        .pointer("/commit/author/name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let author_date = json
        .pointer("/commit/author/date")
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&chrono::Utc));

    Ok(Some(CommitInfo {
        sha,
        message_first_line,
        author_name,
        author_date,
    }))
}

async fn fetch_branches(
    http: &reqwest::Client,
    owner: &str,
    repo: &str,
    token: Option<&str>,
) -> Result<Vec<BranchInfo>, String> {
    let url = format!("{GITHUB_API}/repos/{owner}/{repo}/branches?per_page=100");
    let resp = send_gh(http, &url, token)
        .await
        .map_err(|e| format!("{url}: {e}"))?;

    let status = resp.status();
    if status.as_u16() == 404 {
        return Ok(Vec::new());
    }
    if !status.is_success() {
        return Err(format!("{url} returned {status}"));
    }

    let arr: Vec<serde_json::Value> = resp.json().await.map_err(|e| format!("parse {url}: {e}"))?;

    Ok(arr
        .into_iter()
        .filter_map(|v| {
            let name = v.get("name").and_then(|x| x.as_str())?.to_string();
            let sha = v
                .pointer("/commit/sha")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            Some(BranchInfo {
                name,
                last_commit_sha: sha,
            })
        })
        .collect())
}

async fn fetch_pulls(
    http: &reqwest::Client,
    owner: &str,
    repo: &str,
    token: Option<&str>,
) -> Result<Vec<PullInfo>, String> {
    let url = format!("{GITHUB_API}/repos/{owner}/{repo}/pulls?state=all&per_page=30");
    let resp = send_gh(http, &url, token)
        .await
        .map_err(|e| format!("{url}: {e}"))?;

    let status = resp.status();
    if status.as_u16() == 404 {
        return Ok(Vec::new());
    }
    if !status.is_success() {
        return Err(format!("{url} returned {status}"));
    }

    let arr: Vec<serde_json::Value> = resp.json().await.map_err(|e| format!("parse {url}: {e}"))?;

    Ok(arr
        .into_iter()
        .filter_map(|v| {
            let number = v.get("number").and_then(|x| x.as_i64())? as i32;
            let state = v
                .get("state")
                .and_then(|x| x.as_str())
                .unwrap_or("open")
                .to_string();
            let head_ref = v.pointer("/head/ref").and_then(|x| x.as_str())?.to_string();
            let url = v
                .get("html_url")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string();
            Some(PullInfo {
                number,
                state,
                head_ref,
                url,
            })
        })
        .collect())
}

async fn send_gh(
    http: &reqwest::Client,
    url: &str,
    token: Option<&str>,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut req = http
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(t) = token.filter(|s| !s.trim().is_empty()) {
        req = req.bearer_auth(t.trim());
    }
    req.send().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https_url() {
        let (o, r) = parse_owner_repo("https://github.com/venkatyarl/forge-fleet").unwrap();
        assert_eq!(o, "venkatyarl");
        assert_eq!(r, "forge-fleet");
    }

    #[test]
    fn parses_trailing_slash_and_dot_git() {
        let (o, r) = parse_owner_repo("https://github.com/venkatyarl/forge-fleet.git/").unwrap();
        assert_eq!(o, "venkatyarl");
        assert_eq!(r, "forge-fleet");
    }

    #[test]
    fn parses_ssh_form() {
        let (o, r) = parse_owner_repo("git@github.com:venkatyarl/hireflow360.git").unwrap();
        assert_eq!(o, "venkatyarl");
        assert_eq!(r, "hireflow360");
    }

    #[test]
    fn rejects_bad_url() {
        assert!(parse_owner_repo("not a url").is_none());
        assert!(parse_owner_repo("https://github.com/").is_none());
    }
}
