//! Resource-aware cluster scheduler for ForgeFleet.
//!
//! Routes tasks to the optimal node based on resource availability, GPU affinity,
//! priority preemption, and project fairness.
//!
//! # Scheduling algorithm
//!
//! 1. Filter nodes that meet resource requirements (CPU, memory, GPU)
//! 2. Score remaining candidates using the configured [`PlacementPolicy`]
//! 3. Pick the best-fit node (bin packing: prefer most-free to pack later tasks)
//! 4. If no node qualifies, queue the task or preempt a lower-priority one
//! 5. Round-robin across projects when multiple tasks share the same priority

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::placement::{PlacementEngine, PlacementPolicy};

// ─── Task Priority ───────────────────────────────────────────────────────────

/// Priority levels for scheduled tasks.
///
/// Ordered so that `Critical` is the *smallest* discriminant, which means
/// `BTreeMap<TaskPriority, _>` iterates highest-priority entries first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPriority {
    /// System-critical — can preempt Background tasks
    Critical = 0,
    /// User-initiated, time-sensitive
    High = 1,
    /// Default priority
    Normal = 2,
    /// Deferred / batch work
    Low = 3,
    /// Best-effort — preemptable by Critical
    Background = 4,
}

impl TaskPriority {
    /// Returns `true` if `self` can preempt `other`.
    pub fn can_preempt(&self, other: &TaskPriority) -> bool {
        *self == TaskPriority::Critical && *other == TaskPriority::Background
    }

    /// All priority levels, highest first.
    pub fn all() -> &'static [TaskPriority] {
        &[
            TaskPriority::Critical,
            TaskPriority::High,
            TaskPriority::Normal,
            TaskPriority::Low,
            TaskPriority::Background,
        ]
    }
}

impl std::fmt::Display for TaskPriority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Critical => write!(f, "Critical"),
            Self::High => write!(f, "High"),
            Self::Normal => write!(f, "Normal"),
            Self::Low => write!(f, "Low"),
            Self::Background => write!(f, "Background"),
        }
    }
}

impl Default for TaskPriority {
    fn default() -> Self {
        Self::Normal
    }
}

// ─── Resource Requirements ───────────────────────────────────────────────────

/// What a task needs to run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceRequirements {
    /// Minimum CPU cores needed.
    pub cpu_cores: u32,
    /// Minimum memory in GiB.
    pub memory_gib: u64,
    /// Whether the task requires a GPU.
    pub gpu_required: bool,
    /// Estimated wall-clock duration.
    #[serde(with = "humantime_serde_compat")]
    pub estimated_duration: Duration,
}

impl Default for ResourceRequirements {
    fn default() -> Self {
        Self {
            cpu_cores: 1,
            memory_gib: 1,
            gpu_required: false,
            estimated_duration: Duration::from_secs(60),
        }
    }
}

/// Serde helper for `std::time::Duration` as seconds (f64).
mod humantime_serde_compat {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        d.as_secs_f64().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let secs = f64::deserialize(d)?;
        Ok(Duration::from_secs_f64(secs))
    }
}

// ─── Node Capacity (runtime snapshot) ────────────────────────────────────────

/// Runtime resource snapshot for a single node.
///
/// Tracks total vs. available resources so the scheduler can make
/// capacity-aware decisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeCapacity {
    /// Node identifier (matches node name from fleet.toml).
    pub node_name: String,
    /// Total CPU cores on this node.
    pub total_cpu_cores: u32,
    /// Currently available CPU cores.
    pub available_cpu_cores: u32,
    /// Total memory in GiB.
    pub total_memory_gib: u64,
    /// Currently available memory in GiB.
    pub available_memory_gib: u64,
    /// Whether this node has a GPU.
    pub has_gpu: bool,
    /// Whether the node is online and accepting work.
    pub online: bool,
    /// IDs of tasks currently running on this node.
    pub running_tasks: Vec<RunningTask>,
}

impl NodeCapacity {
    /// Create a capacity snapshot from fleet.toml resource info.
    pub fn from_config(
        node_name: String,
        cpu_cores: u32,
        memory_gib: u64,
        has_gpu: bool,
    ) -> Self {
        Self {
            node_name,
            total_cpu_cores: cpu_cores,
            available_cpu_cores: cpu_cores,
            total_memory_gib: memory_gib,
            available_memory_gib: memory_gib,
            has_gpu,
            online: true,
            running_tasks: Vec::new(),
        }
    }

    /// Check if this node can satisfy the given requirements.
    pub fn can_fit(&self, req: &ResourceRequirements) -> bool {
        if !self.online {
            return false;
        }
        if req.gpu_required && !self.has_gpu {
            return false;
        }
        self.available_cpu_cores >= req.cpu_cores && self.available_memory_gib >= req.memory_gib
    }

    /// Allocate resources for a task. Returns `false` if insufficient capacity.
    pub fn allocate(&mut self, task_id: Uuid, req: &ResourceRequirements, priority: TaskPriority) -> bool {
        if !self.can_fit(req) {
            return false;
        }
        self.available_cpu_cores -= req.cpu_cores;
        self.available_memory_gib -= req.memory_gib;
        self.running_tasks.push(RunningTask {
            task_id,
            priority,
            cpu_cores: req.cpu_cores,
            memory_gib: req.memory_gib,
            started_at: Utc::now(),
        });
        true
    }

    /// Release resources when a task completes or is preempted.
    pub fn release(&mut self, task_id: Uuid) -> Option<RunningTask> {
        if let Some(pos) = self.running_tasks.iter().position(|t| t.task_id == task_id) {
            let task = self.running_tasks.remove(pos);
            self.available_cpu_cores += task.cpu_cores;
            self.available_memory_gib += task.memory_gib;
            Some(task)
        } else {
            None
        }
    }

    /// Fraction of resources currently free (0.0–1.0).
    pub fn free_ratio(&self) -> f64 {
        if self.total_cpu_cores == 0 && self.total_memory_gib == 0 {
            return 0.0;
        }
        let cpu_ratio = if self.total_cpu_cores > 0 {
            self.available_cpu_cores as f64 / self.total_cpu_cores as f64
        } else {
            1.0
        };
        let mem_ratio = if self.total_memory_gib > 0 {
            self.available_memory_gib as f64 / self.total_memory_gib as f64
        } else {
            1.0
        };
        (cpu_ratio + mem_ratio) / 2.0
    }

    /// Find the lowest-priority preemptable task, if any.
    pub fn lowest_priority_task(&self) -> Option<&RunningTask> {
        self.running_tasks.iter().max_by_key(|t| t.priority)
    }
}

/// A task currently running on a node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningTask {
    pub task_id: Uuid,
    pub priority: TaskPriority,
    pub cpu_cores: u32,
    pub memory_gib: u64,
    pub started_at: DateTime<Utc>,
}

// ─── Schedule Decision ───────────────────────────────────────────────────────

/// The scheduler's decision for a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ScheduleDecision {
    /// Assign the task to the named node.
    Assign {
        node_name: String,
        score: f64,
    },
    /// No capacity — task should be queued.
    Queue {
        reason: String,
    },
    /// Preempt a lower-priority task to make room.
    Preempt {
        /// Node where preemption will happen.
        node_name: String,
        /// Task to evict.
        evict_task_id: Uuid,
        score: f64,
    },
}

impl ScheduleDecision {
    /// Returns `true` if the task was assigned (directly or via preemption).
    pub fn is_assigned(&self) -> bool {
        matches!(self, Self::Assign { .. } | Self::Preempt { .. })
    }

    /// The target node name, if assigned.
    pub fn target_node(&self) -> Option<&str> {
        match self {
            Self::Assign { node_name, .. } | Self::Preempt { node_name, .. } => {
                Some(node_name.as_str())
            }
            Self::Queue { .. } => None,
        }
    }
}

// ─── Scheduled Task ──────────────────────────────────────────────────────────

/// A task submitted to the scheduler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    /// Unique task ID.
    pub id: Uuid,
    /// Human-readable description.
    pub description: String,
    /// Project this task belongs to (for fairness).
    pub project: Option<String>,
    /// Resource requirements.
    pub requirements: ResourceRequirements,
    /// Task priority.
    pub priority: TaskPriority,
    /// When the task was submitted.
    pub submitted_at: DateTime<Utc>,
    /// Preferred node names (hints, not hard constraints).
    pub preferred_nodes: Vec<String>,
    /// Workload type (e.g. "coding", "review", "build").
    pub workload_type: Option<String>,
}

impl ScheduledTask {
    /// Create a new task with defaults.
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            description: description.into(),
            project: None,
            requirements: ResourceRequirements::default(),
            priority: TaskPriority::Normal,
            submitted_at: Utc::now(),
            preferred_nodes: Vec::new(),
            workload_type: None,
        }
    }

    /// Builder: set priority.
    pub fn with_priority(mut self, priority: TaskPriority) -> Self {
        self.priority = priority;
        self
    }

    /// Builder: set project.
    pub fn with_project(mut self, project: impl Into<String>) -> Self {
        self.project = Some(project.into());
        self
    }

    /// Builder: set requirements.
    pub fn with_requirements(mut self, req: ResourceRequirements) -> Self {
        self.requirements = req;
        self
    }

    /// Builder: set workload type.
    pub fn with_workload_type(mut self, wt: impl Into<String>) -> Self {
        self.workload_type = Some(wt.into());
        self
    }
}

// ─── Scheduler ───────────────────────────────────────────────────────────────

/// Resource-aware cluster scheduler.
///
/// Maintains runtime node capacities and makes scheduling decisions
/// using configurable placement policies.
pub struct Scheduler {
    /// Per-node capacity tracking.
    nodes: HashMap<String, NodeCapacity>,
    /// Placement engine for scoring nodes.
    placement: PlacementEngine,
    /// Round-robin index per priority for project fairness.
    round_robin_index: HashMap<TaskPriority, usize>,
}

impl Scheduler {
    /// Create a new scheduler with the given placement policy.
    pub fn new(policy: PlacementPolicy) -> Self {
        Self {
            nodes: HashMap::new(),
            placement: PlacementEngine::new(policy),
            round_robin_index: HashMap::new(),
        }
    }

    /// Register a node with its capacity.
    pub fn add_node(&mut self, capacity: NodeCapacity) {
        info!(node = %capacity.node_name, "registered node in scheduler");
        self.nodes.insert(capacity.node_name.clone(), capacity);
    }

    /// Remove a node from the scheduler.
    pub fn remove_node(&mut self, node_name: &str) -> Option<NodeCapacity> {
        self.nodes.remove(node_name)
    }

    /// Get a reference to a node's capacity.
    pub fn get_node(&self, node_name: &str) -> Option<&NodeCapacity> {
        self.nodes.get(node_name)
    }

    /// Get a mutable reference to a node's capacity.
    pub fn get_node_mut(&mut self, node_name: &str) -> Option<&mut NodeCapacity> {
        self.nodes.get_mut(node_name)
    }

    /// Update a node's online status.
    pub fn set_node_online(&mut self, node_name: &str, online: bool) {
        if let Some(node) = self.nodes.get_mut(node_name) {
            node.online = online;
        }
    }

    /// Release resources for a completed task.
    pub fn release_task(&mut self, node_name: &str, task_id: Uuid) -> Option<RunningTask> {
        self.nodes.get_mut(node_name).and_then(|n| n.release(task_id))
    }

    /// Number of registered nodes.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// All registered node names.
    pub fn node_names(&self) -> Vec<&str> {
        self.nodes.keys().map(|s| s.as_str()).collect()
    }

    /// Schedule a task onto the best available node.
    ///
    /// # Algorithm
    ///
    /// 1. Filter to nodes that can fit the resource requirements
    /// 2. Score each candidate using the placement engine
    /// 3. Apply project-based round-robin tiebreaking
    /// 4. If no node fits, attempt preemption (Critical → Background only)
    /// 5. If preemption fails, return Queue decision
    pub fn schedule_task(&mut self, task: &ScheduledTask) -> ScheduleDecision {
        let req = &task.requirements;

        // Step 1: Filter eligible nodes and score them
        // Collect owned Strings to avoid holding borrows on self.nodes
        let mut candidates: Vec<(String, f64)> = self
            .nodes
            .iter()
            .filter(|(_, cap)| cap.can_fit(req))
            .map(|(name, cap)| {
                let score = self.placement.score_node(task, cap);
                (name.clone(), score)
            })
            .collect();

        // Step 2: Sort by score descending (best first)
        candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Step 3: Apply project fairness via round-robin when scores are tied
        if candidates.len() > 1 {
            self.apply_fairness_tiebreak(&mut candidates, task);
        }

        // Step 4: Pick best candidate
        if let Some((ref best_node, score)) = candidates.first().cloned() {
            let node_name = best_node.to_string();

            // Allocate resources
            if let Some(cap) = self.nodes.get_mut(&node_name) {
                cap.allocate(task.id, req, task.priority);
            }

            info!(
                task_id = %task.id,
                node = %node_name,
                score = score,
                priority = %task.priority,
                "task scheduled"
            );

            return ScheduleDecision::Assign { node_name, score };
        }

        // Step 5: Try preemption for Critical tasks
        if task.priority == TaskPriority::Critical {
            if let Some(decision) = self.try_preemption(task) {
                return decision;
            }
        }

        // Step 6: Queue the task
        let online_count = self.nodes.values().filter(|n| n.online).count();
        let reason = if online_count == 0 {
            "no nodes online".to_string()
        } else {
            format!(
                "insufficient resources on {} online node(s) — need {}cpu/{}GiB{}",
                online_count,
                req.cpu_cores,
                req.memory_gib,
                if req.gpu_required { " +GPU" } else { "" }
            )
        };

        warn!(task_id = %task.id, %reason, "task queued");
        ScheduleDecision::Queue { reason }
    }

    /// Apply project-based round-robin tiebreaking.
    ///
    /// When multiple candidates have similar scores (within 5%), rotate
    /// among them to distribute work from the same project across nodes.
    fn apply_fairness_tiebreak(&mut self, candidates: &mut Vec<(String, f64)>, task: &ScheduledTask) {
        if candidates.is_empty() {
            return;
        }

        let top_score = candidates[0].1;
        let threshold = top_score * 0.95; // 5% tolerance

        // Count how many candidates are within the tie threshold
        let tied_count = candidates.iter().filter(|(_, s)| *s >= threshold).count();
        if tied_count <= 1 {
            return;
        }

        // Rotate the round-robin index
        let idx = self
            .round_robin_index
            .entry(task.priority)
            .or_insert(0);
        let pick = *idx % tied_count;
        *idx = idx.wrapping_add(1);

        // If the picked candidate isn't already first, swap it in
        if pick > 0 && pick < candidates.len() {
            candidates.swap(0, pick);
        }

        debug!(
            priority = %task.priority,
            tied = tied_count,
            picked_index = pick,
            node = %candidates[0].0,
            "fairness round-robin applied"
        );
    }

    /// Attempt to preempt a Background task for a Critical task.
    fn try_preemption(&mut self, task: &ScheduledTask) -> Option<ScheduleDecision> {
        let req = &task.requirements;

        // Find nodes with preemptable (Background) tasks whose resources
        // would free up enough capacity.
        let mut preempt_candidates: Vec<(String, Uuid, f64)> = Vec::new();

        for (name, cap) in &self.nodes {
            if !cap.online {
                continue;
            }
            // GPU check
            if req.gpu_required && !cap.has_gpu {
                continue;
            }

            // Find background tasks we could evict
            for running in &cap.running_tasks {
                if !task.priority.can_preempt(&running.priority) {
                    continue;
                }

                // Would evicting this task free enough resources?
                let freed_cpu = cap.available_cpu_cores + running.cpu_cores;
                let freed_mem = cap.available_memory_gib + running.memory_gib;

                if freed_cpu >= req.cpu_cores && freed_mem >= req.memory_gib {
                    let score = self.placement.score_node(task, cap);
                    preempt_candidates.push((name.clone(), running.task_id, score));
                }
            }
        }

        // Pick the best preemption candidate
        preempt_candidates.sort_by(|a, b| {
            b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal)
        });

        if let Some((node_name, evict_task_id, score)) = preempt_candidates.into_iter().next() {
            // Release the evicted task's resources
            if let Some(cap) = self.nodes.get_mut(&node_name) {
                cap.release(evict_task_id);
                cap.allocate(task.id, req, task.priority);
            }

            info!(
                task_id = %task.id,
                evicted = %evict_task_id,
                node = %node_name,
                "critical task preempted background task"
            );

            return Some(ScheduleDecision::Preempt {
                node_name,
                evict_task_id,
                score,
            });
        }

        None
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_node(name: &str, cpus: u32, mem: u64, gpu: bool) -> NodeCapacity {
        NodeCapacity::from_config(name.to_string(), cpus, mem, gpu)
    }

    fn make_task(desc: &str, cpus: u32, mem: u64, gpu: bool) -> ScheduledTask {
        ScheduledTask::new(desc).with_requirements(ResourceRequirements {
            cpu_cores: cpus,
            memory_gib: mem,
            gpu_required: gpu,
            estimated_duration: Duration::from_secs(60),
        })
    }

    #[test]
    fn test_priority_ordering() {
        assert!(TaskPriority::Critical < TaskPriority::High);
        assert!(TaskPriority::High < TaskPriority::Normal);
        assert!(TaskPriority::Normal < TaskPriority::Low);
        assert!(TaskPriority::Low < TaskPriority::Background);
    }

    #[test]
    fn test_preemption_rules() {
        assert!(TaskPriority::Critical.can_preempt(&TaskPriority::Background));
        assert!(!TaskPriority::Critical.can_preempt(&TaskPriority::Low));
        assert!(!TaskPriority::High.can_preempt(&TaskPriority::Background));
        assert!(!TaskPriority::Normal.can_preempt(&TaskPriority::Background));
    }

    #[test]
    fn test_node_capacity_can_fit() {
        let node = make_node("test", 16, 64, false);
        let small = ResourceRequirements {
            cpu_cores: 4,
            memory_gib: 8,
            gpu_required: false,
            estimated_duration: Duration::from_secs(30),
        };
        let big = ResourceRequirements {
            cpu_cores: 32,
            memory_gib: 128,
            gpu_required: false,
            estimated_duration: Duration::from_secs(30),
        };
        let gpu_req = ResourceRequirements {
            cpu_cores: 4,
            memory_gib: 8,
            gpu_required: true,
            estimated_duration: Duration::from_secs(30),
        };

        assert!(node.can_fit(&small));
        assert!(!node.can_fit(&big));
        assert!(!node.can_fit(&gpu_req)); // no GPU
    }

    #[test]
    fn test_node_capacity_allocate_and_release() {
        let mut node = make_node("test", 16, 64, false);
        let task_id = Uuid::new_v4();
        let req = ResourceRequirements {
            cpu_cores: 4,
            memory_gib: 16,
            gpu_required: false,
            estimated_duration: Duration::from_secs(60),
        };

        assert!(node.allocate(task_id, &req, TaskPriority::Normal));
        assert_eq!(node.available_cpu_cores, 12);
        assert_eq!(node.available_memory_gib, 48);
        assert_eq!(node.running_tasks.len(), 1);

        let released = node.release(task_id);
        assert!(released.is_some());
        assert_eq!(node.available_cpu_cores, 16);
        assert_eq!(node.available_memory_gib, 64);
        assert_eq!(node.running_tasks.len(), 0);
    }

    #[test]
    fn test_schedule_basic_assignment() {
        let mut scheduler = Scheduler::new(PlacementPolicy::BinPack);
        scheduler.add_node(make_node("james", 16, 64, false));
        scheduler.add_node(make_node("marcus", 16, 64, false));

        let task = make_task("build project", 4, 8, false);
        let decision = scheduler.schedule_task(&task);

        assert!(decision.is_assigned());
        assert!(decision.target_node().is_some());
    }

    #[test]
    fn test_schedule_gpu_affinity() {
        let mut scheduler = Scheduler::new(PlacementPolicy::BinPack);
        scheduler.add_node(make_node("james", 16, 64, false)); // no GPU
        scheduler.add_node(make_node("taylor", 32, 96, true)); // has GPU

        let task = make_task("train model", 8, 32, true);
        let decision = scheduler.schedule_task(&task);

        match &decision {
            ScheduleDecision::Assign { node_name, .. } => {
                assert_eq!(node_name, "taylor", "GPU task must go to GPU node");
            }
            other => panic!("Expected Assign, got {:?}", other),
        }
    }

    #[test]
    fn test_schedule_queue_when_no_capacity() {
        let mut scheduler = Scheduler::new(PlacementPolicy::BinPack);
        scheduler.add_node(make_node("james", 4, 8, false));

        let task = make_task("big task", 32, 128, false);
        let decision = scheduler.schedule_task(&task);

        match &decision {
            ScheduleDecision::Queue { reason } => {
                assert!(reason.contains("insufficient resources"));
            }
            other => panic!("Expected Queue, got {:?}", other),
        }
    }

    #[test]
    fn test_schedule_preemption() {
        let mut scheduler = Scheduler::new(PlacementPolicy::BinPack);
        scheduler.add_node(make_node("james", 8, 32, false));

        // Fill the node with a background task
        let bg_task = make_task("background work", 6, 24, false)
            .with_priority(TaskPriority::Background);
        let bg_decision = scheduler.schedule_task(&bg_task);
        assert!(bg_decision.is_assigned());

        // Now a critical task arrives that needs the same resources
        let critical_task = make_task("urgent fix", 6, 24, false)
            .with_priority(TaskPriority::Critical);
        let decision = scheduler.schedule_task(&critical_task);

        match &decision {
            ScheduleDecision::Preempt {
                node_name,
                evict_task_id,
                ..
            } => {
                assert_eq!(node_name, "james");
                assert_eq!(*evict_task_id, bg_task.id);
            }
            other => panic!("Expected Preempt, got {:?}", other),
        }
    }

    #[test]
    fn test_schedule_no_preemption_for_non_critical() {
        let mut scheduler = Scheduler::new(PlacementPolicy::BinPack);
        scheduler.add_node(make_node("james", 8, 32, false));

        // Fill the node with a background task
        let bg_task = make_task("background work", 6, 24, false)
            .with_priority(TaskPriority::Background);
        scheduler.schedule_task(&bg_task);

        // High priority (not Critical) cannot preempt
        let high_task = make_task("high priority", 6, 24, false)
            .with_priority(TaskPriority::High);
        let decision = scheduler.schedule_task(&high_task);

        assert!(matches!(decision, ScheduleDecision::Queue { .. }));
    }

    #[test]
    fn test_free_ratio() {
        let mut node = make_node("test", 16, 64, false);
        assert!((node.free_ratio() - 1.0).abs() < f64::EPSILON);

        let req = ResourceRequirements {
            cpu_cores: 8,
            memory_gib: 32,
            gpu_required: false,
            estimated_duration: Duration::from_secs(60),
        };
        node.allocate(Uuid::new_v4(), &req, TaskPriority::Normal);
        assert!((node.free_ratio() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_offline_node_excluded() {
        let mut scheduler = Scheduler::new(PlacementPolicy::BinPack);
        let mut cap = make_node("james", 16, 64, false);
        cap.online = false;
        scheduler.add_node(cap);

        let task = make_task("build", 4, 8, false);
        let decision = scheduler.schedule_task(&task);

        match &decision {
            ScheduleDecision::Queue { reason } => {
                assert!(reason.contains("no nodes online"));
            }
            other => panic!("Expected Queue, got {:?}", other),
        }
    }

    #[test]
    fn test_multiple_allocations_deplete_resources() {
        let mut scheduler = Scheduler::new(PlacementPolicy::BinPack);
        scheduler.add_node(make_node("james", 16, 64, false));

        // Schedule 4 tasks, each using 4 CPU / 16 GiB
        for i in 0..4 {
            let task = make_task(&format!("task-{}", i), 4, 16, false);
            let decision = scheduler.schedule_task(&task);
            assert!(decision.is_assigned(), "task {} should be assigned", i);
        }

        // 5th task should be queued (no capacity left)
        let task = make_task("task-overflow", 4, 16, false);
        let decision = scheduler.schedule_task(&task);
        assert!(matches!(decision, ScheduleDecision::Queue { .. }));
    }
}
