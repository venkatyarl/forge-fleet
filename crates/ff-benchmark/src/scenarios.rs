use ff_core::Tier;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Canonical benchmark scenario families used across ForgeFleet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScenarioKind {
    Latency,
    Throughput,
    LongContext,
    MultiModelRouting,
}

/// Request shape used by benchmark scenarios.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkRequest {
    pub prompt: String,
    pub max_tokens: u32,
    pub temperature: f32,
}

/// A concrete benchmark scenario that can be executed by the runner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkScenario {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub kind: ScenarioKind,
    pub iterations: u32,
    pub concurrency: u32,
    pub warmup_requests: u32,
    pub target_tier: Option<Tier>,
    pub target_model: Option<String>,
    pub routing_models: Vec<String>,
    pub request: BenchmarkRequest,
}

impl BenchmarkScenario {
    /// Low-latency single-turn benchmark, useful for p50/p95/p99 tracking.
    pub fn latency(model: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: "latency".to_string(),
            description: "Single-turn latency benchmark under light concurrency".to_string(),
            kind: ScenarioKind::Latency,
            iterations: 120,
            concurrency: 4,
            warmup_requests: 8,
            target_tier: None,
            target_model: Some(model.into()),
            routing_models: Vec::new(),
            request: BenchmarkRequest {
                prompt: "Give a concise explanation of why low-latency inference matters in fleet routing.".to_string(),
                max_tokens: 96,
                temperature: 0.2,
            },
        }
    }

    /// High-concurrency benchmark to measure throughput and queue pressure.
    pub fn throughput(model: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: "throughput".to_string(),
            description: "Sustained throughput benchmark with elevated concurrency".to_string(),
            kind: ScenarioKind::Throughput,
            iterations: 300,
            concurrency: 24,
            warmup_requests: 16,
            target_tier: None,
            target_model: Some(model.into()),
            routing_models: Vec::new(),
            request: BenchmarkRequest {
                prompt: "Return five bullet points describing backpressure handling in distributed inference systems.".to_string(),
                max_tokens: 140,
                temperature: 0.3,
            },
        }
    }

    /// Long-context benchmark that stresses context-window and KV/cache behavior.
    pub fn long_context(model: impl Into<String>) -> Self {
        let corpus = "ForgeFleet context segment. ".repeat(750);

        Self {
            id: Uuid::new_v4(),
            name: "long-context".to_string(),
            description: "Large prompt benchmark for long-context and memory-pressure behavior"
                .to_string(),
            kind: ScenarioKind::LongContext,
            iterations: 40,
            concurrency: 3,
            warmup_requests: 4,
            target_tier: None,
            target_model: Some(model.into()),
            routing_models: Vec::new(),
            request: BenchmarkRequest {
                prompt: format!(
                    "{corpus}\n\nFrom the preceding context, summarize routing constraints in under 6 bullets."
                ),
                max_tokens: 220,
                temperature: 0.1,
            },
        }
    }

    /// Multi-model routing scenario that rotates models to validate router behavior.
    pub fn multi_model_routing(models: Vec<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: "multi-model-routing".to_string(),
            description: "Routes requests across multiple models and records routing stability"
                .to_string(),
            kind: ScenarioKind::MultiModelRouting,
            iterations: 160,
            concurrency: 10,
            warmup_requests: 10,
            target_tier: Some(Tier::Tier2),
            target_model: None,
            routing_models: models,
            request: BenchmarkRequest {
                prompt: "Solve: classify this task as tier1/tier2/tier3/tier4 and justify in one paragraph.".to_string(),
                max_tokens: 128,
                temperature: 0.0,
            },
        }
    }

    /// Build the standard benchmark suite for a fleet.
    pub fn standard_suite(
        default_model: impl Into<String>,
        routing_models: Vec<String>,
    ) -> Vec<Self> {
        let model = default_model.into();
        vec![
            Self::latency(model.clone()),
            Self::throughput(model.clone()),
            Self::long_context(model),
            Self::multi_model_routing(routing_models),
        ]
    }

    /// Resolve which model should be used for an iteration.
    pub fn resolve_model_for_iteration(&self, iteration: u32) -> Option<String> {
        if !self.routing_models.is_empty() {
            let idx = (iteration as usize) % self.routing_models.len();
            return self.routing_models.get(idx).cloned();
        }

        self.target_model.clone()
    }
}
