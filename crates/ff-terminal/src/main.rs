//! `ff` — ForgeFleet unified CLI.
//!
//! Usage:
//!   ff                          — interactive TUI agent
//!   ff "fix the bug"            — headless agent run
//!   ff start                    — start ForgeFleet daemon
//!   ff status                   — fleet status
//!   ff nodes                    — list fleet nodes
//!   ff models                   — list available models
//!   ff config show              — show fleet.toml
//!   ff health                   — run diagnostics

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{fs, env};

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
use ff_terminal::app::App;
use ff_terminal::render;

// ─── ANSI colors for non-TUI output ────────────────────────────────────────

const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

// ─── CLI argument parsing ──────────────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(
    name = "ff",
    version,
    about = "ForgeFleet — distributed AI agent platform",
    long_about = "ForgeFleet unified CLI. Run with no arguments for interactive TUI, or use subcommands for fleet management."
)]
struct Cli {
    /// Config file path (defaults to ~/.forgefleet/fleet.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// LLM endpoint URL (overrides config)
    #[arg(long, global = true)]
    llm: Option<String>,

    /// Model name (overrides config)
    #[arg(short = 'm', long, global = true)]
    model: Option<String>,

    /// Working directory
    #[arg(long, global = true)]
    cwd: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,

    /// Prompt to run in headless mode (when no subcommand given)
    #[arg(trailing_var_arg = true)]
    prompt: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start ForgeFleet daemon
    Start {
        #[arg(long, default_value_t = false)]
        leader: bool,
    },
    /// Show fleet status
    Status,
    /// List fleet nodes
    Nodes,
    /// List available LLM models
    Models,
    /// Run diagnostics
    Health,
    /// LLM proxy server
    Proxy {
        #[arg(long, default_value_t = 4000)]
        port: u16,
    },
    /// Discover fleet nodes on network
    Discover {
        #[arg(long, default_value = "192.168.5.0/24")]
        subnet: String,
    },
    /// Manage fleet configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Show version
    Version,
    /// Run a prompt through the agent (headless)
    Run {
        /// The prompt
        prompt: String,
        /// Output format (text or json)
        #[arg(long, default_value = "text")]
        output: String,
        /// Max turns
        #[arg(long, default_value_t = 30)]
        max_turns: u32,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Show current configuration
    Show,
    /// Set a configuration value
    Set {
        /// Dotted key path (e.g. general.log_level)
        key: String,
        /// Value to set
        value: String,
    },
}

// ─── Main ──────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = resolve_config_path(cli.config)?;

    let llm = cli.llm.or_else(|| env::var("FORGEFLEET_LLM_URL").ok())
        .unwrap_or_else(|| detect_local_llm().unwrap_or_else(|| "http://localhost:51000".into()));
    let model = cli.model.or_else(|| env::var("FORGEFLEET_MODEL").ok())
        .unwrap_or_else(|| "auto".into());
    let working_dir = cli.cwd.unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));

    let agent_config = AgentSessionConfig {
        model,
        llm_base_url: llm,
        working_dir: working_dir.clone(),
        system_prompt: None,
        max_turns: 30,
        ..Default::default()
    };

    match cli.command {
        // Subcommands
        Some(Command::Start { leader }) => handle_start(leader, &config_path),
        Some(Command::Status) => handle_status(&config_path),
        Some(Command::Nodes) => handle_nodes(&config_path),
        Some(Command::Models) => handle_models(&agent_config).await,
        Some(Command::Health) => handle_health(&agent_config).await,
        Some(Command::Proxy { port }) => handle_proxy(port, &config_path),
        Some(Command::Discover { subnet }) => handle_discover(&subnet, &config_path),
        Some(Command::Config { command }) => handle_config(command, &config_path),
        Some(Command::Version) => { println!("ff {}", env!("CARGO_PKG_VERSION")); Ok(()) }
        Some(Command::Run { prompt, output, max_turns }) => {
            let mut cfg = agent_config;
            cfg.max_turns = max_turns;
            run_headless(&prompt, cfg, &output).await
        }

        // No subcommand — check for trailing prompt or launch TUI
        None => {
            let prompt_text = cli.prompt.join(" ");
            if !prompt_text.is_empty() {
                run_headless(&prompt_text, agent_config, "text").await
            } else {
                run_tui(agent_config).await
            }
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

    let result = run_event_loop(&mut terminal, &mut app, config, &commands, &command_list).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    config: AgentSessionConfig,
    commands: &CommandRegistry,
    command_list: &[(&str, &str)],
) -> Result<()> {
    loop {
        app.frame += 1;
        terminal.draw(|frame| render::render(frame, app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                match (key.code, key.modifiers) {
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                        app.should_quit = true;
                    }

                    (KeyCode::Enter, KeyModifiers::NONE) if !app.is_running => {
                        if app.input.text.trim().is_empty() { continue; }

                        let trimmed = app.input.text.trim().to_string();
                        if trimmed == "/exit" || trimmed == "/quit" {
                            app.should_quit = true;
                            continue;
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
                        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();

                        let session_handle = tokio::spawn(async move {
                            let outcome = session.run(&prompt, Some(event_tx)).await;
                            (session, outcome)
                        });

                        loop {
                            while let Ok(ev) = event_rx.try_recv() { app.handle_event(ev); }
                            app.frame += 1;
                            terminal.draw(|frame| render::render(frame, app))?;

                            if event::poll(Duration::from_millis(50))? {
                                if let Event::Key(k) = event::read()? {
                                    if k.code == KeyCode::Esc || (k.code == KeyCode::Char('c') && k.modifiers == KeyModifiers::CONTROL) {
                                        break;
                                    }
                                }
                            }
                            if session_handle.is_finished() { break; }
                        }

                        while let Ok(ev) = event_rx.try_recv() { app.handle_event(ev); }

                        if let Ok((session, _)) = session_handle.await {
                            app.session_id = session.id.to_string();
                            app.session = Some(session);
                        }
                        app.is_running = false;
                        app.status = "Ready".into();
                    }

                    (KeyCode::Tab, _) if !app.is_running => {
                        app.input.compute_suggestions(command_list);
                        app.input.next_suggestion();
                    }
                    (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) if !app.is_running => {
                        app.input.insert_char(c);
                        if app.input.text.starts_with('/') {
                            app.input.compute_suggestions(command_list);
                        }
                    }
                    (KeyCode::Backspace, _) if !app.is_running => app.input.backspace(),
                    (KeyCode::Delete, _) if !app.is_running => app.input.delete(),
                    (KeyCode::Left, _) if !app.is_running => app.input.move_left(),
                    (KeyCode::Right, _) if !app.is_running => app.input.move_right(),
                    (KeyCode::Home, _) if !app.is_running => app.input.home(),
                    (KeyCode::End, _) if !app.is_running => app.input.end(),
                    (KeyCode::Up, _) if !app.is_running => app.input.history_up(),
                    (KeyCode::Down, _) if !app.is_running => app.input.history_down(),
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

    let handle = tokio::spawn(async move {
        session.run(&prompt, Some(event_tx)).await
    });

    let mut events = Vec::new();
    while let Some(event) = event_rx.recv().await {
        if is_json {
            events.push(event);
        } else {
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
        let result = serde_json::json!({
            "outcome": match &outcome {
                ff_agent::agent_loop::AgentOutcome::EndTurn { final_message } => serde_json::json!({"status": "done", "message": final_message}),
                ff_agent::agent_loop::AgentOutcome::MaxTurns { partial_message } => serde_json::json!({"status": "max_turns", "message": partial_message}),
                ff_agent::agent_loop::AgentOutcome::Error(e) => serde_json::json!({"status": "error", "message": e}),
                ff_agent::agent_loop::AgentOutcome::Cancelled => serde_json::json!({"status": "cancelled"}),
            },
            "events": events,
        });
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        if let ff_agent::agent_loop::AgentOutcome::EndTurn { final_message } = &outcome {
            if !final_message.is_empty() { println!("{final_message}"); }
        }
    }

    Ok(())
}

// ─── Fleet Management Commands ─────────────────────────────────────────────

/// Auto-detect the local LLM endpoint by checking common ports.
fn detect_local_llm() -> Option<String> {
    // Check local ports synchronously (fast TCP connect)
    let ports = [51000, 51001, 11434, 8080];
    for port in ports {
        if std::net::TcpStream::connect_timeout(
            &format!("127.0.0.1:{port}").parse().ok()?,
            std::time::Duration::from_millis(100),
        ).is_ok() {
            return Some(format!("http://127.0.0.1:{port}"));
        }
    }

    // Check known fleet IPs
    let fleet_ips = ["192.168.5.100", "192.168.5.102", "192.168.5.103", "192.168.5.104", "192.168.5.108"];
    for ip in fleet_ips {
        if std::net::TcpStream::connect_timeout(
            &format!("{ip}:51000").parse().ok()?,
            std::time::Duration::from_millis(200),
        ).is_ok() {
            return Some(format!("http://{ip}:51000"));
        }
    }

    None
}

fn resolve_config_path(override_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = override_path { return Ok(path); }
    let home = env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".forgefleet").join("fleet.toml"))
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct FleetConfig {
    #[serde(default)] general: BTreeMap<String, toml::Value>,
    #[serde(default)] nodes: BTreeMap<String, toml::Value>,
    #[serde(default)] models: BTreeMap<String, toml::Value>,
    #[serde(flatten)] extra: BTreeMap<String, toml::Value>,
}

fn load_config(path: &Path) -> Result<FleetConfig> {
    if !path.exists() { return Ok(FleetConfig::default()); }
    let content = fs::read_to_string(path).with_context(|| format!("Failed reading {}", path.display()))?;
    Ok(toml::from_str(&content).with_context(|| format!("Failed parsing {}", path.display()))?)
}

fn save_config(path: &Path, cfg: &FleetConfig) -> Result<()> {
    if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
    fs::write(path, toml::to_string_pretty(cfg)?)?;
    Ok(())
}

fn handle_start(leader: bool, config_path: &Path) -> Result<()> {
    let mode = if leader { "leader" } else { "auto" };
    println!("{CYAN}▶ Starting ForgeFleet daemon{RESET}");
    println!("  mode: {mode}");
    println!("  config: {}", config_path.display());
    Ok(())
}

fn handle_status(config_path: &Path) -> Result<()> {
    let cfg = load_config(config_path)?;
    println!("{GREEN}✓ ForgeFleet Status{RESET}");
    println!("  config: {}", config_path.display());
    println!("  nodes configured: {}", cfg.nodes.len());
    println!("  model groups: {}", cfg.models.len());
    Ok(())
}

fn handle_nodes(config_path: &Path) -> Result<()> {
    let cfg = load_config(config_path)?;
    println!("{GREEN}✓ Fleet Nodes{RESET}");
    if cfg.nodes.is_empty() { println!("  {YELLOW}No nodes in config{RESET}"); return Ok(()); }
    for (name, details) in cfg.nodes { println!("  - {name}: {details}"); }
    Ok(())
}

async fn handle_models(config: &AgentSessionConfig) -> Result<()> {
    println!("{GREEN}✓ Fleet Models{RESET}");
    let client = reqwest::Client::builder().timeout(Duration::from_secs(5)).build()?;
    let url = format!("{}/v1/models", config.llm_base_url.trim_end_matches('/'));
    match client.get(&url).send().await {
        Ok(resp) => {
            let body = resp.text().await.unwrap_or_default();
            println!("  {body}");
        }
        Err(e) => println!("  {RED}Failed to fetch models: {e}{RESET}"),
    }
    Ok(())
}

async fn handle_health(_config: &AgentSessionConfig) -> Result<()> {
    println!("{GREEN}✓ ForgeFleet Health{RESET}");
    let client = reqwest::Client::builder().timeout(Duration::from_secs(3)).build()?;

    let nodes = [
        ("Taylor", "192.168.5.100:51000"), ("Marcus", "192.168.5.102:51000"),
        ("Sophie", "192.168.5.103:51000"), ("Priya", "192.168.5.104:51000"),
        ("James", "192.168.5.108:51000"),
    ];

    for (name, addr) in &nodes {
        let url = format!("http://{addr}/health");
        let status = match client.get(&url).send().await {
            Ok(r) if r.status().is_success() => format!("{GREEN}ONLINE{RESET}"),
            _ => format!("{RED}OFFLINE{RESET}"),
        };
        println!("  {name:<12} {addr:<25} {status}");
    }
    Ok(())
}

fn handle_proxy(port: u16, config_path: &Path) -> Result<()> {
    println!("{CYAN}▶ Starting LLM proxy on 0.0.0.0:{port}{RESET}");
    println!("  config: {}", config_path.display());
    Ok(())
}

fn handle_discover(subnet: &str, config_path: &Path) -> Result<()> {
    println!("{CYAN}▶ Discovering nodes on {subnet}{RESET}");
    println!("  config: {}", config_path.display());
    Ok(())
}

fn handle_config(cmd: ConfigCommand, config_path: &Path) -> Result<()> {
    match cmd {
        ConfigCommand::Show => {
            let cfg = load_config(config_path)?;
            println!("{CYAN}Config ({}){RESET}", config_path.display());
            println!("{}", toml::to_string_pretty(&cfg)?.trim_end());
            Ok(())
        }
        ConfigCommand::Set { key, value } => {
            let mut cfg = load_config(config_path)?;
            let parsed = value.parse::<toml::Value>().unwrap_or(toml::Value::String(value.clone()));
            let parts: Vec<&str> = key.split('.').collect();
            if parts.len() < 2 { anyhow::bail!("Key must be dotted: section.key"); }
            match parts[0] {
                "general" => { cfg.general.insert(parts[1..].join("."), parsed); }
                "nodes" => { cfg.nodes.insert(parts[1..].join("."), parsed); }
                "models" => { cfg.models.insert(parts[1..].join("."), parsed); }
                _ => { cfg.extra.insert(key.clone(), parsed); }
            }
            save_config(config_path, &cfg)?;
            println!("{GREEN}✓ Updated{RESET} {key}={value}");
            Ok(())
        }
    }
}
