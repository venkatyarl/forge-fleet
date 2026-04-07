//! Context compaction — manage context window size for long agent sessions.
//!
//! When conversation history approaches the LLM's context window limit,
//! this module summarizes older messages to free space while preserving
//! key context. ForgeFleet's auto-compact system for sustained sessions.

use ff_api::tool_calling::ToolChatMessage;
use serde_json::Value;

/// Rough token estimation: ~4 chars per token for English text.
const CHARS_PER_TOKEN: usize = 4;

/// Configuration for auto-compaction behavior.
#[derive(Debug, Clone)]
pub struct CompactionConfig {
    /// Context window size in tokens (default 32768 for Qwen2.5-Coder-32B).
    pub context_window_tokens: usize,
    /// Trigger compaction when usage exceeds this fraction (default 0.80).
    pub trigger_threshold: f64,
    /// Number of recent messages to always keep (default 6).
    pub keep_recent_messages: usize,
    /// Target free tokens after compaction.
    pub target_free_tokens: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            context_window_tokens: 32_768,
            trigger_threshold: 0.80,
            keep_recent_messages: 6,
            target_free_tokens: 8_000,
        }
    }
}

/// Token usage tracking for a session.
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    /// Cumulative input tokens across all turns.
    pub total_input_tokens: u64,
    /// Cumulative output tokens across all turns.
    pub total_output_tokens: u64,
    /// Last reported input tokens from the LLM.
    pub last_input_tokens: u64,
    /// Last reported output tokens from the LLM.
    pub last_output_tokens: u64,
    /// Number of compactions performed.
    pub compaction_count: u32,
}

impl TokenUsage {
    pub fn record_turn(&mut self, input: u64, output: u64) {
        self.total_input_tokens += input;
        self.total_output_tokens += output;
        self.last_input_tokens = input;
        self.last_output_tokens = output;
    }

    pub fn total_tokens(&self) -> u64 {
        self.total_input_tokens + self.total_output_tokens
    }
}

/// Estimate token count for a list of messages.
pub fn estimate_message_tokens(messages: &[ToolChatMessage]) -> usize {
    messages
        .iter()
        .map(|m| estimate_single_message_tokens(m))
        .sum()
}

fn estimate_single_message_tokens(msg: &ToolChatMessage) -> usize {
    let mut chars = 0;

    // Role overhead
    chars += msg.role.len() + 4; // role: "xxx"\n

    // Content
    if let Some(content) = &msg.content {
        chars += match content {
            Value::String(s) => s.len(),
            other => other.to_string().len(),
        };
    }

    // Tool calls
    if let Some(calls) = &msg.tool_calls {
        for call in calls {
            chars += call.function.name.len();
            chars += call.function.arguments.len();
            chars += call.id.len();
            chars += 40; // overhead for JSON structure
        }
    }

    // Tool call ID
    if let Some(id) = &msg.tool_call_id {
        chars += id.len();
    }

    chars / CHARS_PER_TOKEN + 1
}

/// Check if compaction should be triggered.
pub fn should_compact(
    messages: &[ToolChatMessage],
    config: &CompactionConfig,
) -> bool {
    let estimated = estimate_message_tokens(messages);
    let threshold = (config.context_window_tokens as f64 * config.trigger_threshold) as usize;
    estimated > threshold
}

/// Compact messages by replacing older messages with a summary.
///
/// Returns the compacted message list. The first message (system prompt) and
/// the most recent `keep_recent` messages are always preserved.
pub fn compact_messages(
    messages: &[ToolChatMessage],
    config: &CompactionConfig,
) -> Vec<ToolChatMessage> {
    if messages.len() <= config.keep_recent_messages + 1 {
        return messages.to_vec();
    }

    // Always keep: [0] = system prompt, last N = recent messages
    let system_msg = messages[0].clone();
    let keep_start = messages.len().saturating_sub(config.keep_recent_messages);
    let recent = &messages[keep_start..];
    let to_summarize = &messages[1..keep_start];

    if to_summarize.is_empty() {
        return messages.to_vec();
    }

    // Build a summary of the compacted messages
    let summary = build_summary(to_summarize);

    let mut compacted = Vec::with_capacity(config.keep_recent_messages + 2);
    compacted.push(system_msg);
    compacted.push(ToolChatMessage::user(format!(
        "[Context Summary — {count} earlier messages compacted]\n\n{summary}",
        count = to_summarize.len()
    )));
    compacted.extend_from_slice(recent);

    compacted
}

/// Build a text summary of messages being compacted.
fn build_summary(messages: &[ToolChatMessage]) -> String {
    let mut summary = String::new();
    let mut tool_calls_seen = Vec::new();
    let mut key_decisions = Vec::new();

    for msg in messages {
        match msg.role.as_str() {
            "user" => {
                if let Some(text) = msg.text_content() {
                    let truncated = if text.len() > 200 {
                        format!("{}...", &text[..200])
                    } else {
                        text.to_string()
                    };
                    key_decisions.push(format!("User asked: {truncated}"));
                }
            }
            "assistant" => {
                if let Some(calls) = &msg.tool_calls {
                    for call in calls {
                        tool_calls_seen.push(format!(
                            "{}({})",
                            call.function.name,
                            truncate_args(&call.function.arguments, 80)
                        ));
                    }
                }
                if let Some(text) = msg.text_content() {
                    if !text.is_empty() && text.len() > 20 {
                        let truncated = if text.len() > 300 {
                            format!("{}...", &text[..300])
                        } else {
                            text.to_string()
                        };
                        key_decisions.push(format!("Assistant: {truncated}"));
                    }
                }
            }
            "tool" => {
                // Tool results are the bulkiest — just note they happened
            }
            _ => {}
        }
    }

    if !key_decisions.is_empty() {
        summary.push_str("Key interactions:\n");
        for (i, decision) in key_decisions.iter().enumerate().take(10) {
            summary.push_str(&format!("  {}. {decision}\n", i + 1));
        }
        if key_decisions.len() > 10 {
            summary.push_str(&format!("  ... and {} more\n", key_decisions.len() - 10));
        }
    }

    if !tool_calls_seen.is_empty() {
        summary.push_str("\nTools used: ");
        let display: Vec<_> = tool_calls_seen.iter().take(20).cloned().collect();
        summary.push_str(&display.join(", "));
        if tool_calls_seen.len() > 20 {
            summary.push_str(&format!(" ... and {} more", tool_calls_seen.len() - 20));
        }
        summary.push('\n');
    }

    if summary.is_empty() {
        summary.push_str("(Previous conversation context — no notable decisions recorded)");
    }

    summary
}

fn truncate_args(args: &str, max: usize) -> String {
    if args.len() <= max {
        args.to_string()
    } else {
        format!("{}...", &args[..max])
    }
}

/// Apply tool-result budgeting: truncate oldest tool results when total size
/// exceeds the budget. Returns the number of results truncated.
pub fn apply_tool_result_budget(
    messages: &mut [ToolChatMessage],
    budget_chars: usize,
) -> usize {
    // Calculate total tool result size
    let total: usize = messages
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.text_content())
        .map(|c| c.len())
        .sum();

    if total <= budget_chars {
        return 0;
    }

    let mut to_free = total - budget_chars;
    let mut truncated = 0;

    // Walk from oldest to newest, truncating tool results
    for msg in messages.iter_mut() {
        if to_free == 0 {
            break;
        }
        if msg.role != "tool" {
            continue;
        }
        if let Some(content) = msg.text_content() {
            if content.len() > 100 {
                let freed = content.len() - 50;
                msg.content = Some(Value::String("[tool result truncated — context budget exceeded]".into()));
                to_free = to_free.saturating_sub(freed);
                truncated += 1;
            }
        }
    }

    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_basic() {
        let msgs = vec![
            ToolChatMessage::system("You are an agent."),
            ToolChatMessage::user("Hello"),
            ToolChatMessage::assistant("Hi there!"),
        ];
        let tokens = estimate_message_tokens(&msgs);
        assert!(tokens > 0);
        assert!(tokens < 100);
    }

    #[test]
    fn should_compact_small_conversation() {
        let msgs = vec![
            ToolChatMessage::system("sys"),
            ToolChatMessage::user("hi"),
        ];
        let config = CompactionConfig::default();
        assert!(!should_compact(&msgs, &config));
    }

    #[test]
    fn compact_preserves_system_and_recent() {
        let mut msgs = vec![ToolChatMessage::system("system prompt")];
        for i in 0..20 {
            msgs.push(ToolChatMessage::user(format!("message {i}")));
            msgs.push(ToolChatMessage::assistant(format!("reply {i}")));
        }

        let config = CompactionConfig {
            keep_recent_messages: 4,
            ..Default::default()
        };

        let compacted = compact_messages(&msgs, &config);
        // system + summary + 4 recent
        assert_eq!(compacted.len(), 6);
        assert_eq!(compacted[0].role, "system");
        assert!(compacted[1].text_content().unwrap().contains("Context Summary"));
    }

    #[test]
    fn tool_result_budget_truncates_oldest() {
        let mut msgs = vec![
            ToolChatMessage::system("sys"),
            ToolChatMessage::user("do something"),
            ToolChatMessage::tool_result("call_1", &"x".repeat(1000)),
            ToolChatMessage::tool_result("call_2", &"y".repeat(1000)),
            ToolChatMessage::tool_result("call_3", &"z".repeat(1000)),
        ];

        let truncated = apply_tool_result_budget(&mut msgs, 1500);
        assert!(truncated > 0);
        // First tool result should be truncated
        assert!(msgs[2].text_content().unwrap().contains("truncated"));
    }
}
