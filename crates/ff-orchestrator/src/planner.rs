//! Execution planner — DAG-based dependency resolution and stage scheduling.
//!
//! Given a [`TaskDecomposition`](crate::TaskDecomposition) with subtask
//! dependencies, the planner produces an [`ExecutionPlan`] — an ordered
//! sequence of stages where each stage contains subtasks that can run
//! in parallel.
//!
//! The planner performs topological sorting on the dependency DAG, groups
//! independent subtasks into stages, and validates the graph for cycles.

use std::collections::{HashMap, HashSet, VecDeque};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::decomposer::{SubTask, TaskDecomposition};

// ─── Plan Node ───────────────────────────────────────────────────────────────

/// A node in the execution plan — wraps a subtask with scheduling metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanNode {
    /// The subtask this node represents.
    pub subtask_id: Uuid,
    /// Which stage (wave) this node is in (0-indexed).
    pub stage: usize,
    /// IDs of subtasks that must complete before this one.
    pub dependencies: Vec<Uuid>,
    /// IDs of subtasks that depend on this one.
    pub dependents: Vec<Uuid>,
    /// Estimated duration (ms) — for scheduling heuristics.
    pub estimated_duration_ms: Option<u64>,
}

// ─── Plan Stage ──────────────────────────────────────────────────────────────

/// A stage (wave) in the execution plan.
///
/// All subtasks in a stage can execute in parallel — none depend on each other.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStage {
    /// Stage index (0 = first wave).
    pub index: usize,
    /// Subtask IDs in this stage.
    pub subtask_ids: Vec<Uuid>,
    /// Maximum estimated duration across subtasks in this stage (ms).
    ///
    /// The stage completes when its slowest subtask finishes.
    pub estimated_duration_ms: Option<u64>,
}

impl PlanStage {
    /// Number of subtasks that can run in parallel in this stage.
    pub fn parallelism(&self) -> usize {
        self.subtask_ids.len()
    }
}

// ─── Execution Plan ──────────────────────────────────────────────────────────

/// The complete execution plan for a decomposed task.
///
/// Stages are executed sequentially; subtasks *within* a stage run in parallel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionPlan {
    /// Unique plan ID.
    pub id: Uuid,
    /// The decomposition this plan was built from.
    pub decomposition_id: Uuid,
    /// Ordered stages (waves) of execution.
    pub stages: Vec<PlanStage>,
    /// Detailed node info for each subtask.
    pub nodes: HashMap<Uuid, PlanNode>,
    /// When this plan was generated.
    pub created_at: DateTime<Utc>,
    /// Total estimated duration (sum of stage durations).
    pub estimated_total_ms: Option<u64>,
}

impl ExecutionPlan {
    /// Total number of subtasks across all stages.
    pub fn total_subtasks(&self) -> usize {
        self.stages.iter().map(|s| s.subtask_ids.len()).sum()
    }

    /// Number of stages (sequential waves).
    pub fn num_stages(&self) -> usize {
        self.stages.len()
    }

    /// Maximum parallelism across all stages.
    pub fn max_parallelism(&self) -> usize {
        self.stages
            .iter()
            .map(|s| s.parallelism())
            .max()
            .unwrap_or(0)
    }

    /// Get the stage a subtask belongs to.
    pub fn stage_of(&self, subtask_id: Uuid) -> Option<usize> {
        self.nodes.get(&subtask_id).map(|n| n.stage)
    }

    /// Get subtask IDs in execution order (stage by stage, left to right).
    pub fn execution_order(&self) -> Vec<Uuid> {
        self.stages
            .iter()
            .flat_map(|s| s.subtask_ids.iter().copied())
            .collect()
    }
}

// ─── Planner Errors ──────────────────────────────────────────────────────────

/// Errors that can occur during plan construction.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PlanError {
    #[error("dependency cycle detected involving subtask {0}")]
    CycleDetected(Uuid),

    #[error("subtask {0} depends on unknown subtask {1}")]
    MissingDependency(Uuid, Uuid),

    #[error("empty decomposition — nothing to plan")]
    EmptyDecomposition,
}

// ─── Planner ─────────────────────────────────────────────────────────────────

/// The execution planner.
///
/// Takes a decomposition and builds an optimal execution plan via
/// topological sort + level assignment (Kahn's algorithm).
pub struct Planner;

impl Planner {
    /// Build an execution plan from a task decomposition.
    ///
    /// # Errors
    ///
    /// Returns [`PlanError::CycleDetected`] if the dependency graph has cycles,
    /// [`PlanError::MissingDependency`] if a subtask references a non-existent
    /// dependency, or [`PlanError::EmptyDecomposition`] if there are no subtasks.
    pub fn plan(decomposition: &TaskDecomposition) -> Result<ExecutionPlan, PlanError> {
        if decomposition.is_empty() {
            return Err(PlanError::EmptyDecomposition);
        }

        let subtasks = &decomposition.subtasks;

        // Build ID → subtask lookup
        let id_set: HashSet<Uuid> = subtasks.iter().map(|s| s.id).collect();

        // Validate all dependencies exist
        for st in subtasks {
            for dep in &st.depends_on {
                if !id_set.contains(dep) {
                    return Err(PlanError::MissingDependency(st.id, *dep));
                }
            }
        }

        // Build adjacency list and in-degree map (Kahn's algorithm)
        let mut in_degree: HashMap<Uuid, usize> = HashMap::new();
        let mut dependents: HashMap<Uuid, Vec<Uuid>> = HashMap::new();

        for st in subtasks {
            in_degree.entry(st.id).or_insert(0);
            dependents.entry(st.id).or_default();
        }

        for st in subtasks {
            *in_degree.entry(st.id).or_insert(0) += st.depends_on.len();
            for dep in &st.depends_on {
                dependents.entry(*dep).or_default().push(st.id);
            }
        }

        // Topological sort with level assignment
        let mut queue: VecDeque<Uuid> = VecDeque::new();
        let mut levels: HashMap<Uuid, usize> = HashMap::new();

        // Seed with root nodes (in-degree 0)
        for st in subtasks {
            if in_degree[&st.id] == 0 {
                queue.push_back(st.id);
                levels.insert(st.id, 0);
            }
        }

        let mut processed = 0usize;

        while let Some(current) = queue.pop_front() {
            processed += 1;
            let current_level = levels[&current];

            if let Some(deps) = dependents.get(&current) {
                for &dep_id in deps {
                    let deg = in_degree.get_mut(&dep_id).unwrap();
                    *deg -= 1;
                    // Level = max(level of all dependencies) + 1
                    let new_level = current_level + 1;
                    let entry = levels.entry(dep_id).or_insert(0);
                    if new_level > *entry {
                        *entry = new_level;
                    }
                    if *deg == 0 {
                        queue.push_back(dep_id);
                    }
                }
            }
        }

        // Cycle detection: if we didn't process all nodes, there's a cycle
        if processed < subtasks.len() {
            // Find a node that wasn't processed
            let unprocessed = subtasks
                .iter()
                .find(|s| !levels.contains_key(&s.id) || in_degree[&s.id] > 0)
                .map(|s| s.id)
                .unwrap_or(Uuid::nil());
            return Err(PlanError::CycleDetected(unprocessed));
        }

        // Group by level into stages
        let max_level = levels.values().max().copied().unwrap_or(0);
        let mut stages: Vec<PlanStage> = Vec::with_capacity(max_level + 1);

        for level in 0..=max_level {
            let subtask_ids: Vec<Uuid> = subtasks
                .iter()
                .filter(|s| levels.get(&s.id) == Some(&level))
                .map(|s| s.id)
                .collect();

            if !subtask_ids.is_empty() {
                stages.push(PlanStage {
                    index: level,
                    subtask_ids,
                    estimated_duration_ms: None,
                });
            }
        }

        // Build PlanNode map
        let mut plan_nodes: HashMap<Uuid, PlanNode> = HashMap::new();
        for st in subtasks {
            let deps_of_this: Vec<Uuid> = dependents.get(&st.id).cloned().unwrap_or_default();

            plan_nodes.insert(
                st.id,
                PlanNode {
                    subtask_id: st.id,
                    stage: levels[&st.id],
                    dependencies: st.depends_on.clone(),
                    dependents: deps_of_this,
                    estimated_duration_ms: estimate_duration(st),
                },
            );
        }

        // Estimate stage durations (max of subtask durations in each stage)
        for stage in &mut stages {
            let max_dur = stage
                .subtask_ids
                .iter()
                .filter_map(|id| plan_nodes.get(id)?.estimated_duration_ms)
                .max();
            stage.estimated_duration_ms = max_dur;
        }

        let estimated_total_ms: Option<u64> = {
            let total: u64 = stages.iter().filter_map(|s| s.estimated_duration_ms).sum();
            if total > 0 { Some(total) } else { None }
        };

        Ok(ExecutionPlan {
            id: Uuid::new_v4(),
            decomposition_id: decomposition.id,
            stages,
            nodes: plan_nodes,
            created_at: Utc::now(),
            estimated_total_ms,
        })
    }
}

/// Rough duration estimate based on subtask complexity.
fn estimate_duration(subtask: &SubTask) -> Option<u64> {
    // Heuristic: ~1s per complexity point for Tier1, ~3s for Tier3+
    let base_ms = match subtask.task_type.suggested_ideal_tier() {
        ff_core::Tier::Tier1 => 500,
        ff_core::Tier::Tier2 => 2000,
        ff_core::Tier::Tier3 => 5000,
        ff_core::Tier::Tier4 => 15000,
    };
    Some(base_ms * subtask.complexity as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decomposer::{SubTask, SubTaskType, TaskDecomposition};

    /// Helper: make a linear chain A → B → C.
    fn make_chain_decomposition() -> TaskDecomposition {
        let mut d = TaskDecomposition::new("chain test");

        let a = SubTask::new(0, "A", "research stuff", SubTaskType::Research);
        let a_id = a.id;

        let b = SubTask::new(1, "B", "implement code", SubTaskType::Code).depends_on(a_id);
        let b_id = b.id;

        let c = SubTask::new(2, "C", "review code", SubTaskType::Review).depends_on(b_id);

        d.add_subtask(a);
        d.add_subtask(b);
        d.add_subtask(c);
        d
    }

    /// Helper: make a diamond A → (B, C) → D.
    fn make_diamond_decomposition() -> TaskDecomposition {
        let mut d = TaskDecomposition::new("diamond test");

        let a = SubTask::new(0, "A", "plan", SubTaskType::Planning);
        let a_id = a.id;

        let b = SubTask::new(1, "B", "research", SubTaskType::Research).depends_on(a_id);
        let b_id = b.id;

        let c = SubTask::new(2, "C", "code", SubTaskType::Code).depends_on(a_id);
        let c_id = c.id;

        let dd = SubTask::new(3, "D", "review", SubTaskType::Review)
            .depends_on(b_id)
            .depends_on(c_id);

        d.add_subtask(a);
        d.add_subtask(b);
        d.add_subtask(c);
        d.add_subtask(dd);
        d
    }

    #[test]
    fn test_plan_chain() {
        let decomp = make_chain_decomposition();
        let plan = Planner::plan(&decomp).unwrap();

        assert_eq!(plan.num_stages(), 3, "A → B → C = 3 stages");
        assert_eq!(plan.max_parallelism(), 1, "chain = no parallelism");
        assert_eq!(plan.total_subtasks(), 3);
    }

    #[test]
    fn test_plan_diamond() {
        let decomp = make_diamond_decomposition();
        let plan = Planner::plan(&decomp).unwrap();

        assert_eq!(plan.num_stages(), 3, "A → (B||C) → D = 3 stages");
        assert_eq!(plan.max_parallelism(), 2, "B and C run in parallel");

        // Stage 0: A, Stage 1: B+C, Stage 2: D
        assert_eq!(plan.stages[0].subtask_ids.len(), 1);
        assert_eq!(plan.stages[1].subtask_ids.len(), 2);
        assert_eq!(plan.stages[2].subtask_ids.len(), 1);
    }

    #[test]
    fn test_plan_all_parallel() {
        let mut decomp = TaskDecomposition::new("parallel test");
        for i in 0..5 {
            decomp.add_subtask(SubTask::new(
                i,
                &format!("task {i}"),
                "do stuff",
                SubTaskType::FastLookup,
            ));
        }

        let plan = Planner::plan(&decomp).unwrap();
        assert_eq!(plan.num_stages(), 1, "all independent = 1 stage");
        assert_eq!(plan.max_parallelism(), 5);
    }

    #[test]
    fn test_plan_cycle_detection() {
        // A depends on C, B depends on A, C depends on B → cycle
        let mut decomp = TaskDecomposition::new("cycle test");

        let mut a = SubTask::new(0, "A", "a", SubTaskType::Code);
        let mut b = SubTask::new(1, "B", "b", SubTaskType::Code);
        let mut c = SubTask::new(2, "C", "c", SubTaskType::Code);

        // Create the cycle: A→B→C→A
        let a_id = a.id;
        let b_id = b.id;
        let c_id = c.id;

        a.depends_on = vec![c_id];
        b.depends_on = vec![a_id];
        c.depends_on = vec![b_id];

        decomp.add_subtask(a);
        decomp.add_subtask(b);
        decomp.add_subtask(c);

        let result = Planner::plan(&decomp);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), PlanError::CycleDetected(_)));
    }

    #[test]
    fn test_plan_missing_dependency() {
        let mut decomp = TaskDecomposition::new("missing dep test");
        let ghost_id = Uuid::new_v4();
        let st = SubTask::new(0, "orphan", "stuff", SubTaskType::Code).depends_on(ghost_id);
        decomp.add_subtask(st);

        let result = Planner::plan(&decomp);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            PlanError::MissingDependency(_, _)
        ));
    }

    #[test]
    fn test_plan_empty() {
        let decomp = TaskDecomposition::new("empty");
        let result = Planner::plan(&decomp);
        assert!(matches!(result.unwrap_err(), PlanError::EmptyDecomposition));
    }

    #[test]
    fn test_execution_order() {
        let decomp = make_chain_decomposition();
        let plan = Planner::plan(&decomp).unwrap();
        let order = plan.execution_order();

        // A must come before B, B before C
        let pos_a = order
            .iter()
            .position(|&id| id == decomp.subtasks[0].id)
            .unwrap();
        let pos_b = order
            .iter()
            .position(|&id| id == decomp.subtasks[1].id)
            .unwrap();
        let pos_c = order
            .iter()
            .position(|&id| id == decomp.subtasks[2].id)
            .unwrap();
        assert!(pos_a < pos_b);
        assert!(pos_b < pos_c);
    }

    #[test]
    fn test_stage_of() {
        let decomp = make_diamond_decomposition();
        let plan = Planner::plan(&decomp).unwrap();

        // A is stage 0
        assert_eq!(plan.stage_of(decomp.subtasks[0].id), Some(0));
        // B and C are stage 1
        assert_eq!(plan.stage_of(decomp.subtasks[1].id), Some(1));
        assert_eq!(plan.stage_of(decomp.subtasks[2].id), Some(1));
        // D is stage 2
        assert_eq!(plan.stage_of(decomp.subtasks[3].id), Some(2));
    }

    #[test]
    fn test_estimated_duration() {
        let decomp = make_chain_decomposition();
        let plan = Planner::plan(&decomp).unwrap();
        assert!(plan.estimated_total_ms.is_some());
        assert!(plan.estimated_total_ms.unwrap() > 0);
    }
}
