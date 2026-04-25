//! Orchestrator agent — smart dispatcher that breaks complex tasks into subtasks
//! and routes them to the best fleet node for execution.
//!
//! Architecture:
//! - Orchestrator runs on the fleet leader (the node with the lowest election_priority)
//! - Analyzes user intent → decides what needs to happen
//! - Routes work to specialist worker nodes
//! - Collects results → synthesizes response
//!
//! This is what makes ForgeFleet work like Claude Code but distributed.

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::agent_loop::{AgentOutcome, AgentSession, AgentSessionConfig};

// ---------------------------------------------------------------------------
// Fleet node capabilities
// ---------------------------------------------------------------------------

/// What a fleet node can do.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeCapability {
    pub name: String,
    pub ip: String,
    pub user: String,
    pub llm_port: u16,
    pub model_name: String,
    pub model_params: u64,
    pub strengths: Vec<Strength>,
    pub available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strength {
    Coding,
    Reasoning,
    FastResponse,
    LargeContext,
    Review,
    Research,
    General,
}

/// Get the fleet's capabilities from the Postgres `fleet_nodes` + `fleet_models` tables.
///
/// Each (node, model) pair becomes one `NodeCapability`. Strengths are inferred
/// from the model's family, size, and preferred workloads so that `select_nodes`
/// can still route tasks without any hardcoded fleet identities.
pub async fn fleet_capabilities() -> Vec<NodeCapability> {
    let snapshot = match crate::fleet_info::fetch_snapshot().await {
        Ok(s) => s,
        Err(err) => {
            warn!(error = %err, "fleet_capabilities: failed to query Postgres");
            return Vec::new();
        }
    };

    let mut out = Vec::new();
    for node in &snapshot.nodes {
        let node_models: Vec<&ff_db::FleetModelRow> = snapshot
            .models
            .iter()
            .filter(|m| m.node_name == node.name)
            .collect();

        if node_models.is_empty() {
            // Node with no registered model — still expose a generic capability
            // at the conventional port 51000 so ops tooling can target it.
            out.push(NodeCapability {
                name: node.name.clone(),
                ip: node.ip.clone(),
                user: node.ssh_user.clone(),
                llm_port: 51000,
                model_name: "unknown".into(),
                model_params: 0,
                strengths: vec![Strength::General],
                available: node.status.eq_ignore_ascii_case("online"),
            });
            continue;
        }

        let model_count = node_models.len();
        for m in &node_models {
            let cap_name = if model_count > 1 {
                format!("{}-{}", node.name, m.slug)
            } else {
                node.name.clone()
            };
            let strengths = infer_strengths(m);
            let params = infer_params_from_name(&m.name);
            out.push(NodeCapability {
                name: cap_name,
                ip: node.ip.clone(),
                user: node.ssh_user.clone(),
                llm_port: m.port as u16,
                model_name: m.name.clone(),
                model_params: params,
                strengths,
                available: node.status.eq_ignore_ascii_case("online"),
            });
        }
    }
    out
}

/// Infer a model's strengths from its family, size, and preferred workloads.
fn infer_strengths(model: &ff_db::FleetModelRow) -> Vec<Strength> {
    let mut set: Vec<Strength> = Vec::new();
    let family = model.family.to_ascii_lowercase();
    let name = model.name.to_ascii_lowercase();
    let params = infer_params_from_name(&model.name);

    // Preferred workloads from the DB take precedence.
    if let Some(arr) = model.preferred_workloads.as_array() {
        for v in arr {
            if let Some(s) = v.as_str() {
                match s.to_ascii_lowercase().as_str() {
                    "code" | "coding" => set.push(Strength::Coding),
                    "reasoning" | "reason" => set.push(Strength::Reasoning),
                    "review" => set.push(Strength::Review),
                    "fast" | "fast_response" => set.push(Strength::FastResponse),
                    "large_context" | "long_context" => set.push(Strength::LargeContext),
                    "research" => set.push(Strength::Research),
                    "general" => set.push(Strength::General),
                    _ => {}
                }
            }
        }
    }

    // Family-based defaults.
    if set.is_empty() {
        if family.contains("coder") || name.contains("coder") {
            set.push(Strength::Coding);
            set.push(Strength::Review);
        } else if family.contains("gemma") || family.contains("llama") {
            set.push(Strength::General);
            set.push(Strength::Reasoning);
        } else if family.contains("qwen") {
            set.push(Strength::General);
            set.push(Strength::Reasoning);
        } else {
            set.push(Strength::General);
        }
    }

    // Size-based defaults.
    if params >= 65_000_000_000 {
        if !set.contains(&Strength::Reasoning) {
            set.push(Strength::Reasoning);
        }
        if !set.contains(&Strength::LargeContext) {
            set.push(Strength::LargeContext);
        }
    } else if params > 0 && params <= 10_000_000_000 && !set.contains(&Strength::FastResponse) {
        set.push(Strength::FastResponse);
    }

    set
}

/// Parse a model's parameter count from its name (e.g. "Qwen2.5-72B" → 72e9).
fn infer_params_from_name(name: &str) -> u64 {
    let lower = name.to_ascii_lowercase();
    // Look for patterns like "7b", "32b", "405b".
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'b' {
                if let Ok(n) = lower[start..i].parse::<f64>() {
                    return (n * 1_000_000_000.0) as u64;
                }
            }
        } else {
            i += 1;
        }
    }
    0
}

// ---------------------------------------------------------------------------
// Task analysis
// ---------------------------------------------------------------------------

/// What type of work a task requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    /// Simple question — just answer it
    SimpleQuestion,
    /// Code writing — needs a coding model
    CodeWriting,
    /// Code review — needs careful analysis
    CodeReview,
    /// Fleet operation — needs SSH/tool access
    FleetOp,
    /// Research — needs web search + analysis
    Research,
    /// Complex task — needs to be broken into subtasks
    Complex,
    /// Multi-node — explicitly needs parallel execution
    MultiNode,
    /// Architecture/design — needs reasoning model
    Architecture,
    /// Testing — write or run tests
    Testing,
    /// Documentation — write docs, comments, READMEs
    Documentation,
    /// Debugging — investigate and fix issues
    Debugging,
}

/// Analyze what type of task the user is asking for.
///
/// Classification is based on keyword matching, improved with patterns from
/// router training data analysis. Categories are checked in priority order:
/// multi-node > fleet > debugging > testing > architecture > code-writing >
/// code-review > documentation > research > complex > simple.
pub fn analyze_task(prompt: &str) -> TaskType {
    let lower = prompt.to_ascii_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();

    // ---- Multi-node patterns (highest priority) ----
    if lower.contains("all computers")
        || lower.contains("all nodes")
        || lower.contains("every node")
        || lower.contains("each computer")
        || lower.contains("entire fleet")
        || lower.contains("all of them")
        || lower.contains("each of them")
        || lower.contains("across all")
        || lower.contains("on every machine")
        || lower.contains("cluster-wide")
    {
        return TaskType::MultiNode;
    }

    // ---- Fleet operations ----
    if lower.contains("ssh")
        || lower.contains("deploy")
        || lower.contains("restart service")
        || lower.contains("install on")
        || lower.contains("update all")
        || lower.contains("fleet status")
        || lower.contains("node status")
        || lower.contains("systemctl")
        || lower.contains("ansible")
        || lower.contains("rolling update")
        || lower.contains("health check")
        || (lower.contains("fleet")
            && has_any(&words, &["manage", "configure", "monitor", "check"]))
        || (lower.contains("node") && has_any(&words, &["restart", "stop", "start", "drain"]))
    {
        return TaskType::FleetOp;
    }

    // ---- Debugging / troubleshooting ----
    if lower.contains("debug")
        || lower.contains("troubleshoot")
        || lower.contains("not working")
        || lower.contains("doesn't work")
        || lower.contains("broken")
        || lower.contains("investigate")
        || lower.contains("why is")
        || lower.contains("stack trace")
        || lower.contains("segfault")
        || lower.contains("panic at")
        || lower.contains("core dump")
        || lower.contains("error log")
        || (lower.contains("fix")
            && has_any(&words, &["bug", "error", "crash", "issue", "failure"]))
    {
        return TaskType::Debugging;
    }

    // ---- Testing ----
    if lower.contains("write test")
        || lower.contains("add test")
        || lower.contains("unit test")
        || lower.contains("integration test")
        || lower.contains("run test")
        || lower.contains("test coverage")
        || lower.contains("cargo test")
        || lower.contains("pytest")
        || lower.contains("jest")
        || lower.contains("benchmark")
        || (lower.contains("test") && has_any(&words, &["create", "write", "add", "run", "fix"]))
    {
        return TaskType::Testing;
    }

    // ---- Architecture / design ----
    if lower.contains("architect")
        || lower.contains("design pattern")
        || lower.contains("system design")
        || lower.contains("data model")
        || lower.contains("schema design")
        || lower.contains("api design")
        || lower.contains("trade-off")
        || lower.contains("tradeoff")
        || lower.contains("migration strategy")
        || lower.contains("scaling")
        || lower.contains("microservice")
        || lower.contains("monolith")
        || (lower.contains("design") && has_any(&words, &["system", "api", "database", "service"]))
    {
        return TaskType::Architecture;
    }

    // ---- Code writing ----
    if lower.contains("write")
        || lower.contains("create")
        || lower.contains("implement")
        || lower.contains("add a")
        || lower.contains("build")
        || lower.contains("fix the")
        || lower.contains("refactor")
        || lower.contains("function")
        || lower.contains("component")
        || lower.contains("endpoint")
        || lower.contains("handler")
        || lower.contains("struct")
        || lower.contains("module")
        || lower.contains("class")
        || lower.contains("add support for")
        || lower.contains("port to")
        || lower.contains("convert to")
        || lower.contains("migrate")
        || lower.contains("scaffold")
        || lower.contains("boilerplate")
        || lower.contains("impl ")
        || lower.contains("fn ")
    {
        return TaskType::CodeWriting;
    }

    // ---- Code review ----
    if lower.contains("review")
        || lower.contains("check the code")
        || lower.contains("analyze")
        || lower.contains("audit")
        || lower.contains("security")
        || lower.contains("code quality")
        || lower.contains("lint")
        || lower.contains("smell")
        || lower.contains("best practice")
        || lower.contains("pr review")
        || lower.contains("pull request")
        || lower.contains("diff")
        || lower.contains("vulnerability")
        || lower.contains("cve")
    {
        return TaskType::CodeReview;
    }

    // ---- Documentation ----
    if lower.contains("document")
        || lower.contains("readme")
        || lower.contains("docstring")
        || lower.contains("rustdoc")
        || lower.contains("jsdoc")
        || lower.contains("api doc")
        || lower.contains("write docs")
        || lower.contains("add comments")
        || lower.contains("changelog")
        || lower.contains("explain the code")
    {
        return TaskType::Documentation;
    }

    // ---- Research ----
    if lower.contains("research")
        || lower.contains("find out")
        || lower.contains("what is")
        || lower.contains("compare")
        || lower.contains("how does")
        || lower.contains("search for")
        || lower.contains("benchmark")
        || lower.contains("evaluate")
        || lower.contains("pros and cons")
        || lower.contains("alternative")
        || lower.contains("state of the art")
        || lower.contains("best library for")
    {
        return TaskType::Research;
    }

    // ---- Complex patterns ----
    if (lower.contains(" and ") && lower.contains(" then "))
        || lower
            .split(|c: char| c == '.' || c == ';' || c == ',')
            .count()
            > 3
        || lower.len() > 300
        || words.len() > 50
    {
        return TaskType::Complex;
    }

    TaskType::SimpleQuestion
}

/// Helper: check if any word from `needles` is present in `words`.
fn has_any(words: &[&str], needles: &[&str]) -> bool {
    needles.iter().any(|n| words.contains(n))
}

/// Select the best node(s) for a task type.
pub async fn select_nodes(task_type: TaskType) -> Vec<NodeCapability> {
    let fleet = fleet_capabilities().await;

    match task_type {
        TaskType::SimpleQuestion | TaskType::Documentation => {
            // Fastest model available
            fleet
                .into_iter()
                .filter(|n| n.strengths.contains(&Strength::FastResponse))
                .take(1)
                .collect()
        }
        TaskType::CodeWriting | TaskType::Testing => {
            // Best coding model
            fleet
                .into_iter()
                .filter(|n| n.strengths.contains(&Strength::Coding))
                .take(1)
                .collect()
        }
        TaskType::CodeReview => {
            // Largest model for thorough review
            let mut nodes: Vec<_> = fleet
                .into_iter()
                .filter(|n| {
                    n.strengths.contains(&Strength::Review)
                        || n.strengths.contains(&Strength::Reasoning)
                })
                .collect();
            nodes.sort_by(|a, b| b.model_params.cmp(&a.model_params));
            nodes.into_iter().take(1).collect()
        }
        TaskType::FleetOp => {
            // Leader node (highest election priority) handles fleet ops.
            // Pull the DB snapshot to find which node name is the leader, then
            // return the matching capability.
            let leader_name = crate::fleet_info::fetch_nodes()
                .await
                .ok()
                .and_then(|mut rows| {
                    rows.sort_by_key(|r| r.election_priority);
                    rows.into_iter().next().map(|r| r.name)
                });
            if let Some(lname) = leader_name {
                fleet
                    .into_iter()
                    .filter(|n| n.name == lname || n.name.starts_with(&format!("{lname}-")))
                    .take(1)
                    .collect()
            } else {
                fleet.into_iter().take(1).collect()
            }
        }
        TaskType::Research | TaskType::Architecture => {
            // Best reasoning model
            fleet
                .into_iter()
                .filter(|n| n.strengths.contains(&Strength::Reasoning))
                .take(1)
                .collect()
        }
        TaskType::Debugging => {
            // Debugging benefits from larger models — prefer reasoning + coding
            let mut nodes: Vec<_> = fleet
                .into_iter()
                .filter(|n| {
                    n.strengths.contains(&Strength::Coding)
                        || n.strengths.contains(&Strength::Reasoning)
                })
                .collect();
            nodes.sort_by(|a, b| b.model_params.cmp(&a.model_params));
            nodes.into_iter().take(1).collect()
        }
        TaskType::Complex => {
            // Break into subtasks — orchestrator picks
            fleet
                .into_iter()
                .filter(|n| n.strengths.contains(&Strength::Reasoning))
                .take(1)
                .collect()
        }
        TaskType::MultiNode => {
            // All coding nodes for parallel work
            fleet
                .into_iter()
                .filter(|n| n.strengths.contains(&Strength::Coding))
                .collect()
        }
    }
}

// ---------------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------------

/// Result of an orchestrated multi-node execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratedResult {
    pub task_type: TaskType,
    pub nodes_used: Vec<String>,
    pub results: Vec<NodeResult>,
    pub synthesized_response: String,
    pub total_duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeResult {
    pub node: String,
    pub model: String,
    pub output: String,
    pub success: bool,
    pub duration_ms: u64,
}

/// Run a task through the orchestrator.
/// Analyzes the task, selects nodes, dispatches, collects results.
pub async fn orchestrate(prompt: &str, working_dir: &std::path::Path) -> OrchestratedResult {
    let start = std::time::Instant::now();
    let task_type = analyze_task(prompt);
    let nodes = select_nodes(task_type).await;

    info!(
        task_type = ?task_type,
        nodes = nodes.len(),
        "orchestrator dispatching task"
    );

    if nodes.is_empty() {
        return OrchestratedResult {
            task_type,
            nodes_used: vec![],
            results: vec![],
            synthesized_response: "No available nodes for this task type.".into(),
            total_duration_ms: start.elapsed().as_millis() as u64,
        };
    }

    match task_type {
        TaskType::MultiNode => {
            // Parallel execution across all selected nodes
            run_parallel(prompt, &nodes, working_dir).await
        }
        _ => {
            // Single node execution on the best node
            let node = &nodes[0];
            let result = run_on_node(prompt, node, working_dir).await;
            OrchestratedResult {
                task_type,
                nodes_used: vec![node.name.clone()],
                results: vec![result],
                synthesized_response: String::new(), // filled after
                total_duration_ms: start.elapsed().as_millis() as u64,
            }
        }
    }
}

/// Run a prompt on a specific fleet node.
async fn run_on_node(
    prompt: &str,
    node: &NodeCapability,
    working_dir: &std::path::Path,
) -> NodeResult {
    let start = std::time::Instant::now();
    let llm_url = format!("http://{}:{}", node.ip, node.llm_port);

    let config = AgentSessionConfig {
        model: node.model_name.clone(),
        llm_base_url: llm_url,
        working_dir: working_dir.to_path_buf(),
        system_prompt: None,
        max_turns: 10,
        auto_save: false,
        ..Default::default()
    };

    let mut session = AgentSession::new(config);
    let outcome = session.run(prompt, None).await;

    let (success, output) = match outcome {
        AgentOutcome::EndTurn { final_message } => (true, final_message),
        AgentOutcome::MaxTurns { partial_message } => (true, partial_message),
        AgentOutcome::Error(e) => (false, e),
        AgentOutcome::Cancelled => (false, "Cancelled".into()),
    };

    NodeResult {
        node: node.name.clone(),
        model: node.model_name.clone(),
        output,
        success,
        duration_ms: start.elapsed().as_millis() as u64,
    }
}

/// Run the same prompt on multiple nodes in parallel.
async fn run_parallel(
    prompt: &str,
    nodes: &[NodeCapability],
    working_dir: &std::path::Path,
) -> OrchestratedResult {
    let start = std::time::Instant::now();
    let mut handles = Vec::new();

    for node in nodes {
        let prompt = prompt.to_string();
        let node = node.clone();
        let wd = working_dir.to_path_buf();

        handles.push(tokio::spawn(async move {
            run_on_node(&prompt, &node, &wd).await
        }));
    }

    let mut results = Vec::new();
    let mut nodes_used = Vec::new();

    for handle in handles {
        match handle.await {
            Ok(result) => {
                nodes_used.push(result.node.clone());
                results.push(result);
            }
            Err(e) => {
                results.push(NodeResult {
                    node: "unknown".into(),
                    model: "unknown".into(),
                    output: format!("Task panicked: {e}"),
                    success: false,
                    duration_ms: 0,
                });
            }
        }
    }

    // Synthesize results
    let successful: Vec<&NodeResult> = results.iter().filter(|r| r.success).collect();
    let synthesis = if successful.is_empty() {
        "All nodes failed.".into()
    } else {
        successful
            .iter()
            .map(|r| {
                format!(
                    "**{}** ({}): {}",
                    r.node,
                    r.model,
                    &r.output[..r.output.len().min(500)]
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };

    OrchestratedResult {
        task_type: TaskType::MultiNode,
        nodes_used,
        results,
        synthesized_response: synthesis,
        total_duration_ms: start.elapsed().as_millis() as u64,
    }
}

// ---------------------------------------------------------------------------
// Training data collection (for future LoRA fine-tuning)
// ---------------------------------------------------------------------------

/// A training example for tool-calling fine-tuning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainingExample {
    pub prompt: String,
    pub task_type: TaskType,
    pub tool_calls: Vec<ToolCallExample>,
    pub success: bool,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallExample {
    pub tool_name: String,
    pub arguments: String,
    pub result_preview: String,
    pub was_correct: bool,
}

/// Save a training example for future fine-tuning.
pub async fn save_training_example(example: &TrainingExample) {
    let dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".forgefleet")
        .join("training_data");
    let _ = tokio::fs::create_dir_all(&dir).await;

    let file = dir.join(format!(
        "{}.json",
        chrono::Utc::now().format("%Y%m%d_%H%M%S")
    ));
    if let Ok(json) = serde_json::to_string_pretty(example) {
        let _ = tokio::fs::write(&file, json).await;
    }
}

/// Count available training examples.
pub async fn training_data_count() -> usize {
    let dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".forgefleet")
        .join("training_data");
    let mut count = 0;
    if let Ok(mut entries) = tokio::fs::read_dir(&dir).await {
        while let Ok(Some(_)) = entries.next_entry().await {
            count += 1;
        }
    }
    count
}
