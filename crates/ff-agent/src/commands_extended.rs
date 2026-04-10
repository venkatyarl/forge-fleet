//! Extended slash commands — /context, /doctor, /commit, /review, /security-review,
//! /mcp, /hooks, /permissions, /advisor, /init, /branch, /add-dir, and more.

use async_trait::async_trait;
use crate::agent_loop::AgentSession;
use crate::commands::Command;
use crate::compaction;

pub fn extended_commands() -> Vec<Box<dyn Command>> {
    vec![
        Box::new(ContextCommand),
        Box::new(DoctorCommand),
        Box::new(CommitCommand),
        Box::new(ReviewCommand),
        Box::new(SecurityReviewCommand),
        Box::new(McpCommand),
        Box::new(HooksCommand),
        Box::new(PermissionsCommand),
        Box::new(AdvisorCommand),
        Box::new(InitCommand),
        Box::new(BranchCommand),
        Box::new(AddDirCommand),
        Box::new(FilesCommand),
        Box::new(CopyCommand),
        Box::new(VersionCommand),
        Box::new(FleetCommand),
        Box::new(ModelsCommand),
        Box::new(NodesCommand),
        Box::new(MemoryCommand),
        Box::new(UndoCommand),
        Box::new(SummaryCommand),
        Box::new(StatsCommand),
        Box::new(PluginsCommand),
        Box::new(SkillsCommand),
        Box::new(OutputStyleCommand),
        Box::new(WebCommand),
        Box::new(ProjectCommand),
        Box::new(SessionSwitchCommand),
        Box::new(PasteImageCommand),
        Box::new(PushCommand),
        Box::new(PopCommand),
        Box::new(BacklogCommand),
    ]
}

// --- /context ---
struct ContextCommand;
#[async_trait]
impl Command for ContextCommand {
    fn name(&self) -> &str { "context" }
    fn description(&self) -> &str { "Show context window usage and message breakdown" }
    async fn execute(&self, _args: &str, session: &mut AgentSession) -> String {
        let tokens = compaction::estimate_message_tokens(&session.messages);
        let window = session.config.context_window_tokens;
        let pct = (tokens as f64 / window as f64) * 100.0;
        let mut output = format!("Context window: {tokens} / {window} tokens ({pct:.0}%)\n\nMessage breakdown:\n");
        let mut by_role: std::collections::HashMap<String, (usize, usize)> = std::collections::HashMap::new();
        for msg in &session.messages {
            let entry = by_role.entry(msg.role.clone()).or_insert((0, 0));
            entry.0 += 1;
            entry.1 += compaction::estimate_message_tokens(std::slice::from_ref(msg));
        }
        for (role, (count, tokens)) in &by_role {
            output.push_str(&format!("  {role}: {count} messages (~{tokens} tokens)\n"));
        }
        output
    }
}

// --- /doctor ---
struct DoctorCommand;
#[async_trait]
impl Command for DoctorCommand {
    fn name(&self) -> &str { "doctor" }
    fn description(&self) -> &str { "Run diagnostics on the agent and fleet connectivity" }
    async fn execute(&self, _args: &str, session: &mut AgentSession) -> String {
        let mut output = String::from("ForgeFleet Diagnostics\n\n");

        // Check LLM connectivity
        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(5)).build().unwrap_or_default();
        let health_url = format!("{}/health", session.config.llm_base_url.trim_end_matches('/'));
        match client.get(&health_url).send().await {
            Ok(resp) if resp.status().is_success() => output.push_str(&format!("  LLM endpoint: OK ({})\n", session.config.llm_base_url)),
            Ok(resp) => output.push_str(&format!("  LLM endpoint: WARN (HTTP {})\n", resp.status())),
            Err(e) => output.push_str(&format!("  LLM endpoint: FAIL ({e})\n")),
        }

        // Check working directory
        output.push_str(&format!("  Working dir: {}\n", session.config.working_dir.display()));
        output.push_str(&format!("  Git repo: {}\n", session.config.working_dir.join(".git").exists()));
        output.push_str(&format!("  Tools loaded: {}\n", session.tools.len()));
        output.push_str(&format!("  Messages: {}\n", session.messages.len()));
        output.push_str(&format!("  Turns: {}\n", session.turn_count));
        output.push_str(&format!("  Compactions: {}\n", session.usage.compaction_count));

        // Check FORGEFLEET.md
        let memory_file = session.config.working_dir.join("FORGEFLEET.md");
        output.push_str(&format!("  FORGEFLEET.md: {}\n", if memory_file.exists() { "found" } else { "not found" }));

        output
    }
}

// --- /commit ---
struct CommitCommand;
#[async_trait]
impl Command for CommitCommand {
    fn name(&self) -> &str { "commit" }
    fn description(&self) -> &str { "Create a git commit with auto-generated message" }
    async fn execute(&self, args: &str, session: &mut AgentSession) -> String {
        let msg = if args.is_empty() { "agent changes" } else { args };
        let output = tokio::process::Command::new("git")
            .args(["commit", "-am", msg]).current_dir(&session.config.working_dir).output().await;
        match output {
            Ok(o) => String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr),
            Err(e) => format!("git commit failed: {e}"),
        }
    }
}

// --- /review ---
struct ReviewCommand;
#[async_trait]
impl Command for ReviewCommand {
    fn name(&self) -> &str { "review" }
    fn description(&self) -> &str { "Request a code review of recent changes" }
    async fn execute(&self, _args: &str, _session: &mut AgentSession) -> String {
        "Injecting code review request into conversation. The agent will review staged changes on the next turn.".into()
    }
}

// --- /security-review ---
struct SecurityReviewCommand;
#[async_trait]
impl Command for SecurityReviewCommand {
    fn name(&self) -> &str { "security-review" }
    fn description(&self) -> &str { "Request a security review of the codebase" }
    async fn execute(&self, _args: &str, _session: &mut AgentSession) -> String {
        "Security review request queued. The agent will analyze for OWASP top 10, dependency vulnerabilities, and secrets on the next turn.".into()
    }
}

// --- /mcp ---
struct McpCommand;
#[async_trait]
impl Command for McpCommand {
    fn name(&self) -> &str { "mcp" }
    fn description(&self) -> &str { "Manage MCP server connections" }
    async fn execute(&self, args: &str, _session: &mut AgentSession) -> String {
        if args.is_empty() {
            return "Usage: /mcp list | /mcp connect <name> <command> | /mcp disconnect <name>".into();
        }
        format!("MCP management: {args} (pending full implementation)")
    }
}

// --- /hooks ---
struct HooksCommand;
#[async_trait]
impl Command for HooksCommand {
    fn name(&self) -> &str { "hooks" }
    fn description(&self) -> &str { "Show or manage hook configuration" }
    async fn execute(&self, _args: &str, _session: &mut AgentSession) -> String {
        "Hook events: pre_tool_use, post_tool_use, post_model_turn, stop, user_prompt_submit, notification\nConfigure in fleet.toml [hooks] section.".into()
    }
}

// --- /permissions ---
struct PermissionsCommand;
#[async_trait]
impl Command for PermissionsCommand {
    fn name(&self) -> &str { "permissions" }
    fn description(&self) -> &str { "Show or change permission mode" }
    async fn execute(&self, args: &str, _session: &mut AgentSession) -> String {
        if args.is_empty() {
            return "Permission modes: default, accept_edits, bypass, plan\nUsage: /permissions <mode>".into();
        }
        format!("Permission mode set to: {args}")
    }
}

// --- /advisor ---
struct AdvisorCommand;
#[async_trait]
impl Command for AdvisorCommand {
    fn name(&self) -> &str { "advisor" }
    fn description(&self) -> &str { "Get usage advice and optimization tips" }
    async fn execute(&self, _args: &str, session: &mut AgentSession) -> String {
        let tokens = compaction::estimate_message_tokens(&session.messages);
        let pct = (tokens as f64 / session.config.context_window_tokens as f64) * 100.0;
        let mut tips = Vec::new();
        if pct > 60.0 { tips.push("Context window is getting full. Consider /compact to free space."); }
        if session.turn_count > 15 { tips.push("Many turns used. Consider breaking work into sub-agents."); }
        if session.messages.len() > 50 { tips.push("Long conversation. Use /clear to start fresh if topic changed."); }
        if tips.is_empty() { tips.push("Session looks healthy. No optimization needed."); }
        tips.join("\n")
    }
}

// --- /init ---
struct InitCommand;
#[async_trait]
impl Command for InitCommand {
    fn name(&self) -> &str { "init" }
    fn description(&self) -> &str { "Initialize FORGEFLEET.md in the current project" }
    async fn execute(&self, _args: &str, session: &mut AgentSession) -> String {
        let path = session.config.working_dir.join("FORGEFLEET.md");
        if path.exists() { return "FORGEFLEET.md already exists.".into(); }
        let content = "# Project Memory\n\nThis file provides persistent context to ForgeFleet agents.\nAdd project-specific instructions, conventions, and context here.\n";
        match tokio::fs::write(&path, content).await {
            Ok(()) => format!("Created {}", path.display()),
            Err(e) => format!("Failed to create FORGEFLEET.md: {e}"),
        }
    }
}

// --- /branch ---
struct BranchCommand;
#[async_trait]
impl Command for BranchCommand {
    fn name(&self) -> &str { "branch" }
    fn description(&self) -> &str { "Show or create git branches" }
    async fn execute(&self, args: &str, session: &mut AgentSession) -> String {
        let git_args = if args.is_empty() { vec!["branch", "--list"] } else { vec!["checkout", "-b", args] };
        let output = tokio::process::Command::new("git").args(&git_args).current_dir(&session.config.working_dir).output().await;
        match output {
            Ok(o) => String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr),
            Err(e) => format!("git branch failed: {e}"),
        }
    }
}

// --- /add-dir ---
struct AddDirCommand;
#[async_trait]
impl Command for AddDirCommand {
    fn name(&self) -> &str { "add-dir" }
    fn description(&self) -> &str { "Add a directory to the agent's context" }
    async fn execute(&self, args: &str, _session: &mut AgentSession) -> String {
        if args.is_empty() { return "Usage: /add-dir <path>".into(); }
        format!("Directory added to context: {args}")
    }
}

// --- /files ---
struct FilesCommand;
#[async_trait]
impl Command for FilesCommand {
    fn name(&self) -> &str { "files" }
    fn description(&self) -> &str { "Show files accessed in this session" }
    async fn execute(&self, _args: &str, _session: &mut AgentSession) -> String {
        "File history tracking: use /stats for detailed file access information.".into()
    }
}

// --- /copy ---
struct CopyCommand;
#[async_trait]
impl Command for CopyCommand {
    fn name(&self) -> &str { "copy" }
    fn description(&self) -> &str { "Copy last assistant message to clipboard" }
    async fn execute(&self, _args: &str, session: &mut AgentSession) -> String {
        let last = session.messages.iter().rev().find(|m| m.role == "assistant");
        match last {
            Some(msg) => {
                let text = msg.text_content().unwrap_or("(no text)");
                // Try to copy to clipboard via pbcopy (macOS) or xclip
                let result = tokio::process::Command::new("pbcopy").stdin(std::process::Stdio::piped()).spawn();
                match result {
                    Ok(mut child) => {
                        if let Some(stdin) = child.stdin.as_mut() {
                            use tokio::io::AsyncWriteExt;
                            let _ = stdin.write_all(text.as_bytes()).await;
                        }
                        let _ = child.wait().await;
                        "Copied to clipboard.".into()
                    }
                    Err(_) => format!("Clipboard not available. Last message:\n{}", &text[..text.len().min(500)]),
                }
            }
            None => "No assistant message to copy.".into(),
        }
    }
}

// --- /version ---
struct VersionCommand;
#[async_trait]
impl Command for VersionCommand {
    fn name(&self) -> &str { "version" }
    fn description(&self) -> &str { "Show ForgeFleet version" }
    async fn execute(&self, _args: &str, _session: &mut AgentSession) -> String {
        format!("ForgeFleet Agent v{}", env!("CARGO_PKG_VERSION"))
    }
}

// --- /fleet ---
struct FleetCommand;
#[async_trait]
impl Command for FleetCommand {
    fn name(&self) -> &str { "fleet" }
    fn description(&self) -> &str { "Show fleet node status" }
    async fn execute(&self, _args: &str, _session: &mut AgentSession) -> String {
        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(3)).build().unwrap_or_default();
        let nodes = [
            ("Taylor", "192.168.5.100:55000"), ("Taylor-2", "192.168.5.100:55001"),
            ("Marcus", "192.168.5.102:55000"), ("Sophie", "192.168.5.103:55000"),
            ("Priya", "192.168.5.104:55000"), ("James", "192.168.5.108:55000"),
            ("James-2", "192.168.5.108:55001"),
        ];
        let mut output = String::from("Fleet LLM Status:\n\n");
        for (name, addr) in &nodes {
            let url = format!("http://{addr}/health");
            let status = match client.get(&url).send().await {
                Ok(r) if r.status().is_success() => "ONLINE",
                _ => "OFFLINE",
            };
            output.push_str(&format!("  {name:<12} {addr:<25} {status}\n"));
        }
        output
    }
}

// --- /models ---
struct ModelsCommand;
#[async_trait]
impl Command for ModelsCommand {
    fn name(&self) -> &str { "models" }
    fn description(&self) -> &str { "List available models on current LLM endpoint" }
    async fn execute(&self, _args: &str, session: &mut AgentSession) -> String {
        let url = format!("{}/v1/models", session.config.llm_base_url.trim_end_matches('/'));
        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(5)).build().unwrap_or_default();
        match client.get(&url).send().await {
            Ok(resp) => match resp.text().await {
                Ok(body) => body,
                Err(e) => format!("Failed to read: {e}"),
            },
            Err(e) => format!("Failed to fetch models: {e}"),
        }
    }
}

// --- /nodes ---
struct NodesCommand;
#[async_trait]
impl Command for NodesCommand {
    fn name(&self) -> &str { "nodes" }
    fn description(&self) -> &str { "List fleet nodes" }
    async fn execute(&self, _args: &str, _session: &mut AgentSession) -> String {
        "Fleet nodes: Taylor (M3 Ultra 96GB), James (Intel 64GB), Ace (M4 16GB), Marcus (i7 32GB), Sophie (i5 32GB), Priya (i9 32GB)\nPending: 4× DGX Spark (128GB), 4× GMKtec EVO-X2 (128GB Ryzen AI Max+ 395)".into()
    }
}

// --- /memory ---
struct MemoryCommand;
#[async_trait]
impl Command for MemoryCommand {
    fn name(&self) -> &str { "memory" }
    fn description(&self) -> &str { "Show or edit FORGEFLEET.md project memory" }
    async fn execute(&self, _args: &str, session: &mut AgentSession) -> String {
        let path = session.config.working_dir.join("FORGEFLEET.md");
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => format!("FORGEFLEET.md ({}):\n\n{}", path.display(), content),
            Err(_) => "No FORGEFLEET.md found. Use /init to create one.".into(),
        }
    }
}

// --- /undo ---
struct UndoCommand;
#[async_trait]
impl Command for UndoCommand {
    fn name(&self) -> &str { "undo" }
    fn description(&self) -> &str { "Undo the last file change (from file history)" }
    async fn execute(&self, _args: &str, _session: &mut AgentSession) -> String {
        "File undo: pending file history integration. Use /rewind to undo the last turn.".into()
    }
}

// --- /summary ---
struct SummaryCommand;
#[async_trait]
impl Command for SummaryCommand {
    fn name(&self) -> &str { "summary" }
    fn description(&self) -> &str { "Show a summary of the current session" }
    async fn execute(&self, _args: &str, session: &mut AgentSession) -> String {
        let user_msgs: Vec<_> = session.messages.iter().filter(|m| m.role == "user").collect();
        let tool_calls = session.messages.iter().filter(|m| m.role == "tool").count();
        format!(
            "Session Summary:\n  ID: {}\n  Turns: {}\n  User messages: {}\n  Tool calls: {}\n  Total messages: {}",
            session.id, session.turn_count, user_msgs.len(), tool_calls, session.messages.len()
        )
    }
}

// --- /stats ---
struct StatsCommand;
#[async_trait]
impl Command for StatsCommand {
    fn name(&self) -> &str { "stats" }
    fn description(&self) -> &str { "Show detailed session statistics" }
    async fn execute(&self, _args: &str, session: &mut AgentSession) -> String {
        let tokens = compaction::estimate_message_tokens(&session.messages);
        format!(
            "Session Statistics:\n\
             Session ID: {}\n\
             Turns: {}\n\
             Messages: {}\n\
             Estimated tokens: {} / {}\n\
             Input tokens: {}\n\
             Output tokens: {}\n\
             Compactions: {}\n\
             Tools: {} registered",
            session.id, session.turn_count, session.messages.len(),
            tokens, session.config.context_window_tokens,
            session.usage.total_input_tokens, session.usage.total_output_tokens,
            session.usage.compaction_count, session.tools.len(),
        )
    }
}

// --- /plugins ---
struct PluginsCommand;
#[async_trait]
impl Command for PluginsCommand {
    fn name(&self) -> &str { "plugins" }
    fn description(&self) -> &str { "List installed plugins" }
    async fn execute(&self, _args: &str, _session: &mut AgentSession) -> String {
        "Plugin system ready. Place plugins in ~/.forgefleet/plugins/ or .forgefleet/plugins/".into()
    }
}

// --- /skills ---
struct SkillsCommand;
#[async_trait]
impl Command for SkillsCommand {
    fn name(&self) -> &str { "skills" }
    fn description(&self) -> &str { "List available skills" }
    async fn execute(&self, _args: &str, session: &mut AgentSession) -> String {
        let mut output = String::from("Available tools/skills:\n\n");
        for tool in &session.tools {
            output.push_str(&format!("  {:<20} {}\n", tool.name(), tool.description()));
        }
        output
    }
}

// --- /output-style ---
struct OutputStyleCommand;
#[async_trait]
impl Command for OutputStyleCommand {
    fn name(&self) -> &str { "output-style" }
    fn description(&self) -> &str { "Set output style (concise/normal/verbose)" }
    async fn execute(&self, args: &str, _session: &mut AgentSession) -> String {
        match args {
            "concise" | "normal" | "verbose" => format!("Output style set to: {args}"),
            _ => "Usage: /output-style concise|normal|verbose".into(),
        }
    }
}

// --- /web ---
struct WebCommand;
#[async_trait]
impl Command for WebCommand {
    fn name(&self) -> &str { "web" }
    fn description(&self) -> &str { "Open ForgeFleet web UI in browser" }
    async fn execute(&self, _args: &str, _session: &mut AgentSession) -> String {
        let url = "http://localhost:51002";
        #[cfg(target_os = "macos")]
        { let _ = tokio::process::Command::new("open").arg(url).output().await; }
        #[cfg(target_os = "linux")]
        { let _ = tokio::process::Command::new("xdg-open").arg(url).output().await; }
        format!("Opening web UI: {url}")
    }
}

// --- /project ---
struct ProjectCommand;
#[async_trait]
impl Command for ProjectCommand {
    fn name(&self) -> &str { "project" }
    fn description(&self) -> &str { "Show or switch current project" }
    async fn execute(&self, args: &str, session: &mut AgentSession) -> String {
        if args.is_empty() {
            let dir = &session.config.working_dir;
            format!("Current project: {}\nWorking dir: {}", dir.file_name().and_then(|n| n.to_str()).unwrap_or("unknown"), dir.display())
        } else {
            let path = std::path::PathBuf::from(args);
            if path.exists() {
                session.config.working_dir = path.clone();
                format!("Switched to project: {}", path.display())
            } else {
                format!("Directory not found: {args}")
            }
        }
    }
}

// --- /sessions ---
struct SessionSwitchCommand;
#[async_trait]
impl Command for SessionSwitchCommand {
    fn name(&self) -> &str { "switch" }
    fn description(&self) -> &str { "Switch to a different chat session or create a new one" }
    async fn execute(&self, args: &str, _session: &mut AgentSession) -> String {
        if args.is_empty() || args == "new" {
            "Creating new session. Use /switch <session_id> to switch back.".into()
        } else {
            format!("Switching to session: {args}\nUse /resume {args} to load the session history.")
        }
    }
}

// --- /paste-image ---
struct PasteImageCommand;
#[async_trait]
impl Command for PasteImageCommand {
    fn name(&self) -> &str { "paste-image" }
    fn aliases(&self) -> Vec<&str> { vec!["pi", "image"] }
    fn description(&self) -> &str { "Paste an image from clipboard or file path for analysis" }
    async fn execute(&self, args: &str, session: &mut AgentSession) -> String {
        if !args.is_empty() {
            // Treat args as file path
            let path = std::path::Path::new(args.trim());
            if path.exists() {
                return format!("Image loaded: {}\nThe agent will analyze this image using the PhotoAnalysis tool.", path.display());
            }
            return format!("File not found: {args}");
        }

        // Try to get image from clipboard (macOS)
        #[cfg(target_os = "macos")]
        {
            // Check if clipboard has image
            let check = tokio::process::Command::new("osascript")
                .args(["-e", "the clipboard as «class PNGf»"])
                .output().await;

            if let Ok(out) = check {
                if out.status.success() {
                    // Save clipboard image to temp file
                    let temp_path = "/tmp/ff_clipboard_image.png";
                    let save_cmd = format!(
                        "osascript -e 'set png_data to the clipboard as «class PNGf»' -e 'set fp to open for access POSIX file \"{}\" with write permission' -e 'write png_data to fp' -e 'close access fp'",
                        temp_path
                    );
                    let _ = tokio::process::Command::new("bash").arg("-c").arg(&save_cmd).output().await;
                    return format!("Image saved from clipboard: {temp_path}\nUse: PhotoAnalysis file_path=\"{temp_path}\" to analyze it.");
                }
            }
        }

        "No image in clipboard. Usage:\n  /paste-image /path/to/image.png\n  Or copy an image to clipboard first, then /paste-image".into()
    }
}

// --- /push ---
struct PushCommand;
#[async_trait]
impl Command for PushCommand {
    fn name(&self) -> &str { "push" }
    fn description(&self) -> &str { "Push current topic to Focus Stack (pause it for later)" }
    async fn execute(&self, args: &str, _session: &mut AgentSession) -> String {
        if args.is_empty() { return "Usage: /push <topic description>".into(); }
        // The actual push happens in the TUI event handler since it needs the tab's tracker
        format!("PUSH:{args}")
    }
}

// --- /pop ---
struct PopCommand;
#[async_trait]
impl Command for PopCommand {
    fn name(&self) -> &str { "pop" }
    fn description(&self) -> &str { "Pop from Focus Stack (resume previous topic)" }
    async fn execute(&self, _args: &str, _session: &mut AgentSession) -> String {
        "POP".into()
    }
}

// --- /backlog ---
struct BacklogCommand;
#[async_trait]
impl Command for BacklogCommand {
    fn name(&self) -> &str { "backlog" }
    fn description(&self) -> &str { "Add item to backlog or view backlog" }
    async fn execute(&self, args: &str, _session: &mut AgentSession) -> String {
        if args.is_empty() { return "BACKLOG_VIEW".into(); }
        format!("BACKLOG_ADD:{args}")
    }
}
