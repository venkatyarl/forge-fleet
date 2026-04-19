pub mod agent_loop;
pub mod agent_roles;
pub mod bash_security;
pub mod brain;
pub mod hf_download;
pub mod hf_version_check;
pub mod mesh_check;
pub mod upgrade_playbooks;
pub mod verify_node;
pub mod version_check;
pub mod hive_sync;
pub mod learning;
pub mod supervisor;
pub mod chat_manager;
pub mod commands;
pub mod commands_extended;
pub mod compaction;
pub mod consensus;
pub mod features;
pub mod fleet_events;
pub mod fleet_info;
pub mod focus_stack;
pub mod ha;
pub mod inference_router;
pub mod file_history;
pub mod fleet_inference;
pub mod hooks;
pub mod leader_tick;
pub mod mcp_client;
pub mod mcp_tools;
pub mod memory;
pub mod alert_evaluator;
pub mod alert_policy_seed;
pub mod deployment_reconciler;
pub mod disk_sampler;
pub mod metrics_downsampler;
pub mod nats_log_layer;
pub mod job_sweeper;
pub mod coverage_guard;
pub mod model_catalog;
pub mod model_catalog_seed;
pub mod model_convert;
pub mod model_library_scanner;
pub mod model_runtime;
pub mod model_scout;
pub mod model_transfer;
pub mod model_upstream;
pub mod task_coverage_seed;
pub mod smart_lru;
pub mod multi_agent;
pub mod notifications;
pub mod openai_bridge;
pub mod openclaw;
pub mod orchestrator_agent;
pub mod permissions;
pub mod plugins;
pub mod revive;
pub mod rpc_inference;
pub mod project_github_sync;
pub mod project_registry;
pub mod scoped_memory;
pub mod session_store;
pub mod software_registry;
pub mod software_upstream;
pub mod streaming;
pub mod sub_agents;
pub mod system_prompt;
pub mod template_registry;
pub mod thinking;
pub mod tools;
pub mod training;

pub use software_registry::{seed_from_toml, SeedReport};
pub use model_catalog_seed::{seed_from_toml as seed_model_catalog_from_toml, ModelSeedReport};
pub use project_registry::{seed_from_toml as seed_projects_from_toml, ProjectSeedReport};
pub use alert_policy_seed::{seed_from_toml as seed_alert_policies_from_toml, AlertSeedReport};

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ff_db::{OperationalStore, queries::TaskRow};
use ff_orchestrator::{DecomposedSubTask, SubTaskType, TemplateDecomposer};
use ff_pipeline::{ExecutorConfig, PipelineGraph, Step, StepKind, execute};
use ff_security::autonomy_policy::{ActionType, ComplianceLevel, Decision, RiskLevel, decide};
use serde_json::Value;
use tokio::sync::watch;
use tracing::{debug, info, warn};

/// Embedded agent subsystem configuration used by `forgefleetd`.
#[derive(Debug, Clone)]
pub struct EmbeddedAgentConfig {
    /// Node name used for ownership and audit metadata.
    pub node_name: String,
    /// Toggle autonomous claim/decompose/execute/report mode.
    pub autonomous_mode: bool,
    /// Poll interval for autonomous work claiming.
    pub poll_interval_secs: u64,
    /// Optional ownership/lease API endpoint.
    pub ownership_api_base_url: Option<String>,
    /// Optional LLM endpoint override for non-shell steps.
    pub llm_base_url: Option<String>,
    /// Optional default model for LLM steps.
    pub llm_model: Option<String>,
    /// Local-first inference router with fleet fallback. When set, LLM steps
    /// use this to pick the best available endpoint automatically.
    pub inference_router: Option<Arc<crate::inference_router::InferenceRouter>>,
}

impl EmbeddedAgentConfig {
    /// Backward-compatible heartbeat-only mode.
    pub fn heartbeat_only(node_name: String) -> Self {
        Self {
            node_name,
            autonomous_mode: false,
            poll_interval_secs: 8,
            ownership_api_base_url: None,
            llm_base_url: None,
            llm_model: None,
            inference_router: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkExecutionKind {
    ShellCommand,
    ModelInference,
    Generic,
}

#[derive(Debug, Clone)]
struct ClaimedWorkItem {
    task_id: String,
    kind: WorkExecutionKind,
    summary: String,
    shell_command: Option<String>,
    model: Option<String>,
    max_tokens: Option<u32>,
}

#[derive(Debug, Clone)]
struct ExecutionOutcome {
    success: bool,
    output: String,
    duration_ms: u64,
}

#[derive(Clone)]
struct LeaseClient {
    base_url: Option<String>,
    node_name: String,
    http: reqwest::Client,
}

impl LeaseClient {
    fn new(base_url: Option<String>, node_name: String) -> Self {
        Self {
            base_url,
            node_name,
            http: reqwest::Client::new(),
        }
    }

    /// Acquire ownership lease for a claimed task if a lease API is configured.
    async fn acquire(&self, task_id: &str) -> Result<bool> {
        let Some(base) = self.base_url.as_deref() else {
            return Ok(true);
        };

        let payload = serde_json::json!({
            "task_id": task_id,
            "owner": self.node_name,
            "ttl_secs": 90,
        });

        for endpoint in [
            "/api/ownership/lease/claim",
            "/ownership/lease/claim",
            "/lease/claim",
        ] {
            let url = format!("{}{}", base.trim_end_matches('/'), endpoint);
            match self.http.post(&url).json(&payload).send().await {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(value) = resp.json::<Value>().await {
                        if let Some(granted) = value.get("granted").and_then(Value::as_bool) {
                            return Ok(granted);
                        }
                    }
                    return Ok(true);
                }
                Ok(resp) => {
                    debug!(task_id, status = %resp.status(), %url, "lease endpoint rejected claim");
                }
                Err(err) => {
                    debug!(task_id, error = %err, %url, "lease endpoint unavailable");
                }
            }
        }

        // If a lease service was configured but unreachable, fail closed so
        // we don't execute without ownership guarantees.
        Ok(false)
    }

    /// Release ownership lease after task completion/failure.
    async fn release(&self, task_id: &str) {
        let Some(base) = self.base_url.as_deref() else {
            return;
        };

        let payload = serde_json::json!({
            "task_id": task_id,
            "owner": self.node_name,
        });

        for endpoint in [
            "/api/ownership/lease/release",
            "/ownership/lease/release",
            "/lease/release",
        ] {
            let url = format!("{}{}", base.trim_end_matches('/'), endpoint);
            match self.http.post(&url).json(&payload).send().await {
                Ok(resp) if resp.status().is_success() => return,
                Ok(_) => continue,
                Err(_) => continue,
            }
        }
    }
}

/// Embedded agent subsystem entrypoint for root ForgeFleet daemon wiring.
///
/// - Heartbeat mode (default): preserves existing behavior.
/// - Autonomous mode: claim next work item, decompose into executable steps,
///   execute via orchestrator+pipeline, and persist status transitions/results.
pub async fn run(
    config: EmbeddedAgentConfig,
    operational_store: OperationalStore,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    info!(
        node = %config.node_name,
        autonomous = config.autonomous_mode,
        "ff-agent subsystem started"
    );

    // Start the agent HTTP server for inter-node messaging callbacks on port 50002.
    // Uses a minimal standalone router so it compiles without the binary-only state module.
    {
        let message_router = axum::Router::new()
            .route("/health", axum::routing::get(agent_http_health))
            .route("/agent/message", axum::routing::post(handle_agent_message))
            .route("/tasks", axum::routing::get(list_agent_tasks));
        let agent_http_addr = "0.0.0.0:50002";
        tokio::spawn(async move {
            match tokio::net::TcpListener::bind(agent_http_addr).await {
                Ok(listener) => {
                    tracing::info!(addr = agent_http_addr, "agent message server listening");
                    if let Err(err) = axum::serve(listener, message_router).await {
                        tracing::error!(error = %err, "agent HTTP server failed");
                    }
                }
                Err(err) => {
                    tracing::error!(error = %err, addr = agent_http_addr, "failed to bind agent HTTP server");
                }
            }
        });
    }

    if config.autonomous_mode {
        run_autonomous_loop(config, operational_store, &mut shutdown_rx).await?;
    } else {
        run_heartbeat_loop(config.node_name.clone(), &mut shutdown_rx).await;
    }

    info!("ff-agent subsystem stopped");
    Ok(())
}

async fn run_heartbeat_loop(node_name: String, shutdown_rx: &mut watch::Receiver<bool>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(15));

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                info!(%node_name, "ff-agent heartbeat");
            }
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }
}

async fn run_autonomous_loop(
    config: EmbeddedAgentConfig,
    operational_store: OperationalStore,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<()> {
    let lease_client = LeaseClient::new(
        config.ownership_api_base_url.clone(),
        config.node_name.clone(),
    );

    let mut ticker = tokio::time::interval(Duration::from_secs(config.poll_interval_secs.max(1)));

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Some(task) = claim_next(&operational_store, &config.node_name).await? {
                    handle_claimed_task(&config, &operational_store, &lease_client, task).await?;
                }
            }
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }

    Ok(())
}

async fn claim_next(store: &OperationalStore, node_name: &str) -> Result<Option<TaskRow>> {
    store
        .claim_next_task(node_name)
        .await
        .context("failed to claim next autonomous task")
}

async fn handle_claimed_task(
    config: &EmbeddedAgentConfig,
    store: &OperationalStore,
    lease_client: &LeaseClient,
    task: TaskRow,
) -> Result<()> {
    let task_id = task.id.clone();
    info!(task_id = %task_id, node = %config.node_name, "claimed autonomous task");

    persist_transition(
        store,
        &task_id,
        "queued",
        "claimed",
        &config.node_name,
        Some(serde_json::json!({"kind": task.kind})),
    )
    .await?;

    if !acquire_lease(store, lease_client, &task_id, &config.node_name).await? {
        persist_failed(
            store,
            &task_id,
            &config.node_name,
            "lease acquisition rejected",
            0,
        )
        .await?;
        warn!(task_id = %task_id, "lease not granted; task marked failed");
        return Ok(());
    }

    let claimed = parse_claimed_item(task)?;
    let (action_type, compliance_level, risk_level) = classify_policy_input(&claimed);
    let decision = decide(action_type, compliance_level, risk_level);

    if decision != Decision::AutoAllow {
        let reason = format!(
            "blocked by autonomy policy: action={}, compliance={}, risk={}, decision={}",
            action_type.as_str(),
            compliance_level.as_str(),
            risk_level.as_str(),
            decision.as_str()
        );

        record_autonomy_event(store, "pre_execution_gate", action_type, decision, &reason).await?;

        let target_status = if decision == Decision::RequireHumanApproval {
            "review"
        } else {
            "failed"
        };

        persist_transition(
            store,
            &claimed.task_id,
            "claimed",
            target_status,
            &config.node_name,
            Some(serde_json::json!({
                "decision": decision.as_str(),
                "action_type": action_type.as_str(),
                "risk": risk_level.as_str(),
                "compliance": compliance_level.as_str(),
            })),
        )
        .await?;

        persist_result(store, &claimed.task_id, false, &reason, 0).await?;
        release_lease(store, lease_client, &task_id, &config.node_name).await;

        if decision == Decision::RequireHumanApproval {
            warn!(task_id = %claimed.task_id, "autonomous execution paused for human approval");
        } else {
            warn!(task_id = %claimed.task_id, "autonomous execution denied by policy");
        }

        return Ok(());
    }

    persist_transition(
        store,
        &task_id,
        "claimed",
        "in_progress",
        &config.node_name,
        None,
    )
    .await?;

    let outcome = execute_claimed_task(config, &claimed).await;

    match outcome {
        Ok(outcome) if outcome.success => {
            persist_transition(
                store,
                &claimed.task_id,
                "in_progress",
                "review",
                &config.node_name,
                None,
            )
            .await?;

            persist_transition(
                store,
                &claimed.task_id,
                "review",
                "done",
                &config.node_name,
                None,
            )
            .await?;

            persist_result(
                store,
                &claimed.task_id,
                true,
                &outcome.output,
                outcome.duration_ms,
            )
            .await?;

            info!(task_id = %claimed.task_id, duration_ms = outcome.duration_ms, "autonomous task completed");
        }
        Ok(outcome) => {
            persist_failed(
                store,
                &claimed.task_id,
                &config.node_name,
                &outcome.output,
                outcome.duration_ms,
            )
            .await?;
            warn!(task_id = %claimed.task_id, "autonomous task failed");
        }
        Err(err) => {
            persist_failed(
                store,
                &claimed.task_id,
                &config.node_name,
                &format!("execution error: {err}"),
                0,
            )
            .await?;
            warn!(task_id = %claimed.task_id, error = %err, "autonomous task errored");
        }
    }

    release_lease(store, lease_client, &task_id, &config.node_name).await;
    Ok(())
}

async fn acquire_lease(
    store: &OperationalStore,
    lease_client: &LeaseClient,
    task_id: &str,
    node_name: &str,
) -> Result<bool> {
    match store.ownership_claim(task_id, node_name, 90).await {
        Ok(granted) => return Ok(granted),
        Err(err) => {
            debug!(task_id, error = %err, "local ownership lease unavailable; falling back to HTTP lease client");
        }
    }

    lease_client.acquire(task_id).await
}

async fn release_lease(
    store: &OperationalStore,
    lease_client: &LeaseClient,
    task_id: &str,
    node_name: &str,
) {
    let released = store
        .ownership_release(task_id, node_name)
        .await
        .unwrap_or(false);

    if !released {
        lease_client.release(task_id).await;
    }
}

async fn persist_failed(
    store: &OperationalStore,
    task_id: &str,
    node_name: &str,
    output: &str,
    duration_ms: u64,
) -> Result<()> {
    persist_transition(store, task_id, "in_progress", "failed", node_name, None).await?;
    persist_result(store, task_id, false, output, duration_ms).await
}

async fn persist_result(
    store: &OperationalStore,
    task_id: &str,
    success: bool,
    output: &str,
    duration_ms: u64,
) -> Result<()> {
    store
        .record_task_result(
            task_id,
            success,
            output,
            duration_ms.min(i64::MAX as u64) as i64,
        )
        .await
        .context("failed to persist task result")
}

async fn persist_transition(
    store: &OperationalStore,
    task_id: &str,
    from: &str,
    to: &str,
    node_name: &str,
    extra: Option<Value>,
) -> Result<()> {
    let target_status = to.to_string();
    let from_status = from.to_string();
    let node = node_name.to_string();

    let mut details = serde_json::json!({
        "task_id": task_id,
        "from": from,
        "to": to,
        "node": node_name,
    });

    if let Some(extra) = extra
        && let Some(map) = details.as_object_mut()
    {
        map.insert("extra".to_string(), extra);
    }

    let details_json = details.to_string();

    let id_for_error = task_id.to_string();
    let target_for_error = target_status.clone();

    let id = task_id.to_string();
    store
        .set_task_status(&id, &target_status)
        .await
        .context("failed to set task status")?;

    store
        .audit_log(
            "agent_task_status_transition",
            &node,
            Some(&id),
            &details_json,
            Some(&node),
        )
        .await
        .with_context(|| {
            format!(
                "failed to persist task transition {from_status}->{target_for_error} for {id_for_error}"
            )
        })
        .map(|_| ())
}

async fn record_autonomy_event(
    store: &OperationalStore,
    event_type: &str,
    action_type: ActionType,
    decision: Decision,
    reason: &str,
) -> Result<()> {
    let event = event_type.to_string();
    let action = action_type.as_str().to_string();
    let chosen = decision.as_str().to_string();
    let why = reason.to_string();

    store
        .insert_autonomy_event(&event, &action, &chosen, &why)
        .await
        .context("failed to persist autonomy event")
        .map(|_| ())
}

fn classify_policy_input(task: &ClaimedWorkItem) -> (ActionType, ComplianceLevel, RiskLevel) {
    match task.kind {
        WorkExecutionKind::ModelInference => (
            ActionType::OperationalRead,
            ComplianceLevel::Standard,
            RiskLevel::Low,
        ),
        WorkExecutionKind::Generic => (
            ActionType::MutatingOperation,
            ComplianceLevel::Elevated,
            RiskLevel::Medium,
        ),
        WorkExecutionKind::ShellCommand => {
            let command = task
                .shell_command
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase();

            let compliance = if looks_compliance_critical(&command) {
                ComplianceLevel::ComplianceCritical
            } else {
                ComplianceLevel::Standard
            };

            if looks_destructive_shell(&command) {
                return (
                    ActionType::DestructiveOperation,
                    ComplianceLevel::ComplianceCritical,
                    RiskLevel::High,
                );
            }

            if looks_read_only_shell(&command) {
                return (ActionType::OperationalRead, compliance, RiskLevel::Low);
            }

            (ActionType::MutatingOperation, compliance, RiskLevel::Medium)
        }
    }
}

fn looks_read_only_shell(command: &str) -> bool {
    [
        "cat ", "ls", "grep ", "head ", "tail ", "pwd", "echo ", "whoami", "date", "uname", "df ",
        "du ",
    ]
    .iter()
    .any(|needle| command.starts_with(needle))
}

fn looks_destructive_shell(command: &str) -> bool {
    [
        "rm -rf",
        "mkfs",
        "shutdown",
        "reboot",
        "userdel",
        "dd if=",
        "drop table",
        "truncate table",
    ]
    .iter()
    .any(|needle| command.contains(needle))
}

fn looks_compliance_critical(command: &str) -> bool {
    [
        "pii",
        "gdpr",
        "hipaa",
        "sox",
        "compliance",
        "audit",
        "secrets",
    ]
    .iter()
    .any(|needle| command.contains(needle))
}

fn parse_claimed_item(task: TaskRow) -> Result<ClaimedWorkItem> {
    let payload: Value = serde_json::from_str(&task.payload_json)
        .unwrap_or_else(|_| serde_json::json!({ "raw": task.payload_json }));

    let kind = task.kind.trim().to_ascii_lowercase();
    let (exec_kind, shell_command, model, max_tokens, summary) = match kind.as_str() {
        "shell_command" => {
            let command = payload
                .get("command")
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .filter(|s| !s.trim().is_empty())
                .or_else(|| {
                    payload
                        .get("shell")
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                })
                .with_context(|| format!("task {} missing shell command payload", task.id))?;

            let summary = payload
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or(&command)
                .to_string();

            (
                WorkExecutionKind::ShellCommand,
                Some(command),
                None,
                None,
                summary,
            )
        }
        "model_inference" => {
            let prompt = payload
                .get("prompt")
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .or_else(|| {
                    payload
                        .get("instruction")
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                })
                .unwrap_or_else(|| task.payload_json.clone());

            let summary = payload
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or(&prompt)
                .to_string();

            (
                WorkExecutionKind::ModelInference,
                None,
                payload
                    .get("model")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                payload
                    .get("max_tokens")
                    .and_then(Value::as_u64)
                    .map(|n| n.min(u32::MAX as u64) as u32),
                summary,
            )
        }
        _ => {
            let summary = payload
                .get("task")
                .or_else(|| payload.get("description"))
                .or_else(|| payload.get("prompt"))
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .unwrap_or_else(|| task.payload_json.clone());
            (WorkExecutionKind::Generic, None, None, None, summary)
        }
    };

    Ok(ClaimedWorkItem {
        task_id: task.id,
        kind: exec_kind,
        summary,
        shell_command,
        model,
        max_tokens,
    })
}

async fn execute_claimed_task(
    config: &EmbeddedAgentConfig,
    task: &ClaimedWorkItem,
) -> Result<ExecutionOutcome> {
    let start = Instant::now();

    let (_pattern, _strategy, decomposed) = TemplateDecomposer::decompose(&task.summary);
    let graph = build_pipeline_graph(task, &decomposed)?;

    let mut exec_config = ExecutorConfig::default();

    // Prefer the inference router's active endpoint; fall back to static llm_base_url.
    let effective_llm_base_url = config
        .inference_router
        .as_ref()
        .and_then(|r| r.active_url())
        .or_else(|| config.llm_base_url.clone());

    if let Some(base) = effective_llm_base_url {
        exec_config = exec_config.with_llm_base_url(base);
    }
    if let Some(model) = task.model.clone().or_else(|| config.llm_model.clone()) {
        exec_config = exec_config.with_llm_model(model);
    }

    match execute(&graph, exec_config, None).await {
        Ok(run) => {
            let mut lines = Vec::new();
            for step_id in graph.step_ids() {
                if let Some(result) = run.results.get(step_id) {
                    lines.push(format!(
                        "{} [{}]\n{}",
                        step_id,
                        format!("{:?}", result.status).to_lowercase(),
                        result.output.trim()
                    ));
                }
            }

            Ok(ExecutionOutcome {
                success: run.success,
                output: lines.join("\n\n"),
                duration_ms: run.total_duration_ms,
            })
        }
        Err(err) => Ok(ExecutionOutcome {
            success: false,
            output: format!("pipeline execution failed: {err}"),
            duration_ms: start.elapsed().as_millis().min(u64::MAX as u128) as u64,
        }),
    }
}

fn build_pipeline_graph(
    task: &ClaimedWorkItem,
    subtasks: &[DecomposedSubTask],
) -> Result<PipelineGraph> {
    let mut graph = PipelineGraph::new();
    let mut step_ids = Vec::with_capacity(subtasks.len());

    let execution_index = subtasks
        .iter()
        .position(|st| matches!(st.task_type, SubTaskType::Code))
        .unwrap_or(0);

    for (idx, subtask) in subtasks.iter().enumerate() {
        let step_id = format!("{}-{}", idx + 1, slugify(&subtask.title));
        let kind = match task.kind {
            WorkExecutionKind::ShellCommand => {
                if idx == execution_index {
                    StepKind::Shell {
                        command: task
                            .shell_command
                            .clone()
                            .unwrap_or_else(|| "echo missing shell command".to_string()),
                        cwd: None,
                        env: Vec::new(),
                    }
                } else {
                    StepKind::Shell {
                        command: format!(
                            "printf '%s\\n' {}",
                            shell_quote(&format!("autonomous step: {}", subtask.description))
                        ),
                        cwd: None,
                        env: Vec::new(),
                    }
                }
            }
            WorkExecutionKind::ModelInference | WorkExecutionKind::Generic => StepKind::LlmPrompt {
                prompt: format!(
                    "You are executing autonomous work item {}.\nSubtask: {}\nDescription: {}",
                    task.task_id, subtask.title, subtask.description
                ),
                model: task.model.clone(),
                max_tokens: task.max_tokens.or(Some(512)),
            },
        };

        let step = Step::new(step_id.clone(), subtask.title.clone(), kind);
        graph
            .add_step(step)
            .with_context(|| format!("failed to add pipeline step {step_id}"))?;
        step_ids.push(step_id);
    }

    for (idx, subtask) in subtasks.iter().enumerate() {
        for dep_idx in &subtask.dependency_indices {
            if let (Some(dep), Some(current)) = (step_ids.get(*dep_idx), step_ids.get(idx)) {
                graph
                    .add_dependency(&current.clone().into(), &dep.clone().into())
                    .with_context(|| format!("failed to add dependency {} -> {}", dep, current))?;
            }
        }
    }

    Ok(graph)
}

fn slugify(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

fn shell_quote(input: &str) -> String {
    if input.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", input.replace('\'', "'\"'\"'"))
}

// ---------------------------------------------------------------------------
// Standalone inter-agent message HTTP handlers (used by the embedded run() loop)
// ---------------------------------------------------------------------------

async fn agent_http_health() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({"ok": true, "service": "ff-agent-message-server"}))
}

async fn list_agent_tasks() -> axum::Json<serde_json::Value> {
    use crate::tools::task_tools::TASK_STORE_PUB;
    let mut tasks: Vec<serde_json::Value> = TASK_STORE_PUB
        .iter()
        .map(|e| {
            let t = e.value();
            serde_json::json!({
                "id": t.id,
                "subject": t.subject,
                "status": t.status,
                "origin_node": t.origin_node,
                "created_at": t.created_at,
                "output": t.output,
            })
        })
        .collect();
    // Sort by created_at descending
    tasks.sort_by(|a, b| {
        let ca = a.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        let cb = b.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        cb.cmp(ca)
    });
    let count = tasks.len();
    axum::Json(serde_json::json!({"tasks": tasks, "count": count}))
}

async fn handle_agent_message(
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    let task_id = payload.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
    let status = payload.get("status").and_then(|v| v.as_str()).unwrap_or("");
    let output = payload.get("output").and_then(|v| v.as_str()).unwrap_or("");
    let from = payload.get("from").and_then(|v| v.as_str()).unwrap_or("?");

    tracing::info!(task_id, status, from, "agent message received");

    // Update in-memory task store if a task completion callback arrives.
    if !task_id.is_empty() && status == "completed" {
        use crate::tools::task_tools::TASK_STORE_PUB;
        if let Some(mut task) = TASK_STORE_PUB.get_mut(task_id) {
            task.status = "completed".to_string();
            task.output = Some(output.to_string());
        }
    }

    axum::Json(serde_json::json!({"ok": true}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use ff_db::{DbPool, DbPoolConfig, OperationalStore, queries, run_migrations};
    use tokio::time::timeout;
    use uuid::Uuid;

    async fn setup_db() -> DbPool {
        let pool = DbPool::open(DbPoolConfig::in_memory()).expect("open in-memory db");
        pool.with_conn(|conn| {
            let _ = run_migrations(conn)?;
            queries::upsert_node(
                conn,
                &queries::NodeRow {
                    id: Uuid::new_v4().to_string(),
                    name: "taylor".into(),
                    host: "127.0.0.1".into(),
                    port: 51800,
                    role: "leader".into(),
                    election_priority: 1,
                    status: "online".into(),
                    hardware_json: "{}".into(),
                    models_json: "[]".into(),
                    last_heartbeat: None,
                    registered_at: Utc::now().to_rfc3339(),
                },
            )?;
            Ok(())
        })
        .await
        .expect("run migrations");
        pool
    }

    async fn insert_shell_task(pool: &DbPool, status: &str, command: &str) -> String {
        let id = Uuid::new_v4().to_string();
        let task = queries::TaskRow {
            id: id.clone(),
            kind: "shell_command".into(),
            payload_json: serde_json::json!({"command": command, "summary": command}).to_string(),
            status: status.to_string(),
            assigned_node: None,
            priority: 10,
            created_at: Utc::now().to_rfc3339(),
            started_at: None,
            completed_at: None,
        };

        pool.with_conn(move |conn| queries::insert_task(conn, &task))
            .await
            .expect("insert task");

        id
    }

    #[tokio::test]
    async fn heartbeat_mode_keeps_pending_tasks_untouched() {
        let pool = setup_db().await;
        let task_id = insert_shell_task(&pool, "pending", "echo should-not-run").await;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn(run(
            EmbeddedAgentConfig::heartbeat_only("taylor".into()),
            OperationalStore::sqlite(pool.clone()),
            shutdown_rx,
        ));

        tokio::time::sleep(Duration::from_millis(250)).await;
        let _ = shutdown_tx.send(true);
        let _ = handle.await.expect("join");

        let fetched = pool
            .with_conn(move |conn| queries::get_task(conn, &task_id))
            .await
            .expect("query task")
            .expect("task exists");

        assert_eq!(fetched.status, "pending");
    }

    #[tokio::test]
    async fn autonomous_mode_claims_executes_and_transitions_to_done() {
        let pool = setup_db().await;
        let task_id = insert_shell_task(&pool, "pending", "echo autonomous-ok").await;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let cfg = EmbeddedAgentConfig {
            node_name: "taylor".into(),
            autonomous_mode: true,
            poll_interval_secs: 1,
            ownership_api_base_url: None,
            llm_base_url: None,
            llm_model: None,
            inference_router: None,
        };

        let handle = tokio::spawn(run(
            cfg,
            OperationalStore::sqlite(pool.clone()),
            shutdown_rx,
        ));

        let task_id_for_wait = task_id.clone();
        let wait_result = timeout(Duration::from_secs(8), async {
            loop {
                let current = pool
                    .with_conn({
                        let task_id = task_id_for_wait.clone();
                        move |conn| queries::get_task(conn, &task_id)
                    })
                    .await
                    .expect("query task")
                    .expect("task exists");

                if current.status == "done" || current.status == "failed" {
                    break current.status;
                }

                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("task should reach terminal state quickly");

        let _ = shutdown_tx.send(true);
        let _ = handle.await.expect("join");

        assert_eq!(wait_result, "done");

        let events = pool
            .with_conn(move |conn| queries::recent_audit_log(conn, 10))
            .await
            .expect("audit query");

        let mut transitions: Vec<(String, String)> = events
            .iter()
            .filter(|e| e.event_type == "agent_task_status_transition")
            .filter_map(|e| serde_json::from_str::<Value>(&e.details_json).ok())
            .filter_map(|v| {
                let from = v.get("from")?.as_str()?.to_string();
                let to = v.get("to")?.as_str()?.to_string();
                Some((from, to))
            })
            .collect();

        transitions.reverse(); // recent_audit_log returns newest first.

        assert!(transitions.contains(&("queued".to_string(), "claimed".to_string())));
        assert!(transitions.contains(&("claimed".to_string(), "in_progress".to_string())));
        assert!(transitions.contains(&("in_progress".to_string(), "review".to_string())));
        assert!(transitions.contains(&("review".to_string(), "done".to_string())));

        let task_id_for_result = task_id.clone();
        let (success, output): (i64, String) = pool
            .with_conn(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT success, output FROM task_results WHERE task_id = ?1 LIMIT 1",
                )?;
                let row = stmt.query_row([task_id_for_result.as_str()], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
                Ok(row)
            })
            .await
            .expect("task result query");
        assert_eq!(success, 1);
        assert!(output.contains("autonomous-ok"));
    }

    #[tokio::test]
    async fn autonomous_mode_blocks_destructive_commands_and_records_event() {
        let pool = setup_db().await;
        let task_id = insert_shell_task(&pool, "pending", "rm -rf /tmp/unsafe").await;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let cfg = EmbeddedAgentConfig {
            node_name: "taylor".into(),
            autonomous_mode: true,
            poll_interval_secs: 1,
            ownership_api_base_url: None,
            llm_base_url: None,
            llm_model: None,
            inference_router: None,
        };

        let handle = tokio::spawn(run(
            cfg,
            OperationalStore::sqlite(pool.clone()),
            shutdown_rx,
        ));

        let task_id_for_wait = task_id.clone();
        let terminal = timeout(Duration::from_secs(8), async {
            loop {
                let current = pool
                    .with_conn({
                        let task_id = task_id_for_wait.clone();
                        move |conn| queries::get_task(conn, &task_id)
                    })
                    .await
                    .expect("query task")
                    .expect("task exists");

                if current.status == "failed" || current.status == "review" {
                    break current.status;
                }

                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("task should be gated quickly");

        let _ = shutdown_tx.send(true);
        let _ = handle.await.expect("join");

        assert_eq!(terminal, "failed");

        let events = pool
            .with_conn(move |conn| queries::list_recent_autonomy_events(conn, 5))
            .await
            .expect("autonomy event query");

        let event = events
            .into_iter()
            .find(|e| e.event_type == "pre_execution_gate")
            .expect("policy gate event should exist");

        assert_eq!(event.action_type, "destructive_operation");
        assert_eq!(event.decision, "deny");
        assert!(event.reason.contains("blocked by autonomy policy"));
    }
}
