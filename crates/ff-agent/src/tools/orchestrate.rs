//! Orchestrate tool — smart dispatcher that routes work to the best fleet node.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};
use crate::orchestrator_agent;

pub struct OrchestrateTool;

#[async_trait]
impl AgentTool for OrchestrateTool {
    fn name(&self) -> &str { "Orchestrate" }

    fn description(&self) -> &str {
        "Route a complex task to the best fleet node(s) for execution. Analyzes the task type (coding, review, research, fleet ops, multi-node) and dispatches to the most capable model. For multi-node tasks, runs in parallel across the fleet."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": {"type": "string", "description": "The task to dispatch"},
                "force_parallel": {"type": "boolean", "description": "Force parallel execution across all coding nodes"},
                "target_node": {"type": "string", "description": "Specific node to run on (optional)"}
            },
            "required": ["task"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let task = match input.get("task").and_then(Value::as_str) {
            Some(t) if !t.is_empty() => t,
            _ => return AgentToolResult::err("Missing 'task'"),
        };

        let force_parallel = input.get("force_parallel").and_then(Value::as_bool).unwrap_or(false);

        // If specific node requested, route there
        if let Some(target) = input.get("target_node").and_then(Value::as_str) {
            let fleet = orchestrator_agent::fleet_capabilities();
            if let Some(node) = fleet.iter().find(|n| n.name == target || n.ip == target) {
                let config = crate::agent_loop::AgentSessionConfig {
                    model: node.model_name.clone(),
                    llm_base_url: format!("http://{}:{}", node.ip, node.llm_port),
                    working_dir: ctx.working_dir.clone(),
                    max_turns: 10,
                    auto_save: false,
                    ..Default::default()
                };
                let mut session = crate::agent_loop::AgentSession::new(config);
                let outcome = session.run(task, None).await;
                return match outcome {
                    crate::agent_loop::AgentOutcome::EndTurn { final_message } => {
                        AgentToolResult::ok(format!("[{}] {}", node.name, truncate_output(&final_message, MAX_TOOL_RESULT_CHARS - 50)))
                    }
                    crate::agent_loop::AgentOutcome::Error(e) => AgentToolResult::err(format!("[{}] Error: {e}", node.name)),
                    _ => AgentToolResult::ok(format!("[{}] Task completed", node.name)),
                };
            } else {
                return AgentToolResult::err(format!("Node '{target}' not found in fleet"));
            }
        }

        // Analyze and orchestrate
        let task_type = orchestrator_agent::analyze_task(task);
        let task_type_override = if force_parallel { orchestrator_agent::TaskType::MultiNode } else { task_type };

        let result = orchestrator_agent::orchestrate(task, &ctx.working_dir).await;

        // Collect training data
        let example = orchestrator_agent::TrainingExample {
            prompt: task.to_string(),
            task_type: task_type_override,
            tool_calls: vec![],
            success: result.results.iter().any(|r| r.success),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        orchestrator_agent::save_training_example(&example).await;

        let mut output = format!(
            "Orchestrated: {:?} → {} node(s) ({:.1}s)\n\n",
            result.task_type,
            result.nodes_used.len(),
            result.total_duration_ms as f64 / 1000.0
        );

        for r in &result.results {
            let icon = if r.success { "✓" } else { "✗" };
            output.push_str(&format!(
                "{icon} {} ({}, {:.1}s):\n{}\n\n",
                r.node, r.model, r.duration_ms as f64 / 1000.0,
                truncate_output(&r.output, 2000)
            ));
        }

        if result.results.iter().any(|r| r.success) {
            AgentToolResult::ok(truncate_output(&output, MAX_TOOL_RESULT_CHARS))
        } else {
            AgentToolResult::err(truncate_output(&output, MAX_TOOL_RESULT_CHARS))
        }
    }
}
