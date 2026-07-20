//! Wrapper around the local 480B codegen model endpoint.
//!
//! Limits concurrent calls to the local endpoint to 2 using a tokio
//! semaphore, matching the `--parallel 2` flag passed to the underlying
//! codegen process.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::time;
use tracing::{info, warn};

use crate::errors::{ControlError, Result};

/// Number of concurrent 480B codegen requests allowed by default.
const DEFAULT_PARALLELISM: usize = 2;

/// Default timeout for a single 480B codegen call.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// Result of a single 480B codegen invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodegenResult {
    /// Standard output captured from the codegen process.
    pub stdout: String,
    /// Standard error captured from the codegen process.
    pub stderr: String,
    /// Exit code of the codegen process, or `-1` if no code was available.
    pub exit_code: i32,
}

/// Wrapper around the local 480B model endpoint.
///
/// The wrapper owns a [`Semaphore`] that caps concurrent invocations and
/// always passes `--parallel 2` to the underlying binary.
#[derive(Debug)]
pub struct Llm480bWrapper {
    binary: String,
    semaphore: Semaphore,
    timeout: Duration,
}

impl Llm480bWrapper {
    /// Create a new wrapper using `binary` as the local 480B codegen CLI.
    ///
    /// Defaults to a concurrency limit of 2.
    pub fn new(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
            semaphore: Semaphore::new(DEFAULT_PARALLELISM),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Set a custom concurrency limit.
    ///
    /// Values below 1 are clamped to 1.
    pub fn with_parallel(mut self, parallel: usize) -> Self {
        self.semaphore = Semaphore::new(parallel.max(1));
        self
    }

    /// Set a custom per-invocation timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Invoke the 480B codegen CLI for `task` inside `repo`.
    ///
    /// A semaphore permit is acquired before spawning the process and is
    /// released automatically when the returned future completes.
    pub async fn generate(&self, task: &str, repo: &Path) -> Result<CodegenResult> {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|e| ControlError::Llm480b(format!("semaphore acquire failed: {e}")))?;

        info!(
            binary = %self.binary,
            task,
            repo = %repo.display(),
            "480B codegen invocation start"
        );

        let output = time::timeout(
            self.timeout,
            Command::new(&self.binary)
                .arg("--parallel")
                .arg("2")
                .arg("--task")
                .arg(task)
                .arg("--repo")
                .arg(repo)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| ControlError::Llm480b("480B codegen timed out".to_string()))?
        .map_err(|e| ControlError::Llm480b(format!("failed to spawn 480B codegen: {e}")))?;

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let exit_code = output.status.code().unwrap_or(-1);

        if output.status.success() {
            info!(exit_code, "480B codegen invocation succeeded");
        } else {
            warn!(exit_code, stderr = %stderr, "480B codegen invocation failed");
        }

        Ok(CodegenResult {
            stdout,
            stderr,
            exit_code,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn default_parallelism_allows_two_concurrent_slots() {
        let wrapper = Llm480bWrapper::new("dummy");
        let _p1 = wrapper.semaphore.try_acquire().expect("first permit");
        let _p2 = wrapper.semaphore.try_acquire().expect("second permit");
        assert!(
            wrapper.semaphore.try_acquire().is_err(),
            "third permit should be denied with default parallelism of 2"
        );
    }

    #[tokio::test]
    async fn with_parallel_changes_capacity() {
        let wrapper = Llm480bWrapper::new("dummy").with_parallel(4);
        let mut permits = Vec::new();
        for _ in 0..4 {
            permits.push(wrapper.semaphore.try_acquire().expect("acquire permit"));
        }
        assert!(
            wrapper.semaphore.try_acquire().is_err(),
            "fifth permit should be denied with parallelism of 4"
        );
    }

    #[tokio::test]
    async fn with_parallel_clamps_to_one() {
        let wrapper = Llm480bWrapper::new("dummy").with_parallel(0);
        let _p1 = wrapper.semaphore.try_acquire().expect("single permit");
        assert!(wrapper.semaphore.try_acquire().is_err());
    }

    #[tokio::test]
    async fn generate_passes_parallel_two_and_task_and_repo() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake_codegen.sh");
        {
            let mut f = std::fs::File::create(&script).unwrap();
            writeln!(f, "#!/bin/sh\nprintf '%s ' \"$@\"\necho",).unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }

        let wrapper = Llm480bWrapper::new(script.to_string_lossy().as_ref())
            .with_timeout(Duration::from_secs(5));
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();

        let result = wrapper.generate("do work", &repo).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(
            result.stdout.contains("--parallel 2"),
            "stdout: {:?}",
            result.stdout
        );
        assert!(
            result.stdout.contains("--task do work"),
            "stdout: {:?}",
            result.stdout
        );
        assert!(
            result
                .stdout
                .contains(&format!("--repo {}", repo.display())),
            "stdout: {:?}",
            result.stdout
        );
    }
}
