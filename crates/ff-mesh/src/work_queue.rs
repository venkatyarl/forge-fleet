//! Distributed work queue — submit tasks, claim tasks,
//! track completion, retry on failure.

use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use uuid::Uuid;

use ff_core::task::AgentTask;

// ─── Task Entry ──────────────────────────────────────────────────────────────

/// State of a task in the work queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// Waiting to be claimed by a worker.
    Pending,
    /// Claimed by a worker — execution in progress.
    Running,
    /// Completed successfully.
    Completed,
    /// Failed and exhausted retries.
    Failed,
    /// Failed but eligible for retry.
    RetryPending,
    /// Cancelled by user or leader.
    Cancelled,
}

/// Priority levels for tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum TaskPriority {
    /// Background / batch tasks — lowest priority.
    Low = 0,
    /// Normal tasks — default.
    #[default]
    Normal = 1,
    /// Urgent tasks — preempt normal.
    High = 2,
    /// Critical tasks — system health, failover.
    Critical = 3,
}

/// An entry in the work queue wrapping a task with metadata.
#[derive(Debug, Clone)]
pub struct QueueEntry {
    /// The task itself.
    pub task: AgentTask,
    /// Priority.
    pub priority: TaskPriority,
    /// Current state.
    pub state: TaskState,
    /// When the task was submitted.
    pub submitted_at: DateTime<Utc>,
    /// When the task was claimed (if running).
    pub claimed_at: Option<DateTime<Utc>>,
    /// Who claimed it (worker node ID).
    pub claimed_by: Option<Uuid>,
    /// When the task completed or failed.
    pub finished_at: Option<DateTime<Utc>>,
    /// Number of attempts so far.
    pub attempts: u32,
    /// Maximum retry attempts.
    pub max_retries: u32,
}

/// Queue statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueStats {
    /// Total tasks ever submitted.
    pub total_submitted: u64,
    /// Currently pending.
    pub pending: usize,
    /// Currently running.
    pub running: usize,
    /// Completed successfully.
    pub total_completed: u64,
    /// Failed permanently.
    pub total_failed: u64,
    /// Waiting for retry.
    pub retry_pending: usize,
    /// Cancelled.
    pub total_cancelled: u64,
}

// ─── Work Queue ──────────────────────────────────────────────────────────────

/// A distributed work queue with priority, retry, and claim semantics.
///
/// Thread-safe via interior mutability (Mutex). In a multi-node deployment,
/// this would be backed by Postgres — but the in-memory version provides
/// the same API for single-leader operation.
pub struct WorkQueue {
    /// All entries, ordered by submission time.
    entries: Mutex<Vec<QueueEntry>>,
    /// Counter for total submissions.
    total_submitted: Mutex<u64>,
    /// Counter for completed.
    total_completed: Mutex<u64>,
    /// Counter for failed.
    total_failed: Mutex<u64>,
    /// Counter for cancelled.
    total_cancelled: Mutex<u64>,
}

impl WorkQueue {
    /// Create a new empty work queue.
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            total_submitted: Mutex::new(0),
            total_completed: Mutex::new(0),
            total_failed: Mutex::new(0),
            total_cancelled: Mutex::new(0),
        }
    }

    /// Submit a task to the queue with default priority.
    pub fn submit(&self, task: AgentTask) -> Uuid {
        self.submit_with_priority(task, TaskPriority::Normal)
    }

    /// Submit a task with a specific priority.
    pub fn submit_with_priority(&self, task: AgentTask, priority: TaskPriority) -> Uuid {
        let task_id = task.id;
        let entry = QueueEntry {
            task,
            priority,
            state: TaskState::Pending,
            submitted_at: Utc::now(),
            claimed_at: None,
            claimed_by: None,
            finished_at: None,
            attempts: 0,
            max_retries: 3,
        };

        {
            let mut entries = self.entries.lock().unwrap();
            entries.push(entry);
        }

        {
            let mut count = self.total_submitted.lock().unwrap();
            *count += 1;
        }

        info!(task_id = %task_id, "task submitted to work queue");
        task_id
    }

    /// Peek at the highest-priority pending task without claiming it.
    pub fn peek_pending(&self) -> Option<QueueEntry> {
        let entries = self.entries.lock().unwrap();

        // Find highest-priority pending or retry-pending task.
        entries
            .iter()
            .filter(|e| e.state == TaskState::Pending || e.state == TaskState::RetryPending)
            .max_by_key(|e| e.priority)
            .cloned()
    }

    /// Claim a specific task for a worker. Returns the entry if successful.
    pub fn claim(&self, task_id: Uuid, worker_id: Uuid) -> Option<QueueEntry> {
        let mut entries = self.entries.lock().unwrap();

        if let Some(entry) = entries.iter_mut().find(|e| {
            e.task.id == task_id
                && (e.state == TaskState::Pending || e.state == TaskState::RetryPending)
        }) {
            entry.state = TaskState::Running;
            entry.claimed_at = Some(Utc::now());
            entry.claimed_by = Some(worker_id);
            entry.attempts += 1;

            debug!(
                task_id = %task_id,
                worker_id = %worker_id,
                attempt = entry.attempts,
                "task claimed by worker"
            );

            Some(entry.clone())
        } else {
            None
        }
    }

    /// Claim the next available task (highest-priority first).
    pub fn claim_next(&self, worker_id: Uuid) -> Option<QueueEntry> {
        let mut entries = self.entries.lock().unwrap();

        // Find highest-priority pending task.
        let idx = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.state == TaskState::Pending || e.state == TaskState::RetryPending)
            .max_by_key(|(_, e)| e.priority)
            .map(|(i, _)| i);

        if let Some(idx) = idx {
            let entry = &mut entries[idx];
            entry.state = TaskState::Running;
            entry.claimed_at = Some(Utc::now());
            entry.claimed_by = Some(worker_id);
            entry.attempts += 1;

            debug!(
                task_id = %entry.task.id,
                worker_id = %worker_id,
                attempt = entry.attempts,
                "task claimed (next available)"
            );

            Some(entry.clone())
        } else {
            None
        }
    }

    /// Mark a task as completed.
    pub fn complete(&self, task_id: Uuid, success: bool) {
        let mut entries = self.entries.lock().unwrap();

        if let Some(entry) = entries.iter_mut().find(|e| e.task.id == task_id) {
            entry.finished_at = Some(Utc::now());

            if success {
                entry.state = TaskState::Completed;
                let mut count = self.total_completed.lock().unwrap();
                *count += 1;
                info!(task_id = %task_id, "task completed successfully");
            } else if entry.attempts < entry.max_retries {
                // Failed but can retry.
                entry.state = TaskState::RetryPending;
                entry.claimed_at = None;
                entry.claimed_by = None;
                warn!(
                    task_id = %task_id,
                    attempts = entry.attempts,
                    max_retries = entry.max_retries,
                    "task failed — queued for retry"
                );
            } else {
                // Exhausted retries.
                entry.state = TaskState::Failed;
                let mut count = self.total_failed.lock().unwrap();
                *count += 1;
                warn!(
                    task_id = %task_id,
                    attempts = entry.attempts,
                    "task permanently failed — retries exhausted"
                );
            }
        }
    }

    /// Cancel a task. Returns `true` if the task was found and cancelled.
    pub fn cancel(&self, task_id: Uuid) -> bool {
        let mut entries = self.entries.lock().unwrap();

        if let Some(entry) = entries.iter_mut().find(|e| {
            e.task.id == task_id
                && (e.state == TaskState::Pending || e.state == TaskState::RetryPending)
        }) {
            entry.state = TaskState::Cancelled;
            entry.finished_at = Some(Utc::now());
            let mut count = self.total_cancelled.lock().unwrap();
            *count += 1;
            info!(task_id = %task_id, "task cancelled");
            true
        } else {
            false
        }
    }

    /// Get the current state of a task.
    pub fn get_task(&self, task_id: &Uuid) -> Option<QueueEntry> {
        let entries = self.entries.lock().unwrap();
        entries.iter().find(|e| e.task.id == *task_id).cloned()
    }

    /// Get all entries matching a state.
    pub fn get_by_state(&self, state: TaskState) -> Vec<QueueEntry> {
        let entries = self.entries.lock().unwrap();
        entries
            .iter()
            .filter(|e| e.state == state)
            .cloned()
            .collect()
    }

    /// Get queue statistics.
    pub fn stats(&self) -> QueueStats {
        let entries = self.entries.lock().unwrap();
        let total_submitted = *self.total_submitted.lock().unwrap();
        let total_completed = *self.total_completed.lock().unwrap();
        let total_failed = *self.total_failed.lock().unwrap();
        let total_cancelled = *self.total_cancelled.lock().unwrap();

        let pending = entries
            .iter()
            .filter(|e| e.state == TaskState::Pending)
            .count();
        let running = entries
            .iter()
            .filter(|e| e.state == TaskState::Running)
            .count();
        let retry_pending = entries
            .iter()
            .filter(|e| e.state == TaskState::RetryPending)
            .count();

        QueueStats {
            total_submitted,
            pending,
            running,
            total_completed,
            total_failed,
            retry_pending,
            total_cancelled,
        }
    }

    /// Clean up finished entries older than a threshold.
    /// Returns the number of entries removed.
    pub fn cleanup(&self, max_age_secs: i64) -> usize {
        let mut entries = self.entries.lock().unwrap();
        let now = Utc::now();
        let before_len = entries.len();

        entries.retain(|e| {
            if let Some(finished) = e.finished_at {
                let age = now.signed_duration_since(finished).num_seconds();
                age < max_age_secs
            } else {
                true // Keep unfinished tasks.
            }
        });

        let removed = before_len - entries.len();
        if removed > 0 {
            debug!(removed, "cleaned up old queue entries");
        }
        removed
    }

    /// Total number of entries in the queue (all states).
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.lock().unwrap().is_empty()
    }
}

impl Default for WorkQueue {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ff_core::task::AgentTaskKind;

    fn make_task() -> AgentTask {
        AgentTask {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            kind: AgentTaskKind::ShellCommand {
                command: "echo test".into(),
                timeout_secs: None,
            },
        }
    }

    #[test]
    fn test_submit_and_peek() {
        let queue = WorkQueue::new();
        let task = make_task();
        let task_id = task.id;

        queue.submit(task);

        let pending = queue.peek_pending();
        assert!(pending.is_some());
        assert_eq!(pending.unwrap().task.id, task_id);
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn test_claim_task() {
        let queue = WorkQueue::new();
        let task = make_task();
        let task_id = task.id;
        let worker_id = Uuid::new_v4();

        queue.submit(task);

        let claimed = queue.claim(task_id, worker_id);
        assert!(claimed.is_some());
        let claimed = claimed.unwrap();
        assert_eq!(claimed.state, TaskState::Running);
        assert_eq!(claimed.claimed_by, Some(worker_id));
        assert_eq!(claimed.attempts, 1);

        // Can't claim again.
        assert!(queue.claim(task_id, worker_id).is_none());
    }

    #[test]
    fn test_claim_next() {
        let queue = WorkQueue::new();
        let worker_id = Uuid::new_v4();

        queue.submit(make_task());
        queue.submit(make_task());

        let first = queue.claim_next(worker_id);
        assert!(first.is_some());

        let second = queue.claim_next(worker_id);
        assert!(second.is_some());

        // No more pending.
        assert!(queue.claim_next(worker_id).is_none());
    }

    #[test]
    fn test_complete_success() {
        let queue = WorkQueue::new();
        let task = make_task();
        let task_id = task.id;
        let worker_id = Uuid::new_v4();

        queue.submit(task);
        queue.claim(task_id, worker_id);
        queue.complete(task_id, true);

        let entry = queue.get_task(&task_id).unwrap();
        assert_eq!(entry.state, TaskState::Completed);
        assert!(entry.finished_at.is_some());

        let stats = queue.stats();
        assert_eq!(stats.total_completed, 1);
    }

    #[test]
    fn test_retry_on_failure() {
        let queue = WorkQueue::new();
        let task = make_task();
        let task_id = task.id;
        let worker_id = Uuid::new_v4();

        queue.submit(task);

        // First attempt fails.
        queue.claim(task_id, worker_id);
        queue.complete(task_id, false);

        let entry = queue.get_task(&task_id).unwrap();
        assert_eq!(entry.state, TaskState::RetryPending);
        assert_eq!(entry.attempts, 1);

        // Second attempt fails.
        queue.claim(task_id, worker_id);
        queue.complete(task_id, false);

        let entry = queue.get_task(&task_id).unwrap();
        assert_eq!(entry.state, TaskState::RetryPending);
        assert_eq!(entry.attempts, 2);

        // Third attempt fails — now permanently failed (max_retries = 3).
        queue.claim(task_id, worker_id);
        queue.complete(task_id, false);

        let entry = queue.get_task(&task_id).unwrap();
        assert_eq!(entry.state, TaskState::Failed);
        assert_eq!(entry.attempts, 3);

        let stats = queue.stats();
        assert_eq!(stats.total_failed, 1);
    }

    #[test]
    fn test_cancel_task() {
        let queue = WorkQueue::new();
        let task = make_task();
        let task_id = task.id;

        queue.submit(task);
        assert!(queue.cancel(task_id));

        let entry = queue.get_task(&task_id).unwrap();
        assert_eq!(entry.state, TaskState::Cancelled);

        let stats = queue.stats();
        assert_eq!(stats.total_cancelled, 1);
    }

    #[test]
    fn test_priority_ordering() {
        let queue = WorkQueue::new();

        let low_task = make_task();
        let high_task = make_task();
        let high_id = high_task.id;

        queue.submit_with_priority(low_task, TaskPriority::Low);
        queue.submit_with_priority(high_task, TaskPriority::High);

        let peeked = queue.peek_pending().unwrap();
        assert_eq!(peeked.task.id, high_id); // High priority first.
    }

    #[test]
    fn test_get_by_state() {
        let queue = WorkQueue::new();
        let worker_id = Uuid::new_v4();

        let t1 = make_task();
        let t2 = make_task();
        let t2_id = t2.id;

        queue.submit(t1);
        queue.submit(t2);

        queue.claim(t2_id, worker_id);

        let pending = queue.get_by_state(TaskState::Pending);
        assert_eq!(pending.len(), 1);

        let running = queue.get_by_state(TaskState::Running);
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].task.id, t2_id);
    }

    #[test]
    fn test_stats() {
        let queue = WorkQueue::new();
        let worker_id = Uuid::new_v4();

        let t1 = make_task();
        let t2 = make_task();
        let t1_id = t1.id;

        queue.submit(t1);
        queue.submit(t2);

        queue.claim(t1_id, worker_id);
        queue.complete(t1_id, true);

        let stats = queue.stats();
        assert_eq!(stats.total_submitted, 2);
        assert_eq!(stats.pending, 1);
        assert_eq!(stats.running, 0);
        assert_eq!(stats.total_completed, 1);
    }

    #[test]
    fn test_cleanup() {
        let queue = WorkQueue::new();
        let task = make_task();
        let task_id = task.id;
        let worker_id = Uuid::new_v4();

        queue.submit(task);
        queue.claim(task_id, worker_id);
        queue.complete(task_id, true);

        // Should not clean up entries that are < max_age_secs old.
        let removed = queue.cleanup(3600);
        assert_eq!(removed, 0);
        assert_eq!(queue.len(), 1);
    }
}
