//! Planning tools — AskUserQuestion, EnterPlanMode, ExitPlanMode.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{AgentTool, AgentToolContext, AgentToolResult};

// ---------------------------------------------------------------------------
// AskUserQuestion
// ---------------------------------------------------------------------------

pub struct AskUserQuestionTool;

#[async_trait]
impl AgentTool for AskUserQuestionTool {
    fn name(&self) -> &str { "AskUserQuestion" }

    fn description(&self) -> &str {
        "Ask the user a question and wait for their response. Use this when you need clarification, a decision, or approval before proceeding."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to ask the user"
                }
            },
            "required": ["question"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let question = input.get("question").and_then(Value::as_str).unwrap_or("");
        if question.is_empty() {
            return AgentToolResult::err("Missing 'question' parameter");
        }

        // Terminate the current turn. The agent loop will stop here; the NEXT
        // user message will be treated as the answer. Previously this tool
        // told the LLM "user is unavailable, proceed with best judgment"
        // which caused endless rationalization loops — the agent would keep
        // calling tools (Grep, Glob, Read) fabricating its own context
        // instead of waiting for the user's actual answer.
        AgentToolResult::ok(format!(
            "Question posed to user: \"{question}\"\n\n\
             STOPPING. The agent loop will now exit this turn. The next \
             user message will be the answer — do NOT plan any further \
             tool calls, and do NOT rationalize an answer yourself."
        ))
        .end_turn()
    }
}

// ---------------------------------------------------------------------------
// EnterPlanMode
// ---------------------------------------------------------------------------

pub struct EnterPlanModeTool;

#[async_trait]
impl AgentTool for EnterPlanModeTool {
    fn name(&self) -> &str { "EnterPlanMode" }

    fn description(&self) -> &str {
        "Enter plan mode for designing an implementation approach before writing code. In plan mode, focus on reading code and designing a plan, not making changes."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        // Plan mode is a state flag in the agent session.
        // For now, return an acknowledgment. The agent loop can check this
        // and restrict to read-only tools.
        AgentToolResult::ok("Entered plan mode. Focus on reading and exploring the codebase to design your approach. Do not make edits until you exit plan mode.")
    }
}

// ---------------------------------------------------------------------------
// ExitPlanMode
// ---------------------------------------------------------------------------

pub struct ExitPlanModeTool;

#[async_trait]
impl AgentTool for ExitPlanModeTool {
    fn name(&self) -> &str { "ExitPlanMode" }

    fn description(&self) -> &str {
        "Exit plan mode after designing your approach. You can now make edits and implement your plan."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        AgentToolResult::ok("Exited plan mode. You can now make edits and implement changes.")
    }
}
