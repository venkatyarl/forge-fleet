//! Bridge between Pulse live fleet state and `ff-orchestrator` abstractions.
//!
//! Converts `PulseBeatV2` + `fleet_model_catalog` rows into `ff_core::Node`,
//! `ff_core::Model`, and `ff_orchestrator::NodeLoad` so the orchestrator's
//! `TaskRouter` can make routing decisions based on real-time fleet state.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use serde_json::{Value, json};
use uuid::Uuid;

use ff_api::token_ledger::TokenUsageRecord;
use ff_core::{
    GpuType, Hardware, Interconnect, MemoryType, Model as CoreModel, Node as CoreNode, NodeStatus,
    OsType, Role, Runtime, Tier,
};
use ff_orchestrator::crew::CrewAssignment;
use ff_orchestrator::decomposer::SubTask;
use ff_orchestrator::parallel::{DispatchFn, SubTaskResult, SubTaskStatus};
use ff_orchestrator::router::NodeLoad as OrchestratorNodeLoad;
use ff_orchestrator::router::RouteDecision;
use ff_pulse::beat_v2::{LlmServer, PulseBeatV2};

// ─── Fleet state conversion ─────────────────────────────────────────────────

/// Convert live Pulse beats into the static types the orchestrator expects.
///
/// `catalog` is a map of `model_id → (name, tier, extra_json)` from
/// `fleet_model_catalog`. Tier and params are inferred from the catalog when
/// available, otherwise heuristics are applied.
pub fn pulse_beats_to_fleet_state(
    beats: Vec<PulseBeatV2>,
    catalog: &HashMap<String, (String, i32)>,
) -> (
    Vec<CoreNode>,
    Vec<CoreModel>,
    HashMap<String, OrchestratorNodeLoad>,
) {
    let mut nodes = Vec::new();
    let mut models: Vec<CoreModel> = Vec::new();
    let mut loads = HashMap::new();

    for beat in beats {
        if beat.going_offline {
            continue;
        }

        let worker_name = beat.computer_name.clone();
        let node = beat_to_node(&beat);
        let load = beat_to_load(&beat);

        for server in &beat.llm_servers {
            if server.status != "active" || !server.is_healthy {
                continue;
            }
            let model = server_to_model(server, &worker_name, catalog);
            models.push(model);
        }

        nodes.push(node);
        loads.insert(worker_name, load);
    }

    (nodes, models, loads)
}

fn beat_to_node(beat: &PulseBeatV2) -> CoreNode {
    let status = if beat.maintenance_mode {
        NodeStatus::Maintenance
    } else if beat.going_offline {
        NodeStatus::Offline
    } else if beat.llm_servers.iter().any(|s| s.is_healthy) {
        NodeStatus::Online
    } else {
        NodeStatus::Degraded
    };

    let gpu = map_gpu_kind(&beat.capabilities.gpu_kind);
    let runtimes: Vec<Runtime> = beat
        .capabilities
        .recommended_runtimes
        .iter()
        .filter_map(|r| map_runtime(r))
        .collect();

    let model_ids: Vec<String> = beat
        .llm_servers
        .iter()
        .filter(|s| s.status == "active" && s.is_healthy)
        .map(|s| s.model.id.clone())
        .collect();

    CoreNode {
        id: beat.computer_id.unwrap_or_else(Uuid::new_v4),
        name: beat.computer_name.clone(),
        host: beat.network.primary_ip.clone(),
        port: 55000,
        role: if beat.role_claimed == "leader" {
            Role::Leader
        } else {
            Role::Worker
        },
        election_priority: beat.election_priority.max(0) as u32,
        status,
        hardware: Hardware {
            os: OsType::Linux,
            cpu_model: "unknown".to_string(),
            cpu_cores: beat.hardware.cpu_cores.max(0) as u32,
            gpu,
            gpu_model: beat.hardware.gpu.clone(),
            memory_gib: beat.hardware.ram_gb.max(0) as u64,
            memory_type: MemoryType::Unknown,
            interconnect: Interconnect::Unknown,
            runtimes,
        },
        models: model_ids,
        last_heartbeat: Some(beat.timestamp),
        registered_at: beat.timestamp,
    }
}

fn beat_to_load(beat: &PulseBeatV2) -> OrchestratorNodeLoad {
    let active_requests: u32 = beat
        .llm_servers
        .iter()
        .map(|s| s.active_requests.max(0) as u32)
        .sum();
    let queue_depth: u32 = beat
        .llm_servers
        .iter()
        .map(|s| s.queue_depth.max(0) as u32)
        .sum();
    let max_concurrent: u32 = beat
        .llm_servers
        .iter()
        .map(|s| s.model.parallel_slots.max(1) as u32)
        .sum();

    // Estimate latency from throughput: ~1000ms / tokens_per_sec
    let avg_tps: f64 = beat
        .llm_servers
        .iter()
        .filter(|s| s.tokens_per_sec_last_min > 0.0)
        .map(|s| s.tokens_per_sec_last_min)
        .fold(0.0, |a, b| a + b)
        / beat.llm_servers.len().max(1) as f64;
    let avg_latency_ms = if avg_tps > 0.0 {
        (1000.0 / avg_tps) as u64
    } else {
        5000
    };

    OrchestratorNodeLoad {
        active_requests,
        max_concurrent: max_concurrent.max(4),
        queue_depth,
        avg_latency_ms,
    }
}

fn server_to_model(
    server: &LlmServer,
    worker_name: &str,
    catalog: &HashMap<String, (String, i32)>,
) -> CoreModel {
    let id = server.model.id.clone();
    let display = if server.model.display_name.is_empty() {
        id.clone()
    } else {
        server.model.display_name.clone()
    };

    let (tier, params_b) = catalog.get(&id).map_or_else(
        || (infer_tier(&id), infer_params_b(&id)),
        |(_, tier)| {
            let t = Tier::from_u8(*tier as u8).unwrap_or(Tier::Tier2);
            (t, infer_params_b(&id))
        },
    );

    let runtime = map_runtime(&server.runtime).unwrap_or(Runtime::LlamaCpp);

    CoreModel {
        id: id.clone(),
        name: display,
        tier,
        params_b,
        quant: "unknown".to_string(),
        path: server.model.loaded_path.clone(),
        ctx_size: server.model.context_window.max(0) as u32,
        runtime,
        nodes: vec![worker_name.to_string()],
    }
}

// ─── Heuristic helpers ──────────────────────────────────────────────────────

fn map_gpu_kind(kind: &str) -> GpuType {
    match kind {
        "apple_silicon" => GpuType::AppleSilicon,
        "nvidia_cuda" => GpuType::NvidiaCuda,
        "amd_rocm" => GpuType::AmdRdna,
        "intel_gpu" => GpuType::IntelGpu,
        _ => GpuType::None,
    }
}

fn map_runtime(rt: &str) -> Option<Runtime> {
    match rt.to_ascii_lowercase().as_str() {
        "llama.cpp" | "llamacpp" => Some(Runtime::LlamaCpp),
        "vllm" => Some(Runtime::Vllm),
        "mlx" => Some(Runtime::Mlx),
        "tensorrt" | "tensorrt-llm" => Some(Runtime::TensorRt),
        "ollama" => Some(Runtime::Ollama),
        _ => None,
    }
}

/// Parse the parameter count (billions) out of a model name by reading the
/// actual `<number>b` token(s), rather than substring-matching a fixed list.
///
/// The old enumerated `contains("8b")` approach had the param-size substring
/// trap (cf. #585/#586): `128b` contains `8b` → read as 8B, and `235b` (the
/// fleet's real big model) matched no pattern → fell back to 7B. This scans for
/// `<digits[.digits]>b` runs and takes the LARGEST (so a MoE id like
/// `qwen3-30b-a3b` reads its 30B total, not the 3B active). A `b` followed by a
/// letter is NOT a size — `8bit`/`4bit` quant markers are skipped. Pure.
fn parse_params_b(model_id: &str) -> Option<f32> {
    let lower = model_id.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut best: Option<f32> = None;
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            i += 1;
        }
        // A size token is `<number>b` where the char after `b` is not a letter
        // (so `8bit`/`bf16` aren't misread as a parameter count).
        if i < bytes.len() && bytes[i] == b'b' {
            let after_is_letter = bytes.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic());
            if !after_is_letter && let Ok(n) = lower[start..i].parse::<f32>() {
                best = Some(best.map_or(n, |b: f32| b.max(n)));
            }
            i += 1; // consume the 'b'
        }
    }
    best
}

/// Infer tier from a model's parameter count. Thresholds reproduce the legacy
/// enumerated mapping (≥70B→Tier4, ≥40B→Tier3, ≥20B→Tier2, >0→Tier1) while
/// correctly classifying sizes the old substring list missed (128B, 235B).
/// Unknown size → Tier2 (mid default), as before.
fn infer_tier(model_id: &str) -> Tier {
    match parse_params_b(model_id) {
        Some(p) if p >= 70.0 => Tier::Tier4,
        Some(p) if p >= 40.0 => Tier::Tier3,
        Some(p) if p >= 20.0 => Tier::Tier2,
        Some(p) if p > 0.0 => Tier::Tier1,
        _ => Tier::Tier2,
    }
}

/// Extract parameter count (billions) from a model name; 7.0 when unknown.
fn infer_params_b(model_id: &str) -> f32 {
    parse_params_b(model_id).unwrap_or(7.0)
}

// ─── HTTP dispatch closure ──────────────────────────────────────────────────

/// Build a `DispatchFn` that sends each subtask to its routed LLM endpoint
/// via the gateway's HTTP client.
pub fn build_dispatch_fn(
    state: Arc<crate::GatewayState>,
    subtasks: HashMap<Uuid, SubTask>,
    model_endpoints: HashMap<String, String>,
) -> DispatchFn {
    Arc::new(
        move |subtask_id: Uuid, decision: RouteDecision, assignment: CrewAssignment| {
            let state = state.clone();
            let subtasks = subtasks.clone();
            let model_endpoints = model_endpoints.clone();

            tokio::spawn(async move {
                let started_at = Utc::now();
                let prompt = subtasks
                    .get(&subtask_id)
                    .map(|st| st.prompt.clone())
                    .unwrap_or_default();

                let system = if let Some(extra) = &assignment.extra_instructions {
                    format!("{}\n\n{}", assignment.role.system_prompt(), extra)
                } else {
                    assignment.role.system_prompt().to_string()
                };

                let body = json!({
                    "model": decision.model_id,
                    "messages": [
                        {"role": "system", "content": system},
                        {"role": "user", "content": prompt}
                    ],
                    "stream": false,
                });

                let endpoint = model_endpoints
                    .get(&decision.model_id)
                    .cloned()
                    .unwrap_or_else(|| format!("http://{}", decision.endpoint));

                let url = ff_core::url::normalize_chat_completions_url(&endpoint);

                let resp = match state.http_client.post(&url).json(&body).send().await {
                    Ok(r) => r,
                    Err(e) => {
                        ff_observability::metrics::ORCHESTRATE_SUBTASKS_TOTAL
                            .with_label_values(&["failed"])
                            .inc();
                        return SubTaskResult {
                            subtask_id,
                            status: SubTaskStatus::Failed,
                            output: String::new(),
                            error: Some(format!("HTTP dispatch failed: {e}")),
                            model_id: Some(decision.model_id),
                            worker_name: Some(decision.worker_name),
                            started_at: Some(started_at),
                            completed_at: Some(Utc::now()),
                            duration_ms: Some((Utc::now() - started_at).num_milliseconds() as u64),
                            tokens_used: None,
                        };
                    }
                };

                let status_code = resp.status();
                let body_text = match resp.text().await {
                    Ok(t) => t,
                    Err(e) => {
                        ff_observability::metrics::ORCHESTRATE_SUBTASKS_TOTAL
                            .with_label_values(&["failed"])
                            .inc();
                        return SubTaskResult {
                            subtask_id,
                            status: SubTaskStatus::Failed,
                            output: String::new(),
                            error: Some(format!("Failed to read response body: {e}")),
                            model_id: Some(decision.model_id),
                            worker_name: Some(decision.worker_name),
                            started_at: Some(started_at),
                            completed_at: Some(Utc::now()),
                            duration_ms: Some((Utc::now() - started_at).num_milliseconds() as u64),
                            tokens_used: None,
                        };
                    }
                };

                if !status_code.is_success() {
                    ff_observability::metrics::ORCHESTRATE_SUBTASKS_TOTAL
                        .with_label_values(&["failed"])
                        .inc();
                    return SubTaskResult {
                        subtask_id,
                        status: SubTaskStatus::Failed,
                        output: String::new(),
                        error: Some(format!("Upstream returned {status_code}: {body_text}")),
                        model_id: Some(decision.model_id),
                        worker_name: Some(decision.worker_name),
                        started_at: Some(started_at),
                        completed_at: Some(Utc::now()),
                        duration_ms: Some((Utc::now() - started_at).num_milliseconds() as u64),
                        tokens_used: None,
                    };
                }

                let parsed: Value = match serde_json::from_str(&body_text) {
                    Ok(v) => v,
                    Err(_) => {
                        // Non-JSON success — treat body as raw output
                        return SubTaskResult {
                            subtask_id,
                            status: SubTaskStatus::Completed,
                            output: body_text,
                            error: None,
                            model_id: Some(decision.model_id),
                            worker_name: Some(decision.worker_name),
                            started_at: Some(started_at),
                            completed_at: Some(Utc::now()),
                            duration_ms: Some((Utc::now() - started_at).num_milliseconds() as u64),
                            tokens_used: None,
                        };
                    }
                };

                let content = parsed
                    .get("choices")
                    .and_then(|c| c.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|choice| choice.get("message"))
                    .and_then(|msg| msg.get("content"))
                    .and_then(|c| c.as_str())
                    .unwrap_or(&body_text)
                    .to_string();

                let prompt_tokens = parsed
                    .get("usage")
                    .and_then(|u| u.get("prompt_tokens"))
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0) as u32;
                let completion_tokens = parsed
                    .get("usage")
                    .and_then(|u| u.get("completion_tokens"))
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0) as u32;
                let total_tokens = prompt_tokens + completion_tokens;
                let tokens_used = if total_tokens > 0 {
                    Some(total_tokens as u64)
                } else {
                    parsed
                        .get("usage")
                        .and_then(|u| u.get("total_tokens"))
                        .and_then(|t| t.as_u64())
                };
                let latency_ms = (Utc::now() - started_at).num_milliseconds() as u64;

                // Record cost tracking
                let record = TokenUsageRecord::new(
                    uuid::Uuid::new_v4().to_string(),
                    &decision.model_id,
                    &decision.worker_name,
                )
                .with_tokens(prompt_tokens, completion_tokens)
                .with_cost(0.0, true) // fleet inference is local / free
                .with_latency(latency_ms);

                state.cost_tracker.record_usage(record).await;

                // Update Prometheus counters
                ff_observability::metrics::LLM_TOKENS_TOTAL
                    .with_label_values(&[&decision.model_id, "prompt"])
                    .inc_by(prompt_tokens as u64);
                ff_observability::metrics::LLM_TOKENS_TOTAL
                    .with_label_values(&[&decision.model_id, "completion"])
                    .inc_by(completion_tokens as u64);
                ff_observability::metrics::LLM_COST_USD_TOTAL
                    .with_label_values(&[&decision.model_id, "true"])
                    .add(0.0);
                ff_observability::metrics::ORCHESTRATE_SUBTASKS_TOTAL
                    .with_label_values(&["completed"])
                    .inc();

                SubTaskResult {
                    subtask_id,
                    status: SubTaskStatus::Completed,
                    output: content,
                    error: None,
                    model_id: Some(decision.model_id),
                    worker_name: Some(decision.worker_name),
                    started_at: Some(started_at),
                    completed_at: Some(Utc::now()),
                    duration_ms: Some(latency_ms),
                    tokens_used,
                }
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ff_pulse::beat_v2::{
        Capabilities, ClusterInfo, DbTopology, DockerStatus, HardwareInfo, LlmMemoryUsage,
        LlmServer, LlmServerModel, LoadInfo, MemoryInfo, NetworkInfo, PulseBeatV2,
    };

    fn mock_beat() -> PulseBeatV2 {
        PulseBeatV2 {
            pulse_protocol_version: 2,
            computer_id: Some(Uuid::new_v4()),
            computer_name: "test-node".to_string(),
            timestamp: Utc::now(),
            epoch: 1,
            role_claimed: "member".to_string(),
            dispatch_tick_at: None,
            boot_id: None,
            system_uptime_secs: None,
            election_priority: 0,
            is_yielding: false,
            going_offline: false,
            maintenance_mode: false,
            network: NetworkInfo {
                primary_ip: "192.168.1.100".to_string(),
                all_ips: vec![],
            },
            hardware: HardwareInfo {
                cpu_cores: 8,
                ram_gb: 32,
                disk_gb: 500,
                gpu: Some("Apple M3 Max".to_string()),
            },
            load: LoadInfo {
                cpu_pct: 10.0,
                ram_pct: 40.0,
                disk_free_gb: 200.0,
                gpu_pct: 25.0,
                active_inference_requests: 2,
                active_agent_sessions: 0,
            },
            memory: MemoryInfo {
                ram_total_gb: 32.0,
                ram_used_gb: 12.0,
                ram_free_gb: 20.0,
                llm_ram_allocated_gb: 8.0,
                ram_available_for_new_llm_gb: 16.0,
                vram_total_gb: Some(48.0),
                vram_used_gb: Some(12.0),
                vram_free_gb: Some(36.0),
                llm_vram_allocated_gb: Some(8.0),
            },
            capabilities: Capabilities {
                can_serve_ff_gateway: true,
                can_serve_openclaw_gateway: false,
                can_host_postgres_replica: false,
                can_host_redis_replica: false,
                gpu_kind: "apple_silicon".to_string(),
                gpu_count: 1,
                gpu_vram_gb: Some(48.0),
                gpu_total_vram_gb: Some(48.0),
                can_run_cuda: false,
                can_run_metal: true,
                can_run_rocm: false,
                recommended_runtimes: vec!["mlx".to_string()],
                max_runnable_model_gb: Some(40.0),
            },
            llm_servers: vec![LlmServer {
                deployment_id: Uuid::new_v4(),
                runtime: "mlx".to_string(),
                endpoint: "http://192.168.1.100:8080".to_string(),
                openai_compatible: true,
                model: LlmServerModel {
                    id: "qwen3-32b".to_string(),
                    display_name: "Qwen3 32B".to_string(),
                    loaded_path: "/models/qwen3-32b-4bit.gguf".to_string(),
                    context_window: 32768,
                    parallel_slots: 4,
                },
                status: "active".to_string(),
                pid: Some(1234),
                started_at: Utc::now(),
                cluster: ClusterInfo {
                    cluster_id: None,
                    role: "single".to_string(),
                    tensor_parallel_size: 1,
                    pipeline_parallel_size: 1,
                    peers: vec![],
                },
                queue_depth: 1,
                active_requests: 2,
                tokens_per_sec_last_min: 45.0,
                gpu_memory_used_gb: Some(8.0),
                is_healthy: true,
                last_probed_at: Utc::now(),
                memory_used: LlmMemoryUsage {
                    model_weights_gb: 18.0,
                    kv_cache_gb: 2.0,
                    overhead_gb: 1.0,
                    total_gb: 21.0,
                },
            }],
            available_models: vec![],
            installed_software: vec![],
            docker: DockerStatus {
                daemon_running: false,
                total_cpu_pct: 0.0,
                total_memory_mb: 0.0,
                memory_limit_mb: 0.0,
                projects: vec![],
            },
            peers_seen: vec![],
            db_topology: DbTopology {
                postgres_primary: None,
                postgres_replicas: vec![],
                redis_primary: None,
                redis_replicas: vec![],
            },
            config_version: None,
            multi_host_participation: None,
            encountered_bugs: vec![],
            local_tasks: vec![],
            receivers: vec![],
            os: Default::default(),
            build_sha: None,
            source_tree_path: None,
        }
    }

    #[test]
    fn pulse_beats_to_fleet_state_produces_node_model_and_load() {
        let beat = mock_beat();
        let catalog = HashMap::new();
        let (nodes, models, loads) = pulse_beats_to_fleet_state(vec![beat], &catalog);

        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].name, "test-node");
        assert_eq!(nodes[0].host, "192.168.1.100");
        assert_eq!(nodes[0].hardware.gpu, GpuType::AppleSilicon);
        assert_eq!(nodes[0].models, vec!["qwen3-32b"]);

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "qwen3-32b");
        assert_eq!(models[0].name, "Qwen3 32B");
        assert_eq!(models[0].tier, Tier::Tier2);
        assert_eq!(models[0].path, "/models/qwen3-32b-4bit.gguf");
        assert_eq!(models[0].ctx_size, 32768);
        assert_eq!(models[0].runtime, Runtime::Mlx);

        let load = loads.get("test-node").expect("load missing");
        assert_eq!(load.active_requests, 2);
        assert_eq!(load.queue_depth, 1);
        assert_eq!(load.max_concurrent, 4);
        // avg_latency_ms = 1000 / 45 ≈ 22
        assert!(load.avg_latency_ms > 0 && load.avg_latency_ms < 100);
    }

    #[test]
    fn catalog_overrides_tier_inference() {
        let mut beat = mock_beat();
        beat.llm_servers[0].model.id = "custom-model".to_string();

        let mut catalog = HashMap::new();
        catalog.insert("custom-model".to_string(), ("Custom".to_string(), 4));

        let (_, models, _) = pulse_beats_to_fleet_state(vec![beat], &catalog);
        assert_eq!(models[0].tier, Tier::Tier4);
    }

    #[test]
    fn offline_beats_are_filtered_out() {
        let mut beat = mock_beat();
        beat.going_offline = true;
        let catalog = HashMap::new();
        let (nodes, models, loads) = pulse_beats_to_fleet_state(vec![beat], &catalog);
        assert!(nodes.is_empty());
        assert!(models.is_empty());
        assert!(loads.is_empty());
    }

    #[test]
    fn unhealthy_servers_are_excluded() {
        let mut beat = mock_beat();
        beat.llm_servers[0].is_healthy = false;
        let catalog = HashMap::new();
        let (_, models, _) = pulse_beats_to_fleet_state(vec![beat], &catalog);
        assert!(models.is_empty());
    }

    #[test]
    fn infer_tier_from_model_name() {
        assert_eq!(infer_tier("llama-3-70b"), Tier::Tier4);
        assert_eq!(infer_tier("qwen3-32b"), Tier::Tier2);
        assert_eq!(infer_tier("gemma-2b"), Tier::Tier1);
        // 0.5b is now correctly parsed as a real (tiny) size → Tier1 (was the
        // unknown-default Tier2 under the old substring list).
        assert_eq!(infer_tier("qwen3-0.5b"), Tier::Tier1);
        assert_eq!(infer_tier("unknown"), Tier::Tier2);
        // BUG FIXES: sizes the old substring list misclassified.
        assert_eq!(infer_tier("model-128b"), Tier::Tier4); // contained "8b" → was Tier1
        assert_eq!(infer_tier("qwen3-235b-a22b"), Tier::Tier4); // matched nothing → was Tier2
        assert_eq!(infer_tier("llama-3.1-405b"), Tier::Tier4);
        // A quant marker is NOT a parameter size.
        assert_eq!(infer_tier("qwen3-coder-30b-a3b-8bit"), Tier::Tier2); // 30B total
    }

    #[test]
    fn infer_params_b_from_model_name() {
        assert_eq!(infer_params_b("llama-3-70b"), 70.0);
        assert_eq!(infer_params_b("qwen3-32b"), 32.0);
        assert_eq!(infer_params_b("gemma-2b"), 2.0);
        assert_eq!(infer_params_b("qwen3-0.5b"), 0.5); // now parsed correctly (was 7.0)
        assert_eq!(infer_params_b("unknown"), 7.0);
        // BUG FIXES.
        assert_eq!(infer_params_b("model-128b"), 128.0); // was 8.0 (contained "8b")
        assert_eq!(infer_params_b("qwen3-235b-a22b"), 235.0); // was 7.0 (no pattern); MoE → total
        assert_eq!(infer_params_b("qwen3-coder-30b-a3b"), 30.0); // MoE total, not 3B active
        assert_eq!(infer_params_b("llama-3.1-8b"), 8.0); // version 3.1 ignored
        assert_eq!(infer_params_b("qwen2.5-coder-7b-4bit"), 7.0); // quant marker ignored
    }

    #[test]
    fn map_runtime_variants() {
        assert_eq!(map_runtime("llama.cpp"), Some(Runtime::LlamaCpp));
        assert_eq!(map_runtime("vllm"), Some(Runtime::Vllm));
        assert_eq!(map_runtime("mlx"), Some(Runtime::Mlx));
        assert_eq!(map_runtime("unknown"), None);
    }

    #[test]
    fn map_gpu_kind_variants() {
        assert_eq!(map_gpu_kind("apple_silicon"), GpuType::AppleSilicon);
        assert_eq!(map_gpu_kind("nvidia_cuda"), GpuType::NvidiaCuda);
        assert_eq!(map_gpu_kind("amd_rocm"), GpuType::AmdRdna);
        assert_eq!(map_gpu_kind("intel_gpu"), GpuType::IntelGpu);
        assert_eq!(map_gpu_kind("none"), GpuType::None);
    }
}
