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
use ff_terminal::app::App;
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
    /// Manage ForgeFleet tasks
    Task { #[command(subcommand)] command: TaskCommand },
    /// Manage fleet-wide secrets (HF token, API keys, etc.) stored in Postgres.
    Secrets { #[command(subcommand)] command: SecretsCommand },
    /// Deferred task queue — schedule work that runs when conditions are met
    /// (node comes online, a time is reached, manual retry).
    Defer { #[command(subcommand)] command: DeferCommand },
    /// Model lifecycle management (catalog, library, deployments, jobs).
    Model { #[command(subcommand)] command: ModelCommand },
    /// Run the deferred task worker loop (scheduler + executor).
    /// Typically run as a background service on the fleet leader.
    DeferWorker {
        /// Optional node name to use when claiming tasks; defaults to `hostname`.
        #[arg(long)] as_node: Option<String>,
        /// Poll interval in seconds (scheduler + fallback for Redis).
        #[arg(long, default_value_t = 15)] interval: u64,
        /// Also act as scheduler (evaluate triggers → dispatchable). Only one node should do this.
        #[arg(long, default_value_t = false)] scheduler: bool,
        /// Exit after one scheduler+worker pass (useful for tests / cron).
        #[arg(long, default_value_t = false)] once: bool,
    },
    /// Show installed-vs-latest tool versions across the fleet (drift matrix).
    Versions {
        #[arg(long)] node: Option<String>,
    },
    /// Fleet-wide operations (mesh check, verify node, etc.)
    Fleet { #[command(subcommand)] command: FleetCommand },
    /// Self-service onboarding helpers (show curl command, list recent, revoke).
    Onboard { #[command(subcommand)] command: OnboardCommand },
    /// Virtual Brain vault indexer + utilities.
    #[command(alias = "brain")]
    VirtualBrain { #[command(subcommand)] command: BrainCommand },
    /// Run ForgeFleet's unified daemon: deferred-task scheduler+worker, disk
    /// sampler, and deployment reconciler all in one long-lived process.
    /// Typically run on boot via launchd/systemd.
    Daemon {
        /// Worker node name (defaults to this host via DB lookup).
        #[arg(long)] as_node: Option<String>,
        /// Act as the deferred-task scheduler too (only one node should).
        #[arg(long, default_value_t = false)] scheduler: bool,
        /// Deferred-worker poll interval in seconds.
        #[arg(long, default_value_t = 15)] defer_interval: u64,
        /// Disk-sampler interval in seconds (default 300 = 5 min).
        #[arg(long, default_value_t = 300)] disk_interval: u64,
        /// Reconciler interval in seconds (default 60).
        #[arg(long, default_value_t = 60)] reconcile_interval: u64,
        /// Exit after one pass of each (useful for cron/testing).
        #[arg(long, default_value_t = false)] once: bool,
    },
}

#[derive(Debug, Clone, Subcommand)]
enum ConfigCommand {
    /// Show the local project config (TOML).
    Show,
    /// Set a dotted key in the local project config (TOML).
    Set { key: String, value: String },
    /// Configure properties of a fleet node in Postgres.
    /// Supported keys: runtime (mlx|llama.cpp|vllm|unknown), models_dir,
    /// disk_quota_pct, sub_agent_count, gh_account, role.
    Node {
        /// Node name (e.g. "marcus").
        name: String,
        /// Property to set.
        #[arg(value_parser = ["runtime", "models_dir", "disk_quota_pct", "sub_agent_count", "gh_account", "role"])]
        key: String,
        /// New value for the property.
        value: String,
    },
    /// Show per-node configuration (runtime, models_dir, disk_quota_pct).
    Nodes,
}

#[derive(Debug, Clone, Subcommand)]
enum DeferCommand {
    /// List deferred tasks. Filter by status or limit count.
    #[command(alias = "ls")]
    List {
        #[arg(long)] status: Option<String>,
        #[arg(long, default_value_t = 50)] limit: i64,
    },
    /// Enqueue a shell command to run when a target node comes online.
    /// Example: ff defer add-shell --when-node-online ace --run "rm -rf ~/.ollama" --title "Ollama cleanup on ace"
    AddShell {
        /// Human-readable title shown in listings.
        #[arg(long)] title: String,
        /// Shell command to execute on the target node (via SSH).
        #[arg(long)] run: String,
        /// Trigger: task runs when this node becomes reachable.
        #[arg(long = "when-node-online")] when_node_online: Option<String>,
        /// Optional: run at a specific RFC3339 time instead (UTC).
        #[arg(long = "when-at")] when_at: Option<String>,
        /// Node that should execute the command (defaults to the target in when-node-online).
        #[arg(long = "on-node")] on_node: Option<String>,
        #[arg(long, default_value_t = 5)] max_attempts: i32,
    },
    /// Show details for a single deferred task by id.
    Get { id: String },
    /// Cancel a pending/dispatchable/failed task.
    Cancel { id: String },
    /// Retry a failed or cancelled task (resets attempts-aware status, runs ASAP).
    Retry { id: String },
}

#[derive(Debug, Clone, Subcommand)]
enum FleetCommand {
    /// Pairwise SSH reachability check across the fleet (N×(N-1) probes).
    SshMeshCheck {
        #[arg(long)] node: Option<String>,
        #[arg(long)] json: bool,
        /// Only re-probe pairs whose last_checked in fleet_mesh_status is
        /// older than the given ISO-8601 duration prefix (e.g. "1h", "30m", "2d").
        #[arg(long)] since: Option<String>,
        /// Before probing, re-distribute user + host keys to any pair that
        /// is currently status='failed'. Requires --yes to actually run.
        #[arg(long, default_value_t = false)] repair: bool,
        #[arg(long, default_value_t = false)] yes: bool,
    },
    /// Full 12-check verify battery for one node.
    VerifyNode {
        name: String,
        #[arg(long)] json: bool,
    },
    /// Migrate every fleet node to a new GitHub owner + move the repo from
    /// ~/taylorProjects/forge-fleet → ~/projects/forge-fleet. Enqueues one
    /// idempotent shell task per node via the deferred queue (trigger=node_online),
    /// so offline nodes pick it up when they come back online.
    MigrateGithub {
        /// New GitHub owner/org for the forge-fleet remote (default: venkatyarl).
        #[arg(long, default_value = "venkatyarl")] new_owner: String,
        /// Skip the local node (the one running this command). Default: true.
        #[arg(long, default_value_t = true)] skip_local: bool,
        /// Only enqueue for this specific node (for testing a single target).
        #[arg(long)] only: Option<String>,
        /// Show planned enqueues without writing to the defer queue.
        #[arg(long, default_value_t = false)] dry_run: bool,
        /// Required to actually enqueue (otherwise prints plan and exits).
        #[arg(long, default_value_t = false)] yes: bool,
    },
}

#[derive(Debug, Clone, Subcommand)]
enum BrainCommand {
    /// Run a full vault index (parse all .md files, upsert nodes + edges).
    Index {
        /// Vault root path (default: ~/projects/Yarli_KnowledgeBase).
        #[arg(long)]
        vault_path: Option<String>,
        /// Only index this subfolder within the vault (default: index everything).
        #[arg(long)]
        subfolder: Option<String>,
    },
    /// Run community detection on the vault graph (Leiden placeholder).
    Communities,
    /// Show vault index stats.
    Stats,
}

#[derive(Debug, Clone, Subcommand)]
enum OnboardCommand {
    /// Print the copy-paste curl command for onboarding a new computer.
    Show {
        #[arg(long)] name: String,
        #[arg(long)] ip: Option<String>,
        #[arg(long)] ssh_user: Option<String>,
        #[arg(long, default_value = "builder")] role: String,
        #[arg(long, default_value = "auto")] runtime: String,
    },
    /// List fleet nodes by election_priority (recent onboards appear first).
    #[command(alias = "ls")]
    List {
        #[arg(long, default_value_t = 25)] limit: i64,
    },
    /// Revoke a node: delete its fleet_nodes row, ssh keys, and mesh rows.
    Revoke {
        name: String,
        #[arg(long, default_value_t = false)] yes: bool,
    },
}

#[derive(Debug, Clone, Subcommand)]
enum ModelCommand {
    /// Sync the curated model catalog TOML into Postgres.
    SyncCatalog,
    /// Search the catalog (fuzzy on id/name/family).
    Search { query: String },
    /// List catalog entries (what can be downloaded).
    Catalog,
    /// List library entries (what's on disk, per node).
    Library {
        #[arg(long)] node: Option<String>,
    },
    /// List current deployments (what's running, per node).
    Deployments {
        #[arg(long)] node: Option<String>,
    },
    /// Scan a node's local models directory and reconcile with fleet_model_library.
    /// Defaults to the current host (taylor) scanning ~/models.
    Scan {
        #[arg(long)] node: Option<String>,
        #[arg(long)] models_dir: Option<PathBuf>,
    },
    /// Show latest disk usage per node (from fleet_disk_usage snapshots).
    Disk,
    /// List lifecycle jobs (downloads, deletes, loads, swaps).
    Jobs {
        #[arg(long)] status: Option<String>,
        #[arg(long, default_value_t = 30)] limit: i64,
    },
    /// Download a model from HuggingFace to this node's models dir.
    /// Picks the variant matching this node's runtime (llama.cpp / mlx / vllm).
    Download {
        /// Catalog id (use `ff model search` to find one).
        id: String,
        /// Override runtime (default: this node's runtime from DB).
        #[arg(long)] runtime: Option<String>,
        /// Override target node (default: this host).
        #[arg(long)] node: Option<String>,
        /// Force re-download even if files already exist.
        #[arg(long, default_value_t = false)] force: bool,
    },
    /// Delete a model from a node's library (removes files from disk).
    Delete {
        /// Library id (UUID from `ff model library`).
        id: String,
        #[arg(long, default_value_t = false)] yes: bool,
    },
    /// Load a model: start a local inference server for it on the given port.
    Load {
        /// Library id (UUID from `ff model library`).
        id: String,
        /// Port to bind the inference server on (default: 51001).
        #[arg(long, default_value_t = 51001)] port: u16,
        /// Context window tokens (default 32768).
        #[arg(long)] ctx: Option<u32>,
        /// Parallel request slots (default 4).
        #[arg(long)] parallel: Option<u32>,
    },
    /// Enqueue downloads of multiple catalog ids onto a node via the deferred queue.
    DownloadBatch {
        #[arg(long)] node: String,
        ids: Vec<String>,
    },
    /// Unload: stop a running inference server by deployment id.
    Unload {
        /// Deployment id (UUID from `ff model deployments`).
        id: String,
    },
    /// List inference-server processes running on this host.
    Ps,
    /// Sample this node's disk usage and write to fleet_disk_usage.
    DiskSample,
    /// Show full details for a catalog id, library row UUID, or deployment UUID.
    Info { id: String },
    /// Show a smart-LRU eviction plan for a node (dry-run).
    Prune {
        #[arg(long)] node: Option<String>,
        /// Min days since last use before a row can be considered cold.
        #[arg(long, default_value_t = 7)] min_cold_days: i64,
    },
    /// Health-check a running deployment by id.
    Ping {
        id: String,
    },
    /// Transfer a model from one node to another (same-runtime, LAN rsync).
    Transfer {
        /// Library UUID on the source node.
        #[arg(long)] library_id: String,
        /// Source node name.
        #[arg(long)] from: String,
        /// Target node name.
        #[arg(long)] to: String,
    },
    /// Auto-load a catalog model on this node: resolves library row, picks a free
    /// port, calls load_model. No-op if already deployed.
    Autoload {
        /// Catalog id (e.g. "qwen3-coder-30b").
        catalog_id: String,
        /// Override context size (default 32768).
        #[arg(long)] ctx: Option<u32>,
    },
    /// Convert a safetensors library entry to MLX on this Apple Silicon host.
    Convert {
        /// Library UUID (must be runtime=vllm i.e. safetensors).
        library_id: String,
        /// Quantization bits (4 or 8).
        #[arg(long, default_value_t = 4)]
        q_bits: u8,
    },
}

#[derive(Debug, Clone, Subcommand)]
enum SecretsCommand {
    /// List secret keys (values are not printed).
    #[command(alias = "ls")]
    List,
    /// Print a secret value by key (careful — goes to stdout).
    Get { key: String },
    /// Set (or update) a secret.
    Set {
        key: String,
        value: String,
        #[arg(long)] description: Option<String>,
    },
    /// Delete a secret by key.
    #[command(alias = "rm")]
    Delete { key: String },
}

#[derive(Debug, Subcommand)]
enum TaskCommand {
    /// List recent tasks
    List {
        /// Filter by status (pending/in_progress/completed/failed)
        #[arg(long)]
        status: Option<String>,
        /// Maximum number of tasks to show
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Get details for a specific task
    Get { id: String },
    /// Update a task's status
    Update {
        id: String,
        #[arg(long)]
        status: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = resolve_config_path(cli.config)?;

    // Fast-path subcommands that don't need the inference router or any LLM probing.
    // Skips a network round-trip to the fleet + `/v1/models` HTTP fetch.
    match &cli.command {
        Some(Command::Version) => { println!("ff {}", env!("CARGO_PKG_VERSION")); return Ok(()); }
        Some(Command::Secrets { command }) => return handle_secrets(command.clone()).await,
        Some(Command::Defer   { command }) => return handle_defer(command.clone()).await,
        Some(Command::Model   { command }) => return handle_model(command.clone()).await,
        Some(Command::DeferWorker { as_node, interval, scheduler, once }) => {
            return handle_defer_worker(as_node.clone(), *interval, *scheduler, *once).await;
        }
        Some(Command::Daemon { as_node, scheduler, defer_interval, disk_interval, reconcile_interval, once }) => {
            return handle_daemon(as_node.clone(), *scheduler, *defer_interval, *disk_interval, *reconcile_interval, *once).await;
        }
        Some(Command::Config  { command }) => return handle_config(command.clone(), &config_path).await,
        Some(Command::Status)              => return handle_status(&config_path).await,
        Some(Command::Nodes)               => return handle_nodes(&config_path),
        Some(Command::Versions { node })   => return handle_versions(node.clone()).await,
        Some(Command::Fleet { command })   => return handle_fleet(command.clone()).await,
        Some(Command::Onboard { command }) => return handle_onboard(command.clone()).await,
        Some(Command::VirtualBrain { command }) => return handle_brain(command.clone()).await,
        _ => {}
    }

    // Build the local-first inference router (probes localhost + fleet from DB).
    // If the user explicitly passed --llm, skip auto-routing and use that URL directly.
    let (llm, router) = if let Some(explicit_url) = cli.llm.or_else(|| env::var("FORGEFLEET_LLM_URL").ok()) {
        (explicit_url, None)
    } else {
        let r = ff_agent::inference_router::InferenceRouter::from_config(&config_path).await;
        let primary = if let Some(url) = r.active_url() {
            url
        } else {
            detect_llm_from_db_or_local(&config_path).await
        };
        (primary, Some(std::sync::Arc::new(r)))
    };

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
        inference_router: router,
        ..Default::default()
    };

    match cli.command {
        Some(Command::Start { leader }) => handle_start(leader, &config_path, &working_dir).await,
        Some(Command::Stop) => handle_stop().await,
        Some(Command::Status) => handle_status(&config_path).await,
        Some(Command::Nodes) => handle_nodes(&config_path),
        Some(Command::Models) => handle_models(&agent_config).await,
        Some(Command::Health) => handle_health(&agent_config).await,
        Some(Command::Proxy { port }) => { println!("{CYAN}▶ Starting LLM proxy on 0.0.0.0:{port}{RESET}"); Ok(()) }
        Some(Command::Discover { subnet }) => { println!("{CYAN}▶ Discovering nodes on {subnet}{RESET}"); Ok(()) }
        Some(Command::Config { command }) => handle_config(command, &config_path).await,
        Some(Command::Version) => { println!("ff {}", env!("CARGO_PKG_VERSION")); Ok(()) }
        Some(Command::Run { prompt, output, max_turns }) => {
            let mut cfg = agent_config; cfg.max_turns = max_turns;
            run_headless(&prompt, cfg, &output).await
        }
        Some(Command::Task { command }) => handle_task(command, &config_path).await,
        Some(Command::Secrets { command }) => handle_secrets(command).await,
        Some(Command::Defer { command }) => handle_defer(command).await,
        Some(Command::Model { command }) => handle_model(command).await,
        Some(Command::DeferWorker { as_node, interval, scheduler, once }) => {
            handle_defer_worker(as_node, interval, scheduler, once).await
        }
        Some(Command::Daemon { as_node, scheduler, defer_interval, disk_interval, reconcile_interval, once }) => {
            handle_daemon(as_node, scheduler, defer_interval, disk_interval, reconcile_interval, once).await
        }
        Some(Command::Versions { node }) => handle_versions(node).await,
        Some(Command::Fleet { command }) => handle_fleet(command).await,
        Some(Command::Onboard { command }) => handle_onboard(command).await,
        Some(Command::VirtualBrain { command }) => handle_brain(command).await,
        Some(Command::Supervise { prompt, max_attempts }) => {
            let sup_config = ff_agent::supervisor::SupervisorConfig {
                max_attempts,
                ..Default::default()
            };
            let llm_display = agent_config.llm_base_url.trim_end_matches('/').to_string();
            eprintln!("{CYAN}▶ ForgeFleet Supervisor{RESET}  \x1b[2m{llm_display} · model={}{RESET}", agent_config.model);
            eprintln!("\x1b[2m  Task: {}{RESET}", &prompt[..prompt.len().min(80)]);
            eprintln!("\x1b[2m  Max attempts: {max_attempts}{RESET}");
            eprintln!();

            let result = ff_agent::supervisor::supervise(&prompt, agent_config, sup_config).await;

            eprintln!();
            if result.success {
                eprintln!("{GREEN}✓ Task completed on attempt {}/{max_attempts}{RESET}", result.attempts);
            } else {
                eprintln!("{RED}✗ Task failed after {} attempt(s){RESET}", result.attempts);
            }

            if !result.diagnoses.is_empty() {
                eprintln!();
                for d in &result.diagnoses {
                    let status = if d.attempt < result.attempts || result.success { "✓" } else { "✗" };
                    eprintln!("  \x1b[2mAttempt {}: [{status}] {} → {}\x1b[0m", d.attempt, d.failure_type, d.fix_applied);
                }
            }

            eprintln!();
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

    let mut app = App::new(config.clone()).await;

    // Warm the ff-agent fleet-info cache so system prompts include the live
    // fleet description on first session creation.
    let _ = ff_agent::fleet_info::ensure_fleet_description_cached().await;
    let _ = ff_agent::fleet_info::ensure_snapshot_cached().await;
    let commands = CommandRegistry::new();
    let mut command_list: Vec<(&str, &str)> = commands.list();
    // Add built-in TUI commands
    command_list.push(("new", "Start a new session tab"));
    command_list.push(("memory", "Search across all memory layers: /memory <query>"));
    command_list.push(("search", "Search memory: /search <query>"));
    command_list.push(("help", "Show available commands"));
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

        // Poll any in-flight async picker load
        poll_picker_load(app);

        // Poll async fleet health refresh result (non-blocking).
        poll_fleet_health_refresh(app);

        // Kick off a fleet health refresh every ~30s (20 fps × 30s = 600 frames).
        if app.frame % 600 == 0 && app.frame > 0 {
            kick_fleet_health_refresh(&app.fleet_nodes);
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
                // Modal: Model Picker overlay captures all key input.
                if app.picker.is_some() {
                    handle_picker_key(app, key);
                    continue;
                }

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

                    // Shift+Enter or Alt+Enter: insert newline for multi-line input
                    (KeyCode::Enter, m) if m.contains(KeyModifiers::SHIFT) || m.contains(KeyModifiers::ALT) => {
                        app.tab_mut().input.insert_newline();
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

                        // /model with no args → open interactive picker overlay
                        if trimmed == "/model" {
                            open_model_picker(app);
                            let tab = app.tab_mut();
                            tab.input.text.clear();
                            tab.input.cursor = 0;
                            tab.input.suggestions.clear();
                            tab.input.suggestion_index = None;
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
                    (KeyCode::Backspace, _) => {
                        app.tab_mut().input.backspace();
                        if app.tab().input.text.starts_with('/') {
                            app.tab_mut().input.compute_suggestions(command_list);
                        } else {
                            app.tab_mut().input.suggestions.clear();
                            app.tab_mut().input.suggestion_index = None;
                        }
                    }
                    (KeyCode::Delete, _) => {
                        app.tab_mut().input.delete();
                        if app.tab().input.text.starts_with('/') {
                            app.tab_mut().input.compute_suggestions(command_list);
                        } else {
                            app.tab_mut().input.suggestions.clear();
                            app.tab_mut().input.suggestion_index = None;
                        }
                    }
                    // Mac Option+Left/Right (and common Alt+Left/Right) — jump by word
                    (KeyCode::Left, m) if m.contains(KeyModifiers::ALT) => app.tab_mut().input.move_word_left(),
                    (KeyCode::Right, m) if m.contains(KeyModifiers::ALT) => app.tab_mut().input.move_word_right(),
                    (KeyCode::Left, _) => app.tab_mut().input.move_left(),
                    (KeyCode::Right, _) => app.tab_mut().input.move_right(),
                    (KeyCode::Home, _) => app.tab_mut().input.home(),
                    (KeyCode::End, _) => app.tab_mut().input.end(),
                    // Up/Down: priority order:
                    //   1. If suggestions popup is open → cycle through suggestions
                    //   2. Else if multi-line input → navigate within input
                    //   3. Else → history nav
                    (KeyCode::Up, _) => {
                        if !app.tab().input.suggestions.is_empty() {
                            app.tab_mut().input.prev_suggestion();
                        } else if !app.tab_mut().input.move_line_up() {
                            app.tab_mut().input.history_up();
                        }
                    }
                    (KeyCode::Down, _) => {
                        if !app.tab().input.suggestions.is_empty() {
                            app.tab_mut().input.next_suggestion();
                        } else if !app.tab_mut().input.move_line_down() {
                            app.tab_mut().input.history_down();
                        }
                    }

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

/// Summarize tool input for display — extract the most relevant parameter.
fn summarize_tool_input(tool_name: &str, input_json: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };

    // Pick the most meaningful field per tool
    let key = match tool_name {
        "Bash" => "command",
        "Read" => "file_path",
        "Write" => "file_path",
        "Edit" => "file_path",
        "Glob" => "pattern",
        "Grep" => "pattern",
        "WebFetch" | "WebSearch" => "url",
        "Agent" => "description",
        "Orchestrate" => "task",
        "TaskCreate" => "subject",
        "TaskUpdate" => "task_id",
        "SendMessage" => "to",
        _ => "",
    };

    if !key.is_empty() {
        if let Some(val) = v.get(key).and_then(|v| v.as_str()) {
            let truncated = &val[..val.len().min(60)];
            return truncated.replace('\n', " ").to_string();
        }
    }

    // Fallback: first string value in the object
    if let Some(obj) = v.as_object() {
        for (_, val) in obj.iter().take(1) {
            if let Some(s) = val.as_str() {
                return s[..s.len().min(60)].replace('\n', " ").to_string();
            }
        }
    }

    String::new()
}

// ─── Model Picker overlay ──────────────────────────────────────────────────

/// Open the model picker overlay and kick off async loading of fleet models.
fn open_model_picker(app: &mut ff_terminal::app::App) {
    use ff_terminal::app::ModelPicker;
    app.picker = Some(ModelPicker { loading: true, ..Default::default() });
    // Spawn background load. We poll `app.picker` synchronously, so write results into a shared slot.
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None::<Result<Vec<ff_terminal::app::ModelPickerItem>, String>>));
    let slot_clone = slot.clone();
    tokio::spawn(async move {
        let result = load_picker_items().await;
        if let Ok(mut g) = slot_clone.lock() {
            *g = Some(result);
        }
    });
    // Stash the slot on the picker via a polling field — store in a thread-local-ish way.
    // Simplest: poll once per frame in the main loop. We'll use a global static for the in-flight load.
    PICKER_LOAD_SLOT.lock().unwrap().replace(slot);
}

/// Global slot for in-flight picker load. Polled each frame by the main loop.
static PICKER_LOAD_SLOT: std::sync::Mutex<Option<std::sync::Arc<std::sync::Mutex<Option<Result<Vec<ff_terminal::app::ModelPickerItem>, String>>>>>> = std::sync::Mutex::new(None);

/// Drain the picker load slot if a result is available; install it onto the picker.
pub fn poll_picker_load(app: &mut ff_terminal::app::App) {
    let slot_opt = PICKER_LOAD_SLOT.lock().unwrap().clone();
    let Some(slot) = slot_opt else { return };
    let result = {
        let mut g = slot.lock().unwrap();
        g.take()
    };
    let Some(result) = result else { return };
    PICKER_LOAD_SLOT.lock().unwrap().take(); // clear
    if let Some(picker) = app.picker.as_mut() {
        picker.loading = false;
        match result {
            Ok(items) => { picker.items = items; picker.selected = 0; }
            Err(e) => { picker.error = Some(e); }
        }
    }
}

// ─── Periodic Fleet Health Refresh ─────────────────────────────────────────

/// Result slot for an in-flight health refresh. Keyed only by presence.
static FLEET_HEALTH_SLOT: std::sync::Mutex<Option<std::sync::Arc<std::sync::Mutex<Option<Vec<ff_terminal::app::FleetNode>>>>>> = std::sync::Mutex::new(None);

/// Kick off a background task that pings every node + its model endpoints.
/// Idempotent — if one is already in flight, this does nothing.
pub fn kick_fleet_health_refresh(current_nodes: &[ff_terminal::app::FleetNode]) {
    // Already a refresh in flight? Skip.
    {
        let guard = FLEET_HEALTH_SLOT.lock().unwrap();
        if guard.is_some() { return; }
    }
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
    *FLEET_HEALTH_SLOT.lock().unwrap() = Some(slot.clone());

    // Snapshot the current node list so the background task can work without sharing &mut.
    let nodes_snapshot = current_nodes.to_vec();

    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap_or_default();
        let mut refreshed = nodes_snapshot;
        for node in refreshed.iter_mut() {
            let daemon_url = format!("http://{}:{}/health", node.ip, ff_terminal::app::PORT_DAEMON);
            node.daemon_online = client.get(&daemon_url).send().await
                .map(|r| r.status().is_success()).unwrap_or(false);
            for model in node.models.iter_mut() {
                let model_url = format!("http://{}:{}/health", node.ip, model.port);
                model.online = client.get(&model_url).send().await
                    .map(|r| r.status().is_success()).unwrap_or(false);
            }
        }
        *slot.lock().unwrap() = Some(refreshed);
    });
}

/// Install the refreshed fleet node list if the background task is done.
pub fn poll_fleet_health_refresh(app: &mut ff_terminal::app::App) {
    let slot_opt = FLEET_HEALTH_SLOT.lock().unwrap().clone();
    let Some(slot) = slot_opt else { return };
    let result = {
        let mut g = slot.lock().unwrap();
        g.take()
    };
    let Some(fresh) = result else { return };
    *FLEET_HEALTH_SLOT.lock().unwrap() = None;
    app.fleet_nodes = fresh;
}

async fn load_picker_items() -> Result<Vec<ff_terminal::app::ModelPickerItem>, String> {
    use ff_terminal::app::{ModelPickerItem, PickerItemState};
    use std::collections::BTreeMap;

    // Connect to Postgres using ~/.forgefleet/fleet.toml (same pattern as fleet_nodes_from_db).
    let home = dirs::home_dir().ok_or_else(|| "no home dir".to_string())?;
    let config_path = home.join(".forgefleet/fleet.toml");
    let toml_str = std::fs::read_to_string(&config_path)
        .map_err(|e| format!("read fleet.toml: {e}"))?;
    let config: ff_core::config::FleetConfig = toml::from_str(&toml_str)
        .map_err(|e| format!("parse fleet.toml: {e}"))?;
    let db_url = config.database.url.trim().to_string();
    if db_url.is_empty() { return Err("database.url is empty in fleet.toml".into()); }
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(3))
        .connect(&db_url)
        .await
        .map_err(|e| format!("connect postgres: {e}"))?;

    // Fetch everything in parallel.
    let (catalog_r, library_r, deployments_r, nodes_r, jobs_running_r, jobs_queued_r) = tokio::join!(
        ff_db::pg_list_catalog(&pool),
        ff_db::pg_list_library(&pool, None),
        ff_db::pg_list_deployments(&pool, None),
        ff_db::pg_list_nodes(&pool),
        ff_db::pg_list_jobs(&pool, Some("running"), 50),
        ff_db::pg_list_jobs(&pool, Some("queued"), 50),
    );
    let catalog = catalog_r.map_err(|e| format!("list catalog: {e}"))?;
    let library = library_r.map_err(|e| format!("list library: {e}"))?;
    let deployments = deployments_r.map_err(|e| format!("list deployments: {e}"))?;
    let nodes = nodes_r.map_err(|e| format!("list nodes: {e}"))?;
    let mut jobs = jobs_running_r.map_err(|e| format!("list running jobs: {e}"))?;
    jobs.extend(jobs_queued_r.map_err(|e| format!("list queued jobs: {e}"))?);

    // Node name -> ip.
    let node_ip: std::collections::HashMap<String, String> = nodes
        .iter().map(|n| (n.name.clone(), n.ip.clone())).collect();

    // catalog_id -> CatMeta.
    #[derive(Clone)]
    struct CatMeta { name: String, tier: i32 }
    let cat_meta: std::collections::HashMap<String, CatMeta> = catalog
        .iter()
        .map(|c| (c.id.clone(), CatMeta { name: c.name.clone(), tier: c.tier }))
        .collect();

    #[derive(Default)]
    struct Agg {
        lib_nodes: Vec<String>,
        lib_runtime: Option<String>,
        lib_size_bytes: i64,
        deploy: Option<(String, String, i32, String)>, // (node, ip, port, runtime)
        deploy_healthy: bool,
        job: Option<(f32, String)>, // (pct, status)
    }
    let mut aggs: BTreeMap<String, Agg> = BTreeMap::new();
    for c in &catalog { aggs.entry(c.id.clone()).or_default(); }
    for l in &library {
        let a = aggs.entry(l.catalog_id.clone()).or_default();
        if !a.lib_nodes.contains(&l.node_name) { a.lib_nodes.push(l.node_name.clone()); }
        a.lib_runtime.get_or_insert_with(|| l.runtime.clone());
        a.lib_size_bytes = a.lib_size_bytes.max(l.size_bytes);
    }
    for d in &deployments {
        let Some(cid) = d.catalog_id.as_ref() else { continue };
        let a = aggs.entry(cid.clone()).or_default();
        let healthy = d.health_status == "healthy";
        if a.deploy.is_none() || (healthy && !a.deploy_healthy) {
            let ip = node_ip.get(&d.node_name).cloned().unwrap_or_default();
            a.deploy = Some((d.node_name.clone(), ip, d.port, d.runtime.clone()));
            a.deploy_healthy = healthy;
        }
    }
    for j in &jobs {
        if j.kind != "download" { continue; }
        let Some(cid) = j.target_catalog_id.as_ref() else { continue };
        let a = aggs.entry(cid.clone()).or_default();
        if a.job.as_ref().map(|(p, _)| j.progress_pct > *p).unwrap_or(true) {
            a.job = Some((j.progress_pct, j.status.clone()));
        }
    }

    let mut items: Vec<ModelPickerItem> = Vec::new();
    for (cid, a) in aggs.into_iter() {
        let meta = cat_meta.get(&cid).cloned()
            .unwrap_or(CatMeta { name: cid.clone(), tier: 0 });

        // State precedence: Loaded > Downloading > OnDisk > Catalog.
        let (state, endpoint, endpoint_display, progress_pct, detail, runtime, online) =
            if a.deploy_healthy {
                let (node, ip, port, runtime) = a.deploy.clone().unwrap();
                let endpoint = format!("http://{ip}:{port}");
                let disp = format!("{node} @ {ip}:{port}");
                (PickerItemState::Loaded, endpoint, Some(disp), None,
                 format!("on {node}"), Some(runtime), true)
            } else if a.job.is_some() {
                let (pct, status) = a.job.clone().unwrap();
                let tag = if status == "queued" { "queued" } else { "downloading" };
                (PickerItemState::Downloading, String::new(), None, Some(pct),
                 format!("{tag} {pct:.0}%"), a.lib_runtime.clone(), false)
            } else if !a.lib_nodes.is_empty() {
                let mut nodes_sorted = a.lib_nodes.clone();
                nodes_sorted.sort();
                let detail = if a.lib_size_bytes > 0 {
                    format!("on {} ({})", nodes_sorted.join(", "), human_bytes_i64(a.lib_size_bytes))
                } else {
                    format!("on {}", nodes_sorted.join(", "))
                };
                (PickerItemState::OnDisk, String::new(), None, None, detail, a.lib_runtime.clone(), false)
            } else if a.deploy.is_some() {
                let (node, _ip, _port, runtime) = a.deploy.clone().unwrap();
                (PickerItemState::OnDisk, String::new(), None, None,
                 format!("deploy unhealthy on {node}"), Some(runtime), false)
            } else {
                (PickerItemState::Catalog, String::new(), None, None,
                 "not yet on fleet".into(), None, false)
            };

        let mut nodes_v = a.lib_nodes.clone();
        nodes_v.sort();
        if let Some((n, _, _, _)) = a.deploy.as_ref() {
            if !nodes_v.contains(n) { nodes_v.push(n.clone()); }
        }

        items.push(ModelPickerItem {
            name: meta.name, tier: meta.tier, nodes: nodes_v,
            endpoint, online, state, endpoint_display, progress_pct, detail, runtime,
        });
    }

    fn state_rank(s: ff_terminal::app::PickerItemState) -> u8 {
        use ff_terminal::app::PickerItemState::*;
        match s { Auto => 0, Loaded => 1, Downloading => 2, OnDisk => 3, Catalog => 4 }
    }
    items.sort_by(|a, b| {
        state_rank(a.state).cmp(&state_rank(b.state))
            .then(b.tier.cmp(&a.tier))
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    // Build "auto" sentinel at the top.
    let leader_ip = nodes.iter().find(|n| n.role == "leader").map(|n| n.ip.clone())
        .unwrap_or_else(|| "127.0.0.1".into());
    let auto = ModelPickerItem {
        name: "auto".into(),
        tier: 99,
        nodes: vec!["router".into()],
        endpoint: format!("http://{leader_ip}:{}", ff_terminal::app::PORT_LLM),
        online: true,
        state: PickerItemState::Auto,
        endpoint_display: Some(format!("{leader_ip}:{}", ff_terminal::app::PORT_LLM)),
        progress_pct: None,
        detail: "fleet router".into(),
        runtime: None,
    };

    let mut out = Vec::with_capacity(items.len() + 1);
    out.push(auto);
    out.extend(items);
    Ok(out)
}

/// Human-readable bytes (i64) — tiny helper for the picker detail column.
fn human_bytes_i64(n: i64) -> String {
    if n < 0 { return "0 B".into(); }
    human_bytes(n as u64)
}

/// Handle a key press while the model picker overlay is active.
fn handle_picker_key(app: &mut ff_terminal::app::App, key: crossterm::event::KeyEvent) {
    use crossterm::event::{KeyCode, KeyModifiers};
    let Some(picker) = app.picker.as_mut() else { return };
    let visible = picker.visible_indices();
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => { app.picker = None; }
        (KeyCode::Up, _) => {
            if !visible.is_empty() {
                picker.selected = picker.selected.saturating_sub(1);
            }
        }
        (KeyCode::Down, _) => {
            if !visible.is_empty() && picker.selected + 1 < visible.len() {
                picker.selected += 1;
            }
        }
        (KeyCode::Backspace, _) => {
            picker.filter.pop();
            picker.selected = 0;
        }
        (KeyCode::Enter, _) => {
            use ff_terminal::app::PickerItemState;
            if let Some(&idx) = visible.get(picker.selected) {
                let chosen = picker.items[idx].clone();
                match chosen.state {
                    PickerItemState::Auto | PickerItemState::Loaded => {
                        app.config.llm_base_url = chosen.endpoint.clone();
                        app.config.model = chosen.name.clone();
                        app.tab_mut().current_model = chosen.name.clone();
                        let msg = format!("Switched to {} @ {}", chosen.name, chosen.endpoint);
                        app.tab_mut().messages.push(ff_terminal::messages::render_status(&msg));
                        app.picker = None;
                    }
                    PickerItemState::Downloading => {
                        let msg = format!("{} is still downloading; wait for it to finish.", chosen.name);
                        app.tab_mut().messages.push(ff_terminal::messages::render_status(&msg));
                        app.picker = None;
                    }
                    PickerItemState::OnDisk | PickerItemState::Catalog => {
                        let hint = if matches!(chosen.state, PickerItemState::OnDisk) {
                            format!("Model not loaded; use `ff model load {}` first.", chosen.name)
                        } else {
                            format!("Model not loaded; use `ff model download {}` and `ff model load {}` first.", chosen.name, chosen.name)
                        };
                        app.tab_mut().messages.push(ff_terminal::messages::render_status(&hint));
                        app.picker = None;
                    }
                }
            } else {
                app.picker = None;
            }
        }
        (KeyCode::Char(c), mods) if !mods.contains(KeyModifiers::CONTROL) && !mods.contains(KeyModifiers::ALT) => {
            picker.filter.push(c);
            picker.selected = 0;
        }
        _ => {}
    }
}

/// Whether to show a result preview for this tool.
fn should_show_result_preview(tool_name: &str) -> bool {
    matches!(tool_name,
        "Bash" | "WebSearch" | "WebFetch" | "Orchestrate" |
        "TaskCreate" | "TaskList" | "TaskGet" | "SendMessage"
    )
}

async fn run_headless(prompt: &str, config: AgentSessionConfig, output_format: &str) -> Result<()> {
    let is_json = output_format == "json";

    // Print session header
    if !is_json {
        let llm_display = config.llm_base_url.trim_end_matches('/').to_string();
        eprintln!("{CYAN}▶ ForgeFleet Agent{RESET}  \x1b[2m{llm_display} · model={}{RESET}", config.model);
        eprintln!();
    }

    let mut session = AgentSession::new(config);
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let prompt = prompt.to_string();

    let handle = tokio::spawn(async move { session.run(&prompt, Some(event_tx)).await });

    let mut events = Vec::new();
    while let Some(event) = event_rx.recv().await {
        if is_json { events.push(event); }
        else {
            match &event {
                AgentEvent::Status { message, .. } => {
                    eprintln!("\x1b[2m  → {message}\x1b[0m");
                }
                AgentEvent::TurnComplete { turn, .. } => {
                    eprintln!("\x1b[2m── turn {turn} ──────────────────────────────\x1b[0m");
                }
                AgentEvent::ToolStart { tool_name, input_json, .. } => {
                    let input_summary = summarize_tool_input(tool_name, input_json);
                    eprint!("{YELLOW}⚡ {tool_name}{RESET}");
                    if !input_summary.is_empty() {
                        eprint!("\x1b[2m({input_summary})\x1b[0m");
                    }
                    eprint!(" ");
                }
                AgentEvent::ToolEnd { tool_name, result, is_error, duration_ms, .. } => {
                    if *is_error {
                        eprintln!("{RED}✗ ({duration_ms}ms){RESET}");
                        let first_line = result.lines().next().unwrap_or("").trim();
                        if !first_line.is_empty() {
                            eprintln!("  {RED}{}{RESET}", &first_line[..first_line.len().min(120)]);
                        }
                    } else {
                        eprintln!("{GREEN}✓ ({duration_ms}ms){RESET}");
                        if should_show_result_preview(tool_name) {
                            let preview = result.trim();
                            if !preview.is_empty() {
                                let lines: Vec<&str> = preview.lines().take(3).collect();
                                for line in lines {
                                    let trimmed = line.trim();
                                    if !trimmed.is_empty() {
                                        eprintln!("  \x1b[2m{}\x1b[0m", &trimmed[..trimmed.len().min(120)]);
                                    }
                                }
                            }
                        }
                    }
                }
                AgentEvent::AssistantText { text, .. } => {
                    print!("{text}");
                }
                AgentEvent::Compaction { messages_before, messages_after, .. } => {
                    eprintln!("\x1b[2m  ⟳ context compacted: {messages_before} → {messages_after} messages\x1b[0m");
                }
                AgentEvent::TokenWarning { usage_pct, .. } => {
                    let pct = (*usage_pct * 100.0) as u32;
                    eprintln!("{YELLOW}  ⚠ context {pct}% full\x1b[0m");
                }
                AgentEvent::Error { message, .. } => {
                    eprintln!("{RED}  ✗ {message}{RESET}");
                }
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
                                    // Qwen and Gemma-4 (via MLX) both support OpenAI tool calling.
                                    // Check id/slug/name for "gemma-4" or "gemma4" to distinguish from older Gemma variants.
                                    let fam = m.family.to_lowercase();
                                    let id_lower = m.id.to_lowercase();
                                    let name_lower = m.name.to_lowercase();
                                    let is_gemma4 = (id_lower.contains("gemma-4") || id_lower.contains("gemma4")
                                        || name_lower.contains("gemma-4") || name_lower.contains("gemma4"))
                                        && fam.contains("gemma");
                                    let supports_tools = fam.contains("qwen") || is_gemma4;
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

async fn handle_status(p: &Path) -> Result<()> {
    // Cap total runtime at 15s.
    let fut = handle_status_inner(p.to_path_buf());
    match tokio::time::timeout(Duration::from_secs(15), fut).await {
        Ok(r) => r,
        Err(_) => {
            println!("{RED}✗ ff status timed out after 15s{RESET}");
            Ok(())
        }
    }
}

async fn handle_status_inner(p: PathBuf) -> Result<()> {
    println!("{CYAN}━━━ ForgeFleet Status ━━━{RESET}");

    // Load fleet.toml (needed for redis URL and as a fallback for DB URL).
    let fleet_cfg: Option<ff_core::config::FleetConfig> = fs::read_to_string(&p)
        .ok()
        .and_then(|s| toml::from_str(&s).ok());

    // ── 1. Database ────────────────────────────────────────────────────────
    print!("{CYAN}Database{RESET}  : ");
    let pool_res = tokio::time::timeout(
        Duration::from_secs(3),
        ff_agent::fleet_info::get_fleet_pool(),
    ).await;
    let pool_opt: Option<sqlx::PgPool> = match pool_res {
        Ok(Ok(pool)) => {
            // Count applied migrations.
            let migs: Option<i64> = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*)::bigint FROM _migrations"
            )
            .fetch_one(&pool)
            .await
            .ok();
            match migs {
                Some(n) => println!("{GREEN}✓ connected{RESET} ({n} migrations applied)"),
                None    => println!("{GREEN}✓ connected{RESET} (migrations table missing)"),
            }
            Some(pool)
        }
        Ok(Err(e)) => {
            println!("{RED}✗ unreachable{RESET} ({})", truncate(&e, 60));
            None
        }
        Err(_) => {
            println!("{RED}✗ unreachable{RESET} (timeout)");
            None
        }
    };

    // ── 2. Redis ───────────────────────────────────────────────────────────
    print!("{CYAN}Redis{RESET}     : ");
    let redis_url = fleet_cfg.as_ref()
        .map(|c| c.redis.url.clone())
        .unwrap_or_else(|| "redis://127.0.0.1:6379".to_string());
    match ping_redis(&redis_url).await {
        Ok(ms)   => println!("{GREEN}✓ PONG{RESET} ({redis_url}, {ms}ms)"),
        Err(e)   => println!("{RED}✗ unreachable{RESET} ({redis_url}) — {}", truncate(&e, 50)),
    }

    // ── 3. Fleet nodes ─────────────────────────────────────────────────────
    println!("{CYAN}Nodes{RESET}     :");
    let nodes: Vec<ff_db::FleetNodeRow> = match &pool_opt {
        Some(pool) => ff_db::pg_list_nodes(pool).await.unwrap_or_default(),
        None => Vec::new(),
    };
    if nodes.is_empty() {
        println!("  {YELLOW}(no nodes — DB unavailable or empty){RESET}");
    } else {
        // Probe SSH port 22 on each node in parallel.
        let probes: Vec<_> = nodes.iter().map(|n| {
            let ip = n.ip.clone();
            async move { tcp_probe(&ip, 22, Duration::from_secs(2)).await }
        }).collect();
        let online: Vec<bool> = futures::future::join_all(probes).await;
        for (n, up) in nodes.iter().zip(online.iter()) {
            let status = if *up {
                format!("{GREEN}online{RESET}")
            } else {
                format!("{RED}offline{RESET}")
            };
            println!(
                "  {:<10} {:<16} {:<10} {}",
                n.name, n.ip, n.runtime, status
            );
        }
    }

    // ── 4. Deployments ─────────────────────────────────────────────────────
    print!("{CYAN}Deployments{RESET}: ");
    if let Some(pool) = &pool_opt {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT health_status, COUNT(*)::bigint FROM fleet_model_deployments \
             GROUP BY health_status ORDER BY health_status"
        ).fetch_all(pool).await.unwrap_or_default();
        if rows.is_empty() {
            println!("{YELLOW}(none){RESET}");
        } else {
            let parts: Vec<String> = rows.iter().map(|(s, c)| {
                let color = match s.as_str() {
                    "healthy"   => GREEN,
                    "unhealthy" => RED,
                    "starting"  => YELLOW,
                    _           => RESET,
                };
                format!("{color}{s}={c}{RESET}")
            }).collect();
            println!("{}", parts.join("  "));
        }
    } else {
        println!("{RED}✗ unreachable{RESET}");
    }

    // ── 5. Model library ───────────────────────────────────────────────────
    print!("{CYAN}Library{RESET}   : ");
    if let Some(pool) = &pool_opt {
        let row: Option<(i64, i64)> = sqlx::query_as(
            "SELECT COUNT(*)::bigint, COALESCE(SUM(size_bytes), 0)::bigint FROM fleet_model_library"
        ).fetch_one(pool).await.ok();
        match row {
            Some((n, bytes)) => {
                let gib = (bytes as f64) / 1024.0 / 1024.0 / 1024.0;
                println!("{n} models, {gib:.1} GiB across fleet");
            }
            None => println!("{RED}✗ query failed{RESET}"),
        }
    } else {
        println!("{RED}✗ unreachable{RESET}");
    }

    // ── 6. Catalog ─────────────────────────────────────────────────────────
    print!("{CYAN}Catalog{RESET}   : ");
    if let Some(pool) = &pool_opt {
        let n: Option<i64> = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM fleet_model_catalog"
        ).fetch_one(pool).await.ok();
        match n {
            Some(n) => println!("{n} entries"),
            None    => println!("{RED}✗ query failed{RESET}"),
        }
    } else {
        println!("{RED}✗ unreachable{RESET}");
    }

    // ── 7. Disk usage ──────────────────────────────────────────────────────
    println!("{CYAN}Disk{RESET}      :");
    if let Some(pool) = &pool_opt {
        // Latest sample per node.
        let rows: Vec<(String, i64, i64, i64, i32)> = sqlx::query_as(
            "SELECT DISTINCT ON (d.node_name) \
                    d.node_name, d.total_bytes, d.used_bytes, d.models_bytes, \
                    COALESCE(n.disk_quota_pct, 80) \
             FROM fleet_disk_usage d \
             LEFT JOIN fleet_nodes n ON n.name = d.node_name \
             ORDER BY d.node_name, d.sampled_at DESC"
        ).fetch_all(pool).await.unwrap_or_default();
        if rows.is_empty() {
            println!("  {YELLOW}(no samples yet){RESET}");
        } else {
            for (name, total, used, models, quota) in rows {
                let total_gib = (total as f64) / 1024.0 / 1024.0 / 1024.0;
                let used_gib  = (used  as f64) / 1024.0 / 1024.0 / 1024.0;
                let models_gib= (models as f64) / 1024.0 / 1024.0 / 1024.0;
                let used_pct = if total > 0 { (used as f64 / total as f64) * 100.0 } else { 0.0 };
                let over = used_pct >= quota as f64;
                let line = format!(
                    "  {:<10} {:5.1}/{:5.1} GiB ({:4.1}%)  models {:5.1} GiB  quota {}%",
                    name, used_gib, total_gib, used_pct, models_gib, quota
                );
                if over { println!("{RED}{line}{RESET}"); } else { println!("{line}"); }
            }
        }
    } else {
        println!("  {RED}✗ unreachable{RESET}");
    }

    // ── 8. Deferred tasks ──────────────────────────────────────────────────
    print!("{CYAN}Deferred{RESET}  : ");
    if let Some(pool) = &pool_opt {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT status, COUNT(*)::bigint FROM deferred_tasks \
             GROUP BY status ORDER BY status"
        ).fetch_all(pool).await.unwrap_or_default();
        if rows.is_empty() {
            println!("{YELLOW}(none){RESET}");
        } else {
            let parts: Vec<String> = rows.iter().map(|(s, c)| {
                if s == "failed" && *c > 0 {
                    format!("{RED}{s}={c}{RESET}")
                } else {
                    format!("{s}={c}")
                }
            }).collect();
            println!("{}", parts.join("  "));
        }
    } else {
        println!("{RED}✗ unreachable{RESET}");
    }

    // ── 9. In-flight jobs ──────────────────────────────────────────────────
    print!("{CYAN}Jobs{RESET}      : ");
    if let Some(pool) = &pool_opt {
        let n: Option<i64> = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM fleet_model_jobs WHERE status IN ('running','queued')"
        ).fetch_one(pool).await.ok();
        match n {
            Some(0) => println!("0 in-flight"),
            Some(n) => println!("{YELLOW}{n} in-flight{RESET} (running or queued)"),
            None    => println!("{RED}✗ query failed{RESET}"),
        }
    } else {
        println!("{RED}✗ unreachable{RESET}");
    }

    // ── 10. Secrets ───────────────────────────────────────────────────────
    print!("{CYAN}Secrets{RESET}   : ");
    if let Some(pool) = &pool_opt {
        let keys: Vec<(String,)> = sqlx::query_as(
            "SELECT key FROM fleet_secrets ORDER BY key"
        ).fetch_all(pool).await.unwrap_or_default();
        if keys.is_empty() {
            println!("{YELLOW}(none){RESET}");
        } else {
            let list: Vec<String> = keys.into_iter().map(|(k,)| k).collect();
            println!("{}", list.join(", "));
        }
    } else {
        println!("{RED}✗ unreachable{RESET}");
    }

    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() } else { format!("{}…", &s[..max]) }
}

async fn tcp_probe(host: &str, port: u16, timeout: Duration) -> bool {
    let addr = format!("{host}:{port}");
    match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(&addr)).await {
        Ok(Ok(_)) => true,
        _ => false,
    }
}

/// Lightweight Redis PING — speaks RESP directly without a redis client dep.
async fn ping_redis(url: &str) -> std::result::Result<u128, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Parse redis://host:port (ignore auth/db for this health ping).
    let rest = url.strip_prefix("redis://").unwrap_or(url);
    let host_port = rest.split('/').next().unwrap_or(rest);
    // Strip userinfo if present.
    let host_port = host_port.rsplit('@').next().unwrap_or(host_port);
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(6379)),
        None => (host_port.to_string(), 6379),
    };

    let start = std::time::Instant::now();
    let connect = tokio::net::TcpStream::connect((host.as_str(), port));
    let mut stream = tokio::time::timeout(Duration::from_secs(3), connect)
        .await
        .map_err(|_| "connect timeout".to_string())?
        .map_err(|e| format!("connect: {e}"))?;

    tokio::time::timeout(Duration::from_secs(3), stream.write_all(b"PING\r\n"))
        .await
        .map_err(|_| "write timeout".to_string())?
        .map_err(|e| format!("write: {e}"))?;

    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buf))
        .await
        .map_err(|_| "read timeout".to_string())?
        .map_err(|e| format!("read: {e}"))?;

    let reply = String::from_utf8_lossy(&buf[..n]);
    if reply.starts_with("+PONG") {
        Ok(start.elapsed().as_millis())
    } else {
        Err(format!("unexpected reply: {}", reply.trim()))
    }
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

async fn handle_health(c: &AgentSessionConfig) -> Result<()> {
    let nodes = load_fleet_nodes_for_health(c).await;
    let client = std::sync::Arc::new(
        reqwest::Client::builder().timeout(Duration::from_millis(2500)).build()?
    );

    // Check all nodes in parallel
    let futs: Vec<_> = nodes.iter().map(|(name, ip, port)| {
        let client = client.clone();
        let url = format!("http://{ip}:{port}/health");
        let agent_url = format!("http://{ip}:50002/health");
        let name = name.clone();
        let ip = ip.clone();
        let port = *port;
        async move {
            let daemon_ok = client.get(&url).send().await.map(|r| r.status().is_success()).unwrap_or(false);
            let agent_ok = client.get(&agent_url).send().await.map(|r| r.status().is_success()).unwrap_or(false);
            (name, ip, port, daemon_ok, agent_ok)
        }
    }).collect();

    let results = futures::future::join_all(futs).await;

    println!("{GREEN}✓ ForgeFleet Health{RESET}");
    for (name, ip, port, daemon_ok, agent_ok) in results {
        let daemon_str = if daemon_ok { format!("{GREEN}ONLINE{RESET}") } else { format!("{RED}OFFLINE{RESET}") };
        let agent_str = if agent_ok { format!("  agent{GREEN}✓{RESET}") } else { format!("  agent{YELLOW}✗{RESET}") };
        println!("  {name:<12} {ip}:{port}  {daemon_str}{agent_str}");
    }
    Ok(())
}

async fn load_fleet_nodes_for_health(c: &AgentSessionConfig) -> Vec<(String, String, u16)> {
    // Try Postgres first
    let config_path = dirs::home_dir()
        .unwrap_or_default()
        .join(".forgefleet/fleet.toml");

    if let Ok(toml_str) = fs::read_to_string(&config_path) {
        if let Ok(cfg) = toml::from_str::<ff_core::config::FleetConfig>(&toml_str) {
            let db_url = cfg.database.url.trim().to_string();
            if !db_url.is_empty() {
                if let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
                    .max_connections(1)
                    .acquire_timeout(Duration::from_secs(3))
                    .connect(&db_url)
                    .await
                {
                    let rows: Vec<(String, String)> = sqlx::query_as(
                        "SELECT name, ip FROM fleet_nodes ORDER BY election_priority, name"
                    )
                    .fetch_all(&pool)
                    .await
                    .unwrap_or_default();

                    if !rows.is_empty() {
                        return rows.into_iter()
                            .map(|(n, ip)| (n, ip, 51000u16))
                            .collect();
                    }
                }
            }
        }
    }

    // Fallback: probe the local daemon + known hardcoded list
    let _ = c; // suppress unused warning
    vec![
        ("Taylor".into(), "192.168.5.100".into(), 51000),
        ("Marcus".into(), "192.168.5.102".into(), 51000),
        ("Sophie".into(), "192.168.5.103".into(), 51000),
        ("Priya".into(),  "192.168.5.104".into(), 51000),
        ("James".into(),  "192.168.5.108".into(), 51000),
        ("Logan".into(),  "192.168.5.111".into(), 51000),
        ("Lily".into(),   "192.168.5.113".into(), 51000),
        ("Veronica".into(),"192.168.5.112".into(), 51000),
        ("Duncan".into(), "192.168.5.114".into(), 51000),
        ("Aura".into(),   "192.168.5.110".into(), 51000),
    ]
}

async fn handle_secrets(cmd: SecretsCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    // Ensure secrets table + other Postgres migrations are applied. Idempotent.
    ff_db::run_postgres_migrations(&pool).await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;
    match cmd {
        SecretsCommand::List => {
            let rows = ff_db::pg_list_secrets(&pool).await?;
            if rows.is_empty() {
                println!("(no secrets stored)");
                return Ok(());
            }
            println!("{:<28} {:<14} {:<20} {}", "KEY", "UPDATED BY", "UPDATED AT", "DESCRIPTION");
            for (key, desc, updated_by, updated_at) in rows {
                let ts = updated_at.format("%Y-%m-%d %H:%M UTC").to_string();
                println!(
                    "{:<28} {:<14} {:<20} {}",
                    key,
                    updated_by.unwrap_or_else(|| "-".into()),
                    ts,
                    desc.unwrap_or_default()
                );
            }
        }
        SecretsCommand::Get { key } => {
            match ff_db::pg_get_secret(&pool, &key).await? {
                Some(value) => println!("{value}"),
                None => {
                    eprintln!("No secret set for key: {key}");
                    std::process::exit(1);
                }
            }
        }
        SecretsCommand::Set { key, value, description } => {
            let who = whoami_tag();
            ff_db::pg_set_secret(&pool, &key, &value, description.as_deref(), Some(&who)).await?;
            println!("Secret '{key}' stored ({} bytes) by {who}", value.len());
        }
        SecretsCommand::Delete { key } => {
            let deleted = ff_db::pg_delete_secret(&pool, &key).await?;
            if deleted {
                println!("Deleted secret '{key}'");
            } else {
                println!("No secret with key '{key}' to delete");
            }
        }
    }
    Ok(())
}

/// POSIX shell single-quote escape: wraps the argument in single quotes and
/// escapes any embedded single quotes. Safe for pasting into `sh -c`.
fn shell_escape_single(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Best-effort tag for `updated_by`: `user@host`.
fn whoami_tag() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    let host = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    format!("{user}@{host}")
}

async fn handle_defer(cmd: DeferCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool).await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;
    match cmd {
        DeferCommand::List { status, limit } => {
            let rows = ff_db::pg_list_deferred(&pool, status.as_deref(), limit).await?;
            if rows.is_empty() {
                println!("(no deferred tasks)");
                return Ok(());
            }
            println!(
                "{:<38} {:<10} {:<12} {:<16} {:<6} {}",
                "ID", "STATUS", "TRIGGER", "TARGET", "TRY", "TITLE"
            );
            for r in rows {
                let trigger = format!(
                    "{}",
                    match r.trigger_type.as_str() {
                        "node_online" => r.trigger_spec.get("node").and_then(|v| v.as_str()).map(|n| format!("node={n}")).unwrap_or_else(|| "node_online".into()),
                        "at_time" => r.trigger_spec.get("at").and_then(|v| v.as_str()).unwrap_or("at_time").to_string(),
                        other => other.to_string(),
                    }
                );
                let target = r.preferred_node.clone().unwrap_or_else(|| "-".into());
                println!(
                    "{:<38} {:<10} {:<12} {:<16} {:<6} {}",
                    r.id,
                    r.status,
                    trigger,
                    target,
                    format!("{}/{}", r.attempts, r.max_attempts),
                    r.title
                );
            }
        }
        DeferCommand::AddShell { title, run, when_node_online, when_at, on_node, max_attempts } => {
            let (trigger_type, trigger_spec, preferred_node) =
                if let Some(node) = when_node_online.clone() {
                    (
                        "node_online".to_string(),
                        serde_json::json!({"node": node}),
                        on_node.clone().or(Some(node)),
                    )
                } else if let Some(at) = when_at {
                    ("at_time".to_string(), serde_json::json!({"at": at}), on_node.clone())
                } else {
                    anyhow::bail!("must specify --when-node-online <node> or --when-at <rfc3339>");
                };

            let payload = serde_json::json!({
                "command": run,
            });
            let id = ff_db::pg_enqueue_deferred(
                &pool,
                &title,
                "shell",
                &payload,
                &trigger_type,
                &trigger_spec,
                preferred_node.as_deref(),
                &serde_json::json!([]),
                Some(&whoami_tag()),
                Some(max_attempts),
            )
            .await?;
            println!("Enqueued deferred task: {id}");
            println!("  title:         {title}");
            println!("  kind:          shell");
            println!("  trigger:       {trigger_type} ({trigger_spec})");
            if let Some(n) = &preferred_node {
                println!("  runs on node:  {n}");
            }
            println!("  max attempts:  {max_attempts}");
            println!();
            println!("NOTE: executor loop is not yet running. Task is captured durably in Postgres");
            println!("      and will begin processing once `forgefleetd defer-worker` is live.");
        }
        DeferCommand::Get { id } => {
            match ff_db::pg_get_deferred(&pool, &id).await? {
                Some(r) => {
                    println!("ID:            {}", r.id);
                    println!("Title:         {}", r.title);
                    println!("Status:        {}", r.status);
                    println!("Kind:          {}", r.kind);
                    println!("Trigger:       {} ({})", r.trigger_type, r.trigger_spec);
                    println!("Preferred node:{}", r.preferred_node.clone().unwrap_or_else(|| "-".into()));
                    println!("Attempts:      {}/{}", r.attempts, r.max_attempts);
                    println!("Created:       {}  by {}", r.created_at.format("%Y-%m-%d %H:%M UTC"), r.created_by.clone().unwrap_or_else(|| "-".into()));
                    if let Some(ts) = r.next_attempt_at { println!("Next attempt:  {}", ts.format("%Y-%m-%d %H:%M UTC")); }
                    if let Some(n) = &r.claimed_by { println!("Claimed by:    {n}"); }
                    if let Some(err) = &r.last_error { println!("Last error:    {err}"); }
                    if let Some(res) = &r.result { println!("Result:        {res}"); }
                    println!("\nPayload:\n{}", serde_json::to_string_pretty(&r.payload).unwrap_or_default());
                }
                None => {
                    eprintln!("No deferred task with id '{id}'");
                    std::process::exit(1);
                }
            }
        }
        DeferCommand::Cancel { id } => {
            if ff_db::pg_cancel_deferred(&pool, &id).await? {
                println!("Cancelled task {id}");
            } else {
                println!("Task {id} is not in a cancellable state (or does not exist)");
            }
        }
        DeferCommand::Retry { id } => {
            if ff_db::pg_retry_deferred(&pool, &id).await? {
                println!("Task {id} requeued for retry (status=pending)");
            } else {
                println!("Task {id} is not in a retryable state (must be failed or cancelled)");
            }
        }
    }
    Ok(())
}

// ─── Model lifecycle CLI ───────────────────────────────────────────────────

async fn handle_model(cmd: ModelCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool).await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        ModelCommand::SyncCatalog => {
            let n = ff_agent::model_catalog::sync_catalog(&pool).await
                .map_err(|e| anyhow::anyhow!(e))?;
            println!("Synced {n} catalog entries from TOML to Postgres");
        }
        ModelCommand::Search { query } => {
            let rows = ff_db::pg_search_catalog(&pool, &query).await?;
            if rows.is_empty() {
                println!("(no catalog matches for \"{query}\")");
                return Ok(());
            }
            println!("{:<28} {:<10} {:<6} {:<7} {}", "ID", "FAMILY", "TIER", "GATED", "NAME");
            for r in rows {
                let gated = if r.gated { "yes" } else { "-" };
                println!("{:<28} {:<10} T{:<5} {:<7} {}", r.id, r.family, r.tier, gated, r.name);
            }
        }
        ModelCommand::Catalog => {
            let rows = ff_db::pg_list_catalog(&pool).await?;
            if rows.is_empty() {
                println!("(catalog empty — run `ff model sync-catalog` first)");
                return Ok(());
            }
            println!("{:<28} {:<10} {:<6} {:<7} {:<7} {}", "ID", "FAMILY", "TIER", "PARAMS", "GATED", "NAME");
            for r in rows {
                let gated = if r.gated { "yes" } else { "-" };
                println!("{:<28} {:<10} T{:<5} {:<7} {:<7} {}",
                    r.id, r.family, r.tier, r.parameters, gated, r.name);
            }
        }
        ModelCommand::Library { node } => {
            let rows = ff_db::pg_list_library(&pool, node.as_deref()).await?;
            if rows.is_empty() {
                println!("(library empty — run `ff model scan` to index your local models dir)");
                return Ok(());
            }
            println!("{:<10} {:<28} {:<10} {:<10} {:<10} {}", "NODE", "CATALOG_ID", "RUNTIME", "QUANT", "SIZE", "PATH");
            for r in rows {
                let sz = human_bytes(r.size_bytes as u64);
                let quant = r.quant.clone().unwrap_or_else(|| "-".into());
                println!("{:<10} {:<28} {:<10} {:<10} {:<10} {}",
                    r.node_name, r.catalog_id, r.runtime, quant, sz, r.file_path);
            }
        }
        ModelCommand::Deployments { node } => {
            let rows = ff_db::pg_list_deployments(&pool, node.as_deref()).await?;
            if rows.is_empty() {
                println!("(no deployments recorded)");
                return Ok(());
            }
            println!("{:<10} {:<28} {:<10} {:<6} {:<10} {}", "NODE", "CATALOG_ID", "RUNTIME", "PORT", "HEALTH", "STARTED");
            for r in rows {
                let catalog = r.catalog_id.clone().unwrap_or_else(|| "-".into());
                println!("{:<10} {:<28} {:<10} {:<6} {:<10} {}",
                    r.node_name, catalog, r.runtime, r.port, r.health_status,
                    r.started_at.format("%Y-%m-%d %H:%M UTC"));
            }
        }
        ModelCommand::Scan { node, models_dir } => {
            // Default: resolve this host's node name from Postgres by IP.
            let node_name = match node {
                Some(n) => n,
                None => ff_agent::fleet_info::resolve_this_node_name().await,
            };
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
            let default_dir = PathBuf::from(home).join("models");
            let dir = models_dir.unwrap_or(default_dir);

            if !dir.exists() {
                anyhow::bail!("models dir does not exist: {}", dir.display());
            }
            println!("Scanning {} on node {} ...", dir.display(), node_name);
            let summary = ff_agent::model_library_scanner::scan_local_library(&pool, &node_name, &dir)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            println!("  added:   {}", summary.added);
            println!("  updated: {}", summary.updated);
            println!("  removed: {}", summary.removed);
            println!("  total:   {} across models dir", human_bytes(summary.total_bytes));
        }
        ModelCommand::Disk => {
            let rows = ff_db::pg_latest_disk_usage(&pool).await?;
            if rows.is_empty() {
                println!("(no disk usage samples yet — the daemon records these periodically)");
                return Ok(());
            }
            println!("{:<10} {:<24} {:<10} {:<10} {:<10} {}", "NODE", "MODELS_DIR", "FREE", "USED", "MODELS", "SAMPLED");
            for (node, dir, total, used, free, models_sz, ts) in rows {
                let _ = total;
                println!("{:<10} {:<24} {:<10} {:<10} {:<10} {}",
                    node,
                    dir,
                    human_bytes(free as u64),
                    human_bytes(used as u64),
                    human_bytes(models_sz as u64),
                    ts.format("%Y-%m-%d %H:%M UTC")
                );
            }
        }
        ModelCommand::Download { id, runtime, node, force } => {
            // Resolve target node + node runtime + models_dir.
            let node_name = match node {
                Some(n) => n,
                None => ff_agent::fleet_info::resolve_this_node_name().await,
            };
            let node_row = ff_db::pg_get_node(&pool, &node_name)
                .await?
                .ok_or_else(|| anyhow::anyhow!("node '{node_name}' not in fleet_nodes"))?;
            let target_runtime = runtime.unwrap_or_else(|| node_row.runtime.clone());
            if target_runtime == "unknown" {
                anyhow::bail!("node '{node_name}' has unknown runtime; set with: ff config set fleet.{node_name}.runtime mlx|llama.cpp|vllm");
            }

            // Lookup catalog entry; pick variant for runtime.
            let catalog = ff_db::pg_get_catalog(&pool, &id).await?
                .ok_or_else(|| anyhow::anyhow!("no catalog entry with id '{id}' (try `ff model search`)"))?;
            let variants = catalog.variants.as_array()
                .ok_or_else(|| anyhow::anyhow!("catalog variants for '{id}' is not an array"))?;
            let variant = variants.iter().find(|v| {
                v.get("runtime").and_then(|x| x.as_str()) == Some(target_runtime.as_str())
            }).ok_or_else(|| {
                let available: Vec<String> = variants.iter()
                    .filter_map(|v| v.get("runtime").and_then(|x| x.as_str()).map(String::from))
                    .collect();
                anyhow::anyhow!("no variant for runtime '{target_runtime}' on '{id}'. available: {}", available.join(", "))
            })?;

            let hf_repo = variant.get("hf_repo").and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("variant missing hf_repo"))?;
            let quant = variant.get("quant").and_then(|v| v.as_str()).map(String::from);
            let size_gb = variant.get("size_gb").and_then(|v| v.as_f64()).unwrap_or(0.0);

            // Cross-node downloads are dispatched via the deferred task queue: a
            // defer-worker running on the target node will claim it and run
            // `ff model download <id> --runtime <rt>` locally there.
            let this_node = ff_agent::fleet_info::resolve_this_node_name().await;
            if node_name != this_node {
                let escaped_id = shell_escape_single(&id);
                let command = format!(
                    "ff model download {} --runtime {}",
                    escaped_id, target_runtime
                );
                let title = format!(
                    "Download {} ({} variant) on {}",
                    id, target_runtime, node_name
                );
                let payload = serde_json::json!({ "command": command });
                let trigger_spec = serde_json::json!({});
                let defer_id = ff_db::pg_enqueue_deferred(
                    &pool,
                    &title,
                    "shell",
                    &payload,
                    "now",
                    &trigger_spec,
                    Some(&node_name),
                    &serde_json::json!([]),
                    Some(&whoami_tag()),
                    Some(3),
                )
                .await?;
                println!(
                    "Enqueued cross-node download as deferred task {defer_id}. It will run on {node_name} when a defer-worker there claims it."
                );
                println!("Check status with: ff defer list");
                return Ok(());
            }

            // Compute destination dir under models_dir.
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
            let models_dir = expand_tilde(&node_row.models_dir, &home);
            let dest = models_dir.join(&id);

            // HF token (optional — gated models need it).
            let token = ff_agent::fleet_info::get_hf_token().await;
            if catalog.gated && token.is_none() {
                anyhow::bail!("model '{id}' is gated on HF; set token first with: ff secrets set huggingface.token <hf_xxx>");
            }

            // Allow patterns: prefer runtime-specific glob to avoid pulling everything.
            let allow_patterns: Vec<String> = match target_runtime.as_str() {
                "llama.cpp" => vec!["*.gguf".into(), "tokenizer*".into(), "*config*".into()],
                "mlx" | "vllm" => vec![
                    "*.safetensors".into(),
                    "*.json".into(),
                    "tokenizer*".into(),
                    "*config*".into(),
                    "README*".into(),
                ],
                other => vec![format!("*.{other}")],
            };
            let deny_patterns: Vec<String> = vec!["*.f16*".into(), "*.bf16*".into()];

            let _ = force; // not yet used; resume-by-size is automatic

            // Create job row for tracking.
            let params = serde_json::json!({
                "hf_repo": hf_repo,
                "runtime": target_runtime,
                "quant": quant,
                "dest": dest.to_string_lossy(),
            });
            let job_id = ff_db::pg_create_job(
                &pool, &node_name, "download",
                Some(&id), None, &params,
            ).await?;
            ff_db::pg_update_job_progress(
                &pool, &job_id, Some("running"), Some(0.0), None, None, None, None,
            ).await?;

            println!("{CYAN}▶ Downloading {} ({})\n  source: {}\n  dest:   {}\n  job:    {}{RESET}",
                catalog.name, target_runtime, hf_repo, dest.display(), job_id);
            if size_gb > 0.0 { println!("  estimated size: {size_gb:.1} GB"); }

            // Run download with progress callback.
            let pool_for_progress = pool.clone();
            let job_id_for_progress = job_id.clone();
            let mut last_pct = -1i32;
            let opts = ff_agent::hf_download::DownloadOptions {
                repo: hf_repo.to_string(),
                revision: None,
                dest_dir: dest.clone(),
                token: token.clone(),
                allow_patterns,
                deny_patterns,
                skip_verify: false,
            };

            let result = ff_agent::hf_download::download_repo(opts, move |p| {
                let pct = p.percent as i32;
                if pct != last_pct {
                    last_pct = pct;
                    let bar_w = 30;
                    let filled = (bar_w as f32 * p.percent / 100.0) as usize;
                    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(bar_w - filled));
                    let done_mb = p.bytes_done / (1u64 << 20);
                    let total_mb = p.bytes_total / (1u64 << 20);
                    eprint!("\r  [{bar}] {pct:>3}%  {done_mb}/{total_mb} MiB  {}", trunc_for_status(&p.file, 40));
                    use std::io::Write as _;
                    let _ = std::io::stderr().flush();
                    // Update DB job (fire and forget — best effort)
                    let pool2 = pool_for_progress.clone();
                    let jid = job_id_for_progress.clone();
                    let bd = p.bytes_done as i64;
                    let bt = p.bytes_total as i64;
                    let pp = p.percent;
                    tokio::spawn(async move {
                        let _ = ff_db::pg_update_job_progress(
                            &pool2, &jid, None, Some(pp), Some(bd), Some(bt), None, None,
                        ).await;
                    });
                }
            }).await;
            eprintln!(); // newline after progress bar

            match result {
                Ok(files) => {
                    println!("{CYAN}✓ Downloaded {} file(s){RESET}", files.len());
                    let _ = ff_db::pg_update_job_progress(
                        &pool, &job_id, Some("completed"), Some(100.0), None, None, None, None,
                    ).await;
                    // Re-scan node so library reflects the new model.
                    println!("Re-scanning library...");
                    let summary = ff_agent::model_library_scanner::scan_local_library(
                        &pool, &node_name, &models_dir
                    ).await.map_err(|e| anyhow::anyhow!(e))?;
                    println!("  added: {}, updated: {}", summary.added, summary.updated);
                }
                Err(e) => {
                    let _ = ff_db::pg_update_job_progress(
                        &pool, &job_id, Some("failed"), None, None, None, None, Some(&e),
                    ).await;
                    anyhow::bail!("download failed: {e}");
                }
            }
        }
        ModelCommand::DownloadBatch { node, ids } => {
            if ids.is_empty() {
                anyhow::bail!("no catalog ids provided; usage: ff model download-batch --node <name> <id>...");
            }
            // Resolve target node + its runtime.
            let node_row = ff_db::pg_get_node(&pool, &node)
                .await?
                .ok_or_else(|| anyhow::anyhow!("node '{node}' not in fleet_nodes"))?;
            let target_runtime = node_row.runtime.clone();
            if target_runtime == "unknown" {
                anyhow::bail!("node '{node}' has unknown runtime; set with: ff config set fleet.{node}.runtime mlx|llama.cpp|vllm");
            }

            // Validate every id exists in the catalog BEFORE enqueuing anything.
            for id in &ids {
                if ff_db::pg_get_catalog(&pool, id).await?.is_none() {
                    anyhow::bail!("no catalog entry with id '{id}' (try `ff model search`)");
                }
            }

            let who = whoami_tag();
            let mut enqueued: Vec<(String, String)> = Vec::with_capacity(ids.len());
            for id in &ids {
                let escaped_id = shell_escape_single(id);
                let command = format!(
                    "ff model download {} --runtime {}",
                    escaped_id, target_runtime
                );
                let title = format!(
                    "Download {} ({} variant) on {}",
                    id, target_runtime, node
                );
                let payload = serde_json::json!({ "command": command });
                let trigger_spec = serde_json::json!({});
                let defer_id = ff_db::pg_enqueue_deferred(
                    &pool,
                    &title,
                    "shell",
                    &payload,
                    "now",
                    &trigger_spec,
                    Some(&node),
                    &serde_json::json!([]),
                    Some(&who),
                    Some(3),
                )
                .await?;
                enqueued.push((id.clone(), defer_id));
            }

            println!("Enqueued {} cross-node downloads on '{}':", enqueued.len(), node);
            for (id, defer_id) in &enqueued {
                println!("  {defer_id}  {id}");
            }
            println!("Check status with: ff defer list");
        }
        ModelCommand::Delete { id, yes } => {
            // Look up library row.
            let all = ff_db::pg_list_library(&pool, None).await?;
            let row = all.iter().find(|r| r.id == id)
                .ok_or_else(|| anyhow::anyhow!("no library entry with id '{id}' (try `ff model library`)"))?;

            // Safety: refuse if a deployment references this library row.
            let deployments = ff_db::pg_list_deployments(&pool, Some(&row.node_name)).await?;
            let in_use = deployments.iter().any(|d| d.library_id.as_deref() == Some(&id));
            if in_use {
                anyhow::bail!("model is currently deployed on {} — unload it first (`ff model unload <deployment_id>`)", row.node_name);
            }

            // Cross-node delete not yet wired — only this host.
            let this_node = ff_agent::fleet_info::resolve_this_node_name().await;
            if row.node_name != this_node {
                anyhow::bail!("cross-node delete not yet implemented. run on '{}' instead.", row.node_name);
            }

            if !yes {
                println!("This will delete {} ({}) from disk. Re-run with --yes to confirm.",
                    row.file_path, human_bytes(row.size_bytes as u64));
                return Ok(());
            }

            let path = std::path::Path::new(&row.file_path);
            let result = if path.is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            };
            match result {
                Ok(()) => {
                    let _ = ff_db::pg_delete_library(&pool, &id).await?;
                    println!("Deleted {} ({}) from {}", row.file_path, human_bytes(row.size_bytes as u64), row.node_name);
                }
                Err(e) => anyhow::bail!("filesystem remove failed: {e}"),
            }
        }
        ModelCommand::Load { id, port, ctx, parallel } => {
            let opts = ff_agent::model_runtime::LoadOptions {
                library_id: id.clone(),
                port,
                context_size: ctx,
                parallel,
            };
            println!("{CYAN}▶ Loading library {} on port {port}...{RESET}", id);
            match ff_agent::model_runtime::load_model(&pool, opts).await {
                Ok(res) => {
                    println!("{CYAN}✓ Loaded{RESET} — deployment {} pid {} @ http://127.0.0.1:{}",
                        res.deployment_id, res.pid, res.port);
                }
                Err(e) => anyhow::bail!("load failed: {e}"),
            }
        }
        ModelCommand::Autoload { catalog_id, ctx } => {
            let node_name = ff_agent::fleet_info::resolve_this_node_name().await;

            // 1. Already deployed?
            let deps = ff_db::pg_list_deployments(&pool, Some(&node_name)).await?;
            if let Some(d) = deps.iter().find(|d| d.catalog_id.as_deref() == Some(&catalog_id) && d.health_status == "healthy") {
                println!("Already deployed on port {} (deployment {})", d.port, d.id);
                return Ok(());
            }

            // 2. Find library row on this node for this catalog_id.
            let libs = ff_db::pg_list_library(&pool, Some(&node_name)).await?;
            let lib = libs.iter().find(|r| r.catalog_id == catalog_id)
                .ok_or_else(|| anyhow::anyhow!("model '{catalog_id}' not in library on '{node_name}'. Download it first: ff model download {catalog_id}"))?;

            // 3. Pick a free port (51001..=51020, skipping ones in deployments).
            let used_ports: std::collections::HashSet<i32> = deps.iter().map(|d| d.port).collect();
            let port = (51001u16..=51020).find(|p| !used_ports.contains(&(*p as i32)))
                .ok_or_else(|| anyhow::anyhow!("no free port in 51001-51020"))?;

            // 4. Load.
            let res = ff_agent::model_runtime::load_model(&pool, ff_agent::model_runtime::LoadOptions {
                library_id: lib.id.clone(),
                port,
                context_size: ctx,
                parallel: None,
            }).await.map_err(|e| anyhow::anyhow!(e))?;

            println!("Autoloaded {} on port {} (deployment {})", catalog_id, res.port, res.deployment_id);
        }
        ModelCommand::Unload { id } => {
            match ff_agent::model_runtime::unload_model(&pool, &id).await {
                Ok(()) => println!("Unloaded deployment {id}"),
                Err(e) => anyhow::bail!("unload failed: {e}"),
            }
        }
        ModelCommand::Ps => {
            let procs = ff_agent::model_runtime::list_local_processes().await;
            if procs.is_empty() {
                println!("(no inference servers running)");
                return Ok(());
            }
            println!("{:<8} {:<10} {:<8} {}", "PID", "RUNTIME", "PORT", "MODEL");
            for p in procs {
                println!("{:<8} {:<10} {:<8} {}",
                    p.pid, p.runtime,
                    p.port.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
                    p.model_path.clone().unwrap_or_else(|| "-".into()));
            }
        }
        ModelCommand::Info { id } => {
            // Try as catalog id first.
            if let Some(c) = ff_db::pg_get_catalog(&pool, &id).await? {
                println!("{CYAN}━ Catalog entry ━{RESET}");
                println!("ID:           {}", c.id);
                println!("Name:         {}", c.name);
                println!("Family:       {}", c.family);
                println!("Parameters:   {}", c.parameters);
                println!("Tier:         T{}", c.tier);
                println!("Gated:        {}", if c.gated { "yes (HF license required)" } else { "no" });
                if let Some(d) = &c.description { println!("Description:  {d}"); }
                if let Some(arr) = c.preferred_workloads.as_array() {
                    let wl: Vec<String> = arr.iter().filter_map(|v| v.as_str().map(String::from)).collect();
                    if !wl.is_empty() { println!("Workloads:    {}", wl.join(", ")); }
                }
                if let Some(variants) = c.variants.as_array() {
                    println!("\nVariants:");
                    for v in variants {
                        let runtime = v.get("runtime").and_then(|x| x.as_str()).unwrap_or("?");
                        let quant = v.get("quant").and_then(|x| x.as_str()).unwrap_or("-");
                        let repo = v.get("hf_repo").and_then(|x| x.as_str()).unwrap_or("?");
                        let size = v.get("size_gb").and_then(|x| x.as_f64()).unwrap_or(0.0);
                        println!("  - {runtime:<10} quant={quant:<8} {size:>6.1} GB  {repo}");
                    }
                }
                // Where is it on the fleet?
                let lib = ff_db::pg_list_library(&pool, None).await?;
                let copies: Vec<&ff_db::ModelLibraryRow> = lib.iter().filter(|r| r.catalog_id == c.id).collect();
                if !copies.is_empty() {
                    println!("\nOn disk:");
                    for r in &copies {
                        let q = r.quant.clone().unwrap_or_else(|| "-".into());
                        println!("  - {:<10} ({:<10} {:<6}) {}  [{}]",
                            r.node_name, r.runtime, q, human_bytes(r.size_bytes as u64), &r.id[..8]);
                    }
                }
                let deps = ff_db::pg_list_deployments(&pool, None).await?;
                let live: Vec<&ff_db::ModelDeploymentRow> = deps.iter()
                    .filter(|d| d.catalog_id.as_deref() == Some(&c.id))
                    .collect();
                if !live.is_empty() {
                    println!("\nDeployments:");
                    for d in &live {
                        println!("  - {:<10} port {:<5} {:<10} health={}  [{}]",
                            d.node_name, d.port, d.runtime, d.health_status, &d.id[..8]);
                    }
                }
                return Ok(());
            }
            // Try as library row UUID.
            let all_lib = ff_db::pg_list_library(&pool, None).await?;
            if let Some(r) = all_lib.iter().find(|r| r.id == id) {
                println!("{CYAN}━ Library row ━{RESET}");
                println!("ID:           {}", r.id);
                println!("Node:         {}", r.node_name);
                println!("Catalog ID:   {}", r.catalog_id);
                println!("Runtime:      {}", r.runtime);
                println!("Quant:        {}", r.quant.clone().unwrap_or_else(|| "-".into()));
                println!("File path:    {}", r.file_path);
                println!("Size:         {}", human_bytes(r.size_bytes as u64));
                if let Some(s) = &r.sha256 { println!("SHA256:       {s}"); }
                println!("Downloaded:   {}", r.downloaded_at.format("%Y-%m-%d %H:%M UTC"));
                if let Some(t) = r.last_used_at { println!("Last used:    {}", t.format("%Y-%m-%d %H:%M UTC")); }
                if let Some(s) = &r.source_url { println!("Source:       {s}"); }
                return Ok(());
            }
            // Try as deployment UUID.
            let all_dep = ff_db::pg_list_deployments(&pool, None).await?;
            if let Some(d) = all_dep.iter().find(|d| d.id == id) {
                println!("{CYAN}━ Deployment ━{RESET}");
                println!("ID:           {}", d.id);
                println!("Node:         {}", d.node_name);
                println!("Catalog ID:   {}", d.catalog_id.clone().unwrap_or_else(|| "-".into()));
                println!("Runtime:      {}", d.runtime);
                println!("Port:         {}", d.port);
                println!("PID:          {}", d.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()));
                println!("Health:       {}", d.health_status);
                println!("Started:      {}", d.started_at.format("%Y-%m-%d %H:%M UTC"));
                if let Some(t) = d.last_health_at { println!("Last health:  {}", t.format("%Y-%m-%d %H:%M UTC")); }
                if let Some(c) = d.context_window { println!("Ctx window:   {c}"); }
                println!("Tokens used:  {}", d.tokens_used);
                println!("Requests:     {}", d.request_count);
                return Ok(());
            }
            anyhow::bail!("'{id}' is not a known catalog id, library UUID, or deployment UUID");
        }
        ModelCommand::Prune { node, min_cold_days } => {
            let node_name = match node {
                Some(n) => n,
                None => ff_agent::fleet_info::resolve_this_node_name().await,
            };
            let policy = ff_agent::smart_lru::LruPolicy {
                min_cold_days,
                ..Default::default()
            };
            let plan = ff_agent::smart_lru::plan_eviction(&pool, &node_name, &policy).await
                .map_err(|e| anyhow::anyhow!(e))?;
            if plan.candidates.is_empty() {
                println!("Node '{node_name}' is within quota — no eviction needed.");
                return Ok(());
            }
            println!("Eviction plan for {node_name} (would free {}):\n", human_bytes(plan.total_bytes_freed));
            println!("{:<38} {:<24} {:<10} {:<10} {}", "LIBRARY_ID", "CATALOG", "RUNTIME", "SIZE", "REASONS");
            for c in &plan.candidates {
                println!("{:<38} {:<24} {:<10} {:<10} {}",
                    c.library_id, c.catalog_id, c.runtime,
                    human_bytes(c.size_bytes),
                    c.reasons.join(", "));
            }
            println!("\n(dry-run; use `ff model delete <library-id> --yes` to actually remove)");
        }
        ModelCommand::DiskSample => {
            match ff_agent::disk_sampler::sample_local_disk(&pool).await {
                Ok(s) => {
                    println!("Node:        {}", s.node_name);
                    println!("Models dir:  {}", s.models_dir.display());
                    println!("Total:       {}", human_bytes(s.total_bytes));
                    println!("Used:        {}", human_bytes(s.used_bytes));
                    println!("Free:        {}", human_bytes(s.free_bytes));
                    println!("Models size: {}", human_bytes(s.models_bytes));
                    println!("Quota:       {}%", s.quota_pct);
                    println!("Over quota:  {}", s.over_quota);
                }
                Err(e) => anyhow::bail!("disk sample failed: {e}"),
            }
        }
        ModelCommand::Ping { id } => {
            match ff_agent::model_runtime::health_check_deployment(&pool, &id).await {
                Ok(true) => println!("{CYAN}✓ healthy{RESET}"),
                Ok(false) => println!("{YELLOW}⚠ unhealthy (reachable but failing){RESET}"),
                Err(e) => anyhow::bail!("health check failed: {e}"),
            }
        }
        ModelCommand::Transfer { library_id, from, to } => {
            let opts = ff_agent::model_transfer::TransferOptions {
                source_node: from.clone(),
                target_node: to.clone(),
                library_id: library_id.clone(),
            };
            match ff_agent::model_transfer::transfer_model(&pool, opts).await {
                Ok(res) => {
                    println!(
                        "{CYAN}✓ transferred{RESET} {} bytes  new library id: {}",
                        res.bytes_transferred, res.target_library_id
                    );
                }
                Err(e) => anyhow::bail!("transfer failed: {e}"),
            }
        }
        ModelCommand::Convert { library_id, q_bits } => {
            let opts = ff_agent::model_convert::ConvertOptions {
                library_id: library_id.clone(),
                quant_bits: q_bits,
                output_dir: None,
            };
            println!(
                "{CYAN}▶ Converting library {library_id} to MLX ({q_bits}-bit)...{RESET}"
            );
            match ff_agent::model_convert::convert_safetensors_to_mlx(&pool, opts).await {
                Ok(res) => {
                    println!(
                        "{CYAN}✓ converted{RESET} in {}s → {}  (new library id: {})",
                        res.duration_seconds,
                        res.output_path.display(),
                        res.new_library_id,
                    );
                }
                Err(e) => anyhow::bail!("convert failed: {e}"),
            }
        }
        ModelCommand::Jobs { status, limit } => {
            let rows = ff_db::pg_list_jobs(&pool, status.as_deref(), limit).await?;
            if rows.is_empty() {
                println!("(no jobs)");
                return Ok(());
            }
            println!("{:<38} {:<10} {:<12} {:<10} {:<7} {}", "ID", "NODE", "KIND", "STATUS", "PCT", "TARGET");
            for r in rows {
                let target = r.target_catalog_id.clone()
                    .or(r.target_library_id.clone())
                    .unwrap_or_else(|| "-".into());
                println!("{:<38} {:<10} {:<12} {:<10} {:<6.1}% {}",
                    r.id, r.node_name, r.kind, r.status, r.progress_pct, target);
            }
        }
    }
    Ok(())
}

/// Pretty-print a byte size (KiB/MiB/GiB/TiB).
fn human_bytes(n: u64) -> String {
    let (unit, v) = if n >= 1 << 40 { ("TiB", n as f64 / (1u64 << 40) as f64) }
        else if n >= 1 << 30 { ("GiB", n as f64 / (1u64 << 30) as f64) }
        else if n >= 1 << 20 { ("MiB", n as f64 / (1u64 << 20) as f64) }
        else if n >= 1 << 10 { ("KiB", n as f64 / (1u64 << 10) as f64) }
        else { return format!("{n}B"); };
    format!("{v:.1}{unit}")
}

/// Expand a leading `~` to `$HOME` so config strings like "~/models" resolve to absolute paths.
fn expand_tilde(p: &str, home: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        PathBuf::from(home).join(rest)
    } else if p == "~" {
        PathBuf::from(home)
    } else {
        PathBuf::from(p)
    }
}

/// Truncate a string for inline status display, with a leading ellipsis.
fn trunc_for_status(s: &str, max: usize) -> String {
    if s.chars().count() <= max { return s.to_string(); }
    let take = max.saturating_sub(1);
    let suffix: String = s.chars().rev().take(take).collect::<Vec<_>>().into_iter().rev().collect();
    format!("…{suffix}")
}


// ─── Deferred task worker ──────────────────────────────────────────────────

/// Probe each fleet node's SSH port (22) to determine reachability. Returns the list of reachable node names.
async fn probe_online_nodes(nodes: &[ff_db::FleetNodeRow]) -> Vec<String> {
    use tokio::net::TcpStream;
    use tokio::time::{timeout, Duration as TokDuration};
    // KNOWN LIMITATION: this probes SSH port 22, which means a node with its
    // OS up but its `ff daemon` dead will still appear online. As a result, the
    // Redis `fleet:node_online` publish only fires on OS-level transitions, not
    // daemon-level transitions. Proper fix would be a Redis heartbeat key per
    // daemon (TTL 30s) that workers refresh; the scheduler would read those
    // keys instead of SSH-probing. Out of scope for now — the 15s defer poll
    // catches daemon-only restarts within one cycle.
    let mut handles = Vec::new();
    for n in nodes {
        let name = n.name.clone();
        let ip = n.ip.clone();
        let handle: tokio::task::JoinHandle<Option<String>> = tokio::spawn(async move {
            let addr = format!("{ip}:22");
            match timeout(TokDuration::from_secs(3), TcpStream::connect(&addr)).await {
                Ok(Ok(_)) => Some(name),
                _ => None,
            }
        });
        handles.push(handle);
    }
    let mut online = Vec::new();
    for h in handles {
        if let Ok(Some(name)) = h.await {
            online.push(name);
        }
    }
    online
}

/// Execute a single deferred task. Returns (success, result_json, error).
///
/// `workspace` — optional sub-agent workspace dir. Shell tasks use this
/// as `cwd` when running locally; SSH-dispatched shell tasks ignore it
/// (the remote node sets its own cwd). Future `agent_run` kind will use
/// this for checkpoint/scratch isolation across concurrent sub-agents.
fn detect_os_family() -> String {
    if cfg!(target_os = "macos") { "macos".into() }
    else if cfg!(target_os = "linux") { "linux".into() }
    else { "unknown".into() }
}

/// Parse shorthand duration specs like "1h", "30m", "2d", "45s".
fn parse_duration(spec: &str) -> Option<chrono::Duration> {
    let spec = spec.trim();
    let (num, unit) = spec.split_at(spec.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = num.parse().ok()?;
    match unit {
        "s" | "sec" => Some(chrono::Duration::seconds(n)),
        "m" | "min" => Some(chrono::Duration::minutes(n)),
        "h" | "hr"  => Some(chrono::Duration::hours(n)),
        "d" | "day" => Some(chrono::Duration::days(n)),
        _ => None,
    }
}

async fn execute_deferred(
    task: &ff_db::DeferredTaskRow,
    nodes: &[ff_db::FleetNodeRow],
    workspace: Option<&std::path::Path>,
) -> (bool, Option<serde_json::Value>, Option<String>) {
    match task.kind.as_str() {
        "shell" => {
            let command = match task.payload.get("command").and_then(|v| v.as_str()) {
                Some(c) => c,
                None => return (false, None, Some("shell payload missing 'command' field".into())),
            };
            // preferred_node tells us where to run. If None, run locally.
            let target = task.preferred_node.as_deref();
            execute_shell(target, command, nodes, workspace).await
        }
        "http" => {
            let url = match task.payload.get("url").and_then(|v| v.as_str()) {
                Some(u) => u,
                None => return (false, None, Some("http payload missing 'url' field".into())),
            };
            let method = task.payload.get("method").and_then(|v| v.as_str()).unwrap_or("GET");
            let body = task.payload.get("body").cloned();
            execute_http(method, url, body).await
        }
        "internal" => {
            // Internal ForgeFleet tasks dispatched by title. Requires DB pool —
            // we open a short-lived one here so execute_deferred stays pure.
            if task.title.starts_with("Mesh propagate SSH for ") {
                match ff_agent::fleet_info::get_fleet_pool().await {
                    Ok(pool) => match ff_agent::mesh_check::mesh_propagate(&pool, &task.payload).await {
                        Ok((ok, fail)) => {
                            let result = serde_json::json!({"ok_peers": ok, "failed_peers": fail});
                            let success = fail == 0;
                            let err = if success { None } else { Some(format!("{fail} peer(s) failed")) };
                            (success, Some(result), err)
                        }
                        Err(e) => (false, None, Some(format!("mesh_propagate: {e}"))),
                    },
                    Err(e) => (false, None, Some(format!("pool: {e}"))),
                }
            } else {
                (false, None, Some(format!("unknown internal task title: {}", task.title)))
            }
        }
        "upgrade" => {
            // Run the tool-specific upgrade playbook.
            let tool = match task.payload.get("tool").and_then(|v| v.as_str()) {
                Some(t) => t,
                None => return (false, None, Some("upgrade payload missing 'tool'".into())),
            };
            let os_family = detect_os_family();
            let script = match ff_agent::upgrade_playbooks::playbook_for(tool, &os_family) {
                Some(s) => s,
                None => return (false, None, Some(format!("no playbook for tool={tool} os={os_family}"))),
            };
            let target = task.preferred_node.as_deref();
            execute_shell(target, &script, nodes, workspace).await
        }
        "mesh_retry" => {
            // Re-probe a specific (src, dst) pair and refresh fleet_mesh_status.
            let src = task.payload.get("src").and_then(|v| v.as_str()).unwrap_or("");
            let dst = task.payload.get("dst").and_then(|v| v.as_str()).unwrap_or("");
            if src.is_empty() || dst.is_empty() {
                return (false, None, Some("mesh_retry payload needs src+dst".into()));
            }
            match ff_agent::fleet_info::get_fleet_pool().await {
                Ok(pool) => match ff_agent::mesh_check::probe_single_pair(&pool, src, dst).await {
                    Ok(cell) => {
                        let ok = cell.status == "ok";
                        let result = serde_json::json!({"status": cell.status, "error": cell.last_error});
                        (ok, Some(result), if ok { None } else { cell.last_error })
                    }
                    Err(e) => (false, None, Some(format!("probe: {e}"))),
                },
                Err(e) => (false, None, Some(format!("pool: {e}"))),
            }
        }
        other => (false, None, Some(format!("unknown task kind: {other}"))),
    }
}

/// Run a shell command either locally (when target is this host or None) or via SSH.
async fn execute_shell(
    target_node: Option<&str>,
    command: &str,
    nodes: &[ff_db::FleetNodeRow],
    workspace: Option<&std::path::Path>,
) -> (bool, Option<serde_json::Value>, Option<String>) {
    use tokio::process::Command as TokCmd;
    let this_hostname = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_default();

    let mut local = true;
    let (program, args): (&str, Vec<String>) = match target_node {
        None => ("sh", vec!["-c".into(), command.to_string()]),
        Some(n) if this_hostname.starts_with(&n.to_lowercase()) => {
            ("sh", vec!["-c".into(), command.to_string()])
        }
        Some(n) => {
            local = false;
            // SSH to target: look up user@ip from DB.
            let node = match nodes.iter().find(|x| x.name.eq_ignore_ascii_case(n)) {
                Some(n) => n,
                None => return (false, None, Some(format!("node '{n}' not in fleet_nodes"))),
            };
            let dest = format!("{}@{}", node.ssh_user, node.ip);
            (
                "ssh",
                vec![
                    "-o".into(), "ConnectTimeout=8".into(),
                    "-o".into(), "StrictHostKeyChecking=accept-new".into(),
                    "-o".into(), "BatchMode=yes".into(),
                    dest,
                    command.to_string(),
                ],
            )
        }
    };

    let mut cmd = TokCmd::new(program);
    cmd.args(&args);
    if local {
        if let Some(ws) = workspace {
            cmd.current_dir(ws);
        }
    }
    let output = cmd.output().await;
    match output {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout).to_string();
            let stderr = String::from_utf8_lossy(&o.stderr).to_string();
            let result = serde_json::json!({
                "exit_code": o.status.code(),
                "stdout": stdout,
                "stderr": stderr,
            });
            if o.status.success() {
                (true, Some(result), None)
            } else {
                let err = format!("exit {}: {}", o.status.code().unwrap_or(-1), stderr.trim().lines().last().unwrap_or(""));
                (false, Some(result), Some(err))
            }
        }
        Err(e) => (false, None, Some(format!("spawn {program} failed: {e}"))),
    }
}

/// Execute an HTTP request task.
async fn execute_http(
    method: &str,
    url: &str,
    body: Option<serde_json::Value>,
) -> (bool, Option<serde_json::Value>, Option<String>) {
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(30)).build() {
        Ok(c) => c,
        Err(e) => return (false, None, Some(format!("http client: {e}"))),
    };
    let method_obj = match method.to_uppercase().as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "DELETE" => reqwest::Method::DELETE,
        "PATCH" => reqwest::Method::PATCH,
        other => return (false, None, Some(format!("bad http method: {other}"))),
    };
    let mut req = client.request(method_obj, url);
    if let Some(b) = body {
        req = req.json(&b);
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let result = serde_json::json!({"status": status.as_u16(), "body": text});
            if status.is_success() {
                (true, Some(result), None)
            } else {
                (false, Some(result), Some(format!("HTTP {status}")))
            }
        }
        Err(e) => (false, None, Some(format!("http send: {e}"))),
    }
}

// ─── Versions / Fleet / Onboard CLI handlers (Phase 3+5) ──────────────────

async fn handle_versions(node_filter: Option<String>) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool).await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    let nodes = ff_db::pg_list_nodes(&pool).await?;
    let filtered: Vec<&ff_db::FleetNodeRow> = nodes.iter()
        .filter(|n| node_filter.as_deref().map(|f| n.name == f).unwrap_or(true))
        .collect();

    // Collect every tool key seen across all nodes.
    let mut all_keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for n in &filtered {
        if let Some(obj) = n.tooling.as_object() {
            for k in obj.keys() {
                all_keys.insert(k.clone());
            }
        }
    }
    if all_keys.is_empty() {
        println!("(no tool-version data yet — run `ff daemon` for 6h or manually trigger version_check)");
        return Ok(());
    }

    // Header
    print!("{:<14}", "TOOL");
    for n in &filtered {
        print!(" {:<14}", truncate_for_col(&n.name, 14));
    }
    println!();
    for k in &all_keys {
        print!("{:<14}", truncate_for_col(k, 14));
        for n in &filtered {
            let cell = n.tooling.get(k);
            let (cur, lat) = match cell {
                Some(obj) => (
                    obj.get("current").and_then(|v| v.as_str()).unwrap_or("-"),
                    obj.get("latest").and_then(|v| v.as_str()),
                ),
                None => ("—", None),
            };
            let marker = match lat {
                Some(l) if l == cur => "✓",
                Some(_) => "⚠",
                None => " ",
            };
            let disp = format!("{} {}", truncate_for_col(cur, 11), marker);
            print!(" {:<14}", disp);
        }
        println!();
    }
    Ok(())
}

fn truncate_for_col(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() }
    else { s.chars().take(n.saturating_sub(1)).collect::<String>() + "…" }
}

async fn handle_fleet(cmd: FleetCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool).await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        FleetCommand::SshMeshCheck { node, json, since, repair, yes } => {
            if repair && !yes {
                anyhow::bail!("--repair rewrites authorized_keys / known_hosts on every failed peer — pass --yes to proceed");
            }
            if repair {
                println!("{CYAN}▶ Repairing mesh before probing...{RESET}");
                let failed = ff_db::pg_list_mesh_status(&pool, None).await
                    .map_err(|e| anyhow::anyhow!("pg_list_mesh_status: {e}"))?
                    .into_iter()
                    .filter(|r| r.status == "failed")
                    .collect::<Vec<_>>();
                println!("  found {} failed pair(s) — re-enqueuing as mesh_retry tasks", failed.len());
                let created = ff_agent::mesh_check::enqueue_retries(&pool).await
                    .map_err(|e| anyhow::anyhow!(e))?;
                println!("  enqueued {} mesh_retry task(s)", created);
            }
            if let Some(spec) = &since {
                let age = parse_duration(spec)
                    .ok_or_else(|| anyhow::anyhow!("unrecognized --since value '{spec}' (try 1h, 30m, 2d)"))?;
                println!("{CYAN}▶ Refreshing pairs older than {spec}...{RESET}");
                let n = ff_agent::mesh_check::refresh_stale(&pool, age).await
                    .map_err(|e| anyhow::anyhow!(e))?;
                println!("  refreshed {n} stale pair(s)");
                return Ok(());
            }
            println!("{CYAN}▶ Running pairwise SSH mesh check...{RESET}");
            let matrix = match &node {
                Some(n) => ff_agent::mesh_check::pairwise_ssh_check_node(&pool, n).await,
                None => ff_agent::mesh_check::pairwise_ssh_check(&pool).await,
            }.map_err(|e| anyhow::anyhow!(e))?;
            if json {
                let arr: Vec<_> = matrix.cells.iter().map(|c| serde_json::json!({
                    "src": c.src, "dst": c.dst, "status": c.status, "last_error": c.last_error,
                })).collect();
                println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
            } else {
                let mut ok = 0; let mut fail = 0;
                for c in &matrix.cells {
                    let marker = if c.status == "ok" { "✓" } else { "✗" };
                    if c.status == "ok" { ok += 1; } else { fail += 1; }
                    let err = c.last_error.as_deref().unwrap_or("");
                    println!("  {:<10} → {:<10}  {}  {}", c.src, c.dst, marker, err);
                }
                println!("\n{ok} ok, {fail} failed — checked {} pairs", matrix.cells.len());
            }
        }
        FleetCommand::VerifyNode { name, json } => {
            println!("{CYAN}▶ Running verify-node battery for {name}...{RESET}");
            let report = ff_agent::verify_node::verify_node(&pool, &name).await
                .map_err(|e| anyhow::anyhow!(e))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
            } else {
                println!("\nResults for {}: {} pass, {} fail, {} skip", report.node, report.passed, report.failed, report.skipped);
                for r in &report.details {
                    let marker = match r.status.as_str() {
                        "pass" => "✓", "fail" => "✗", _ => "—",
                    };
                    let msg = r.message.as_deref().unwrap_or("");
                    println!("  {}  {:<28}  {}", marker, r.check, msg);
                }
            }
        }
        FleetCommand::MigrateGithub { new_owner, skip_local, only, dry_run, yes } => {
            let nodes = ff_db::pg_list_nodes(&pool).await?;
            let local = ff_agent::fleet_info::resolve_this_node_name().await;
            let mut targets: Vec<&ff_db::FleetNodeRow> = nodes.iter().collect();
            if let Some(name) = &only {
                targets.retain(|n| &n.name == name);
                if targets.is_empty() {
                    anyhow::bail!("no fleet node named '{name}'");
                }
            } else if skip_local {
                targets.retain(|n| n.name != local);
            }
            println!("{CYAN}▶ ff fleet migrate-github{RESET}");
            println!("  new owner:       {new_owner}");
            println!("  local node:      {local}{}", if skip_local { " (skipped)" } else { "" });
            println!("  targets:         {} node(s)", targets.len());
            for n in &targets {
                println!("    {:<15} {:<16} {}", n.name, n.ip, n.gh_account.clone().unwrap_or_else(|| "-".into()));
            }
            if targets.is_empty() {
                println!("{YELLOW}No nodes to enqueue. Nothing to do.{RESET}");
                return Ok(());
            }
            if dry_run || !yes {
                println!("\n{YELLOW}Dry run — not enqueuing. Pass --yes to actually enqueue.{RESET}");
                return Ok(());
            }

            let who = whoami_tag();
            let mut enqueued: Vec<(String, String)> = Vec::with_capacity(targets.len());
            for n in &targets {
                let script = build_migrate_github_script(&new_owner);
                let title = format!("Migrate GitHub owner → {new_owner} on {}", n.name);
                let payload = serde_json::json!({ "command": script });
                let trigger_spec = serde_json::json!({ "node": n.name });
                let defer_id = ff_db::pg_enqueue_deferred(
                    &pool,
                    &title,
                    "shell",
                    &payload,
                    "node_online",
                    &trigger_spec,
                    Some(&n.name),
                    &serde_json::json!([]),
                    Some(&who),
                    Some(3),
                )
                .await?;
                enqueued.push((n.name.clone(), defer_id));
            }
            println!("\n{GREEN}✓ Enqueued {} migration task(s):{RESET}", enqueued.len());
            for (node, id) in &enqueued {
                println!("  {:<15} {id}", node);
            }
            println!("\nTrack progress with: ff defer list");
        }
    }
    Ok(())
}

fn build_migrate_github_script(new_owner: &str) -> String {
    format!(
        r#"set -e
if [ -d "/Users/$USER" ]; then
  HOME_BASE="/Users/$USER"
  OS_TYPE="mac"
else
  HOME_BASE="/home/$USER"
  OS_TYPE="linux"
fi
OLD_DIR="$HOME_BASE/taylorProjects/forge-fleet"
NEW_DIR="$HOME_BASE/projects/forge-fleet"
mkdir -p "$HOME_BASE/projects"
if [ ! -d "$NEW_DIR/.git" ]; then
  if [ -d "$OLD_DIR/.git" ]; then
    mv "$OLD_DIR" "$NEW_DIR"
  else
    git clone --depth 50 "https://github.com/{new_owner}/forge-fleet.git" "$NEW_DIR"
  fi
fi
if [ ! -e "$OLD_DIR" ]; then
  mkdir -p "$HOME_BASE/taylorProjects"
  ln -sfn "$NEW_DIR" "$OLD_DIR"
fi
cd "$NEW_DIR"
git remote set-url origin "https://github.com/{new_owner}/forge-fleet.git"
git fetch origin main
git reset --hard origin/main
cargo build --release -p ff-terminal
install -m 755 target/release/ff "$HOME_BASE/.local/bin/ff"
if [ "$OS_TYPE" = "mac" ]; then
  codesign --force --sign - "$HOME_BASE/.local/bin/ff" || true
fi
if [ "$OS_TYPE" = "linux" ]; then
  UNIT="/etc/systemd/system/forgefleet-daemon.service"
  if [ -f "$UNIT" ]; then
    sudo sed -i "s|WorkingDirectory=.*taylorProjects.*forge-fleet|WorkingDirectory=$NEW_DIR|" "$UNIT" || true
    sudo systemctl daemon-reload || true
    sudo systemctl restart forgefleet-daemon.service || true
  fi
fi
echo "migrate-github complete on $(hostname): remote=https://github.com/{new_owner}/forge-fleet.git path=$NEW_DIR"
"#
    )
}

async fn handle_brain(cmd: BrainCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool).await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        BrainCommand::Index { vault_path, subfolder } => {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/venkat".into());
            let vault = vault_path.unwrap_or_else(|| format!("{home}/projects/Yarli_KnowledgeBase"));
            let sub = subfolder.unwrap_or_default();
            let config = ff_brain::VaultConfig {
                vault_path: std::path::PathBuf::from(&vault),
                brain_subfolder: sub.clone(),
            };
            let root = if sub.is_empty() { vault.clone() } else { format!("{vault}/{sub}") };
            println!("{CYAN}▶ Indexing vault: {root}{RESET}");
            let report = ff_brain::index_vault(&pool, &config).await
                .map_err(|e| anyhow::anyhow!(e))?;
            println!("  files scanned:    {}", report.files_scanned);
            println!("  nodes upserted:   {}", report.nodes_upserted);
            println!("  edges created:    {}", report.edges_created);
            println!("  chunks written:   {}", report.chunks_written);
            println!("  unchanged skipped: {}", report.unchanged_skipped);
            println!("{CYAN}✓ Done{RESET}");
        }
        BrainCommand::Communities => {
            println!("{CYAN}▶ Running community detection...{RESET}");
            let summary = ff_brain::detect_communities(&pool).await
                .map_err(|e| anyhow::anyhow!(e))?;
            println!("  communities: {}", summary.communities_found);
            println!("  largest:     {} nodes", summary.largest_community);
        }
        BrainCommand::Stats => {
            let nodes = ff_db::pg_list_brain_vault_nodes_current(&pool, None).await
                .map_err(|e| anyhow::anyhow!("list nodes: {e}"))?;
            let total_edges: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM brain_vault_edges")
                .fetch_one(&pool)
                .await
                .unwrap_or(0);
            let communities: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM brain_communities")
                .fetch_one(&pool)
                .await
                .unwrap_or(0);
            println!("Vault graph stats:");
            println!("  nodes (current): {}", nodes.len());
            println!("  edges:           {total_edges}");
            println!("  communities:     {communities}");
        }
    }
    Ok(())
}

async fn handle_onboard(cmd: OnboardCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool).await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        OnboardCommand::Show { name, ip, ssh_user, role, runtime } => {
            // Try to get token from fleet_secrets, fallback to env var.
            let token = ff_agent::fleet_info::fetch_secret("enrollment.shared_secret")
                .await
                .or_else(|| std::env::var("FORGEFLEET_ENROLLMENT_TOKEN").ok())
                .unwrap_or_else(|| "<SET-TOKEN-FIRST>".into());
            let leader = std::env::var("FORGEFLEET_LEADER_HOST")
                .unwrap_or_else(|_| "192.168.5.100".into());
            let ssh_user = ssh_user.unwrap_or_else(|| name.clone());
            let ip_q = ip.unwrap_or_else(|| "auto".into());
            println!("{CYAN}▶ On the new computer, paste:{RESET}\n");
            println!("curl -fsSL 'http://{leader}:51002/onboard/bootstrap.sh\\");
            println!("    ?token={token}&name={name}&ip={ip_q}\\");
            println!("    &ssh_user={ssh_user}&role={role}&runtime={runtime}' \\");
            println!("  | sudo bash");
            println!("\n  (Or open http://{leader}:51002/onboard in the browser.)");
        }
        OnboardCommand::List { limit } => {
            // Recent enrollments via deferred_tasks + fleet_nodes updated_at.
            let nodes = ff_db::pg_list_nodes(&pool).await?;
            let mut sorted: Vec<&ff_db::FleetNodeRow> = nodes.iter().collect();
            sorted.sort_by(|a, b| b.election_priority.cmp(&a.election_priority));
            println!("{:<15} {:<16} {:<10} {:<6} {}", "NAME", "IP", "RUNTIME", "PRIO", "GH");
            for n in sorted.into_iter().take(limit as usize) {
                println!("{:<15} {:<16} {:<10} {:<6} {}",
                    n.name, n.ip, n.runtime, n.election_priority,
                    n.gh_account.clone().unwrap_or_else(|| "-".into()));
            }
        }
        OnboardCommand::Revoke { name, yes } => {
            if !yes {
                println!("This will DELETE fleet_nodes row '{name}', all its SSH keys, and mesh-status rows.");
                println!("Re-run with --yes to confirm.");
                return Ok(());
            }
            let removed_keys = ff_db::pg_delete_node_ssh_keys(&pool, &name).await?;
            let removed_mesh = ff_db::pg_delete_mesh_status_for_node(&pool, &name).await?;
            // Delete fleet_nodes row (via raw SQL — no helper exists).
            let r = sqlx::query("DELETE FROM fleet_nodes WHERE name = $1")
                .bind(&name)
                .execute(&pool)
                .await?;
            println!("Revoked '{name}': {} ssh keys, {} mesh rows, {} node row(s)",
                removed_keys, removed_mesh, r.rows_affected());
        }
    }
    Ok(())
}

async fn handle_defer_worker(
    as_node: Option<String>,
    interval: u64,
    scheduler: bool,
    once: bool,
) -> Result<()> {
    let worker_name = match as_node {
        Some(n) => n,
        None => ff_agent::fleet_info::resolve_this_node_name().await,
    };

    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool).await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    // Sub-agent concurrency slots — read fleet_nodes.sub_agent_count for this node.
    let slot_count = ff_db::pg_get_node(&pool, &worker_name).await.ok()
        .flatten()
        .map(|n| n.sub_agent_count.max(1) as u32)
        .unwrap_or(1);
    let _ = ff_agent::sub_agents::ensure_workspaces(slot_count);
    let slots = ff_agent::sub_agents::Slots::new(slot_count);

    println!("{CYAN}▶ defer-worker starting{RESET}");
    println!("  node:      {worker_name}");
    println!("  scheduler: {scheduler}");
    println!("  interval:  {interval}s");
    println!("  mode:      {}", if once { "single-pass" } else { "continuous" });

    // Subscribe to fleet:node_online so this worker wakes instantly when
    // the scheduler reports that this node is back online.
    let (wake_tx, mut wake_rx) = tokio::sync::mpsc::channel::<()>(8);
    if !once {
        let my_node = worker_name.clone();
        tokio::spawn(async move {
            use futures::StreamExt;
            let mut stream = ff_agent::fleet_events::subscribe_node_online();
            while let Some(node) = stream.next().await {
                if node.eq_ignore_ascii_case(&my_node) {
                    let _ = wake_tx.try_send(());
                }
            }
        });
    }

    loop {
        let pass_start = std::time::Instant::now();
        let ran_any = defer_pass(&pool, &worker_name, scheduler, &slots).await? > 0;

        if once {
            println!("{CYAN}▶ defer-worker: --once set, exiting{RESET}");
            return Ok(());
        }

        let elapsed = pass_start.elapsed();
        let sleep_for = Duration::from_secs(interval).saturating_sub(elapsed);
        if !ran_any && sleep_for.as_millis() > 0 {
            tokio::select! {
                _ = tokio::time::sleep(sleep_for) => {}
                Some(_) = wake_rx.recv() => {
                    println!("{CYAN}[worker]{RESET} woken by fleet:node_online");
                }
            }
        } else if sleep_for.as_millis() > 0 {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

/// One scheduler+worker pass. Returns number of tasks executed.
///
/// `slots` — sub-agent concurrency pool. On hosts with capacity > 1
/// the pass claims and spawns up to `capacity` tasks in parallel.
async fn defer_pass(
    pool: &sqlx::PgPool,
    worker_name: &str,
    scheduler: bool,
    slots: &ff_agent::sub_agents::Slots,
) -> Result<usize> {
    // Scheduler pass: promote pending tasks whose trigger fired.
    if scheduler {
        match ff_db::pg_list_nodes(pool).await {
            Ok(nodes) => {
                let online = probe_online_nodes(&nodes).await;

                // Detect online/offline transitions and publish to Redis so
                // workers on newly-online nodes can wake up immediately
                // instead of waiting for the next poll tick.
                static LAST_ONLINE: std::sync::OnceLock<
                    std::sync::Mutex<std::collections::HashSet<String>>,
                > = std::sync::OnceLock::new();
                let last_online = LAST_ONLINE
                    .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
                let current: std::collections::HashSet<String> =
                    online.iter().cloned().collect();
                let (newly_online, newly_offline) = {
                    let mut prev = last_online.lock().unwrap();
                    let newly_online: Vec<String> =
                        current.difference(&*prev).cloned().collect();
                    let newly_offline: Vec<String> =
                        prev.difference(&current).cloned().collect();
                    *prev = current.clone();
                    (newly_online, newly_offline)
                };
                for n in &newly_online {
                    if let Err(e) = ff_agent::fleet_events::publish_node_online(n).await {
                        eprintln!("{YELLOW}[sched] publish_node_online({n}): {e}{RESET}");
                    } else {
                        println!("{CYAN}[sched]{RESET} node online → {n} (published)");
                    }
                }
                for n in &newly_offline {
                    if let Err(e) = ff_agent::fleet_events::publish_node_offline(n).await {
                        eprintln!("{YELLOW}[sched] publish_node_offline({n}): {e}{RESET}");
                    } else {
                        println!("{CYAN}[sched]{RESET} node offline → {n} (published)");
                    }
                }

                let now = chrono::Utc::now();
                match ff_db::pg_scheduler_pass(pool, &online, now).await {
                    Ok(n) if n > 0 => {
                        println!("{CYAN}[sched]{RESET} promoted {n} task(s) to dispatchable (online: {})", online.join(","));
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("{RED}[sched] pg_scheduler_pass: {e}{RESET}"),
                }
            }
            Err(e) => eprintln!("{RED}[sched] list nodes: {e}{RESET}"),
        }
    }

    // Worker pass: reserve a sub-agent slot, claim one task per slot,
    // spawn each in its own tokio task. We keep looping until either
    // the queue is empty or all slots are busy.
    let mut count = 0usize;
    let mut spawned = Vec::new();
    loop {
        let guard = match slots.try_reserve_owned() {
            Some(g) => g,
            None => break, // all slots busy — try next tick
        };

        let claimed = match ff_db::pg_claim_deferred(pool, worker_name).await {
            Ok(Some(t)) => t,
            Ok(None) => break, // queue empty
            Err(e) => {
                eprintln!("{RED}[worker] claim error: {e}{RESET}");
                break;
            }
        };
        count += 1;
        println!(
            "{YELLOW}[worker]{RESET} slot#{} claimed {} — {}",
            guard.index(),
            claimed.id,
            claimed.title,
        );

        let pool2 = pool.clone();
        let nodes = ff_db::pg_list_nodes(pool).await.unwrap_or_default();
        let h = tokio::spawn(async move {
            let workspace = guard.workspace().to_path_buf();
            let (ok, result, err) =
                execute_deferred(&claimed, &nodes, Some(&workspace)).await;
            match ff_db::pg_finish_deferred(
                &pool2,
                &claimed.id,
                ok,
                result.as_ref(),
                err.as_deref(),
            )
            .await
            {
                Ok(()) => {
                    if ok {
                        println!(
                            "  {CYAN}✓ completed{RESET} (slot#{} id={})",
                            guard.index(),
                            claimed.id,
                        );
                    } else {
                        println!(
                            "  {RED}✗ failed{RESET} (slot#{} id={}): {}",
                            guard.index(),
                            claimed.id,
                            err.clone().unwrap_or_default(),
                        );
                    }
                }
                Err(e) => eprintln!("{RED}  finalize error: {e}{RESET}"),
            }
            // guard drops here, releasing the slot.
            drop(guard);
        });
        spawned.push(h);
    }

    // If this pass only has one slot (legacy single-claim behaviour),
    // await the task so callers see the same semantics as before.
    if slots.capacity() == 1 {
        for h in spawned {
            let _ = h.await;
        }
    }
    Ok(count)
}

async fn handle_daemon(
    as_node: Option<String>,
    scheduler: bool,
    defer_interval: u64,
    disk_interval: u64,
    reconcile_interval: u64,
    once: bool,
) -> Result<()> {
    let worker_name = match as_node {
        Some(n) => n,
        None => ff_agent::fleet_info::resolve_this_node_name().await,
    };

    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool).await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    // Sub-agent concurrency slots — read fleet_nodes.sub_agent_count for this node.
    let slot_count = ff_db::pg_get_node(&pool, &worker_name).await.ok()
        .flatten()
        .map(|n| n.sub_agent_count.max(1) as u32)
        .unwrap_or(1);
    let _ = ff_agent::sub_agents::ensure_workspaces(slot_count);
    let slots = ff_agent::sub_agents::Slots::new(slot_count);

    println!("{CYAN}▶ ForgeFleet daemon starting{RESET}");
    println!("  node:       {worker_name}");
    println!("  scheduler:  {scheduler}");
    println!("  sub-agents: {slot_count}");
    println!("  defer:      every {defer_interval}s");
    println!("  disk:       every {disk_interval}s");
    println!("  reconcile:  every {reconcile_interval}s");

    if once {
        // Run one pass of each sequentially, then exit.
        match defer_pass(&pool, &worker_name, scheduler, &slots).await {
            Ok(n) => println!("{CYAN}[defer]{RESET} one-pass complete ({n} task(s))"),
            Err(e) => eprintln!("{RED}[defer] pass error: {e}{RESET}"),
        }
        match ff_agent::disk_sampler::sample_local_disk(&pool).await {
            Ok(s) => println!(
                "{CYAN}[disk]{RESET} {} total={}MB used={}MB free={}MB models={}MB quota={}%{}",
                s.node_name,
                s.total_bytes / 1_048_576,
                s.used_bytes / 1_048_576,
                s.free_bytes / 1_048_576,
                s.models_bytes / 1_048_576,
                s.quota_pct,
                if s.over_quota { " OVER" } else { "" },
            ),
            Err(e) => eprintln!("{RED}[disk] sample error: {e}{RESET}"),
        }
        match ff_agent::deployment_reconciler::reconcile_local(&pool).await {
            Ok(r) => println!(
                "{CYAN}[reconcile]{RESET} adopted={} removed={} refreshed={}",
                r.adopted, r.removed, r.refreshed,
            ),
            Err(e) => eprintln!("{RED}[reconcile] error: {e}{RESET}"),
        }
        // Sweeper — only the scheduler needs to do this fleet-wide.
        if scheduler {
            match ff_agent::job_sweeper::sweep_stale(
                &pool,
                &ff_agent::job_sweeper::SweepPolicy::default(),
            ).await {
                Ok(s) if s.jobs_failed + s.deferred_failed > 0 => println!(
                    "{CYAN}[sweeper]{RESET} jobs_failed={} deferred_failed={}",
                    s.jobs_failed, s.deferred_failed,
                ),
                Ok(_) => println!("{CYAN}[sweeper]{RESET} no stale work"),
                Err(e) => eprintln!("{RED}[sweeper] error: {e}{RESET}"),
            }
        }
        println!("{CYAN}▶ daemon: --once set, exiting{RESET}");
        return Ok(());
    }

    let mut defer_tick = tokio::time::interval(Duration::from_secs(defer_interval));
    let mut disk_tick = tokio::time::interval(Duration::from_secs(disk_interval));
    let mut recon_tick = tokio::time::interval(Duration::from_secs(reconcile_interval));
    // Sweeper: every 5 minutes, only on the scheduler node.
    let mut sweep_tick = tokio::time::interval(Duration::from_secs(300));
    // Version check: every 6 hours (fleet-wide drift detection).
    let mut version_tick = tokio::time::interval(Duration::from_secs(6 * 3600));
    // Brain vault re-index: every 30 minutes (pick up Obsidian edits).
    let mut brain_tick = tokio::time::interval(Duration::from_secs(30 * 60));
    // First tick fires immediately for each — prime all six.
    defer_tick.tick().await;
    disk_tick.tick().await;
    recon_tick.tick().await;
    sweep_tick.tick().await;
    version_tick.tick().await;
    brain_tick.tick().await;

    // Do an initial pass immediately on startup.
    let _ = defer_pass(&pool, &worker_name, scheduler, &slots).await;
    // Initial version check on daemon startup so operators see data within
    // seconds instead of waiting 6 hours for the first tick.
    match ff_agent::version_check::version_check_pass(&pool).await {
        Ok(s) if !s.drifted_keys.is_empty() => println!(
            "{CYAN}[versions]{RESET} drift: {}", s.drifted_keys.join(", ")),
        Ok(s) => println!("{CYAN}[versions]{RESET} initial pass: {} tools ✓", s.total_keys),
        Err(e) => eprintln!("{RED}[versions] startup: {e}{RESET}"),
    }
    match ff_agent::disk_sampler::sample_local_disk(&pool).await {
        Ok(s) => println!(
            "{CYAN}[disk]{RESET} {} total={}MB used={}MB free={}MB models={}MB quota={}%{}",
            s.node_name,
            s.total_bytes / 1_048_576,
            s.used_bytes / 1_048_576,
            s.free_bytes / 1_048_576,
            s.models_bytes / 1_048_576,
            s.quota_pct,
            if s.over_quota { " OVER" } else { "" },
        ),
        Err(e) => eprintln!("{RED}[disk] sample error: {e}{RESET}"),
    }
    match ff_agent::deployment_reconciler::reconcile_local(&pool).await {
        Ok(r) if r.adopted + r.removed + r.refreshed > 0 => println!(
            "{CYAN}[reconcile]{RESET} adopted={} removed={} refreshed={}",
            r.adopted, r.removed, r.refreshed,
        ),
        Ok(_) => {}
        Err(e) => eprintln!("{RED}[reconcile] error: {e}{RESET}"),
    }

    // Subscribe to fleet:node_online so the daemon runs an immediate
    // defer_pass when this node comes back online (instant wake-up
    // instead of waiting for the next defer_tick).
    let (wake_tx, mut wake_rx) = tokio::sync::mpsc::channel::<()>(8);
    {
        let my_node = worker_name.clone();
        tokio::spawn(async move {
            use futures::StreamExt;
            let mut stream = ff_agent::fleet_events::subscribe_node_online();
            while let Some(node) = stream.next().await {
                if node.eq_ignore_ascii_case(&my_node) {
                    let _ = wake_tx.try_send(());
                }
            }
        });
    }

    loop {
        tokio::select! {
            _ = defer_tick.tick() => {
                if let Err(e) = defer_pass(&pool, &worker_name, scheduler, &slots).await {
                    eprintln!("{RED}[defer] pass error: {e}{RESET}");
                }
            }
            Some(_) = wake_rx.recv() => {
                println!("{CYAN}[defer]{RESET} woken by fleet:node_online");
                if let Err(e) = defer_pass(&pool, &worker_name, scheduler, &slots).await {
                    eprintln!("{RED}[defer] pass error: {e}{RESET}");
                }
            }
            _ = disk_tick.tick() => {
                match ff_agent::disk_sampler::sample_local_disk(&pool).await {
                    Ok(s) => println!(
                        "{CYAN}[disk]{RESET} {} total={}MB used={}MB free={}MB models={}MB quota={}%{}",
                        s.node_name,
                        s.total_bytes / 1_048_576,
                        s.used_bytes / 1_048_576,
                        s.free_bytes / 1_048_576,
                        s.models_bytes / 1_048_576,
                        s.quota_pct,
                        if s.over_quota { " OVER" } else { "" },
                    ),
                    Err(e) => eprintln!("{RED}[disk] sample error: {e}{RESET}"),
                }
            }
            _ = recon_tick.tick() => {
                match ff_agent::deployment_reconciler::reconcile_local(&pool).await {
                    Ok(r) if r.adopted + r.removed + r.refreshed > 0 => println!(
                        "{CYAN}[reconcile]{RESET} adopted={} removed={} refreshed={}",
                        r.adopted, r.removed, r.refreshed,
                    ),
                    Ok(_) => {}
                    Err(e) => eprintln!("{RED}[reconcile] error: {e}{RESET}"),
                }
            }
            _ = sweep_tick.tick(), if scheduler => {
                match ff_agent::job_sweeper::sweep_stale(
                    &pool,
                    &ff_agent::job_sweeper::SweepPolicy::default(),
                ).await {
                    Ok(s) if s.jobs_failed + s.deferred_failed > 0 => println!(
                        "{CYAN}[sweeper]{RESET} jobs_failed={} deferred_failed={}",
                        s.jobs_failed, s.deferred_failed,
                    ),
                    Ok(_) => {}
                    Err(e) => eprintln!("{RED}[sweeper] error: {e}{RESET}"),
                }
            }
            _ = version_tick.tick() => {
                match ff_agent::version_check::version_check_pass(&pool).await {
                    Ok(s) if !s.drifted_keys.is_empty() => println!(
                        "{CYAN}[versions]{RESET} drift detected: {}",
                        s.drifted_keys.join(", ")),
                    Ok(_) => {}
                    Err(e) => eprintln!("{RED}[versions] {e}{RESET}"),
                }
                // Leader-only: refresh the mesh matrix at the same cadence so
                // stale rows don't accumulate and operators see fresh status.
                if worker_name == "taylor" {
                    match ff_agent::mesh_check::pairwise_ssh_check(&pool).await {
                        Ok(m) => {
                            let (ok, fail) = m.cells.iter()
                                .fold((0usize, 0usize), |(o, f), c| {
                                    if c.status == "ok" { (o + 1, f) } else { (o, f + 1) }
                                });
                            println!("{CYAN}[mesh]{RESET} refreshed: {ok} ok, {fail} fail");
                            // Auto-retry any failed pair whose last check was
                            // more than 10 minutes ago — capped at 5 retries
                            // per 24h by pg_enqueue_deferred's max_attempts.
                            let _ = ff_agent::mesh_check::enqueue_retries(&pool).await;
                        }
                        Err(e) => eprintln!("{RED}[mesh] refresh error: {e}{RESET}"),
                    }
                }
            }
            _ = brain_tick.tick() => {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/venkat".into());
                let vault_path = std::path::PathBuf::from(format!("{home}/projects/Yarli_KnowledgeBase"));
                if vault_path.exists() {
                    let config = ff_brain::VaultConfig {
                        vault_path,
                        brain_subfolder: String::new(),
                    };
                    match ff_brain::index_vault(&pool, &config).await {
                        Ok(r) if r.nodes_upserted > 0 => println!(
                            "{CYAN}[brain]{RESET} vault re-indexed: {} new/changed, {} skipped",
                            r.nodes_upserted, r.unchanged_skipped),
                        Ok(_) => {}
                        Err(e) => eprintln!("{RED}[brain] vault index error: {e}{RESET}"),
                    }
                }
            }
        }
    }
}

async fn handle_task(cmd: TaskCommand, _config_path: &Path) -> Result<()> {
    // Tasks live in the agent in-memory store, exposed via the agent HTTP server on :50002.
    let client = reqwest::Client::builder().timeout(Duration::from_secs(5)).build()?;
    let base = "http://127.0.0.1:50002";

    match cmd {
        TaskCommand::List { status, limit } => {
            let resp = client.get(format!("{base}/tasks")).send().await;
            let body = match resp {
                Ok(r) => r.json::<serde_json::Value>().await.unwrap_or_default(),
                Err(e) => {
                    println!("{RED}✗ Cannot reach agent HTTP server (is forgefleetd running?): {e}{RESET}");
                    return Ok(());
                }
            };

            let empty = vec![];
            let all_tasks = body.get("tasks").and_then(|v| v.as_array()).unwrap_or(&empty);
            let tasks: Vec<&serde_json::Value> = all_tasks.iter()
                .filter(|t| {
                    if let Some(ref s) = status {
                        t.get("status").and_then(|v| v.as_str()) == Some(s.as_str())
                    } else { true }
                })
                .take(limit as usize)
                .collect();

            if tasks.is_empty() {
                println!("{YELLOW}No tasks found{RESET}");
                return Ok(());
            }

            println!("{GREEN}✓ Tasks ({} shown){RESET}", tasks.len());
            println!("  {:<6} {:<40} {:<12} {:<16} {}", "ID", "SUBJECT", "STATUS", "NODE", "CREATED");
            println!("  {}", "-".repeat(95));
            for t in &tasks {
                let id = t.get("id").and_then(|v| v.as_str()).unwrap_or("-");
                let subject = t.get("subject").and_then(|v| v.as_str()).unwrap_or("-");
                let status_str = t.get("status").and_then(|v| v.as_str()).unwrap_or("-");
                let node = t.get("origin_node").and_then(|v| v.as_str()).unwrap_or("-");
                let created = t.get("created_at").and_then(|v| v.as_str()).unwrap_or("-");
                let status_color = match status_str {
                    "completed" => GREEN,
                    "failed" => RED,
                    "in_progress" => CYAN,
                    _ => YELLOW,
                };
                let short_subject = &subject[..subject.len().min(39)];
                let short_created = &created[..created.len().min(19)];
                println!("  {id:<6} {short_subject:<40} {status_color}{status_str:<12}{RESET} {node:<16} {short_created}");
            }
        }
        TaskCommand::Get { id } => {
            let resp = client.get(format!("{base}/tasks")).send().await;
            let body = match resp {
                Ok(r) => r.json::<serde_json::Value>().await.unwrap_or_default(),
                Err(e) => {
                    println!("{RED}✗ Cannot reach agent HTTP server: {e}{RESET}");
                    return Ok(());
                }
            };

            let empty = vec![];
            let task = body.get("tasks").and_then(|v| v.as_array()).unwrap_or(&empty)
                .iter()
                .find(|t| {
                    t.get("id").and_then(|v| v.as_str())
                        .map(|tid| tid == id || tid.starts_with(&id))
                        .unwrap_or(false)
                });

            match task {
                None => println!("{RED}✗ Task not found: {id}{RESET}"),
                Some(t) => {
                    let tid = t.get("id").and_then(|v| v.as_str()).unwrap_or(&id);
                    let subject = t.get("subject").and_then(|v| v.as_str()).unwrap_or("-");
                    let status = t.get("status").and_then(|v| v.as_str()).unwrap_or("-");
                    let node = t.get("origin_node").and_then(|v| v.as_str()).unwrap_or("-");
                    let created = t.get("created_at").and_then(|v| v.as_str()).unwrap_or("-");
                    println!("{GREEN}✓ Task #{tid}{RESET}");
                    println!("  subject:     {subject}");
                    println!("  status:      {status}");
                    println!("  origin_node: {node}");
                    println!("  created:     {created}");
                    if let Some(output) = t.get("output").and_then(|v| v.as_str()) {
                        if !output.is_empty() {
                            println!("\n  Output:\n    {}", &output[..output.len().min(500)]);
                        }
                    }
                }
            }
        }
        TaskCommand::Update { id, status } => {
            // POST a status update via the agent message endpoint
            let valid = ["pending", "in_progress", "completed", "failed", "cancelled"];
            if !valid.contains(&status.as_str()) {
                println!("{RED}✗ Invalid status '{status}'. Valid: {}{RESET}", valid.join(", "));
                return Ok(());
            }
            let payload = serde_json::json!({
                "task_id": id,
                "status": status,
                "output": "",
                "from": "ff-cli",
            });
            let r = client.post(format!("{base}/agent/message"))
                .json(&payload)
                .send()
                .await;
            match r {
                Ok(_) => println!("{GREEN}✓ Task #{id} → {status}{RESET}"),
                Err(e) => println!("{RED}✗ Failed: {e}{RESET}"),
            }
        }
    }
    Ok(())
}

async fn handle_config(cmd: ConfigCommand, p: &Path) -> Result<()> {
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
        ConfigCommand::Nodes => {
            let pool = ff_agent::fleet_info::get_fleet_pool()
                .await
                .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
            ff_db::run_postgres_migrations(&pool).await
                .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;
            let nodes = ff_db::pg_list_nodes(&pool).await?;
            if nodes.is_empty() { println!("(no fleet nodes registered)"); return Ok(()); }
            println!("{:<12} {:<12} {:<24} {:>14}", "NODE", "RUNTIME", "MODELS_DIR", "DISK_QUOTA_PCT");
            for n in &nodes {
                println!("{:<12} {:<12} {:<24} {:>14}", n.name, n.runtime, n.models_dir, n.disk_quota_pct);
            }
            Ok(())
        }
        ConfigCommand::Node { name, key, value } => {
            let pool = ff_agent::fleet_info::get_fleet_pool()
                .await
                .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
            ff_db::run_postgres_migrations(&pool).await
                .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;
            let mut row = ff_db::pg_get_node(&pool, &name).await?
                .ok_or_else(|| anyhow::anyhow!("node '{name}' not found in fleet_nodes"))?;
            match key.as_str() {
                "runtime" => {
                    let allowed = ["mlx", "llama.cpp", "vllm", "unknown"];
                    if !allowed.contains(&value.as_str()) {
                        anyhow::bail!("runtime must be one of: mlx, llama.cpp, vllm, unknown");
                    }
                    row.runtime = value.clone();
                }
                "models_dir" => {
                    if value.trim().is_empty() { anyhow::bail!("models_dir must be non-empty"); }
                    row.models_dir = value.clone();
                }
                "disk_quota_pct" => {
                    let n: i32 = value.parse()
                        .map_err(|_| anyhow::anyhow!("disk_quota_pct must be an integer 1-100"))?;
                    if !(1..=100).contains(&n) {
                        anyhow::bail!("disk_quota_pct must be between 1 and 100");
                    }
                    row.disk_quota_pct = n;
                }
                _ => anyhow::bail!("unsupported key '{key}' (use runtime, models_dir, or disk_quota_pct)"),
            }
            ff_db::pg_upsert_node(&pool, &row).await?;
            println!("{GREEN}✓{RESET} Updated {name}.{key} = {value}");
            Ok(())
        }
    }
}
