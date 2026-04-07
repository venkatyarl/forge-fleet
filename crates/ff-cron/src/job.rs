use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::policy::{BackoffPolicy, JobPriority};
use crate::schedule::{CronSchedule, ScheduleError};

/// Executable payload attached to a scheduled job.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobTask {
    /// Execute a command on the local node.
    LocalCommand {
        command: String,
        timeout_secs: Option<u64>,
    },
    /// Dispatch a task to a remote ff-mesh worker.
    FleetTask {
        kind: String,
        payload: serde_json::Value,
        worker_hint: Option<String>,
    },
}

/// Non-functional metadata for audit/search/UI.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct JobMetadata {
    pub description: Option<String>,
    pub created_by: Option<String>,
    pub tags: Vec<String>,
    pub labels: HashMap<String, String>,
}

/// Ownership and leasing information for distributed schedulers.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct JobOwnership {
    pub owner_node: Option<String>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub claimed_by_run: Option<Uuid>,
}

/// Retry behavior for failed job runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff: BackoffPolicy,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            backoff: BackoffPolicy::default(),
        }
    }
}

/// Static job definition and mutable scheduling state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobDefinition {
    pub id: Uuid,
    pub name: String,
    pub schedule_expression: String,
    pub enabled: bool,
    pub priority: JobPriority,
    pub task: JobTask,
    pub metadata: JobMetadata,
    pub retry: RetryPolicy,
    pub ownership: JobOwnership,

    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub next_run_at: Option<DateTime<Utc>>,

    /// Current streak of consecutive failed runs.
    pub consecutive_failures: u32,
}

impl JobDefinition {
    pub fn new(
        name: impl Into<String>,
        schedule_expression: impl Into<String>,
        task: JobTask,
        priority: JobPriority,
    ) -> Result<Self, ScheduleError> {
        let schedule_expression = schedule_expression.into();
        let schedule = CronSchedule::parse(&schedule_expression)?;
        let now = Utc::now();

        Ok(Self {
            id: Uuid::new_v4(),
            name: name.into(),
            schedule_expression,
            enabled: true,
            priority,
            task,
            metadata: JobMetadata::default(),
            retry: RetryPolicy::default(),
            ownership: JobOwnership::default(),
            created_at: now,
            updated_at: now,
            last_run_at: None,
            next_run_at: schedule.next_after(now),
            consecutive_failures: 0,
        })
    }

    pub fn validate_schedule(&self) -> Result<CronSchedule, ScheduleError> {
        CronSchedule::parse(&self.schedule_expression)
    }

    pub fn recompute_next_run(&mut self, from: DateTime<Utc>) -> Result<(), ScheduleError> {
        let schedule = self.validate_schedule()?;
        self.next_run_at = schedule.next_after(from);
        self.updated_at = Utc::now();
        Ok(())
    }

    pub fn should_run_at(&self, now: DateTime<Utc>) -> bool {
        self.enabled && self.next_run_at.map(|ts| ts <= now).unwrap_or(false)
    }

    pub fn note_success(&mut self, finished_at: DateTime<Utc>) -> Result<(), ScheduleError> {
        self.last_run_at = Some(finished_at);
        self.consecutive_failures = 0;
        self.recompute_next_run(finished_at)
    }

    pub fn note_failure_with_retry(
        &mut self,
        failed_at: DateTime<Utc>,
        next_retry_at: DateTime<Utc>,
    ) {
        self.last_run_at = Some(failed_at);
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.next_run_at = Some(next_retry_at);
        self.updated_at = Utc::now();
    }

    pub fn can_retry(&self) -> bool {
        self.consecutive_failures < self.retry.max_attempts
    }
}

/// Lifecycle state of one job execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Pending,
    Running,
    Dispatched,
    Succeeded,
    Failed,
    Skipped,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Dispatched => "dispatched",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }

    pub fn parse_str(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "dispatched" => Some(Self::Dispatched),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "skipped" => Some(Self::Skipped),
            _ => None,
        }
    }
}

/// A concrete run attempt for a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRun {
    pub id: Uuid,
    pub job_id: Uuid,
    pub status: RunStatus,
    pub scheduled_for: DateTime<Utc>,
    pub attempt: u32,
    pub worker: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub output: Option<String>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl JobRun {
    pub fn pending(job_id: Uuid, scheduled_for: DateTime<Utc>, attempt: u32) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            job_id,
            status: RunStatus::Pending,
            scheduled_for,
            attempt,
            worker: None,
            started_at: None,
            finished_at: None,
            output: None,
            error: None,
            created_at: now,
        }
    }

    pub fn mark_running(&mut self, worker: Option<String>) {
        self.status = RunStatus::Running;
        self.worker = worker;
        self.started_at = Some(Utc::now());
    }

    pub fn mark_dispatched(&mut self, worker: Option<String>, details: String) {
        self.status = RunStatus::Dispatched;
        self.worker = worker;
        if self.started_at.is_none() {
            self.started_at = Some(Utc::now());
        }
        self.output = Some(details);
    }

    pub fn mark_success(&mut self, output: String) {
        self.status = RunStatus::Succeeded;
        self.output = Some(output);
        if self.started_at.is_none() {
            self.started_at = Some(Utc::now());
        }
        self.finished_at = Some(Utc::now());
    }

    pub fn mark_failure(&mut self, message: String) {
        self.status = RunStatus::Failed;
        self.error = Some(message);
        if self.started_at.is_none() {
            self.started_at = Some(Utc::now());
        }
        self.finished_at = Some(Utc::now());
    }

    pub fn mark_skipped(&mut self, reason: String) {
        self.status = RunStatus::Skipped;
        self.output = Some(reason);
        self.finished_at = Some(Utc::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_creation_sets_next_run() {
        let job = JobDefinition::new(
            "heartbeat",
            "*/5 * * * *",
            JobTask::LocalCommand {
                command: "echo ok".into(),
                timeout_secs: None,
            },
            JobPriority::Normal,
        )
        .unwrap();

        assert!(job.next_run_at.is_some());
    }

    #[test]
    fn run_status_roundtrip() {
        for value in [
            RunStatus::Pending,
            RunStatus::Running,
            RunStatus::Dispatched,
            RunStatus::Succeeded,
            RunStatus::Failed,
            RunStatus::Skipped,
        ] {
            let parsed = RunStatus::parse_str(value.as_str()).unwrap();
            assert_eq!(parsed, value);
        }
    }
}
