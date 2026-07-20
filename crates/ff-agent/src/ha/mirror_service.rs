//! LAN git mirror health monitor + fallback fetch.
//!
//! ForgeFleet can route git fetches through a LAN mirror via
//! [`register_github_mirror_rewrite`] or per-repo `remote set-url`.  When that
//! mirror lags behind GitHub (or is outright unreachable), builds can branch
//! from stale refs or fail entirely.  This module detects stale mirrors by
//! comparing the SHA advertised by the mirror against the SHA returned by the
//! GitHub API, then falls back to a direct GitHub fetch when the mirror is not
//! current.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::process::Command;
use tracing::{info, warn};

use crate::project_github_sync::parse_owner_repo;
use crate::software_upstream::github_get_json;

/// Default timeout for the `git ls-remote` probe against a mirror.
const MIRROR_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Default timeout for the actual `git fetch` operations.
const FETCH_TIMEOUT: Duration = Duration::from_secs(120);

/// Max attempts for each fetch phase (mirror and direct fallback).
const FETCH_ATTEMPTS: usize = 3;

/// Bounded exponential backoff base: 500ms → 1s → 2s before each retry.
const FETCH_BACKOFF_BASE_MS: u64 = 500;

/// Health of a LAN git mirror for a specific repository/branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MirrorHealth {
    /// Mirror advertises the same SHA as GitHub for the branch.
    Healthy {
        /// Commit SHA that both GitHub and the mirror advertise.
        sha: String,
    },
    /// Mirror is reachable but advertises a different SHA than GitHub.
    Stale {
        /// Latest SHA on GitHub.
        github_sha: String,
        /// SHA advertised by the mirror (may be empty if parsing failed).
        mirror_sha: String,
    },
    /// Mirror could not be reached or its response could not be parsed.
    Unreachable {
        /// Underlying error message.
        error: String,
    },
}

impl MirrorHealth {
    /// Returns `true` if the mirror can be trusted for this branch.
    pub fn is_healthy(&self) -> bool {
        matches!(self, MirrorHealth::Healthy { .. })
    }

    /// Returns `true` if a direct GitHub fetch should be used instead.
    pub fn needs_fallback(&self) -> bool {
        !self.is_healthy()
    }
}

/// Query the GitHub API for the current commit SHA of `branch`.
///
/// A missing repo or branch surfaces as an `Err` (permanent 404 from
/// [`github_get_json`]), not as `Ok(None)`.
pub async fn fetch_github_sha(
    http: &reqwest::Client,
    owner: &str,
    repo: &str,
    branch: &str,
    token: Option<&str>,
) -> Result<Option<String>, String> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/commits/{branch}");
    let body = github_get_json(http, &url, token).await?;
    body.get("sha")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing .sha in {url} response"))
        .map(Some)
}

/// Probe a mirror for the SHA it advertises for `branch`.
///
/// `mirror_url` is the replacement prefix configured for
/// `url.<mirror>.insteadOf`, e.g. `https://git-mirror.local/` or
/// `git@git-mirror.local:`.  The full repo URL is built by appending
/// `{owner}/{repo}`.
///
/// Returns `Ok(None)` when the branch is not advertised by the mirror.
pub async fn fetch_mirror_sha(
    mirror_url: &str,
    owner: &str,
    repo: &str,
    branch: &str,
) -> Result<Option<String>, String> {
    let remote_url = format!("{mirror_url}{owner}/{repo}");
    let refspec = format!("refs/heads/{branch}");

    let output = tokio::time::timeout(
        MIRROR_PROBE_TIMEOUT,
        Command::new("git")
            .args(["ls-remote", &remote_url, &refspec])
            .output(),
    )
    .await
    .map_err(|_| format!("git ls-remote {remote_url} timed out after {MIRROR_PROBE_TIMEOUT:?}"))?
    .map_err(|e| format!("spawn git ls-remote {remote_url}: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "git ls-remote {remote_url} failed ({}): {stderr}",
            output.status
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        // Format: "<sha>\t<ref>"
        let mut parts = line.split('\t');
        if let (Some(sha), Some(r)) = (parts.next(), parts.next()) {
            if r == refspec && !sha.is_empty() {
                return Ok(Some(sha.to_string()));
            }
        }
    }

    Ok(None)
}

/// Compare the mirror's advertised SHA with GitHub's SHA for a branch.
///
/// `github_url` is used only to derive `owner/repo` via [`parse_owner_repo`];
/// the actual GitHub query goes through the API.
pub async fn check_mirror_health(
    http: &reqwest::Client,
    github_url: &str,
    mirror_url: &str,
    branch: &str,
    token: Option<&str>,
) -> MirrorHealth {
    let (owner, repo) = match parse_owner_repo(github_url) {
        Some(pair) => pair,
        None => {
            return MirrorHealth::Unreachable {
                error: format!("could not parse owner/repo from {github_url}"),
            };
        }
    };

    let github_sha = match fetch_github_sha(http, &owner, &repo, branch, token).await {
        Ok(Some(sha)) => sha,
        Ok(None) => {
            return MirrorHealth::Unreachable {
                error: format!("branch {branch} not found on GitHub for {owner}/{repo}"),
            };
        }
        Err(e) => {
            return MirrorHealth::Unreachable {
                error: format!("GitHub API lookup failed: {e}"),
            };
        }
    };

    let mirror_sha = match fetch_mirror_sha(mirror_url, &owner, &repo, branch).await {
        Ok(Some(sha)) => sha,
        Ok(None) => {
            return MirrorHealth::Stale {
                github_sha,
                mirror_sha: String::new(),
            };
        }
        Err(e) => {
            return MirrorHealth::Unreachable {
                error: format!("mirror probe failed: {e}"),
            };
        }
    };

    if mirror_sha.eq_ignore_ascii_case(&github_sha) {
        MirrorHealth::Healthy { sha: github_sha }
    } else {
        MirrorHealth::Stale {
            github_sha,
            mirror_sha,
        }
    }
}

/// Fetch `origin/{branch}` from a LAN mirror, falling back to direct GitHub if
/// the mirror is unreachable or stale.
///
/// On entry the repo's `origin` remote is assumed to point at the mirror.  On
/// fallback the remote URL is rewritten to `github_url` for the duration of the
/// fetch and then restored to the mirror URL so subsequent push/fetch behavior
/// remains mirror-first.
///
/// Returns the resolved [`MirrorHealth`] so callers can log whether the mirror
/// or the fallback path was used.
pub async fn fetch_with_fallback(
    repo_path: &Path,
    branch: &str,
    github_url: &str,
    mirror_url: &str,
    http: &reqwest::Client,
    token: Option<&str>,
) -> Result<MirrorHealth> {
    let health = tokio::time::timeout(
        Duration::from_secs(30),
        check_mirror_health(http, github_url, mirror_url, branch, token),
    )
    .await
    .unwrap_or_else(|_| MirrorHealth::Unreachable {
        error: "mirror health check timed out".to_string(),
    });

    if health.needs_fallback() {
        warn!(
            branch,
            health = ?health,
            "mirror_service: mirror is not current; falling back to direct GitHub fetch"
        );
        run_git(
            repo_path,
            ["remote", "set-url", "origin", github_url],
            FETCH_TIMEOUT,
        )
        .await
        .with_context(|| format!("set origin to GitHub URL {github_url}"))?;
    } else {
        let sha = match &health {
            MirrorHealth::Healthy { sha } => sha.as_str(),
            _ => "",
        };
        info!(
            branch,
            sha, "mirror_service: mirror is current; fetching from mirror"
        );
    }

    let mut fetched = false;
    for attempt in 0..FETCH_ATTEMPTS {
        if attempt > 0 {
            let backoff = Duration::from_millis(FETCH_BACKOFF_BASE_MS * (1u64 << (attempt - 1)));
            tokio::time::sleep(backoff).await;
        }
        match run_git(repo_path, ["fetch", "origin", branch], FETCH_TIMEOUT).await {
            Ok(_) => {
                fetched = true;
                break;
            }
            Err(e) => {
                warn!(
                    branch,
                    attempt,
                    error = %e,
                    "mirror_service: fetch failed; retrying"
                );
            }
        }
    }

    if !fetched {
        bail!("mirror_service: could not fetch origin/{branch} in {FETCH_ATTEMPTS} tries");
    }

    // Restore mirror URL so future operations remain mirror-first.
    if health.needs_fallback() {
        if let Err(e) = run_git(
            repo_path,
            ["remote", "set-url", "origin", mirror_url],
            FETCH_TIMEOUT,
        )
        .await
        {
            warn!(
                mirror_url,
                error = %e,
                "mirror_service: failed to restore mirror URL after fallback fetch"
            );
        }
    }

    Ok(health)
}

/// Run a git command with a bounded timeout.
async fn run_git<I, S>(cwd: &Path, args: I, timeout: Duration) -> Result<std::process::Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd);
    for arg in args {
        cmd.arg(arg);
    }
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(output)) if output.status.success() => Ok(output),
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(anyhow::anyhow!("git failed: {stderr}"))
        }
        Ok(Err(e)) => Err(anyhow::anyhow!("git spawn failed: {e}")),
        Err(_) => Err(anyhow::anyhow!("git command timed out after {timeout:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_health_helpers() {
        let h = MirrorHealth::Healthy {
            sha: "abc".to_string(),
        };
        assert!(h.is_healthy());
        assert!(!h.needs_fallback());

        let s = MirrorHealth::Stale {
            github_sha: "abc".to_string(),
            mirror_sha: "def".to_string(),
        };
        assert!(!s.is_healthy());
        assert!(s.needs_fallback());

        let u = MirrorHealth::Unreachable {
            error: "boom".to_string(),
        };
        assert!(!u.is_healthy());
        assert!(u.needs_fallback());
    }

    #[test]
    fn parse_owner_repo_rejects_bad_url_for_health_check() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let http = reqwest::Client::new();
            let health = check_mirror_health(&http, "not a url", "https://mirror.local/", "main", None).await;
            assert!(
                matches!(health, MirrorHealth::Unreachable { error } if error.contains("could not parse owner/repo"))
            );
        });
    }
}
