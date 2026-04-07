//! Placement engine for ForgeFleet scheduling.
//!
//! Determines which node is the best fit for a task based on configurable
//! policies: bin packing, spread, and affinity-based placement.
//!
//! # Policies
//!
//! - **BinPack** — Fill nodes as densely as possible (fewer active nodes, save power)
//! - **Spread** — Distribute tasks evenly across nodes (maximize redundancy)
//! - **Affinity** — Prefer nodes matching workload preferences from fleet.toml

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::scheduler::{NodeCapacity, ScheduledTask};

// ─── Placement Policy ────────────────────────────────────────────────────────

/// How the placement engine scores nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementPolicy {
    /// Fill nodes as densely as possible (prefer nodes with *least* free resources
    /// that can still fit the task). Reduces active node count.
    BinPack,
    /// Distribute tasks evenly across nodes (prefer nodes with *most* free resources).
    /// Maximizes fault tolerance and headroom.
    Spread,
    /// Prefer nodes whose `preferred_workloads` match the task's workload type.
    /// Falls back to Spread scoring for non-matching nodes.
    Affinity,
}

impl Default for PlacementPolicy {
    fn default() -> Self {
        Self::Spread
    }
}

// ─── Anti-Affinity Rule ──────────────────────────────────────────────────────

/// Anti-affinity constraint: tasks from the same project should avoid
/// co-location on the same node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AntiAffinityRule {
    /// Project name this rule applies to.
    pub project: String,
    /// Nodes that already have a task from this project.
    pub occupied_nodes: HashSet<String>,
}

impl AntiAffinityRule {
    pub fn new(project: impl Into<String>) -> Self {
        Self {
            project: project.into(),
            occupied_nodes: HashSet::new(),
        }
    }

    /// Record that a task from this project is on the given node.
    pub fn mark_node(&mut self, node_name: impl Into<String>) {
        self.occupied_nodes.insert(node_name.into());
    }

    /// Check if the node already has a task from this project.
    pub fn is_occupied(&self, node_name: &str) -> bool {
        self.occupied_nodes.contains(node_name)
    }
}

// ─── Node Preference (from fleet.toml) ───────────────────────────────────────

/// Workload preferences for a node, read from fleet.toml's
/// `[nodes.<name>.models.<slug>]` preferred_workloads field.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeWorkloadPreference {
    /// Node name.
    pub node_name: String,
    /// Workload types this node prefers (e.g. ["coding", "review", "build"]).
    pub preferred_workloads: Vec<String>,
}

impl NodeWorkloadPreference {
    /// Check if this node prefers the given workload type.
    pub fn prefers(&self, workload_type: &str) -> bool {
        self.preferred_workloads
            .iter()
            .any(|w| w.eq_ignore_ascii_case(workload_type))
    }
}

// ─── Placement Engine ────────────────────────────────────────────────────────

/// Scores nodes for task placement using the configured policy.
pub struct PlacementEngine {
    /// Active placement policy.
    policy: PlacementPolicy,
    /// Per-project anti-affinity rules.
    anti_affinity: Vec<AntiAffinityRule>,
    /// Per-node workload preferences (from fleet.toml).
    node_preferences: Vec<NodeWorkloadPreference>,
}

impl PlacementEngine {
    /// Create a new placement engine with the given policy.
    pub fn new(policy: PlacementPolicy) -> Self {
        Self {
            policy,
            anti_affinity: Vec::new(),
            node_preferences: Vec::new(),
        }
    }

    /// Set the placement policy.
    pub fn set_policy(&mut self, policy: PlacementPolicy) {
        self.policy = policy;
    }

    /// Get the current policy.
    pub fn policy(&self) -> PlacementPolicy {
        self.policy
    }

    /// Add an anti-affinity rule for a project.
    pub fn add_anti_affinity(&mut self, rule: AntiAffinityRule) {
        self.anti_affinity.push(rule);
    }

    /// Update anti-affinity: mark that a project has a task on a node.
    pub fn mark_project_on_node(&mut self, project: &str, node_name: &str) {
        if let Some(rule) = self.anti_affinity.iter_mut().find(|r| r.project == project) {
            rule.mark_node(node_name);
        } else {
            let mut rule = AntiAffinityRule::new(project);
            rule.mark_node(node_name);
            self.anti_affinity.push(rule);
        }
    }

    /// Clear anti-affinity state for a project on a node (when task completes).
    pub fn clear_project_on_node(&mut self, project: &str, node_name: &str) {
        if let Some(rule) = self.anti_affinity.iter_mut().find(|r| r.project == project) {
            rule.occupied_nodes.remove(node_name);
        }
    }

    /// Set node workload preferences (typically loaded from fleet.toml).
    pub fn set_node_preferences(&mut self, preferences: Vec<NodeWorkloadPreference>) {
        self.node_preferences = preferences;
    }

    /// Add a single node's workload preference.
    pub fn add_node_preference(&mut self, pref: NodeWorkloadPreference) {
        self.node_preferences.push(pref);
    }

    /// Score a node for a task. Higher score = better fit.
    ///
    /// The score is in [0.0, 1.0] with bonus multipliers for affinity
    /// and anti-affinity adjustments.
    pub fn score_node(&self, task: &ScheduledTask, node: &NodeCapacity) -> f64 {
        if !node.online || !node.can_fit(&task.requirements) {
            return 0.0;
        }

        // Base score from placement policy
        let base_score = match self.policy {
            PlacementPolicy::BinPack => self.score_bin_pack(node),
            PlacementPolicy::Spread => self.score_spread(node),
            PlacementPolicy::Affinity => self.score_affinity(task, node),
        };

        // Anti-affinity penalty: reduce score if project already has a task on this node
        let anti_affinity_factor = if let Some(project) = &task.project {
            if self.is_project_on_node(project, &node.node_name) {
                0.5 // Halve the score — still possible, but strongly disfavored
            } else {
                1.0
            }
        } else {
            1.0
        };

        // Preferred node bonus (from task.preferred_nodes)
        let preferred_bonus = if task.preferred_nodes.contains(&node.node_name) {
            1.2
        } else {
            1.0
        };

        let final_score = base_score * anti_affinity_factor * preferred_bonus;

        debug!(
            node = %node.node_name,
            policy = ?self.policy,
            base = base_score,
            anti_affinity = anti_affinity_factor,
            preferred = preferred_bonus,
            final_score = final_score,
            "node scored"
        );

        final_score
    }

    /// BinPack: prefer nodes that are already busy (least free resources that can still fit).
    /// This packs tasks tightly, leaving other nodes idle for power savings.
    fn score_bin_pack(&self, node: &NodeCapacity) -> f64 {
        // Invert free ratio: nodes with less free space score higher
        1.0 - node.free_ratio()
    }

    /// Spread: prefer nodes with the most free resources.
    /// This distributes load evenly across the fleet.
    fn score_spread(&self, node: &NodeCapacity) -> f64 {
        node.free_ratio()
    }

    /// Affinity: prefer nodes whose preferred_workloads match the task's workload type.
    /// Falls back to Spread scoring for non-matching nodes.
    fn score_affinity(&self, task: &ScheduledTask, node: &NodeCapacity) -> f64 {
        let spread_score = self.score_spread(node);

        if let Some(workload_type) = &task.workload_type {
            if let Some(pref) = self
                .node_preferences
                .iter()
                .find(|p| p.node_name == node.node_name)
            {
                if pref.prefers(workload_type) {
                    // Affinity match: boost significantly
                    return spread_score * 1.5;
                }
            }
        }

        spread_score
    }

    /// Check if a project has a running task on a node.
    fn is_project_on_node(&self, project: &str, node_name: &str) -> bool {
        self.anti_affinity
            .iter()
            .any(|r| r.project == project && r.is_occupied(node_name))
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::{NodeCapacity, ResourceRequirements, ScheduledTask, TaskPriority};
    use std::time::Duration;
    use uuid::Uuid;

    fn make_node(name: &str, cpus: u32, mem: u64, gpu: bool) -> NodeCapacity {
        NodeCapacity::from_config(name.to_string(), cpus, mem, gpu)
    }

    fn make_task(desc: &str) -> ScheduledTask {
        ScheduledTask::new(desc)
    }

    #[test]
    fn test_bin_pack_prefers_busy_nodes() {
        let engine = PlacementEngine::new(PlacementPolicy::BinPack);
        let task = make_task("test");

        let free_node = make_node("free", 16, 64, false);
        let mut busy_node = make_node("busy", 16, 64, false);

        // Make busy_node half-occupied
        busy_node.allocate(
            Uuid::new_v4(),
            &ResourceRequirements {
                cpu_cores: 8,
                memory_gib: 32,
                gpu_required: false,
                estimated_duration: Duration::from_secs(60),
            },
            TaskPriority::Normal,
        );

        let free_score = engine.score_node(&task, &free_node);
        let busy_score = engine.score_node(&task, &busy_node);

        assert!(
            busy_score > free_score,
            "BinPack should prefer busy node ({} > {})",
            busy_score,
            free_score
        );
    }

    #[test]
    fn test_spread_prefers_free_nodes() {
        let engine = PlacementEngine::new(PlacementPolicy::Spread);
        let task = make_task("test");

        let free_node = make_node("free", 16, 64, false);
        let mut busy_node = make_node("busy", 16, 64, false);

        busy_node.allocate(
            Uuid::new_v4(),
            &ResourceRequirements {
                cpu_cores: 8,
                memory_gib: 32,
                gpu_required: false,
                estimated_duration: Duration::from_secs(60),
            },
            TaskPriority::Normal,
        );

        let free_score = engine.score_node(&task, &free_node);
        let busy_score = engine.score_node(&task, &busy_node);

        assert!(
            free_score > busy_score,
            "Spread should prefer free node ({} > {})",
            free_score,
            busy_score
        );
    }

    #[test]
    fn test_affinity_boosts_matching_workload() {
        let mut engine = PlacementEngine::new(PlacementPolicy::Affinity);

        engine.add_node_preference(NodeWorkloadPreference {
            node_name: "james".to_string(),
            preferred_workloads: vec!["coding".to_string(), "build".to_string()],
        });

        let mut task = make_task("code task");
        task.workload_type = Some("coding".to_string());

        let james = make_node("james", 16, 64, false);
        let marcus = make_node("marcus", 16, 64, false);

        let james_score = engine.score_node(&task, &james);
        let marcus_score = engine.score_node(&task, &marcus);

        assert!(
            james_score > marcus_score,
            "Affinity should boost james for coding ({} > {})",
            james_score,
            marcus_score
        );
    }

    #[test]
    fn test_anti_affinity_penalty() {
        let mut engine = PlacementEngine::new(PlacementPolicy::Spread);

        // Mark project "forge-fleet" as having a task on "james"
        engine.mark_project_on_node("forge-fleet", "james");

        let mut task = make_task("another fleet task");
        task.project = Some("forge-fleet".to_string());

        let james = make_node("james", 16, 64, false);
        let marcus = make_node("marcus", 16, 64, false);

        let james_score = engine.score_node(&task, &james);
        let marcus_score = engine.score_node(&task, &marcus);

        assert!(
            marcus_score > james_score,
            "Anti-affinity should penalize james ({} < {})",
            james_score,
            marcus_score
        );
    }

    #[test]
    fn test_preferred_node_bonus() {
        let engine = PlacementEngine::new(PlacementPolicy::Spread);

        let mut task = make_task("test");
        task.preferred_nodes = vec!["james".to_string()];

        let james = make_node("james", 16, 64, false);
        let marcus = make_node("marcus", 16, 64, false);

        let james_score = engine.score_node(&task, &james);
        let marcus_score = engine.score_node(&task, &marcus);

        assert!(
            james_score > marcus_score,
            "Preferred node should get bonus ({} > {})",
            james_score,
            marcus_score
        );
    }

    #[test]
    fn test_offline_node_scores_zero() {
        let engine = PlacementEngine::new(PlacementPolicy::Spread);
        let task = make_task("test");

        let mut offline = make_node("offline", 16, 64, false);
        offline.online = false;

        let score = engine.score_node(&task, &offline);
        assert!(
            score == 0.0,
            "Offline node should score 0.0, got {}",
            score
        );
    }

    #[test]
    fn test_insufficient_resources_scores_zero() {
        let engine = PlacementEngine::new(PlacementPolicy::Spread);

        let mut task = make_task("big task");
        task.requirements = ResourceRequirements {
            cpu_cores: 32,
            memory_gib: 128,
            gpu_required: false,
            estimated_duration: Duration::from_secs(60),
        };

        let small_node = make_node("small", 8, 16, false);
        let score = engine.score_node(&task, &small_node);
        assert!(
            score == 0.0,
            "Insufficient node should score 0.0, got {}",
            score
        );
    }

    #[test]
    fn test_clear_anti_affinity() {
        let mut engine = PlacementEngine::new(PlacementPolicy::Spread);

        engine.mark_project_on_node("myproject", "james");
        assert!(engine.is_project_on_node("myproject", "james"));

        engine.clear_project_on_node("myproject", "james");
        assert!(!engine.is_project_on_node("myproject", "james"));
    }

    #[test]
    fn test_workload_preference_case_insensitive() {
        let pref = NodeWorkloadPreference {
            node_name: "james".to_string(),
            preferred_workloads: vec!["Coding".to_string()],
        };
        assert!(pref.prefers("coding"));
        assert!(pref.prefers("CODING"));
        assert!(pref.prefers("Coding"));
        assert!(!pref.prefers("review"));
    }

    #[test]
    fn test_policy_default() {
        assert_eq!(PlacementPolicy::default(), PlacementPolicy::Spread);
    }
}
