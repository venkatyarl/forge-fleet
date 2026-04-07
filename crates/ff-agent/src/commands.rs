//! Slash command system — registry, parser, and built-in commands.
//!
//! Commands are triggered by `/command` or `!command` prefixes in user input.
//! The agent loop checks for commands before sending to the LLM.

use async_trait::async_trait;
use crate::agent_loop::AgentSession;
use crate::compaction;
use crate::session_store;

// ---------------------------------------------------------------------------
// Command trait and registry
// ---------------------------------------------------------------------------

/// A slash command handler.
#[async_trait]
pub trait Command: Send + Sync {
    /// Command name (without prefix, e.g. "help").
    fn name(&self) -> &str;
    /// Aliases (e.g. "h" for "help").
    fn aliases(&self) -> Vec<&str> { vec![] }
    /// Short description.
    fn description(&self) -> &str;
    /// Execute the command. Returns output text to show the user.
    async fn execute(&self, args: &str, session: &mut AgentSession) -> String;
}

/// Registry of all available commands.
pub struct CommandRegistry {
    commands: Vec<Box<dyn Command>>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        let mut registry = Self { commands: Vec::new() };
        // Register all built-in commands
        registry.register(Box::new(HelpCommand));
        registry.register(Box::new(ClearCommand));
        registry.register(Box::new(CompactCommand));
        registry.register(Box::new(ModelCommand));
        registry.register(Box::new(StatusCommand));
        registry.register(Box::new(CostCommand));
        registry.register(Box::new(ResumeCommand));
        registry.register(Box::new(SessionsCommand));
        registry.register(Box::new(RewindCommand));
        registry.register(Box::new(PlanCommand));
        registry.register(Box::new(DiffCommand));
        registry.register(Box::new(TasksCommand));
        registry.register(Box::new(ExportCommand));
        registry.register(Box::new(FastCommand));
        registry.register(Box::new(ConfigCommand));
        // Register extended commands
        for cmd in crate::commands_extended::extended_commands() {
            registry.register(cmd);
        }
        registry
    }

    pub fn register(&mut self, cmd: Box<dyn Command>) {
        self.commands.push(cmd);
    }

    /// Try to parse and execute a command from user input.
    /// Returns Some(output) if a command was found, None if not a command.
    pub async fn try_execute(&self, input: &str, session: &mut AgentSession) -> Option<String> {
        let trimmed = input.trim();
        if !trimmed.starts_with('/') && !trimmed.starts_with('!') {
            return None;
        }

        let without_prefix = &trimmed[1..];
        let (cmd_name, args) = match without_prefix.split_once(' ') {
            Some((name, args)) => (name.to_lowercase(), args.trim()),
            None => (without_prefix.to_lowercase(), ""),
        };

        for cmd in &self.commands {
            if cmd.name() == cmd_name || cmd.aliases().contains(&cmd_name.as_str()) {
                return Some(cmd.execute(args, session).await);
            }
        }

        Some(format!("Unknown command: /{cmd_name}. Type /help for available commands."))
    }

    pub fn list(&self) -> Vec<(&str, &str)> {
        self.commands.iter().map(|c| (c.name(), c.description())).collect()
    }
}

impl Default for CommandRegistry {
    fn default() -> Self { Self::new() }
}

// ---------------------------------------------------------------------------
// Built-in commands
// ---------------------------------------------------------------------------

struct HelpCommand;
#[async_trait]
impl Command for HelpCommand {
    fn name(&self) -> &str { "help" }
    fn aliases(&self) -> Vec<&str> { vec!["h", "?"] }
    fn description(&self) -> &str { "Show available commands" }
    async fn execute(&self, _args: &str, _session: &mut AgentSession) -> String {
        let registry = CommandRegistry::new();
        let mut output = String::from("Available commands:\n\n");
        for (name, desc) in registry.list() {
            output.push_str(&format!("  /{name:<16} {desc}\n"));
        }
        output
    }
}

struct ClearCommand;
#[async_trait]
impl Command for ClearCommand {
    fn name(&self) -> &str { "clear" }
    fn description(&self) -> &str { "Clear conversation history (keep system prompt)" }
    async fn execute(&self, _args: &str, session: &mut AgentSession) -> String {
        let system = session.messages.first().cloned();
        session.messages.clear();
        if let Some(sys) = system {
            session.messages.push(sys);
        }
        "Conversation cleared.".into()
    }
}

struct CompactCommand;
#[async_trait]
impl Command for CompactCommand {
    fn name(&self) -> &str { "compact" }
    fn description(&self) -> &str { "Manually compact conversation history" }
    async fn execute(&self, _args: &str, session: &mut AgentSession) -> String {
        let before = session.messages.len();
        let config = compaction::CompactionConfig {
            context_window_tokens: session.config.context_window_tokens,
            keep_recent_messages: 4,
            ..Default::default()
        };
        session.messages = compaction::compact_messages(&session.messages, &config);
        let after = session.messages.len();
        format!("Compacted: {before} messages → {after} messages")
    }
}

struct ModelCommand;
#[async_trait]
impl Command for ModelCommand {
    fn name(&self) -> &str { "model" }
    fn description(&self) -> &str { "Show or switch the current LLM model/endpoint" }
    async fn execute(&self, args: &str, session: &mut AgentSession) -> String {
        if args.is_empty() {
            return format!(
                "Current model: {}\nEndpoint: {}",
                session.config.model, session.config.llm_base_url
            );
        }
        // If args contains a URL, switch endpoint
        if args.starts_with("http") {
            session.config.llm_base_url = args.to_string();
            format!("Switched LLM endpoint to: {args}")
        } else {
            session.config.model = args.to_string();
            format!("Switched model to: {args}")
        }
    }
}

struct StatusCommand;
#[async_trait]
impl Command for StatusCommand {
    fn name(&self) -> &str { "status" }
    fn description(&self) -> &str { "Show session status (tokens, turns, model)" }
    async fn execute(&self, _args: &str, session: &mut AgentSession) -> String {
        let estimated_tokens = compaction::estimate_message_tokens(&session.messages);
        let pct = (estimated_tokens as f64 / session.config.context_window_tokens as f64) * 100.0;
        format!(
            "Session: {}\n\
             Model: {}\n\
             Endpoint: {}\n\
             Messages: {}\n\
             Estimated tokens: {} / {} ({:.0}%)\n\
             Turns: {}\n\
             Total input tokens: {}\n\
             Total output tokens: {}\n\
             Compactions: {}",
            session.id,
            session.config.model,
            session.config.llm_base_url,
            session.messages.len(),
            estimated_tokens,
            session.config.context_window_tokens,
            pct,
            session.turn_count,
            session.usage.total_input_tokens,
            session.usage.total_output_tokens,
            session.usage.compaction_count,
        )
    }
}

struct CostCommand;
#[async_trait]
impl Command for CostCommand {
    fn name(&self) -> &str { "cost" }
    fn description(&self) -> &str { "Show token usage for this session" }
    async fn execute(&self, _args: &str, session: &mut AgentSession) -> String {
        format!(
            "Token usage:\n\
             Input:  {} tokens\n\
             Output: {} tokens\n\
             Total:  {} tokens\n\
             (Local LLMs — no $ cost)",
            session.usage.total_input_tokens,
            session.usage.total_output_tokens,
            session.usage.total_tokens(),
        )
    }
}

struct ResumeCommand;
#[async_trait]
impl Command for ResumeCommand {
    fn name(&self) -> &str { "resume" }
    fn description(&self) -> &str { "Resume a previous session by ID" }
    async fn execute(&self, args: &str, session: &mut AgentSession) -> String {
        if args.is_empty() {
            return "Usage: /resume <session_id>".into();
        }
        match session_store::load_session(args).await {
            Ok(persisted) => {
                session.messages = persisted.messages;
                session.turn_count = persisted.meta.turn_count;
                format!(
                    "Resumed session {} ({} messages, {} turns)\nSummary: {}",
                    persisted.meta.session_id,
                    persisted.meta.message_count,
                    persisted.meta.turn_count,
                    persisted.meta.summary,
                )
            }
            Err(e) => format!("Failed to load session: {e}"),
        }
    }
}

struct SessionsCommand;
#[async_trait]
impl Command for SessionsCommand {
    fn name(&self) -> &str { "sessions" }
    fn description(&self) -> &str { "List saved sessions" }
    async fn execute(&self, _args: &str, _session: &mut AgentSession) -> String {
        let sessions = session_store::list_sessions().await;
        if sessions.is_empty() {
            return "No saved sessions.".into();
        }
        let mut output = String::from("Saved sessions:\n\n");
        for s in sessions.iter().take(20) {
            output.push_str(&format!(
                "  {} | {} | {} msgs | {}\n",
                &s.session_id[..8],
                s.updated_at.format("%Y-%m-%d %H:%M"),
                s.message_count,
                s.summary,
            ));
        }
        if sessions.len() > 20 {
            output.push_str(&format!("\n  ... and {} more\n", sessions.len() - 20));
        }
        output
    }
}

struct RewindCommand;
#[async_trait]
impl Command for RewindCommand {
    fn name(&self) -> &str { "rewind" }
    fn description(&self) -> &str { "Undo the last assistant turn (removes last assistant + tool messages)" }
    async fn execute(&self, _args: &str, session: &mut AgentSession) -> String {
        // Remove messages from the end until we hit a user message
        let mut removed = 0;
        while session.messages.len() > 1 {
            let last = session.messages.last().unwrap();
            if last.role == "user" && removed > 0 {
                break;
            }
            session.messages.pop();
            removed += 1;
        }
        if removed == 0 {
            "Nothing to rewind.".into()
        } else {
            format!("Rewound {removed} messages.")
        }
    }
}

struct PlanCommand;
#[async_trait]
impl Command for PlanCommand {
    fn name(&self) -> &str { "plan" }
    fn description(&self) -> &str { "Enter plan mode (read-only exploration)" }
    async fn execute(&self, _args: &str, _session: &mut AgentSession) -> String {
        "Plan mode entered. Focus on reading and exploring. Use /plan again to exit.".into()
    }
}

struct DiffCommand;
#[async_trait]
impl Command for DiffCommand {
    fn name(&self) -> &str { "diff" }
    fn description(&self) -> &str { "Show git diff in working directory" }
    async fn execute(&self, _args: &str, session: &mut AgentSession) -> String {
        let output = tokio::process::Command::new("git")
            .args(["diff", "--stat"])
            .current_dir(&session.config.working_dir)
            .output()
            .await;

        match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                if stdout.trim().is_empty() {
                    "No changes (working tree clean).".into()
                } else {
                    stdout.to_string()
                }
            }
            Err(e) => format!("git diff failed: {e}"),
        }
    }
}

struct TasksCommand;
#[async_trait]
impl Command for TasksCommand {
    fn name(&self) -> &str { "tasks" }
    fn description(&self) -> &str { "List current tasks" }
    async fn execute(&self, _args: &str, _session: &mut AgentSession) -> String {
        // Use the in-memory task store from task_tools
        let store = &*crate::tools::task_tools::TASK_STORE_PUB;
        if store.is_empty() {
            return "No tasks.".into();
        }
        let mut output = String::new();
        for entry in store.iter() {
            let t = entry.value();
            if t.status != "deleted" {
                output.push_str(&format!("  #{} [{}] {}\n", t.id, t.status, t.subject));
            }
        }
        if output.is_empty() { "No active tasks.".into() } else { output }
    }
}

struct ExportCommand;
#[async_trait]
impl Command for ExportCommand {
    fn name(&self) -> &str { "export" }
    fn description(&self) -> &str { "Export conversation as JSON or Markdown" }
    async fn execute(&self, args: &str, session: &mut AgentSession) -> String {
        let format = if args.contains("md") || args.contains("markdown") { "md" } else { "json" };
        let filename = format!(
            "session-{}.{}",
            &session.id.to_string()[..8],
            format
        );
        let path = session.config.working_dir.join(&filename);

        let content = if format == "json" {
            serde_json::to_string_pretty(&session.messages).unwrap_or_default()
        } else {
            let mut md = String::new();
            for msg in &session.messages {
                let role = &msg.role;
                let text = msg.text_content().unwrap_or("(no content)");
                md.push_str(&format!("## {role}\n\n{text}\n\n---\n\n"));
            }
            md
        };

        match tokio::fs::write(&path, &content).await {
            Ok(()) => format!("Exported to {}", path.display()),
            Err(e) => format!("Export failed: {e}"),
        }
    }
}

struct FastCommand;
#[async_trait]
impl Command for FastCommand {
    fn name(&self) -> &str { "fast" }
    fn description(&self) -> &str { "Switch to fastest available fleet LLM" }
    async fn execute(&self, _args: &str, session: &mut AgentSession) -> String {
        // Switch to smallest/fastest model — Qwen3.5-9B on James
        session.config.llm_base_url = "http://192.168.5.108:51001".into();
        session.config.model = "auto".into();
        "Switched to fast mode (James — Qwen3.5-9B)".into()
    }
}

struct ConfigCommand;
#[async_trait]
impl Command for ConfigCommand {
    fn name(&self) -> &str { "config" }
    fn description(&self) -> &str { "Show or modify session configuration" }
    async fn execute(&self, args: &str, session: &mut AgentSession) -> String {
        if args.is_empty() {
            return format!(
                "Session config:\n\
                 model: {}\n\
                 llm_base_url: {}\n\
                 working_dir: {}\n\
                 max_turns: {}\n\
                 temperature: {}\n\
                 max_tokens: {}\n\
                 context_window: {}\n\
                 tool_result_budget: {} chars\n\
                 auto_save: {}",
                session.config.model,
                session.config.llm_base_url,
                session.config.working_dir.display(),
                session.config.max_turns,
                session.config.temperature,
                session.config.max_tokens,
                session.config.context_window_tokens,
                session.config.tool_result_budget_chars,
                session.config.auto_save,
            );
        }
        // Parse key=value
        if let Some((key, value)) = args.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            match key {
                "max_turns" => {
                    if let Ok(v) = value.parse() { session.config.max_turns = v; }
                }
                "temperature" => {
                    if let Ok(v) = value.parse() { session.config.temperature = v; }
                }
                "max_tokens" => {
                    if let Ok(v) = value.parse() { session.config.max_tokens = v; }
                }
                _ => return format!("Unknown config key: {key}"),
            }
            format!("Set {key} = {value}")
        } else {
            "Usage: /config key=value".into()
        }
    }
}
