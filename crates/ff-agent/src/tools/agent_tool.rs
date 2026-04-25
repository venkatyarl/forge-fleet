//! AgentTool — spawn sub-agents on fleet nodes for parallel work.
//!
//! Dispatches sub-agents to different fleet LLMs based on the task,
//! enabling distributed parallel execution across ForgeFleet nodes.

use async_trait::async_trait;
use serde_json::{Value, json};
use tracing::info;

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};
use crate::agent_loop::{AgentOutcome, AgentSession, AgentSessionConfig};

pub struct SubAgentTool;

#[async_trait]
impl AgentTool for SubAgentTool {
    fn name(&self) -> &str {
        "Agent"
    }

    fn description(&self) -> &str {
        "Launch a sub-agent to handle a complex task autonomously. The sub-agent gets its own conversation with the LLM and can use all available tools. Use this for tasks that require multiple steps or when you want to parallelize work. Specify a fleet LLM endpoint to run the sub-agent on a specific node."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The task for the sub-agent to accomplish. Be detailed — the sub-agent has no context from this conversation."
                },
                "description": {
                    "type": "string",
                    "description": "Short 3-5 word description of what the agent will do"
                },
                "model": {
                    "type": "string",
                    "description": "Optional model name override for the sub-agent"
                },
                "llm_base_url": {
                    "type": "string",
                    "description": "Optional fleet LLM endpoint URL (e.g. http://192.168.5.102:55000). Defaults to same as parent."
                },
                "working_dir": {
                    "type": "string",
                    "description": "Optional working directory for the sub-agent"
                },
                "max_turns": {
                    "type": "number",
                    "description": "Maximum turns for the sub-agent (default 10)"
                },
                "parent_task_id": {
                    "type": "string",
                    "description": "Task ID of the spawning task"
                },
                "origin_node": {
                    "type": "string",
                    "description": "Node that originated this work"
                }
            },
            "required": ["prompt"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let prompt = match input.get("prompt").and_then(Value::as_str) {
            Some(p) if !p.trim().is_empty() => p.to_string(),
            _ => return AgentToolResult::err("Missing or empty 'prompt' parameter"),
        };

        // Inject provenance context into the prompt if provided.
        let parent_task_id = input
            .get("parent_task_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let origin_node = input
            .get("origin_node")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let prompt = if !parent_task_id.is_empty() || !origin_node.is_empty() {
            format!(
                "[PROVENANCE: origin={}, parent_task={}]\n{}",
                if origin_node.is_empty() {
                    "unknown"
                } else {
                    &origin_node
                },
                if parent_task_id.is_empty() {
                    "none"
                } else {
                    &parent_task_id
                },
                prompt
            )
        } else {
            prompt
        };

        let description = input
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("sub-agent task");

        let max_turns = input.get("max_turns").and_then(Value::as_u64).unwrap_or(10) as u32;

        // Inherit parent's LLM config or use overrides.
        // Fall back to localhost so the InferenceRouter handles actual routing.
        let llm_base_url = input
            .get("llm_base_url")
            .and_then(Value::as_str)
            .unwrap_or("http://localhost:55000")
            .to_string();

        let model = input
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let working_dir = input
            .get("working_dir")
            .and_then(Value::as_str)
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| ctx.working_dir.clone());

        info!(
            description = description,
            llm = %llm_base_url,
            "spawning sub-agent"
        );

        let config = AgentSessionConfig {
            model: if model.is_empty() {
                "auto".into()
            } else {
                model
            },
            llm_base_url,
            working_dir,
            system_prompt: None,
            max_turns,
            temperature: 0.3,
            max_tokens: 4096,
            auto_save: false, // sub-agents don't auto-save
            ..Default::default()
        };

        let mut session = AgentSession::new(config);
        let outcome = session.run(&prompt, None).await;

        match outcome {
            AgentOutcome::EndTurn { final_message } => {
                AgentToolResult::ok(truncate_output(&final_message, MAX_TOOL_RESULT_CHARS))
            }
            AgentOutcome::MaxTurns { partial_message } => {
                let msg = format!(
                    "Sub-agent hit max turn limit ({max_turns} turns). Partial result:\n{}",
                    partial_message
                );
                AgentToolResult::ok(truncate_output(&msg, MAX_TOOL_RESULT_CHARS))
            }
            AgentOutcome::Cancelled => AgentToolResult::err("Sub-agent was cancelled"),
            AgentOutcome::Error(e) => AgentToolResult::err(format!("Sub-agent error: {e}")),
        }
    }
}
