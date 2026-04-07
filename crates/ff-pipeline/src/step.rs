//! Pipeline step definitions.
//!
//! Each step in a pipeline has an identity, a kind (what it does), configuration
//! (timeouts, retries), and produces a result.

use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ─── Step Identity ───────────────────────────────────────────────────────────

/// Unique identifier for a pipeline step.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StepId(pub String);

impl StepId {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }
}

impl fmt::Display for StepId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<&str> for StepId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for StepId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

// ─── Step Kind ───────────────────────────────────────────────────────────────

/// What a pipeline step actually does.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StepKind {
    /// Execute a shell command.
    Shell {
        command: String,
        /// Working directory (optional).
        cwd: Option<String>,
        /// Environment variables to set.
        #[serde(default)]
        env: Vec<(String, String)>,
    },
    /// Call a named Rust function (identified by string key).
    RustFn {
        /// Name used to look up the function in a registry.
        name: String,
        /// JSON-encoded arguments.
        args: Option<String>,
    },
    /// Send a prompt to an LLM.
    LlmPrompt {
        prompt: String,
        model: Option<String>,
        max_tokens: Option<u32>,
    },
    /// Make an HTTP call.
    HttpCall {
        method: String,
        url: String,
        headers: Option<Vec<(String, String)>>,
        body: Option<String>,
    },
    /// A no-op step (useful for synchronisation barriers).
    Noop,
}

// ─── Step Configuration ──────────────────────────────────────────────────────

/// Configuration knobs for a step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepConfig {
    /// How long to wait before considering the step timed-out.
    #[serde(with = "duration_secs")]
    pub timeout: Duration,
    /// Maximum number of retry attempts on failure (0 = no retries).
    pub retries: u32,
    /// Delay between retries.
    #[serde(with = "duration_secs")]
    pub retry_delay: Duration,
    /// If true, the pipeline continues even if this step fails.
    pub allow_failure: bool,
}

impl Default for StepConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(300),
            retries: 0,
            retry_delay: Duration::from_secs(5),
            allow_failure: false,
        }
    }
}

/// Serde helper: serialize/deserialize `Duration` as seconds (u64).
mod duration_secs {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(dur: &Duration, s: S) -> Result<S::Ok, S::Error> {
        dur.as_secs().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let secs = u64::deserialize(d)?;
        Ok(Duration::from_secs(secs))
    }
}

// ─── Step ────────────────────────────────────────────────────────────────────

/// A complete pipeline step: identity + what to do + how to do it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub id: StepId,
    pub name: String,
    pub kind: StepKind,
    pub config: StepConfig,
}

impl Step {
    /// Create a new step with default config.
    pub fn new(id: impl Into<StepId>, name: impl Into<String>, kind: StepKind) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            kind,
            config: StepConfig::default(),
        }
    }

    /// Builder: set timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.config.timeout = timeout;
        self
    }

    /// Builder: set retries.
    pub fn with_retries(mut self, retries: u32, delay: Duration) -> Self {
        self.config.retries = retries;
        self.config.retry_delay = delay;
        self
    }

    /// Builder: allow failure.
    pub fn allow_failure(mut self) -> Self {
        self.config.allow_failure = true;
        self
    }

    /// Create a shell step.
    pub fn shell(
        id: impl Into<StepId>,
        name: impl Into<String>,
        command: impl Into<String>,
    ) -> Self {
        Self::new(
            id,
            name,
            StepKind::Shell {
                command: command.into(),
                cwd: None,
                env: Vec::new(),
            },
        )
    }

    /// Create a noop/barrier step.
    pub fn noop(id: impl Into<StepId>, name: impl Into<String>) -> Self {
        Self::new(id, name, StepKind::Noop)
    }
}

// ─── Step Status / Result ────────────────────────────────────────────────────

/// Current status of a step in the executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// Waiting for dependencies.
    Pending,
    /// Currently running.
    Running,
    /// Completed successfully.
    Succeeded,
    /// Failed after all retries.
    Failed,
    /// Skipped because a dependency failed.
    Skipped,
    /// Timed out.
    TimedOut,
}

impl StepStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            StepStatus::Succeeded | StepStatus::Failed | StepStatus::Skipped | StepStatus::TimedOut
        )
    }

    pub fn is_success(self) -> bool {
        self == StepStatus::Succeeded
    }
}

/// The result of executing a single step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    pub step_id: StepId,
    pub status: StepStatus,
    pub output: String,
    pub error: Option<String>,
    pub attempts: u32,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<u64>,
}

impl StepResult {
    /// Create a successful result.
    pub fn success(step_id: StepId, output: String, attempts: u32, duration_ms: u64) -> Self {
        Self {
            step_id,
            status: StepStatus::Succeeded,
            output,
            error: None,
            attempts,
            started_at: None,
            completed_at: Some(Utc::now()),
            duration_ms: Some(duration_ms),
        }
    }

    /// Create a failure result.
    pub fn failure(
        step_id: StepId,
        error: String,
        output: String,
        attempts: u32,
        duration_ms: u64,
    ) -> Self {
        Self {
            step_id,
            status: StepStatus::Failed,
            output,
            error: Some(error),
            attempts,
            started_at: None,
            completed_at: Some(Utc::now()),
            duration_ms: Some(duration_ms),
        }
    }

    /// Create a skipped result.
    pub fn skipped(step_id: StepId, reason: String) -> Self {
        Self {
            step_id,
            status: StepStatus::Skipped,
            output: String::new(),
            error: Some(reason),
            attempts: 0,
            started_at: None,
            completed_at: Some(Utc::now()),
            duration_ms: None,
        }
    }

    /// Create a timed-out result.
    pub fn timed_out(step_id: StepId, attempts: u32, duration_ms: u64) -> Self {
        Self {
            step_id,
            status: StepStatus::TimedOut,
            output: String::new(),
            error: Some("step timed out".to_string()),
            attempts,
            started_at: None,
            completed_at: Some(Utc::now()),
            duration_ms: Some(duration_ms),
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_id_equality() {
        let a = StepId::new("build");
        let b: StepId = "build".into();
        assert_eq!(a, b);
    }

    #[test]
    fn step_id_display() {
        let id = StepId::new("cargo-test");
        assert_eq!(id.to_string(), "cargo-test");
    }

    #[test]
    fn default_config() {
        let cfg = StepConfig::default();
        assert_eq!(cfg.timeout, Duration::from_secs(300));
        assert_eq!(cfg.retries, 0);
        assert!(!cfg.allow_failure);
    }

    #[test]
    fn step_builder() {
        let step = Step::shell("s1", "Build", "cargo build")
            .with_timeout(Duration::from_secs(60))
            .with_retries(3, Duration::from_secs(10))
            .allow_failure();

        assert_eq!(step.id, StepId::new("s1"));
        assert_eq!(step.config.timeout, Duration::from_secs(60));
        assert_eq!(step.config.retries, 3);
        assert_eq!(step.config.retry_delay, Duration::from_secs(10));
        assert!(step.config.allow_failure);
    }

    #[test]
    fn step_status_terminal() {
        assert!(!StepStatus::Pending.is_terminal());
        assert!(!StepStatus::Running.is_terminal());
        assert!(StepStatus::Succeeded.is_terminal());
        assert!(StepStatus::Failed.is_terminal());
        assert!(StepStatus::Skipped.is_terminal());
        assert!(StepStatus::TimedOut.is_terminal());
    }

    #[test]
    fn step_result_constructors() {
        let ok = StepResult::success(StepId::new("x"), "done".into(), 1, 500);
        assert!(ok.status.is_success());
        assert_eq!(ok.attempts, 1);

        let fail = StepResult::failure(StepId::new("y"), "boom".into(), "".into(), 3, 1000);
        assert_eq!(fail.status, StepStatus::Failed);
        assert_eq!(fail.error.as_deref(), Some("boom"));

        let skip = StepResult::skipped(StepId::new("z"), "dep failed".into());
        assert_eq!(skip.status, StepStatus::Skipped);

        let timeout = StepResult::timed_out(StepId::new("w"), 2, 30000);
        assert_eq!(timeout.status, StepStatus::TimedOut);
    }

    #[test]
    fn step_serialize_roundtrip() {
        let step = Step::shell("test", "Run tests", "cargo test");
        let json = serde_json::to_string(&step).unwrap();
        let back: Step = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, step.id);
    }
}
