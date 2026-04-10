//! `ff` — ForgeFleet unified CLI.
//!
//! Usage:
//!   ff                          — interactive TUI agent
//!   ff "fix the bug"            — headless agent run
//!   ff start                    — start ForgeFleet daemon
//!   ff status / nodes / models / health / config / version

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, fs};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use crossterm::event::{self, Event, KeyCode, KeyModifiers, MouseEventKind, EnableMouseCapture, DisableMouseCapture};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::execute;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use serde::{Deserialize, Serialize};

use ff_agent::agent_loop::{AgentEvent, AgentSession, AgentSessionConfig};
use ff_agent::commands::CommandRegistry;
use ff_terminal::app::{App, PORT_WEB};
use ff_terminal::render;

const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

#[derive(Debug, Parser)]
#[command(name = "ff", version, about = "ForgeFleet — distributed AI agent platform")]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[arg(long, global = true)]
    llm: Option<String>,
    #[arg(short = 'm', long, global = true)]
    model: Option<String>,
    #[arg(long, global = true)]
    cwd: Option<PathBuf>,
    /// Attach an image to the prompt (for multimodal models)
    #[arg(long, short = 'i', global = true)]
    image: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(trailing_var_arg = true)]
    prompt: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start ForgeFleet (daemon + LLM + web)
    Start { #[arg(long, default_value_t = false)] leader: bool },
    /// Stop ForgeFleet daemon
    Stop,
    Status, Nodes, Models, Health,
    Proxy { #[arg(long, default_value_t = 4000)] port: u16 },
    Discover { #[arg(long, default_value = "192.168.5.0/24")] subnet: String },
    Config { #[command(subcommand)] command: ConfigCommand },
    Version,
    Run { prompt: String, #[arg(long, default_value = "text")] output: String, #[arg(long, default_value_t = 30)] max_turns: u32 },
    /// Run with supervisor — auto-detect failures, fix, and retry
    Supervise { prompt: String, #[arg(long, default_value_t = 3)] max_attempts: u32 },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand { Show, Set { key: String, value: String } }

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = resolve_config_path(cli.config)?;
    let llm = cli.llm.or_else(|| env::var("FORGEFLEET_LLM_URL").ok())
        .unwrap_or(detect_llm_from_db_or_local(&config_path).await);
    let mut model = cli.model.or_else(|| env::var("FORGEFLEET_MODEL").ok()).unwrap_or_else(|| "auto".into());

    // If model is "auto", query the LLM server for its actual model name
    if model == "auto" {
        let detect_url = format!("{}/v1/models", llm.trim_end_matches('/'));
        match reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default()
            .get(&detect_url)
            .send().await
        {
            Ok(resp) => {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    if let Some(id) = body.get("data")
                        .and_then(|d| d.as_array())
                        .and_then(|arr| arr.last())
                        .and_then(|m| m.get("id"))
                        .and_then(|id| id.as_str())
                    {
                        model = id.to_string();
                    }
                }
            }
            Err(_) => {
                // Fallback: infer from port
                if llm.contains("51005") {
                    model = "ForgeFleet-LoRA".into();
                }
            }
        }
    }
    let working_dir = cli.cwd.unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));

    let agent_config = AgentSessionConfig {
        model, llm_base_url: llm, working_dir: working_dir.clone(),
        system_prompt: None, max_turns: 30,
        image_path: cli.image,
        ..Default::default()
    };

    match cli.command {
        Some(Command::Start { leader }) => handle_start(leader, &config_path, &working_dir).await,
        Some(Command::Stop) => handle_stop().await,
        Some(Command::Status) => handle_status(&config_path),
        Some(Command::Nodes) => handle_nodes(&config_path),
        Some(Command::Models) => handle_models(&agent_config).await,
        Some(Command::Health) => handle_health(&agent_config).await,
        Some(Command::Proxy { port }) => { println!("{CYAN}▶ Starting LLM proxy on 0.0.0.0:{port}{RESET}"); Ok(()) }
        Some(Command::Discover { subnet }) => { println!("{CYAN}▶ Discovering nodes on {subnet}{RESET}"); Ok(()) }
        Some(Command::Config { command }) => handle_config(command, &config_path),
        Some(Command::Version) => { println!("ff {}", env!("CARGO_PKG_VERSION")); Ok(()) }
        Some(Command::Run { prompt, output, max_turns }) => {
            let mut cfg = agent_config; cfg.max_turns = max_turns;
            run_headless(&prompt, cfg, &output).await
        }
        Some(Command::Supervise { prompt, max_attempts }) => {
            let sup_config = ff_agent::supervisor::SupervisorConfig {
                max_attempts,
                ..Default::default()
            };
            println!("{CYAN}▶ Supervisor mode: up to {max_attempts} attempts{RESET}");
            println!("{CYAN}  Task: {}{RESET}", &prompt[..prompt.len().min(80)]);
            println!();

            let result = ff_agent::supervisor::supervise(&prompt, agent_config, sup_config).await;

            if result.success {
                println!("{GREEN}✓ Task completed on attempt {}/{}{RESET}", result.attempts, max_attempts);
            } else {
                println!("{RED}✗ Task failed after {} attempts{RESET}", result.attempts);
            }

            for d in &result.diagnoses {
                println!("  Attempt {}: {} → {}", d.attempt, d.failure_type, d.fix_applied);
            }

            println!();
            println!("{}", &result.final_output[..result.final_output.len().min(500)]);
            Ok(())
        }
        None => {
            let prompt_text = cli.prompt.join(" ");
            if !prompt_text.is_empty() { run_headless(&prompt_text, agent_config, "text").await }
            else { run_tui(agent_config).await }
        }
    }
}

// ─── TUI Mode ──────────────────────────────────────────────────────────────

async fn run_tui(config: AgentSessionConfig) -> Result<()> {
    // Set up panic hook to restore terminal on crash
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        original_hook(info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(config.clone());
    let commands = CommandRegistry::new();
    let mut command_list: Vec<(&str, &str)> = commands.list();
    // Add built-in TUI commands
    command_list.push(("/new", "Start a new session tab"));
    command_list.push(("/memory", "Search across all memory layers: /memory <query>"));
    command_list.push(("/search", "Search memory: /search <query>"));
    command_list.push(("/help", "Show available commands"));
    command_list.sort();

    // Async fleet health check on startup
    check_fleet_health(&mut app).await;

    // Pre-load three-brain memory context
    let brain_ctx = ff_agent::brain::BrainLoader::load_for_dir(&config.working_dir).await;
    app.brain_status = Some(ff_agent::brain::BrainLoadedStatus::from(&brain_ctx));

    // Initialize Hive Mind
    let hive = ff_agent::hive_sync::HiveSync::new();
    hive.ensure_initialized().await;
    let sync_result = hive.pull().await;
    if let Some(status) = &mut app.brain_status {
        status.hive_synced_at = sync_result.last_sync_at;
    }

    let result = run_event_loop(&mut terminal, &mut app, config, &commands, &command_list).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    result
}

/// Check fleet node health on startup.
async fn check_fleet_health(app: &mut App) {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(2)).build().unwrap_or_default();
    for node in &mut app.fleet_nodes {
        // Check daemon
        let daemon_url = format!("http://{}:{}/health", node.ip, ff_terminal::app::PORT_DAEMON);
        node.daemon_online = client.get(&daemon_url).send().await
            .map(|r| r.status().is_success()).unwrap_or(false);

        // Check each model endpoint
        for model in &mut node.models {
            let model_url = format!("http://{}:{}/health", node.ip, model.port);
            model.online = client.get(&model_url).send().await
                .map(|r| r.status().is_success()).unwrap_or(false);
        }
    }
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    config: AgentSessionConfig,
    commands: &CommandRegistry,
    command_list: &[(&str, &str)],
) -> Result<()> {
    // Channel for async agent communication
    let mut agent_handle: Option<tokio::task::JoinHandle<(AgentSession, ff_agent::agent_loop::AgentOutcome)>> = None;
    let mut event_rx: Option<tokio::sync::mpsc::UnboundedReceiver<AgentEvent>> = None;

    loop {
        // Process agent events if running
        if let Some(rx) = &mut event_rx {
            while let Ok(ev) = rx.try_recv() {
                app.handle_event(ev);
            }
        }

        // Check if agent finished
        if let Some(handle) = &agent_handle {
            if handle.is_finished() {
                if let Some(handle) = agent_handle.take() {
                    if let Ok((session, _)) = handle.await {
                        app.tab_mut().session_id = session.id.to_string();
                        app.tab_mut().session = Some(session);
                    }
                }
                event_rx = None;
                app.tab_mut().is_running = false;
                app.tab_mut().status = "Ready".into();

                // Auto-send queued message if one was waiting
                if let Some(queued) = app.tab_mut().queued_message.take() {
                    let prompt = detect_dropped_content(&queued);
                    // Show user message
                    app.tab_mut().input.text = queued;
                    app.submit_input();
                    // Start agent with queued message
                    let mut session = app.tab_mut().session.take().unwrap_or_else(|| AgentSession::new(config.clone()));
                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
                    let handle = tokio::spawn(async move {
                        let outcome = session.run(&prompt, Some(tx)).await;
                        (session, outcome)
                    });
                    agent_handle = Some(handle);
                    event_rx = Some(rx);
                }
            }
        }

        // Render
        app.frame += 1;
        terminal.draw(|frame| render::render(frame, app))?;

        // Poll events
        if event::poll(Duration::from_millis(50))? {
            let ev = event::read()?;

            // Handle mouse scroll for chat scrolling
            if let Event::Mouse(mouse) = &ev {
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        let tab = app.tab_mut();
                        tab.auto_scroll = false;
                        tab.scroll_offset = tab.scroll_offset.saturating_add(3);
                    }
                    MouseEventKind::ScrollDown => {
                        let tab = app.tab_mut();
                        if tab.scroll_offset > 0 {
                            tab.scroll_offset = tab.scroll_offset.saturating_sub(3);
                        }
                        if tab.scroll_offset == 0 {
                            tab.auto_scroll = true;
                        }
                    }
                    _ => {}
                }
            }

            if let Event::Key(key) = ev {
                match (key.code, key.modifiers) {
                    // Esc: cancel running agent (don't quit)
                    (KeyCode::Esc, _) if app.tab().is_running => {
                        if let Some(handle) = agent_handle.take() {
                            handle.abort();
                        }
                        event_rx = None;
                        app.tab_mut().is_running = false;
                        app.tab_mut().status = "Cancelled".into();
                        app.tab_mut().messages.push(ff_terminal::messages::render_status("Agent cancelled by user"));
                    }

                    // Ctrl+C: quit (only when not running, otherwise cancel)
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        if app.tab().is_running {
                            if let Some(handle) = agent_handle.take() { handle.abort(); }
                            event_rx = None;
                            app.tab_mut().is_running = false;
                            app.tab_mut().status = "Cancelled".into();
                        } else {
                            app.should_quit = true;
                        }
                    }
                    (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                        app.should_quit = true;
                    }

                    // Enter: accept suggestion if active, otherwise submit
                    (KeyCode::Enter, KeyModifiers::NONE) => {
                        // If a suggestion is selected, accept it first
                        if app.tab_mut().input.suggestion_index.is_some() {
                            app.tab_mut().input.accept_suggestion();
                            continue;
                        }

                        if app.tab_mut().input.text.trim().is_empty() { continue; }

                        let trimmed = app.tab_mut().input.text.trim().to_string();
                        if trimmed == "/exit" || trimmed == "/quit" {
                            app.should_quit = true;
                            continue;
                        }

                        // If running, queue the message for after the agent finishes
                        if app.tab().is_running {
                            app.tab_mut().queued_message = Some(trimmed.clone());
                            app.tab_mut().messages.push(ff_terminal::messages::render_status(
                                &format!("Queued: \"{}\" — will send when agent finishes.", if trimmed.len() > 60 { format!("{}...", &trimmed[..60]) } else { trimmed })
                            ));
                            app.tab_mut().input.text.clear();
                            app.tab_mut().input.cursor = 0;
                            continue;
                        }

                        // Built-in navigation commands
                        // Memory search command
                        if trimmed.starts_with("/memory ") || trimmed.starts_with("/search ") {
                            let query = trimmed.split_once(' ').map(|(_, q)| q).unwrap_or("");
                            if !query.is_empty() {
                                let results = ff_agent::brain::search_all(query, &config.working_dir).await;
                                if results.is_empty() {
                                    app.tab_mut().messages.push(ff_terminal::messages::render_status(
                                        &format!("No memory entries match \"{query}\"")
                                    ));
                                } else {
                                    let mut output = format!("Found {} results for \"{}\":\n", results.len(), query);
                                    for r in results.iter().take(10) {
                                        output.push_str(&format!("\n[{}] ({}) {}", r.layer, r.category, r.content));
                                    }
                                    app.tab_mut().messages.push(ff_terminal::messages::render_assistant_message(&output));
                                }
                            }
                            app.tab_mut().input.text.clear();
                            app.tab_mut().input.cursor = 0;
                            continue;
                        }

                        if trimmed == "/new" || trimmed == "/new-session" {
                            let n = app.tabs.len() + 1;
                            app.tabs.push(ff_terminal::app::SessionTab::new(&format!("Session {n}")));
                            app.active_tab = app.tabs.len() - 1;
                            app.tab_mut().messages.push(ff_terminal::messages::render_status(
                                "New session created. Use Ctrl+N/P to switch tabs, Ctrl+W to close."
                            ));
                            app.tab_mut().input.text.clear();
                            app.tab_mut().input.cursor = 0;
                            continue;
                        }

                        // Slash commands
                        if trimmed.starts_with('/') {
                            let mut session = app.tab_mut().session.take().unwrap_or_else(|| AgentSession::new(config.clone()));
                            if let Some(output) = commands.try_execute(&trimmed, &mut session).await {
                                // Handle Focus Stack / Backlog commands
                                if output.starts_with("PUSH:") {
                                    let topic = &output[5..];
                                    app.tab_mut().push_focus(topic, "", ff_agent::focus_stack::PushReason::Explicit);
                                    app.tab_mut().messages.push(ff_terminal::messages::render_status(&format!("Pushed to Focus Stack: {topic}")));
                                } else if output == "POP" {
                                    if let Some(topic) = app.tab_mut().pop_focus() {
                                        app.tab_mut().messages.push(ff_terminal::messages::render_status(&format!("Resumed from Focus Stack: {topic}")));
                                    } else {
                                        app.tab_mut().messages.push(ff_terminal::messages::render_status("Focus Stack is empty"));
                                    }
                                } else if output.starts_with("BACKLOG_ADD:") {
                                    let item = &output[12..];
                                    app.tab_mut().add_backlog(item, "", ff_agent::focus_stack::BacklogPriority::Medium);
                                    app.tab_mut().messages.push(ff_terminal::messages::render_status(&format!("Added to Backlog: {item}")));
                                } else if output == "BACKLOG_VIEW" {
                                    let items = app.tab().tracker.backlog.items();
                                    if items.is_empty() {
                                        app.tab_mut().messages.push(ff_terminal::messages::render_status("Backlog is empty"));
                                    } else {
                                        let list: Vec<String> = items.iter().enumerate().map(|(i, item)| format!("  {}. {}", i+1, item.title)).collect();
                                        app.tab_mut().messages.push(ff_terminal::messages::render_assistant_message(&format!("Backlog:\n{}", list.join("\n"))));
                                    }
                                } else {
                                    app.tab_mut().messages.push(ff_terminal::messages::render_user_message(&trimmed));
                                    app.tab_mut().messages.push(ff_terminal::messages::render_assistant_message(&output));
                                }
                                app.tab_mut().input.submit();
                            }
                            app.tab_mut().session = Some(session);
                            continue;
                        }

                        // Detect dragged file/folder paths and auto-contextualize
                        let prompt = detect_dropped_content(&trimmed);

                        // Agent run
                        app.submit_input();
                        let mut session = app.tab_mut().session.take().unwrap_or_else(|| AgentSession::new(config.clone()));
                        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();

                        let handle = tokio::spawn(async move {
                            let outcome = session.run(&prompt, Some(tx)).await;
                            (session, outcome)
                        });

                        agent_handle = Some(handle);
                        event_rx = Some(rx);
                    }

                    // Text editing — ALWAYS works (even while running)
                    (KeyCode::Tab, _) => {
                        app.tab_mut().input.compute_suggestions(command_list);
                        app.tab_mut().input.next_suggestion();
                    }
                    (KeyCode::Char(c), mods) if !mods.contains(KeyModifiers::CONTROL) && !mods.contains(KeyModifiers::ALT) => {
                        app.tab_mut().input.insert_char(c);
                        if app.tab_mut().input.text.starts_with('/') {
                            app.tab_mut().input.compute_suggestions(command_list);
                        }
                    }
                    // Tab management
                    (KeyCode::Char('t'), KeyModifiers::CONTROL) => { app.new_tab(); }
                    (KeyCode::Char('w'), KeyModifiers::CONTROL) => { app.close_tab(); }
                    // Ctrl+N/P for tab switching (works on macOS, emacs-style)
                    (KeyCode::Char('n'), KeyModifiers::CONTROL) => { app.next_tab(); }
                    (KeyCode::Char('p'), KeyModifiers::CONTROL) => { app.prev_tab(); }

                    // Text editing
                    (KeyCode::Backspace, _) => app.tab_mut().input.backspace(),
                    (KeyCode::Delete, _) => app.tab_mut().input.delete(),
                    (KeyCode::Left, _) => app.tab_mut().input.move_left(),
                    (KeyCode::Right, _) => app.tab_mut().input.move_right(),
                    (KeyCode::Home, _) => app.tab_mut().input.home(),
                    (KeyCode::End, _) => app.tab_mut().input.end(),
                    (KeyCode::Up, _) => app.tab_mut().input.history_up(),
                    (KeyCode::Down, _) => app.tab_mut().input.history_down(),

                    // Scroll
                    (KeyCode::PageUp, _) => { app.tab_mut().auto_scroll = false; app.tab_mut().scroll_offset = app.tab_mut().scroll_offset.saturating_add(10); }
                    (KeyCode::PageDown, _) => {
                        let so = app.tab_mut().scroll_offset;
                        if so > 10 { app.tab_mut().scroll_offset -= 10; } else { app.tab_mut().scroll_offset = 0; app.tab_mut().auto_scroll = true; }
                    }

                    _ => {}
                }
            }
        }

        if app.should_quit { break; }
    }
    Ok(())
}

// ─── Headless Mode ─────────────────────────────────────────────────────────

async fn run_headless(prompt: &str, config: AgentSessionConfig, output_format: &str) -> Result<()> {
    let mut session = AgentSession::new(config);
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let prompt = prompt.to_string();
    let is_json = output_format == "json";

    let handle = tokio::spawn(async move { session.run(&prompt, Some(event_tx)).await });

    let mut events = Vec::new();
    while let Some(event) = event_rx.recv().await {
        if is_json { events.push(event); }
        else {
            match &event {
                AgentEvent::AssistantText { text, .. } => print!("{text}"),
                AgentEvent::ToolStart { tool_name, .. } => eprint!("{YELLOW}⚡ {tool_name}...{RESET} "),
                AgentEvent::ToolEnd { duration_ms, is_error, .. } => {
                    if *is_error { eprintln!("{RED}✗ ({duration_ms}ms){RESET}"); }
                    else { eprintln!("{GREEN}✓ ({duration_ms}ms){RESET}"); }
                }
                AgentEvent::Error { message, .. } => eprintln!("{RED}Error: {message}{RESET}"),
                _ => {}
            }
        }
    }

    let outcome = handle.await?;
    if is_json {
        let result = serde_json::json!({ "outcome": match &outcome {
            ff_agent::agent_loop::AgentOutcome::EndTurn { final_message } => serde_json::json!({"status":"done","message":final_message}),
            ff_agent::agent_loop::AgentOutcome::MaxTurns { partial_message } => serde_json::json!({"status":"max_turns","message":partial_message}),
            ff_agent::agent_loop::AgentOutcome::Error(e) => serde_json::json!({"status":"error","message":e}),
            ff_agent::agent_loop::AgentOutcome::Cancelled => serde_json::json!({"status":"cancelled"}),
        }, "events": events });
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else if let ff_agent::agent_loop::AgentOutcome::EndTurn { final_message } = &outcome {
        if !final_message.is_empty() { println!("{final_message}"); }
    }
    Ok(())
}

async fn handle_stop() -> Result<()> {
    println!("{CYAN}▶ Stopping ForgeFleet{RESET}");

    // Kill forgefleetd
    let kill = tokio::process::Command::new("pkill").args(["-f", "forgefleetd"]).output().await;
    match kill {
        Ok(o) if o.status.success() => println!("  {GREEN}✓ Daemon stopped{RESET}"),
        _ => println!("  {YELLOW}⚠ No daemon process found{RESET}"),
    }

    // Verify
    tokio::time::sleep(Duration::from_secs(1)).await;
    let client = reqwest::Client::builder().timeout(Duration::from_secs(1)).build()?;
    let still_running = client.get(format!("http://127.0.0.1:{}/health", ff_terminal::app::PORT_DAEMON))
        .send().await.map(|r| r.status().is_success()).unwrap_or(false);

    if still_running {
        println!("  {RED}✗ Daemon still running — try: kill $(pgrep forgefleetd){RESET}");
    } else {
        println!("  {GREEN}✓ ForgeFleet stopped{RESET}");
    }
    Ok(())
}

// ─── Helpers ───────────────────────────────────────────────────────────────

/// Detect if input is a dropped file/folder path and wrap with appropriate context.
fn detect_dropped_content(input: &str) -> String {
    let trimmed = input.trim().trim_matches('\'').trim_matches('"');
    let path = std::path::Path::new(trimmed);

    // Only trigger if it looks like an absolute path that exists
    if !trimmed.starts_with('/') || !path.exists() {
        return input.to_string();
    }

    if path.is_dir() {
        format!("I've dropped a folder: {trimmed}\nPlease explore this directory and tell me what's in it. Use Glob and Read to understand the contents.")
    } else {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
        match ext.as_str() {
            // Images
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" => {
                format!("I've dropped an image: {trimmed}\nPlease analyze this image using PhotoAnalysis with file_path=\"{trimmed}\"")
            }
            // Videos
            "mp4" | "mov" | "avi" | "mkv" | "webm" => {
                format!("I've dropped a video: {trimmed}\nPlease analyze this video using VideoAnalysis with file_path=\"{trimmed}\" action=\"info\"")
            }
            // Audio
            "mp3" | "wav" | "flac" | "m4a" | "ogg" => {
                format!("I've dropped an audio file: {trimmed}\nPlease analyze using AudioAnalysis with file_path=\"{trimmed}\" action=\"info\"")
            }
            // PDFs
            "pdf" => {
                format!("I've dropped a PDF: {trimmed}\nPlease extract and summarize the content using PdfExtract with file_path=\"{trimmed}\"")
            }
            // Spreadsheets
            "csv" | "xlsx" | "xls" => {
                format!("I've dropped a spreadsheet: {trimmed}\nPlease read and summarize using SpreadsheetQuery with file_path=\"{trimmed}\" action=\"head\"")
            }
            // Code/text files — just read them
            _ => {
                format!("I've dropped a file: {trimmed}\nPlease read and analyze this file using Read with file_path=\"{trimmed}\"")
            }
        }
    }
}

/// Detect the best LLM endpoint by querying Postgres for fleet nodes + models,
/// then probing each for a healthy connection. Falls back to localhost:55000.
async fn detect_llm_from_db_or_local(config_path: &std::path::Path) -> String {
    // Try to load fleet.toml to get the database URL
    if let Ok(toml_str) = std::fs::read_to_string(config_path) {
        if let Ok(config) = toml::from_str::<ff_core::config::FleetConfig>(&toml_str) {
            let db_url = config.database.url.trim();
            if !db_url.is_empty() {
                // Query Postgres for fleet nodes and their model ports
                if let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
                    .max_connections(1)
                    .acquire_timeout(Duration::from_secs(3))
                    .connect(db_url)
                    .await
                {
                    if let Ok(nodes) = ff_db::pg_list_nodes(&pool).await {
                        // Also get models to find ports
                        let models = ff_db::pg_list_models(&pool).await.unwrap_or_default();

                        // Build (ip, port, cores, supports_tools) pairs
                        // Prefer models that support tool calling (Qwen) over those that don't (Gemma)
                        let mut endpoints: Vec<(String, u16, i32, bool)> = Vec::new();
                        for node in &nodes {
                            let node_models: Vec<_> = models.iter().filter(|m| m.node_name == node.name).collect();
                            if node_models.is_empty() {
                                endpoints.push((node.ip.clone(), 55000, node.cpu_cores, true));
                            } else {
                                for m in node_models {
                                    // Qwen models support tool calling, Gemma does not
                                    let supports_tools = m.family.to_lowercase().contains("qwen");
                                    endpoints.push((node.ip.clone(), m.port as u16, node.cpu_cores, supports_tools));
                                }
                            }
                        }
                        // Sort: tool-calling models first, then by cores descending
                        endpoints.sort_by(|a, b| b.3.cmp(&a.3).then(b.2.cmp(&a.2)));

                        for (ip, port, _, _) in &endpoints {
                            if let Ok(addr) = format!("{ip}:{port}").parse() {
                                if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
                                    tracing::info!(ip = %ip, port, "auto-detected LLM endpoint from database");
                                    return format!("http://{ip}:{port}");
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Fallback: probe localhost
    for port in [55000, 55001, 11434] {
        if let Ok(addr) = format!("127.0.0.1:{port}").parse() {
            if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
                return format!("http://127.0.0.1:{port}");
            }
        }
    }

    "http://localhost:55000".into()
}

fn resolve_config_path(p: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = p { return Ok(p); }
    Ok(PathBuf::from(env::var("HOME").context("HOME not set")?).join(".forgefleet").join("fleet.toml"))
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct FleetConfig {
    #[serde(default)] general: BTreeMap<String, toml::Value>,
    #[serde(default)] nodes: BTreeMap<String, toml::Value>,
    #[serde(default)] models: BTreeMap<String, toml::Value>,
    #[serde(flatten)] extra: BTreeMap<String, toml::Value>,
}

fn load_config(p: &Path) -> Result<FleetConfig> {
    if !p.exists() { return Ok(FleetConfig::default()); }
    Ok(toml::from_str(&fs::read_to_string(p)?)?)
}

async fn handle_start(leader: bool, config_path: &Path, working_dir: &Path) -> Result<()> {
    println!("{CYAN}▶ Starting ForgeFleet{RESET}");
    println!("  Config: {}", config_path.display());
    println!("  Mode:   {}", if leader { "leader" } else { "auto" });
    println!();

    // Check if daemon is already running (check web UI port — only daemon serves this)
    let client = reqwest::Client::builder().timeout(Duration::from_secs(2)).build()?;
    let daemon_running = client.get(format!("http://127.0.0.1:{}/health", ff_terminal::app::PORT_WEB))
        .send().await.map(|r| r.status().is_success()).unwrap_or(false);
    if daemon_running {
        println!("{GREEN}✓ ForgeFleet daemon is already running{RESET}");
        println!("  Daemon:    http://localhost:{}", ff_terminal::app::PORT_DAEMON);
        println!("  Web UI:    http://localhost:{}", ff_terminal::app::PORT_WEB);
        println!("  WebSocket: ws://localhost:{}", ff_terminal::app::PORT_WS);
        return Ok(());
    }

    // Step 1: Find and start LLM server
    println!("{YELLOW}1/4{RESET} Checking LLM server...");
    let llm_running = std::net::TcpStream::connect_timeout(
        &"127.0.0.1:51000".parse().unwrap(), Duration::from_millis(500)).is_ok();

    if llm_running {
        println!("  {GREEN}✓ LLM server already running on :51000{RESET}");
    } else {
        println!("  {YELLOW}⚠ No LLM server detected locally{RESET}");
        println!("  Start one with: ollama serve & ollama run qwen2.5-coder:32b");
        println!("  Or: llama-server -m /path/to/model.gguf --host 0.0.0.0 --port 51000 --ctx-size 32768");
    }

    // Step 2: Start ForgeFleet daemon
    println!("{YELLOW}2/4{RESET} Starting ForgeFleet daemon...");

    // Find the forgefleetd binary
    let daemon_binary = find_daemon_binary(working_dir);
    match daemon_binary {
        Some(bin) => {
            let mut cmd = tokio::process::Command::new(&bin);
            cmd.arg("--config").arg(config_path);
            if leader { cmd.arg("start").arg("--leader"); }

            // Spawn as background process
            match cmd.stdout(std::process::Stdio::null())
                     .stderr(std::process::Stdio::null())
                     .spawn() {
                Ok(child) => {
                    println!("  {GREEN}✓ Daemon started (PID: {}){RESET}", child.id().unwrap_or(0));

                    // Wait a moment for it to boot
                    tokio::time::sleep(Duration::from_secs(2)).await;

                    // Verify it's running
                    let health = client.get(format!("http://127.0.0.1:{}/health", ff_terminal::app::PORT_DAEMON))
                        .send().await;
                    match health {
                        Ok(r) if r.status().is_success() => {
                            println!("  {GREEN}✓ Daemon healthy{RESET}");
                        }
                        _ => {
                            println!("  {YELLOW}⚠ Daemon started but health check pending{RESET}");
                            println!("  It may still be initializing. Check: ff health");
                        }
                    }
                }
                Err(e) => {
                    println!("  {RED}✗ Failed to start daemon: {e}{RESET}");
                    println!("  Binary: {}", bin.display());
                    println!("  Try: cargo run --release (from forge-fleet directory)");
                }
            }
        }
        None => {
            println!("  {RED}✗ forgefleetd binary not found{RESET}");
            println!("  Build with: cargo build --release");
            println!("  Or run: cargo run --release");
        }
    }

    // Step 3: Check fleet connectivity
    println!("{YELLOW}3/4{RESET} Checking fleet nodes...");
    let nodes = [("Taylor","192.168.5.100"),("Marcus","192.168.5.102"),("Sophie","192.168.5.103"),("Priya","192.168.5.104"),("James","192.168.5.108")];
    let mut online = 0;
    for (name, ip) in &nodes {
        let ok = client.get(format!("http://{ip}:51000/health")).send().await
            .map(|r| r.status().is_success()).unwrap_or(false);
        if ok { online += 1; }
        let icon = if ok { format!("{GREEN}●{RESET}") } else { format!("{RED}○{RESET}") };
        println!("  {icon} {name} ({ip})");
    }

    // Step 4: Summary
    println!("{YELLOW}4/4{RESET} Summary");
    println!();
    println!("  {GREEN}ForgeFleet v{}{RESET}", env!("CARGO_PKG_VERSION"));
    println!("  Fleet: {online}/{} nodes online", nodes.len());
    println!();
    println!("  Daemon:    http://localhost:{}", ff_terminal::app::PORT_DAEMON);
    println!("  LLM API:   http://localhost:{}", ff_terminal::app::PORT_LLM);
    println!("  Web UI:    http://localhost:{}", ff_terminal::app::PORT_WEB);
    println!("  WebSocket: ws://localhost:{}", ff_terminal::app::PORT_WS);
    println!("  Metrics:   http://localhost:{}", ff_terminal::app::PORT_METRICS);
    println!();
    println!("  Run {CYAN}ff{RESET} for terminal, or open {CYAN}http://localhost:{}{RESET} for web UI", ff_terminal::app::PORT_WEB);

    Ok(())
}

/// Find the forgefleetd daemon binary.
fn find_daemon_binary(working_dir: &Path) -> Option<PathBuf> {
    // Check common locations
    let candidates = [
        working_dir.join("target/release/forgefleetd"),
        working_dir.join("target/debug/forgefleetd"),
        PathBuf::from("/usr/local/bin/forgefleetd"),
        dirs::home_dir().unwrap_or_default().join(".local/bin/forgefleetd"),
        dirs::home_dir().unwrap_or_default().join(".cargo/bin/forgefleetd"),
    ];

    for path in candidates.iter() {
        if path.exists() { return Some(path.to_path_buf()); }
    }

    // Try which
    if let Ok(output) = std::process::Command::new("which").arg("forgefleetd").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() { return Some(PathBuf::from(path)); }
        }
    }

    None
}

fn handle_status(p: &Path) -> Result<()> {
    let cfg = load_config(p)?;
    println!("{GREEN}✓ ForgeFleet Status{RESET}\n  nodes: {}\n  models: {}", cfg.nodes.len(), cfg.models.len()); Ok(())
}

fn handle_nodes(p: &Path) -> Result<()> {
    let cfg = load_config(p)?;
    println!("{GREEN}✓ Fleet Nodes{RESET}");
    for (n, d) in cfg.nodes { println!("  - {n}: {d}"); } Ok(())
}

async fn handle_models(c: &AgentSessionConfig) -> Result<()> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(5)).build()?;
    let url = format!("{}/v1/models", c.llm_base_url.trim_end_matches('/'));
    match client.get(&url).send().await {
        Ok(r) => println!("{}", r.text().await.unwrap_or_default()),
        Err(e) => println!("{RED}Failed: {e}{RESET}"),
    } Ok(())
}

async fn handle_health(_c: &AgentSessionConfig) -> Result<()> {
    println!("{GREEN}✓ ForgeFleet Health{RESET}");
    let client = reqwest::Client::builder().timeout(Duration::from_secs(3)).build()?;
    for (name, ip) in [("Taylor","192.168.5.100"),("Marcus","192.168.5.102"),("Sophie","192.168.5.103"),("Priya","192.168.5.104"),("James","192.168.5.108")] {
        let s = client.get(format!("http://{ip}:51000/health")).send().await.map(|r| r.status().is_success()).unwrap_or(false);
        println!("  {name:<12} {ip}:51000  {}", if s {format!("{GREEN}ONLINE{RESET}")} else {format!("{RED}OFFLINE{RESET}")});
    } Ok(())
}

fn handle_config(cmd: ConfigCommand, p: &Path) -> Result<()> {
    match cmd {
        ConfigCommand::Show => { let c = load_config(p)?; println!("{}", toml::to_string_pretty(&c)?.trim_end()); Ok(()) }
        ConfigCommand::Set { key, value } => {
            let mut c = load_config(p)?;
            let v = value.parse::<toml::Value>().unwrap_or(toml::Value::String(value.clone()));
            let parts: Vec<&str> = key.split('.').collect();
            if parts.len() < 2 { anyhow::bail!("Key must be dotted: section.key"); }
            match parts[0] { "general" => { c.general.insert(parts[1..].join("."), v); } "nodes" => { c.nodes.insert(parts[1..].join("."), v); } _ => { c.extra.insert(key.clone(), v); } }
            if let Some(parent) = p.parent() { fs::create_dir_all(parent)?; }
            fs::write(p, toml::to_string_pretty(&c)?)?;
            println!("{GREEN}✓{RESET} {key}={value}"); Ok(())
        }
    }
}
