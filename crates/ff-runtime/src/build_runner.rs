//! Build runner — supervise long-running build jobs with duration tracking.
//!
//! Runs build commands (e.g. compiling llama.cpp / vLLM from source) as child
//! processes, tracks how long each build has been running, and enforces a
//! maximum build duration:
//!
//! - **Duration tracking** — every build records its start time and, once it
//!   finishes (or is killed), its total wall-clock duration.
//! - **Timeout watcher** — a background sweep compares each running build's
//!   duration against [`BuildRunnerConfig::max_build_duration`] and triggers
//!   [`BuildRunner::kill_hung_build`] for any build that exceeds it.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::error::{Result, RuntimeError};
use crate::process_manager::{is_pid_alive, send_signal};

// ─── Configuration ───────────────────────────────────────────────────────────

/// Configuration for the [`BuildRunner`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildRunnerConfig {
    /// Kill a build once it has been running longer than this many seconds.
    pub max_build_duration_secs: u64,
    /// Seconds between timeout-watcher sweeps.
    pub watch_interval_secs: u64,
    /// Seconds to wait for graceful SIGTERM before sending SIGKILL.
    pub kill_timeout_secs: u64,
}

impl Default for BuildRunnerConfig {
    fn default() -> Self {
        Self {
            max_build_duration_secs: 1800,
            watch_interval_secs: 10,
            kill_timeout_secs: 10,
        }
    }
}

impl BuildRunnerConfig {
    /// Maximum allowed build duration as a [`Duration`].
    pub fn max_build_duration(&self) -> Duration {
        Duration::from_secs(self.max_build_duration_secs)
    }
}

// ─── Build State ─────────────────────────────────────────────────────────────

/// Lifecycle state of a tracked build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuildStatus {
    /// The build process is still running.
    Running,
    /// The build exited with a zero status.
    Completed,
    /// The build exited with a non-zero status (or was killed externally).
    Failed { exit_code: Option<i32> },
    /// The build exceeded `max_build_duration` and was killed by the watcher.
    TimedOut,
}

/// A build job tracked by the [`BuildRunner`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedBuild {
    /// Runner-assigned build ID.
    pub id: u64,
    /// OS process ID of the build.
    pub pid: u32,
    /// Full command line of the build.
    pub cmd_line: String,
    /// Current lifecycle state.
    pub status: BuildStatus,
    /// Total wall-clock duration in seconds, set once the build finishes
    /// (or is killed).
    pub duration_secs: Option<f64>,
    /// When the build was started.
    #[serde(skip, default = "Instant::now")]
    pub started_at: Instant,
}

// ─── Build Runner ────────────────────────────────────────────────────────────

/// Runs build commands and enforces a maximum build duration.
///
/// Thread-safe: inner state is behind `Arc<RwLock<..>>`.
#[derive(Clone)]
pub struct BuildRunner {
    builds: Arc<RwLock<HashMap<u64, TrackedBuild>>>,
    next_id: Arc<AtomicU64>,
    config: BuildRunnerConfig,
}

impl BuildRunner {
    /// Create a new build runner with default configuration.
    pub fn new() -> Self {
        Self::with_config(BuildRunnerConfig::default())
    }

    /// Create with explicit configuration.
    pub fn with_config(config: BuildRunnerConfig) -> Self {
        Self {
            builds: Arc::new(RwLock::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            config,
        }
    }

    // ── Start ─────────────────────────────────────────────────────────────

    /// Spawn a build command and start tracking its duration.
    ///
    /// Returns the runner-assigned build ID. Output is discarded; builds that
    /// need their output captured should redirect to a log file themselves.
    pub async fn start_build(
        &self,
        program: &str,
        args: &[String],
        cwd: Option<&std::path::Path>,
    ) -> Result<u64> {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }

        let mut child = command.spawn().map_err(|e| RuntimeError::StartFailed {
            reason: format!("failed to spawn build `{program}`: {e}"),
        })?;

        let pid = child.id();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cmd_line = std::iter::once(program.to_string())
            .chain(args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ");

        self.builds.write().await.insert(
            id,
            TrackedBuild {
                id,
                pid,
                cmd_line,
                status: BuildStatus::Running,
                duration_secs: None,
                started_at: Instant::now(),
            },
        );

        // Reap the child and record its exit + duration, unless the timeout
        // watcher already marked it timed-out.
        let builds = Arc::clone(&self.builds);
        tokio::task::spawn_blocking(move || {
            let exit = child.wait();
            let mut builds = builds.blocking_write();
            if let Some(build) = builds.get_mut(&id)
                && build.status == BuildStatus::Running
            {
                build.duration_secs = Some(build.started_at.elapsed().as_secs_f64());
                build.status = match exit {
                    Ok(status) if status.success() => BuildStatus::Completed,
                    Ok(status) => BuildStatus::Failed {
                        exit_code: status.code(),
                    },
                    Err(_) => BuildStatus::Failed { exit_code: None },
                };
            }
        });

        info!(id, pid, "build started");
        Ok(id)
    }

    // ── Kill Hung Build ───────────────────────────────────────────────────

    /// Kill a build that exceeded its allowed duration.
    ///
    /// Marks the build [`BuildStatus::TimedOut`], sends `SIGTERM`, waits up to
    /// `kill_timeout_secs` for it to exit, then escalates to `SIGKILL`.
    pub async fn kill_hung_build(&self, build_id: u64) -> Result<()> {
        let (pid, elapsed) = {
            let mut builds = self.builds.write().await;
            let build = builds.get_mut(&build_id).ok_or(RuntimeError::NotRunning)?;
            if build.status != BuildStatus::Running {
                return Err(RuntimeError::NotRunning);
            }
            // Mark first so the reaper thread doesn't overwrite the status
            // with Completed/Failed when the killed process exits.
            let elapsed = build.started_at.elapsed();
            build.status = BuildStatus::TimedOut;
            build.duration_secs = Some(elapsed.as_secs_f64());
            (build.pid, elapsed)
        };

        warn!(
            build_id,
            pid,
            elapsed_secs = elapsed.as_secs(),
            "killing hung build"
        );

        send_signal(pid, "TERM");

        let start = Instant::now();
        let timeout = Duration::from_secs(self.config.kill_timeout_secs);

        loop {
            if !is_pid_alive(pid) {
                info!(build_id, pid, "hung build terminated");
                break;
            }
            if start.elapsed() > timeout {
                warn!(build_id, pid, "SIGTERM timeout, sending SIGKILL");
                send_signal(pid, "KILL");
                tokio::time::sleep(Duration::from_millis(500)).await;
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        Ok(())
    }

    // ── Timeout Watcher ───────────────────────────────────────────────────

    /// One timeout-watcher sweep: compare every running build's duration
    /// against `max_build_duration` and kill any build that exceeded it.
    ///
    /// Returns the IDs of the builds that were killed.
    pub async fn check_and_kill_stuck_builds(&self) -> Vec<u64> {
        let max = self.config.max_build_duration();

        let expired: Vec<u64> = {
            let builds = self.builds.read().await;
            builds
                .values()
                .filter(|b| b.status == BuildStatus::Running && b.started_at.elapsed() > max)
                .map(|b| b.id)
                .collect()
        };

        let mut killed = Vec::new();
        for id in expired {
            warn!(
                build_id = id,
                max_secs = self.config.max_build_duration_secs,
                "build exceeded max_build_duration"
            );
            if self.kill_hung_build(id).await.is_ok() {
                killed.push(id);
            }
        }
        killed
    }

    /// Compatibility alias for [`BuildRunner::check_and_kill_stuck_builds`].
    pub async fn check_timeouts(&self) -> Vec<u64> {
        self.check_and_kill_stuck_builds().await
    }

    /// Spawn the background timeout watcher.
    ///
    /// Sweeps every `watch_interval_secs`, monitoring each running build's
    /// duration against `max_build_duration` and triggering
    /// [`BuildRunner::kill_hung_build`] for any build that exceeded it.
    /// Abort the returned handle to stop the watcher.
    pub fn spawn_timeout_watcher(&self) -> JoinHandle<()> {
        let runner = self.clone();
        let interval = Duration::from_secs(runner.config.watch_interval_secs.max(1));

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let killed = runner.check_and_kill_stuck_builds().await;
                if !killed.is_empty() {
                    warn!(count = killed.len(), "timeout watcher killed hung builds");
                }
            }
        })
    }

    // ── Status / Introspection ────────────────────────────────────────────

    /// Snapshot of all tracked builds keyed by build ID.
    pub async fn status(&self) -> HashMap<u64, TrackedBuild> {
        self.builds.read().await.clone()
    }

    /// Look up a single build by ID.
    pub async fn get_build(&self, build_id: u64) -> Option<TrackedBuild> {
        self.builds.read().await.get(&build_id).cloned()
    }

    /// Number of builds currently tracked.
    pub async fn build_count(&self) -> usize {
        self.builds.read().await.len()
    }
}

impl Default for BuildRunner {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Poll a build until its status is no longer `Running` (or give up).
    async fn wait_for_finish(runner: &BuildRunner, id: u64) -> TrackedBuild {
        for _ in 0..50 {
            let build = runner.get_build(id).await.expect("build should exist");
            if build.status != BuildStatus::Running {
                return build;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("build {id} did not finish in time");
    }

    #[tokio::test]
    async fn build_runner_starts_empty() {
        let runner = BuildRunner::new();
        assert_eq!(runner.build_count().await, 0);
        assert!(runner.status().await.is_empty());
    }

    #[tokio::test]
    async fn kill_nonexistent_build_returns_error() {
        let runner = BuildRunner::new();
        assert!(runner.kill_hung_build(42).await.is_err());
    }

    #[tokio::test]
    async fn check_and_kill_stuck_builds_empty_returns_empty() {
        let runner = BuildRunner::new();
        assert!(runner.check_and_kill_stuck_builds().await.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn completed_build_records_duration() {
        let runner = BuildRunner::new();
        let id = runner
            .start_build("sh", &["-c".into(), "exit 0".into()], None)
            .await
            .expect("spawn should succeed");

        let build = wait_for_finish(&runner, id).await;
        assert_eq!(build.status, BuildStatus::Completed);
        assert!(build.duration_secs.is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_build_records_exit_code() {
        let runner = BuildRunner::new();
        let id = runner
            .start_build("sh", &["-c".into(), "exit 3".into()], None)
            .await
            .expect("spawn should succeed");

        let build = wait_for_finish(&runner, id).await;
        assert_eq!(build.status, BuildStatus::Failed { exit_code: Some(3) });
        assert!(build.duration_secs.is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_watcher_kills_hung_build() {
        let runner = BuildRunner::with_config(BuildRunnerConfig {
            max_build_duration_secs: 0,
            watch_interval_secs: 1,
            kill_timeout_secs: 5,
        });

        let id = runner
            .start_build("sleep", &["30".into()], None)
            .await
            .expect("spawn should succeed");

        // Ensure some (non-zero) duration has elapsed past max_build_duration.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let killed = runner.check_and_kill_stuck_builds().await;
        assert_eq!(killed, vec![id]);

        let build = runner.get_build(id).await.expect("build should exist");
        assert_eq!(build.status, BuildStatus::TimedOut);
        assert!(build.duration_secs.is_some());
        assert!(!is_pid_alive(build.pid));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn check_and_kill_stuck_builds_ignores_builds_within_limit() {
        let runner = BuildRunner::new(); // default max is 1800s
        let id = runner
            .start_build("sleep", &["30".into()], None)
            .await
            .expect("spawn should succeed");

        assert!(runner.check_and_kill_stuck_builds().await.is_empty());
        let build = runner.get_build(id).await.expect("build should exist");
        assert_eq!(build.status, BuildStatus::Running);

        // Clean up the sleeping child.
        runner
            .kill_hung_build(id)
            .await
            .expect("kill should succeed");
    }
}
