//! POST /v1/orchestrate — multi-model task decomposition and execution.
//!
//! Accepts a complex task description, decomposes it into subtasks,
//! routes each subtask to the best available model/node in the fleet,
//! executes them in parallel where dependencies allow, and returns
//! aggregated results.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use tracing::{Instrument, info, info_span, warn};
use uuid::Uuid;

use ff_core::Node as CoreNode;
use ff_orchestrator::crew::CrewAssignment;
use ff_orchestrator::decomposer::Decomposer;
use ff_orchestrator::parallel::ParallelExecutor;
use ff_orchestrator::planner::Planner;
use ff_orchestrator::router::{RouteConstraints, RouteDecision, TaskRouter};

use crate::llm_routing::CircuitBreaker;
use crate::orchestrator_adapter::{build_dispatch_fn, pulse_beats_to_fleet_state};

/// Request body for the orchestrate endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct OrchestrateRequest {
    /// The high-level task to decompose and execute.
    pub task: String,
    /// Optional routing constraints (tier limits, excluded nodes, etc.).
    #[serde(default)]
    pub constraints: RouteConstraints,
    /// If true, abort remaining subtasks on first failure.
    #[serde(default)]
    pub fail_fast: bool,
}

/// Handler for `POST /v1/orchestrate`.
pub async fn handle_orchestrate(
    State(state): State<Arc<crate::GatewayState>>,
    Json(req): Json<OrchestrateRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let start = Instant::now();

    let result = do_orchestrate(state, req).await;

    let duration = start.elapsed().as_secs_f64();
    ff_observability::metrics::ORCHESTRATE_DURATION_SECONDS.observe(duration);

    match &result {
        Ok(_) => {
            ff_observability::metrics::ORCHESTRATE_REQUESTS_TOTAL
                .with_label_values(&["success"])
                .inc();
        }
        Err((status, _)) => {
            let label = if status == &StatusCode::BAD_REQUEST {
                "planning_failed"
            } else if status == &StatusCode::SERVICE_UNAVAILABLE {
                "routing_failed"
            } else {
                "execute_failed"
            };
            ff_observability::metrics::ORCHESTRATE_REQUESTS_TOTAL
                .with_label_values(&[label])
                .inc();
        }
    }

    result
}

async fn do_orchestrate(
    state: Arc<crate::GatewayState>,
    req: OrchestrateRequest,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // 1. Decompose
    let decomposition = async {
        let decomposer = Decomposer::new();
        let decomposition = decomposer.decompose(&req.task);
        info!(
            subtask_count = decomposition.subtasks.len(),
            "orchestrate: task decomposed"
        );
        decomposition
    }
    .instrument(info_span!("orchestrate_decompose"))
    .await;

    // 2. Plan
    let plan = async {
        match Planner::plan(&decomposition) {
            Ok(p) => Ok(p),
            Err(e) => {
                warn!(error = %e, "orchestrate: planning failed");
                Err((
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("planning failed: {e}")})),
                ))
            }
        }
    }
    .instrument(info_span!("orchestrate_plan"))
    .await?;

    // 3. Build fleet snapshot from Pulse beats
    let router = match state.pulse_router.as_ref() {
        Some(r) => r,
        None => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Pulse router not available"})),
            ));
        }
    };

    let beats = match router.all_beats().await {
        Ok(b) => b,
        Err(e) => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": format!("pulse reader error: {e}")})),
            ));
        }
    };

    let circuit_breaker = router.circuit_breaker();

    // Extract actual inference endpoints before we consume beats.
    let mut model_endpoints: HashMap<String, String> = HashMap::new();
    for beat in &beats {
        for server in &beat.llm_servers {
            if server.status == "active" && server.is_healthy && !server.endpoint.is_empty() {
                model_endpoints.insert(server.model.id.clone(), server.endpoint.clone());
            }
        }
    }

    // Query catalog from DB if available
    let catalog = if let Some(pool) = state.operational_store.as_ref().and_then(|s| s.pg_pool()) {
        match query_model_catalog(pool).await {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "orchestrate: catalog query failed, using heuristics");
                HashMap::new()
            }
        }
    } else {
        HashMap::new()
    };

    let (nodes, models, loads) = pulse_beats_to_fleet_state(beats, &catalog);

    if nodes.is_empty() || models.is_empty() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "no active models or nodes in fleet"})),
        ));
    }

    // 4. Route each subtask
    let routes = async {
        let task_router = TaskRouter::new(nodes.clone(), models, loads);
        let mut routes = HashMap::new();
        for subtask in &decomposition.subtasks {
            match task_router.route(subtask, &req.constraints) {
                Some(decision) => {
                    routes.insert(subtask.id, decision);
                }
                None => {
                    warn!(subtask_id = %subtask.id, "orchestrate: no viable route for subtask");
                    return Err((
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({
                            "error": format!("no viable route for subtask {}", subtask.id),
                            "subtask": subtask,
                        })),
                    ));
                }
            }
        }
        Ok(routes)
    }
    .instrument(info_span!("orchestrate_route"))
    .await?;

    // 5. Post-filter routes through circuit breaker
    let routes = resolve_routes_with_circuit_breaker(
        routes,
        circuit_breaker.as_ref(),
        &nodes,
        &model_endpoints,
    )
    .await?;

    // 6. Build crew assignments
    let assignments: HashMap<_, _> = decomposition
        .subtasks
        .iter()
        .map(|st| {
            let assignment = CrewAssignment::auto(st.id, st.task_type);
            (st.id, assignment)
        })
        .collect();

    // 7. Execute
    let executor = ParallelExecutor::new(&plan, routes.clone(), assignments, req.fail_fast);
    let subtask_map: HashMap<_, _> = decomposition
        .subtasks
        .into_iter()
        .map(|s| (s.id, s))
        .collect();
    let dispatch = build_dispatch_fn(state, subtask_map, model_endpoints);

    info!(
        stage_count = plan.stages.len(),
        "orchestrate: executing plan"
    );
    let result = executor
        .execute(&plan, dispatch)
        .instrument(info_span!("orchestrate_execute"))
        .await;

    // 8. Build response
    let routing_alternatives: HashMap<String, Vec<Value>> = routes
        .iter()
        .map(|(id, decision)| {
            let alts: Vec<Value> = decision
                .alternatives
                .iter()
                .map(|alt| {
                    json!({
                        "model_id": &alt.model_id,
                        "worker_name": &alt.worker_name,
                        "score": alt.total,
                    })
                })
                .collect();
            (id.to_string(), alts)
        })
        .collect();

    let failures: Vec<Value> = result
        .results
        .iter()
        .filter(|r| r.error.is_some())
        .map(|r| {
            json!({
                "subtask_id": r.subtask_id,
                "model_id": r.model_id,
                "worker_name": r.worker_name,
                "error": r.error,
            })
        })
        .collect();

    let response = json!({
        "success": result.success,
        "combined_output": result.combined_output(),
        "results": result.results,
        "stage_count": plan.stages.len(),
        "total_duration_ms": result.total_duration_ms,
        "plan_id": result.plan_id,
        "started_at": result.started_at,
        "completed_at": result.completed_at,
        "routing_alternatives": routing_alternatives,
        "failures": failures,
    });

    Ok(Json(response))
}

/// After the TaskRouter picks winners, verify each winner's node is not
/// circuit-broken. If it is, try alternatives in score order. If all
/// alternatives are also broken, return 503.
async fn resolve_routes_with_circuit_breaker(
    routes: HashMap<Uuid, RouteDecision>,
    cb: Option<&Arc<CircuitBreaker>>,
    nodes: &[CoreNode],
    model_endpoints: &HashMap<String, String>,
) -> Result<HashMap<Uuid, RouteDecision>, (StatusCode, Json<Value>)> {
    let Some(cb) = cb else {
        return Ok(routes);
    };

    let mut resolved = HashMap::with_capacity(routes.len());

    for (subtask_id, decision) in routes {
        if !cb.is_open(&decision.worker_name) {
            resolved.insert(subtask_id, decision);
            continue;
        }

        tracing::warn!(
            subtask_id = %subtask_id,
            node = %decision.worker_name,
            "circuit breaker open for winner, trying alternatives"
        );

        ff_observability::metrics::PULSE_CIRCUIT_BREAKER_TRIPS_TOTAL
            .with_label_values(&[&decision.worker_name])
            .inc();

        let mut found = false;
        for alt in &decision.alternatives {
            if cb.is_open(&alt.worker_name) {
                continue;
            }

            // Reconstruct endpoint from model_endpoints or node host:port
            let endpoint = model_endpoints
                .get(&alt.model_id)
                .cloned()
                .unwrap_or_else(|| {
                    nodes
                        .iter()
                        .find(|n| n.name == alt.worker_name)
                        .map(|n| format!("{}:{}", n.host, n.port))
                        .unwrap_or_else(|| decision.endpoint.clone())
                });

            let fallback = RouteDecision {
                subtask_id,
                model_id: alt.model_id.clone(),
                worker_name: alt.worker_name.clone(),
                endpoint,
                score: alt.clone(),
                alternatives: vec![], // Simplify: don't chain further
                decided_at: Utc::now(),
            };

            resolved.insert(subtask_id, fallback);
            found = true;
            break;
        }

        if !found {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": format!(
                        "circuit breaker open for subtask {} and all alternatives",
                        subtask_id
                    ),
                    "subtask_id": subtask_id,
                    "rejected_node": decision.worker_name,
                })),
            ));
        }
    }

    Ok(resolved)
}

/// Query `fleet_model_catalog` for tier + name metadata.
async fn query_model_catalog(
    pool: &sqlx::PgPool,
) -> Result<HashMap<String, (String, i32)>, sqlx::Error> {
    let rows = sqlx::query(
        r#"
        SELECT id, name, tier
        FROM fleet_model_catalog
        "#,
    )
    .fetch_all(pool)
    .await?;

    let mut out = HashMap::new();
    for row in rows {
        let id: String = row.try_get("id")?;
        let name: String = row.try_get("name")?;
        let tier: i32 = row.try_get("tier")?;
        out.insert(id, (name, tier));
    }

    Ok(out)
}

// ─── Integration tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ff_orchestrator::parallel::{DispatchFn, SubTaskStatus};
    use ff_orchestrator::router::ModelScore;
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
    fn circuit_breaker_filters_broken_node() {
        let cb = CircuitBreaker::new();
        cb.record_failure("bad-node");
        cb.record_failure("bad-node");
        cb.record_failure("bad-node"); // breaker open

        let mut routes = HashMap::new();
        let decision = RouteDecision {
            subtask_id: Uuid::new_v4(),
            model_id: "model-a".to_string(),
            worker_name: "bad-node".to_string(),
            endpoint: "http://bad-node:8080".to_string(),
            score: ModelScore {
                model_id: "model-a".to_string(),
                worker_name: "bad-node".to_string(),
                specialty_score: 1.0,
                health_score: 1.0,
                load_score: 1.0,
                hardware_score: 1.0,
                tier_score: 1.0,
                total: 5.0,
            },
            alternatives: vec![ModelScore {
                model_id: "model-b".to_string(),
                worker_name: "good-node".to_string(),
                specialty_score: 0.8,
                health_score: 1.0,
                load_score: 1.0,
                hardware_score: 1.0,
                tier_score: 1.0,
                total: 4.8,
            }],
            decided_at: Utc::now(),
        };
        routes.insert(decision.subtask_id, decision);

        let nodes = vec![CoreNode {
            id: Uuid::new_v4(),
            name: "good-node".to_string(),
            host: "192.168.1.101".to_string(),
            port: 55000,
            role: ff_core::Role::Worker,
            election_priority: 0,
            status: ff_core::NodeStatus::Online,
            hardware: ff_core::Hardware {
                os: ff_core::OsType::Linux,
                cpu_model: "test".to_string(),
                cpu_cores: 8,
                gpu: ff_core::GpuType::None,
                gpu_model: None,
                memory_gib: 32,
                memory_type: ff_core::MemoryType::Unknown,
                interconnect: ff_core::Interconnect::Unknown,
                runtimes: vec![],
            },
            models: vec!["model-b".to_string()],
            last_heartbeat: Some(Utc::now()),
            registered_at: Utc::now(),
        }];

        let model_endpoints = HashMap::new();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let resolved = rt
            .block_on(resolve_routes_with_circuit_breaker(
                routes,
                Some(&Arc::new(cb)),
                &nodes,
                &model_endpoints,
            ))
            .unwrap();

        assert_eq!(resolved.len(), 1);
        let (_, d) = resolved.into_iter().next().unwrap();
        assert_eq!(d.worker_name, "good-node");
        assert_eq!(d.model_id, "model-b");
    }

    #[test]
    fn circuit_breaker_all_alternatives_open_returns_err() {
        let cb = CircuitBreaker::new();
        cb.record_failure("bad-node");
        cb.record_failure("bad-node");
        cb.record_failure("bad-node");

        let mut routes = HashMap::new();
        let decision = RouteDecision {
            subtask_id: Uuid::new_v4(),
            model_id: "model-a".to_string(),
            worker_name: "bad-node".to_string(),
            endpoint: "http://bad-node:8080".to_string(),
            score: ModelScore {
                model_id: "model-a".to_string(),
                worker_name: "bad-node".to_string(),
                specialty_score: 1.0,
                health_score: 1.0,
                load_score: 1.0,
                hardware_score: 1.0,
                tier_score: 1.0,
                total: 5.0,
            },
            alternatives: vec![ModelScore {
                model_id: "model-b".to_string(),
                worker_name: "bad-node".to_string(),
                specialty_score: 0.8,
                health_score: 1.0,
                load_score: 1.0,
                hardware_score: 1.0,
                tier_score: 1.0,
                total: 4.8,
            }],
            decided_at: Utc::now(),
        };
        routes.insert(decision.subtask_id, decision);

        let nodes = vec![];
        let model_endpoints = HashMap::new();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(resolve_routes_with_circuit_breaker(
            routes,
            Some(&Arc::new(cb)),
            &nodes,
            &model_endpoints,
        ));

        assert!(result.is_err());
    }

    #[test]
    fn circuit_breaker_none_passes_through() {
        let routes = HashMap::new();
        let nodes = vec![];
        let model_endpoints = HashMap::new();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let resolved = rt
            .block_on(resolve_routes_with_circuit_breaker(
                routes,
                None,
                &nodes,
                &model_endpoints,
            ))
            .unwrap();

        assert!(resolved.is_empty());
    }

    #[tokio::test]
    async fn full_orchestration_flow_with_mock_dispatch() {
        let beat = mock_beat();
        let catalog = HashMap::new();
        let (nodes, models, loads) = pulse_beats_to_fleet_state(vec![beat], &catalog);

        let task = "Summarize Rust async patterns and give code examples.";
        let decomposition = Decomposer::new().decompose(task);
        let plan = Planner::plan(&decomposition).unwrap();
        let task_router = TaskRouter::new(nodes, models, loads);
        let mut routes = HashMap::new();
        for subtask in &decomposition.subtasks {
            let decision = task_router.route(subtask, &RouteConstraints::default());
            assert!(decision.is_some(), "routing failed for subtask");
            routes.insert(subtask.id, decision.unwrap());
        }

        let assignments: HashMap<_, _> = decomposition
            .subtasks
            .iter()
            .map(|st| {
                let assignment = CrewAssignment::auto(st.id, st.task_type);
                (st.id, assignment)
            })
            .collect();

        // Mock dispatch that returns synthetic results
        let mock_dispatch: DispatchFn = Arc::new(|subtask_id, _decision, _assignment| {
            tokio::spawn(async move {
                ff_orchestrator::parallel::SubTaskResult {
                    subtask_id,
                    status: SubTaskStatus::Completed,
                    output: format!("Result for {subtask_id}"),
                    error: None,
                    model_id: Some("qwen3-32b".to_string()),
                    worker_name: Some("test-node".to_string()),
                    started_at: Some(Utc::now()),
                    completed_at: Some(Utc::now()),
                    duration_ms: Some(100),
                    tokens_used: Some(42),
                }
            })
        });

        let executor = ParallelExecutor::new(&plan, routes, assignments, false);
        let result = executor.execute(&plan, mock_dispatch).await;

        assert!(result.success);
        assert!(!result.results.is_empty());
        assert!(result.combined_output().contains("Result for"));
    }
}
