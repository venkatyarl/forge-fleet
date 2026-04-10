use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use ff_agent::agent_loop::{AgentEvent, AgentSession, AgentSessionConfig};
use ff_agent::commands::CommandRegistry;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

#[derive(Debug, Parser)]
#[command(
    name = "forgefleet",
    version,
    about = "ForgeFleet unified AI operating system"
)]
struct Cli {
    /// Config file path (defaults to ~/.forgefleet/fleet.toml)
    #[arg(long)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Start(StartArgs),
    Agent(AgentArgs),
    /// Interactive agent chat (like Claude Code CLI)
    Chat(ChatArgs),
    /// Run a single prompt through the agent (headless mode)
    Run(RunArgs),
    Status,
    Nodes,
    Models,
    Proxy(ProxyArgs),
    Discover(DiscoverArgs),
    Health,
    Config(ConfigArgs),
    Version,
}

#[derive(Debug, Args)]
struct ChatArgs {
    /// LLM endpoint URL
    #[arg(long, default_value = "http://192.168.5.102:55000")]
    llm: String,
    /// Model name
    #[arg(long, short = 'm', default_value = "auto")]
    model: String,
    /// Working directory
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// System prompt
    #[arg(long, short = 's')]
    system_prompt: Option<String>,
    /// Max turns per interaction
    #[arg(long, default_value_t = 30)]
    max_turns: u32,
}

#[derive(Debug, Args)]
struct RunArgs {
    /// The prompt to send to the agent
    prompt: String,
    /// LLM endpoint URL
    #[arg(long, default_value = "http://192.168.5.102:55000")]
    llm: String,
    /// Model name
    #[arg(long, short = 'm', default_value = "auto")]
    model: String,
    /// Working directory
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Output format (text or json)
    #[arg(long, default_value = "text")]
    output: String,
    /// Max turns
    #[arg(long, default_value_t = 30)]
    max_turns: u32,
}

#[derive(Debug, Args)]
struct StartArgs {
    #[arg(long, default_value_t = false)]
    leader: bool,
}

#[derive(Debug, Args)]
struct AgentArgs {
    #[arg(long)]
    node_id: Option<String>,
}

#[derive(Debug, Args)]
struct ProxyArgs {
    #[arg(long, default_value_t = 4000)]
    port: u16,
}

#[derive(Debug, Args)]
struct DiscoverArgs {
    #[arg(long, default_value = "192.168.5.0/24")]
    subnet: String,
}

#[derive(Debug, Args)]
struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Show,
    Set {
        /// Dotted key path, e.g. general.log_level
        key: String,
        /// Value to set (stored as TOML value when possible)
        value: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct FleetConfig {
    #[serde(default)]
    general: BTreeMap<String, toml::Value>,
    #[serde(default)]
    nodes: BTreeMap<String, toml::Value>,
    #[serde(default)]
    models: BTreeMap<String, toml::Value>,
    #[serde(flatten)]
    extra: BTreeMap<String, toml::Value>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = resolve_config_path(cli.config)?;

    match cli.command {
        Command::Start(args) => handle_start(args, &config_path),
        Command::Agent(args) => handle_agent(args, &config_path),
        Command::Chat(args) => handle_chat(args).await,
        Command::Run(args) => handle_run(args).await,
        Command::Status => handle_status(&config_path),
        Command::Nodes => handle_nodes(&config_path),
        Command::Models => handle_models(&config_path),
        Command::Proxy(args) => handle_proxy(args, &config_path),
        Command::Discover(args) => handle_discover(args, &config_path),
        Command::Health => handle_health(&config_path),
        Command::Config(args) => handle_config(args, &config_path),
        Command::Version => {
            println!("forgefleet {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

async fn handle_chat(args: ChatArgs) -> Result<()> {
    let working_dir = args.cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));

    println!("{CYAN}ForgeFleet Agent Chat{RESET}");
    println!("  LLM: {}", args.llm);
    println!("  Model: {}", args.model);
    println!("  Working dir: {}", working_dir.display());
    println!("  Type /help for commands, /exit to quit\n");

    let config = AgentSessionConfig {
        model: args.model,
        llm_base_url: args.llm,
        working_dir,
        system_prompt: args.system_prompt,
        max_turns: args.max_turns,
        ..Default::default()
    };

    let mut session = AgentSession::new(config);
    let command_registry = CommandRegistry::new();

    loop {
        // Read user input
        print!("{GREEN}> {RESET}");
        use std::io::Write;
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let input = input.trim();

        if input.is_empty() {
            continue;
        }

        if input == "/exit" || input == "/quit" || input == "exit" || input == "quit" {
            println!("{CYAN}Goodbye!{RESET}");
            break;
        }

        // Check for slash commands
        if input.starts_with('/') || input.starts_with('!') {
            if let Some(output) = command_registry.try_execute(input, &mut session).await {
                println!("{output}");
                continue;
            }
        }

        // Run agent loop
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<AgentEvent>();

        // Print events in background
        let printer = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                match event {
                    AgentEvent::ToolStart { tool_name, input_json, .. } => {
                        println!("{YELLOW}⚡ {tool_name}{RESET} {}", truncate_display(&input_json, 80));
                    }
                    AgentEvent::ToolEnd { tool_name, result, is_error, duration_ms, .. } => {
                        let icon = if is_error { RED } else { GREEN };
                        let status = if is_error { "✗" } else { "✓" };
                        println!("{icon}{status} {tool_name}{RESET} ({duration_ms}ms) {}", truncate_display(&result, 200));
                    }
                    AgentEvent::AssistantText { text, .. } => {
                        println!("\n{CYAN}{text}{RESET}\n");
                    }
                    AgentEvent::Status { message, .. } => {
                        println!("{YELLOW}  {message}{RESET}");
                    }
                    AgentEvent::Error { message, .. } => {
                        println!("{RED}Error: {message}{RESET}");
                    }
                    AgentEvent::Compaction { messages_before, messages_after, .. } => {
                        println!("{YELLOW}  Compacted: {messages_before} → {messages_after} messages{RESET}");
                    }
                    AgentEvent::TokenWarning { usage_pct, .. } => {
                        println!("{YELLOW}  ⚠ Context window at {usage_pct:.0}%{RESET}");
                    }
                    _ => {}
                }
            }
        });

        let outcome = session.run(input, Some(event_tx)).await;
        printer.abort();

        match outcome {
            ff_agent::agent_loop::AgentOutcome::EndTurn { .. } => {},
            ff_agent::agent_loop::AgentOutcome::MaxTurns { .. } => {
                println!("{YELLOW}(hit max turn limit){RESET}");
            }
            ff_agent::agent_loop::AgentOutcome::Error(e) => {
                println!("{RED}Error: {e}{RESET}");
            }
            ff_agent::agent_loop::AgentOutcome::Cancelled => {
                println!("{YELLOW}(cancelled){RESET}");
            }
        }
    }

    Ok(())
}

async fn handle_run(args: RunArgs) -> Result<()> {
    let working_dir = args.cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));

    let config = AgentSessionConfig {
        model: args.model,
        llm_base_url: args.llm,
        working_dir,
        system_prompt: None,
        max_turns: args.max_turns,
        ..Default::default()
    };

    let mut session = AgentSession::new(config);
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<AgentEvent>();

    // Collect events for JSON output
    let is_json = args.output == "json";
    let printer = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = event_rx.recv().await {
            if is_json {
                events.push(event);
            } else {
                match &event {
                    AgentEvent::AssistantText { text, .. } => print!("{text}"),
                    AgentEvent::Error { message, .. } => eprintln!("Error: {message}"),
                    _ => {}
                }
            }
        }
        events
    });

    let outcome = session.run(&args.prompt, Some(event_tx)).await;
    let events = printer.await.unwrap_or_default();

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
        match &outcome {
            ff_agent::agent_loop::AgentOutcome::EndTurn { final_message } => println!("{final_message}"),
            ff_agent::agent_loop::AgentOutcome::Error(e) => eprintln!("Error: {e}"),
            _ => {}
        }
    }

    Ok(())
}

fn truncate_display(s: &str, max: usize) -> String {
    let single_line = s.replace('\n', " ");
    if single_line.len() > max {
        format!("{}...", &single_line[..max])
    } else {
        single_line
    }
}

fn resolve_config_path(override_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(path);
    }

    let home = std::env::var("HOME").context("HOME environment variable not set")?;
    Ok(PathBuf::from(home).join(".forgefleet").join("fleet.toml"))
}

fn load_config(path: &Path) -> Result<FleetConfig> {
    if !path.exists() {
        return Ok(FleetConfig::default());
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed reading config file: {}", path.display()))?;

    let cfg = toml::from_str::<FleetConfig>(&content)
        .with_context(|| format!("Failed parsing TOML config: {}", path.display()))?;

    Ok(cfg)
}

fn save_config(path: &Path, cfg: &FleetConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed creating config directory: {}", parent.display()))?;
    }

    let content = toml::to_string_pretty(cfg).context("Failed serializing config to TOML")?;
    fs::write(path, content)
        .with_context(|| format!("Failed writing config: {}", path.display()))?;
    Ok(())
}

fn handle_start(args: StartArgs, config_path: &Path) -> Result<()> {
    let mode = if args.leader { "leader" } else { "auto" };
    println!("{CYAN}▶ Starting ForgeFleet daemon{RESET}");
    println!("  mode: {mode}");
    println!("  config: {}", config_path.display());
    Ok(())
}

fn handle_agent(args: AgentArgs, config_path: &Path) -> Result<()> {
    println!("{CYAN}▶ Starting ForgeFleet agent{RESET}");
    println!(
        "  node_id: {}",
        args.node_id.unwrap_or_else(|| "auto-detect".to_string())
    );
    println!("  config: {}", config_path.display());
    Ok(())
}

fn handle_status(config_path: &Path) -> Result<()> {
    let cfg = load_config(config_path)?;

    println!("{GREEN}✓ ForgeFleet Status{RESET}");
    println!("  config: {}", config_path.display());
    println!("  nodes configured: {}", cfg.nodes.len());
    println!("  model groups: {}", cfg.models.len());
    println!("  state: {YELLOW}bootstrap in progress{RESET}");
    Ok(())
}

fn handle_nodes(config_path: &Path) -> Result<()> {
    let cfg = load_config(config_path)?;

    println!("{GREEN}✓ Fleet Nodes{RESET}");
    if cfg.nodes.is_empty() {
        println!("  {YELLOW}No nodes found in config{RESET}");
        return Ok(());
    }

    for (name, details) in cfg.nodes {
        println!("  - {name}: {details}");
    }
    Ok(())
}

fn handle_models(config_path: &Path) -> Result<()> {
    let cfg = load_config(config_path)?;

    println!("{GREEN}✓ Fleet Models{RESET}");
    if cfg.models.is_empty() {
        println!("  {YELLOW}No model groups found in config{RESET}");
        return Ok(());
    }

    for (name, details) in cfg.models {
        println!("  - {name}: {details}");
    }
    Ok(())
}

fn handle_proxy(args: ProxyArgs, config_path: &Path) -> Result<()> {
    println!("{CYAN}▶ Starting LLM proxy{RESET}");
    println!("  listen: 0.0.0.0:{}", args.port);
    println!("  config: {}", config_path.display());
    Ok(())
}

fn handle_discover(args: DiscoverArgs, config_path: &Path) -> Result<()> {
    println!("{CYAN}▶ Discovering nodes{RESET}");
    println!("  subnet: {}", args.subnet);
    println!("  config: {}", config_path.display());
    Ok(())
}

fn handle_health(config_path: &Path) -> Result<()> {
    println!("{GREEN}✓ Health Check{RESET}");
    println!("  config: {}", config_path.display());
    println!("  api: {YELLOW}unknown{RESET}");
    println!("  discovery: {YELLOW}unknown{RESET}");
    println!("  agent: {YELLOW}unknown{RESET}");
    Ok(())
}

fn handle_config(args: ConfigArgs, config_path: &Path) -> Result<()> {
    match args.command {
        ConfigCommand::Show => {
            let cfg = load_config(config_path)?;
            let rendered = toml::to_string_pretty(&cfg)?;
            println!("{CYAN}Config ({}){RESET}", config_path.display());
            println!("{}", rendered.trim_end());
            Ok(())
        }
        ConfigCommand::Set { key, value } => {
            let mut cfg = load_config(config_path)?;
            set_dotted_key(&mut cfg, &key, &value)?;
            save_config(config_path, &cfg)?;
            println!("{GREEN}✓ Updated{RESET} {key}={value}");
            println!("  file: {}", config_path.display());
            Ok(())
        }
    }
}

fn parse_value(raw: &str) -> toml::Value {
    match raw.parse::<toml::Value>() {
        Ok(v) => v,
        Err(_) => toml::Value::String(raw.to_string()),
    }
}

fn set_dotted_key(cfg: &mut FleetConfig, dotted: &str, value: &str) -> Result<()> {
    let parts: Vec<&str> = dotted.split('.').collect();
    if parts.len() < 2 {
        anyhow::bail!("Key must be dotted path like section.key");
    }

    let section = parts[0];
    let key = parts[1..].join(".");
    let parsed = parse_value(value);

    match section {
        "general" => {
            cfg.general.insert(key, parsed);
        }
        "nodes" => {
            cfg.nodes.insert(key, parsed);
        }
        "models" => {
            cfg.models.insert(key, parsed);
        }
        _ => {
            cfg.extra.insert(dotted.to_string(), parsed);
        }
    }

    Ok(())
}

#[allow(dead_code)]
fn print_error_context(msg: &str) {
    eprintln!("{RED}error:{RESET} {msg}");
}
