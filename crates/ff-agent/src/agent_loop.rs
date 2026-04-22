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
use crate::scoped_memory::MemoryScope;
use crate::session_store;
use std::sync::Arc;

use crate::inference_router::InferenceRouter;
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
    /// Optional image path to attach to the first user message (multimodal).
    pub image_path: Option<PathBuf>,
    /// Local-first inference router. When set, the agent uses this to pick the
    /// active LLM endpoint per turn, with automatic fleet failover.
    /// Falls back to `llm_base_url` if not set (backwards compat).
    pub inference_router: Option<Arc<InferenceRouter>>,
    /// Permission mode: "default" | "accept_edits" | "bypass" | "plan".
    ///
    /// In "plan" mode, mutating tools (Bash, Edit, Write, NotebookEdit, etc.)
    /// are blocked at dispatch; only read-only tools run. "accept_edits" and
    /// "bypass" are recognized for forward compatibility and behave the same as
    /// "default" in the headless agent loop (there is no confirmation UI yet).
    pub permission_mode: String,
    /// Output verbosity hint appended to the system prompt at the start of each
    /// run. Values: "concise" | "normal" | "verbose".
    pub output_style: String,
    /// Optional tool allowlist. When `Some(names)`, only tools whose
    /// `name()` appears in the set are registered with the LLM. When
    /// `None` (default), all registered tools are exposed.
    ///
    /// Useful for forcing-a-shape: e.g. pure-create tasks can set
    /// `Some(["Write", "Bash"])` to forbid Read so the model can't idle
    /// in a Read-loop without ever calling Write. See
    /// `feedback_ff_supervise_read_loop.md`.
    pub allowed_tools: Option<std::collections::HashSet<String>>,
}

impl Default for AgentSessionConfig {
    fn default() -> Self {
        Self {
            model: "auto".into(),
            llm_base_url: "http://localhost:55000".into(),
            working_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            system_prompt: None,
            max_turns: 30,
            temperature: 0.3,
            max_tokens: 4096,
            context_window_tokens: 32_768,
            tool_result_budget_chars: 50_000,
            auto_save: true,
            memory_scope: None,
            inference_router: None,
            image_path: None,
            permission_mode: "default".into(),
            output_style: "normal".into(),
            allowed_tools: None,
        }
    }
}

/// Read-only tools permitted while `permission_mode == "plan"`.
///
/// Anything not in this set is blocked at tool dispatch and returns a
/// synthetic tool result telling the LLM to exit plan mode.
pub(crate) const PLAN_MODE_READ_ONLY_TOOLS: &[&str] = &[
    "Read", "Glob", "Grep", "WebFetch", "WebSearch", "ToolSearch",
    "TaskList", "TaskGet",
    "fleet_status", "fleet_nodes_db", "fleet_models_db",
    "fleet_models_catalog", "fleet_models_search",
    "fleet_models_library", "fleet_models_deployments",
    "fleet_models_disk_usage",
];

/// Returns the blocked-tool synthetic result string, or None if the tool is
/// allowed under the current permission mode.
pub(crate) fn plan_mode_block(permission_mode: &str, tool_name: &str) -> Option<String> {
    if permission_mode == "plan" && !PLAN_MODE_READ_ONLY_TOOLS.contains(&tool_name) {
        Some(format!(
            "blocked: plan mode forbids mutating tools (attempted: {tool_name}). \
             Use /plan to exit plan mode, then retry."
        ))
    } else {
        None
    }
}

/// Translate the configured output_style into a one-line directive appended to
/// the system prompt. Returns an empty string for "normal" (no addition).
pub(crate) fn output_style_directive(output_style: &str) -> &'static str {
    match output_style {
        "concise" => "\n\n## Output Style\nReply tersely. No preamble, no recap, no more than 3 sentences unless the user asks for detail.",
        "verbose" => "\n\n## Output Style\nWalk the user through your reasoning step by step. Show your work.",
        _ => "",
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
    /// Track recent tool signatures for loop detection.
    recent_tool_sigs: Vec<String>,
    /// Count of consecutive tool errors (resets on success).
    consecutive_errors: u32,
    /// Count of mid-session cargo-check failures the agent has been asked to
    /// fix after Rust edits. Caps at `MAX_BUILD_VERIFY_RETRIES` before
    /// the session is aborted with `BuildStuckAfterNRetries`. See issue #117.
    build_verify_retries: u32,
    /// Cached answer for "is Rust auto-verify enabled for this session?"
    /// Populated lazily on the first `.rs` edit of the session.
    auto_verify_rust: Option<bool>,
}

/// Max mid-session cargo-check retries before a task aborts with
/// `BuildStuckAfterNRetries`. See issue #117.
pub const MAX_BUILD_VERIFY_RETRIES: u32 = 5;

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

        // Apply --allowed-tools filter if set: only register tools whose
        // `name()` is in the allowlist. When None, all core tools are exposed.
        let tools = if let Some(allow) = &config.allowed_tools {
            tools::core_tools_arc()
                .into_iter()
                .filter(|t| allow.contains(t.name()))
                .collect()
        } else {
            tools::core_tools_arc()
        };

        Self {
            id: session_id,
            config,
            messages,
            tools,
            cancel_token: CancellationToken::new(),
            usage: TokenUsage::default(),
            tracker: ConversationTracker::new(),
            tool_ctx,
            compaction_config,
            turn_count: 0,
            recent_tool_sigs: Vec::new(),
            consecutive_errors: 0,
            build_verify_retries: 0,
            auto_verify_rust: None,
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

        // Apply --allowed-tools filter if set: only register tools whose
        // `name()` is in the allowlist. When None, all core tools are exposed.
        let tools = if let Some(allow) = &config.allowed_tools {
            tools::core_tools_arc()
                .into_iter()
                .filter(|t| allow.contains(t.name()))
                .collect()
        } else {
            tools::core_tools_arc()
        };

        Self {
            id: session_id,
            config,
            messages,
            tools,
            cancel_token: CancellationToken::new(),
            usage: TokenUsage::default(),
            tracker: ConversationTracker::new(),
            tool_ctx,
            compaction_config,
            turn_count,
            recent_tool_sigs: Vec::new(),
            consecutive_errors: 0,
            build_verify_retries: 0,
            auto_verify_rust: None,
        }
    }

    /// Inject a user message and run the agent loop until completion.
    pub async fn run(
        &mut self,
        prompt: &str,
        event_tx: Option<mpsc::UnboundedSender<AgentEvent>>,
    ) -> AgentOutcome {
        // Re-apply output-style directive to the system prompt on every run,
        // so `/output-style` changes take effect on the NEXT request.
        if let Some(sys_msg) = self.messages.first_mut() {
            if let Some(current) = sys_msg.text_content().map(String::from) {
                // Strip any previously-applied directive (marker-bounded).
                const START: &str = "\n\n<!--ff:output_style-->";
                const END: &str = "<!--/ff:output_style-->";
                let base = if let (Some(s), Some(e)) = (current.find(START), current.find(END)) {
                    if e > s {
                        let mut b = String::new();
                        b.push_str(&current[..s]);
                        b.push_str(&current[e + END.len()..]);
                        b
                    } else {
                        current.clone()
                    }
                } else {
                    current.clone()
                };
                let directive = output_style_directive(&self.config.output_style);
                let new_prompt = if directive.is_empty() {
                    base
                } else {
                    format!("{base}{START}{directive}{END}")
                };
                *sys_msg = ToolChatMessage::system(new_prompt);
            }
        }

        // Load three-brain memory and inject into system prompt (first turn only)
        if self.turn_count == 0 {
            let brain_ctx = crate::brain::BrainLoader::load_for_dir(&self.config.working_dir).await;
            let injection = crate::brain::BrainLoader::build_injection(&brain_ctx, 3000);
            if !injection.is_empty() {
                if let Some(sys_msg) = self.messages.first_mut() {
                    if let Some(current) = sys_msg.text_content().map(String::from) {
                        *sys_msg = ToolChatMessage::system(format!("{current}{injection}"));
                    }
                }
            }
        }

        // Inject Focus Stack + Backlog context as a system reminder
        let tracker_ctx = self.tracker.context_injection();
        let full_prompt = if !tracker_ctx.is_empty() {
            format!("[System Context]\n{tracker_ctx}\nNow, please address: {prompt}")
        } else {
            prompt.to_string()
        };

        // If image is attached, create multimodal message
        if let Some(image_path) = &self.config.image_path {
            if let Ok(image_data) = tokio::fs::read(image_path).await {
                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&image_data);
                let ext = image_path.extension().and_then(|e| e.to_str()).unwrap_or("png");
                let mime = match ext {
                    "jpg" | "jpeg" => "image/jpeg",
                    "gif" => "image/gif",
                    "webp" => "image/webp",
                    "svg" => "image/svg+xml",
                    _ => "image/png",
                };
                self.messages.push(ToolChatMessage::user_with_image(&full_prompt, &b64, mime));
                // Clear image_path so it's only sent once
                self.config.image_path = None;
            } else {
                warn!(path = %image_path.display(), "failed to read image file");
                self.messages.push(ToolChatMessage::user(full_prompt));
            }
        } else {
            self.messages.push(ToolChatMessage::user(full_prompt));
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

        // Auto-collect training data for future LoRA fine-tuning
        if self.config.auto_save {
            let success = matches!(outcome, AgentOutcome::EndTurn { .. });
            let task_type = crate::orchestrator_agent::analyze_task(prompt);
            let conv = crate::training::TrainingConversation {
                id: self.id.to_string(),
                system_prompt: self.messages.first()
                    .and_then(|m| m.text_content())
                    .unwrap_or_default()
                    .to_string(),
                turns: self.messages.iter().skip(1).map(|m| {
                    crate::training::TrainingTurn {
                        role: m.role.clone(),
                        content: m.text_content().unwrap_or_default().to_string(),
                        tool_calls: m.tool_calls.as_ref().map(|calls| {
                            calls.iter().map(|c| crate::training::TrainingToolCall {
                                name: c.function.name.clone(),
                                arguments: c.function.arguments.clone(),
                            }).collect()
                        }),
                        tool_call_id: m.tool_call_id.clone(),
                    }
                }).collect(),
                task_type: format!("{:?}", task_type),
                success,
                collected_at: chrono::Utc::now().to_rfc3339(),
            };
            if let Err(e) = crate::training::save_conversation(&conv).await {
                debug!(error = %e, "failed to save training data");
            }

            // Auto-learn from this session — extract and route to brains
            let brain_ctx = crate::brain::BrainLoader::load_for_dir(&self.config.working_dir).await;
            let learn_report = crate::learning::extract_and_route(
                &self.messages, &brain_ctx, &self.id.to_string()
            ).await;

            // Auto-sync hive if we added hive entries
            if learn_report.hive_count > 0 {
                let hive = crate::hive_sync::HiveSync::new();
                hive.auto_sync().await;
            }

            // Apply relevance decay periodically (roughly every 10 sessions)
            if self.turn_count % 10 == 0 {
                crate::learning::decay_all_brains(&brain_ctx).await;
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
        let mut llm_retry_count = 0u32;

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
            // Enable llama.cpp prompt caching — the server reuses KV-cache
            // entries for the static system-prompt + tool-definition prefix,
            // cutting time-to-first-token on turns 2+ by ~40-60%.
            cache_prompt: Some(true),
        };

        // --- Send to LLM ---
        emit(
            &event_tx,
            AgentEvent::Status {
                session_id: session_id.clone(),
                message: format!("Thinking... (sending {} messages to LLM)", session.messages.len()),
            },
        );

        // Resolve the active LLM endpoint: use InferenceRouter (local-first +
        // fleet failover) when available, otherwise fall back to llm_base_url.
        let active_base = if let Some(router) = &session.config.inference_router {
            router.active_url().unwrap_or_else(|| session.config.llm_base_url.clone())
        } else {
            session.config.llm_base_url.clone()
        };

        let url = format!("{}/v1/chat/completions", active_base.trim_end_matches('/'));

        debug!(turn, url = %url, model = %session.config.model, "sending agent request");

        let response = tokio::select! {
            _ = session.cancel_token.cancelled() => {
                return AgentOutcome::Cancelled;
            }
            result = send_request(http_client, &url, &request) => result
        };

        let response = match response {
            Ok(resp) => {
                // Mark the endpoint healthy on success
                if let Some(router) = &session.config.inference_router {
                    router.report_success(&active_base);
                }
                resp
            }
            Err(err) => {
                let err_str = format!("{err}");

                // Detect context overflow and auto-compact
                if err_str.contains("exceed_context_size") || err_str.contains("context size") || err_str.contains("too many tokens") {
                    let before = session.messages.len();
                    let compact_config = crate::compaction::CompactionConfig {
                        context_window_tokens: 8192, // conservative — match server
                        trigger_threshold: 0.5,
                        keep_recent_messages: 4,
                        target_free_tokens: 4000,
                    };
                    session.messages = crate::compaction::compact_messages(&session.messages, &compact_config);
                    let after = session.messages.len();
                    session.usage.compaction_count += 1;

                    emit(&event_tx, AgentEvent::Compaction {
                        session_id: session_id.clone(),
                        messages_before: before,
                        messages_after: after,
                    });
                    emit(&event_tx, AgentEvent::Status {
                        session_id: session_id.clone(),
                        message: format!("Context overflow — auto-compacted {before} → {after} messages. Retrying..."),
                    });

                    continue;
                }

                // On LLM failure: mark endpoint as down and immediately try the
                // next available fleet node (rather than waiting with backoff).
                if let Some(router) = &session.config.inference_router {
                    router.report_failure(&active_base);
                    let next = router.active_url();
                    if next.as_deref() != Some(active_base.as_str()) {
                        emit(&event_tx, AgentEvent::Status {
                            session_id: session_id.clone(),
                            message: format!(
                                "LLM at {} unreachable — failing over to {}",
                                active_base,
                                next.as_deref().unwrap_or("(none)")
                            ),
                        });
                        // Retry this turn immediately on the new endpoint
                        continue;
                    }
                }

                // No router or all endpoints exhausted — exponential backoff
                llm_retry_count += 1;
                if llm_retry_count <= 3 {
                    let delay_ms = 1000 * (1u64 << llm_retry_count); // 2s, 4s, 8s
                    emit(&event_tx, AgentEvent::Status {
                        session_id: session_id.clone(),
                        message: format!("LLM error (attempt {}/3), retrying in {}s: {}", llm_retry_count, delay_ms / 1000, &err_str[..err_str.len().min(100)]),
                    });
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    continue;
                }

                let msg = format!("LLM request failed after 3 retries: {err}");
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
        // Set by any tool that returns should_end_turn (e.g. AskUserQuestion).
        // After processing the batch we break out of the turn loop so the
        // model isn't re-invoked to rationalize past the pause.
        let mut end_turn_requested = false;

        if tool_calls.len() > 1 {
            // Parallel execution using futures::join_all
            let mut futures = Vec::new();
            // Pre-computed plan-mode blocks (no spawn needed).
            let mut pre_blocked: Vec<(String, String, bool)> = Vec::new();

            for tc in &tool_calls {
                let tool_name = tc.function.name.clone();
                let tool_id = tc.id.clone();
                let args_str = tc.function.arguments.clone();

                // Plan-mode gate: short-circuit without spawning.
                if let Some(blocked_msg) = plan_mode_block(&session.config.permission_mode, &tool_name) {
                    emit(&event_tx, AgentEvent::ToolEnd {
                        session_id: session_id.clone(),
                        tool_name: tool_name.clone(),
                        tool_id: tool_id.clone(),
                        result: blocked_msg.clone(),
                        is_error: true,
                        duration_ms: 0,
                    });
                    pre_blocked.push((tool_id, blocked_msg, true));
                    continue;
                }

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

            // Record pre-blocked results first
            for (tool_id, content, is_error) in pre_blocked {
                if is_error { session.consecutive_errors += 1; }
                tool_results.push((tool_id, content));
            }

            let results = futures::future::join_all(futures).await;
            for result in results {
                match result {
                    Ok((tool_id, content, is_error, should_end_turn)) => {
                        if is_error { session.consecutive_errors += 1; } else { session.consecutive_errors = 0; }
                        if should_end_turn { end_turn_requested = true; }
                        tool_results.push((tool_id, content));
                    }
                    Err(e) => {
                        session.consecutive_errors += 1;
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

            // --- Plan-mode gate: block mutating tools before dispatch ---
            if let Some(blocked_msg) = plan_mode_block(&session.config.permission_mode, &tool_name) {
                emit(
                    &event_tx,
                    AgentEvent::ToolEnd {
                        session_id: session_id.clone(),
                        tool_name: tool_name.clone(),
                        tool_id: tool_id.clone(),
                        result: blocked_msg.clone(),
                        is_error: true,
                        duration_ms: 0,
                    },
                );
                session
                    .messages
                    .push(ToolChatMessage::tool_result(&tool_id, &blocked_msg));
                continue;
            }

            // Find and execute tool
            if let Some(idx) = tools::find_tool_arc(&tool_name, &session.tools) {
                let start = std::time::Instant::now();
                let result = session.tools[idx].execute(args, &session.tool_ctx).await;
                let duration_ms = start.elapsed().as_millis() as u64;

                let result_content =
                    tools::truncate_output(&result.content, tools::MAX_TOOL_RESULT_CHARS);

                if result.is_error { session.consecutive_errors += 1; } else { session.consecutive_errors = 0; }
                if result.should_end_turn { end_turn_requested = true; }

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

        // --- End-turn request from tool (e.g. AskUserQuestion) ---
        //
        // If any tool this turn set should_end_turn=true, terminate the
        // turn NOW without re-invoking the LLM. Prevents the "agent keeps
        // looping past a user question" failure mode where the model
        // fabricates its own answer and keeps calling tools.
        //
        // The next user message will resume the agent with the answer.
        if end_turn_requested {
            return AgentOutcome::EndTurn {
                final_message: "Waiting for user response…".into(),
            };
        }

        // --- #117: Per-edit cargo check (auto-verify-after-edit loop) ---
        //
        // Did any tool call in this turn mutate a `.rs` file? If so, run
        // `cargo check --workspace` to confirm the edit compiles. On
        // failure, inject up to ~2000 chars of stderr as a synthetic user
        // message so the agent fixes the breakage on the next turn.
        //
        // Capped at `MAX_BUILD_VERIFY_RETRIES` per session. Exceeding the
        // cap returns `AgentOutcome::Error("BuildStuckAfterNRetries: ...")`
        // so the outer `ff supervise` loop can decide what to do next.
        let edited_rust_file = tool_calls.iter().any(|tc| {
            RUST_MUTATING_TOOLS.contains(&tc.function.name.as_str())
                && tool_call_file_path(&tc.function.arguments)
                    .map(|p| is_rust_file(&p))
                    .unwrap_or(false)
        });

        if edited_rust_file {
            // Lazy-load the per-session toggle on the first .rs edit.
            if session.auto_verify_rust.is_none() {
                session.auto_verify_rust = Some(auto_verify_rust_enabled().await);
            }

            if session.auto_verify_rust == Some(true) {
                emit(&event_tx, AgentEvent::Status {
                    session_id: session_id.clone(),
                    message: "Verifying build (cargo check --workspace)...".into(),
                });

                let verdict = cargo_check_workspace(&session.config.working_dir, 2000).await;
                match verdict {
                    CargoVerifyResult::Ok => {
                        session.build_verify_retries = 0;
                        emit(&event_tx, AgentEvent::Status {
                            session_id: session_id.clone(),
                            message: "cargo check: ok".into(),
                        });
                    }
                    CargoVerifyResult::Skipped(why) => {
                        debug!(session = %session_id, reason = %why, "cargo check skipped");
                    }
                    CargoVerifyResult::Failed(stderr) => {
                        session.build_verify_retries += 1;
                        warn!(
                            session = %session_id,
                            retry = session.build_verify_retries,
                            "cargo check failed after Rust edit"
                        );

                        if session.build_verify_retries > MAX_BUILD_VERIFY_RETRIES {
                            let msg = format!(
                                "BuildStuckAfterNRetries: cargo check failed {} times in a row \
                                 after Rust edits — aborting task so the supervisor can retry.",
                                MAX_BUILD_VERIFY_RETRIES
                            );
                            emit(&event_tx, AgentEvent::Error {
                                session_id: session_id.clone(),
                                message: msg.clone(),
                            });
                            return AgentOutcome::Error(msg);
                        }

                        emit(&event_tx, AgentEvent::Status {
                            session_id: session_id.clone(),
                            message: format!(
                                "cargo check failed (retry {}/{}). Injecting errors for fix...",
                                session.build_verify_retries, MAX_BUILD_VERIFY_RETRIES
                            ),
                        });

                        session.messages.push(ToolChatMessage::user(format!(
                            "Your last edit broke the build. Fix these errors:\n\n\
                             ```\n{stderr}\n```\n\n\
                             Read the failing file(s) and correct the compile errors. \
                             Do not move on to other tasks until `cargo check --workspace` is green."
                        )));
                    }
                }
            }
        }

        // --- Loop detection ---
        for tc in &tool_calls {
            let sig = format!("{}:{}", tc.function.name, &tc.function.arguments[..tc.function.arguments.len().min(80)]);
            session.recent_tool_sigs.push(sig);
        }
        // Keep sliding window of last 20 signatures
        if session.recent_tool_sigs.len() > 20 {
            session.recent_tool_sigs.drain(0..session.recent_tool_sigs.len() - 20);
        }
        // Check for repetition
        if let Some(last) = session.recent_tool_sigs.last() {
            let repeat_count = session.recent_tool_sigs.iter().filter(|s| *s == last).count();
            if repeat_count >= 3 {
                warn!(session = %session_id, tool = %last, count = repeat_count, "loop detected");
                emit(&event_tx, AgentEvent::Status {
                    session_id: session_id.clone(),
                    message: format!("Loop detected: same action repeated {} times. Injecting recovery...", repeat_count),
                });
                session.messages.push(ToolChatMessage::user(
                    "STOP. You are repeating the same action in a loop. \
                     This approach is not working. Step back and try a completely different strategy. \
                     What is the root cause of the problem? Try a different tool or different arguments."
                        .to_string()
                ));
                session.recent_tool_sigs.clear();
            }
        }

        // --- Consecutive error ceiling ---
        if session.consecutive_errors >= 5 {
            warn!(session = %session_id, errors = session.consecutive_errors, "consecutive error ceiling hit");
            emit(&event_tx, AgentEvent::Status {
                session_id: session_id.clone(),
                message: format!("{} consecutive tool errors. Injecting recovery...", session.consecutive_errors),
            });
            session.messages.push(ToolChatMessage::user(
                "5 consecutive tool calls have failed. STOP and reassess. \
                 What is fundamentally wrong? Check if the file exists, if you have the right path, \
                 or if you need a completely different approach. Read the error messages carefully."
                    .to_string()
            ));
            session.consecutive_errors = 0;
        }

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
/// Returns `(tool_id, content, is_error, should_end_turn)`.
/// `should_end_turn` is set by tools that need to pause the agent loop
/// (e.g. AskUserQuestion, ExitPlanMode). The caller must check it and
/// break out of the turn if true, so the model doesn't get re-invoked
/// and rationalize past the pause.
async fn execute_single_tool(
    tool_name: &str,
    tool_id: &str,
    args_str: &str,
    tools_list: &[Arc<dyn AgentTool>],
    ctx: &AgentToolContext,
    event_tx: &Option<mpsc::UnboundedSender<AgentEvent>>,
    session_id: &str,
) -> (String, String, bool, bool) {
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
                return (tool_id.to_string(), err, true, false);
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

        (tool_id.to_string(), content, result.is_error, result.should_end_turn)
    } else {
        let err = format!("Unknown tool: {tool_name}");
        emit(event_tx, AgentEvent::ToolEnd {
            session_id: session_id.to_string(),
            tool_name: tool_name.to_string(),
            tool_id: tool_id.to_string(),
            result: err.clone(), is_error: true, duration_ms: 0,
        });
        (tool_id.to_string(), err, true, false)
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
// #117 — Per-edit cargo check (self-verify loop)
// ---------------------------------------------------------------------------

/// Tools that can mutate files and therefore may break a Rust build.
pub(crate) const RUST_MUTATING_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// Extract a `file_path`-shaped string from a tool-call arguments JSON.
/// Supports the standard `file_path` key used by Edit/Write and the
/// `notebook_path` key used by NotebookEdit.
pub(crate) fn tool_call_file_path(args_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(args_json).ok()?;
    let p = v.get("file_path")
        .or_else(|| v.get("notebook_path"))
        .and_then(|x| x.as_str())?;
    Some(p.to_string())
}

/// True if `path` ends with `.rs` (case-insensitive).
pub(crate) fn is_rust_file(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("rs"))
        .unwrap_or(false)
}

/// Walk up from `start` looking for a `Cargo.toml`. Returns the directory
/// containing it — where `cargo check --workspace` should run.
pub(crate) fn find_cargo_manifest_dir(start: &std::path::Path) -> Option<PathBuf> {
    let mut cur: Option<&std::path::Path> = Some(start);
    while let Some(dir) = cur {
        if dir.join("Cargo.toml").is_file() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

/// Result of an auto-verify cargo-check run after a Rust edit.
#[derive(Debug)]
pub(crate) enum CargoVerifyResult {
    /// Build succeeded — clear the retry counter and continue.
    Ok,
    /// Build failed — first `max_chars` of stderr.
    Failed(String),
    /// Skipped (no Cargo.toml up-tree, cargo missing, or timed out).
    Skipped(String),
}

/// Run `cargo check --workspace --message-format=short` in the closest
/// Cargo workspace root above `working_dir`. 5-minute hard timeout.
pub(crate) async fn cargo_check_workspace(
    working_dir: &std::path::Path,
    max_chars: usize,
) -> CargoVerifyResult {
    let manifest_dir = match find_cargo_manifest_dir(working_dir) {
        Some(d) => d,
        None => return CargoVerifyResult::Skipped(
            "no Cargo.toml in or above working_dir".into()
        ),
    };

    let fut = tokio::process::Command::new("cargo")
        .arg("check")
        .arg("--workspace")
        .arg("--message-format=short")
        .current_dir(&manifest_dir)
        .output();

    let out = match tokio::time::timeout(std::time::Duration::from_secs(300), fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return CargoVerifyResult::Skipped(format!("failed to spawn cargo: {e}")),
        Err(_) => return CargoVerifyResult::Skipped("cargo check timed out after 5m".into()),
    };

    if out.status.success() {
        return CargoVerifyResult::Ok;
    }

    // Prefer stderr (cargo writes diagnostics there); fall back to stdout.
    let mut msg = String::from_utf8_lossy(&out.stderr).into_owned();
    if msg.trim().is_empty() {
        msg = String::from_utf8_lossy(&out.stdout).into_owned();
    }
    if msg.len() > max_chars {
        msg.truncate(max_chars);
        msg.push_str("\n… (truncated)");
    }
    CargoVerifyResult::Failed(msg)
}

/// Resolve the `agent_auto_verify_rust` fleet secret. Default `true`.
/// Any value starting with `0`, `f`/`F`, or `n`/`N` disables auto-verify.
pub(crate) async fn auto_verify_rust_enabled() -> bool {
    match crate::fleet_info::fetch_secret("agent_auto_verify_rust").await {
        None => true,
        Some(v) => {
            let first = v.trim().chars().next();
            !matches!(first, Some('0') | Some('f') | Some('F') | Some('n') | Some('N'))
        }
    }
}

// ---------------------------------------------------------------------------
// System prompt
// ---------------------------------------------------------------------------

fn default_system_prompt(working_dir: &std::path::Path) -> String {
    let fleet_section = crate::fleet_info::cached_fleet_description();
    format!(
        r#"You are ForgeFleet, an AI agent running on a distributed fleet of computers. You have access to tools for file operations, shell commands, web access, and fleet management.

Working directory: {working_dir}

## Fleet Nodes
You manage a fleet of computers. Here are the nodes you can SSH into (loaded from the fleet database):
{fleet_section}

SSH user for each node is listed in the fleet database; use it when running remote commands.

## How to use tools
- Use the Bash tool to run shell commands (ssh, git, cargo, etc.)
- Use the Agent tool to spawn sub-agents on different fleet nodes for parallel work
- Use Read/Write/Edit for file operations
- Use Glob/Grep for searching
- When asked to do something on multiple computers, use Bash with SSH to run commands remotely

## Guidelines
- Always use tools when the task requires interacting with the system
- For fleet operations, SSH into nodes using: ssh user@ip 'command'
- For parallel work across nodes, spawn sub-agents with the Agent tool
- Be concise. Show results, not explanations.
- Always READ a file before trying to EDIT it.
- Only stop calling tools when the task is ACTUALLY DONE, not when you have a plan.

## Reasoning Protocol (ReAct)
Follow the Reason-Act-Observe pattern on every turn:
1. REASON: Before each tool call, briefly state your reasoning (1 sentence) — what you intend to do and why.
2. ACT: Call the tool.
3. OBSERVE: After each tool result, briefly state what you learned (1 sentence) — what the result tells you and what to do next.
This makes your thought process visible and improves tool selection accuracy.

## Error Recovery
- If a tool call fails, READ the error message carefully and try a different approach.
- NEVER retry the exact same command that just failed — change something.
- If 3 attempts at the same approach fail, try a completely different strategy.
- If you cannot complete a task, explain specifically what is blocking you and what you tried.

## Progress
- Every turn must make concrete forward progress — read a file, run a command, edit code.
- If you don't have enough information, use a tool to get it. Don't speculate.
- If you're unsure, ask the user with AskUserQuestion instead of guessing."#,
        working_dir = working_dir.display(),
        fleet_section = fleet_section,
    )
}
