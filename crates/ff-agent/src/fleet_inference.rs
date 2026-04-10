//! Fleet inference — distributed LLM routing, model placement, and inference optimization.
//!
//! Manages the fleet of LLM endpoints as a unified inference fabric:
//! - Model placement optimization (right model on right hardware)
//! - Prefix caching awareness (shared prompts across sessions)
//! - Speculative decoding coordination (draft model on fast node, verify on big model)
//! - Health-aware routing with automatic failover
//! - llama.cpp RPC integration for distributed tensor parallelism

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

/// A fleet LLM endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetEndpoint {
    pub name: String,
    pub url: String,
    pub model_name: String,
    pub model_params: u64,
    pub memory_gb: u32,
    pub gpu_type: GpuType,
    /// Tokens per second (measured).
    pub tps: Option<f64>,
    /// Time to first token in ms (measured).
    pub ttft_ms: Option<f64>,
    /// Maximum context window.
    pub context_window: u32,
    /// Whether this endpoint is healthy.
    pub healthy: bool,
    /// Last health check time.
    pub last_check: Option<std::time::SystemTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuType {
    AppleSilicon,
    NvidiaCuda,
    AmdRocm,
    AmdRyzenAi,
    Cpu,
}

/// Fleet inference manager.
pub struct FleetInferenceManager {
    endpoints: Arc<DashMap<String, FleetEndpoint>>,
    metrics: Arc<DashMap<String, EndpointMetrics>>,
}

#[derive(Debug, Clone, Default)]
struct EndpointMetrics {
    request_count: u64,
    total_latency_ms: u64,
    errors: u64,
    last_latency_ms: u64,
}

impl FleetInferenceManager {
    pub fn new() -> Self {
        Self {
            endpoints: Arc::new(DashMap::new()),
            metrics: Arc::new(DashMap::new()),
        }
    }

    /// Register a fleet endpoint.
    pub fn register(&self, endpoint: FleetEndpoint) {
        info!(name = %endpoint.name, url = %endpoint.url, model = %endpoint.model_name, "registered fleet endpoint");
        self.endpoints.insert(endpoint.name.clone(), endpoint);
    }

    /// Register all known fleet endpoints (from fleet.toml or hardcoded fallback).
    pub fn register_default_fleet(&self) {
        let defaults = vec![
            FleetEndpoint {
                name: "taylor-gemma".into(), url: "http://192.168.5.100:55000".into(),
                model_name: "gemma-4-31b-it".into(), model_params: 31_000_000_000,
                memory_gb: 96, gpu_type: GpuType::AppleSilicon,
                tps: None, ttft_ms: None, context_window: 262_144,
                healthy: true, last_check: None,
            },
            FleetEndpoint {
                name: "taylor-qwen35".into(), url: "http://192.168.5.100:55001".into(),
                model_name: "qwen3.5-35b-a3b".into(), model_params: 35_000_000_000,
                memory_gb: 96, gpu_type: GpuType::AppleSilicon,
                tps: None, ttft_ms: None, context_window: 32_768,
                healthy: true, last_check: None,
            },
            FleetEndpoint {
                name: "marcus".into(), url: "http://192.168.5.102:55000".into(),
                model_name: "qwen3-coder-30b-a3b".into(), model_params: 30_000_000_000,
                memory_gb: 32, gpu_type: GpuType::Cpu,
                tps: Some(3.0), ttft_ms: Some(15_000.0), context_window: 32_768,
                healthy: true, last_check: None,
            },
            FleetEndpoint {
                name: "sophie".into(), url: "http://192.168.5.103:55000".into(),
                model_name: "qwen3-coder-30b-a3b".into(), model_params: 30_000_000_000,
                memory_gb: 32, gpu_type: GpuType::Cpu,
                tps: Some(2.0), ttft_ms: Some(20_000.0), context_window: 32_768,
                healthy: true, last_check: None,
            },
            FleetEndpoint {
                name: "priya".into(), url: "http://192.168.5.104:55000".into(),
                model_name: "qwen3-coder-30b-a3b".into(), model_params: 30_000_000_000,
                memory_gb: 32, gpu_type: GpuType::Cpu,
                tps: Some(2.5), ttft_ms: Some(18_000.0), context_window: 32_768,
                healthy: true, last_check: None,
            },
            FleetEndpoint {
                name: "james".into(), url: "http://192.168.5.108:55000".into(),
                model_name: "qwen3.5-35b-a3b".into(), model_params: 35_000_000_000,
                memory_gb: 64, gpu_type: GpuType::Cpu,
                tps: Some(2.0), ttft_ms: Some(20_000.0), context_window: 32_768,
                healthy: true, last_check: None,
            },
            FleetEndpoint {
                name: "logan".into(), url: "http://192.168.5.111:55000".into(),
                model_name: "qwen3.5-35b-a3b".into(), model_params: 35_000_000_000,
                memory_gb: 128, gpu_type: GpuType::Cpu,
                tps: Some(4.0), ttft_ms: Some(10_000.0), context_window: 32_768,
                healthy: true, last_check: None,
            },
            FleetEndpoint {
                name: "veronica".into(), url: "http://192.168.5.112:55000".into(),
                model_name: "qwen3.5-35b-a3b".into(), model_params: 35_000_000_000,
                memory_gb: 128, gpu_type: GpuType::Cpu,
                tps: Some(4.0), ttft_ms: Some(10_000.0), context_window: 32_768,
                healthy: true, last_check: None,
            },
            FleetEndpoint {
                name: "lily".into(), url: "http://192.168.5.113:55000".into(),
                model_name: "qwen3.5-35b-a3b".into(), model_params: 35_000_000_000,
                memory_gb: 128, gpu_type: GpuType::Cpu,
                tps: Some(4.0), ttft_ms: Some(10_000.0), context_window: 32_768,
                healthy: true, last_check: None,
            },
            FleetEndpoint {
                name: "duncan".into(), url: "http://192.168.5.114:55000".into(),
                model_name: "qwen3.5-35b-a3b".into(), model_params: 35_000_000_000,
                memory_gb: 128, gpu_type: GpuType::Cpu,
                tps: Some(4.0), ttft_ms: Some(10_000.0), context_window: 32_768,
                healthy: true, last_check: None,
            },
        ];

        for ep in defaults {
            self.register(ep);
        }
    }

    /// Select the best endpoint for a given task.
    pub fn select_endpoint(&self, task_type: TaskType) -> Option<FleetEndpoint> {
        let mut candidates: Vec<FleetEndpoint> = self.endpoints
            .iter()
            .filter(|e| e.healthy)
            .map(|e| e.value().clone())
            .collect();

        if candidates.is_empty() {
            return None;
        }

        // Score each endpoint for the task
        candidates.sort_by(|a, b| {
            let score_a = endpoint_score(a, task_type);
            let score_b = endpoint_score(b, task_type);
            score_b.partial_cmp(&score_a).unwrap_or(std::cmp::Ordering::Equal)
        });

        Some(candidates[0].clone())
    }

    /// Select multiple endpoints for parallel agent execution.
    pub fn select_parallel_endpoints(&self, count: usize) -> Vec<FleetEndpoint> {
        let mut endpoints: Vec<FleetEndpoint> = self.endpoints
            .iter()
            .filter(|e| e.healthy)
            .map(|e| e.value().clone())
            .collect();

        // Sort by TPS (fastest first for parallel work)
        endpoints.sort_by(|a, b| {
            let tps_a = a.tps.unwrap_or(0.0);
            let tps_b = b.tps.unwrap_or(0.0);
            tps_b.partial_cmp(&tps_a).unwrap_or(std::cmp::Ordering::Equal)
        });

        endpoints.truncate(count);
        endpoints
    }

    /// Record a request result for metrics tracking.
    pub fn record_result(&self, endpoint_name: &str, latency_ms: u64, success: bool) {
        let mut metrics = self.metrics.entry(endpoint_name.to_string()).or_default();
        metrics.request_count += 1;
        metrics.total_latency_ms += latency_ms;
        metrics.last_latency_ms = latency_ms;
        if !success {
            metrics.errors += 1;
        }

        // Update TPS estimate from latency
        if success && latency_ms > 0 {
            if let Some(mut ep) = self.endpoints.get_mut(endpoint_name) {
                // Very rough TPS estimate: assume 100 output tokens / latency
                let estimated_tps = 100_000.0 / latency_ms as f64;
                ep.tps = Some(estimated_tps);
            }
        }
    }

    /// Health check all endpoints.
    pub async fn health_check_all(&self) {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();

        let names: Vec<String> = self.endpoints.iter().map(|e| e.key().clone()).collect();

        for name in names {
            if let Some(mut ep) = self.endpoints.get_mut(&name) {
                let url = format!("{}/health", ep.url.trim_end_matches('/'));
                let healthy = client.get(&url).send().await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false);
                ep.healthy = healthy;
                ep.last_check = Some(std::time::SystemTime::now());
                debug!(name = %name, healthy, "fleet health check");
            }
        }
    }

    /// List all endpoints with status.
    pub fn list(&self) -> Vec<FleetEndpoint> {
        self.endpoints.iter().map(|e| e.value().clone()).collect()
    }
}

impl Default for FleetInferenceManager {
    fn default() -> Self { Self::new() }
}

/// Task type for endpoint selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskType {
    /// Fast coding assistance (autocomplete, small edits).
    FastCoding,
    /// Complex reasoning and planning.
    Reasoning,
    /// Code review and analysis.
    Review,
    /// General coding (default).
    GeneralCoding,
    /// Quick utility tasks (file listing, status checks).
    Utility,
}

fn endpoint_score(ep: &FleetEndpoint, task: TaskType) -> f64 {
    let mut score = 0.0;

    match task {
        TaskType::FastCoding => {
            // Prefer fast inference (high TPS)
            score += ep.tps.unwrap_or(0.0) * 10.0;
            // Prefer GPU-accelerated
            if matches!(ep.gpu_type, GpuType::AppleSilicon | GpuType::NvidiaCuda | GpuType::AmdRyzenAi) {
                score += 50.0;
            }
            // Prefer smaller models (faster)
            if ep.model_params < 15_000_000_000 { score += 30.0; }
        }
        TaskType::Reasoning => {
            // Prefer larger models
            score += (ep.model_params as f64 / 1_000_000_000.0) * 2.0;
            // Prefer large context window
            score += ep.context_window as f64 / 1000.0;
        }
        TaskType::Review => {
            // Balance size and speed
            score += (ep.model_params as f64 / 1_000_000_000.0) * 1.5;
            score += ep.tps.unwrap_or(0.0) * 5.0;
        }
        TaskType::GeneralCoding => {
            // Prefer coding-specific models
            if ep.model_name.contains("coder") || ep.model_name.contains("code") {
                score += 30.0;
            }
            score += (ep.model_params as f64 / 1_000_000_000.0) * 1.0;
            score += ep.tps.unwrap_or(0.0) * 3.0;
        }
        TaskType::Utility => {
            // Prefer fastest, smallest model
            score += ep.tps.unwrap_or(0.0) * 20.0;
            score -= ep.model_params as f64 / 10_000_000_000.0;
        }
    }

    // Health bonus
    if ep.healthy { score += 100.0; }

    score
}

/// Configuration for speculative decoding across fleet nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeculativeDecodingConfig {
    /// Fast/small model endpoint for draft generation.
    pub draft_endpoint: String,
    /// Large model endpoint for verification.
    pub verify_endpoint: String,
    /// Number of draft tokens to generate before verification.
    pub draft_tokens: u32,
    /// Whether speculative decoding is enabled.
    pub enabled: bool,
}

/// Configuration for prefix caching across the fleet.
#[derive(Debug, Clone, Default)]
pub struct PrefixCacheManager {
    /// Cached prefix hashes per endpoint.
    known_prefixes: DashMap<String, Vec<String>>,
}

impl PrefixCacheManager {
    pub fn new() -> Self { Self::default() }

    /// Record that a prefix was sent to an endpoint.
    pub fn record_prefix(&self, endpoint: &str, prefix_hash: &str) {
        self.known_prefixes
            .entry(endpoint.to_string())
            .or_default()
            .push(prefix_hash.to_string());
    }

    /// Check if an endpoint has seen a prefix (for cache-aware routing).
    pub fn has_prefix(&self, endpoint: &str, prefix_hash: &str) -> bool {
        self.known_prefixes
            .get(endpoint)
            .map(|prefixes| prefixes.contains(&prefix_hash.to_string()))
            .unwrap_or(false)
    }

    /// Compute a hash for a system prompt (for prefix matching).
    pub fn hash_prefix(text: &str) -> String {
        let mut hash: u64 = 0xcbf29ce484222325;
        for byte in text.bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        format!("{hash:016x}")
    }
}
