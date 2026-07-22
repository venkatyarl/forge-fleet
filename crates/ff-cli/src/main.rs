use std::{
    collections::BTreeMap,
    fs,
    num::NonZeroUsize,
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

    /// Path to a local GGUF small-language model
    #[arg(long, global = true)]
    slm_model: Option<PathBuf>,

    /// Number of llama.cpp threads to use for the local SLM
    #[arg(long, global = true)]
    slm_threads: Option<NonZeroUsize>,

    /// Maximum RAM available to the local SLM, in MiB
    #[arg(long, global = true)]
    slm_mem_budget_mb: Option<u64>,

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
    /// Diagnose the local installation or installation state across the fleet
    Doctor(DoctorArgs),
    Tools(ToolsArgs),
    Config(ConfigArgs),
    /// Export data to external systems
    Export(ExportArgs),
    /// Project management — KPI digests over work_items
    Pm(PmArgs),
    Version,
}

#[derive(Debug, Args)]
struct PmArgs {
    #[command(subcommand)]
    command: PmCommand,
}

#[derive(Debug, Subcommand)]
enum PmCommand {
    /// Print the PM velocity KPI digest (identical to the Telegram daemon digest)
    Velocity,
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
struct DoctorArgs {
    /// Show installation status reported by every fleet node
    #[arg(long, default_value_t = false)]
    fleet: bool,
    /// Show software names instead of summary counts
    #[arg(long, default_value_t = false)]
    verbose: bool,
}

#[derive(Debug, Args)]
struct ToolsArgs {
    #[command(subcommand)]
    command: ToolsCommand,
}

#[derive(Debug, Subcommand)]
enum ToolsCommand {
    /// List all tools registered across the fleet
    List {
        /// Filter by node name
        #[arg(long)]
        node: Option<String>,
        /// Filter by tool name (substring match)
        #[arg(long)]
        name: Option<String>,
        /// Show only unhealthy tools (stale >5 min)
        #[arg(long)]
        unhealthy: bool,
    },
    /// Show tool health status across all nodes
    Health,
    /// Register local tools with the fleet registry (usually auto-run on startup)
    Register {
        /// Node name to register as (defaults to hostname)
        #[arg(long)]
        node: Option<String>,
    },
}

#[derive(Debug, Args)]
struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Debug, Args)]
struct ExportArgs {
    #[command(subcommand)]
    command: ExportCommand,
}

#[derive(Debug, Subcommand)]
enum ExportCommand {
    /// Trigger the Obsidian export daemon.
    Obsidian,
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
    slm_model: Option<PathBuf>,
    #[serde(default)]
    slm_threads: Option<usize>,
    #[serde(default)]
    slm_mem_budget_mb: Option<u64>,
    #[serde(default)]
    general: BTreeMap<String, toml::Value>,
    #[serde(default)]
    nodes: BTreeMap<String, toml::Value>,
    #[serde(default)]
    models: BTreeMap<String, toml::Value>,
    #[serde(default)]
    minio: ff_cli::MinioConfig,
    #[serde(flatten)]
    extra: BTreeMap<String, toml::Value>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = resolve_config_path(cli.config)?;

    configure_slm(
        &config_path,
        cli.slm_model,
        cli.slm_threads,
        cli.slm_mem_budget_mb,
    )?;

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
        Command::Doctor(args) => handle_doctor(args).await,
        Command::Tools(args) => handle_tools(args, &config_path).await,
        Command::Config(args) => handle_config(args, &config_path),
        Command::Export(args) => handle_export(args).await,
        Command::Pm(args) => handle_pm(args).await,
        Command::Version => {
            println!("forgefleet {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

fn configure_slm(
    config_path: &Path,
    model: Option<PathBuf>,
    threads: Option<NonZeroUsize>,
    mem_budget_mb: Option<u64>,
) -> Result<()> {
    let cfg = load_config(config_path)?;
    let model = model.or(cfg.slm_model);
    let threads = threads.map(NonZeroUsize::get).or(cfg.slm_threads);
    let mem_budget_mb = mem_budget_mb.or(cfg.slm_mem_budget_mb);

    if threads == Some(0) {
        anyhow::bail!("--slm-threads must be greater than zero");
    }
    if let Some(budget) = mem_budget_mb {
        ff_agent::slm::validate_memory_budget_mb(budget).map_err(anyhow::Error::msg)?;
    }

    // SAFETY: configuration is applied during startup, before this process
    // creates any worker tasks that could concurrently access the environment.
    unsafe {
        if let Some(model) = model {
            std::env::set_var("FORGEFLEET_SLM_MODEL", model);
        }
        if let Some(threads) = threads {
            std::env::set_var("FORGEFLEET_SLM_THREADS", threads.to_string());
        }
        if let Some(budget) = mem_budget_mb {
            std::env::set_var("FORGEFLEET_SLM_MEM_BUDGET_MB", budget.to_string());
        }
    }
    Ok(())
}

async fn handle_chat(args: ChatArgs) -> Result<()> {
    let working_dir = args
        .cwd
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));

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
        let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(256);

        // Print events in background
        let printer = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                match event {
                    AgentEvent::ToolStart {
                        tool_name,
                        input_json,
                        ..
                    } => {
                        println!(
                            "{YELLOW}⚡ {tool_name}{RESET} {}",
                            truncate_display(&input_json, 80)
                        );
                    }
                    AgentEvent::ToolEnd {
                        tool_name,
                        result,
                        is_error,
                        duration_ms,
                        ..
                    } => {
                        let icon = if is_error { RED } else { GREEN };
                        let status = if is_error { "✗" } else { "✓" };
                        println!(
                            "{icon}{status} {tool_name}{RESET} ({duration_ms}ms) {}",
                            truncate_display(&result, 200)
                        );
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
                    AgentEvent::Compaction {
                        messages_before,
                        messages_after,
                        ..
                    } => {
                        println!(
                            "{YELLOW}  Compacted: {messages_before} → {messages_after} messages{RESET}"
                        );
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
            ff_agent::agent_loop::AgentOutcome::EndTurn { .. } => {}
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
    let working_dir = args
        .cwd
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));

    let config = AgentSessionConfig {
        model: args.model,
        llm_base_url: args.llm,
        working_dir,
        system_prompt: None,
        max_turns: args.max_turns,
        ..Default::default()
    };

    let mut session = AgentSession::new(config);
    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(256);

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
            ff_agent::agent_loop::AgentOutcome::EndTurn { final_message } => {
                println!("{final_message}")
            }
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

async fn handle_doctor(args: DoctorArgs) -> Result<()> {
    if !args.fleet {
        println!("{GREEN}✓ ForgeFleet doctor{RESET}");
        println!("  Run with --fleet to show installation status for every node.");
        return Ok(());
    }

    let gateway = std::env::var("FF_ORCHESTRATOR_URL")
        .or_else(|_| std::env::var("FF_GATEWAY_URL"))
        .unwrap_or_else(|_| "http://192.168.5.100:51002".to_string());
    let url = format!("{}/api/pulses/recent", gateway.trim_end_matches('/'));
    let response = reqwest::Client::new().get(&url).send().await?;
    if !response.status().is_success() {
        anyhow::bail!("Gateway returned {} for {url}", response.status());
    }

    let payload: serde_json::Value = response.json().await?;
    let rows = fleet_install_rows(&payload, args.verbose);

    println!("{GREEN}✓ Fleet Install Status{RESET}");
    println!(
        "  {CYAN}{:<20} {:<10} {}{RESET}",
        "NODE", "INSTALLED", "MISSING"
    );
    if rows.is_empty() {
        println!("  {YELLOW}No recent install status reported{RESET}");
    } else {
        for (node, installed, missing) in rows {
            println!("  {node:<20} {installed:<10} {missing}");
        }
    }
    Ok(())
}

fn fleet_install_rows(payload: &serde_json::Value, verbose: bool) -> Vec<(String, String, String)> {
    let pulses = payload
        .as_array()
        .or_else(|| payload.get("pulses").and_then(serde_json::Value::as_array))
        .or_else(|| payload.get("beats").and_then(serde_json::Value::as_array))
        .or_else(|| payload.get("nodes").and_then(serde_json::Value::as_array));

    let mut rows = pulses
        .into_iter()
        .flatten()
        .filter_map(|pulse| {
            let diff = pulse.get("install_diff")?;
            let node = ["computer_name", "node_name", "worker_name", "name"]
                .into_iter()
                .find_map(|key| pulse.get(key).and_then(serde_json::Value::as_str))
                .unwrap_or("?")
                .to_string();
            let installed = software_names(diff.get("installed"));
            let missing = software_names(diff.get("missing"));
            Some((
                node,
                render_software(&installed, verbose),
                render_software(&missing, verbose),
            ))
        })
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

fn software_names(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            item.as_str().map(str::to_owned).or_else(|| {
                ["name", "software", "id"]
                    .into_iter()
                    .find_map(|key| item.get(key).and_then(serde_json::Value::as_str))
                    .map(str::to_owned)
            })
        })
        .collect()
}

fn render_software(items: &[String], verbose: bool) -> String {
    if verbose {
        if items.is_empty() {
            "—".to_string()
        } else {
            items.join(", ")
        }
    } else {
        items.len().to_string()
    }
}

async fn handle_tools(args: ToolsArgs, _config_path: &Path) -> Result<()> {
    static SHARED_HTTP: std::sync::LazyLock<reqwest::Client> =
        std::sync::LazyLock::new(reqwest::Client::new);
    let gateway = std::env::var("FF_GATEWAY_URL")
        .unwrap_or_else(|_| "http://192.168.5.100:51002".to_string());
    let client = &*SHARED_HTTP;

    match args.command {
        ToolsCommand::List {
            node,
            name,
            unhealthy,
        } => {
            let mut url = format!("{gateway}/api/tools");
            let mut params = vec![];
            if let Some(n) = node {
                params.push(format!("node={n}"));
            }
            if let Some(n) = name {
                params.push(format!("name={n}"));
            }
            if unhealthy {
                params.push("unhealthy=true".to_string());
            }
            if !params.is_empty() {
                url = format!("{}?{}", url, params.join("&"));
            }

            let resp = client.get(&url).send().await?;
            if !resp.status().is_success() {
                anyhow::bail!("Gateway returned {}", resp.status());
            }
            let body: serde_json::Value = resp.json().await?;
            let empty_tools = vec![];
            let tools = body["tools"].as_array().unwrap_or(&empty_tools);

            println!("{GREEN}✓ Fleet Tools{RESET} ({} total)", tools.len());
            for tool in tools {
                let name = tool["tool_name"].as_str().unwrap_or("?");
                let node = tool["worker_name"].as_str().unwrap_or("?");
                let healthy = tool["healthy"].as_bool().unwrap_or(false);
                let status = if healthy {
                    format!("{GREEN}●{RESET}")
                } else {
                    format!("{RED}●{RESET}")
                };
                println!("  {status} {name:<30} on {node}",);
            }
        }
        ToolsCommand::Health => {
            let url = format!("{gateway}/api/tools/health");
            let resp = client.get(&url).send().await?;
            if !resp.status().is_success() {
                anyhow::bail!("Gateway returned {}", resp.status());
            }
            let body: serde_json::Value = resp.json().await?;

            let total = body["total_tools"].as_i64().unwrap_or(0);
            let healthy = body["healthy_tools"].as_i64().unwrap_or(0);
            let unhealthy = body["unhealthy_tools"].as_i64().unwrap_or(0);

            println!("{GREEN}✓ Tool Registry Health{RESET}");
            println!("  total:     {total}");
            println!(
                "  healthy:   {}{GREEN}{}{RESET}",
                if healthy == total { "" } else { "  " },
                healthy
            );
            if unhealthy > 0 {
                println!("  unhealthy: {RED}{unhealthy}{RESET}");
            }
            if let Some(nodes) = body["nodes"].as_array() {
                println!("\n  By node:");
                for node in nodes {
                    let name = node["worker_name"].as_str().unwrap_or("?");
                    let n_tools = node["tool_count"].as_i64().unwrap_or(0);
                    let n_healthy = node["healthy_count"].as_i64().unwrap_or(0);
                    let n_unhealthy = node["unhealthy_count"].as_i64().unwrap_or(0);
                    let status = if n_unhealthy == 0 {
                        format!("{GREEN}✓{RESET}")
                    } else {
                        format!("{RED}✗{RESET}")
                    };
                    println!(
                        "    {status} {name:<15} {n_tools} tools ({n_healthy} healthy, {n_unhealthy} unhealthy)",
                    );
                }
            }
        }
        ToolsCommand::Register { node } => {
            let worker_name = node.unwrap_or_else(|| {
                std::env::var("HOSTNAME")
                    .or_else(|_| std::env::var("COMPUTERNAME"))
                    .unwrap_or_else(|_| "unknown".to_string())
            });
            println!("{CYAN}▶ Registering tools for {worker_name}{RESET}");
            println!("  (Tool registration is automatic on ff-agent startup.");
            println!("   This command is for manual re-registration if needed.)");
        }
    }
    Ok(())
}

async fn handle_pm(args: PmArgs) -> Result<()> {
    match args.command {
        PmCommand::Velocity => {
            let pool = ff_agent::fleet_info::get_fleet_pool()
                .await
                .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
            let digest = ff_agent::pm_velocity::velocity_digest(&pool).await?;
            print!("{digest}");
            Ok(())
        }
    }
}

async fn handle_export(args: ExportArgs) -> Result<()> {
    match args.command {
        ExportCommand::Obsidian => handle_export_obsidian().await,
    }
}

async fn handle_export_obsidian() -> Result<()> {
    let gateway = std::env::var("FF_GATEWAY_URL")
        .unwrap_or_else(|_| "http://192.168.5.100:51002".to_string());
    let client = reqwest::Client::new();
    let url = format!("{gateway}/api/export/obsidian");

    println!("{CYAN}▶ Triggering Obsidian export daemon{RESET}");
    println!("  endpoint: {url}");

    let resp = client.post(&url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("Obsidian export daemon returned {}", resp.status());
    }

    println!("{GREEN}✓ Obsidian export daemon triggered{RESET}");
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
