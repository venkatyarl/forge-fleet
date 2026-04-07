//! DAG data structure for pipeline execution.
//!
//! `PipelineGraph` stores steps as nodes and dependency edges between them.
//! It provides topological sorting, cycle detection, and queries for
//! which steps are ready to execute.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::error::PipelineError;
use crate::step::{Step, StepId, StepStatus};

// ─── Pipeline Graph ──────────────────────────────────────────────────────────

/// A directed acyclic graph of pipeline steps.
///
/// Nodes are `Step` values keyed by `StepId`.
/// Edges go from dependency → dependent (i.e. "A must finish before B").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineGraph {
    /// All steps keyed by their ID.
    steps: HashMap<StepId, Step>,
    /// Insertion order so iteration is deterministic.
    order: Vec<StepId>,
    /// Forward edges: step → set of steps that depend on it.
    dependents: HashMap<StepId, HashSet<StepId>>,
    /// Reverse edges: step → set of steps it depends on.
    dependencies: HashMap<StepId, HashSet<StepId>>,
}

impl PipelineGraph {
    /// Create an empty graph.
    pub fn new() -> Self {
        Self {
            steps: HashMap::new(),
            order: Vec::new(),
            dependents: HashMap::new(),
            dependencies: HashMap::new(),
        }
    }

    // ── Mutation ──────────────────────────────────────────────────────────

    /// Add a step to the graph. Errors if a step with that ID already exists.
    pub fn add_step(&mut self, step: Step) -> Result<(), PipelineError> {
        if self.steps.contains_key(&step.id) {
            return Err(PipelineError::DuplicateStep(step.id.clone()));
        }
        let id = step.id.clone();
        self.steps.insert(id.clone(), step);
        self.order.push(id.clone());
        self.dependents.entry(id.clone()).or_default();
        self.dependencies.entry(id).or_default();
        Ok(())
    }

    /// Remove a step and all edges touching it.
    pub fn remove_step(&mut self, id: &StepId) -> Result<Step, PipelineError> {
        let step = self
            .steps
            .remove(id)
            .ok_or_else(|| PipelineError::StepNotFound(id.clone()))?;

        self.order.retain(|x| x != id);

        // Remove from other steps' dependent sets.
        if let Some(deps) = self.dependencies.remove(id) {
            for dep in &deps {
                if let Some(fwd) = self.dependents.get_mut(dep) {
                    fwd.remove(id);
                }
            }
        }

        // Remove from other steps' dependency sets.
        if let Some(fwd) = self.dependents.remove(id) {
            for dependent in &fwd {
                if let Some(rev) = self.dependencies.get_mut(dependent) {
                    rev.remove(id);
                }
            }
        }

        Ok(step)
    }

    /// Add a dependency edge: `dependent` depends on `dependency`.
    ///
    /// Both steps must already exist. Returns error on missing step or
    /// if the edge would create a cycle.
    pub fn add_dependency(
        &mut self,
        dependent: &StepId,
        dependency: &StepId,
    ) -> Result<(), PipelineError> {
        if !self.steps.contains_key(dependent) {
            return Err(PipelineError::StepNotFound(dependent.clone()));
        }
        if !self.steps.contains_key(dependency) {
            return Err(PipelineError::StepNotFound(dependency.clone()));
        }
        if dependent == dependency {
            return Err(PipelineError::CycleDetected);
        }

        // Tentatively add the edge.
        self.dependencies
            .entry(dependent.clone())
            .or_default()
            .insert(dependency.clone());
        self.dependents
            .entry(dependency.clone())
            .or_default()
            .insert(dependent.clone());

        // Check for cycles.
        if self.has_cycle() {
            // Roll back.
            self.dependencies
                .get_mut(dependent)
                .unwrap()
                .remove(dependency);
            self.dependents
                .get_mut(dependency)
                .unwrap()
                .remove(dependent);
            return Err(PipelineError::CycleDetected);
        }

        Ok(())
    }

    // ── Queries ──────────────────────────────────────────────────────────

    /// Get a step by ID.
    pub fn get_step(&self, id: &StepId) -> Option<&Step> {
        self.steps.get(id)
    }

    /// All step IDs in insertion order.
    pub fn step_ids(&self) -> &[StepId] {
        &self.order
    }

    /// Number of steps.
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// Is the graph empty?
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Get the direct dependencies of a step.
    pub fn dependencies_of(&self, id: &StepId) -> HashSet<StepId> {
        self.dependencies.get(id).cloned().unwrap_or_default()
    }

    /// Get the direct dependents of a step.
    pub fn dependents_of(&self, id: &StepId) -> HashSet<StepId> {
        self.dependents.get(id).cloned().unwrap_or_default()
    }

    /// Return step IDs that are ready to execute: all their dependencies are
    /// in a terminal-success state according to the supplied status map.
    ///
    /// A step is ready when:
    /// - It is currently `Pending`
    /// - All its dependencies are `Succeeded` (or the step `allow_failure` is
    ///   true for the dependency, and the dependency is terminal).
    pub fn ready_steps(&self, statuses: &HashMap<StepId, StepStatus>) -> Vec<StepId> {
        let mut ready = Vec::new();
        for id in &self.order {
            let status = statuses.get(id).copied().unwrap_or(StepStatus::Pending);
            if status != StepStatus::Pending {
                continue;
            }
            let deps = self.dependencies_of(id);
            let all_satisfied = deps.iter().all(|dep_id| {
                let dep_status = statuses.get(dep_id).copied().unwrap_or(StepStatus::Pending);
                if dep_status == StepStatus::Succeeded {
                    return true;
                }
                // If the dependency allows failure and is terminal, count as satisfied.
                if let Some(dep_step) = self.steps.get(dep_id)
                    && dep_step.config.allow_failure
                    && dep_status.is_terminal()
                {
                    return true;
                }
                false
            });
            if all_satisfied {
                ready.push(id.clone());
            }
        }
        ready
    }

    /// Steps whose dependencies have failed (and don't allow failure) — these
    /// should be skipped.
    pub fn skippable_steps(&self, statuses: &HashMap<StepId, StepStatus>) -> Vec<StepId> {
        let mut skippable = Vec::new();
        for id in &self.order {
            let status = statuses.get(id).copied().unwrap_or(StepStatus::Pending);
            if status != StepStatus::Pending {
                continue;
            }
            let deps = self.dependencies_of(id);
            let any_hard_fail = deps.iter().any(|dep_id| {
                let dep_status = statuses.get(dep_id).copied().unwrap_or(StepStatus::Pending);
                let failed = matches!(
                    dep_status,
                    StepStatus::Failed | StepStatus::TimedOut | StepStatus::Skipped
                );
                if !failed {
                    return false;
                }
                // Only a hard fail if the dependency does NOT allow failure.
                if let Some(dep_step) = self.steps.get(dep_id) {
                    !dep_step.config.allow_failure
                } else {
                    true
                }
            });
            if any_hard_fail {
                skippable.push(id.clone());
            }
        }
        skippable
    }

    // ── Topological Sort ─────────────────────────────────────────────────

    /// Return a topological ordering of step IDs (Kahn's algorithm).
    /// Returns `Err(CycleDetected)` if the graph has a cycle.
    pub fn topological_sort(&self) -> Result<Vec<StepId>, PipelineError> {
        let mut in_degree: HashMap<StepId, usize> = HashMap::new();
        for id in &self.order {
            in_degree.insert(id.clone(), self.dependencies.get(id).map_or(0, |s| s.len()));
        }

        let mut queue: VecDeque<StepId> = VecDeque::new();
        for (id, &deg) in &in_degree {
            if deg == 0 {
                queue.push_back(id.clone());
            }
        }

        let mut sorted = Vec::with_capacity(self.order.len());
        while let Some(id) = queue.pop_front() {
            sorted.push(id.clone());
            if let Some(fwd) = self.dependents.get(&id) {
                for dep in fwd {
                    if let Some(deg) = in_degree.get_mut(dep) {
                        *deg -= 1;
                        if *deg == 0 {
                            queue.push_back(dep.clone());
                        }
                    }
                }
            }
        }

        if sorted.len() != self.order.len() {
            return Err(PipelineError::CycleDetected);
        }

        Ok(sorted)
    }

    // ── Cycle Detection ──────────────────────────────────────────────────

    /// Returns true if the graph contains a cycle (DFS-based).
    pub fn has_cycle(&self) -> bool {
        #[derive(Clone, Copy, PartialEq)]
        enum Color {
            White,
            Gray,
            Black,
        }

        let mut color: HashMap<&StepId, Color> = HashMap::new();
        for id in self.steps.keys() {
            color.insert(id, Color::White);
        }

        fn dfs<'a>(
            node: &'a StepId,
            dependents: &'a HashMap<StepId, HashSet<StepId>>,
            color: &mut HashMap<&'a StepId, Color>,
        ) -> bool {
            color.insert(node, Color::Gray);
            if let Some(neighbours) = dependents.get(node) {
                for n in neighbours {
                    match color.get(n) {
                        Some(Color::Gray) => return true, // back edge → cycle
                        Some(Color::White) => {
                            if dfs(n, dependents, color) {
                                return true;
                            }
                        }
                        _ => {}
                    }
                }
            }
            color.insert(node, Color::Black);
            false
        }

        for id in self.steps.keys() {
            if color[id] == Color::White && dfs(id, &self.dependents, &mut color) {
                return true;
            }
        }

        false
    }

    /// Recursively collect all transitive dependents of a step.
    pub fn all_dependents(&self, id: &StepId) -> HashSet<StepId> {
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(id.clone());
        while let Some(current) = queue.pop_front() {
            if let Some(deps) = self.dependents.get(&current) {
                for d in deps {
                    if visited.insert(d.clone()) {
                        queue.push_back(d.clone());
                    }
                }
            }
        }
        visited
    }
}

impl Default for PipelineGraph {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::step::Step;

    fn shell_step(id: &str) -> Step {
        Step::shell(id, id, format!("echo {id}"))
    }

    #[test]
    fn empty_graph() {
        let g = PipelineGraph::new();
        assert!(g.is_empty());
        assert_eq!(g.len(), 0);
    }

    #[test]
    fn add_and_get_step() {
        let mut g = PipelineGraph::new();
        g.add_step(shell_step("a")).unwrap();
        assert_eq!(g.len(), 1);
        assert!(g.get_step(&StepId::new("a")).is_some());
    }

    #[test]
    fn duplicate_step_error() {
        let mut g = PipelineGraph::new();
        g.add_step(shell_step("a")).unwrap();
        let err = g.add_step(shell_step("a")).unwrap_err();
        assert!(matches!(err, PipelineError::DuplicateStep(_)));
    }

    #[test]
    fn add_dependency() {
        let mut g = PipelineGraph::new();
        g.add_step(shell_step("a")).unwrap();
        g.add_step(shell_step("b")).unwrap();
        g.add_dependency(&"b".into(), &"a".into()).unwrap();

        assert!(g.dependencies_of(&"b".into()).contains(&"a".into()));
        assert!(g.dependents_of(&"a".into()).contains(&"b".into()));
    }

    #[test]
    fn dependency_missing_step() {
        let mut g = PipelineGraph::new();
        g.add_step(shell_step("a")).unwrap();
        let err = g
            .add_dependency(&"a".into(), &"missing".into())
            .unwrap_err();
        assert!(matches!(err, PipelineError::StepNotFound(_)));
    }

    #[test]
    fn self_dependency_cycle() {
        let mut g = PipelineGraph::new();
        g.add_step(shell_step("a")).unwrap();
        let err = g.add_dependency(&"a".into(), &"a".into()).unwrap_err();
        assert!(matches!(err, PipelineError::CycleDetected));
    }

    #[test]
    fn cycle_detection_triangle() {
        let mut g = PipelineGraph::new();
        g.add_step(shell_step("a")).unwrap();
        g.add_step(shell_step("b")).unwrap();
        g.add_step(shell_step("c")).unwrap();

        g.add_dependency(&"b".into(), &"a".into()).unwrap(); // a → b
        g.add_dependency(&"c".into(), &"b".into()).unwrap(); // b → c
        let err = g.add_dependency(&"a".into(), &"c".into()).unwrap_err(); // c → a would cycle
        assert!(matches!(err, PipelineError::CycleDetected));
    }

    #[test]
    fn topological_sort_linear() {
        let mut g = PipelineGraph::new();
        g.add_step(shell_step("a")).unwrap();
        g.add_step(shell_step("b")).unwrap();
        g.add_step(shell_step("c")).unwrap();
        g.add_dependency(&"b".into(), &"a".into()).unwrap();
        g.add_dependency(&"c".into(), &"b".into()).unwrap();

        let sorted = g.topological_sort().unwrap();
        let pos = |id: &str| sorted.iter().position(|x| x.0 == id).unwrap();
        assert!(pos("a") < pos("b"));
        assert!(pos("b") < pos("c"));
    }

    #[test]
    fn topological_sort_diamond() {
        // a → b, a → c, b → d, c → d
        let mut g = PipelineGraph::new();
        g.add_step(shell_step("a")).unwrap();
        g.add_step(shell_step("b")).unwrap();
        g.add_step(shell_step("c")).unwrap();
        g.add_step(shell_step("d")).unwrap();
        g.add_dependency(&"b".into(), &"a".into()).unwrap();
        g.add_dependency(&"c".into(), &"a".into()).unwrap();
        g.add_dependency(&"d".into(), &"b".into()).unwrap();
        g.add_dependency(&"d".into(), &"c".into()).unwrap();

        let sorted = g.topological_sort().unwrap();
        let pos = |id: &str| sorted.iter().position(|x| x.0 == id).unwrap();
        assert!(pos("a") < pos("b"));
        assert!(pos("a") < pos("c"));
        assert!(pos("b") < pos("d"));
        assert!(pos("c") < pos("d"));
    }

    #[test]
    fn ready_steps_initial() {
        let mut g = PipelineGraph::new();
        g.add_step(shell_step("a")).unwrap();
        g.add_step(shell_step("b")).unwrap();
        g.add_dependency(&"b".into(), &"a".into()).unwrap();

        let statuses = HashMap::new();
        let ready = g.ready_steps(&statuses);
        assert_eq!(ready, vec![StepId::new("a")]);
    }

    #[test]
    fn ready_steps_after_completion() {
        let mut g = PipelineGraph::new();
        g.add_step(shell_step("a")).unwrap();
        g.add_step(shell_step("b")).unwrap();
        g.add_dependency(&"b".into(), &"a".into()).unwrap();

        let mut statuses = HashMap::new();
        statuses.insert(StepId::new("a"), StepStatus::Succeeded);

        let ready = g.ready_steps(&statuses);
        assert_eq!(ready, vec![StepId::new("b")]);
    }

    #[test]
    fn skippable_on_failure() {
        let mut g = PipelineGraph::new();
        g.add_step(shell_step("a")).unwrap();
        g.add_step(shell_step("b")).unwrap();
        g.add_dependency(&"b".into(), &"a".into()).unwrap();

        let mut statuses = HashMap::new();
        statuses.insert(StepId::new("a"), StepStatus::Failed);

        let skippable = g.skippable_steps(&statuses);
        assert_eq!(skippable, vec![StepId::new("b")]);
    }

    #[test]
    fn remove_step_cleans_edges() {
        let mut g = PipelineGraph::new();
        g.add_step(shell_step("a")).unwrap();
        g.add_step(shell_step("b")).unwrap();
        g.add_step(shell_step("c")).unwrap();
        g.add_dependency(&"b".into(), &"a".into()).unwrap();
        g.add_dependency(&"c".into(), &"b".into()).unwrap();

        g.remove_step(&"b".into()).unwrap();
        assert_eq!(g.len(), 2);
        assert!(g.dependents_of(&"a".into()).is_empty());
        assert!(g.dependencies_of(&"c".into()).is_empty());
    }

    #[test]
    fn all_dependents_transitive() {
        let mut g = PipelineGraph::new();
        g.add_step(shell_step("a")).unwrap();
        g.add_step(shell_step("b")).unwrap();
        g.add_step(shell_step("c")).unwrap();
        g.add_dependency(&"b".into(), &"a".into()).unwrap();
        g.add_dependency(&"c".into(), &"b".into()).unwrap();

        let deps = g.all_dependents(&"a".into());
        assert!(deps.contains(&"b".into()));
        assert!(deps.contains(&"c".into()));
    }
}
