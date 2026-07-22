//! LAN git mirror maintenance + health monitor + fallback fetch.
//!
//! ForgeFleet can route git fetches through a LAN mirror via
//! [`register_github_mirror_rewrite`] or per-repo `remote set-url`.  This
//! module has two halves:
//!
//! * [`MirrorFetchService`] — maintains the bare mirror clone itself
//!   (`git clone --mirror` on first run, then a periodic fetch every 30s plus
//!   webhook-triggered refreshes via [`MirrorFetchTrigger`]).
//! * Health checking / fallback — when a mirror lags behind GitHub (or is
//!   outright unreachable), builds can branch from stale refs or fail
//!   entirely.  [`check_mirror_health`] detects stale mirrors by comparing the
//!   SHA advertised by the mirror against the SHA returned by the GitHub API,
//!   and [`fetch_with_fallback`] falls back to a direct GitHub fetch when the
//!   mirror is not current.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use tokio::process::Command;
use tokio::sync::{Notify, RwLock, watch};
use tokio::task::JoinHandle;
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

// ─── Bare mirror maintenance service ─────────────────────────────────────────

/// Default interval between periodic mirror fetches.
pub const DEFAULT_MIRROR_FETCH_INTERVAL: Duration = Duration::from_secs(30);

/// Timeout for the initial `git clone --mirror` — a fresh clone of a large
/// repo takes much longer than an incremental fetch.
const CLONE_TIMEOUT: Duration = Duration::from_secs(600);

/// Configuration for [`MirrorFetchService`].
#[derive(Debug, Clone)]
pub struct MirrorFetchConfig {
    /// Upstream repository URL to mirror (GitHub or any git remote).
    pub upstream_url: String,
    /// Filesystem path of the bare mirror clone, e.g.
    /// `~/.forgefleet/mirrors/owner/repo.git`.
    pub mirror_path: PathBuf,
    /// Interval between periodic fetches.
    pub fetch_interval: Duration,
}

impl MirrorFetchConfig {
    /// Config with the default 30-second fetch interval.
    pub fn new(upstream_url: impl Into<String>, mirror_path: impl Into<PathBuf>) -> Self {
        Self {
            upstream_url: upstream_url.into(),
            mirror_path: mirror_path.into(),
            fetch_interval: DEFAULT_MIRROR_FETCH_INTERVAL,
        }
    }
}

/// Outcome of a single [`MirrorFetchService::sync_once`] pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorSyncOutcome {
    /// The bare mirror did not exist yet and was created with
    /// `git clone --mirror`.
    Cloned,
    /// The existing mirror was refreshed with `git remote update --prune`.
    Fetched,
}

/// In-process health snapshot for the bare mirror maintenance loop.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MirrorFetchStatus {
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

impl MirrorFetchStatus {
    pub fn is_healthy(&self) -> bool {
        self.last_success_at.is_some() && self.last_error.is_none()
    }
}

/// Cloneable handle that lets a webhook endpoint request an immediate mirror
/// refresh without waiting for the next periodic tick.
///
/// Multiple triggers before the service wakes up coalesce into a single
/// fetch.
#[derive(Debug, Clone)]
pub struct MirrorFetchTrigger(Arc<Notify>);

impl MirrorFetchTrigger {
    /// Wake the service loop for an out-of-band fetch (e.g. on a GitHub push
    /// webhook).
    pub fn request_fetch(&self) {
        self.0.notify_one();
    }
}

/// Maintains a bare mirror clone of an upstream repository.
///
/// On each sync pass the service creates the mirror with `git clone --mirror`
/// if it does not exist yet, otherwise refreshes every ref with
/// `git remote update --prune` (mirror clones carry the `+refs/*:refs/*`
/// refspec, so this keeps branches, tags, and deletions in lockstep with
/// upstream). [`spawn`](Self::spawn) runs the pass every
/// [`fetch_interval`](MirrorFetchConfig::fetch_interval) and immediately when
/// a [`MirrorFetchTrigger`] fires.
pub struct MirrorFetchService {
    config: MirrorFetchConfig,
    trigger: Arc<Notify>,
    status: Arc<RwLock<MirrorFetchStatus>>,
}

impl MirrorFetchService {
    pub fn new(config: MirrorFetchConfig) -> Self {
        Self {
            config,
            trigger: Arc::new(Notify::new()),
            status: Arc::new(RwLock::new(MirrorFetchStatus::default())),
        }
    }

    /// Handle for webhook handlers to request an immediate fetch.
    pub fn trigger(&self) -> MirrorFetchTrigger {
        MirrorFetchTrigger(Arc::clone(&self.trigger))
    }

    /// Whether the bare mirror clone already exists on disk.
    pub fn mirror_exists(&self) -> bool {
        // A bare repo keeps HEAD at its top level (no .git directory).
        self.config.mirror_path.join("HEAD").is_file()
    }

    /// Return the latest sync attempt, success, and error state.
    pub async fn status(&self) -> MirrorFetchStatus {
        self.status.read().await.clone()
    }

    /// Create the mirror if missing, otherwise fetch all refs from upstream.
    pub async fn sync_once(&self) -> Result<MirrorSyncOutcome> {
        self.status.write().await.last_attempt_at = Some(Utc::now());
        let result = self.sync_once_inner().await;
        let mut status = self.status.write().await;
        match &result {
            Ok(_) => {
                status.last_success_at = Some(Utc::now());
                status.last_error = None;
            }
            Err(error) => status.last_error = Some(error.to_string()),
        }
        result
    }

    async fn sync_once_inner(&self) -> Result<MirrorSyncOutcome> {
        if !self.mirror_exists() {
            self.clone_mirror().await?;
            return Ok(MirrorSyncOutcome::Cloned);
        }

        run_git(
            &self.config.mirror_path,
            ["remote", "update", "--prune"],
            FETCH_TIMEOUT,
        )
        .await
        .with_context(|| {
            format!(
                "update mirror {} from {}",
                self.config.mirror_path.display(),
                self.config.upstream_url
            )
        })?;
        Ok(MirrorSyncOutcome::Fetched)
    }

    async fn clone_mirror(&self) -> Result<()> {
        // `run_git` needs an existing cwd, and a relative mirror_path must
        // stay relative to the caller's cwd, not the clone's parent dir.
        let mirror_path = std::path::absolute(&self.config.mirror_path)
            .with_context(|| format!("absolutize {}", self.config.mirror_path.display()))?;
        let parent = mirror_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/"));
        tokio::fs::create_dir_all(&parent)
            .await
            .with_context(|| format!("create mirror parent dir {}", parent.display()))?;

        info!(
            upstream = %self.config.upstream_url,
            mirror = %mirror_path.display(),
            "mirror_service: creating bare mirror clone"
        );
        run_git(
            &parent,
            [
                std::ffi::OsStr::new("clone"),
                std::ffi::OsStr::new("--mirror"),
                std::ffi::OsStr::new(&self.config.upstream_url),
                mirror_path.as_os_str(),
            ],
            CLONE_TIMEOUT,
        )
        .await
        .with_context(|| format!("git clone --mirror {}", self.config.upstream_url))?;
        Ok(())
    }

    /// Spawn the background maintenance loop.
    ///
    /// The first tick fires immediately (creating the mirror if needed), then
    /// every `fetch_interval`. A [`MirrorFetchTrigger`] wakes the loop early
    /// and resets the periodic timer so a webhook push is not immediately
    /// followed by a redundant periodic fetch.
    pub fn spawn(self, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker =
                tokio::time::interval(self.config.fetch_interval.max(Duration::from_secs(1)));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                let outcome = tokio::select! {
                    _ = ticker.tick() => self.sync_once().await,
                    _ = self.trigger.notified() => {
                        info!(
                            mirror = %self.config.mirror_path.display(),
                            "mirror_service: webhook-triggered mirror fetch"
                        );
                        let outcome = self.sync_once().await;
                        ticker.reset();
                        outcome
                    }
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            break;
                        }
                        continue;
                    }
                };

                match outcome {
                    Ok(MirrorSyncOutcome::Cloned) => {
                        info!(
                            mirror = %self.config.mirror_path.display(),
                            "mirror_service: bare mirror clone created"
                        );
                    }
                    Ok(MirrorSyncOutcome::Fetched) => {
                        tracing::debug!(
                            mirror = %self.config.mirror_path.display(),
                            "mirror_service: mirror refreshed"
                        );
                    }
                    Err(e) => {
                        warn!(
                            mirror = %self.config.mirror_path.display(),
                            error = %e,
                            "mirror_service: mirror sync failed"
                        );
                    }
                }
            }
        })
    }
}

/// Run a git command with a bounded timeout.
async fn run_git<I, S>(cwd: &Path, args: I, timeout: Duration) -> Result<std::process::Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut cmd = Command::new("git");
    cmd.kill_on_drop(true);
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

    #[test]
    fn mirror_fetch_config_defaults_to_30s() {
        let cfg = MirrorFetchConfig::new("https://example.com/o/r.git", "/tmp/r.git");
        assert_eq!(cfg.fetch_interval, Duration::from_secs(30));
        assert_eq!(cfg.upstream_url, "https://example.com/o/r.git");
        assert_eq!(cfg.mirror_path, PathBuf::from("/tmp/r.git"));
    }

    #[test]
    fn mirror_fetch_trigger_wakes_service() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let service = MirrorFetchService::new(MirrorFetchConfig::new(
                "https://example.com/o/r.git",
                "/tmp/r.git",
            ));
            let trigger = service.trigger();
            trigger.request_fetch();
            tokio::time::timeout(Duration::from_secs(1), service.trigger.notified())
                .await
                .expect("trigger should have a pending notification");
        });
    }

    /// Shell out to git for test-repo setup; returns false if git is missing.
    fn test_git(cwd: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .current_dir(cwd)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn sync_once_clones_then_fetches() {
        // Skip when git is not on PATH (mirrors the DB-test early-return rule).
        if !std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            eprintln!("skipping sync_once_clones_then_fetches: git not available");
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let upstream = tmp.path().join("upstream");
        std::fs::create_dir_all(&upstream).unwrap();
        assert!(test_git(&upstream, &["init"]));
        let commit_env = &[
            "-c",
            "user.email=test@forgefleet.local",
            "-c",
            "user.name=ff-test",
        ];
        let mut commit_args: Vec<&str> = commit_env.to_vec();
        commit_args.extend(["commit", "--allow-empty", "-m", "first"]);
        assert!(test_git(&upstream, &commit_args));

        let mirror_path = tmp.path().join("mirrors").join("upstream.git");
        let service = MirrorFetchService::new(MirrorFetchConfig::new(
            upstream.to_string_lossy().to_string(),
            &mirror_path,
        ));
        assert!(!service.mirror_exists());

        let rt = tokio::runtime::Runtime::new().unwrap();

        // First pass: clone --mirror creates a bare repo.
        let outcome = rt.block_on(service.sync_once()).expect("clone pass");
        assert_eq!(outcome, MirrorSyncOutcome::Cloned);
        assert!(service.mirror_exists());
        assert!(mirror_path.join("HEAD").is_file());

        // New upstream commit, then a second pass must fetch it.
        let mut commit2_args: Vec<&str> = commit_env.to_vec();
        commit2_args.extend(["commit", "--allow-empty", "-m", "second"]);
        assert!(test_git(&upstream, &commit2_args));

        let outcome = rt.block_on(service.sync_once()).expect("fetch pass");
        assert_eq!(outcome, MirrorSyncOutcome::Fetched);

        let status = rt.block_on(service.status());
        assert!(status.is_healthy());
        assert!(status.last_attempt_at.is_some());
        assert!(status.last_success_at.is_some());
        assert!(status.last_error.is_none());

        let head_of = |repo: &Path| -> String {
            let out = std::process::Command::new("git")
                .current_dir(repo)
                .args(["rev-parse", "HEAD"])
                .output()
                .expect("rev-parse");
            assert!(out.status.success());
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        assert_eq!(head_of(&upstream), head_of(&mirror_path));
    }
}
