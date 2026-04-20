//! External-tools upstream version checker.
//!
//! Queries upstream registries (GitHub releases, Homebrew, PyPI) for the
//! latest version of every entry in `external_tools`, then updates
//! `latest_version` + `latest_version_at` in the DB. Also flips any
//! `computer_external_tools` row whose `installed_version` differs from
//! the new `latest_version` into `status = 'upgrade_available'`.
//!
//! Mirrors [`crate::software_upstream`] — see that module for the full
//! dispatch matrix. We intentionally copy rather than share to keep the
//! two subsystems decoupled (the user may want different policies per
//! catalog in the future).
//!
//! Dispatch matrix (via `version_source.method`):
//!   - `"github_release"` with `repo = "owner/name"`
//!   - `"brew"`            with `formula = "name"`
//!   - `"pip"`             with `package = "name"`
//!   - other/empty         — SKIPPED

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sqlx::{PgPool, Row};
use thiserror::Error;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// User-Agent string sent to every upstream API.
const USER_AGENT: &str = "ForgeFleet/1.0";

/// Per-request HTTP timeout.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors that can occur while constructing or running the checker.
#[derive(Debug, Error)]
pub enum UpstreamError {
    #[error("reqwest client build failed: {0}")]
    Client(#[from] reqwest::Error),

    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
}

/// Report returned by [`ExternalToolsUpstreamChecker::check_all`].
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CheckReport {
    pub checked: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub skipped: usize,
    pub errors: Vec<(String, String)>,
    pub details: Vec<CheckDetail>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckDetail {
    pub tool_id: String,
    pub method: String,
    pub old_version: Option<String>,
    pub new_version: Option<String>,
    /// "updated" | "unchanged" | "skipped" | "error"
    pub status: String,
    pub message: Option<String>,
}

/// Upstream checker for the external-tools catalog.
pub struct ExternalToolsUpstreamChecker {
    pg: PgPool,
}

impl ExternalToolsUpstreamChecker {
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    /// Run one pass over every `external_tools` row.
    pub async fn check_all(&self) -> Result<CheckReport, UpstreamError> {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()?;

        let github_token = ff_db::pg_get_secret(&self.pg, "github.venkat_pat")
            .await
            .unwrap_or(None);

        let rows = sqlx::query(
            "SELECT id, version_source, latest_version
               FROM external_tools
              ORDER BY id",
        )
        .fetch_all(&self.pg)
        .await?;

        let mut report = CheckReport {
            checked: rows.len(),
            ..CheckReport::default()
        };

        for row in rows {
            let id: String = row.get("id");
            let version_source: JsonValue = row.get("version_source");
            let old_version: Option<String> = row.get("latest_version");

            let method = version_source
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let query_result =
                query_upstream(&http, &method, &version_source, github_token.as_deref()).await;

            match query_result {
                UpstreamResult::Version(new_version) => {
                    let changed = match &old_version {
                        Some(old) => old != &new_version,
                        None => true,
                    };

                    if changed {
                        sqlx::query(
                            "UPDATE external_tools
                                SET latest_version    = $1,
                                    latest_version_at = NOW()
                              WHERE id = $2",
                        )
                        .bind(&new_version)
                        .bind(&id)
                        .execute(&self.pg)
                        .await?;

                        sqlx::query(
                            "UPDATE computer_external_tools
                                SET status = 'upgrade_available'
                              WHERE tool_id = $1
                                AND installed_version IS NOT NULL
                                AND installed_version <> $2
                                AND status NOT IN ('upgrading', 'installing', 'upgrade_available')",
                        )
                        .bind(&id)
                        .bind(&new_version)
                        .execute(&self.pg)
                        .await?;

                        report.updated += 1;
                        report.details.push(CheckDetail {
                            tool_id: id.clone(),
                            method,
                            old_version,
                            new_version: Some(new_version),
                            status: "updated".to_string(),
                            message: None,
                        });
                    } else {
                        report.unchanged += 1;
                        report.details.push(CheckDetail {
                            tool_id: id.clone(),
                            method,
                            old_version: old_version.clone(),
                            new_version: old_version,
                            status: "unchanged".to_string(),
                            message: None,
                        });
                    }
                }
                UpstreamResult::Skipped(reason) => {
                    report.skipped += 1;
                    report.details.push(CheckDetail {
                        tool_id: id.clone(),
                        method,
                        old_version,
                        new_version: None,
                        status: "skipped".to_string(),
                        message: Some(reason),
                    });
                }
                UpstreamResult::Error(msg) => {
                    warn!(tool_id = %id, error = %msg, "external-tools upstream check failed");
                    report.errors.push((id.clone(), msg.clone()));
                    report.details.push(CheckDetail {
                        tool_id: id.clone(),
                        method,
                        old_version,
                        new_version: None,
                        status: "error".to_string(),
                        message: Some(msg),
                    });
                }
            }
        }

        info!(
            checked = report.checked,
            updated = report.updated,
            unchanged = report.unchanged,
            skipped = report.skipped,
            errors = report.errors.len(),
            "external-tools upstream check complete"
        );

        Ok(report)
    }

    /// Spawn a background tick. First tick fires ~75s after spawn so the
    /// daemon's other subsystems come up first.
    pub fn spawn(
        self,
        interval_hours: u64,
        mut shutdown: watch::Receiver<bool>,
    ) -> JoinHandle<()> {
        let interval = Duration::from_secs(interval_hours.max(1) * 3600);
        let kickoff = Duration::from_secs(75);

        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(kickoff) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return; }
                }
            }

            loop {
                match self.check_all().await {
                    Ok(report) => debug!(
                        checked = report.checked,
                        updated = report.updated,
                        errors = report.errors.len(),
                        "external-tools upstream tick"
                    ),
                    Err(err) => warn!(error = %err, "external-tools upstream tick failed"),
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

enum UpstreamResult {
    Version(String),
    Skipped(String),
    Error(String),
}

async fn query_upstream(
    http: &reqwest::Client,
    method: &str,
    version_source: &JsonValue,
    github_token: Option<&str>,
) -> UpstreamResult {
    match method {
        "github_release" => {
            let Some(repo) = version_source.get("repo").and_then(|v| v.as_str()) else {
                return UpstreamResult::Error("github_release missing 'repo'".to_string());
            };
            match fetch_github_latest(http, repo, github_token).await {
                Ok(v) => UpstreamResult::Version(v),
                Err(e) => UpstreamResult::Error(e),
            }
        }
        "brew" => {
            let Some(formula) = version_source.get("formula").and_then(|v| v.as_str()) else {
                return UpstreamResult::Error("brew missing 'formula'".to_string());
            };
            match fetch_brew_latest(http, formula).await {
                Ok(v) => UpstreamResult::Version(v),
                Err(e) => UpstreamResult::Error(e),
            }
        }
        "pip" => {
            let Some(pkg) = version_source.get("package").and_then(|v| v.as_str()) else {
                return UpstreamResult::Error("pip missing 'package'".to_string());
            };
            match fetch_pip_latest(http, pkg).await {
                Ok(v) => UpstreamResult::Version(v),
                Err(e) => UpstreamResult::Error(e),
            }
        }
        "" => UpstreamResult::Error("version_source missing 'method' field".to_string()),
        other => UpstreamResult::Skipped(format!("unknown method '{other}'")),
    }
}

async fn fetch_github_latest(
    http: &reqwest::Client,
    repo: &str,
    token: Option<&str>,
) -> Result<String, String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let mut req = http.get(&url).header("Accept", "application/vnd.github+json");
    if let Some(t) = token {
        if !t.is_empty() {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
    }
    let resp = req.send().await.map_err(|e| format!("GET {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GET {url}: HTTP {}", resp.status()));
    }
    let body: JsonValue = resp
        .json()
        .await
        .map_err(|e| format!("parse JSON from {url}: {e}"))?;
    let tag = body
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("missing tag_name in {url} response"))?;
    Ok(strip_v_prefix(tag).to_string())
}

async fn fetch_brew_latest(http: &reqwest::Client, formula: &str) -> Result<String, String> {
    let url = format!("https://formulae.brew.sh/api/formula/{formula}.json");
    let resp = http.get(&url).send().await.map_err(|e| format!("GET {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GET {url}: HTTP {}", resp.status()));
    }
    let body: JsonValue = resp
        .json()
        .await
        .map_err(|e| format!("parse JSON from {url}: {e}"))?;
    let stable = body
        .get("versions")
        .and_then(|v| v.get("stable"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("missing versions.stable in {url} response"))?;
    Ok(stable.to_string())
}

async fn fetch_pip_latest(http: &reqwest::Client, package: &str) -> Result<String, String> {
    let url = format!("https://pypi.org/pypi/{package}/json");
    let resp = http.get(&url).send().await.map_err(|e| format!("GET {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GET {url}: HTTP {}", resp.status()));
    }
    let body: JsonValue = resp
        .json()
        .await
        .map_err(|e| format!("parse JSON from {url}: {e}"))?;
    let version = body
        .get("info")
        .and_then(|v| v.get("version"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("missing info.version in {url} response"))?;
    Ok(version.to_string())
}

fn strip_v_prefix(tag: &str) -> &str {
    if let Some(rest) = tag.strip_prefix('v') {
        if rest.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
            return rest;
        }
    }
    tag
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_v_prefix_only_for_digit_versions() {
        assert_eq!(strip_v_prefix("v2.64.0"), "2.64.0");
        assert_eq!(strip_v_prefix("2.0.0"), "2.0.0");
        assert_eq!(strip_v_prefix("vintage-release"), "vintage-release");
        assert_eq!(strip_v_prefix(""), "");
    }
}
