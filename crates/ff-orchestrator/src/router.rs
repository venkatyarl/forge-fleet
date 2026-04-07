//! Model-task router — Perplexity-style "which model is best for THIS subtask?"
//!
//! Given a [`SubTask`](crate::SubTask), the router scores every available model
//! across multiple dimensions:
//!
//! - **Specialty match** — is this model's tier/type ideal for the task?
//! - **Node health** — is the node online and responsive?
//! - **Load** — how busy is the node right now?
//! - **Hardware fit** — GPU memory, context window, quantization
//! - **Tier preference** — user/task-level min/max tier constraints
//!
//! The highest-scoring model wins.  Ties are broken by tier (lower = faster).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use ff_core::{Model, Node, NodeStatus, Tier};

use crate::decomposer::SubTask;

// ─── Score Breakdown ─────────────────────────────────────────────────────────

/// Detailed scoring of a model for a specific subtask.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelScore {
    /// Which model was scored.
    pub model_id: String,
    /// Which node this model lives on.
    pub node_name: String,
    /// Specialty match score (0.0–1.0).
    pub specialty_score: f64,
    /// Node health score (0.0–1.0).
    pub health_score: f64,
    /// Load score (0.0–1.0, higher = less loaded).
    pub load_score: f64,
    /// Hardware fit score (0.0–1.0).
    pub hardware_score: f64,
    /// Tier preference score (0.0–1.0).
    pub tier_score: f64,
    /// Weighted total (0.0–5.0).
    pub total: f64,
}

impl ModelScore {
    /// Compute the weighted total from individual scores.
    pub fn compute_total(&mut self) {
        // Weights: specialty matters most, then load, then tier, then hardware, then health
        self.total = self.specialty_score * 2.0
            + self.load_score * 1.5
            + self.tier_score * 1.0
            + self.hardware_score * 0.8
            + self.health_score * 0.7;
    }
}

// ─── Route Decision ──────────────────────────────────────────────────────────

/// The router's decision for a single subtask.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteDecision {
    /// Which subtask this decision is for.
    pub subtask_id: Uuid,
    /// The winning model.
    pub model_id: String,
    /// The node to send the request to.
    pub node_name: String,
    /// The node's host:port for the inference endpoint.
    pub endpoint: String,
    /// Full score breakdown of the winner.
    pub score: ModelScore,
    /// Runner-up scores (for debugging / observability).
    pub alternatives: Vec<ModelScore>,
    /// When this decision was made.
    pub decided_at: DateTime<Utc>,
}

// ─── Node Load Snapshot ──────────────────────────────────────────────────────

/// Live load information for a node.  In production this comes from health
/// checks; here we accept it as input so the router is pure / testable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeLoad {
    /// Active inference requests on this node right now.
    pub active_requests: u32,
    /// Maximum concurrent requests the node can handle.
    pub max_concurrent: u32,
    /// Queue depth (requests waiting).
    pub queue_depth: u32,
    /// Average latency of recent completions (ms).
    pub avg_latency_ms: u64,
}

impl NodeLoad {
    /// Utilization ratio (0.0 = idle, 1.0 = fully loaded).
    pub fn utilization(&self) -> f64 {
        if self.max_concurrent == 0 {
            return 1.0;
        }
        (self.active_requests as f64) / (self.max_concurrent as f64)
    }
}

// ─── Router Configuration ────────────────────────────────────────────────────

/// Constraints that influence routing decisions.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RouteConstraints {
    /// Minimum tier allowed.
    pub min_tier: Option<Tier>,
    /// Maximum tier allowed.
    pub max_tier: Option<Tier>,
    /// Prefer specific nodes (e.g., keep work local).
    pub preferred_nodes: Vec<String>,
    /// Exclude specific nodes.
    pub excluded_nodes: Vec<String>,
    /// Maximum acceptable latency (ms).
    pub max_latency_ms: Option<u64>,
}

// ─── Task Router ─────────────────────────────────────────────────────────────

/// The core router — scores and selects models for subtasks.
pub struct TaskRouter {
    /// Known nodes in the fleet.
    nodes: Vec<Node>,
    /// Known models across all nodes.
    models: Vec<Model>,
    /// Live load data per node name.
    node_loads: HashMap<String, NodeLoad>,
}

impl TaskRouter {
    /// Create a new router with fleet state.
    pub fn new(
        nodes: Vec<Node>,
        models: Vec<Model>,
        node_loads: HashMap<String, NodeLoad>,
    ) -> Self {
        Self {
            nodes,
            models,
            node_loads,
        }
    }

    /// Update live load data for a node.
    pub fn update_load(&mut self, node_name: &str, load: NodeLoad) {
        self.node_loads.insert(node_name.to_string(), load);
    }

    /// Route a single subtask to the best model/node.
    ///
    /// Returns `None` if no suitable model is available (all nodes offline,
    /// no models match constraints, etc.).
    pub fn route(
        &self,
        subtask: &SubTask,
        constraints: &RouteConstraints,
    ) -> Option<RouteDecision> {
        let mut scores: Vec<ModelScore> = Vec::new();

        for model in &self.models {
            // Find the node(s) that serve this model
            for node_name in &model.nodes {
                let Some(node) = self.nodes.iter().find(|n| &n.name == node_name) else {
                    continue;
                };

                // Skip excluded nodes
                if constraints.excluded_nodes.contains(&node.name) {
                    continue;
                }

                // Skip offline nodes
                if node.status == NodeStatus::Offline {
                    continue;
                }

                // Tier constraints
                if let Some(min) = constraints.min_tier
                    && model.tier < min
                {
                    continue;
                }
                if let Some(max) = constraints.max_tier
                    && model.tier > max
                {
                    continue;
                }

                // Latency constraint
                if let Some(max_lat) = constraints.max_latency_ms
                    && let Some(load) = self.node_loads.get(&node.name)
                    && load.avg_latency_ms > max_lat
                {
                    continue;
                }

                let mut score = self.score_model(model, node, subtask, constraints);
                score.compute_total();
                scores.push(score);
            }
        }

        // Sort by total score descending
        scores.sort_by(|a, b| {
            b.total
                .partial_cmp(&a.total)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let winner = scores.first()?.clone();
        let node = self.nodes.iter().find(|n| n.name == winner.node_name)?;
        let endpoint = format!("{}:{}", node.host, node.port);

        Some(RouteDecision {
            subtask_id: subtask.id,
            model_id: winner.model_id.clone(),
            node_name: winner.node_name.clone(),
            endpoint,
            score: winner,
            alternatives: scores.into_iter().skip(1).collect(),
            decided_at: Utc::now(),
        })
    }

    /// Route multiple subtasks, returning decisions in the same order.
    pub fn route_batch(
        &self,
        subtasks: &[SubTask],
        constraints: &RouteConstraints,
    ) -> Vec<Option<RouteDecision>> {
        subtasks
            .iter()
            .map(|st| self.route(st, constraints))
            .collect()
    }

    /// Score a single model on a single node for a subtask.
    fn score_model(
        &self,
        model: &Model,
        node: &Node,
        subtask: &SubTask,
        constraints: &RouteConstraints,
    ) -> ModelScore {
        let specialty_score = self.compute_specialty_score(model, subtask);
        let health_score = self.compute_health_score(node);
        let load_score = self.compute_load_score(node);
        let hardware_score = self.compute_hardware_score(model, node);
        let tier_score = self.compute_tier_score(model, subtask, constraints);

        ModelScore {
            model_id: model.id.clone(),
            node_name: node.name.clone(),
            specialty_score,
            health_score,
            load_score,
            hardware_score,
            tier_score,
            total: 0.0, // computed by caller
        }
    }

    /// How well does this model's tier match the subtask's ideal tier?
    fn compute_specialty_score(&self, model: &Model, subtask: &SubTask) -> f64 {
        let ideal = subtask.task_type.suggested_ideal_tier();
        let min = subtask.task_type.suggested_min_tier();

        if model.tier == ideal {
            1.0
        } else if model.tier >= min && model.tier > ideal {
            // Higher tier than ideal — can do it but overkill
            0.7
        } else if model.tier >= min {
            // Between min and ideal
            0.8
        } else {
            // Below minimum — risky
            0.2
        }
    }

    /// Node health score — online gets 1.0, degraded gets 0.5, etc.
    fn compute_health_score(&self, node: &Node) -> f64 {
        match node.status {
            NodeStatus::Online => 1.0,
            NodeStatus::Degraded => 0.5,
            NodeStatus::Starting => 0.3,
            NodeStatus::Maintenance => 0.1,
            NodeStatus::Offline => 0.0,
        }
    }

    /// Load score — lower utilization = higher score.
    fn compute_load_score(&self, node: &Node) -> f64 {
        match self.node_loads.get(&node.name) {
            Some(load) => {
                let util = load.utilization();
                // Invert: 0% util → 1.0 score, 100% util → 0.0
                (1.0 - util).max(0.0)
            }
            // No load data → assume lightly loaded
            None => 0.8,
        }
    }

    /// Hardware fit — does this node have enough memory / context for the model?
    fn compute_hardware_score(&self, model: &Model, node: &Node) -> f64 {
        let mut score: f64 = 0.5; // baseline

        // GPU acceleration bonus
        if node.hardware.has_gpu() {
            score += 0.2;
        }

        // Memory headroom: rough estimate of model size
        let est_model_gib = (model.params_b * 0.6) as u64; // ~0.6 GiB per billion params at Q4
        if node.hardware.memory_gib > est_model_gib * 2 {
            score += 0.2; // Plenty of headroom
        } else if node.hardware.memory_gib > est_model_gib {
            score += 0.1;
        }

        // Large context window bonus for complex tasks
        if model.ctx_size >= 32768 {
            score += 0.1;
        }

        score.min(1.0)
    }

    /// Tier preference — bonus for matching preferred tier, penalty for overkill.
    fn compute_tier_score(
        &self,
        model: &Model,
        subtask: &SubTask,
        constraints: &RouteConstraints,
    ) -> f64 {
        let ideal = subtask.task_type.suggested_ideal_tier();

        // Exact match is perfect
        if model.tier == ideal {
            return 1.0;
        }

        // Within user constraints is good
        let in_bounds = constraints.min_tier.is_none_or(|min| model.tier >= min)
            && constraints.max_tier.is_none_or(|max| model.tier <= max);

        if !in_bounds {
            return 0.0;
        }

        // Penalize distance from ideal tier
        let distance = (model.tier.as_u8() as i8 - ideal.as_u8() as i8).unsigned_abs();
        match distance {
            0 => 1.0,
            1 => 0.7,
            2 => 0.4,
            _ => 0.2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decomposer::SubTaskType;
    use ff_core::*;

    fn make_node(name: &str, status: NodeStatus, memory_gib: u64) -> Node {
        Node {
            id: Uuid::new_v4(),
            name: name.into(),
            host: "192.168.5.100".into(),
            port: 51800,
            role: Role::Worker,
            election_priority: 10,
            status,
            hardware: Hardware {
                os: OsType::MacOs,
                cpu_model: "Apple M4 Max".into(),
                cpu_cores: 16,
                gpu: GpuType::AppleSilicon,
                gpu_model: None,
                memory_gib,
                memory_type: MemoryType::Unified,
                interconnect: Interconnect::Ethernet10g,
                runtimes: vec![Runtime::LlamaCpp],
            },
            models: vec!["qwen3-9b".into()],
            last_heartbeat: Some(Utc::now()),
            registered_at: Utc::now(),
        }
    }

    fn make_model(id: &str, tier: Tier, params_b: f32, node: &str) -> Model {
        Model {
            id: id.into(),
            name: id.into(),
            tier,
            params_b,
            quant: "Q4_K_M".into(),
            path: format!("/models/{id}.gguf"),
            ctx_size: 8192,
            runtime: Runtime::LlamaCpp,
            nodes: vec![node.into()],
        }
    }

    #[test]
    fn test_route_picks_best_tier() {
        let nodes = vec![make_node("taylor", NodeStatus::Online, 128)];
        let models = vec![
            make_model("qwen3-9b", Tier::Tier1, 9.0, "taylor"),
            make_model("qwen3-32b", Tier::Tier2, 32.0, "taylor"),
        ];
        let router = TaskRouter::new(nodes, models, HashMap::new());

        // FastLookup → ideal is Tier1
        let st = SubTask::new(0, "lookup", "translate hello", SubTaskType::FastLookup);
        let decision = router.route(&st, &RouteConstraints::default()).unwrap();
        assert_eq!(decision.model_id, "qwen3-9b");

        // Code → ideal is Tier2
        let st = SubTask::new(0, "code", "implement function", SubTaskType::Code);
        let decision = router.route(&st, &RouteConstraints::default()).unwrap();
        assert_eq!(decision.model_id, "qwen3-32b");
    }

    #[test]
    fn test_route_skips_offline_nodes() {
        let nodes = vec![
            make_node("taylor", NodeStatus::Offline, 128),
            make_node("james", NodeStatus::Online, 64),
        ];
        let models = vec![
            make_model("m1", Tier::Tier1, 9.0, "taylor"),
            make_model("m2", Tier::Tier1, 9.0, "james"),
        ];
        let router = TaskRouter::new(nodes, models, HashMap::new());

        let st = SubTask::new(0, "test", "translate hello", SubTaskType::FastLookup);
        let decision = router.route(&st, &RouteConstraints::default()).unwrap();
        assert_eq!(decision.node_name, "james");
    }

    #[test]
    fn test_route_respects_tier_constraints() {
        let nodes = vec![make_node("taylor", NodeStatus::Online, 128)];
        let models = vec![
            make_model("small", Tier::Tier1, 9.0, "taylor"),
            make_model("big", Tier::Tier3, 72.0, "taylor"),
        ];
        let router = TaskRouter::new(nodes, models, HashMap::new());

        let st = SubTask::new(0, "task", "do something", SubTaskType::Analysis);
        let constraints = RouteConstraints {
            min_tier: Some(Tier::Tier3),
            ..Default::default()
        };
        let decision = router.route(&st, &constraints).unwrap();
        assert_eq!(decision.model_id, "big");
    }

    #[test]
    fn test_route_prefers_less_loaded() {
        let nodes = vec![
            make_node("taylor", NodeStatus::Online, 128),
            make_node("james", NodeStatus::Online, 128),
        ];
        let models = vec![
            make_model("m1", Tier::Tier1, 9.0, "taylor"),
            make_model("m2", Tier::Tier1, 9.0, "james"),
        ];
        let mut loads = HashMap::new();
        loads.insert(
            "taylor".to_string(),
            NodeLoad {
                active_requests: 4,
                max_concurrent: 4,
                queue_depth: 2,
                avg_latency_ms: 500,
            },
        );
        loads.insert(
            "james".to_string(),
            NodeLoad {
                active_requests: 0,
                max_concurrent: 4,
                queue_depth: 0,
                avg_latency_ms: 100,
            },
        );
        let router = TaskRouter::new(nodes, models, loads);

        let st = SubTask::new(0, "test", "translate hello", SubTaskType::FastLookup);
        let decision = router.route(&st, &RouteConstraints::default()).unwrap();
        assert_eq!(decision.node_name, "james");
    }

    #[test]
    fn test_route_returns_none_when_no_nodes() {
        let router = TaskRouter::new(vec![], vec![], HashMap::new());
        let st = SubTask::new(0, "test", "do stuff", SubTaskType::Code);
        assert!(router.route(&st, &RouteConstraints::default()).is_none());
    }

    #[test]
    fn test_route_batch() {
        let nodes = vec![make_node("taylor", NodeStatus::Online, 128)];
        let models = vec![make_model("m1", Tier::Tier2, 32.0, "taylor")];
        let router = TaskRouter::new(nodes, models, HashMap::new());

        let subtasks = vec![
            SubTask::new(0, "a", "implement something", SubTaskType::Code),
            SubTask::new(1, "b", "review the code", SubTaskType::Review),
        ];
        let decisions = router.route_batch(&subtasks, &RouteConstraints::default());
        assert_eq!(decisions.len(), 2);
        assert!(decisions.iter().all(|d| d.is_some()));
    }

    #[test]
    fn test_node_load_utilization() {
        let load = NodeLoad {
            active_requests: 2,
            max_concurrent: 4,
            queue_depth: 0,
            avg_latency_ms: 200,
        };
        assert!((load.utilization() - 0.5).abs() < f64::EPSILON);

        let empty = NodeLoad {
            active_requests: 0,
            max_concurrent: 0,
            queue_depth: 0,
            avg_latency_ms: 0,
        };
        assert!((empty.utilization() - 1.0).abs() < f64::EPSILON);
    }
}
