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
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
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
}

#[derive(Debug, Subcommand)]
enum ConfigCommand { Show, Set { key: String, value: String } }

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = resolve_config_path(cli.config)?;
    let llm = cli.llm.or_else(|| env::var("FORGEFLEET_LLM_URL").ok())
        .unwrap_or_else(|| detect_local_llm().unwrap_or_else(|| "http://localhost:51000".into()));
    let model = cli.model.or_else(|| env::var("FORGEFLEET_MODEL").ok()).unwrap_or_else(|| "auto".into());
    let working_dir = cli.cwd.unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));

    let agent_config = AgentSessionConfig {
        model, llm_base_url: llm, working_dir: working_dir.clone(),
        system_prompt: None, max_turns: 30, ..Default::default()
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
        None => {
            let prompt_text = cli.prompt.join(" ");
            if !prompt_text.is_empty() { run_headless(&prompt_text, agent_config, "text").await }
            else { run_tui(agent_config).await }
        }
    }
}

// ─── TUI Mode ──────────────────────────────────────────────────────────────

async fn run_tui(config: AgentSessionConfig) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(config.clone());
    let commands = CommandRegistry::new();
    let command_list: Vec<(&str, &str)> = commands.list();

    // Async fleet health check on startup
    check_fleet_health(&mut app).await;

    let result = run_event_loop(&mut terminal, &mut app, config, &commands, &command_list).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
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
                        app.session_id = session.id.to_string();
                        app.session = Some(session);
                    }
                }
                event_rx = None;
                app.is_running = false;
                app.status = "Ready".into();
            }
        }

        // Render
        app.frame += 1;
        terminal.draw(|frame| render::render(frame, app))?;

        // Poll events
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                match (key.code, key.modifiers) {
                    // Esc: cancel running agent (don't quit)
                    (KeyCode::Esc, _) if app.is_running => {
                        if let Some(handle) = agent_handle.take() {
                            handle.abort();
                        }
                        event_rx = None;
                        app.is_running = false;
                        app.status = "Cancelled".into();
                        app.messages.push(ff_terminal::messages::render_status("Agent cancelled by user"));
                    }

                    // Ctrl+C: quit (only when not running, otherwise cancel)
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        if app.is_running {
                            if let Some(handle) = agent_handle.take() { handle.abort(); }
                            event_rx = None;
                            app.is_running = false;
                            app.status = "Cancelled".into();
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
                        if app.input.suggestion_index.is_some() {
                            app.input.accept_suggestion();
                            continue;
                        }

                        if app.input.text.trim().is_empty() { continue; }

                        let trimmed = app.input.text.trim().to_string();
                        if trimmed == "/exit" || trimmed == "/quit" {
                            app.should_quit = true;
                            continue;
                        }

                        // If running, cancel current and start new
                        if app.is_running {
                            if let Some(handle) = agent_handle.take() { handle.abort(); }
                            event_rx = None;
                            app.is_running = false;
                        }

                        // Slash commands
                        if trimmed.starts_with('/') {
                            let mut session = app.session.take().unwrap_or_else(|| AgentSession::new(config.clone()));
                            if let Some(output) = commands.try_execute(&trimmed, &mut session).await {
                                app.messages.push(ff_terminal::messages::render_user_message(&trimmed));
                                app.messages.push(ff_terminal::messages::render_assistant_message(&output));
                                app.input.submit();
                            }
                            app.session = Some(session);
                            continue;
                        }

                        // Agent run
                        app.submit_input();
                        let mut session = app.session.take().unwrap_or_else(|| AgentSession::new(config.clone()));
                        let prompt = trimmed;
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
                        app.input.compute_suggestions(command_list);
                        app.input.next_suggestion();
                    }
                    (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                        app.input.insert_char(c);
                        if app.input.text.starts_with('/') {
                            app.input.compute_suggestions(command_list);
                        }
                    }
                    (KeyCode::Backspace, _) => app.input.backspace(),
                    (KeyCode::Delete, _) => app.input.delete(),
                    (KeyCode::Left, _) => app.input.move_left(),
                    (KeyCode::Right, _) => app.input.move_right(),
                    (KeyCode::Home, _) => app.input.home(),
                    (KeyCode::End, _) => app.input.end(),
                    (KeyCode::Up, _) => app.input.history_up(),
                    (KeyCode::Down, _) => app.input.history_down(),

                    // Scroll
                    (KeyCode::PageUp, _) => { app.auto_scroll = false; app.scroll_offset = app.scroll_offset.saturating_add(10); }
                    (KeyCode::PageDown, _) => {
                        if app.scroll_offset > 10 { app.scroll_offset -= 10; } else { app.scroll_offset = 0; app.auto_scroll = true; }
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

fn detect_local_llm() -> Option<String> {
    let ports = [51000, 51001, 11434, 8080];
    for port in ports {
        if std::net::TcpStream::connect_timeout(
            &format!("127.0.0.1:{port}").parse().ok()?,
            Duration::from_millis(100),
        ).is_ok() { return Some(format!("http://127.0.0.1:{port}")); }
    }
    let fleet = ["192.168.5.100", "192.168.5.102", "192.168.5.103", "192.168.5.104", "192.168.5.108"];
    for ip in fleet {
        if std::net::TcpStream::connect_timeout(
            &format!("{ip}:51000").parse().ok()?, Duration::from_millis(200),
        ).is_ok() { return Some(format!("http://{ip}:51000")); }
    }
    None
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

    // Check if already running
    let client = reqwest::Client::builder().timeout(Duration::from_secs(2)).build()?;
    if client.get(format!("http://127.0.0.1:{}/health", ff_terminal::app::PORT_DAEMON))
        .send().await.map(|r| r.status().is_success()).unwrap_or(false) {
        println!("{GREEN}✓ ForgeFleet is already running{RESET}");
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
