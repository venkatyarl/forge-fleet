//! MCP request handlers — one per ForgeFleet tool.
//!
//! Each handler accepts JSON params and returns a JSON result.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use ff_api::adaptive_router::AdaptiveRouter;
use ff_api::classifier::TaskType;
use ff_api::quality_tracker::{Outcome, QualityTracker, QualityTrackerConfig};
use ff_api::registry::{BackendEndpoint, BackendRegistry};
use ff_api::router::{TierRouter, TierRouterConfig, TierTimeouts};
use ff_api::types::{ChatCompletionRequest, ChatMessage};
use ff_core::config::{self, DatabaseMode, FleetConfig};
use ff_db::{DbPool, DbPoolConfig, FleetModelRow, OperationalStore, run_migrations};
use ff_discovery::health::{HealthMonitor, HealthStatus, HealthTarget};
use ff_discovery::ports::known_llm_ports;
use ff_discovery::scanner::{
    NodeScanResult, NodeScanStatus, NodeScanner, ScannerConfig, build_scan_targets, scan_subnet,
};
use ff_orchestrator::decomposer::SubTaskType;
use ff_orchestrator::planner::Planner;
use ff_orchestrator::task_decomposer::TemplateDecomposer;
use ff_orchestrator::{
    AgentAssignment, AgentRole, DataSensitivity, DeploymentTarget, ExecutionPolicy,
    HumanApprovalLevel, ModelPreference, ProjectExecutionProfile, ProjectPolicyEngine,
    ReviewStrictness, TeamConfig, TeamTemplates,
};
use ff_pipeline::executor::ExecutorConfig;
use ff_pipeline::graph::PipelineGraph;
use ff_pipeline::step::{Step, StepId, StepKind, StepStatus};
use ff_pulse::PulseClient;
use ff_runtime::engine::EngineConfig;
use ff_runtime::model_manager::ModelManager;
use ff_runtime::process_manager::ProcessManager;
use ff_ssh::{RemoteExecutor, SshNodeConfig};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::postgres::PgPoolOptions;
use tracing::{info, warn};

use crate::federation;

/// Handler result type — all handlers return JSON or an error string.
pub type HandlerResult = std::result::Result<Value, String>;

const QUALITY_SNAPSHOT_KEY: &str = "ff_api.quality_snapshot";
const PROJECT_PROFILE_KEY_PREFIX: &str = "ff_mcp.project_profile.";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectProfileRecord {
    project_id: String,
    display_name: String,
    profile: ProjectExecutionProfile,
    policy: ExecutionPolicy,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AppliedProjectPolicy {
    project_id: String,
    policy: ExecutionPolicy,
    approval_required: bool,
    approval_reason: Option<String>,
}

// ─── Fleet Status ────────────────────────────────────────────────────────────

pub async fn fleet_status(params: Option<Value>) -> HandlerResult {
    let refresh = params
        .as_ref()
        .and_then(|p| p.get("refresh"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let (config, config_path) = load_config_auto()?;

    // ─── Primary source: Postgres. Fallback: fleet.toml ─────────────────────
    let (pg_nodes, pg_models) = match get_pg_pool(&config).await {
        Ok(pool) => {
            let nodes = ff_db::pg_list_nodes(&pool).await.unwrap_or_default();
            let models = ff_db::pg_list_models(&pool).await.unwrap_or_default();
            if nodes.is_empty() {
                info!("fleet_status: Postgres fleet_nodes empty, falling back to fleet.toml");
                (None, None)
            } else {
                info!(
                    nodes = nodes.len(),
                    models = models.len(),
                    "fleet_status: using Postgres as primary source"
                );
                (Some(nodes), Some(models))
            }
        }
        Err(e) => {
            warn!("fleet_status: Postgres unavailable ({e}), falling back to fleet.toml");
            (None, None)
        }
    };

    let using_postgres = pg_nodes.is_some();

    // Build scan targets from whichever source we're using
    let scan_targets = if let Some(ref db_nodes) = pg_nodes {
        let default_port = config.fleet.api_port;
        let node_tuples: Vec<(String, String, Option<u16>, u32)> = db_nodes
            .iter()
            .map(|n| {
                (
                    n.name.clone(),
                    n.ip.clone(),
                    Some(default_port),
                    n.election_priority as u32,
                )
            })
            .collect();
        build_scan_targets(node_tuples, default_port)
    } else {
        build_known_scan_targets(&config)
    };

    info!(
        refresh,
        targets = scan_targets.len(),
        source = if using_postgres {
            "postgres"
        } else {
            "fleet.toml"
        },
        "fleet_status handler called"
    );

    let scan_results = if scan_targets.is_empty() {
        Vec::new()
    } else {
        NodeScanner::new(scan_targets.clone()).scan_once().await
    };

    let scan_by_name: HashMap<String, NodeScanResult> = scan_results
        .iter()
        .map(|r| (r.name.clone(), r.clone()))
        .collect();

    // Best-effort: fetch live metrics from Redis (Fleet Pulse).
    // If Redis is unavailable, we fall back to scan-only data.
    let pulse_metrics: HashMap<String, ff_pulse::NodeMetrics> =
        match PulseClient::connect(&config.redis.url).await {
            Ok(mut pulse) => match pulse.get_all_metrics().await {
                Ok(snapshot) => snapshot
                    .nodes
                    .into_iter()
                    .map(|m| (m.node_name.clone(), m))
                    .collect(),
                Err(e) => {
                    warn!("fleet_status: Redis pulse fetch failed (non-fatal): {e}");
                    HashMap::new()
                }
            },
            Err(e) => {
                warn!("fleet_status: Redis connection failed (non-fatal): {e}");
                HashMap::new()
            }
        };

    let mut nodes_json = Vec::new();
    let mut healthy_nodes = 0usize;
    let mut degraded_nodes = 0usize;
    let mut offline_nodes = 0usize;
    let mut models_loaded = 0usize;

    // Group Postgres models by node_name for easy lookup
    let models_by_node: HashMap<String, Vec<&FleetModelRow>> =
        if let Some(ref db_models) = pg_models {
            let mut map: HashMap<String, Vec<&FleetModelRow>> = HashMap::new();
            for m in db_models {
                map.entry(m.node_name.clone()).or_default().push(m);
            }
            map
        } else {
            HashMap::new()
        };

    if let Some(ref db_nodes) = pg_nodes {
        // ── Postgres path ──────────────────────────────────────────────────
        for node in db_nodes {
            let maybe_scan = scan_by_name.get(&node.name);
            let status = maybe_scan
                .map(|r| scan_status_to_str(r.status))
                .unwrap_or("unknown");

            match status {
                "healthy" => healthy_nodes += 1,
                "degraded" => degraded_nodes += 1,
                "offline" => offline_nodes += 1,
                _ => {}
            }

            let mut models = Vec::new();
            if let Some(node_models) = models_by_node.get(&node.name) {
                for m in node_models {
                    let loaded = status == "healthy" || status == "degraded";
                    if loaded {
                        models_loaded += 1;
                    }
                    models.push(json!({
                        "id": m.slug,
                        "name": m.name,
                        "tier": m.tier,
                        "port": m.port,
                        "status": if loaded { "loaded" } else { "unreachable" }
                    }));
                }
            }

            let pulse = pulse_metrics.get(node.name.as_str());

            nodes_json.push(json!({
                "name": node.name,
                "ip": node.ip,
                "role": node.role,
                "status": status,
                "hardware": node.hardware,
                "ram_gb": node.ram_gb,
                "cpu_cores": node.cpu_cores,
                "os": node.os,
                "latency_ms": maybe_scan.map(|r| r.latency_ms),
                "http_status": maybe_scan.and_then(|r| r.http_status),
                "error": maybe_scan.and_then(|r| r.error.clone()),
                "models": models,
                "pulse": pulse.map(|m| json!({
                    "cpu_percent": m.cpu_percent,
                    "ram_used_gb": m.ram_used_gb,
                    "ram_total_gb": m.ram_total_gb,
                    "disk_used_gb": m.disk_used_gb,
                    "disk_total_gb": m.disk_total_gb,
                    "tokens_per_sec": m.tokens_per_sec,
                    "active_tasks": m.active_tasks,
                    "uptime_secs": m.uptime_secs,
                    "temperature_c": m.temperature_c
                }))
            }));
        }
    } else {
        // ── fleet.toml fallback path ───────────────────────────────────────
        for (name, node_cfg) in &config.nodes {
            let maybe_scan = scan_by_name.get(name);
            let status = maybe_scan
                .map(|r| scan_status_to_str(r.status))
                .unwrap_or("unknown");

            match status {
                "healthy" => healthy_nodes += 1,
                "degraded" => degraded_nodes += 1,
                "offline" => offline_nodes += 1,
                _ => {}
            }

            let mut models = Vec::new();
            for (slug, model) in &node_cfg.models {
                let model_name = if model.name.trim().is_empty() {
                    slug.clone()
                } else {
                    model.name.clone()
                };
                let port = model
                    .port
                    .or(node_cfg.port)
                    .unwrap_or(config.fleet.api_port);
                let loaded = status == "healthy" || status == "degraded";
                if loaded {
                    models_loaded += 1;
                }
                models.push(json!({
                    "id": slug,
                    "name": model_name,
                    "tier": model.tier,
                    "port": port,
                    "status": if loaded { "loaded" } else { "unreachable" }
                }));
            }

            let pulse = pulse_metrics.get(name.as_str());

            nodes_json.push(json!({
                "name": name,
                "ip": node_cfg.ip,
                "role": format!("{}", node_cfg.role),
                "status": status,
                "latency_ms": maybe_scan.map(|r| r.latency_ms),
                "http_status": maybe_scan.and_then(|r| r.http_status),
                "error": maybe_scan.and_then(|r| r.error.clone()),
                "models": models,
                "pulse": pulse.map(|m| json!({
                    "cpu_percent": m.cpu_percent,
                    "ram_used_gb": m.ram_used_gb,
                    "ram_total_gb": m.ram_total_gb,
                    "disk_used_gb": m.disk_used_gb,
                    "disk_total_gb": m.disk_total_gb,
                    "tokens_per_sec": m.tokens_per_sec,
                    "active_tasks": m.active_tasks,
                    "uptime_secs": m.uptime_secs,
                    "temperature_c": m.temperature_c
                }))
            }));
        }
    }

    let total_nodes = if using_postgres {
        pg_nodes.as_ref().map_or(0, |n| n.len())
    } else {
        config.nodes.len()
    };

    Ok(json!({
        "refresh": refresh,
        "config_path": config_path,
        "source": if using_postgres { "postgres" } else { "fleet.toml" },
        "nodes": nodes_json,
        "summary": {
            "total_nodes": total_nodes,
            "healthy": healthy_nodes,
            "degraded": degraded_nodes,
            "offline": offline_nodes,
            "models_loaded": models_loaded,
            "pulse_online": pulse_metrics.len()
        },
        "scanned_at": Utc::now().to_rfc3339(),
        "scan_results": scan_results
    }))
}

// ─── Fleet Config ────────────────────────────────────────────────────────────

pub async fn fleet_config(params: Option<Value>) -> HandlerResult {
    let action = params
        .as_ref()
        .and_then(|p| p.get("action"))
        .and_then(|v| v.as_str())
        .unwrap_or("get_all");

    info!(action, "fleet_config handler called");

    let (config, config_path) = load_config_auto()?;

    match action {
        "get_all" => Ok(json!({
            "action": action,
            "config_path": config_path,
            "config": config
        })),
        "get_nodes" => Ok(json!({
            "action": action,
            "config_path": config_path,
            "nodes": config.nodes
        })),
        "get_node" => {
            let key = params
                .as_ref()
                .and_then(|p| p.get("key"))
                .and_then(|v| v.as_str())
                .or_else(|| {
                    params
                        .as_ref()
                        .and_then(|p| p.get("node"))
                        .and_then(|v| v.as_str())
                })
                .ok_or_else(|| "get_node requires 'key' (or 'node')".to_string())?;

            let node = config
                .nodes
                .get(key)
                .ok_or_else(|| format!("node '{key}' not found"))?;

            Ok(json!({
                "action": action,
                "config_path": config_path,
                "node": {
                    "name": key,
                    "config": node
                }
            }))
        }
        "get_services" => Ok(json!({
            "action": action,
            "config_path": config_path,
            "services": config.services
        })),
        "set" => {
            let key = params
                .as_ref()
                .and_then(|p| p.get("key"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| "set action requires 'key'".to_string())?;
            let raw_value = params
                .as_ref()
                .and_then(|p| p.get("value"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| "set action requires 'value'".to_string())?;

            let mut cfg_json = serde_json::to_value(&config)
                .map_err(|e| format!("failed to convert config to JSON: {e}"))?;
            let parsed_value = parse_value_string(raw_value);
            set_json_dot_path(&mut cfg_json, key, parsed_value.clone())?;

            let updated: FleetConfig = serde_json::from_value(cfg_json)
                .map_err(|e| format!("invalid config mutation for key '{key}': {e}"))?;

            let rendered = toml::to_string_pretty(&updated)
                .map_err(|e| format!("failed to serialize config as TOML: {e}"))?;
            std::fs::write(&config_path, rendered)
                .map_err(|e| format!("failed writing config '{}': {e}", config_path.display()))?;

            Ok(json!({
                "action": "set",
                "config_path": config_path,
                "key": key,
                "value": parsed_value,
                "success": true,
                "updated_at": Utc::now().to_rfc3339()
            }))
        }
        _ => Err(format!("unknown config action: {action}")),
    }
}

// ─── Fleet SSH ───────────────────────────────────────────────────────────────

pub async fn fleet_ssh(params: Option<Value>) -> HandlerResult {
    let node_ref = params
        .as_ref()
        .and_then(|p| p.get("node"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "fleet_ssh requires 'node'".to_string())?;
    let command = params
        .as_ref()
        .and_then(|p| p.get("command"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "fleet_ssh requires 'command'".to_string())?;
    let timeout_secs = params
        .as_ref()
        .and_then(|p| p.get("timeout"))
        .and_then(|v| v.as_u64())
        .unwrap_or(60);
    let use_sudo = params
        .as_ref()
        .and_then(|p| p.get("sudo"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    info!(
        node_ref,
        command, timeout_secs, use_sudo, "fleet_ssh handler called"
    );

    let (config, _config_path) = load_config_auto()?;
    let ssh_node = resolve_ssh_node(&config, params.as_ref(), node_ref)?;

    let executor = RemoteExecutor::new(timeout_secs, true);
    let result = executor
        .run_on_node(ssh_node.clone(), command.to_string(), use_sudo)
        .await
        .map_err(|e| format!("SSH execution failed: {e}"))?;

    Ok(json!({
        "node": result.node,
        "host": result.host,
        "command": result.command,
        "started_at": result.started_at,
        "duration_ms": result.duration_ms,
        "success": result.success,
        "exit_code": result.exit_code,
        "stdout": result.stdout,
        "stderr": result.stderr
    }))
}

// ─── Fleet Run ───────────────────────────────────────────────────────────────

pub async fn fleet_run(params: Option<Value>) -> HandlerResult {
    let prompt = params
        .as_ref()
        .and_then(|p| p.get("prompt"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "fleet_run requires 'prompt'".to_string())?;

    let mut start_tier = params
        .as_ref()
        .and_then(|p| p.get("start_tier"))
        .and_then(|v| v.as_u64())
        .unwrap_or(1)
        .clamp(1, 4) as u8;
    let mut max_tier = params
        .as_ref()
        .and_then(|p| p.get("max_tier"))
        .and_then(|v| v.as_u64())
        .unwrap_or(4)
        .clamp(1, 4) as u8;
    let model_selector = params
        .as_ref()
        .and_then(|p| p.get("model"))
        .and_then(|v| v.as_str())
        .unwrap_or("auto");

    let project_id = params
        .as_ref()
        .and_then(|p| p.get("project_id"))
        .and_then(|v| v.as_str());
    let approval_override = params
        .as_ref()
        .and_then(|p| p.get("approved"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut applied_policy: Option<AppliedProjectPolicy> = None;

    if let Some(project_id) = project_id {
        let policy = resolve_project_policy(project_id, prompt).await?;

        if !ProjectPolicyEngine::model_allowed(&policy.policy.routing, model_selector) {
            return Err(format!(
                "model selector '{model_selector}' is not allowed for project '{project_id}'"
            ));
        }

        let (clamped_start, clamped_max) =
            ProjectPolicyEngine::clamp_tiers(&policy.policy.routing, start_tier, max_tier);
        start_tier = clamped_start;
        max_tier = clamped_max;

        if policy.approval_required && !approval_override {
            return Err(format!(
                "human approval required for project '{project_id}': {} (set approved=true after review)",
                policy
                    .approval_reason
                    .clone()
                    .unwrap_or_else(|| "policy threshold triggered".to_string())
            ));
        }

        applied_policy = Some(policy);
    }

    info!(
        start_tier,
        max_tier,
        model_selector,
        project_id = ?project_id,
        "fleet_run handler called"
    );

    let (config, _config_path) = load_config_auto()?;

    let registry = Arc::new(BackendRegistry::new(
        healthy_backends_from_config(&config).await,
    ));

    let tier_router_cfg = TierRouterConfig {
        timeouts: TierTimeouts {
            tier1: Duration::from_secs(config.llm.timeouts.tier1.unwrap_or(30)),
            tier2: Duration::from_secs(config.llm.timeouts.tier2.unwrap_or(60)),
            tier3: Duration::from_secs(config.llm.timeouts.tier3.unwrap_or(120)),
            tier4: Duration::from_secs(config.llm.timeouts.tier4.unwrap_or(300)),
        },
        start_tier,
        max_tier,
        ..Default::default()
    };

    let tier_router = Arc::new(TierRouter::new(registry.clone(), tier_router_cfg));
    let quality_tracker = Arc::new(QualityTracker::new(QualityTrackerConfig {
        min_samples: 3,
        ..Default::default()
    }));

    if let Some(snapshot) = config_kv_get(QUALITY_SNAPSHOT_KEY).await
        && let Err(err) = quality_tracker.import_json(&snapshot)
    {
        warn!(error = %err, "failed to import quality snapshot");
    }

    let adaptive_router = AdaptiveRouter::with_defaults(
        registry.clone(),
        tier_router.clone(),
        quality_tracker.clone(),
    );

    let messages = vec![ChatMessage {
        role: "user".to_string(),
        content: Value::String(prompt.to_string()),
        name: None,
        extra: HashMap::new(),
    }];

    let (decision, mut chain) = adaptive_router
        .route(model_selector, &messages, Some(start_tier), Some(max_tier))
        .await;

    if let Some(policy) = applied_policy.as_ref() {
        if !policy.policy.routing.allowed_models.is_empty() {
            chain = chain
                .into_iter()
                .filter_map(|(tier, backends)| {
                    let retained: Vec<_> = backends
                        .into_iter()
                        .filter(|backend| {
                            policy
                                .policy
                                .routing
                                .allowed_models
                                .iter()
                                .any(|allowed| allowed.eq_ignore_ascii_case(&backend.model))
                        })
                        .collect();
                    if retained.is_empty() {
                        None
                    } else {
                        Some((tier, retained))
                    }
                })
                .collect();
        }
    }

    if chain.is_empty() {
        return Err(
            "no healthy backends available for requested tiers/profile constraints".to_string(),
        );
    }

    let client = reqwest::Client::new();
    let mut last_error = String::new();

    for (tier, backends) in chain {
        let timeout = tier_router.timeout_for_tier(tier);

        for backend in backends {
            let request = ChatCompletionRequest {
                model: backend.model.clone(),
                messages: messages.clone(),
                temperature: None,
                top_p: None,
                n: None,
                stream: Some(false),
                stop: None,
                max_tokens: None,
                presence_penalty: None,
                frequency_penalty: None,
                user: None,
                extra: HashMap::new(),
            };

            let started = Instant::now();
            let endpoint = format!("{}/v1/chat/completions", backend.base_url());

            match client
                .post(&endpoint)
                .timeout(timeout)
                .json(&request)
                .send()
                .await
            {
                Ok(response) => {
                    let latency = started.elapsed();

                    if !response.status().is_success() {
                        let status = response.status();
                        let body = response
                            .text()
                            .await
                            .unwrap_or_else(|_| "<failed reading error body>".to_string());

                        tier_router.record_failure(&backend.id, latency);
                        quality_tracker.record(
                            &backend.model,
                            decision.profile.task_type,
                            &Outcome::failure(latency.as_millis() as f64),
                        );

                        last_error = format!(
                            "backend '{}' returned HTTP {}: {}",
                            backend.id, status, body
                        );
                        continue;
                    }

                    let payload: Value = response
                        .json()
                        .await
                        .map_err(|e| format!("failed parsing backend JSON response: {e}"))?;

                    tier_router.record_success(&backend.id, latency);
                    quality_tracker.record(
                        &backend.model,
                        decision.profile.task_type,
                        &Outcome::success(latency.as_millis() as f64),
                    );

                    persist_quality_snapshot(&quality_tracker).await;

                    let text = extract_completion_text(&payload);

                    return Ok(json!({
                        "strategy": decision.strategy,
                        "task_profile": {
                            "task_type": decision.profile.task_type.as_str(),
                            "complexity": decision.profile.complexity.as_str(),
                            "recommended_tier": decision.profile.recommended_tier,
                            "estimated_tokens": decision.profile.estimated_tokens
                        },
                        "reason": decision.reason,
                        "recommended_model": decision.recommended_model,
                        "tier_used": tier,
                        "backend": {
                            "id": backend.id,
                            "node": backend.node,
                            "host": backend.host,
                            "port": backend.port,
                            "model": backend.model,
                            "tier": backend.tier
                        },
                        "latency_ms": latency.as_millis(),
                        "response": text,
                        "raw_response": payload,
                        "project_policy": applied_policy
                    }));
                }
                Err(err) => {
                    let latency = started.elapsed();
                    tier_router.record_failure(&backend.id, latency);
                    quality_tracker.record(
                        &backend.model,
                        decision.profile.task_type,
                        &Outcome::failure(latency.as_millis() as f64),
                    );
                    last_error = format!("backend '{}' request failed: {err}", backend.id);
                }
            }
        }
    }

    persist_quality_snapshot(&quality_tracker).await;

    Err(if last_error.is_empty() {
        "all routed backends failed".to_string()
    } else {
        last_error
    })
}

// ─── Fleet Scan ──────────────────────────────────────────────────────────────

pub async fn fleet_scan(params: Option<Value>) -> HandlerResult {
    let mode = params
        .as_ref()
        .and_then(|p| p.get("mode"))
        .and_then(|v| v.as_str())
        .unwrap_or("known");

    info!(mode, "fleet_scan handler called");

    match mode {
        "known" => {
            let (config, config_path) = load_config_auto()?;
            let targets = build_known_scan_targets(&config);
            let results = NodeScanner::new(targets).scan_once().await;

            Ok(json!({
                "mode": mode,
                "config_path": config_path,
                "nodes_scanned": config.nodes.len(),
                "results": results
            }))
        }
        "full" => {
            let (config, config_path) = load_config_auto()?;
            let mut scanner_cfg = ScannerConfig::default();
            if let Some(subnet) = params
                .as_ref()
                .and_then(|p| p.get("subnet"))
                .and_then(|v| v.as_str())
            {
                scanner_cfg.subnet_cidr = subnet.to_string();
            } else if let Some(inferred) = infer_subnet(&config) {
                scanner_cfg.subnet_cidr = inferred;
            }

            let discovered = scan_subnet(&scanner_cfg)
                .await
                .map_err(|e| format!("subnet scan failed: {e}"))?;

            let llm_ports = known_llm_ports();
            let endpoints: Vec<Value> = discovered
                .iter()
                .flat_map(|node| {
                    node.open_ports.iter().filter_map(|port| {
                        if llm_ports.contains(port) {
                            Some(json!({
                                "ip": node.ip,
                                "port": port,
                                "endpoint": format!("http://{}:{}", node.ip, port)
                            }))
                        } else {
                            None
                        }
                    })
                })
                .collect();

            Ok(json!({
                "mode": mode,
                "config_path": config_path,
                "subnet": scanner_cfg.subnet_cidr,
                "discovered_nodes": discovered,
                "endpoints": endpoints,
                "endpoints_found": endpoints.len()
            }))
        }
        other => Err(format!("unknown scan mode: {other}")),
    }
}

// ─── Fleet Install Model ─────────────────────────────────────────────────────

pub async fn fleet_install_model(params: Option<Value>) -> HandlerResult {
    let node = params
        .as_ref()
        .and_then(|p| p.get("node"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "fleet_install_model requires 'node'".to_string())?;
    let model_url = params
        .as_ref()
        .and_then(|p| p.get("model_url"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "fleet_install_model requires 'model_url'".to_string())?;
    let model_path = params
        .as_ref()
        .and_then(|p| p.get("model_path"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "fleet_install_model requires 'model_path'".to_string())?;

    let port = params
        .as_ref()
        .and_then(|p| p.get("port"))
        .and_then(|v| v.as_u64())
        .unwrap_or(55000) as u16;

    let ctx_size = params
        .as_ref()
        .and_then(|p| p.get("ctx_size"))
        .and_then(|v| v.as_u64())
        .unwrap_or(8192) as u32;

    info!(
        node,
        model_path, port, ctx_size, "fleet_install_model handler called"
    );

    let (config, _config_path) = load_config_auto()?;

    if is_local_node(node) {
        return install_model_local(node, model_url, model_path, port, ctx_size).await;
    }

    let ssh_node = resolve_ssh_node(&config, params.as_ref(), node)?;
    let shell_cmd = format!(
        "mkdir -p {dir} && curl -L --retry 3 -o {model} {url} && nohup llama-server --model {model} --host 0.0.0.0 --port {port} --ctx-size {ctx} --parallel 4 > /tmp/llama-{port}.log 2>&1 &",
        dir = shell_quote(
            Path::new(model_path)
                .parent()
                .unwrap_or(Path::new("."))
                .to_string_lossy()
                .as_ref()
        ),
        model = shell_quote(model_path),
        url = shell_quote(model_url),
        port = port,
        ctx = ctx_size
    );

    let executor = RemoteExecutor::new(300, true);
    let result = executor
        .run_on_node(ssh_node.clone(), shell_cmd.clone(), false)
        .await
        .map_err(|e| format!("remote install command failed: {e}"))?;

    let endpoint = format!("{}:{}", ssh_node.host, port);
    let verified = wait_for_endpoint_health(&ssh_node.host, port, 90).await;

    Ok(json!({
        "node": node,
        "host": ssh_node.host,
        "model_url": model_url,
        "model_path": model_path,
        "port": port,
        "ctx_size": ctx_size,
        "command": shell_cmd,
        "ssh_result": {
            "success": result.success,
            "exit_code": result.exit_code,
            "stdout": result.stdout,
            "stderr": result.stderr,
            "duration_ms": result.duration_ms
        },
        "endpoint": endpoint,
        "verified": verified,
        "status": if verified { "ready" } else { "started_unverified" }
    }))
}

// ─── Fleet Wait ──────────────────────────────────────────────────────────────

pub async fn fleet_wait(params: Option<Value>) -> HandlerResult {
    let condition = params
        .as_ref()
        .and_then(|p| p.get("condition"))
        .and_then(|v| v.as_str())
        .unwrap_or("all_healthy");
    let timeout_secs = params
        .as_ref()
        .and_then(|p| p.get("timeout"))
        .and_then(|v| v.as_u64())
        .unwrap_or(300);

    info!(condition, timeout_secs, "fleet_wait handler called");

    let started = Instant::now();
    let deadline = started + Duration::from_secs(timeout_secs);

    loop {
        let (met, details) = evaluate_wait_condition(condition, params.as_ref()).await?;
        if met {
            return Ok(json!({
                "condition": condition,
                "met": true,
                "elapsed_secs": started.elapsed().as_secs(),
                "details": details
            }));
        }

        if Instant::now() >= deadline {
            return Ok(json!({
                "condition": condition,
                "met": false,
                "elapsed_secs": started.elapsed().as_secs(),
                "details": details,
                "timeout": true
            }));
        }

        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

// ─── Fleet Crew ──────────────────────────────────────────────────────────────

pub async fn fleet_crew(params: Option<Value>) -> HandlerResult {
    let task = params
        .as_ref()
        .and_then(|p| p.get("task"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "fleet_crew requires 'task'".to_string())?;
    let repo_dir = params
        .as_ref()
        .and_then(|p| p.get("repo_dir"))
        .and_then(|v| v.as_str())
        .unwrap_or(".");

    let llm_base_url = params
        .as_ref()
        .and_then(|p| p.get("llm_base_url"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let llm_api_key = params
        .as_ref()
        .and_then(|p| p.get("llm_api_key"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let llm_model = params
        .as_ref()
        .and_then(|p| p.get("llm_model"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let requested_parallelism = params
        .as_ref()
        .and_then(|p| p.get("max_parallelism"))
        .and_then(|v| v.as_u64())
        .map(|v| v.max(1) as usize);

    let project_id = params
        .as_ref()
        .and_then(|p| p.get("project_id"))
        .and_then(|v| v.as_str());
    let approval_override = params
        .as_ref()
        .and_then(|p| p.get("approved"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut applied_policy: Option<AppliedProjectPolicy> = None;
    if let Some(project_id) = project_id {
        let policy = resolve_project_policy(project_id, task).await?;
        if policy.approval_required && !approval_override {
            return Err(format!(
                "human approval required for project '{project_id}': {} (set approved=true after review)",
                policy
                    .approval_reason
                    .clone()
                    .unwrap_or_else(|| "policy threshold triggered".to_string())
            ));
        }
        applied_policy = Some(policy);
    }

    info!(task, repo_dir, project_id = ?project_id, "fleet_crew handler called");

    let started = Instant::now();
    let team = choose_team_for_policy(applied_policy.as_ref());
    let policy_notes = applied_policy
        .as_ref()
        .map(policy_notes_for_crew)
        .unwrap_or_default();
    let (pattern, strategy, decomposed) = TemplateDecomposer::decompose(task);
    let decomposition = TemplateDecomposer::to_task_decomposition(task, &decomposed);
    let plan = Planner::plan(&decomposition)
        .map_err(|e| format!("failed to build execution plan: {e}"))?;

    let mut graph = PipelineGraph::new();

    for subtask in &decomposition.subtasks {
        let decomposed_subtask = decomposed
            .get(subtask.index)
            .ok_or_else(|| format!("missing decomposed subtask at index {}", subtask.index))?;

        let assignment = select_assignment_for_subtask(
            &team,
            &decomposed_subtask.role,
            decomposed_subtask.task_type,
        );

        let model_hint = preferred_model_hint(&assignment.model_preference);
        let prompt = build_crew_step_prompt(
            task,
            repo_dir,
            &subtask.title,
            &subtask.prompt,
            &assignment,
            &subtask.depends_on,
            if policy_notes.is_empty() {
                None
            } else {
                Some(policy_notes.as_str())
            },
        );

        let timeout_secs = crew_timeout_for_complexity(decomposed_subtask.estimated_complexity);

        let step = Step::new(
            StepId::new(subtask.id.to_string()),
            subtask.title.clone(),
            StepKind::LlmPrompt {
                prompt,
                model: model_hint,
                max_tokens: Some(1200),
            },
        )
        .with_timeout(Duration::from_secs(timeout_secs))
        .with_retries(1, Duration::from_secs(2));

        graph
            .add_step(step)
            .map_err(|e| format!("failed adding crew step '{}': {e}", subtask.id))?;
    }

    for subtask in &decomposition.subtasks {
        let dependent = StepId::new(subtask.id.to_string());
        for dependency in &subtask.depends_on {
            graph
                .add_dependency(&dependent, &StepId::new(dependency.to_string()))
                .map_err(|e| {
                    format!(
                        "failed wiring crew dependency '{}' -> '{}': {e}",
                        dependency, subtask.id
                    )
                })?;
        }
    }

    let mut effective_parallelism = requested_parallelism.unwrap_or(plan.max_parallelism().max(1));
    if let Some(policy) = applied_policy.as_ref() {
        effective_parallelism = match policy.policy.human_approval.level {
            HumanApprovalLevel::Always => effective_parallelism.min(1),
            HumanApprovalLevel::Strict => effective_parallelism.min(2),
            HumanApprovalLevel::Elevated | HumanApprovalLevel::None => effective_parallelism,
        };
    }

    let mut executor_cfg = ExecutorConfig {
        max_parallelism: effective_parallelism,
        ..ExecutorConfig::default()
    };

    if let Some(url) = llm_base_url {
        executor_cfg = executor_cfg.with_llm_base_url(url);
    }
    if let Some(api_key) = llm_api_key {
        executor_cfg = executor_cfg.with_llm_api_key(api_key);
    }
    if let Some(model) = llm_model {
        executor_cfg = executor_cfg.with_llm_model(model);
    }

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();

    let run = ff_pipeline::execute(&graph, executor_cfg, Some(event_tx))
        .await
        .map_err(|e| format!("failed executing crew pipeline: {e}"))?;

    let mut event_counts = json!({
        "step_started": 0,
        "step_completed": 0,
        "step_skipped": 0,
        "pipeline_finished": 0
    });

    while let Ok(event) = event_rx.try_recv() {
        match event {
            ff_pipeline::PipelineEvent::StepStarted { .. } => {
                event_counts["step_started"] =
                    json!(event_counts["step_started"].as_u64().unwrap_or(0) + 1);
            }
            ff_pipeline::PipelineEvent::StepCompleted { .. } => {
                event_counts["step_completed"] =
                    json!(event_counts["step_completed"].as_u64().unwrap_or(0) + 1);
            }
            ff_pipeline::PipelineEvent::StepSkipped { .. } => {
                event_counts["step_skipped"] =
                    json!(event_counts["step_skipped"].as_u64().unwrap_or(0) + 1);
            }
            ff_pipeline::PipelineEvent::PipelineFinished { .. } => {
                event_counts["pipeline_finished"] =
                    json!(event_counts["pipeline_finished"].as_u64().unwrap_or(0) + 1);
            }
        }
    }

    let plan_stages: Vec<Value> = plan
        .stages
        .iter()
        .enumerate()
        .map(|(idx, stage)| {
            json!({
                "stage": idx,
                "parallelism": stage.parallelism(),
                "subtasks": stage.subtask_ids.len(),
                "subtask_ids": stage.subtask_ids
            })
        })
        .collect();

    let assignments: Vec<Value> = team
        .assignments
        .iter()
        .enumerate()
        .map(|(idx, assignment)| {
            json!({
                "order": idx,
                "role": assignment.role,
                "model_preference": assignment.model_preference,
                "node_preference": assignment.node_preference,
                "instructions": assignment.instructions,
                "system_prompt": assignment.full_system_prompt()
            })
        })
        .collect();

    let mut executed_steps = Vec::new();
    let mut succeeded = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;

    let subtasks: Vec<Value> = decomposition
        .subtasks
        .iter()
        .map(|subtask| {
            let ds = &decomposed[subtask.index];
            let assignment = select_assignment_for_subtask(&team, &ds.role, ds.task_type);
            let step_id = StepId::new(subtask.id.to_string());
            let step_result = run.results.get(&step_id);

            let (status, output, error, attempts, duration_ms) = match step_result {
                Some(result) => {
                    let status = step_status_label(result.status);
                    match result.status {
                        StepStatus::Succeeded => succeeded += 1,
                        StepStatus::Failed | StepStatus::TimedOut => failed += 1,
                        StepStatus::Skipped => skipped += 1,
                        _ => {}
                    }
                    (
                        status,
                        result.output.clone(),
                        result.error.clone(),
                        result.attempts,
                        result.duration_ms,
                    )
                }
                None => {
                    failed += 1;
                    (
                        "missing_result".to_string(),
                        String::new(),
                        Some("no execution result recorded".to_string()),
                        0,
                        None,
                    )
                }
            };

            let step_json = json!({
                "id": subtask.id,
                "index": subtask.index,
                "title": subtask.title,
                "description": subtask.prompt,
                "stage": plan.stage_of(subtask.id),
                "role": ds.role,
                "assigned_role": assignment.role,
                "task_type": ds.task_type,
                "estimated_complexity": ds.estimated_complexity,
                "depends_on": subtask.depends_on,
                "model_preference": assignment.model_preference,
                "node_preference": assignment.node_preference,
                "status": status,
                "attempts": attempts,
                "duration_ms": duration_ms,
                "output": output,
                "error": error
            });

            executed_steps.push(step_json.clone());

            json!({
                "id": subtask.id,
                "index": subtask.index,
                "title": ds.title,
                "description": ds.description,
                "role": ds.role,
                "task_type": ds.task_type,
                "estimated_complexity": ds.estimated_complexity,
                "depends_on": ds.dependency_indices
            })
        })
        .collect();

    let total_steps = decomposition.subtasks.len();
    let execution_status = if run.success { "completed" } else { "failed" };

    let summary = json!({
        "total_steps": total_steps,
        "succeeded": succeeded,
        "failed": failed,
        "skipped": skipped,
        "success": run.success,
        "text": crew_summary_text(task, run.success, total_steps, succeeded, failed, skipped)
    });

    let audit_details = json!({
        "task": task,
        "repo_dir": repo_dir,
        "status": execution_status,
        "duration_ms": run.total_duration_ms,
        "summary": summary,
    });

    let audit =
        match persist_audit_log("fleet_crew_run", "ff-mcp", Some(repo_dir), &audit_details).await {
            Ok(Some(id)) => json!({ "persisted": true, "id": id }),
            Ok(None) => json!({ "persisted": false, "reason": "database_unavailable" }),
            Err(err) => {
                warn!(error = %err, "failed to persist fleet_crew audit event");
                json!({ "persisted": false, "reason": err })
            }
        };

    Ok(json!({
        "task": task,
        "repo_dir": repo_dir,
        "pattern": pattern,
        "strategy": strategy,
        "team": {
            "name": team.name,
            "description": team.description,
            "size": team.len(),
            "assignments": assignments
        },
        "decomposition": {
            "subtasks": subtasks,
            "total_subtasks": decomposed.len()
        },
        "execution_plan": {
            "stages": plan_stages,
            "total_stages": plan.num_stages(),
            "total_subtasks": plan.total_subtasks()
        },
        "execution": {
            "status": execution_status,
            "duration_ms": run.total_duration_ms,
            "wall_duration_ms": started.elapsed().as_millis() as u64,
            "events": event_counts,
            "steps": executed_steps,
            "summary": summary,
            "max_parallelism": effective_parallelism
        },
        "project_policy": applied_policy,
        "audit": audit,
        "status": execution_status
    }))
}

// ─── Model Recommend ─────────────────────────────────────────────────────────

pub async fn mcp_federation_status(params: Option<Value>) -> HandlerResult {
    let timeout_secs = params
        .as_ref()
        .and_then(|p| p.get("timeout_secs"))
        .and_then(|v| v.as_u64())
        .unwrap_or(5)
        .max(1);

    info!(timeout_secs, "mcp_federation_status handler called");

    let (config, config_path) = load_config_auto()?;
    let snapshot = federation::collect_federation_snapshot(&config, timeout_secs).await;

    Ok(json!({
        "config_path": config_path,
        "federation": snapshot
    }))
}

// ─── Model Recommend ─────────────────────────────────────────────────────────

pub async fn model_recommend(params: Option<Value>) -> HandlerResult {
    let task_type_str = params
        .as_ref()
        .and_then(|p| p.get("task_type"))
        .and_then(|v| v.as_str())
        .unwrap_or("chat");

    info!(task_type_str, "model_recommend handler called");

    let task_type = parse_task_type(task_type_str);
    let (config, _config_path) = load_config_auto()?;
    let model_tiers = model_tier_map(&config);

    let tracker = Arc::new(QualityTracker::new(QualityTrackerConfig {
        min_samples: 3,
        ..Default::default()
    }));

    let loaded_snapshot = if let Some(snapshot) = config_kv_get(QUALITY_SNAPSHOT_KEY).await {
        tracker.import_json(&snapshot).is_ok()
    } else {
        false
    };

    let rankings = tracker.rank_models(task_type, &model_tiers);

    if let Some(best) = rankings.first() {
        let alternatives: Vec<Value> = rankings
            .iter()
            .skip(1)
            .take(5)
            .map(|r| {
                json!({
                    "model": r.model_id,
                    "tier": r.tier,
                    "score": r.score,
                    "sample_count": r.sample_count,
                    "avg_latency_ms": r.avg_latency_ms,
                    "confident": r.confident
                })
            })
            .collect();

        return Ok(json!({
            "task_type": task_type.as_str(),
            "source": if loaded_snapshot { "quality_snapshot" } else { "runtime" },
            "recommended": {
                "model": best.model_id,
                "tier": best.tier,
                "mode": if best.confident { "adaptive" } else { "tier_fallback" },
                "confidence": best.score,
                "sample_count": best.sample_count,
                "avg_latency_ms": best.avg_latency_ms
            },
            "alternatives": alternatives
        }));
    }

    let mut fallback: Vec<(String, u8)> = model_tiers.into_iter().collect();
    fallback.sort_by_key(|(_, tier)| *tier);

    let recommended = fallback.first().cloned();

    Ok(json!({
        "task_type": task_type.as_str(),
        "source": "config",
        "recommended": recommended.map(|(model, tier)| json!({
            "model": model,
            "tier": tier,
            "mode": "tier_fallback",
            "confidence": 0.0
        })),
        "alternatives": fallback.into_iter().skip(1).map(|(model, tier)| json!({
            "model": model,
            "tier": tier,
            "mode": "tier_fallback",
            "confidence": 0.0
        })).collect::<Vec<_>>()
    }))
}

// ─── Model Stats ─────────────────────────────────────────────────────────────

pub async fn model_stats(params: Option<Value>) -> HandlerResult {
    let task_type_str = params
        .as_ref()
        .and_then(|p| p.get("task_type"))
        .and_then(|v| v.as_str())
        .unwrap_or("chat");

    info!(task_type_str, "model_stats handler called");

    let task_type = parse_task_type(task_type_str);
    let (config, _config_path) = load_config_auto()?;
    let model_tiers = model_tier_map(&config);

    let tracker = Arc::new(QualityTracker::new(QualityTrackerConfig {
        min_samples: 3,
        ..Default::default()
    }));

    let snapshot_loaded = if let Some(snapshot) = config_kv_get(QUALITY_SNAPSHOT_KEY).await {
        tracker.import_json(&snapshot).is_ok()
    } else {
        false
    };

    let rankings = tracker.rank_models(task_type, &model_tiers);
    let total_runs: u32 = rankings.iter().map(|r| r.sample_count).sum();

    let models: Vec<Value> = rankings
        .iter()
        .map(|r| {
            json!({
                "model": r.model_id,
                "tier": r.tier,
                "score": r.score,
                "sample_count": r.sample_count,
                "avg_latency_ms": r.avg_latency_ms,
                "confident": r.confident
            })
        })
        .collect();

    Ok(json!({
        "task_type": task_type.as_str(),
        "snapshot_loaded": snapshot_loaded,
        "total_runs": total_runs,
        "models": models
    }))
}

// ─── Project profile + policy API ───────────────────────────────────────────

pub async fn project_profile_upsert(params: Option<Value>) -> HandlerResult {
    let profile_value = params
        .as_ref()
        .and_then(|p| p.get("profile"))
        .cloned()
        .or_else(|| params.clone())
        .ok_or_else(|| "project_profile_upsert requires 'profile' object".to_string())?;

    let mut profile: ProjectExecutionProfile = serde_json::from_value(profile_value)
        .map_err(|e| format!("invalid project profile payload: {e}"))?;

    if profile.project_id.trim().is_empty() {
        let project_id = params
            .as_ref()
            .and_then(|p| p.get("project_id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| "project profile missing project_id".to_string())?;
        profile.project_id = project_id.to_string();
    }

    if profile.display_name.trim().is_empty() {
        profile.display_name = profile.project_id.clone();
    }

    if profile.deployment_targets.is_empty() {
        profile.deployment_targets.push(DeploymentTarget::Local);
    }

    let policy = ProjectPolicyEngine::derive(&profile);
    let record = upsert_project_profile_record(&profile, &policy).await?;

    Ok(json!({
        "status": "ok",
        "project_id": record.project_id,
        "display_name": record.display_name,
        "profile": record.profile,
        "policy": record.policy,
        "updated_at": record.updated_at,
    }))
}

pub async fn project_profile_get(params: Option<Value>) -> HandlerResult {
    let project_id = params
        .as_ref()
        .and_then(|p| p.get("project_id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "project_profile_get requires 'project_id'".to_string())?;

    let record = get_project_profile_record(project_id)
        .await?
        .ok_or_else(|| format!("project profile '{project_id}' not found"))?;

    Ok(json!({
        "project_id": record.project_id,
        "display_name": record.display_name,
        "profile": record.profile,
        "policy": record.policy,
        "created_at": record.created_at,
        "updated_at": record.updated_at,
    }))
}

pub async fn project_profile_list(_params: Option<Value>) -> HandlerResult {
    let records = list_project_profile_records().await?;

    Ok(json!({
        "count": records.len(),
        "profiles": records.into_iter().map(|record| json!({
            "project_id": record.project_id,
            "display_name": record.display_name,
            "profile": record.profile,
            "policy": record.policy,
            "created_at": record.created_at,
            "updated_at": record.updated_at,
        })).collect::<Vec<_>>()
    }))
}

pub async fn project_profile_delete(params: Option<Value>) -> HandlerResult {
    let project_id = params
        .as_ref()
        .and_then(|p| p.get("project_id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "project_profile_delete requires 'project_id'".to_string())?;

    let deleted = delete_project_profile_record(project_id).await?;

    Ok(json!({
        "project_id": project_id,
        "deleted": deleted
    }))
}

pub async fn project_policy_resolve(params: Option<Value>) -> HandlerResult {
    let project_id = params
        .as_ref()
        .and_then(|p| p.get("project_id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "project_policy_resolve requires 'project_id'".to_string())?;

    let prompt = params
        .as_ref()
        .and_then(|p| p.get("prompt"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let requested_start_tier = params
        .as_ref()
        .and_then(|p| p.get("start_tier"))
        .and_then(|v| v.as_u64())
        .unwrap_or(1) as u8;
    let requested_max_tier = params
        .as_ref()
        .and_then(|p| p.get("max_tier"))
        .and_then(|v| v.as_u64())
        .unwrap_or(4) as u8;
    let model_selector = params
        .as_ref()
        .and_then(|p| p.get("model"))
        .and_then(|v| v.as_str())
        .unwrap_or("auto");

    let record = get_project_profile_record(project_id)
        .await?
        .ok_or_else(|| format!("project profile '{project_id}' not found"))?;

    let (effective_start_tier, effective_max_tier) = ProjectPolicyEngine::clamp_tiers(
        &record.policy.routing,
        requested_start_tier,
        requested_max_tier,
    );
    let model_allowed = ProjectPolicyEngine::model_allowed(&record.policy.routing, model_selector);
    let approval_reason =
        ProjectPolicyEngine::approval_reason(&record.policy.human_approval, prompt);

    Ok(json!({
        "project_id": project_id,
        "requested": {
            "start_tier": requested_start_tier,
            "max_tier": requested_max_tier,
            "model": model_selector,
            "prompt": prompt,
        },
        "effective": {
            "start_tier": effective_start_tier,
            "max_tier": effective_max_tier,
            "model_allowed": model_allowed,
            "approval_required": approval_reason.is_some(),
            "approval_reason": approval_reason,
        },
        "policy": record.policy,
        "profile": record.profile,
    }))
}

// ─── Fleet Pulse (Redis real-time metrics) ──────────────────────────────────

pub async fn fleet_pulse(params: Option<Value>) -> HandlerResult {
    let (config, _) = load_config_auto()?;

    let mut pulse = PulseClient::connect(&config.redis.url)
        .await
        .map_err(|e| format!("Redis connection failed: {e}"))?;

    let node_filter = params
        .as_ref()
        .and_then(|p| p.get("node"))
        .and_then(|v| v.as_str());

    if let Some(node) = node_filter {
        let metrics = pulse
            .get_metrics(node)
            .await
            .map_err(|e| format!("Redis error: {e}"))?;
        Ok(json!({ "node": node, "metrics": metrics }))
    } else {
        let snapshot = pulse
            .get_all_metrics()
            .await
            .map_err(|e| format!("Redis error: {e}"))?;
        Ok(serde_json::to_value(snapshot).map_err(|e| e.to_string())?)
    }
}

// ─── Fleet Nodes DB (Postgres persistent registry) ──────────────────────────

pub async fn fleet_nodes_db(_params: Option<Value>) -> HandlerResult {
    let (config, _) = load_config_auto()?;
    let pool = get_pg_pool(&config).await?;

    let nodes = ff_db::pg_list_nodes(&pool)
        .await
        .map_err(|e| format!("Postgres query failed: {e}"))?;

    Ok(json!({
        "count": nodes.len(),
        "nodes": nodes
    }))
}

// ─── Fleet Node Detail (Postgres + Redis combined) ──────────────────────────

pub async fn fleet_node_detail(params: Option<Value>) -> HandlerResult {
    let node = params
        .as_ref()
        .and_then(|p| p.get("node"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "fleet_node_detail requires 'node' parameter".to_string())?;

    let (config, _) = load_config_auto()?;
    let pool = get_pg_pool(&config).await?;

    let db_node = ff_db::pg_get_node(&pool, node)
        .await
        .map_err(|e| format!("Postgres query failed: {e}"))?;

    let db_models = ff_db::pg_list_models_for_node(&pool, node)
        .await
        .map_err(|e| format!("Postgres models query failed: {e}"))?;

    // Best-effort Redis metrics
    let live_metrics = match PulseClient::connect(&config.redis.url).await {
        Ok(mut pulse) => match pulse.get_metrics(node).await {
            Ok(metrics) => metrics,
            Err(e) => {
                warn!("fleet_node_detail: Redis fetch failed (non-fatal): {e}");
                None
            }
        },
        Err(e) => {
            warn!("fleet_node_detail: Redis connection failed (non-fatal): {e}");
            None
        }
    };

    Ok(json!({
        "node": node,
        "registry": db_node,
        "models": db_models,
        "live_metrics": live_metrics
    }))
}

// ─── Fleet Models DB (Postgres model registry) ─────────────────────────────

pub async fn fleet_models_db(params: Option<Value>) -> HandlerResult {
    let (config, _) = load_config_auto()?;
    let pool = get_pg_pool(&config).await?;

    let node_filter = params
        .as_ref()
        .and_then(|p| p.get("node"))
        .and_then(|v| v.as_str());

    let models = if let Some(node) = node_filter {
        ff_db::pg_list_models_for_node(&pool, node)
            .await
            .map_err(|e| format!("Postgres query failed: {e}"))?
    } else {
        ff_db::pg_list_models(&pool)
            .await
            .map_err(|e| format!("Postgres query failed: {e}"))?
    };

    Ok(json!({
        "count": models.len(),
        "node_filter": node_filter,
        "models": models
    }))
}

// ─── Task Lineage ────────────────────────────────────────────────────────────

pub async fn task_lineage(params: Option<Value>) -> HandlerResult {
    let task_id = params
        .as_ref()
        .and_then(|p| p.get("task_id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "task_lineage requires 'task_id' parameter".to_string())?;

    let (config, _) = load_config_auto()?;
    let pool = get_pg_pool(&config).await?;

    ff_db::pg_get_task_lineage(&pool, task_id)
        .await
        .map_err(|e| format!("Postgres query failed: {e}"))
}

// ─── Fleet Models Catalog / Library / Deployments / Disk Usage ─────────────

fn catalog_row_to_json(row: &ff_db::ModelCatalogRow) -> Value {
    json!({
        "id": row.id,
        "name": row.name,
        "family": row.family,
        "parameters": row.parameters,
        "tier": row.tier,
        "gated": row.gated,
        "description": row.description,
        "preferred_workloads": row.preferred_workloads,
        "variants": row.variants,
    })
}

pub async fn fleet_models_catalog(_params: Option<Value>) -> HandlerResult {
    let (config, _) = load_config_auto()?;
    let pool = get_pg_pool(&config).await?;

    let rows = ff_db::pg_list_catalog(&pool)
        .await
        .map_err(|e| format!("Postgres query failed: {e}"))?;

    let items: Vec<Value> = rows.iter().map(catalog_row_to_json).collect();
    Ok(json!({
        "count": items.len(),
        "catalog": items,
    }))
}

pub async fn fleet_models_search(params: Option<Value>) -> HandlerResult {
    let query = params
        .as_ref()
        .and_then(|p| p.get("query"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "fleet_models_search requires 'query' parameter".to_string())?;

    let (config, _) = load_config_auto()?;
    let pool = get_pg_pool(&config).await?;

    let rows = ff_db::pg_search_catalog(&pool, query)
        .await
        .map_err(|e| format!("Postgres query failed: {e}"))?;

    let items: Vec<Value> = rows.iter().map(catalog_row_to_json).collect();
    Ok(json!({
        "query": query,
        "count": items.len(),
        "catalog": items,
    }))
}

pub async fn fleet_models_library(params: Option<Value>) -> HandlerResult {
    let node = params
        .as_ref()
        .and_then(|p| p.get("node"))
        .and_then(|v| v.as_str());

    let (config, _) = load_config_auto()?;
    let pool = get_pg_pool(&config).await?;

    let rows = ff_db::pg_list_library(&pool, node)
        .await
        .map_err(|e| format!("Postgres query failed: {e}"))?;

    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "node_name": r.node_name,
                "catalog_id": r.catalog_id,
                "runtime": r.runtime,
                "quant": r.quant,
                "file_path": r.file_path,
                "size_bytes": r.size_bytes,
                "sha256": r.sha256,
                "downloaded_at": r.downloaded_at,
                "last_used_at": r.last_used_at,
                "source_url": r.source_url,
            })
        })
        .collect();

    Ok(json!({
        "node_filter": node,
        "count": items.len(),
        "library": items,
    }))
}

pub async fn fleet_models_deployments(params: Option<Value>) -> HandlerResult {
    let node = params
        .as_ref()
        .and_then(|p| p.get("node"))
        .and_then(|v| v.as_str());

    let (config, _) = load_config_auto()?;
    let pool = get_pg_pool(&config).await?;

    let rows = ff_db::pg_list_deployments(&pool, node)
        .await
        .map_err(|e| format!("Postgres query failed: {e}"))?;

    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "node_name": r.node_name,
                "library_id": r.library_id,
                "catalog_id": r.catalog_id,
                "runtime": r.runtime,
                "port": r.port,
                "pid": r.pid,
                "started_at": r.started_at,
                "last_health_at": r.last_health_at,
                "health_status": r.health_status,
                "context_window": r.context_window,
                "tokens_used": r.tokens_used,
                "request_count": r.request_count,
            })
        })
        .collect();

    Ok(json!({
        "node_filter": node,
        "count": items.len(),
        "deployments": items,
    }))
}

pub async fn fleet_models_disk_usage(_params: Option<Value>) -> HandlerResult {
    let (config, _) = load_config_auto()?;
    let pool = get_pg_pool(&config).await?;

    let rows = ff_db::pg_latest_disk_usage(&pool)
        .await
        .map_err(|e| format!("Postgres query failed: {e}"))?;

    const GB: f64 = 1024.0 * 1024.0 * 1024.0;

    let items: Vec<Value> = rows
        .iter()
        .map(
            |(node_name, models_dir, total, used, free, models, sampled_at)| {
                json!({
                    "node_name": node_name,
                    "models_dir": models_dir,
                    "total_bytes": total,
                    "used_bytes": used,
                    "free_bytes": free,
                    "models_bytes": models,
                    "total_gb": (*total as f64) / GB,
                    "used_gb": (*used as f64) / GB,
                    "free_gb": (*free as f64) / GB,
                    "models_gb": (*models as f64) / GB,
                    "sampled_at": sampled_at,
                })
            },
        )
        .collect();

    Ok(json!({
        "count": items.len(),
        "disk_usage": items,
    }))
}

// ─── Postgres pool helper ───────────────────────────────────────────────────

async fn get_pg_pool(config: &FleetConfig) -> Result<sqlx::PgPool, String> {
    PgPoolOptions::new()
        .max_connections(2)
        .connect(&config.database.url)
        .await
        .map_err(|e| format!("Postgres connection failed: {e}"))
}

// ─── Handler dispatch ────────────────────────────────────────────────────────

/// Dispatch a method call to the appropriate handler.
///
/// Returns `Ok(Value)` on success or `Err(String)` if the method is unknown.
pub async fn dispatch(method: &str, params: Option<Value>) -> HandlerResult {
    match method {
        "fleet_status" => fleet_status(params).await,
        "fleet_config" => fleet_config(params).await,
        "fleet_ssh" => fleet_ssh(params).await,
        "fleet_run" => fleet_run(params).await,
        "fleet_scan" => fleet_scan(params).await,
        "fleet_install_model" => fleet_install_model(params).await,
        "fleet_wait" => fleet_wait(params).await,
        "fleet_crew" => fleet_crew(params).await,
        "mcp_federation_status" => mcp_federation_status(params).await,
        "model_recommend" => model_recommend(params).await,
        "model_stats" => model_stats(params).await,
        "project_profile_upsert" => project_profile_upsert(params).await,
        "project_profile_get" => project_profile_get(params).await,
        "project_profile_list" => project_profile_list(params).await,
        "project_profile_delete" => project_profile_delete(params).await,
        "project_policy_resolve" => project_policy_resolve(params).await,
        "fleet_pulse" => fleet_pulse(params).await,
        "fleet_nodes_db" => fleet_nodes_db(params).await,
        "fleet_node_detail" => fleet_node_detail(params).await,
        "fleet_models_db" => fleet_models_db(params).await,
        "task_lineage" => task_lineage(params).await,
        "fleet_models_catalog" => fleet_models_catalog(params).await,
        "fleet_models_search" => fleet_models_search(params).await,
        "fleet_models_library" => fleet_models_library(params).await,
        "fleet_models_deployments" => fleet_models_deployments(params).await,
        "fleet_models_disk_usage" => fleet_models_disk_usage(params).await,
        // Virtual Brain
        "brain_search" => crate::brain_tools::brain_search(params).await,
        "brain_vault_read" => crate::brain_tools::brain_vault_read(params).await,
        "brain_graph_neighbors" => crate::brain_tools::brain_graph_neighbors(params).await,
        "brain_list_threads" => crate::brain_tools::brain_list_threads(params).await,
        "brain_stats" => crate::brain_tools::brain_stats(params).await,
        "brain_propose_node" => crate::brain_tools::brain_propose_node(params).await,
        "brain_propose_link" => crate::brain_tools::brain_propose_link(params).await,
        "brain_thread_append" => crate::brain_tools::brain_thread_append(params).await,
        "brain_stack_push" => crate::brain_tools::brain_stack_push(params).await,
        "brain_backlog_add" => crate::brain_tools::brain_backlog_add(params).await,
        // Computer Use (Pillar 1)
        "computer_use" => computer_use(params).await,
        _ => Err(format!("unknown method: {method}")),
    }
}

/// Dispatcher for the `computer_use` MCP tool. Translates the action
/// payload into an HTTP call against the local screen-control daemon
/// at 127.0.0.1:51200 (PR-G). The MCP layer doesn't return PNG bytes
/// directly — for `screenshot` we save to a tmp file and return the
/// path, which the LLM can then read via the existing file-read tool.
async fn computer_use(params: Option<Value>) -> Result<Value, String> {
    let params = params.unwrap_or_else(|| serde_json::json!({}));
    let action = params
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| "computer_use: 'action' is required".to_string())?;

    let client = reqwest::Client::new();
    let base = "http://127.0.0.1:51200";

    match action {
        "screenshot" => {
            let url = if let Some(region) = params.get("region").and_then(Value::as_str) {
                format!("{base}/screenshot?region={region}")
            } else {
                format!("{base}/screenshot")
            };
            let resp = client
                .get(&url)
                .send()
                .await
                .map_err(|e| format!("screenshot HTTP: {e}"))?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(format!("screenshot {status}: {body}"));
            }
            let bytes = resp.bytes().await.map_err(|e| format!("read png: {e}"))?;
            // Save to tmp + return path. Multi-modal LLMs can read the
            // file; bytes-over-MCP would balloon the protocol.
            let path = std::env::temp_dir().join(format!(
                "ff-mcp-screen-{}.png",
                chrono::Utc::now().timestamp_millis()
            ));
            std::fs::write(&path, &bytes).map_err(|e| format!("write png: {e}"))?;
            Ok(serde_json::json!({
                "screenshot_path": path.display().to_string(),
                "size_bytes": bytes.len(),
            }))
        }
        "click" | "double_click" | "move" => {
            let x = params.get("x").and_then(Value::as_i64).ok_or_else(|| {
                format!("computer_use {action}: 'x' (integer) is required")
            })?;
            let y = params.get("y").and_then(Value::as_i64).ok_or_else(|| {
                format!("computer_use {action}: 'y' (integer) is required")
            })?;
            let endpoint = match action {
                "click" => "click",
                "double_click" => "double-click",
                "move" => "move",
                _ => unreachable!(),
            };
            let resp = client
                .post(format!("{base}/{endpoint}"))
                .json(&serde_json::json!({"x": x, "y": y}))
                .send()
                .await
                .map_err(|e| format!("{action} HTTP: {e}"))?;
            relay_resp(resp).await
        }
        "type" => {
            let text = params
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| "computer_use type: 'text' is required".to_string())?;
            let resp = client
                .post(format!("{base}/type"))
                .json(&serde_json::json!({"text": text}))
                .send()
                .await
                .map_err(|e| format!("type HTTP: {e}"))?;
            relay_resp(resp).await
        }
        "key" => {
            let key = params
                .get("key")
                .and_then(Value::as_str)
                .ok_or_else(|| "computer_use key: 'key' is required".to_string())?;
            let resp = client
                .post(format!("{base}/key"))
                .json(&serde_json::json!({"key": key}))
                .send()
                .await
                .map_err(|e| format!("key HTTP: {e}"))?;
            relay_resp(resp).await
        }
        "goto" => {
            let url = params
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| "computer_use goto: 'url' is required".to_string())?;
            let resp = client
                .post(format!("{base}/goto"))
                .json(&serde_json::json!({"url": url}))
                .send()
                .await
                .map_err(|e| format!("goto HTTP: {e}"))?;
            relay_resp(resp).await
        }
        other => Err(format!(
            "computer_use: unknown action '{other}' (expected screenshot|click|double_click|move|type|key|goto)"
        )),
    }
}

async fn relay_resp(resp: reqwest::Response) -> Result<Value, String> {
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .unwrap_or_else(|_| serde_json::json!({"ok": status.is_success()}));
    if status.is_success() {
        Ok(body)
    } else {
        Err(format!("{status}: {body}"))
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn choose_team_for_policy(policy: Option<&AppliedProjectPolicy>) -> TeamConfig {
    if let Some(policy) = policy {
        match policy.policy.review.strictness {
            ReviewStrictness::Paranoid | ReviewStrictness::Strict => TeamTemplates::review_team(),
            ReviewStrictness::Relaxed | ReviewStrictness::Standard => TeamTemplates::code_team(),
        }
    } else {
        TeamTemplates::code_team()
    }
}

fn policy_notes_for_crew(policy: &AppliedProjectPolicy) -> String {
    let mut notes = Vec::new();

    notes.push(format!("Project: {}", policy.project_id));
    notes.push(format!(
        "Data sensitivity: {:?}",
        match policy.policy.human_approval.level {
            HumanApprovalLevel::Always => DataSensitivity::Regulated,
            HumanApprovalLevel::Strict => DataSensitivity::Confidential,
            HumanApprovalLevel::Elevated => DataSensitivity::Internal,
            HumanApprovalLevel::None => DataSensitivity::Public,
        }
    ));

    notes.push(format!(
        "Routing tiers: {}-{}",
        policy.policy.routing.min_tier, policy.policy.routing.max_tier
    ));

    if policy.policy.review.require_security_review {
        notes.push("Security review is mandatory.".to_string());
    }
    if policy.policy.review.require_tests {
        notes.push("Testing evidence is mandatory before handoff.".to_string());
    }

    if policy.policy.rollout.require_staging {
        notes.push("Changes must be staged before production rollout.".to_string());
    }

    notes.join("\n")
}

async fn resolve_project_policy(
    project_id: &str,
    operation_text: &str,
) -> Result<AppliedProjectPolicy, String> {
    let record = get_project_profile_record(project_id)
        .await?
        .ok_or_else(|| format!("project profile '{project_id}' not found"))?;

    let approval_reason =
        ProjectPolicyEngine::approval_reason(&record.policy.human_approval, operation_text);

    Ok(AppliedProjectPolicy {
        project_id: project_id.to_string(),
        policy: record.policy,
        approval_required: approval_reason.is_some(),
        approval_reason,
    })
}

fn select_assignment_for_subtask(
    team: &TeamConfig,
    preferred_role: &AgentRole,
    task_type: SubTaskType,
) -> AgentAssignment {
    if let Some(exact) = team
        .assignments
        .iter()
        .find(|assignment| &assignment.role == preferred_role)
    {
        return exact.clone();
    }

    let fallback_role = match task_type {
        SubTaskType::Review => AgentRole::Reviewer,
        SubTaskType::Code | SubTaskType::ToolUse => AgentRole::Coder,
        _ => AgentRole::Planner,
    };

    team.assignments
        .iter()
        .find(|assignment| assignment.role == fallback_role)
        .cloned()
        .or_else(|| team.assignments.first().cloned())
        .unwrap_or_else(|| AgentAssignment::new(fallback_role))
}

fn preferred_model_hint(model_preference: &ModelPreference) -> Option<String> {
    match model_preference {
        ModelPreference::Specific { model_id } => Some(model_id.clone()),
        ModelPreference::Tier { .. } | ModelPreference::Auto => None,
    }
}

fn build_crew_step_prompt(
    task: &str,
    repo_dir: &str,
    title: &str,
    subtask_prompt: &str,
    assignment: &AgentAssignment,
    depends_on: &[uuid::Uuid],
    policy_notes: Option<&str>,
) -> String {
    let dependency_text = if depends_on.is_empty() {
        "None".to_string()
    } else {
        depends_on
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };

    let policy_block = policy_notes.unwrap_or("No project policy override.");

    format!(
        "Role: {role}\n\
         Team: {team_role}\n\
         Repo: {repo_dir}\n\
         Top-level task: {task}\n\
         Step title: {title}\n\
         Dependencies: {dependency_text}\n\n\
         Project policy:\n{policy_block}\n\n\
         {system_prompt}\n\n\
         Subtask instructions:\n{subtask_prompt}\n\n\
         Return concise, actionable output for the next step.",
        role = assignment.role,
        team_role = assignment.role,
        repo_dir = repo_dir,
        task = task,
        title = title,
        dependency_text = dependency_text,
        policy_block = policy_block,
        system_prompt = assignment.full_system_prompt(),
        subtask_prompt = subtask_prompt,
    )
}

fn crew_timeout_for_complexity(complexity: u8) -> u64 {
    match complexity {
        0..=3 => 45,
        4..=6 => 90,
        _ => 180,
    }
}

fn step_status_label(status: StepStatus) -> String {
    match status {
        StepStatus::Pending => "pending",
        StepStatus::Running => "running",
        StepStatus::Succeeded => "succeeded",
        StepStatus::Failed => "failed",
        StepStatus::Skipped => "skipped",
        StepStatus::TimedOut => "timed_out",
    }
    .to_string()
}

fn crew_summary_text(
    task: &str,
    success: bool,
    total_steps: usize,
    succeeded: usize,
    failed: usize,
    skipped: usize,
) -> String {
    if success {
        format!(
            "Crew completed task '{}' with {}/{} successful steps.",
            task, succeeded, total_steps
        )
    } else {
        format!(
            "Crew finished task '{}' with failures (succeeded: {}, failed: {}, skipped: {}).",
            task, succeeded, failed, skipped
        )
    }
}

async fn persist_audit_log(
    event_type: &str,
    actor: &str,
    target: Option<&str>,
    details: &Value,
) -> Result<Option<i64>, String> {
    let store = match open_operational_store().await {
        Ok(store) => store,
        Err(err) => {
            warn!(error = %err, "audit persistence unavailable");
            return Ok(None);
        }
    };

    let details_json = serde_json::to_string(details)
        .map_err(|e| format!("failed serializing audit details: {e}"))?;

    let id = store
        .audit_log(event_type, actor, target, &details_json, None)
        .await
        .map_err(|e| format!("failed writing audit_log entry '{event_type}': {e}"))?;

    Ok(Some(id))
}

fn load_config_auto() -> Result<(FleetConfig, PathBuf), String> {
    config::load_config_auto().map_err(|e| format!("failed to load fleet config: {e}"))
}

fn build_known_scan_targets(config: &FleetConfig) -> Vec<ff_discovery::scanner::ScanTarget> {
    let default_port = config.fleet.api_port;
    let nodes: Vec<(String, String, Option<u16>, u32)> = config
        .nodes
        .iter()
        .map(|(name, node)| {
            (
                name.clone(),
                node.ip.clone(),
                node.port.or(Some(default_port)),
                node.priority(),
            )
        })
        .collect();

    build_scan_targets(nodes, default_port)
}

fn scan_status_to_str(status: NodeScanStatus) -> &'static str {
    match status {
        NodeScanStatus::Online => "healthy",
        NodeScanStatus::Degraded => "degraded",
        NodeScanStatus::Offline => "offline",
    }
}

fn resolve_ssh_node(
    config: &FleetConfig,
    params: Option<&Value>,
    node_ref: &str,
) -> Result<SshNodeConfig, String> {
    if let Some(node_cfg) = config.nodes.get(node_ref) {
        return Ok(SshNodeConfig::from_core_node(node_ref, node_cfg));
    }

    if let Some((name, node_cfg)) = config
        .nodes
        .iter()
        .find(|(_, node)| node.ip == node_ref || node.alt_ips.iter().any(|ip| ip == node_ref))
    {
        return Ok(SshNodeConfig::from_core_node(name, node_cfg));
    }

    let username = params
        .and_then(|p| p.get("username"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "root".to_string());

    let port = params
        .and_then(|p| p.get("ssh_port"))
        .and_then(|v| v.as_u64())
        .unwrap_or(22) as u16;

    Ok(SshNodeConfig {
        name: node_ref.to_string(),
        host: node_ref.to_string(),
        port,
        username,
        key_path: None,
        password: None,
        alternate_ips: vec![],
        batch_mode: true,
        connect_timeout_secs: Some(10),
        known_hosts_path: None,
    })
}

fn backends_from_config(config: &FleetConfig) -> Vec<BackendEndpoint> {
    let mut endpoints = Vec::new();

    for (node_name, node_cfg) in &config.nodes {
        for (slug, model) in &node_cfg.models {
            let Some(port) = model.port.or(node_cfg.port) else {
                continue;
            };

            let model_name = if model.name.trim().is_empty() {
                slug.clone()
            } else {
                model.name.clone()
            };

            endpoints.push(BackendEndpoint {
                id: format!("{}:{}:{}", node_name, slug, port),
                node: node_name.clone(),
                host: node_cfg.ip.clone(),
                port,
                model: model_name,
                tier: model.tier.clamp(1, 4) as u8,
                healthy: true,
                busy: false,
                scheme: "http".to_string(),
            });
        }
    }

    endpoints
}

async fn healthy_backends_from_config(config: &FleetConfig) -> Vec<BackendEndpoint> {
    let mut endpoints = backends_from_config(config);
    if endpoints.is_empty() {
        return endpoints;
    }

    let monitor = HealthMonitor::default();
    let targets: Vec<HealthTarget> = endpoints
        .iter()
        .map(|e| HealthTarget {
            name: e.id.clone(),
            host: e.host.clone(),
            port: e.port,
            check_http_health: true,
        })
        .collect();

    let checks = monitor.check_all(&targets).await;
    let by_name: HashMap<String, bool> = checks
        .into_iter()
        .map(|c| {
            (
                c.name,
                matches!(c.status, HealthStatus::Healthy | HealthStatus::Degraded),
            )
        })
        .collect();

    for endpoint in &mut endpoints {
        endpoint.healthy = by_name.get(&endpoint.id).copied().unwrap_or(false);
    }

    endpoints
}

fn parse_task_type(raw: &str) -> TaskType {
    match raw.trim().to_ascii_lowercase().as_str() {
        "code" => TaskType::Code,
        "reasoning" => TaskType::Reasoning,
        "summary" => TaskType::Summary,
        "translation" => TaskType::Translation,
        "review" => TaskType::Review,
        "debug" => TaskType::Debug,
        _ => TaskType::Chat,
    }
}

fn model_tier_map(config: &FleetConfig) -> HashMap<String, u8> {
    let mut map: HashMap<String, u8> = HashMap::new();
    for endpoint in backends_from_config(config) {
        map.entry(endpoint.model)
            .and_modify(|tier| *tier = (*tier).min(endpoint.tier))
            .or_insert(endpoint.tier);
    }
    map
}

fn parse_value_string(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

fn set_json_dot_path(root: &mut Value, path: &str, new_value: Value) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("key cannot be empty".to_string());
    }

    let parts: Vec<&str> = path.split('.').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return Err("invalid key path".to_string());
    }

    let mut current = root;

    for key in &parts[..parts.len() - 1] {
        if !current.is_object() {
            *current = Value::Object(Map::new());
        }

        let object = current
            .as_object_mut()
            .ok_or_else(|| "intermediate path is not an object".to_string())?;
        current = object
            .entry((*key).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }

    let final_key = parts
        .last()
        .ok_or_else(|| "missing final key segment".to_string())?;

    if !current.is_object() {
        *current = Value::Object(Map::new());
    }

    let object = current
        .as_object_mut()
        .ok_or_else(|| "final path parent is not an object".to_string())?;
    object.insert((*final_key).to_string(), new_value);

    Ok(())
}

fn is_local_node(node: &str) -> bool {
    let lower = node.to_ascii_lowercase();
    lower == "localhost" || lower == "127.0.0.1" || lower == "::1" || lower == "local"
}

async fn install_model_local(
    node: &str,
    model_url: &str,
    model_path: &str,
    port: u16,
    ctx_size: u32,
) -> HandlerResult {
    let model_path_buf = PathBuf::from(model_path);
    let models_dir = model_path_buf
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    let model_id = model_path_buf
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("model")
        .to_string();

    let mut manager = ModelManager::new(models_dir.clone());
    let downloaded = manager
        .download_gguf(model_url, &model_id, &model_id, "GGUF", node)
        .await
        .map_err(|e| format!("local model download failed: {e}"))?;

    if downloaded.path != model_path_buf {
        if let Some(parent) = model_path_buf.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create target model directory: {e}"))?;
        }
        std::fs::copy(&downloaded.path, &model_path_buf)
            .map_err(|e| format!("failed to move downloaded model to requested path: {e}"))?;
    }

    let process_manager = ProcessManager::new();
    let pid = process_manager
        .start_model(EngineConfig {
            model_path: model_path_buf.clone(),
            model_id: model_id.clone(),
            host: "0.0.0.0".to_string(),
            port,
            ctx_size,
            gpu_layers: -1,
            parallel: 4,
            extra_args: vec![],
        })
        .await
        .map_err(|e| format!("failed to start llama-server locally: {e}"))?;

    let verified = wait_for_endpoint_health("127.0.0.1", port, 90).await;

    Ok(json!({
        "node": node,
        "model_url": model_url,
        "model_path": model_path,
        "downloaded_path": downloaded.path,
        "port": port,
        "ctx_size": ctx_size,
        "pid": pid,
        "verified": verified,
        "status": if verified { "ready" } else { "started_unverified" }
    }))
}

async fn wait_for_endpoint_health(host: &str, port: u16, timeout_secs: u64) -> bool {
    let monitor = HealthMonitor::default();
    let started = Instant::now();

    while started.elapsed() < Duration::from_secs(timeout_secs) {
        let result = monitor
            .check_target(&HealthTarget {
                name: format!("{host}:{port}"),
                host: host.to_string(),
                port,
                check_http_health: true,
            })
            .await;

        if matches!(
            result.status,
            HealthStatus::Healthy | HealthStatus::Degraded
        ) {
            return true;
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    false
}

async fn evaluate_wait_condition(
    condition: &str,
    params: Option<&Value>,
) -> Result<(bool, Value), String> {
    match condition {
        "all_healthy" => {
            let (config, _path) = load_config_auto()?;
            let targets = build_known_scan_targets(&config);
            if targets.is_empty() {
                return Ok((true, json!({ "reason": "no configured nodes" })));
            }

            let results = NodeScanner::new(targets).scan_once().await;
            let all_ok = results
                .iter()
                .all(|r| matches!(r.status, NodeScanStatus::Online | NodeScanStatus::Degraded));
            Ok((all_ok, json!({ "results": results })))
        }
        "tier_available" => {
            let tier = params
                .and_then(|p| p.get("tier"))
                .and_then(|v| v.as_u64())
                .ok_or_else(|| "tier_available requires 'tier'".to_string())?
                .clamp(1, 4) as u8;

            let (config, _path) = load_config_auto()?;
            let endpoints: Vec<_> = healthy_backends_from_config(&config)
                .await
                .into_iter()
                .filter(|e| e.tier == tier)
                .collect();
            let met = endpoints.iter().any(|e| e.healthy);

            Ok((
                met,
                json!({
                    "tier": tier,
                    "available": endpoints.iter().filter(|e| e.healthy).count(),
                    "endpoints": endpoints
                }),
            ))
        }
        "model_loaded" => {
            let endpoint = params
                .and_then(|p| p.get("endpoint"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| "model_loaded requires 'endpoint' (ip:port)".to_string())?;

            let (host, port) = parse_host_port(endpoint)?;
            let monitor = HealthMonitor::default();
            let result = monitor
                .check_target(&HealthTarget {
                    name: endpoint.to_string(),
                    host,
                    port,
                    check_http_health: true,
                })
                .await;

            let met = matches!(
                result.status,
                HealthStatus::Healthy | HealthStatus::Degraded
            );
            Ok((met, json!({ "check": result })))
        }
        other => Err(format!("unknown wait condition: {other}")),
    }
}

fn parse_host_port(endpoint: &str) -> Result<(String, u16), String> {
    let mut parts = endpoint.split(':');
    let host = parts
        .next()
        .ok_or_else(|| "invalid endpoint (missing host)".to_string())?;
    let port_str = parts
        .next()
        .ok_or_else(|| "invalid endpoint (missing port)".to_string())?;
    if parts.next().is_some() {
        return Err("invalid endpoint format, expected 'host:port'".to_string());
    }

    let port = port_str
        .parse::<u16>()
        .map_err(|e| format!("invalid endpoint port '{port_str}': {e}"))?;

    Ok((host.to_string(), port))
}

fn infer_subnet(config: &FleetConfig) -> Option<String> {
    for node in config.nodes.values() {
        if let Ok(IpAddr::V4(ipv4)) = node.ip.parse::<IpAddr>() {
            let octets = ipv4.octets();
            return Some(format!("{}.{}.{}.0/24", octets[0], octets[1], octets[2]));
        }
    }

    None
}

fn extract_completion_text(payload: &Value) -> Option<String> {
    payload
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|choices| choices.first())
        .and_then(|choice| {
            choice
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(value_to_string)
                .or_else(|| {
                    choice
                        .get("delta")
                        .and_then(|m| m.get("content"))
                        .and_then(value_to_string)
                })
                .or_else(|| choice.get("text").and_then(value_to_string))
        })
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Array(arr) => {
            let mut text_parts = Vec::new();
            for item in arr {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    text_parts.push(text.to_string());
                }
            }
            if text_parts.is_empty() {
                None
            } else {
                Some(text_parts.join("\n"))
            }
        }
        _ => None,
    }
}

async fn persist_quality_snapshot(tracker: &Arc<QualityTracker>) {
    match tracker.export_json() {
        Ok(snapshot) => {
            if let Err(err) = config_kv_set(QUALITY_SNAPSHOT_KEY, &snapshot).await {
                warn!(error = %err, "failed to persist quality snapshot to config_kv");
            }
        }
        Err(err) => {
            warn!(error = %err, "failed to export quality snapshot");
        }
    }
}

fn sqlite_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Ok(path) = std::env::var("FORGEFLEET_DB_PATH") {
        candidates.push(PathBuf::from(path));
    }

    if let Ok(home) = std::env::var("HOME") {
        candidates.push(
            PathBuf::from(home)
                .join(".forgefleet")
                .join("forgefleet.db"),
        );
    }

    candidates.push(PathBuf::from("forgefleet.db"));
    candidates
}

fn resolve_embedded_sqlite_path(
    config: &FleetConfig,
    config_path: &Path,
) -> Result<PathBuf, String> {
    if let Some(raw) = config
        .database
        .sqlite_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let candidate = PathBuf::from(raw);
        if candidate.is_absolute() {
            return Ok(candidate);
        }

        let parent = config_path.parent().ok_or_else(|| {
            "unable to resolve config parent directory for sqlite path".to_string()
        })?;
        return Ok(parent.join(candidate));
    }

    let parent = config_path
        .parent()
        .ok_or_else(|| "unable to resolve config parent directory for sqlite path".to_string())?;
    Ok(parent.join("forgefleet.db"))
}

fn open_embedded_sqlite_store(db_path: &Path) -> Result<OperationalStore, String> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create sqlite parent '{}': {e}", parent.display()))?;
    }

    let pool = DbPool::open(DbPoolConfig::with_path(db_path))
        .map_err(|e| format!("failed to open sqlite db '{}': {e}", db_path.display()))?;

    let conn = pool
        .open_raw_connection()
        .map_err(|e| format!("failed opening sqlite raw connection: {e}"))?;
    run_migrations(&conn).map_err(|e| format!("sqlite migration failed: {e}"))?;

    Ok(OperationalStore::sqlite(pool))
}

fn open_legacy_sqlite_store() -> Result<OperationalStore, String> {
    let mut errors = Vec::new();

    for path in sqlite_candidates() {
        match open_embedded_sqlite_store(&path) {
            Ok(store) => return Ok(store),
            Err(err) => {
                errors.push(format!("{} ({err})", path.display()));
            }
        }
    }

    Err(format!(
        "unable to initialize sqlite operational store from fallback candidates: {}",
        errors.join("; ")
    ))
}

async fn open_operational_store() -> Result<OperationalStore, String> {
    match load_config_auto() {
        Ok((config, config_path)) => match config.database.mode {
            DatabaseMode::EmbeddedSqlite => {
                let sqlite_path = resolve_embedded_sqlite_path(&config, &config_path)?;
                open_embedded_sqlite_store(&sqlite_path)
            }
            DatabaseMode::PostgresRuntime | DatabaseMode::PostgresFull => {
                let database_url = config.database.url.trim();
                if database_url.is_empty() {
                    return Err(format!(
                        "database.mode={} requires non-empty [database].url",
                        config.database.mode.as_str()
                    ));
                }

                OperationalStore::postgres(database_url, config.database.max_connections)
                    .await
                    .map_err(|e| {
                        format!(
                            "failed to initialize Postgres operational store for MCP handlers: {e}"
                        )
                    })
            }
        },
        Err(err) => {
            warn!(error = %err, "failed to load fleet config; falling back to sqlite candidates for MCP persistence");
            open_legacy_sqlite_store()
        }
    }
}

fn project_profile_storage_key(project_id: &str) -> String {
    format!("{}{}", PROJECT_PROFILE_KEY_PREFIX, project_id)
}

async fn upsert_project_profile_record(
    profile: &ProjectExecutionProfile,
    policy: &ExecutionPolicy,
) -> Result<ProjectProfileRecord, String> {
    let store = open_operational_store().await?;
    let now = Utc::now().to_rfc3339();
    let key = project_profile_storage_key(&profile.project_id);

    let created_at = match store.config_get(&key).await.map_err(|e| {
        format!(
            "failed reading existing project profile '{}': {e}",
            profile.project_id
        )
    })? {
        Some(existing_payload) => serde_json::from_str::<ProjectProfileRecord>(&existing_payload)
            .map(|existing| existing.created_at)
            .unwrap_or_else(|_| now.clone()),
        None => now.clone(),
    };

    let record = ProjectProfileRecord {
        project_id: profile.project_id.clone(),
        display_name: profile.display_name.clone(),
        profile: profile.clone(),
        policy: policy.clone(),
        created_at,
        updated_at: now,
    };

    let payload = serde_json::to_string(&record).map_err(|e| {
        format!(
            "failed serializing project profile '{}': {e}",
            record.project_id
        )
    })?;

    store.config_set(&key, &payload).await.map_err(|e| {
        format!(
            "failed upserting project profile '{}': {e}",
            record.project_id
        )
    })?;

    Ok(record)
}

async fn get_project_profile_record(
    project_id: &str,
) -> Result<Option<ProjectProfileRecord>, String> {
    let store = open_operational_store().await?;
    let key = project_profile_storage_key(project_id);
    let Some(payload) = store
        .config_get(&key)
        .await
        .map_err(|e| format!("failed loading project profile '{project_id}': {e}"))?
    else {
        return Ok(None);
    };

    let record: ProjectProfileRecord = serde_json::from_str(&payload)
        .map_err(|e| format!("failed parsing stored project profile '{project_id}': {e}"))?;
    Ok(Some(record))
}

async fn list_project_profile_records() -> Result<Vec<ProjectProfileRecord>, String> {
    let store = open_operational_store().await?;
    let rows = store
        .config_list_prefix(PROJECT_PROFILE_KEY_PREFIX, 5_000)
        .await
        .map_err(|e| format!("failed listing project profile records: {e}"))?;

    let mut records = Vec::new();
    for (key, payload) in rows {
        match serde_json::from_str::<ProjectProfileRecord>(&payload) {
            Ok(mut record) => {
                if record.project_id.trim().is_empty() {
                    record.project_id = key
                        .strip_prefix(PROJECT_PROFILE_KEY_PREFIX)
                        .unwrap_or(&key)
                        .to_string();
                }
                records.push(record);
            }
            Err(err) => {
                warn!(key = %key, error = %err, "skipping invalid stored project profile record");
            }
        }
    }

    records.sort_by(|a, b| a.project_id.cmp(&b.project_id));
    Ok(records)
}

async fn delete_project_profile_record(project_id: &str) -> Result<bool, String> {
    let store = open_operational_store().await?;
    let key = project_profile_storage_key(project_id);

    store
        .config_delete(&key)
        .await
        .map_err(|e| format!("failed deleting project profile '{project_id}': {e}"))
}

async fn config_kv_get(key: &str) -> Option<String> {
    let store = match open_operational_store().await {
        Ok(store) => store,
        Err(err) => {
            warn!(error = %err, "config_kv_get fallback failed");
            return None;
        }
    };

    match store.config_get(key).await {
        Ok(value) => value,
        Err(err) => {
            warn!(key = %key, error = %err, "failed reading config_kv key");
            None
        }
    }
}

async fn config_kv_set(key: &str, value: &str) -> Result<(), String> {
    let store = open_operational_store().await?;
    store
        .config_set(key, value)
        .await
        .map_err(|e| format!("failed writing config_kv entry '{key}': {e}"))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    use axum::{Json, Router, http::StatusCode, routing::post};
    use tokio::net::TcpListener;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn setup_test_db() -> (String, Option<String>) {
        let db_path = std::env::temp_dir().join(format!(
            "ff-mcp-fleet-crew-test-{}.db",
            uuid::Uuid::new_v4()
        ));
        let _ = std::fs::remove_file(&db_path);

        let prev = std::env::var("FORGEFLEET_DB_PATH").ok();
        unsafe {
            std::env::set_var("FORGEFLEET_DB_PATH", db_path.as_os_str());
        }

        (db_path.to_string_lossy().to_string(), prev)
    }

    fn restore_test_db_env(path: &str, prev: Option<String>) {
        match prev {
            Some(value) => unsafe {
                std::env::set_var("FORGEFLEET_DB_PATH", value);
            },
            None => unsafe {
                std::env::remove_var("FORGEFLEET_DB_PATH");
            },
        }
        let _ = std::fs::remove_file(path);
    }

    async fn spawn_mock_llm_server(always_fail: bool) -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(payload): Json<Value>| async move {
                if always_fail {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "forced failure"})),
                    );
                }

                let prompt = payload
                    .pointer("/messages/0/content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("<missing prompt>");

                (
                    StatusCode::OK,
                    Json(json!({
                        "choices": [
                            {
                                "message": {
                                    "content": format!("mock response: {}", prompt)
                                }
                            }
                        ]
                    })),
                )
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock llm listener");
        let addr = listener.local_addr().expect("listener local addr");

        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("mock llm server should run");
        });

        (format!("http://{addr}"), handle)
    }

    #[test]
    fn parse_task_type_maps_known_values() {
        assert_eq!(parse_task_type("code"), TaskType::Code);
        assert_eq!(parse_task_type("review"), TaskType::Review);
        assert_eq!(parse_task_type("debug"), TaskType::Debug);
        assert_eq!(parse_task_type("unknown"), TaskType::Chat);
    }

    #[test]
    fn set_json_dot_path_sets_nested_value() {
        let mut root = json!({"fleet": {"name": "old"}});
        set_json_dot_path(&mut root, "fleet.name", json!("new")).unwrap();
        assert_eq!(root["fleet"]["name"], "new");
    }

    #[test]
    fn set_json_dot_path_creates_missing_segments() {
        let mut root = json!({});
        set_json_dot_path(&mut root, "services.ff_api.port", json!(4000)).unwrap();
        assert_eq!(root["services"]["ff_api"]["port"], 4000);
    }

    #[test]
    fn parse_host_port_validates_shape() {
        let (host, port) = parse_host_port("127.0.0.1:51800").unwrap();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 51800);
        assert!(parse_host_port("bad-format").is_err());
    }

    #[test]
    fn extract_completion_text_prefers_message_content() {
        let payload = json!({
            "choices": [
                {
                    "message": { "content": "hello" },
                    "text": "fallback"
                }
            ]
        });
        assert_eq!(extract_completion_text(&payload).as_deref(), Some("hello"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fleet_crew_executes_pipeline_successfully() {
        let _guard = env_lock().lock().unwrap();
        let (db_path, prev_db_path) = setup_test_db();

        let (base_url, server_handle) = spawn_mock_llm_server(false).await;

        let repo_dir = std::env::temp_dir().join(format!("ff-mcp-repo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&repo_dir).unwrap();

        let response = fleet_crew(Some(json!({
            "task": "build a new API endpoint",
            "repo_dir": repo_dir,
            "llm_base_url": base_url,
            "max_parallelism": 2
        })))
        .await
        .expect("fleet_crew should succeed");

        let status = response["status"].as_str().unwrap_or_default();
        assert_eq!(status, "completed");
        assert_eq!(response["execution"]["status"], "completed");

        let steps = response["execution"]["steps"].as_array().unwrap();
        assert!(!steps.is_empty());
        assert!(steps.iter().all(|step| step["status"] == "succeeded"));

        assert_eq!(response["execution"]["summary"]["failed"], 0);
        assert_eq!(response["execution_plan"]["total_subtasks"], steps.len());

        server_handle.abort();
        let _ = std::fs::remove_dir_all(&repo_dir);
        restore_test_db_env(&db_path, prev_db_path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fleet_crew_reports_failures_when_steps_fail() {
        let _guard = env_lock().lock().unwrap();
        let (db_path, prev_db_path) = setup_test_db();

        let (base_url, server_handle) = spawn_mock_llm_server(true).await;

        let repo_dir = std::env::temp_dir().join(format!("ff-mcp-repo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&repo_dir).unwrap();

        let response = fleet_crew(Some(json!({
            "task": "build a new API endpoint",
            "repo_dir": repo_dir,
            "llm_base_url": base_url,
            "max_parallelism": 2
        })))
        .await
        .expect("fleet_crew should return structured failure response");

        assert_eq!(response["status"], "failed");
        assert_eq!(response["execution"]["status"], "failed");

        let steps = response["execution"]["steps"]
            .as_array()
            .expect("steps array should exist");
        assert!(!steps.is_empty());

        let failed_like = steps
            .iter()
            .filter(|step| {
                matches!(
                    step["status"].as_str(),
                    Some("failed") | Some("timed_out") | Some("missing_result")
                )
            })
            .count();
        let skipped = steps
            .iter()
            .filter(|step| step["status"].as_str() == Some("skipped"))
            .count();

        assert!(failed_like >= 1);
        assert!(skipped >= 1);
        assert!(
            response["execution"]["summary"]["failed"]
                .as_u64()
                .unwrap_or(0)
                >= 1
        );

        server_handle.abort();
        let _ = std::fs::remove_dir_all(&repo_dir);
        restore_test_db_env(&db_path, prev_db_path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn project_profile_round_trip_and_policy_resolve() {
        let _guard = env_lock().lock().unwrap();
        let (db_path, prev_db_path) = setup_test_db();

        let upsert = project_profile_upsert(Some(json!({
            "profile": {
                "project_id": "hireflow",
                "display_name": "HireFlow",
                "stack": ["web", "backend"],
                "languages": ["rust", "typescript"],
                "deployment_targets": ["staging", "production"],
                "review_strictness": "strict",
                "test_requirements": {
                    "require_unit": true,
                    "require_integration": true,
                    "require_e2e": false,
                    "minimum_coverage_pct": 80.0,
                    "required_commands": ["cargo test --workspace"]
                },
                "allowed_tiers": {
                    "min_tier": 2,
                    "max_tier": 4,
                    "allowed_models": ["qwen-32b", "qwen-72b"]
                },
                "data_sensitivity": "confidential",
                "compliance_flags": ["soc2", "pii"]
            }
        })))
        .await
        .expect("profile upsert should succeed");

        assert_eq!(upsert["project_id"], "hireflow");
        assert_eq!(upsert["policy"]["routing"]["min_tier"], 2);

        let fetched = project_profile_get(Some(json!({"project_id": "hireflow"})))
            .await
            .expect("profile get should succeed");
        assert_eq!(fetched["project_id"], "hireflow");

        let resolved = project_policy_resolve(Some(json!({
            "project_id": "hireflow",
            "prompt": "deploy this service to production",
            "start_tier": 1,
            "max_tier": 4,
            "model": "qwen-32b"
        })))
        .await
        .expect("policy resolve should succeed");

        assert_eq!(resolved["effective"]["start_tier"], 2);
        assert_eq!(resolved["effective"]["model_allowed"], true);
        assert_eq!(resolved["effective"]["approval_required"], true);

        restore_test_db_env(&db_path, prev_db_path);
    }
}
