//! Agent loop — the core think→tool→observe cycle for LLM-driven task execution.
//!
//! ForgeFleet's agent brain — communicates with local fleet LLMs via
//! OpenAI-compatible endpoints. Features:
//!
//! - Think→Tool→Observe cycle with parallel tool execution
//! - Auto-compaction when context window exceeds threshold
//! - Tool-result budgeting to prevent context overflow
//! - Token/usage tracking per session
//! - Session save/resume to disk
//! - Cancellation support

use std::path::PathBuf;

use ff_api::tool_calling::{ToolChatCompletionRequest, ToolChatCompletionResponse, ToolChatMessage};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::compaction::{
    CompactionConfig, TokenUsage, apply_tool_result_budget, compact_messages, should_compact,
};
use crate::focus_stack::ConversationTracker;
use crate::openai_bridge;
use crate::scoped_memory::{MemoryScope, ScopedMemoryStore};
use crate::session_store;
use std::sync::Arc;

use crate::tools::{self, AgentTool, AgentToolContext};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Configuration for creating an agent session.
#[derive(Debug, Clone)]
pub struct AgentSessionConfig {
    /// LLM model name (e.g. "qwen2.5-coder-32b").
    pub model: String,
    /// Base URL of the OpenAI-compatible LLM endpoint.
    pub llm_base_url: String,
    /// Working directory for tool execution.
    pub working_dir: PathBuf,
    /// Optional system prompt override.
    pub system_prompt: Option<String>,
    /// Maximum number of turns (default 30).
    pub max_turns: u32,
    /// Temperature for LLM sampling (default 0.3).
    pub temperature: f32,
    /// Max tokens per LLM response (default 4096).
    pub max_tokens: u32,
    /// Context window size in tokens (default 32768).
    pub context_window_tokens: usize,
    /// Max total chars for tool results before budgeting kicks in (default 50000).
    pub tool_result_budget_chars: usize,
    /// Whether to auto-save sessions to disk (default true).
    pub auto_save: bool,
    /// Memory scope for this session (default: Global).
    pub memory_scope: Option<MemoryScope>,
}

impl Default for AgentSessionConfig {
    fn default() -> Self {
        Self {
            model: "qwen2.5-coder-32b".into(),
            llm_base_url: "http://localhost:51000".into(),
            working_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            system_prompt: None,
            max_turns: 30,
            temperature: 0.3,
            max_tokens: 4096,
            context_window_tokens: 32_768,
            tool_result_budget_chars: 50_000,
            auto_save: true,
            memory_scope: None,
        }
    }
}

/// A running agent session.
pub struct AgentSession {
    pub id: Uuid,
    pub config: AgentSessionConfig,
    pub messages: Vec<ToolChatMessage>,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub cancel_token: CancellationToken,
    pub usage: TokenUsage,
    pub tracker: ConversationTracker,
    tool_ctx: AgentToolContext,
    compaction_config: CompactionConfig,
    pub turn_count: u32,
}

impl AgentSession {
    /// Create a new agent session with default tools.
    pub fn new(config: AgentSessionConfig) -> Self {
        let session_id = Uuid::new_v4();
        let tool_ctx = AgentToolContext {
            working_dir: config.working_dir.clone(),
            session_id: session_id.to_string(),
            shell_state: tools::session_shell_state(&session_id.to_string()),
        };

        let system_prompt = config
            .system_prompt
            .clone()
            .unwrap_or_else(|| default_system_prompt(&config.working_dir));

        // Note: scoped memory is loaded asynchronously in run() on first call.
        // ConversationTracker context is injected per-turn in the loop.

        let messages = vec![ToolChatMessage::system(system_prompt)];

        let compaction_config = CompactionConfig {
            context_window_tokens: config.context_window_tokens,
            ..Default::default()
        };

        Self {
            id: session_id,
            config,
            messages,
            tools: tools::all_tools_arc(),
            cancel_token: CancellationToken::new(),
            usage: TokenUsage::default(),
            tracker: ConversationTracker::new(),
            tool_ctx,
            compaction_config,
            turn_count: 0,
        }
    }

    /// Restore a session from persisted state.
    pub fn from_persisted(
        session_id: Uuid,
        config: AgentSessionConfig,
        messages: Vec<ToolChatMessage>,
        turn_count: u32,
    ) -> Self {
        let tool_ctx = AgentToolContext {
            working_dir: config.working_dir.clone(),
            session_id: session_id.to_string(),
            shell_state: tools::session_shell_state(&session_id.to_string()),
        };

        let compaction_config = CompactionConfig {
            context_window_tokens: config.context_window_tokens,
            ..Default::default()
        };

        Self {
            id: session_id,
            config,
            messages,
            tools: tools::all_tools_arc(),
            cancel_token: CancellationToken::new(),
            usage: TokenUsage::default(),
            tracker: ConversationTracker::new(),
            tool_ctx,
            compaction_config,
            turn_count,
        }
    }

    /// Inject a user message and run the agent loop until completion.
    pub async fn run(
        &mut self,
        prompt: &str,
        event_tx: Option<mpsc::UnboundedSender<AgentEvent>>,
    ) -> AgentOutcome {
        // Load scoped memory and inject into system prompt (first turn only)
        if self.turn_count == 0 {
            if let Some(scope) = &self.config.memory_scope {
                let store = ScopedMemoryStore::open(scope.clone()).await;
                if !store.is_empty() {
                    let memory_context = store.build_context(4000);
                    // Append memory context to system message
                    if let Some(sys_msg) = self.messages.first_mut() {
                        if let Some(current) = sys_msg.text_content().map(String::from) {
                            *sys_msg = ToolChatMessage::system(format!(
                                "{current}\n\n## Project Memory ({} entries)\n\n{memory_context}",
                                store.len()
                            ));
                        }
                    }
                }
            }
        }

        // Inject Focus Stack + Backlog context as a system reminder
        let tracker_ctx = self.tracker.context_injection();
        if !tracker_ctx.is_empty() {
            // Add as a user message that provides context (not a system message, since
            // some models only support one system message)
            self.messages.push(ToolChatMessage::user(format!(
                "[System Context]\n{tracker_ctx}\nNow, please address: {prompt}"
            )));
        } else {
            self.messages.push(ToolChatMessage::user(prompt));
        }

        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(600))
            .build()
            .unwrap_or_default();

        let outcome = run_agent_loop(self, &http_client, event_tx).await;

        // Auto-save session
        if self.config.auto_save {
            if let Err(e) = session_store::save_session(
                &self.id.to_string(),
                &self.config.model,
                &self.config.llm_base_url,
                &self.config.working_dir.to_string_lossy(),
                &self.messages,
                self.turn_count,
            )
            .await
            {
                warn!(error = %e, "failed to auto-save session");
            }
        }

        outcome
    }

    /// Inject a follow-up user message (for multi-turn conversations).
    pub fn add_user_message(&mut self, content: &str) {
        self.messages.push(ToolChatMessage::user(content));
    }
}

impl Drop for AgentSession {
    fn drop(&mut self) {
        tools::clear_session_shell_state(&self.id.to_string());
    }
}

/// Events emitted during agent execution, streamed to UI/CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum AgentEvent {
    #[serde(rename = "tool_start")]
    ToolStart {
        session_id: String,
        tool_name: String,
        tool_id: String,
        input_json: String,
    },
    #[serde(rename = "tool_end")]
    ToolEnd {
        session_id: String,
        tool_name: String,
        tool_id: String,
        result: String,
        is_error: bool,
        duration_ms: u64,
    },
    #[serde(rename = "assistant_text")]
    AssistantText { session_id: String, text: String },
    #[serde(rename = "turn_complete")]
    TurnComplete {
        session_id: String,
        turn: u32,
        finish_reason: String,
    },
    #[serde(rename = "status")]
    Status { session_id: String, message: String },
    #[serde(rename = "error")]
    Error { session_id: String, message: String },
    #[serde(rename = "done")]
    Done {
        session_id: String,
        final_text: String,
    },
    #[serde(rename = "compaction")]
    Compaction {
        session_id: String,
        messages_before: usize,
        messages_after: usize,
    },
    #[serde(rename = "token_warning")]
    TokenWarning {
        session_id: String,
        usage_pct: f64,
        estimated_tokens: usize,
    },
}

/// Outcome of the agent loop.
#[derive(Debug)]
pub enum AgentOutcome {
    /// The agent completed normally with a final message.
    EndTurn { final_message: String },
    /// The agent hit the max turn limit.
    MaxTurns { partial_message: String },
    /// The agent was cancelled.
    Cancelled,
    /// An error occurred.
    Error(String),
}

// ---------------------------------------------------------------------------
// Core loop
// ---------------------------------------------------------------------------

async fn run_agent_loop(
    session: &mut AgentSession,
    http_client: &reqwest::Client,
    event_tx: Option<mpsc::UnboundedSender<AgentEvent>>,
) -> AgentOutcome {
    let session_id = session.id.to_string();
    let openai_tools = openai_bridge::tools_to_openai_arc(&session.tools);

    for turn in 1..=session.config.max_turns {
        session.turn_count = turn;

        // Check cancellation
        if session.cancel_token.is_cancelled() {
            return AgentOutcome::Cancelled;
        }

        emit(
            &event_tx,
            AgentEvent::Status {
                session_id: session_id.clone(),
                message: format!("Turn {turn}/{}", session.config.max_turns),
            },
        );

        // --- Auto-compaction check ---
        if should_compact(&session.messages, &session.compaction_config) {
            let before = session.messages.len();
            session.messages = compact_messages(&session.messages, &session.compaction_config);
            let after = session.messages.len();
            session.usage.compaction_count += 1;

            info!(
                session = %session_id,
                before,
                after,
                compaction = session.usage.compaction_count,
                "auto-compacted conversation history"
            );

            emit(
                &event_tx,
                AgentEvent::Compaction {
                    session_id: session_id.clone(),
                    messages_before: before,
                    messages_after: after,
                },
            );
        }

        // --- Tool-result budgeting ---
        let truncated = apply_tool_result_budget(
            &mut session.messages,
            session.config.tool_result_budget_chars,
        );
        if truncated > 0 {
            debug!(truncated, "applied tool-result budget");
        }

        // --- Build request ---
        let request = ToolChatCompletionRequest {
            model: session.config.model.clone(),
            messages: session.messages.clone(),
            tools: if openai_tools.is_empty() {
                None
            } else {
                Some(openai_tools.clone())
            },
            tool_choice: Some(serde_json::json!("auto")),
            temperature: Some(session.config.temperature),
            max_tokens: Some(session.config.max_tokens),
            stream: Some(false),
        };

        // --- Send to LLM ---
        let url = format!(
            "{}/v1/chat/completions",
            session.config.llm_base_url.trim_end_matches('/')
        );

        debug!(turn, url = %url, model = %session.config.model, "sending agent request");

        let response = tokio::select! {
            _ = session.cancel_token.cancelled() => {
                return AgentOutcome::Cancelled;
            }
            result = send_request(http_client, &url, &request) => result
        };

        let response = match response {
            Ok(resp) => resp,
            Err(err) => {
                let msg = format!("LLM request failed: {err}");
                emit(
                    &event_tx,
                    AgentEvent::Error {
                        session_id: session_id.clone(),
                        message: msg.clone(),
                    },
                );
                return AgentOutcome::Error(msg);
            }
        };

        // --- Track token usage ---
        if let Some(usage) = &response.usage {
            session
                .usage
                .record_turn(usage.prompt_tokens as u64, usage.completion_tokens as u64);

            // Token warning check
            let estimated = crate::compaction::estimate_message_tokens(&session.messages);
            let pct = estimated as f64 / session.config.context_window_tokens as f64;
            if pct > 0.70 {
                emit(
                    &event_tx,
                    AgentEvent::TokenWarning {
                        session_id: session_id.clone(),
                        usage_pct: pct * 100.0,
                        estimated_tokens: estimated,
                    },
                );
            }
        }

        // --- Parse response ---
        let choice = match response.choices.first() {
            Some(c) => c,
            None => {
                let msg = "LLM returned empty choices".to_string();
                emit(
                    &event_tx,
                    AgentEvent::Error {
                        session_id: session_id.clone(),
                        message: msg.clone(),
                    },
                );
                return AgentOutcome::Error(msg);
            }
        };

        let finish_reason = choice
            .finish_reason
            .as_deref()
            .unwrap_or("stop")
            .to_string();

        let assistant_msg = match &choice.message {
            Some(msg) => msg.clone(),
            None => {
                let msg = "LLM response missing message".to_string();
                emit(
                    &event_tx,
                    AgentEvent::Error {
                        session_id: session_id.clone(),
                        message: msg.clone(),
                    },
                );
                return AgentOutcome::Error(msg);
            }
        };

        // Append assistant message to history
        session.messages.push(assistant_msg.clone());

        // Check for tool calls — message itself is primary signal
        let mut tool_calls = openai_bridge::extract_tool_calls(&assistant_msg);

        // Text-mode fallback parsing
        if tool_calls.is_empty() {
            if let Some(text) = assistant_msg.text_content() {
                let text_calls = openai_bridge::parse_text_tool_calls(text);
                if !text_calls.is_empty() {
                    debug!(count = text_calls.len(), "parsed tool calls from text fallback");
                    tool_calls = text_calls;
                }
            }
        }

        if tool_calls.is_empty() {
            // No tool calls — agent is done
            let final_text = assistant_msg.text_content().unwrap_or("").to_string();

            emit(
                &event_tx,
                AgentEvent::AssistantText {
                    session_id: session_id.clone(),
                    text: final_text.clone(),
                },
            );

            emit(
                &event_tx,
                AgentEvent::TurnComplete {
                    session_id: session_id.clone(),
                    turn,
                    finish_reason,
                },
            );

            emit(
                &event_tx,
                AgentEvent::Done {
                    session_id: session_id.clone(),
                    final_text: final_text.clone(),
                },
            );

            info!(session = %session_id, turn, "agent loop completed");
            return AgentOutcome::EndTurn {
                final_message: final_text,
            };
        }

        // --- Execute tool calls ---
        info!(
            session = %session_id,
            turn,
            tool_count = tool_calls.len(),
            "executing tool calls"
        );

        // Emit text content before tool calls if present — but NOT if the
        // text was actually a raw JSON tool call that we parsed via fallback
        // (that would leak the JSON to the user as "assistant text").
        if let Some(text) = assistant_msg.text_content() {
            let text = text.trim();
            let is_raw_tool_json = text.starts_with('{') && text.contains("\"name\"");
            if !text.is_empty() && !is_raw_tool_json {
                emit(
                    &event_tx,
                    AgentEvent::AssistantText {
                        session_id: session_id.clone(),
                        text: text.to_string(),
                    },
                );
            }
        }

        // Execute tools — parallel when multiple tool calls
        let tool_ctx_arc = Arc::new(session.tool_ctx.clone());
        let mut tool_results = Vec::new();

        if tool_calls.len() > 1 {
            // Parallel execution using futures::join_all
            let mut futures = Vec::new();

            for tc in &tool_calls {
                let tool_name = tc.function.name.clone();
                let tool_id = tc.id.clone();
                let args_str = tc.function.arguments.clone();
                let ctx = tool_ctx_arc.clone();
                let tools_clone = session.tools.clone();
                let event_tx_clone = event_tx.clone();
                let sid = session_id.clone();

                futures.push(tokio::spawn(async move {
                    execute_single_tool(
                        &tool_name, &tool_id, &args_str,
                        &tools_clone, &ctx, &event_tx_clone, &sid,
                    ).await
                }));
            }

            let results = futures::future::join_all(futures).await;
            for result in results {
                match result {
                    Ok((tool_id, content, _is_error)) => {
                        tool_results.push((tool_id, content));
                    }
                    Err(e) => {
                        tool_results.push(("error".into(), format!("Tool execution failed: {e}")));
                    }
                }
            }

            // Append all tool result messages
            for (tool_id, content) in &tool_results {
                session.messages.push(ToolChatMessage::tool_result(tool_id, content));
            }
        } else {
            // Single tool — execute directly (avoids spawn overhead)
            for tc in &tool_calls {
            let tool_name = tc.function.name.clone();
            let tool_id = tc.id.clone();
            let args_str = tc.function.arguments.clone();

            emit(
                &event_tx,
                AgentEvent::ToolStart {
                    session_id: session_id.clone(),
                    tool_name: tool_name.clone(),
                    tool_id: tool_id.clone(),
                    input_json: args_str.clone(),
                },
            );

            // Parse arguments
            let args: serde_json::Value = match serde_json::from_str(&args_str) {
                Ok(v) => v,
                Err(e) => match try_fix_json(&args_str) {
                    Some(v) => v,
                    None => {
                        let err_msg =
                            format!("Failed to parse tool arguments: {e}\nRaw: {args_str}");
                        emit(
                            &event_tx,
                            AgentEvent::ToolEnd {
                                session_id: session_id.clone(),
                                tool_name: tool_name.clone(),
                                tool_id: tool_id.clone(),
                                result: err_msg.clone(),
                                is_error: true,
                                duration_ms: 0,
                            },
                        );
                        session
                            .messages
                            .push(ToolChatMessage::tool_result(&tool_id, &err_msg));
                        continue;
                    }
                },
            };

            // Find and execute tool
            if let Some(idx) = tools::find_tool_arc(&tool_name, &session.tools) {
                let start = std::time::Instant::now();
                let result = session.tools[idx].execute(args, &session.tool_ctx).await;
                let duration_ms = start.elapsed().as_millis() as u64;

                let result_content =
                    tools::truncate_output(&result.content, tools::MAX_TOOL_RESULT_CHARS);

                emit(
                    &event_tx,
                    AgentEvent::ToolEnd {
                        session_id: session_id.clone(),
                        tool_name: tool_name.clone(),
                        tool_id: tool_id.clone(),
                        result: result_content.clone(),
                        is_error: result.is_error,
                        duration_ms,
                    },
                );

                session
                    .messages
                    .push(ToolChatMessage::tool_result(&tool_id, &result_content));
            } else {
                let err_msg = format!(
                    "Unknown tool: {tool_name}. Available tools: {}",
                    session
                        .tools
                        .iter()
                        .map(|t| t.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                );

                emit(
                    &event_tx,
                    AgentEvent::ToolEnd {
                        session_id: session_id.clone(),
                        tool_name: tool_name.clone(),
                        tool_id: tool_id.clone(),
                        result: err_msg.clone(),
                        is_error: true,
                        duration_ms: 0,
                    },
                );

                session
                    .messages
                    .push(ToolChatMessage::tool_result(&tool_id, &err_msg));
            }
        }
        } // close else (single tool)

        emit(
            &event_tx,
            AgentEvent::TurnComplete {
                session_id: session_id.clone(),
                turn,
                finish_reason,
            },
        );
    }

    // Hit max turns
    let partial = session
        .messages
        .last()
        .and_then(|m| m.text_content())
        .unwrap_or("")
        .to_string();

    warn!(
        session = %session_id,
        max_turns = session.config.max_turns,
        "agent hit max turn limit"
    );

    AgentOutcome::MaxTurns {
        partial_message: partial,
    }
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

async fn send_request(
    client: &reqwest::Client,
    url: &str,
    request: &ToolChatCompletionRequest,
) -> anyhow::Result<ToolChatCompletionResponse> {
    let resp = client.post(url).json(request).send().await?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("LLM returned HTTP {status}: {body}");
    }

    let body = resp.text().await?;
    debug!(body_len = body.len(), "received LLM response");

    let parsed: ToolChatCompletionResponse = serde_json::from_str(&body).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse LLM response: {e}\nBody: {}",
            truncate_for_error(&body)
        )
    })?;

    Ok(parsed)
}

/// Execute a single tool call (used by parallel executor).
async fn execute_single_tool(
    tool_name: &str,
    tool_id: &str,
    args_str: &str,
    tools_list: &[Arc<dyn AgentTool>],
    ctx: &AgentToolContext,
    event_tx: &Option<mpsc::UnboundedSender<AgentEvent>>,
    session_id: &str,
) -> (String, String, bool) {
    emit(event_tx, AgentEvent::ToolStart {
        session_id: session_id.to_string(),
        tool_name: tool_name.to_string(),
        tool_id: tool_id.to_string(),
        input_json: args_str.to_string(),
    });

    let args: serde_json::Value = match serde_json::from_str(args_str) {
        Ok(v) => v,
        Err(e) => match try_fix_json(args_str) {
            Some(v) => v,
            None => {
                let err = format!("Failed to parse tool arguments: {e}");
                emit(event_tx, AgentEvent::ToolEnd {
                    session_id: session_id.to_string(),
                    tool_name: tool_name.to_string(),
                    tool_id: tool_id.to_string(),
                    result: err.clone(), is_error: true, duration_ms: 0,
                });
                return (tool_id.to_string(), err, true);
            }
        },
    };

    if let Some(idx) = tools::find_tool_arc(tool_name, tools_list) {
        let start = std::time::Instant::now();
        let result = tools_list[idx].execute(args, ctx).await;
        let duration_ms = start.elapsed().as_millis() as u64;
        let content = tools::truncate_output(&result.content, tools::MAX_TOOL_RESULT_CHARS);

        emit(event_tx, AgentEvent::ToolEnd {
            session_id: session_id.to_string(),
            tool_name: tool_name.to_string(),
            tool_id: tool_id.to_string(),
            result: content.clone(), is_error: result.is_error, duration_ms,
        });

        (tool_id.to_string(), content, result.is_error)
    } else {
        let err = format!("Unknown tool: {tool_name}");
        emit(event_tx, AgentEvent::ToolEnd {
            session_id: session_id.to_string(),
            tool_name: tool_name.to_string(),
            tool_id: tool_id.to_string(),
            result: err.clone(), is_error: true, duration_ms: 0,
        });
        (tool_id.to_string(), err, true)
    }
}

fn truncate_for_error(s: &str) -> &str {
    if s.len() > 500 {
        &s[..500]
    } else {
        s
    }
}

/// Attempt to fix common JSON issues from LLMs.
fn try_fix_json(raw: &str) -> Option<serde_json::Value> {
    let fixed = raw.replace(",}", "}").replace(",]", "]");
    serde_json::from_str(&fixed).ok()
}

// ---------------------------------------------------------------------------
// Event emission helper
// ---------------------------------------------------------------------------

fn emit(tx: &Option<mpsc::UnboundedSender<AgentEvent>>, event: AgentEvent) {
    if let Some(tx) = tx {
        let _ = tx.send(event);
    }
}

// ---------------------------------------------------------------------------
// System prompt
// ---------------------------------------------------------------------------

fn default_system_prompt(working_dir: &std::path::Path) -> String {
    format!(
        r#"You are a coding agent with access to tools for reading, writing, and editing files, running shell commands, and searching codebases. You are working in the directory: {working_dir}

## Available Tools

- **Bash**: Execute shell commands. Use for builds, tests, git, system operations.
- **Read**: Read file contents with line numbers. Always read before editing.
- **Write**: Create or overwrite files. Use for new files.
- **Edit**: Make exact string replacements in files. Preferred for modifying existing files.
- **Glob**: Find files by name pattern (e.g., "**/*.rs").
- **Grep**: Search file contents with regex patterns.
- **Agent**: Spawn a sub-agent to handle a complex task autonomously.
- **WebFetch**: Fetch web page content.
- **WebSearch**: Search the web for information.
- **TaskCreate/TaskUpdate/TaskList**: Track work with tasks.
- **AskUserQuestion**: Ask the user for clarification.
- **EnterPlanMode/ExitPlanMode**: Switch to planning mode for design work.
- **EnterWorktree/ExitWorktree**: Create isolated git worktree for safe changes.

## Guidelines

- Always read a file before editing it.
- Use Edit for modifying existing files (not Write, which overwrites entirely).
- Use Bash for running builds, tests, and git commands.
- Be precise with Edit — old_string must match exactly including whitespace.
- When investigating issues, read the relevant code first before making changes.
- After making changes, verify them by reading the modified file or running tests.
- Keep responses concise. Focus on actions, not explanations.
- Use Agent tool to delegate complex subtasks to sub-agents."#,
        working_dir = working_dir.display()
    )
}
