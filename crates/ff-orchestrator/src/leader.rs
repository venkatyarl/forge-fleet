//! Leader-coordinated work queue for ForgeFleet.
//!
//! Integrates [`ff_core::leader`] election state with the orchestrator's
//! priority queue ([`PriorityQueue`]) and resource-aware scheduler
//! ([`Scheduler`]). Only the stable leader schedules work; agents coordinate
//! through heartbeats and task lifecycle events.
//!
//! # Coordination model
//!
//! 1. Tasks are submitted to the leader's [`PriorityQueue`].
//! 2. On each `tick`, the leader boosts stale priorities, peeks at the
//!    highest-priority unreserved task, and asks the scheduler for an
//!    [`Assign`](ScheduleDecision::Assign), [`Queue`](ScheduleDecision::Queue),
//!    or [`Preempt`](ScheduleDecision::Preempt) decision.
//! 3. Assigned tasks are *reserved* in the queue and recorded as pending
//!    assignments keyed by worker name.
//! 4. An agent heartbeat confirms the reservation, removes the task from the
//!    queue, and returns the assignment to the agent.
//! 5. Completed or failed tasks release the scheduler's node capacity.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use uuid::Uuid;

use ff_core::leader::ElectionState;

use crate::placement::PlacementPolicy;
use crate::queue::{PriorityQueue, QueuedTask};
use crate::scheduler::{
    NodeCapacity, ResourceRequirements, ScheduleDecision, ScheduledTask, Scheduler, TaskPriority,
};

/// Default duration after which a queued task is boosted one priority level.
pub const DEFAULT_BOOST_TIMEOUT: Duration = Duration::from_secs(600);

/// Default duration after which a pending assignment is considered stale.
pub const DEFAULT_ASSIGNMENT_TIMEOUT: Duration = Duration::from_secs(60);

/// Agent/node health snapshot tracked by the coordinator.
#[derive(Debug, Clone)]
struct NodeHealth {
    /// Whether the node is currently considered online.
    online: bool,
    /// Last heartbeat observed from the node.
    last_heartbeat: DateTime<Utc>,
}

impl NodeHealth {
    fn new(online: bool) -> Self {
        Self {
            online,
            last_heartbeat: Utc::now(),
        }
    }
}

/// An assignment that has been decided by the leader but not yet confirmed by
/// an agent heartbeat.
#[derive(Debug, Clone)]
struct PendingAssignment {
    task_id: Uuid,
    assigned_at: DateTime<Utc>,
}

/// Result of submitting a task to the leader coordinator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmissionResult {
    /// ID of the submitted task.
    pub task_id: Uuid,
    /// Disposition of the submission.
    pub action: SubmissionAction,
}

/// Disposition of a task submission.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SubmissionAction {
    /// Task was placed in the priority queue.
    Queued,
    /// Task was immediately assigned to a worker.
    Assigned { worker_name: String, score: f64 },
    /// Task preempted a lower-priority task on a worker.
    Preempted {
        worker_name: String,
        evict_task_id: Uuid,
        score: f64,
    },
}

/// Task handed to an agent after a heartbeat confirms the leader's assignment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTask {
    /// Task ID.
    pub task_id: Uuid,
    /// Human-readable description.
    pub description: String,
    /// Priority of the task.
    pub priority: TaskPriority,
    /// Resource requirements.
    pub requirements: ResourceRequirements,
}

/// Result returned to an agent after it heartbeats to the leader.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHeartbeatResult {
    /// Name of the worker that heartbeated.
    pub worker_name: String,
    /// Whether this worker is currently the leader.
    pub is_leader: bool,
    /// Task assigned to this worker, if any.
    pub assigned_task: Option<AgentTask>,
}

/// A single task assignment produced by a leader tick.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assignment {
    /// Task ID.
    pub task_id: Uuid,
    /// Worker assigned to run the task.
    pub worker_name: String,
    /// Placement score for the assignment.
    pub score: f64,
}

/// A single preemption produced by a leader tick.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preemption {
    /// Incoming task ID.
    pub task_id: Uuid,
    /// Worker where preemption will occur.
    pub worker_name: String,
    /// Task ID being evicted.
    pub evict_task_id: Uuid,
    /// Placement score.
    pub score: f64,
}

/// Result of running one leader scheduling tick.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TickResult {
    /// New assignments this tick.
    pub assignments: Vec<Assignment>,
    /// Preemptions this tick.
    pub preemptions: Vec<Preemption>,
    /// Number of tasks whose priority was boosted due to queue age.
    pub boost_count: usize,
}

impl TickResult {
    /// Empty tick result.
    fn empty() -> Self {
        Self {
            assignments: Vec::new(),
            preemptions: Vec::new(),
            boost_count: 0,
        }
    }
}

/// Leader coordinator that owns the priority queue and scheduler.
///
/// The coordinator is designed to run on every node, but only the node that
/// matches [`ElectionState::leader`] performs scheduling decisions.
#[derive(Debug)]
pub struct LeaderCoordinator {
    /// Name of the node this coordinator is running on.
    my_node_name: String,
    /// Current leader election state.
    election_state: ElectionState,
    /// Pending work queue.
    queue: PriorityQueue,
    /// Resource-aware scheduler.
    scheduler: Scheduler,
    /// Health state for registered nodes.
    node_health: HashMap<String, NodeHealth>,
    /// Assignments waiting for an agent heartbeat to confirm.
    pending_assignments: HashMap<String, PendingAssignment>,
    /// How long an assignment can sit unconfirmed before it is reaped.
    assignment_timeout: Duration,
}

impl LeaderCoordinator {
    /// Create a new leader coordinator for the given node.
    pub fn new(my_node_name: impl Into<String>, placement_policy: PlacementPolicy) -> Self {
        let my_node_name = my_node_name.into();
        Self {
            my_node_name: my_node_name.clone(),
            election_state: ElectionState::NoLeader { since: Utc::now() },
            queue: PriorityQueue::with_default_timeout(),
            scheduler: Scheduler::new(placement_policy),
            node_health: HashMap::new(),
            pending_assignments: HashMap::new(),
            assignment_timeout: DEFAULT_ASSIGNMENT_TIMEOUT,
        }
    }

    /// Create a coordinator with custom timeouts (useful in tests).
    #[cfg(test)]
    fn with_timeouts(
        my_node_name: impl Into<String>,
        placement_policy: PlacementPolicy,
        boost_timeout: Duration,
        assignment_timeout: Duration,
    ) -> Self {
        let my_node_name = my_node_name.into();
        Self {
            my_node_name: my_node_name.clone(),
            election_state: ElectionState::NoLeader { since: Utc::now() },
            queue: PriorityQueue::new(boost_timeout),
            scheduler: Scheduler::new(placement_policy),
            node_health: HashMap::new(),
            pending_assignments: HashMap::new(),
            assignment_timeout,
        }
    }

    /// Name of the node this coordinator is running on.
    pub fn my_node_name(&self) -> &str {
        &self.my_node_name
    }

    /// Current election state.
    pub fn election_state(&self) -> &ElectionState {
        &self.election_state
    }

    /// Update the election state. Only the stable leader may schedule work.
    pub fn update_election_state(&mut self, state: ElectionState) {
        let old_leader = self.election_state.leader().map(|s| s.to_string());
        let new_leader = state.leader().map(|s| s.to_string());

        if old_leader != new_leader {
            info!(
                old_leader = ?old_leader,
                new_leader = ?new_leader,
                "leader election state changed"
            );
        }

        self.election_state = state;
    }

    /// Returns `true` if this coordinator's node is the current stable leader.
    pub fn am_i_leader(&self) -> bool {
        self.is_leader(self.my_node_name())
    }

    /// Returns `true` if the named node is the current stable leader.
    pub fn is_leader(&self, node_name: &str) -> bool {
        self.election_state.leader() == Some(node_name)
    }

    /// Register a node and its capacity with the coordinator.
    ///
    /// The node is marked online and added to the scheduler. If a node with the
    /// same name already exists, its capacity is replaced and health is reset.
    pub fn register_node(&mut self, capacity: NodeCapacity) {
        let name = capacity.worker_name.clone();
        self.scheduler.add_node(capacity);
        self.node_health.insert(name.clone(), NodeHealth::new(true));
        info!(node = %name, "node registered with leader coordinator");
    }

    /// Mark a node as online or offline.
    ///
    /// Offline nodes are excluded from scheduling but remain registered so they
    /// can be brought back online after a heartbeat.
    pub fn set_node_online(&mut self, worker_name: &str, online: bool) {
        self.scheduler.set_node_online(worker_name, online);
        if let Some(health) = self.node_health.get_mut(worker_name) {
            health.online = online;
            if online {
                health.last_heartbeat = Utc::now();
            }
        }
        debug!(node = %worker_name, online, "node online status updated");
    }

    /// Remove a node from the coordinator entirely.
    pub fn unregister_node(&mut self, worker_name: &str) -> Option<NodeCapacity> {
        self.node_health.remove(worker_name);
        self.pending_assignments.remove(worker_name);
        self.scheduler.remove_node(worker_name)
    }

    /// Number of tasks currently in the priority queue (including reserved).
    pub fn queued_task_count(&self) -> usize {
        self.queue.len()
    }

    /// Number of nodes registered with the coordinator.
    pub fn node_count(&self) -> usize {
        self.scheduler.node_count()
    }

    /// Submit a task to the leader coordinator.
    ///
    /// The task is always placed in the priority queue. The next leader tick
    /// will dequeue it and ask the scheduler for a placement decision. This
    /// keeps scheduling centralized on the leader and makes agent coordination
    /// deterministic.
    pub fn submit_task(&mut self, task: ScheduledTask) -> SubmissionResult {
        let task_id = task.id;
        let queued = QueuedTask::from_scheduled(&task);
        self.queue.enqueue(queued, task.priority);
        debug!(task_id = %task_id, priority = %task.priority, "task queued");
        SubmissionResult {
            task_id,
            action: SubmissionAction::Queued,
        }
    }

    /// Process a heartbeat from an agent.
    ///
    /// Updates the node's online status, last heartbeat time, and dispatch-tick
    /// timestamp. Logs the dispatch-tick activity so operators can verify the
    /// agent's dispatch loop is being monitored. If the leader has a pending
    /// assignment for this worker, the reservation is confirmed and the task is
    /// returned so the agent can start execution.
    pub fn heartbeat_from_agent(&mut self, worker_name: impl Into<String>) -> AgentHeartbeatResult {
        let worker_name = worker_name.into();
        let is_leader = self.is_leader(&worker_name);
        let now = Utc::now();

        if let Some(health) = self.node_health.get_mut(&worker_name) {
            health.online = true;
            health.last_heartbeat = now;
        }
        self.scheduler.set_node_online(&worker_name, true);
        self.scheduler.update_dispatch_tick(&worker_name, Some(now));

        info!(
            node = %worker_name,
            dispatch_tick_at = %now.to_rfc3339(),
            "agent heartbeat refreshed dispatch tick"
        );

        // Only the leader hands out assignments. If a non-leader heartbeat
        // arrives, still update health but don't return work.
        if !self.am_i_leader() {
            return AgentHeartbeatResult {
                worker_name,
                is_leader,
                assigned_task: None,
            };
        }

        let assigned_task = if let Some(pending) = self.pending_assignments.remove(&worker_name) {
            // Confirm the reservation so the task is no longer in the queue.
            let confirmed = self.queue.confirm_reservation(pending.task_id);
            if confirmed.is_none() {
                // The task may have been removed already; release scheduler resources
                // so we don't leak capacity.
                self.scheduler.release_task(&worker_name, pending.task_id);
            }
            confirmed.map(|task| AgentTask {
                task_id: task.id,
                description: task.description,
                priority: task.effective_priority,
                requirements: task.requirements,
            })
        } else {
            None
        };

        AgentHeartbeatResult {
            worker_name,
            is_leader,
            assigned_task,
        }
    }

    /// Run one leader scheduling tick.
    ///
    /// If this node is not the leader, the tick is a no-op. Otherwise it:
    /// 1. Reaps stale pending assignments.
    /// 2. Applies timeout-based priority boosts.
    /// 3. Dequeues tasks and asks the scheduler for placement decisions.
    /// 4. Records pending assignments to be confirmed by agent heartbeats.
    pub fn tick(&mut self) -> TickResult {
        if !self.am_i_leader() {
            return TickResult::empty();
        }

        self.reap_stale_assignments();

        let boost_count = self.queue.apply_timeout_boosts();
        let mut result = TickResult {
            assignments: Vec::new(),
            preemptions: Vec::new(),
            boost_count,
        };

        // Schedule at most one task per tick. Peek (don't dequeue) so the task
        // stays reserved in the queue until an agent heartbeat confirms it.
        // This avoids losing the task if the agent is slow to heartbeat.
        if let Some(task) = self.queue.peek().cloned() {
            let scheduled = ScheduledTask::from_queued(&task);
            let task_id = scheduled.id;

            match self.scheduler.schedule_task(&scheduled) {
                ScheduleDecision::Assign { worker_name, score } => {
                    self.queue.reserve(task_id, &worker_name);
                    self.pending_assignments.insert(
                        worker_name.clone(),
                        PendingAssignment {
                            task_id,
                            assigned_at: Utc::now(),
                        },
                    );
                    result.assignments.push(Assignment {
                        task_id,
                        worker_name,
                        score,
                    });
                }
                ScheduleDecision::Preempt {
                    worker_name,
                    evict_task_id,
                    score,
                } => {
                    self.queue.reserve(task_id, &worker_name);
                    self.pending_assignments.insert(
                        worker_name.clone(),
                        PendingAssignment {
                            task_id,
                            assigned_at: Utc::now(),
                        },
                    );
                    result.preemptions.push(Preemption {
                        task_id,
                        worker_name,
                        evict_task_id,
                        score,
                    });
                }
                ScheduleDecision::Queue { reason } => {
                    // Task stays in queue; nothing to do.
                    debug!(task_id = %task_id, reason, "task remains queued");
                }
            }
        }

        if !result.assignments.is_empty() || !result.preemptions.is_empty() || boost_count > 0 {
            info!(
                assignments = result.assignments.len(),
                preemptions = result.preemptions.len(),
                boosts = boost_count,
                "leader tick complete"
            );
        }

        result
    }

    /// Release resources when a task completes.
    ///
    /// Returns `true` if the task was running on the node.
    pub fn complete_task(&mut self, worker_name: &str, task_id: Uuid) -> bool {
        let released = self.scheduler.release_task(worker_name, task_id);
        if released.is_some() {
            info!(task_id = %task_id, node = %worker_name, "task completed");
            true
        } else {
            warn!(task_id = %task_id, node = %worker_name, "complete_task called for unknown task");
            false
        }
    }

    /// Mark a task as failed and release its resources.
    ///
    /// Optionally re-queues the task at its current effective priority so it
    /// can be retried. Returns `true` if the task was running on the node.
    pub fn fail_task(
        &mut self,
        worker_name: &str,
        task_id: Uuid,
        reason: impl Into<String>,
        requeue: bool,
    ) -> bool {
        let released = self.scheduler.release_task(worker_name, task_id);
        if released.is_none() {
            warn!(task_id = %task_id, node = %worker_name, "fail_task called for unknown task");
            return false;
        }

        let reason = reason.into();
        if requeue {
            // Reconstruct a queued task from the released running task. We don't
            // have the description/project here, so we use a minimal placeholder.
            // Callers that need richer retry semantics should submit a fresh task.
            let task = QueuedTask::new(
                format!("retry of {task_id}"),
                ResourceRequirements::default(),
                TaskPriority::Normal,
            )
            .with_project("retry");
            self.queue.enqueue(task, TaskPriority::Normal);
            info!(
                task_id = %task_id,
                node = %worker_name,
                %reason,
                "task failed and re-queued for retry"
            );
        } else {
            info!(
                task_id = %task_id,
                node = %worker_name,
                %reason,
                "task failed"
            );
        }
        true
    }

    /// Reap pending assignments that have not been confirmed by an agent
    /// heartbeat within the configured timeout.
    fn reap_stale_assignments(&mut self) {
        let now = Utc::now();
        let stale: Vec<String> = self
            .pending_assignments
            .iter()
            .filter(|(_, pending)| {
                now.signed_duration_since(pending.assigned_at).num_seconds()
                    >= self.assignment_timeout.as_secs() as i64
            })
            .map(|(worker_name, _)| worker_name.clone())
            .collect();

        for worker_name in stale {
            if let Some(pending) = self.pending_assignments.remove(&worker_name) {
                // Cancel the queue reservation and release scheduler capacity.
                self.queue.cancel_reservation(pending.task_id);
                self.scheduler.release_task(&worker_name, pending.task_id);
                warn!(
                    task_id = %pending.task_id,
                    node = %worker_name,
                    "stale assignment reaped"
                );
            }
        }
    }
}

// ─── Conversions between queue and scheduler task types ──────────────────────

impl QueuedTask {
    /// Build a queue entry from a scheduled task.
    fn from_scheduled(task: &ScheduledTask) -> Self {
        Self {
            id: task.id,
            description: task.description.clone(),
            project: task.project.clone(),
            requirements: task.requirements.clone(),
            original_priority: task.priority,
            effective_priority: task.priority,
            enqueued_at: Utc::now(),
            workload_type: task.workload_type.clone(),
            reserved: false,
            reserved_node: None,
        }
    }
}

impl ScheduledTask {
    /// Build a scheduler task from a queued task.
    fn from_queued(task: &QueuedTask) -> Self {
        Self {
            id: task.id,
            description: task.description.clone(),
            project: task.project.clone(),
            requirements: task.requirements.clone(),
            priority: task.effective_priority,
            submitted_at: task.enqueued_at,
            preferred_nodes: task.reserved_node.clone().into_iter().collect(),
            workload_type: task.workload_type.clone(),
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use crate::scheduler::ResourceRequirements;

    fn make_node(name: &str, cpus: u32, mem: u64, gpu: bool) -> NodeCapacity {
        NodeCapacity::from_config(name.to_string(), cpus, mem, gpu)
    }

    fn make_task(desc: &str, cpus: u32, mem: u64) -> ScheduledTask {
        ScheduledTask::new(desc).with_requirements(ResourceRequirements {
            cpu_cores: cpus,
            memory_gib: mem,
            gpu_required: false,
            estimated_duration: Duration::from_secs(60),
        })
    }

    fn make_leader(preferred: &str) -> LeaderCoordinator {
        let mut coordinator = LeaderCoordinator::new(preferred, PlacementPolicy::BinPack);
        coordinator.update_election_state(ElectionState::Stable {
            leader: preferred.to_string(),
            since: Utc::now(),
        });
        coordinator
    }

    #[test]
    fn test_am_i_leader() {
        let mut coordinator = make_leader("taylor");
        assert!(coordinator.am_i_leader());
        assert!(coordinator.is_leader("taylor"));
        assert!(!coordinator.is_leader("james"));

        coordinator.update_election_state(ElectionState::Stable {
            leader: "james".to_string(),
            since: Utc::now(),
        });
        assert!(!coordinator.am_i_leader());
    }

    #[test]
    fn test_non_leader_tick_is_noop() {
        let mut coordinator = LeaderCoordinator::new("james", PlacementPolicy::BinPack);
        coordinator.update_election_state(ElectionState::Stable {
            leader: "taylor".to_string(),
            since: Utc::now(),
        });
        coordinator.register_node(make_node("james", 16, 64, false));
        coordinator.submit_task(make_task("work", 4, 8));

        let result = coordinator.tick();
        assert!(result.assignments.is_empty());
        assert_eq!(coordinator.queued_task_count(), 1);
    }

    #[test]
    fn test_leader_submits_and_schedules_on_tick() {
        let mut coordinator = make_leader("taylor");
        coordinator.register_node(make_node("james", 16, 64, false));
        coordinator.register_node(make_node("marcus", 16, 64, false));

        let task = make_task("build feature", 4, 8);
        let submit_result = coordinator.submit_task(task);
        assert!(matches!(submit_result.action, SubmissionAction::Queued));

        let tick = coordinator.tick();
        assert_eq!(tick.assignments.len(), 1);
        assert_eq!(tick.assignments[0].task_id, submit_result.task_id);
        // Task is reserved in queue until an agent heartbeat confirms it.
        assert_eq!(coordinator.queued_task_count(), 1);

        // Either james or marcus may have been picked; heartbeat both to
        // confirm the assignment regardless of which worker was chosen.
        coordinator.heartbeat_from_agent("james");
        coordinator.heartbeat_from_agent("marcus");
        assert_eq!(coordinator.queued_task_count(), 0);
    }

    #[test]
    fn test_heartbeat_confirms_assignment() {
        let mut coordinator = make_leader("taylor");
        coordinator.register_node(make_node("james", 16, 64, false));

        let task = make_task("urgent fix", 4, 8);
        let submit_result = coordinator.submit_task(task);
        coordinator.tick();

        let hb = coordinator.heartbeat_from_agent("james");
        assert!(!hb.is_leader);
        assert!(hb.assigned_task.is_some());
        assert_eq!(hb.assigned_task.unwrap().task_id, submit_result.task_id);
        assert_eq!(coordinator.queued_task_count(), 0);
    }

    #[test]
    fn test_priority_ordering_in_tick() {
        let mut coordinator = make_leader("taylor");
        coordinator.register_node(make_node("james", 16, 64, false));

        let low = make_task("low", 4, 8).with_priority(TaskPriority::Low);
        let critical = make_task("critical", 4, 8).with_priority(TaskPriority::Critical);
        let normal = make_task("normal", 4, 8).with_priority(TaskPriority::Normal);

        let low_id = low.id;
        let critical_id = critical.id;
        let normal_id = normal.id;

        coordinator.submit_task(low);
        coordinator.submit_task(normal);
        coordinator.submit_task(critical);

        let tick = coordinator.tick();
        assert_eq!(tick.assignments.len(), 1);
        assert_eq!(tick.assignments[0].task_id, critical_id);

        coordinator.heartbeat_from_agent("james");
        coordinator.complete_task("james", critical_id);

        let tick = coordinator.tick();
        assert_eq!(tick.assignments[0].task_id, normal_id);

        coordinator.heartbeat_from_agent("james");
        coordinator.complete_task("james", normal_id);

        let tick = coordinator.tick();
        assert_eq!(tick.assignments[0].task_id, low_id);
    }

    #[test]
    fn test_preemption_through_coordinator() {
        let mut coordinator = make_leader("taylor");
        coordinator.register_node(make_node("james", 8, 32, false));

        let bg = make_task("background", 6, 24).with_priority(TaskPriority::Background);
        let bg_id = bg.id;
        coordinator.submit_task(bg);
        coordinator.tick();
        coordinator.heartbeat_from_agent("james");

        let critical = make_task("critical", 6, 24).with_priority(TaskPriority::Critical);
        let critical_id = critical.id;
        coordinator.submit_task(critical);

        let tick = coordinator.tick();
        assert_eq!(tick.preemptions.len(), 1);
        assert_eq!(tick.preemptions[0].task_id, critical_id);
        assert_eq!(tick.preemptions[0].evict_task_id, bg_id);
    }

    #[test]
    fn test_complete_task_releases_capacity() {
        let mut coordinator = make_leader("taylor");
        coordinator.register_node(make_node("james", 8, 16, false));

        // Fill the node.
        for i in 0..2 {
            let task = make_task(&format!("fill-{i}"), 4, 8);
            coordinator.submit_task(task);
        }
        coordinator.tick();

        // Complete one task.
        let task_id = coordinator.tick().assignments[0].task_id;
        assert!(coordinator.complete_task("james", task_id));

        // A new task should now fit.
        let new_task = make_task("new", 4, 8);
        let new_id = new_task.id;
        coordinator.submit_task(new_task);
        let tick = coordinator.tick();
        assert_eq!(tick.assignments.len(), 1);
        assert_eq!(tick.assignments[0].task_id, new_id);
    }

    #[test]
    fn test_offline_node_excluded() {
        let mut coordinator = make_leader("taylor");
        coordinator.register_node(make_node("james", 16, 64, false));
        coordinator.register_node(make_node("marcus", 16, 64, false));

        coordinator.set_node_online("james", false);

        let task = make_task("work", 4, 8);
        let task_id = task.id;
        coordinator.submit_task(task);

        let tick = coordinator.tick();
        assert_eq!(tick.assignments.len(), 1);
        assert_eq!(tick.assignments[0].task_id, task_id);
        assert_eq!(tick.assignments[0].worker_name, "marcus");
    }

    #[test]
    fn test_stale_assignment_reaped() {
        let mut coordinator = LeaderCoordinator::with_timeouts(
            "taylor",
            PlacementPolicy::BinPack,
            Duration::from_secs(3600),
            Duration::from_secs(0), // immediate stale timeout
        );
        coordinator.update_election_state(ElectionState::Stable {
            leader: "taylor".to_string(),
            since: Utc::now(),
        });
        coordinator.register_node(make_node("james", 16, 64, false));

        let task = make_task("work", 4, 8);
        let task_id = task.id;
        coordinator.submit_task(task);
        coordinator.tick();

        // The assignment is pending but no heartbeat confirms it.
        // The next tick should reap it and re-schedule.
        let tick = coordinator.tick();
        assert_eq!(tick.assignments.len(), 1);
        assert_eq!(tick.assignments[0].task_id, task_id);
    }

    #[test]
    fn test_fail_task_requeues() {
        let mut coordinator = make_leader("taylor");
        coordinator.register_node(make_node("james", 16, 64, false));

        let task = make_task("work", 4, 8);
        let task_id = task.id;
        coordinator.submit_task(task);
        coordinator.tick();
        coordinator.heartbeat_from_agent("james");

        assert!(coordinator.fail_task("james", task_id, "disk full", true));
        assert_eq!(coordinator.queued_task_count(), 1);
    }

    #[test]
    fn test_heartbeat_refreshes_dispatch_tick() {
        let mut coordinator = make_leader("taylor");
        coordinator.register_node(make_node("james", 16, 64, false));

        // Age the dispatch tick so we can observe the refresh.
        let stale = Utc::now() - chrono::Duration::minutes(5);
        coordinator
            .scheduler
            .update_dispatch_tick("james", Some(stale));
        assert_eq!(
            coordinator
                .scheduler
                .get_node("james")
                .unwrap()
                .dispatch_tick_at,
            Some(stale)
        );

        coordinator.heartbeat_from_agent("james");

        let after = coordinator
            .scheduler
            .get_node("james")
            .unwrap()
            .dispatch_tick_at
            .unwrap();
        assert!(after > stale);
        assert!(after <= Utc::now());
    }
}
