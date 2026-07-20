//! Priority task queue for ForgeFleet scheduling.
//!
//! Tasks are queued by priority (Critical → Background) and dequeued
//! highest-priority-first. Supports task reservation (mark as assigned
//! without removing), timeout-based priority boosting, and bulk drain.
//!
//! The queue uses interior mutability so a single instance can be shared
//! across threads and await-style callers without external locking. All
//! edits are serialized through a single mutex, preventing races when
//! multiple workers update the same build slot concurrently.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};
use uuid::Uuid;

use crate::scheduler::{ResourceRequirements, TaskPriority};

// ─── Queued Task ─────────────────────────────────────────────────────────────

/// A task sitting in the priority queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedTask {
    /// Unique task ID.
    pub id: Uuid,
    /// Human-readable description.
    pub description: String,
    /// Project the task belongs to (for fairness grouping).
    pub project: Option<String>,
    /// Resource requirements.
    pub requirements: ResourceRequirements,
    /// Original priority when submitted.
    pub original_priority: TaskPriority,
    /// Current effective priority (may be boosted by timeout).
    pub effective_priority: TaskPriority,
    /// When the task was enqueued.
    pub enqueued_at: DateTime<Utc>,
    /// Workload type hint (e.g. "coding", "review").
    pub workload_type: Option<String>,
    /// Whether this task is reserved (assigned but not yet removed).
    pub reserved: bool,
    /// Node this task is reserved on, if any.
    pub reserved_node: Option<String>,
}

impl QueuedTask {
    /// Create a new queued task.
    pub fn new(
        description: impl Into<String>,
        requirements: ResourceRequirements,
        priority: TaskPriority,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            description: description.into(),
            project: None,
            requirements,
            original_priority: priority,
            effective_priority: priority,
            enqueued_at: Utc::now(),
            workload_type: None,
            reserved: false,
            reserved_node: None,
        }
    }

    /// Builder: set project.
    pub fn with_project(mut self, project: impl Into<String>) -> Self {
        self.project = Some(project.into());
        self
    }

    /// Builder: set workload type.
    pub fn with_workload_type(mut self, wt: impl Into<String>) -> Self {
        self.workload_type = Some(wt.into());
        self
    }

    /// Duration the task has been waiting.
    pub fn wait_duration(&self) -> chrono::TimeDelta {
        Utc::now().signed_duration_since(self.enqueued_at)
    }
}

// ─── Priority Queue ──────────────────────────────────────────────────────────

/// Mutable state guarded by the queue mutex.
#[derive(Debug)]
struct PriorityQueueState {
    /// Priority → FIFO queue of tasks at that level.
    buckets: BTreeMap<TaskPriority, VecDeque<QueuedTask>>,
    /// Task ID → priority lookup for O(1) finding.
    index: HashMap<Uuid, TaskPriority>,
}

/// Priority-based task queue.
///
/// Backed by a `BTreeMap<TaskPriority, VecDeque<QueuedTask>>` so that
/// iteration naturally yields highest-priority tasks first (Critical has
/// the smallest discriminant and comes first in BTreeMap order).
///
/// All mutating operations acquire an internal mutex, so a single
/// [`PriorityQueue`] can safely be shared between threads and concurrent
/// await-style callers. This serializes edits within the same build slot
/// and prevents races on the queue buckets and index.
#[derive(Debug)]
pub struct PriorityQueue {
    state: Mutex<PriorityQueueState>,
    /// How long a task can wait before its priority is boosted.
    boost_timeout: Duration,
}

impl PriorityQueue {
    /// Create a new empty priority queue.
    ///
    /// `boost_timeout` is the duration after which a waiting task gets
    /// promoted one priority level (e.g. Low → Normal).
    pub fn new(boost_timeout: Duration) -> Self {
        Self {
            state: Mutex::new(PriorityQueueState {
                buckets: BTreeMap::new(),
                index: HashMap::new(),
            }),
            boost_timeout,
        }
    }

    /// Create a queue with a default 10-minute boost timeout.
    pub fn with_default_timeout() -> Self {
        Self::new(Duration::from_secs(600))
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, PriorityQueueState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Enqueue a task at the given priority.
    pub fn enqueue(&self, task: QueuedTask, priority: TaskPriority) {
        let id = task.id;
        let mut state = self.lock_state();
        state.buckets.entry(priority).or_default().push_back(task);
        state.index.insert(id, priority);

        debug!(task_id = %id, priority = %priority, "task enqueued");
    }

    /// Dequeue the highest-priority unreserved task.
    ///
    /// Removes and returns the first unreserved task from the highest
    /// priority bucket. Returns `None` if the queue is empty or all
    /// tasks are reserved.
    pub fn dequeue(&self) -> Option<QueuedTask> {
        let mut state = self.lock_state();
        // Iterate priorities highest first (BTreeMap is ascending, Critical=0 is first)
        let priorities: Vec<TaskPriority> = state.buckets.keys().copied().collect();

        for priority in priorities {
            let removed = if let Some(deque) = state.buckets.get_mut(&priority) {
                // Find first unreserved task
                deque.iter().position(|t| !t.reserved).map(|pos| {
                    let task = deque.remove(pos).unwrap();
                    let is_empty = deque.is_empty();
                    (task, is_empty)
                })
            } else {
                None
            };

            if let Some((task, is_empty)) = removed {
                let task_id = task.id;

                // Clean up empty bucket
                if is_empty {
                    state.buckets.remove(&priority);
                }
                state.index.remove(&task_id);

                debug!(task_id = %task_id, priority = %priority, "task dequeued");
                return Some(task);
            }
        }

        None
    }

    /// Peek at the highest-priority unreserved task without removing it.
    pub fn peek(&self) -> Option<QueuedTask> {
        let state = self.lock_state();
        for deque in state.buckets.values() {
            if let Some(task) = deque.iter().find(|t| !t.reserved) {
                return Some(task.clone());
            }
        }
        None
    }

    /// Total number of tasks in the queue (including reserved).
    pub fn len(&self) -> usize {
        let state = self.lock_state();
        state.buckets.values().map(|d| d.len()).sum()
    }

    /// Number of unreserved tasks.
    pub fn unreserved_count(&self) -> usize {
        let state = self.lock_state();
        state
            .buckets
            .values()
            .flat_map(|d| d.iter())
            .filter(|t| !t.reserved)
            .count()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        let state = self.lock_state();
        state.buckets.is_empty()
    }

    /// Drain all tasks at a specific priority level.
    ///
    /// Removes and returns all tasks (including reserved) at the given priority.
    pub fn drain_by_priority(&self, priority: TaskPriority) -> Vec<QueuedTask> {
        let mut state = self.lock_state();
        if let Some(deque) = state.buckets.remove(&priority) {
            for task in &deque {
                state.index.remove(&task.id);
            }
            let tasks: Vec<QueuedTask> = deque.into();
            info!(
                priority = %priority,
                count = tasks.len(),
                "drained tasks at priority"
            );
            tasks
        } else {
            Vec::new()
        }
    }

    /// Reserve a task (mark as assigned to a node without removing from queue).
    ///
    /// Reserved tasks are skipped by `dequeue()` and `peek()` but remain
    /// in the queue for tracking. Call `confirm_reservation()` to remove
    /// or `cancel_reservation()` to un-reserve.
    pub fn reserve(&self, task_id: Uuid, worker_name: impl Into<String>) -> bool {
        let node = worker_name.into();
        let mut state = self.lock_state();
        if let Some(&priority) = state.index.get(&task_id) {
            if let Some(deque) = state.buckets.get_mut(&priority) {
                if let Some(task) = deque.iter_mut().find(|t| t.id == task_id) {
                    task.reserved = true;
                    task.reserved_node = Some(node.clone());
                    debug!(task_id = %task_id, node = %node, "task reserved");
                    return true;
                }
            }
        }
        false
    }

    /// Confirm a reservation — remove the reserved task from the queue.
    pub fn confirm_reservation(&self, task_id: Uuid) -> Option<QueuedTask> {
        let mut state = self.lock_state();
        if let Some(&priority) = state.index.get(&task_id) {
            let removed = if let Some(deque) = state.buckets.get_mut(&priority) {
                deque
                    .iter()
                    .position(|t| t.id == task_id && t.reserved)
                    .map(|pos| {
                        let task = deque.remove(pos).unwrap();
                        let is_empty = deque.is_empty();
                        (task, is_empty)
                    })
            } else {
                None
            };

            if let Some((task, is_empty)) = removed {
                state.index.remove(&task_id);
                if is_empty {
                    state.buckets.remove(&priority);
                }
                debug!(task_id = %task_id, "reservation confirmed, task removed");
                return Some(task);
            }
        }
        None
    }

    /// Cancel a reservation — make the task available for dequeue again.
    pub fn cancel_reservation(&self, task_id: Uuid) -> bool {
        let mut state = self.lock_state();
        if let Some(&priority) = state.index.get(&task_id) {
            if let Some(deque) = state.buckets.get_mut(&priority) {
                if let Some(task) = deque.iter_mut().find(|t| t.id == task_id) {
                    task.reserved = false;
                    task.reserved_node = None;
                    debug!(task_id = %task_id, "reservation cancelled");
                    return true;
                }
            }
        }
        false
    }

    /// Remove a specific task by ID (regardless of reservation status).
    pub fn remove(&self, task_id: Uuid) -> Option<QueuedTask> {
        let mut state = self.lock_state();
        if let Some(&priority) = state.index.get(&task_id) {
            let removed = if let Some(deque) = state.buckets.get_mut(&priority) {
                deque.iter().position(|t| t.id == task_id).map(|pos| {
                    let task = deque.remove(pos).unwrap();
                    let is_empty = deque.is_empty();
                    (task, is_empty)
                })
            } else {
                None
            };

            if let Some((task, is_empty)) = removed {
                state.index.remove(&task_id);
                if is_empty {
                    state.buckets.remove(&priority);
                }
                return Some(task);
            }
        }
        None
    }

    /// Apply timeout-based priority boosting.
    ///
    /// Tasks waiting longer than `boost_timeout` get promoted one priority
    /// level (e.g. Low → Normal). Critical tasks cannot be boosted further.
    ///
    /// Returns the number of tasks that were boosted.
    pub fn apply_timeout_boosts(&self) -> usize {
        let now = Utc::now();
        let timeout_secs = self.boost_timeout.as_secs() as i64;
        let mut to_boost: Vec<(Uuid, TaskPriority, TaskPriority)> = Vec::new();

        let mut state = self.lock_state();
        for (&priority, deque) in &state.buckets {
            for task in deque.iter() {
                if task.reserved {
                    continue; // Don't boost reserved tasks
                }
                let waited = now.signed_duration_since(task.enqueued_at).num_seconds();
                if waited >= timeout_secs {
                    if let Some(new_priority) = boost_priority(priority) {
                        to_boost.push((task.id, priority, new_priority));
                    }
                }
            }
        }

        let count = to_boost.len();

        for (task_id, old_priority, new_priority) in to_boost {
            // Remove from old bucket
            let task = {
                let removed = if let Some(deque) = state.buckets.get_mut(&old_priority) {
                    deque.iter().position(|t| t.id == task_id).map(|pos| {
                        let mut task = deque.remove(pos).unwrap();
                        task.effective_priority = new_priority;
                        // Reset enqueued_at so the boost timer restarts
                        task.enqueued_at = now;

                        let is_empty = deque.is_empty();
                        (task, is_empty)
                    })
                } else {
                    None
                };

                if let Some((task, is_empty)) = removed {
                    if is_empty {
                        state.buckets.remove(&old_priority);
                    }
                    task
                } else {
                    continue;
                }
            };

            // Insert into new bucket
            state.index.insert(task_id, new_priority);
            state
                .buckets
                .entry(new_priority)
                .or_default()
                .push_back(task);

            info!(
                task_id = %task_id,
                from = %old_priority,
                to = %new_priority,
                "task priority boosted due to timeout"
            );
        }

        count
    }

    /// Get counts per priority level.
    pub fn counts_by_priority(&self) -> BTreeMap<TaskPriority, usize> {
        let state = self.lock_state();
        state.buckets.iter().map(|(&p, d)| (p, d.len())).collect()
    }

    /// Iterate over all tasks (immutable, in priority order).
    ///
    /// Returns a cloned snapshot so callers do not hold the queue lock.
    pub fn iter(&self) -> Vec<QueuedTask> {
        let state = self.lock_state();
        state
            .buckets
            .values()
            .flat_map(|d| d.iter().cloned())
            .collect()
    }
}

/// Boost a priority by one level. Returns `None` if already Critical.
fn boost_priority(p: TaskPriority) -> Option<TaskPriority> {
    match p {
        TaskPriority::Background => Some(TaskPriority::Low),
        TaskPriority::Low => Some(TaskPriority::Normal),
        TaskPriority::Normal => Some(TaskPriority::High),
        TaskPriority::High => Some(TaskPriority::Critical),
        TaskPriority::Critical => None, // already max
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_queued_task(desc: &str, priority: TaskPriority) -> QueuedTask {
        QueuedTask::new(desc, ResourceRequirements::default(), priority)
    }

    #[test]
    fn test_enqueue_dequeue_fifo() {
        let q = PriorityQueue::with_default_timeout();

        let t1 = make_queued_task("first", TaskPriority::Normal);
        let t2 = make_queued_task("second", TaskPriority::Normal);
        let id1 = t1.id;
        let id2 = t2.id;

        q.enqueue(t1, TaskPriority::Normal);
        q.enqueue(t2, TaskPriority::Normal);

        assert_eq!(q.len(), 2);

        let out1 = q.dequeue().unwrap();
        assert_eq!(out1.id, id1, "FIFO within same priority");

        let out2 = q.dequeue().unwrap();
        assert_eq!(out2.id, id2);

        assert!(q.dequeue().is_none());
        assert!(q.is_empty());
    }

    #[test]
    fn test_priority_ordering() {
        let q = PriorityQueue::with_default_timeout();

        let low = make_queued_task("low", TaskPriority::Low);
        let high = make_queued_task("high", TaskPriority::High);
        let critical = make_queued_task("critical", TaskPriority::Critical);
        let crit_id = critical.id;
        let high_id = high.id;
        let low_id = low.id;

        // Enqueue in reverse order
        q.enqueue(low, TaskPriority::Low);
        q.enqueue(high, TaskPriority::High);
        q.enqueue(critical, TaskPriority::Critical);

        // Should come out highest-priority first
        assert_eq!(q.dequeue().unwrap().id, crit_id);
        assert_eq!(q.dequeue().unwrap().id, high_id);
        assert_eq!(q.dequeue().unwrap().id, low_id);
    }

    #[test]
    fn test_peek() {
        let q = PriorityQueue::with_default_timeout();
        assert!(q.peek().is_none());

        let t = make_queued_task("test", TaskPriority::Normal);
        let id = t.id;
        q.enqueue(t, TaskPriority::Normal);

        assert_eq!(q.peek().unwrap().id, id);
        assert_eq!(q.len(), 1); // peek doesn't remove
    }

    #[test]
    fn test_drain_by_priority() {
        let q = PriorityQueue::with_default_timeout();

        q.enqueue(
            make_queued_task("n1", TaskPriority::Normal),
            TaskPriority::Normal,
        );
        q.enqueue(
            make_queued_task("n2", TaskPriority::Normal),
            TaskPriority::Normal,
        );
        q.enqueue(
            make_queued_task("h1", TaskPriority::High),
            TaskPriority::High,
        );

        let drained = q.drain_by_priority(TaskPriority::Normal);
        assert_eq!(drained.len(), 2);
        assert_eq!(q.len(), 1); // only High left
    }

    #[test]
    fn test_reservation() {
        let q = PriorityQueue::with_default_timeout();

        let t1 = make_queued_task("reservable", TaskPriority::Normal);
        let t2 = make_queued_task("available", TaskPriority::Normal);
        let id1 = t1.id;
        let id2 = t2.id;

        q.enqueue(t1, TaskPriority::Normal);
        q.enqueue(t2, TaskPriority::Normal);

        // Reserve the first task
        assert!(q.reserve(id1, "james"));

        // Dequeue should skip reserved task and return the second one
        let out = q.dequeue().unwrap();
        assert_eq!(out.id, id2);

        // Peek should return None (only reserved task left)
        assert!(q.peek().is_none());

        // But len includes reserved tasks
        assert_eq!(q.len(), 1);
        assert_eq!(q.unreserved_count(), 0);

        // Confirm the reservation removes it
        let confirmed = q.confirm_reservation(id1).unwrap();
        assert_eq!(confirmed.id, id1);
        assert!(q.is_empty());
    }

    #[test]
    fn test_cancel_reservation() {
        let q = PriorityQueue::with_default_timeout();

        let t = make_queued_task("task", TaskPriority::Normal);
        let id = t.id;

        q.enqueue(t, TaskPriority::Normal);
        q.reserve(id, "james");

        // Peek skips reserved
        assert!(q.peek().is_none());

        // Cancel makes it available again
        q.cancel_reservation(id);
        assert_eq!(q.peek().unwrap().id, id);
    }

    #[test]
    fn test_remove() {
        let q = PriorityQueue::with_default_timeout();

        let t = make_queued_task("task", TaskPriority::Normal);
        let id = t.id;
        q.enqueue(t, TaskPriority::Normal);

        let removed = q.remove(id).unwrap();
        assert_eq!(removed.id, id);
        assert!(q.is_empty());
    }

    #[test]
    fn test_timeout_boost() {
        let q = PriorityQueue::new(Duration::from_secs(0)); // instant boost

        let mut t = make_queued_task("old task", TaskPriority::Low);
        // Backdate the enqueue time
        t.enqueued_at = Utc::now() - chrono::TimeDelta::try_seconds(10).unwrap();
        let id = t.id;

        q.enqueue(t, TaskPriority::Low);

        let boosted = q.apply_timeout_boosts();
        assert_eq!(boosted, 1);

        // Task should now be at Normal priority
        let counts = q.counts_by_priority();
        assert_eq!(counts.get(&TaskPriority::Normal), Some(&1));
        assert!(counts.get(&TaskPriority::Low).is_none());

        // The task itself should reflect the boost
        let out = q.dequeue().unwrap();
        assert_eq!(out.id, id);
        assert_eq!(out.original_priority, TaskPriority::Low);
        assert_eq!(out.effective_priority, TaskPriority::Normal);
    }

    #[test]
    fn test_boost_priority_ladder() {
        assert_eq!(
            boost_priority(TaskPriority::Background),
            Some(TaskPriority::Low)
        );
        assert_eq!(
            boost_priority(TaskPriority::Low),
            Some(TaskPriority::Normal)
        );
        assert_eq!(
            boost_priority(TaskPriority::Normal),
            Some(TaskPriority::High)
        );
        assert_eq!(
            boost_priority(TaskPriority::High),
            Some(TaskPriority::Critical)
        );
        assert_eq!(boost_priority(TaskPriority::Critical), None);
    }

    #[test]
    fn test_counts_by_priority() {
        let q = PriorityQueue::with_default_timeout();

        q.enqueue(
            make_queued_task("h1", TaskPriority::High),
            TaskPriority::High,
        );
        q.enqueue(
            make_queued_task("h2", TaskPriority::High),
            TaskPriority::High,
        );
        q.enqueue(
            make_queued_task("n1", TaskPriority::Normal),
            TaskPriority::Normal,
        );
        q.enqueue(
            make_queued_task("b1", TaskPriority::Background),
            TaskPriority::Background,
        );

        let counts = q.counts_by_priority();
        assert_eq!(counts[&TaskPriority::High], 2);
        assert_eq!(counts[&TaskPriority::Normal], 1);
        assert_eq!(counts[&TaskPriority::Background], 1);
    }

    #[test]
    fn test_iter() {
        let q = PriorityQueue::with_default_timeout();

        q.enqueue(
            make_queued_task("bg", TaskPriority::Background),
            TaskPriority::Background,
        );
        q.enqueue(
            make_queued_task("crit", TaskPriority::Critical),
            TaskPriority::Critical,
        );
        q.enqueue(
            make_queued_task("norm", TaskPriority::Normal),
            TaskPriority::Normal,
        );

        let descriptions: Vec<String> = q.iter().into_iter().map(|t| t.description).collect();
        // Should iterate in priority order: Critical, Normal, Background
        assert_eq!(descriptions, vec!["crit", "norm", "bg"]);
    }
}
