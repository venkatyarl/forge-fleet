#![allow(clippy::type_complexity)]
#![allow(clippy::too_many_arguments)]

//! `ff` — ForgeFleet unified CLI.
//!
//! Usage:
//!   ff                          — interactive TUI agent
//!   ff "fix the bug"            — headless agent run
//!   ff start                    — start ForgeFleet daemon
//!   ff status / nodes / models / health / config / version

use std::env;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use ff_agent::agent_loop::{AgentEvent, AgentSession, AgentSessionConfig};
use ff_agent::commands::CommandRegistry;
use ff_terminal::app::App;
use ff_terminal::render;

// V43/V44: multi-host deployment + self-heal + fleet-tasks CLI modules.
// Wired here as mod decls; Command enum integration lives in the separate
// V131/V132 PRs so this commit only delivers the handlers.
mod agent_cmd;
mod alert_cmd;
mod brain_cmd;
mod cloud_llm_cmd;
mod config_cmd;
mod daemon_cmd;
mod defer_cmd;
mod events_cmd;
mod ext_cmd;
mod fabric_cmd;
mod fleet_cmd;
mod github_cmd;
mod health_cmd;
mod helpers;
mod lifecycle_cmd;
mod llm_cmd;
mod logs_cmd;
mod mcp_cmd;
mod metrics_cmd;
mod model_cmd;
mod model_serve_cmd;
mod onboard_cmd;
mod openclaw_cmd;
mod pm_cmd;
mod ports_cmd;
mod power_cmd;
mod project_cmd;
mod research_cmd;
mod secrets_cmd;
mod self_heal_cmd;
mod skills_cmd;
mod social_cmd;
mod software_cmd;
mod status_cmd;
mod storage_cmd;
mod swarm_cmd;
mod task_cmd;
mod tasks_cmd;
mod tools_cmd;
mod train_cmd;
mod utils;
mod versions_cmd;

pub use utils::{
    CYAN, GREEN, RED, RESET, YELLOW, expand_tilde, human_bytes, human_bytes_i64, load_config,
    parse_duration_secs, pulse_reader, resolve_pulse_redis_url, shell_escape_single,
    trunc_for_status, truncate_for_col, truncate_str, whoami_tag,
};

/// clap's `--version` flag prints THIS string. Must match the `Command::Version`
/// subcommand branches below so both code paths expose the same data — the
/// drift collector parses `ff YYYY.M.D_N (STATE sha)`.
///
/// Format: `YYYY.M.D_N (STATE sha)`, e.g. `2026.4.21_5 (pushed 8355028d12)`.
/// When `build.rs` ran outside a git checkout the state is `unknown` and the
/// sha degrades to `unknown`; the collector treats those as self-built-dev.
const FF_LONG_VERSION: &str = concat!(
    env!("FF_BUILD_VERSION"),
    " (",
    env!("FF_GIT_STATE"),
    " ",
    env!("FF_GIT_SHA"),
    ")"
);

/// Short version print used by `ff --version` (via clap) and `ff version`.
/// Format: `ff 2026.4.21_5 (pushed 8355028d12)`.
fn print_ff_version() {
    println!("ff {FF_LONG_VERSION}");
}

/// Long version display used by the `ff version` subcommand.
/// Prints the short form first, then a labelled block with sha / state
/// hint / build timestamp / semver.
fn print_ff_version_long() {
    let state = env!("FF_GIT_STATE");
    let hint = match state {
        "pushed" => "commit is in origin/main — safe to propagate",
        "unpushed" => "clean build of a local commit not yet in origin/main",
        "dirty" => "working tree has uncommitted changes — refuse to propagate",
        _ => "git state could not be determined (no git, no origin, etc.)",
    };
    print_ff_version();
    println!();
    println!("Primary version:  {}", env!("FF_BUILD_VERSION"));
    println!("Git SHA:          {}", env!("FF_GIT_SHA"));
    println!("Git state:        {state}       ({hint})");
    println!("Built at:         {} (local)", env!("FF_BUILT_AT"));
    println!("Cargo version:    {}", env!("CARGO_PKG_VERSION"));
}

#[derive(Debug, Parser)]
#[command(name = "ff", version = FF_LONG_VERSION, about = "ForgeFleet — distributed AI agent platform")]
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
    Start {
        #[arg(long, default_value_t = false)]
        leader: bool,
    },
    /// Stop ForgeFleet daemon
    Stop,
    Status,
    /// List fleet nodes with hardware/GPU info from Postgres.
    Nodes {
        /// Filter by GPU kind substring (e.g. amd, nvidia, apple, none).
        #[arg(long)]
        gpu: Option<String>,
        /// Output JSON instead of a table.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    Models,
    Health,
    Proxy {
        #[arg(long, default_value_t = 4000)]
        port: u16,
    },
    Discover {
        #[arg(long, default_value = "192.168.5.0/24")]
        subnet: String,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Version,
    /// Run a one-shot task against the agent.
    ///
    /// `--mode agent` (default) runs the full think→tool→observe loop with all
    /// tools registered. `--mode oneshot` disables tools entirely and limits the
    /// run to a single LLM turn — use this for pure code-gen / text-gen where
    /// tool-use would only slow things down (and smaller models can loop).
    Run {
        prompt: String,
        #[arg(long, default_value = "text")]
        output: String,
        /// Execution mode: `agent` (tools + multi-turn loop) or `oneshot`
        /// (no tools, single turn, direct text response).
        #[arg(long, default_value = "agent")]
        mode: String,
        /// Max turns (default: 30 in agent mode, 1 in oneshot mode).
        #[arg(long)]
        max_turns: Option<u32>,
        /// Layer-2 backend: `local` (default — ff's own agent loop on a
        /// fleet LLM) or one of `claude` / `codex` / `gemini` / `kimi` /
        /// `grok` (spawns the vendor CLI as a subprocess and returns its
        /// stdout). Vendor CLI uses whatever credentials are at
        /// `~/.<vendor>/`; for centralised auth run `ff oauth distribute`
        /// first.
        #[arg(long, default_value = "local")]
        backend: String,
        /// Extra args passed through to the vendor CLI (only used when
        /// `--backend` != `local`). Repeatable.
        #[arg(long = "backend-args")]
        backend_args: Vec<String>,
    },
    /// Run with supervisor — auto-detect failures, fix, and retry
    Supervise {
        prompt: String,
        /// How many supervisor retries before giving up. Accepts
        /// `--max-attempts` (canonical) or `--max-turns` (alias for
        /// consistency with `ff run`'s flag — they're semantically
        /// different but every user types `--max-turns` out of habit).
        #[arg(long, alias = "max-turns", default_value_t = 3)]
        max_attempts: u32,
        /// After agent declares done, require these files to exist + be
        /// non-empty. If any are missing, count as a failure and retry
        /// with a stronger write-first reminder. Closes the Read-loop gap
        /// where agents declare DONE without writing. Repeatable:
        /// `--verify-files a.rs --verify-files b.rs`.
        #[arg(long = "verify-files")]
        verify_files: Vec<PathBuf>,
        /// Placeholder strings that must NOT remain in any verify-files
        /// file. If found, the attempt is treated as missing_deliverable
        /// and the retry prompt names the offending file + count. Use to
        /// catch skeletons-with-TBDs where size > 0 but content isn't real.
        /// Repeatable: `--verify-no-placeholder TBD --verify-no-placeholder XXX`.
        #[arg(long = "verify-no-placeholder")]
        verify_no_placeholder: Vec<String>,
        /// Restrict the agent's tool belt to these tools only (comma-separated).
        /// Forbid Read on pure-create tasks to prevent Read-loops:
        /// `--allowed-tools Write,Bash`. When unset, all core tools are exposed.
        #[arg(long = "allowed-tools", value_delimiter = ',')]
        allowed_tools: Vec<String>,
        /// Layer-2 backend: `local` (default — ff supervisor) or one of
        /// `claude` / `codex` / `gemini` / `kimi` / `grok` (spawns the
        /// vendor CLI per attempt; ff still owns the
        /// failure-detect-and-retry loop).
        #[arg(long, default_value = "local")]
        backend: String,
        #[arg(long = "backend-args")]
        backend_args: Vec<String>,
    },
    /// Fleet-parallel research — decomposes a query into N sub-questions,
    /// dispatches each to a different fleet LLM in parallel, and synthesizes
    /// the results into a cited markdown report.
    ///
    /// Uses Schema V42 tables (research_sessions / research_subtasks /
    /// research_findings). Planner + synthesizer run on Taylor's gateway
    /// using the "thinking" pool alias (Qwen3.5-35B-A3B thinking reserve);
    /// sub-agents round-robin across distinct active fleet LLM deployments.
    Research {
        /// The research question.
        prompt: String,
        /// Number of parallel sub-agents (= sub-questions decomposed by planner).
        #[arg(long, default_value_t = 5)]
        parallel: u32,
        /// Max turns each sub-agent can take on its sub-question.
        #[arg(long, default_value_t = 6)]
        depth: u32,
        /// Write the final markdown report to this path.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Gateway base URL for LLM calls (default: http://192.168.5.100:51002).
        #[arg(long)]
        gateway: Option<String>,
        /// Model for planner + synthesizer (default: "thinking").
        #[arg(long = "planner-model")]
        planner_model: Option<String>,
        /// Model for sub-agents (default: "coder").
        #[arg(long = "subagent-model")]
        subagent_model: Option<String>,
        /// Print intermediate progress events to stderr.
        #[arg(long, default_value_t = false)]
        verbose: bool,
    },
    /// Agent coordinator — fleet-wide task dispatch via sub-agent slots.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Manage ForgeFleet tasks
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    /// Manage fleet-wide secrets (HF token, API keys, etc.) stored in Postgres.
    Secrets {
        #[command(subcommand)]
        command: SecretsCommand,
    },
    /// Deferred task queue — schedule work that runs when conditions are met
    /// (node comes online, a time is reached, manual retry).
    Defer {
        #[command(subcommand)]
        command: DeferCommand,
    },
    /// Model lifecycle management (catalog, library, deployments, jobs).
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    /// Run the deferred task worker loop (scheduler + executor).
    /// Typically run as a background service on the fleet leader.
    DeferWorker {
        /// Optional node name to use when claiming tasks; defaults to `hostname`.
        #[arg(long)]
        as_node: Option<String>,
        /// Poll interval in seconds (scheduler + fallback for Redis).
        #[arg(long, default_value_t = 15)]
        interval: u64,
        /// Also act as scheduler (evaluate triggers → dispatchable). Only one node should do this.
        #[arg(long, default_value_t = false)]
        scheduler: bool,
        /// Exit after one scheduler+worker pass (useful for tests / cron).
        #[arg(long, default_value_t = false)]
        once: bool,
    },
    /// Show installed-vs-latest tool versions across the fleet (drift matrix).
    Versions {
        #[arg(long)]
        node: Option<String>,
    },
    /// Fleet-wide operations (mesh check, verify node, etc.)
    Fleet {
        #[command(subcommand)]
        command: FleetCommand,
    },
    /// Manage LLM servers across the fleet.
    Llm {
        #[command(subcommand)]
        command: LlmCommand,
    },
    /// Manage software inventory + upgrades.
    Software {
        #[command(subcommand)]
        command: SoftwareCommand,
    },
    /// External tools — GitHub-hosted CLIs / MCP servers (schema V24).
    ///
    /// Fleet-wide package manager for dev tools like `code-review-graph`
    /// and `context-mode`. Tracks what's installed where, checks upstream
    /// for new releases, and dispatches installs via the deferred queue.
    Ext {
        #[command(subcommand)]
        command: ExtCommand,
    },
    /// Fleet-wide GitHub SSH identity registry. Manages the `Host
    /// github.com-*` aliases and the matching id_* keypairs so every
    /// fleet computer can push to GitHub from day one of enrollment.
    /// Source of truth: Postgres `github_ssh_aliases` + `fleet_secrets`.
    Github {
        #[command(subcommand)]
        command: GithubCommand,
    },
    /// MCP server installer — wire the local forgefleet MCP into each
    /// coding-agent (Claude Code, Codex, Kimi, Cursor, Windsurf, Goose)
    /// so they default to `fleet_run` / `fleet_crew` / `brain_search`
    /// instead of bash / web-fetch. Source of truth: per-tool config
    /// files under `$HOME`.
    Mcp {
        #[command(subcommand)]
        command: mcp_cmd::McpCommand,
    },
    /// Manage the V105 skills catalog: import git repos of SKILL.md
    /// files, list/show/sync to disk, remove or retire entries. The
    /// runtime skill_catalog reader picks them up at session start.
    Skills {
        #[command(subcommand)]
        command: skills_cmd::SkillsCommand,
    },
    /// Fleet-wide swarm orchestration: plan → fan out N sub-tasks
    /// across fleet computers via fleet_tasks → synthesize a final
    /// result. The horizontal alternative to Kimi K2.6's cloud-only
    /// 300-agent swarm — runs on YOUR hardware at $0/token.
    Swarm {
        #[command(subcommand)]
        command: swarm_cmd::SwarmCommand,
    },
    /// Self-service onboarding helpers (show curl command, list recent, revoke).
    Onboard {
        #[command(subcommand)]
        command: OnboardCommand,
    },
    /// Virtual Brain vault indexer + utilities.
    #[command(alias = "brain")]
    VirtualBrain {
        #[command(subcommand)]
        command: BrainCommand,
    },
    /// OpenClaw gateway/node visibility across the fleet.
    Openclaw {
        #[command(subcommand)]
        command: OpenclawCommand,
    },
    /// Project management — projects, work items, branches.
    Pm {
        #[command(subcommand)]
        command: PmCommand,
    },
    /// Project metadata — repos, environments, CI.
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Alert policies + alert events (Phase 10 observability).
    Alert {
        #[command(subcommand)]
        command: AlertCommand,
    },
    /// Metrics history (downsampled Pulse beats, 90-day retention).
    Metrics {
        #[command(subcommand)]
        command: MetricsCommand,
    },
    /// Tail fleet logs via NATS. Requires FORGEFLEET_NATS_URL (default nats://127.0.0.1:54222).
    /// Subscribes to `logs.{computer}.{service}.>`.
    Logs {
        #[arg(long)]
        computer: Option<String>,
        #[arg(long)]
        service: Option<String>,
        #[arg(long, default_value_t = 50)]
        tail: usize,
    },
    /// Subscribe to the NATS fleet-events bus and stream events to stdout.
    /// Subject defaults to `fleet.events.>`; supply `--subject` to narrow
    /// (e.g. `fleet.events.member.>` or `fleet.pulse.>`).
    Events {
        #[command(subcommand)]
        command: EventsCommand,
    },
    /// Shared NFS storage — declare exported volumes and mount them on
    /// fleet nodes. See `ff storage share --help`.
    Storage {
        #[command(subcommand)]
        command: StorageCommand,
    },
    /// Power scheduling — cron-driven sleep/wake/restart rules per computer.
    Power {
        #[command(subcommand)]
        command: PowerCommand,
    },
    /// LoRA / full-finetune training job orchestration.
    Train {
        #[command(subcommand)]
        command: TrainCommand,
    },
    /// Port registry — inventory of every port ForgeFleet uses.
    Ports {
        #[command(subcommand)]
        command: PortsCommand,
    },
    /// Cloud LLM providers (OpenAI/Anthropic/Moonshot/Google). Gateway
    /// routes `/v1/chat/completions` to these when the requested model
    /// matches a provider's `model_prefix`.
    CloudLlm {
        #[command(subcommand)]
        command: CloudLlmCommand,
    },
    /// Social media ingest — pull a TikTok / Instagram / Twitter(X) / YouTube
    /// URL, fetch its media, and run a vision-LLM analysis over its frames.
    Social {
        #[command(subcommand)]
        command: SocialCommand,
    },
    /// Run ForgeFleet's unified daemon: deferred-task scheduler+worker, disk
    /// sampler, and deployment reconciler all in one long-lived process.
    /// Typically run on boot via launchd/systemd.
    Daemon {
        /// Worker node name (defaults to this host via DB lookup).
        #[arg(long)]
        as_node: Option<String>,
        /// Act as the deferred-task scheduler too (only one node should).
        #[arg(long, default_value_t = false)]
        scheduler: bool,
        /// Deferred-worker poll interval in seconds.
        #[arg(long, default_value_t = 15)]
        defer_interval: u64,
        /// Disk-sampler interval in seconds (default 300 = 5 min).
        #[arg(long, default_value_t = 300)]
        disk_interval: u64,
        /// Reconciler interval in seconds (default 60).
        #[arg(long, default_value_t = 60)]
        reconcile_interval: u64,
        /// Exit after one pass of each (useful for cron/testing).
        #[arg(long, default_value_t = false)]
        once: bool,
    },
    /// V43: multi-host private-fabric pair operations (CX-7, InfiniBand, RoCE).
    Fabric {
        #[command(subcommand)]
        command: FabricCommand,
    },
    /// V43/V44: fleet-wide task board view.
    Tasks {
        #[command(subcommand)]
        command: TasksCommand,
    },
    /// V54: outcome-driven multi-LLM sessions. A session has a goal,
    /// a step DAG, and a team of role→model assignments. The
    /// orchestrator (running on the leader) walks the DAG, dispatches
    /// each step via fleet_tasks, and finalises the session when all
    /// steps are terminal.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// OAuth subscription credentials — harvest from a vendor CLI's local
    /// cred file on the leader, distribute to fleet members. Powers
    /// Layer 1 (`oauth_subscription` auth_kind) of the multi-LLM CLI
    /// integration. Verbs are provider-agnostic; pass `all` to fan out.
    Oauth {
        #[command(subcommand)]
        command: OauthCommand,
    },
    /// V43: self-heal coordination (operator escape-hatches).
    SelfHeal {
        #[command(subcommand)]
        command: SelfHealCommand,
    },
    /// Fleet Tool Registry — discover, inspect, and manage tools across all nodes.
    Tools {
        #[command(subcommand)]
        command: ToolsCommand,
    },
    /// Docker stack placement — rank fleet hosts by free RAM/disk for a workload,
    /// excluding reserved-class hosts (leader + DGX). Encodes the policy used by
    /// `ff model distribute`; useful for any docker-compose service the operator
    /// wants to relocate.
    Stack {
        #[command(subcommand)]
        command: StackCommand,
    },
}

#[derive(Debug, Clone, Subcommand)]
enum StackCommand {
    /// Rank candidate hosts for a docker workload. Skips Taylor (leader) and
    /// DGX (training-reserved) by default. Sorts by free RAM desc (RAM is
    /// usually the binding constraint for docker services).
    HostRank {
        /// Minimum RAM required for the workload (GB). Hosts under this
        /// threshold are excluded. Default: 4 GB.
        #[arg(long, default_value_t = 4)]
        min_ram_gb: i64,
        /// Exclude these hosts (comma-separated).
        #[arg(long, default_value = "")]
        exclude: String,
        /// Show the full ranking instead of just the top pick.
        #[arg(long, default_value_t = false)]
        all: bool,
    },
}

#[derive(Debug, Clone, Subcommand)]
enum FabricCommand {
    /// Record that two computers are linked by a private fabric.
    /// Does NOT assign IPs (still manual via nmcli); once both sides start
    /// emitting fabric-kind IPs with paired_with, the materializer auto-fills.
    Pair {
        /// First computer name.
        a: String,
        /// Second computer name.
        b: String,
        /// Fabric kind: cx7-200g | cx7-400g | ib-100g | roce-100g | tb3 | tb4 | tb5.
        #[arg(long, default_value = "cx7-200g")]
        kind: String,
    },
    /// Run iperf3 across a fabric pair and record measured throughput.
    /// Stores into `fabric_measurements` table for trend tracking.
    /// Both directions tested by default; pass `--reverse` for B→A only.
    Benchmark {
        /// First computer name (iperf3 client).
        a: String,
        /// Second computer name (iperf3 server).
        b: String,
        /// Test duration in seconds (default 30).
        #[arg(long, default_value = "30")]
        duration: u32,
        /// Number of parallel streams (default 1).
        #[arg(long, default_value = "1")]
        streams: u32,
        /// Skip A→B direction.
        #[arg(long)]
        reverse_only: bool,
    },
    /// Show fabric measurements (trend over time).
    Measurements {
        /// Filter by node pair.
        #[arg(long)]
        a: Option<String>,
        #[arg(long)]
        b: Option<String>,
        /// How many recent rows to show.
        #[arg(long, default_value = "20")]
        limit: i64,
    },
    /// Iterate every `fabric_pairs` row and benchmark each — keeps
    /// `measured_bandwidth_gbps` fresh fleet-wide. Suitable for daily cron.
    BenchmarkAll {
        /// Test duration in seconds per pair (default 10 — shorter than
        /// the per-pair default since this is a sweep).
        #[arg(long, default_value = "10")]
        duration: u32,
        /// Number of parallel streams (default 1).
        #[arg(long, default_value = "1")]
        streams: u32,
    },
}

#[derive(Debug, Clone, Subcommand)]
enum TasksCommand {
    /// List fleet_tasks with optional filters.
    List {
        #[arg(long)]
        computer: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long = "type")]
        task_type: Option<String>,
        /// Prefix every row with the task UUID.
        #[arg(long, default_value_t = false)]
        show_id: bool,
    },
    /// Enqueue a shell task. Workers on members whose capability set
    /// covers `--capability` will compete for it.
    Add {
        /// Human-readable summary.
        #[arg(long)]
        summary: String,
        /// Shell command to run. Pass via single quotes.
        #[arg(long)]
        command: String,
        /// Required capabilities, comma-separated. e.g. "linux,redis-cli".
        #[arg(long, default_value = "")]
        capability: String,
        /// Pin to a specific computer name. If absent, any eligible worker may claim.
        #[arg(long)]
        preferred: Option<String>,
        /// Higher = picked first. Default 50.
        #[arg(long, default_value_t = 50)]
        priority: i32,
        /// Max task duration in seconds before the worker kills it. Without
        /// this the worker falls back to its built-in default (~600s) — too
        /// short for agent/research tasks. Sets fleet_tasks.timeout_secs.
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Show detailed status, payload, and result for one task.
    Get { id: String },
    /// Cancel a pending or running task. The row flips to `cancelled`;
    /// the worker's completion UPDATE is gated on status='running' so
    /// a late-completing hung worker won't clobber the cancellation.
    /// The child process keeps running on the worker until it exits
    /// or hits MAX_TASK_DURATION (30 min default).
    Cancel {
        id: String,
        /// Reason recorded in the task's `error` field.
        #[arg(long, default_value = "cancelled by operator")]
        reason: String,
    },
    /// Compose the multi-step "bring `<target>` online" task graph.
    /// Reads the target's IPs / ssh user / OS family from `computers`
    /// at compose time — no hardcoded values.
    ComposeNodeBootstrap {
        /// Computer name (must already have a row in `computers`).
        target: String,
    },
    /// Compose a wave-based fleet upgrade for `<software_id>`.
    /// Each task is "executor SSHs into target and runs the playbook";
    /// peer-driven, no daemon-restarts-itself bug. Leader is excluded
    /// from the graph (restart manually).
    ComposeFleetUpgrade {
        /// software_registry id, e.g. `forgefleetd_git`.
        software_id: String,
        /// Targets per wave; subsequent waves run after earlier ones.
        #[arg(long, default_value_t = 4)]
        fanout: usize,
    },
}

#[derive(Debug, Clone, Subcommand)]
enum SelfHealCommand {
    /// Show the self-heal queue + per-daemon trust scores.
    Status,
    /// Halt all in-flight self-heal fixes.
    Pause,
    /// Require human approval for a tier for N hours (probation).
    FreezeTier {
        tier: String,
        #[arg(long, default_value_t = 24)]
        hours: u32,
    },
    /// Rollback a specific fix by bug signature.
    Revert { bug_signature: String },
    /// Reset a daemon's trust score back to operator-approve probation.
    TrustReset { computer: String },
    /// Run the writer LLM for a given bug signature (internal pipeline).
    RunWriter {
        #[arg(long)]
        bug_sig: String,
    },
}

#[derive(Debug, Clone, Subcommand)]
enum ToolsCommand {
    /// List all tools registered across the fleet.
    List {
        /// Filter by node name.
        #[arg(long)]
        node: Option<String>,
        /// Filter by tool name (substring match).
        #[arg(long)]
        name: Option<String>,
        /// Show only unhealthy tools (stale >5 min).
        #[arg(long)]
        unhealthy: bool,
    },
    /// Show tool health status across all nodes.
    Health,
    /// Register local tools with the fleet registry.
    Register {
        /// Node name to register as (defaults to hostname).
        #[arg(long)]
        node: Option<String>,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum ConfigCommand {
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
enum AgentCommand {
    /// Dispatch a prompt to a computer's local LLM via the agent coordinator.
    /// If `--work-item-id` is omitted, creates a transient work_item in the
    /// `ff-agent-dispatch` project.
    Dispatch {
        /// The prompt to send.
        prompt: String,
        /// Route the task to this computer (by name). If omitted, uses any
        /// idle sub-agent slot fleet-wide, preferring online computers.
        #[arg(long)]
        to_computer: Option<String>,
        /// Reuse an existing work_items.id instead of creating a transient one.
        #[arg(long)]
        work_item_id: Option<String>,
        /// Emit JSON instead of pretty text.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// List every sub_agent slot (seeded or live).
    SubAgents {
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Fan out `N` copies of one prompt across the fleet via
    /// `fleet_tasks` — each task runs `ff run --backend <backend>` on
    /// a member that has the matching capability tag (e.g. `claude`).
    /// With ~14 members × 5 CLIs the fleet has ~70 concurrent slots.
    /// Returns the parent task UUID so the caller can `ff tasks list`
    /// to watch progress.
    Fanout {
        /// The prompt to fan out.
        prompt: String,
        /// Vendor backend: claude / codex / gemini / kimi / grok.
        /// Maps to a `requires_capability=[<backend>]` constraint on
        /// each child task.
        #[arg(long, default_value = "claude")]
        backend: String,
        /// Number of parallel copies. Each is a separate task; workers
        /// compete via SKIP LOCKED.
        #[arg(long, default_value_t = 5)]
        fanout: u32,
    },
    /// Run the same prompt on every fleet member that has `<backend>`'s
    /// CLI installed. One task per capable member; observable via
    /// `ff tasks list`.
    DispatchEach {
        prompt: String,
        #[arg(long, default_value = "claude")]
        backend: String,
    },
    /// Seed slot 0 for every computer in the `computers` table.
    /// Idempotent — existing rows are left alone.
    Seed,
    /// Lift fleet-LLM-produced code from a worker's sub-agent workspace back
    /// to Taylor's canonical repo via a feature branch + (optional) PR on
    /// origin/main.
    ///
    /// Looks up the agent session by ID in `work_outputs` (match on
    /// `agent_session_id`) to find the worker name + modified files. SSHes
    /// into the worker and runs `git checkout -b <branch>`, `git add` on the
    /// recorded files, `git commit`, then optionally `git push` and
    /// `gh pr create --base main`. See issue #118.
    CommitBack {
        /// The ff-agent session id (UUID) that produced the code. The session
        /// must have a matching row in `work_outputs.agent_session_id`.
        session: String,
        /// Also run `git push -u origin <branch>` after committing locally.
        #[arg(long, default_value_t = false)]
        push: bool,
        /// After pushing, open a PR via `gh pr create --base main`.
        /// Implies `--push`.
        #[arg(long, default_value_t = false)]
        pr: bool,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum DeferCommand {
    /// List deferred tasks. Filter by status or limit count.
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// Enqueue a shell command to run when a target node comes online.
    /// Example: ff defer add-shell --when-node-online ace --run "rm -rf ~/.ollama" --title "Ollama cleanup on ace"
    AddShell {
        /// Human-readable title shown in listings.
        #[arg(long)]
        title: String,
        /// Shell command to execute on the target node (via SSH).
        #[arg(long)]
        run: String,
        /// Trigger: task runs when this node becomes reachable.
        #[arg(long = "when-node-online")]
        when_node_online: Option<String>,
        /// Optional: run at a specific RFC3339 time instead (UTC).
        #[arg(long = "when-at")]
        when_at: Option<String>,
        /// Node that should execute the command (defaults to the target in when-node-online).
        #[arg(long = "on-node")]
        on_node: Option<String>,
        #[arg(long, default_value_t = 5)]
        max_attempts: i32,
    },
    /// Show details for a single deferred task by id.
    Get { id: String },
    /// Cancel a pending/dispatchable/failed task.
    Cancel { id: String },
    /// Retry a failed or cancelled task (resets attempts-aware status, runs ASAP).
    Retry { id: String },
}

#[derive(Debug, Clone, Subcommand)]
enum SessionCommand {
    /// Create a new outcome-driven session. No steps are added
    /// automatically — use `ff session step add` to compose the DAG
    /// (LLM-driven decomposition by the planner role is a follow-up).
    Spawn {
        /// The user-stated outcome.
        goal: String,
        /// Optional per-session budget cap (USD). Orchestrator stops
        /// dispatching when cumulative cost reaches this.
        #[arg(long)]
        budget: Option<f64>,
    },
    /// Append a step to an existing session.
    AddStep {
        session: String,
        /// Step name (free-form, shown in `ff session get`).
        #[arg(long)]
        name: String,
        /// Role tag (planner / coder / reviewer / browser / synthesiser).
        /// When unset, the step uses the default LLM (qwen3-coder-30b).
        #[arg(long)]
        role: Option<String>,
        /// The LLM prompt this step should run.
        #[arg(long)]
        prompt: String,
        /// IDs of sibling steps that must complete first. Repeatable.
        #[arg(long = "depends-on")]
        depends_on: Vec<String>,
    },
    /// List recent sessions with progress counters.
    List {
        #[arg(long, default_value_t = 30)]
        limit: i64,
    },
    /// Show one session: full step DAG + per-step results.
    Get { id: String },
    /// Read a session_brain entry — per-session shared memory across
    /// roles. JSON value is printed verbatim.
    BrainGet { session: String, key: String },
    /// Write a session_brain entry. Value is parsed as JSON; if the
    /// parse fails it's stored as a JSON string.
    BrainSet {
        session: String,
        key: String,
        /// JSON value (or any string — falls back to JSON string).
        value: String,
        /// Optional role tag.
        #[arg(long)]
        role: Option<String>,
    },
    /// List every session_brain entry for a session, newest first.
    BrainList { session: String },
    /// Add an LLM-driven planner step to a session. The planner role
    /// is asked to decompose the session's goal into a JSON DAG;
    /// follow up with `ff session apply-plan` once it completes.
    Plan { session: String },
    /// Read the most recent completed planner step's output and
    /// insert its planned children as agent_steps. If --from-step is
    /// passed, uses that specific step's output instead.
    ApplyPlan {
        session: String,
        #[arg(long = "from-step")]
        from_step: Option<String>,
    },
    /// Add a parallel multi-LLM vote: N voters run the same prompt
    /// against different models; a tally step depends on all and
    /// picks consensus. Voters are model names (claude-opus-4-7,
    /// gpt-5, gemini-2.5-pro, etc.).
    Vote {
        session: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        prompt: String,
        /// Comma-separated model names. Each becomes one voter.
        #[arg(long, value_delimiter = ',')]
        voters: Vec<String>,
        /// Role used for the tally step (default: synthesiser).
        #[arg(long = "tally-role")]
        tally_role: Option<String>,
    },
    /// Collect the raw answers from a completed vote and store them
    /// in session_brain under `vote_<name>` for operator review.
    VoteCollect {
        session: String,
        #[arg(long)]
        name: String,
    },
    /// Cancel a session in flight: flips status to `cancelled`,
    /// marks pending steps `cancelled`, and cancels still-running
    /// fleet_tasks via the existing pg_cancel_task helper.
    Cancel { id: String },
}

#[derive(Debug, Clone, Subcommand)]
enum OauthCommand {
    /// Harvest the OAuth/session token from one provider's local cred file
    /// on the leader and store it in `fleet_secrets[<provider>.oauth_token]`.
    /// Pass `all` to harvest every configured provider at once.
    Import {
        /// Provider name: `claude`, `codex`, `gemini`, `kimi`, `grok`, or `all`.
        provider: String,
    },
    /// Push the leader's credential file out to every other fleet member's
    /// matching path (mode 0600). After this, ff-driven CLI invocations on
    /// any member use the centralised token. Pass `all` to fan out for
    /// every provider at once.
    ///
    /// TOS WARNING: most vendor consumer subscriptions (Claude Pro,
    /// ChatGPT Plus, Kimi Pro) prohibit using one account on N concurrent
    /// machines. This verb is TOS-grey; running it acknowledges that you
    /// take responsibility for compliance. Strict-compliance shops should
    /// run a separate per-member subscription instead.
    ///
    /// Without `--yes`, the verb prints a confirmation prompt before
    /// fanning out. CI / cron callers should pass `--yes` once they've
    /// made the decision.
    Distribute {
        /// Provider name: `claude`, `codex`, `gemini`, `kimi`, `grok`, or `all`.
        provider: String,
        /// Skip the interactive TOS-acknowledgement prompt. Required for
        /// non-interactive callers (cron, CI, deferred tasks).
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// Show per-provider OAuth state: cred-file present on leader,
    /// mtime, token-in-fleet_secrets, token-preview.
    Status,
    /// Long-running foreground watcher: re-imports + re-distributes
    /// whenever any leader cred file changes (vendor CLI refreshed its
    /// token). Run on the leader; ctrl-C to exit.
    RefreshWatch,
    /// Probe each oauth_subscription provider's API to verify the
    /// harvested token still authenticates. Reports OK / 401 / network
    /// error per provider. Pass `all` (default) to probe every
    /// configured provider, or a name to probe one.
    Probe {
        /// Provider name: `claude`, `codex`, `gemini`, `kimi`, `grok`,
        /// or `all`.
        #[arg(default_value = "all")]
        provider: String,
    },
    /// Refresh — spawn the vendor CLI to trigger its internal token
    /// refresh, then re-import the (potentially newer) token to
    /// `fleet_secrets`. Useful as a periodic cron tick to keep the
    /// harvested token fresh as access_tokens age toward expiry.
    /// Runs `probe` (which spawns the CLI with a tiny prompt — that
    /// causes the CLI to refresh its tokens if stale) and then
    /// `import` (which re-reads the cred source). Pass `all` to
    /// refresh every provider whose cred is currently importable.
    Refresh {
        /// Provider name: `claude`, `codex`, `gemini`, `kimi`, `grok`,
        /// or `all`.
        #[arg(default_value = "all")]
        provider: String,
    },
}

#[derive(Debug, Clone, Subcommand)]
enum FleetCommand {
    /// Pairwise SSH reachability check across the fleet (N×(N-1) probes).
    SshMeshCheck {
        #[arg(long)]
        node: Option<String>,
        #[arg(long)]
        json: bool,
        /// Only re-probe pairs whose last_checked in fleet_mesh_status is
        /// older than the given ISO-8601 duration prefix (e.g. "1h", "30m", "2d").
        #[arg(long)]
        since: Option<String>,
        /// Before probing, re-distribute user + host keys to any pair that
        /// is currently status='failed'. Requires --yes to actually run.
        #[arg(long, default_value_t = false)]
        repair: bool,
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// Full 12-check verify battery for one node.
    VerifyNode {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Show the current fleet leader + election state.
    Leader {
        #[arg(long)]
        json: bool,
    },
    /// Show computer health table (SDOWN/ODOWN flags).
    Health {
        #[arg(long)]
        json: bool,
    },
    /// Show per-host code identity (SHA) + convergence status. Designed
    /// to answer "is the whole fleet on the same code?" without the
    /// per-machine build counter confusing the picture.
    Versions {
        /// Show the verbose per-machine build counter alongside the SHA.
        #[arg(long, default_value_t = false)]
        verbose: bool,
        /// SSH each host in parallel and read the live `forgefleetd
        /// --version` output, instead of using the cached
        /// `computer_software.installed_version` (refreshed every 6h
        /// by the version_check tick). Slower but truthful right
        /// after an upgrade.
        #[arg(long, default_value_t = false)]
        live: bool,
    },
    /// Debug: dump local peer_map + what each member sees.
    Gossip,
    /// Migrate every fleet node to a new GitHub owner + move the repo from
    /// ~/taylorProjects/forge-fleet → ~/projects/forge-fleet. Enqueues one
    /// idempotent shell task per node via the deferred queue (trigger=node_online),
    /// so offline nodes pick it up when they come back online.
    MigrateGithub {
        /// New GitHub owner/org for the forge-fleet remote (default: venkatyarl).
        #[arg(long, default_value = "venkatyarl")]
        new_owner: String,
        /// Skip the local node (the one running this command). Default: true.
        #[arg(long, default_value_t = true)]
        skip_local: bool,
        /// Only enqueue for this specific node (for testing a single target).
        #[arg(long)]
        only: Option<String>,
        /// Show planned enqueues without writing to the defer queue.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// Required to actually enqueue (otherwise prints plan and exits).
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// Manually trigger revive for a specific computer.
    Revive {
        /// Computer name (e.g. "marcus")
        computer: String,
        /// Skip the SSH probe — go straight to WoL + alert.
        #[arg(long, default_value_t = false)]
        wol_only: bool,
        /// Internal flag: called by the deferred task scheduler, output terse JSON.
        #[arg(long, default_value_t = false, hide = true)]
        internal: bool,
    },
    /// Fleet task-coverage requirements (drives CoverageGuard).
    TaskCoverage {
        #[command(subcommand)]
        command: TaskCoverageCommand,
    },
    /// Revoke a computer's SSH trust across the fleet. Removes its
    /// user public key from every other alive computer's authorized_keys.
    RevokeTrust {
        /// Computer name whose key should be revoked (e.g. "marcus").
        #[arg(long)]
        computer: String,
        /// Required — revocation is destructive.
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// Permanently remove a computer from the fleet.
    ///
    /// Deletes every DB row tied to the computer (fleet_workers + computers
    /// and their cascades), clears leader state if it was the elected leader,
    /// and enqueues a deferred `node_online` task on Taylor that fans out an
    /// SSH revocation of the removed node's public key across every remaining
    /// peer's authorized_keys. Publishes `fleet.events.computer_removed` on
    /// NATS best-effort.
    RemoveComputer {
        /// Computer name (e.g. "ace").
        name: String,
        /// Required — removal is destructive.
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// EMERGENCY: wipe every non-Taylor computer from the fleet registry.
    ///
    /// Iterates every computer whose name is not "taylor" and runs the same
    /// removal logic as `remove-computer` against each. Intended for rebuilds
    /// from scratch. Requires BOTH `--yes` and `--i-know-what-im-doing`.
    Disband {
        /// Required — disbanding is destructive.
        #[arg(long, default_value_t = false)]
        yes: bool,
        /// Second required flag — makes the operator spell out the consequence.
        #[arg(long = "i-know-what-im-doing", default_value_t = false)]
        i_know_what_im_doing: bool,
    },
    /// Plan 14 source-tree migration: move `~/taylorProjects/forge-fleet`
    /// to the canonical path (`computers.source_tree_path`, default
    /// `~/.forgefleet/sub-agent-0/forge-fleet`) on every non-Taylor node.
    ///
    /// Inspects each node over SSH, prints a plan (legacy present / canonical
    /// present / needs clone), and with --yes enqueues one deferred shell task
    /// per candidate (trigger=node_online). Idempotent: already-migrated nodes
    /// are skipped.
    MigrateSourceTrees {
        /// Print the plan and exit without enqueueing.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// Required to actually enqueue (otherwise prints plan and exits).
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// Rotate a computer's own SSH keypair. Currently stubbed.
    RotateSshKey {
        #[arg(long)]
        computer: String,
    },
    /// Rotate the fleet-wide pulse_beat_hmac_key. Every daemon picks up
    /// the new key on next 5-minute refresh cycle.
    RotatePulseHmac {
        /// Optional explicit value (64 hex chars). If omitted, generate.
        #[arg(long)]
        value: Option<String>,
    },
    /// Encrypted backup — produce one now and report path + recipient.
    Backup {
        /// Backup kind: postgres, redis, or all.
        #[arg(long, default_value = "postgres")]
        kind: String,
        /// Bypass the "leader only" gate (run locally regardless).
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// Set a computer's network_scope (lan | tailscale_only | wan).
    ///
    /// Controls whether revive tries WoL (LAN only) and which IP the
    /// fleet prefers when opening SSH / Pulse connections. The default
    /// `lan` matches every LAN-joined node; tailscale-only laptops and
    /// off-site WAN replicas should be explicitly set.
    SetNetworkScope {
        /// Computer name, e.g. "taylor".
        computer: String,
        /// One of: lan | tailscale_only | wan.
        scope: String,
    },
    /// Database / replica operations.
    Db {
        #[command(subcommand)]
        command: FleetDbCommand,
    },
    /// EMERGENCY: halt every daemon across the fleet.
    ///
    /// For runaway loops, resource exhaustion, or misbehaving task spam.
    /// Runs `launchctl unload` / `systemctl --user stop` on every computer
    /// — locally for the current node, via SSH for the rest. Remotes run
    /// in parallel. Reports N of M stopped and lists any SSH failures.
    ///
    /// Use `ff fleet resume` to bring everything back up.
    PanicStop {
        /// Required — panic-stop is destructive.
        #[arg(long, default_value_t = false)]
        yes: bool,
        /// ALSO stop the Taylor-local Docker data-plane stack
        /// (postgres/redis/sentinel/nats) for a true full halt. No-op if
        /// this isn't Taylor.
        #[arg(long, default_value_t = false)]
        halt_dbs: bool,
    },
    /// Restart every daemon across the fleet (undo a panic-stop).
    Resume {
        /// Required — resume touches every computer.
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// Isolate a misbehaving computer without removing it from the registry.
    ///
    /// Stops its daemons over SSH, flips `computers.status='maintenance'`,
    /// demotes any OpenClaw gateway row back to 'node', and publishes
    /// `fleet.events.quarantine` on NATS. The node won't participate in
    /// leader election or receive LLM requests while quarantined.
    Quarantine {
        /// Computer name (e.g. "sophie").
        computer: String,
        /// Required — quarantine is destructive to the node's role.
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// Reverse a quarantine: restart daemons, flip status back to 'pending'.
    /// The next pulse beat will move it to 'online'.
    Unquarantine {
        /// Computer name.
        computer: String,
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// Upgrade a software entry across the fleet using its upgrade_playbook.
    ///
    /// Looks up every (computer, software_id) row in computer_software,
    /// resolves the correct playbook key (`{os_family}-{install_source}` →
    /// `{os_family}` → `all`), and enqueues one shell task per target via
    /// the deferred task queue (trigger=node_online). Offline nodes pick
    /// the task up when they come back.
    Upgrade {
        /// Software ID (e.g. "gh", "openclaw", "ff").
        software_id: String,
        /// Target exactly one computer.
        #[arg(long)]
        computer: Option<String>,
        /// Target every computer that has this software installed.
        #[arg(long, default_value_t = false)]
        all: bool,
        /// Print the plan without enqueueing.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// Required to actually enqueue (otherwise prints plan and exits).
        #[arg(long, default_value_t = false)]
        yes: bool,
        /// Bypass the dirty-build gate for `ff_git` / `forgefleetd_git`.
        /// Default: the gate refuses to propagate a leader whose working
        /// tree has uncommitted changes.
        #[arg(long, default_value_t = false)]
        force_dirty: bool,
    },
    /// Resolve fleet computers from Postgres (with fleet.toml fallback).
    Computers {
        /// Output format: text or json
        #[arg(long, default_value = "text")]
        format: String,
        /// Filter by OS substring (e.g. "linux", "macos")
        #[arg(long)]
        os: Option<String>,
        /// Filter by role substring (e.g. "worker", "leader")
        #[arg(long)]
        role: Option<String>,
    },
    /// Run a command synchronously on a fleet computer over SSH.
    ///
    /// Resolves the node's ssh_user + best-reachable IP (LAN preferred,
    /// Tailscale fallback) from Postgres, opens SSH, streams stdout/stderr
    /// live, and exits with the remote command's exit code. Use `--` to
    /// pass arbitrary commands (including flags) verbatim:
    ///
    ///   ff fleet exec sia -- nvidia-smi --query-gpu=name --format=csv
    ///
    /// Unlike `ff fleet upgrade` / the defer queue, this is synchronous —
    /// you get the output and exit code right now.
    Exec {
        /// Computer name (e.g. "sia") or IP.
        node: String,
        /// Emit a single JSON object {node, exit_code, stdout, stderr}
        /// instead of streaming. Captures output rather than streaming it.
        #[arg(long, default_value_t = false)]
        json: bool,
        /// The command (and its arguments) to run on the remote host.
        /// Everything after `--` is passed through verbatim.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
    /// Fast, PARALLEL self-built deploy of forgefleetd + ff to fleet hosts.
    ///
    /// A faster, observable alternative to the `ff tasks compose-fleet-upgrade`
    /// wave (which is fanout-serialized, cold-builds per host, and stalls on
    /// flaky 32GB Linux boxes). This runs the canonical forgefleetd_git
    /// upgrade playbook (git fetch + reset --hard origin/main → cargo build
    /// --release -p forge-fleet -p ff-terminal → install both binaries →
    /// codesign on macOS → restart per os_family) over SSH on every target at
    /// once (bounded concurrency), then verifies convergence by reading each
    /// host's RUNNING binary SHA.
    ///
    /// The Postgres-resolved SSH path is identical to `ff fleet exec`
    /// (user@ip from the `computers` table, never ~/.ssh/config).
    ///
    /// NOTE: the leader (Taylor) is EXCLUDED from `--all` — it restarts
    /// itself badly (kills the daemon mid-deploy). Deploy the leader by hand,
    /// or target it explicitly with `--node taylor` if you accept the risk.
    Deploy {
        /// Deploy to ALL online non-leader computers (leader excluded).
        #[arg(long, default_value_t = false)]
        all: bool,
        /// Deploy to a single named computer (or IP). Mutually exclusive
        /// with --all. The only way to target the leader.
        #[arg(long)]
        node: Option<String>,
        /// Max hosts building concurrently (default 6).
        #[arg(long, default_value_t = 6)]
        concurrency: usize,
        /// Emit a per-host results JSON array instead of the summary table.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum GithubCommand {
    /// List the aliases (and key fingerprints) currently registered in
    /// the DB. Does not print private key material.
    List,
    /// Pull aliases + keys from the DB and apply them to *this*
    /// computer's `~/.ssh/`. Idempotent: skips aliases already present
    /// in `~/.ssh/config` and skips key files that already match.
    /// Intended for enrollment bootstrap — also safe to re-run.
    Sync {
        /// Show what would happen without writing.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum FleetDbCommand {
    /// Register an off-site Postgres replica reachable via Tailscale (or
    /// another overlay network). Stores a row in `database_replicas` with
    /// role='wan_replica' and prints the compose command to run on the
    /// remote machine. See deploy/WAN_REPLICATION.md for the full runbook.
    AddRemoteReplica {
        /// Name of the computer that will host the replica (must already
        /// exist in the `computers` table — run `ff onboard` first).
        #[arg(long)]
        computer: String,
        /// Overlay transport. Currently only `tailscale` is recognised;
        /// other values print a warning but still record the row.
        #[arg(long, default_value = "tailscale")]
        via: String,
        /// Skip the Tailscale reachability probe (used for docs / dry run).
        #[arg(long, default_value_t = false)]
        skip_probe: bool,
    },
    /// Manually trigger a Postgres failover — promote the replica on
    /// `--to <computer>` to primary. This calls the same code path as the
    /// automatic failover that runs inside `leader_tick`.
    ///
    /// Intended for planned cutovers or recovering from a stuck auto
    /// failover. Must be run on the target computer (the new primary).
    Failover {
        /// Name of the computer whose local replica should be promoted.
        /// Must match `hostname` / `fleet_workers.name`.
        #[arg(long = "to")]
        to: String,
        /// Proceed even if the target isn't the current ForgeFleet leader
        /// and/or fencing the old primary fails.
        #[arg(long, default_value_t = false)]
        force: bool,
        /// Skip the interactive confirmation prompt.
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// Restore an age-encrypted backup into a *scratch* Postgres database.
    ///
    /// Looks up the row in `backups` by id, decrypts using
    /// `fleet_secrets.backup_encryption_privkey`, creates the target DB
    /// (default: `forgefleet_restored`) inside the `forgefleet-postgres`
    /// container, and streams the plaintext archive back in via
    /// `pg_restore` (or `psql` for plain SQL dumps). Never overwrites the
    /// live `forgefleet` database.
    Restore {
        /// Backup ID (UUID) from the `backups` table.
        backup_id: String,
        /// Target computer name (reserved for future SSH hand-off;
        /// currently only local restore is supported — anything else
        /// prints a TODO and exits).
        #[arg(long)]
        to: Option<String>,
        /// Target database name. A scratch DB is created and the archive
        /// is loaded into it. Defaults to `forgefleet_restored`.
        #[arg(long, default_value = "forgefleet_restored")]
        target_db: String,
        /// Required — restore actually touches Postgres (creates the DB).
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// Audit every recent backup: size, checksum, decryptability.
    ///
    /// With `--test-restore`, additionally does a full round-trip on the
    /// single most recent Postgres backup — restore to a scratch DB,
    /// count tables, drop the scratch DB.
    VerifyBackups {
        /// How many recent rows (per kind) to show. Default 10.
        #[arg(long, default_value_t = 10)]
        limit: i64,
        /// Run the full restore integration test against the most recent
        /// Postgres backup. Creates + drops a scratch DB.
        #[arg(long, default_value_t = false)]
        test_restore: bool,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum TaskCoverageCommand {
    /// Show the current fleet_task_coverage table.
    #[command(alias = "ls")]
    List,
}

#[derive(Debug, Clone, Subcommand)]
pub enum LlmCommand {
    /// Show all running LLM servers fleet-wide.
    Status {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Subcommand)]
enum SoftwareCommand {
    /// List installed software across the fleet.
    List {
        #[arg(long)]
        computer: Option<String>,
        #[arg(long)]
        software: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show software with upgrades available (installed_version != latest_version).
    Drift {
        #[arg(long)]
        json: bool,
    },
    /// Insert or update a row in `software_registry` without editing `config/software.toml`.
    ///
    /// `version-source` and `upgrade-playbook` are JSON strings (stored as JSONB).
    Add {
        /// Software ID (primary key, e.g. "gh", "openclaw", "ff").
        id: String,
        /// Package kind — "apt", "brew", "cargo", "binary", "npm", "pip", …
        #[arg(long)]
        kind: String,
        /// JSON object describing how to detect the installed/latest version.
        #[arg(long = "version-source")]
        version_source: String,
        /// JSON object describing how to install/upgrade the software.
        #[arg(long = "upgrade-playbook")]
        upgrade_playbook: String,
        /// Human-readable name (defaults to `id`).
        #[arg(long = "display-name")]
        display_name: Option<String>,
    },
    /// Delete a row from `software_registry` (cascades through `computer_software`).
    Remove {
        /// Software ID to remove.
        id: String,
        /// Required to actually delete (otherwise prints plan and exits).
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// Manually trigger one auto-upgrade tick (normally runs hourly in the daemon).
    ///
    /// Runs `refresh_self_built_latest_versions` → `flip_self_built_drift_status`
    /// → `resolve_upgrade_plans` → `enqueue_plans` for every software_id with
    /// drift. Useful for operators who just committed a new leader build and
    /// want the fleet to pick it up without waiting up to 60 min.
    ///
    /// Respects the same `fleet_secrets.auto_upgrade_enabled` gate as the
    /// hourly tick — if off, this command no-ops with a warning.
    AutoUpgradeRunOnce {
        /// Force the run even if `auto_upgrade_enabled` is false in fleet_secrets.
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// Probe every `software_registry.version_source` for a newer
    /// upstream version and populate `latest_version` accordingly.
    /// Normally runs every 6h inside the daemon — use this to trigger
    /// an immediate check (e.g. after editing a `version_source`).
    CheckUpstream,
    /// Clear `status='upgrade_blocked'` and reset the failure counter for one row.
    ///
    /// After 3 consecutive auto-upgrade failures, the finalizer flips a
    /// row to `upgrade_blocked` to stop redispatching the same broken
    /// upgrade every hour. Once the root cause is fixed (e.g. sudoers
    /// entry added, disk freed, broken playbook patched), use this to
    /// hand the row back to the auto-upgrade tick.
    Unblock {
        /// Computer name (case-insensitive). E.g. `taylor`.
        computer: String,
        /// Software ID. E.g. `openclaw`, `claude-code`, `ff_git`.
        software_id: String,
    },
}

#[derive(Debug, Clone, Subcommand)]
enum ExtCommand {
    /// List the external-tools catalog (`external_tools` rows).
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        json: bool,
    },
    /// List per-computer install state (`computer_external_tools` rows).
    Installed {
        #[arg(long)]
        computer: Option<String>,
        #[arg(long)]
        tool: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Dispatch an install to one or every online computer.
    Install {
        tool_id: String,
        #[arg(long)]
        computer: Option<String>,
        /// Target every online computer that doesn't have the tool (or whose status is upgrade_available).
        #[arg(long, default_value_t = false)]
        all: bool,
        /// Show planned enqueues without writing to the defer queue.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// Required to actually enqueue (otherwise prints plan and exits).
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// Show computer/tool rows with `status='upgrade_available'`.
    Drift {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Subcommand)]
enum PortsCommand {
    /// List all registered ports. Filter by kind / scope, or emit JSON.
    List {
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Scan a computer to see what's actually listening, and cross-reference
    /// with port_registry. Reports unexpected listeners and missing
    /// expected services.
    Scan { computer: String },
}

#[derive(Debug, Clone, Subcommand)]
enum CloudLlmCommand {
    /// List cloud providers and whether their API-key secret is set.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Prompt for an API key and store it in fleet_secrets under the
    /// provider's configured `secret_key`. The key is read from stdin
    /// (not an argv argument) so it never lands in shell history.
    SetKey {
        provider_id: String,
        /// Override: pass the key on stdin or via this flag (NOT recommended —
        /// leaks into shell history). If omitted, prompts interactively.
        #[arg(long)]
        value: Option<String>,
    },
    /// Show aggregate usage from cloud_llm_usage.
    Usage {
        /// Window like `24h`, `7d`, `1h`. Default: 24h.
        #[arg(long, default_value = "24h")]
        since: String,
    },
    /// Send a trivial chat-completion probe to the provider to verify the
    /// API key + reachability. Picks a reasonable default model per provider.
    Test {
        provider_id: String,
        /// Override the probe model (defaults: openai=gpt-4o-mini,
        /// anthropic=claude-3-5-haiku-latest, moonshot=kimi/moonshot-v1-8k,
        /// google=gemini/gemini-1.5-flash).
        #[arg(long)]
        model: Option<String>,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum SocialCommand {
    /// Ingest a social-media URL. Inserts a `queued` row in
    /// `social_media_posts`, kicks off the fetch→analyze pipeline in a
    /// detached task, and prints the post UUID.
    Ingest {
        /// URL to a TikTok / Instagram / Twitter(X) / YouTube post.
        url: String,
        /// Optional "who asked" label stored on the row.
        #[arg(long)]
        by: Option<String>,
    },
    /// List recent social-media posts (most-recent first).
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        platform: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Show a single post's full row + pretty-printed analysis JSON.
    Show {
        /// Post UUID as printed by `ff social ingest`.
        id: String,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum BrainCommand {
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
pub enum OpenclawCommand {
    /// Show OpenClaw mode across all fleet members (gateway vs node + version).
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Paired-device migration helpers (phone/IoT/browser pairings that
    /// otherwise break on a leader change).
    Devices {
        #[command(subcommand)]
        command: OpenclawDevicesCommand,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum OpenclawDevicesCommand {
    /// Export paired devices from the local OpenClaw gateway to stdout.
    /// Equivalent to `openclaw devices export --format json`, but routed
    /// through the ForgeFleet OpenClawManager so we can also stash the
    /// result into `fleet_secrets.openclaw.device_pairings_export` for
    /// the next leader to pick up.
    Export {
        /// Also write the export into fleet_secrets (same key the
        /// automatic demotion flow uses).
        #[arg(long, default_value_t = false)]
        stash: bool,
    },
    /// Import paired devices into the local OpenClaw gateway. Reads
    /// JSON from stdin (or --from-secret) and pipes into
    /// `openclaw devices import --format json`.
    Import {
        /// Instead of reading stdin, read the stashed secret
        /// `openclaw.device_pairings_export` from fleet_secrets. Clears
        /// the secret after a successful import.
        #[arg(long, default_value_t = false)]
        from_secret: bool,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum OnboardCommand {
    /// Print the copy-paste curl command for onboarding a new computer.
    Show {
        #[arg(long)]
        name: String,
        #[arg(long)]
        ip: Option<String>,
        #[arg(long)]
        ssh_user: Option<String>,
        #[arg(long, default_value = "builder")]
        role: String,
        #[arg(long, default_value = "auto")]
        runtime: String,
    },
    /// List fleet nodes by election_priority (recent onboards appear first).
    #[command(alias = "ls")]
    List {
        #[arg(long, default_value_t = 25)]
        limit: i64,
    },
    /// Revoke a node: delete its fleet_workers row, ssh keys, and mesh rows.
    Revoke {
        name: String,
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum PmCommand {
    /// List work items.
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        assignee: Option<String>,
    },
    /// Create a new work item.
    Create {
        #[arg(long)]
        project: String,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        title: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        priority: Option<String>,
    },
    /// Show details of a work item (by UUID).
    Show { id: String },
    /// Import Claude Code session tasks into projects.work_items.
    ///
    /// Claude Code's TaskCreate/TaskList/TaskUpdate tools keep their state
    /// in the session transcript JSONL. This command parses the most
    /// recent task list embedded in that transcript and UPSERTs each task
    /// as a work_item, so `ff pm list` surfaces them alongside human-
    /// authored items. Closes #104.
    ImportClaudeTasks {
        /// Path to the session JSONL. Defaults to the session matching
        /// `$CLAUDE_SESSION_ID` under the current `pwd`'s project dir.
        #[arg(long)]
        session: Option<PathBuf>,
        /// Project id to attach imported items to.
        #[arg(long, default_value = "forge-fleet")]
        project: String,
        /// Print the plan without writing.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum ProjectCommand {
    /// List known projects.
    #[command(alias = "ls")]
    List,
    /// Show project status (main + environments + branches).
    Status { id: String },
    /// Force a GitHub sync right now.
    Sync {
        #[arg(long, default_value_t = false)]
        all: bool,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum ModelCommand {
    /// V43: launch a model with tensor-parallel-size 2 across a CX-7 pair
    /// (vllm + ray). Records the launch_recipe in llm_clusters for replay.
    ServeTp2 {
        /// Model id (references model_catalog; used as served-model-name).
        model_id: String,
        /// Paired hosts, e.g. `sia+adele` or `rihanna+beyonce`.
        #[arg(long)]
        across: String,
        /// Shared volume name (must exist; run `ff storage share` first).
        #[arg(long = "shared-vault")]
        shared_vault: String,
        /// Port for the OpenAI-compatible API (default 55001).
        #[arg(long, default_value_t = 55001)]
        port: u16,
        /// Path inside the container (default /models/<model_id>).
        #[arg(long = "container-path")]
        container_path: Option<String>,
        #[arg(long = "max-model-len", default_value_t = 32768)]
        max_model_len: u32,
        #[arg(long = "gpu-memory-utilization", default_value_t = 0.85)]
        gpu_memory_utilization: f32,
    },
    /// Sync the curated model catalog TOML into Postgres.
    SyncCatalog,
    /// Search the catalog (fuzzy on id/name/family).
    Search { query: String },
    /// List catalog entries (what can be downloaded).
    Catalog,
    /// List library entries (what's on disk, per node).
    Library {
        #[arg(long)]
        node: Option<String>,
        /// Prepend the library UUID column (needed for `ff model load <id>`).
        #[arg(long)]
        show_id: bool,
    },
    /// List current deployments (what's running, per node).
    Deployments {
        #[arg(long)]
        node: Option<String>,
        /// Prepend the deployment UUID + show library_id/ctx (needed for
        /// `ff model unload <id>` and a faithful `ff model load` reload).
        #[arg(long)]
        show_id: bool,
    },
    /// Pause local model deployments to free RAM for a release build — only if
    /// this host is memory-tight. Snapshots restorable models for resume.
    /// Called by the self-built upgrade wave before `cargo build`; no-op on
    /// roomy hosts.
    FreeForBuild,
    /// Reload models paused by `free-for-build`. Called by the wave after the
    /// build; no-op if nothing was paused.
    ResumeFromBuild,
    /// Scan a node's local models directory and reconcile with fleet_model_library.
    /// Defaults to the current host (taylor) scanning ~/models.
    Scan {
        #[arg(long)]
        node: Option<String>,
        #[arg(long)]
        models_dir: Option<PathBuf>,
    },
    /// Show latest disk usage per node (from fleet_disk_usage snapshots).
    Disk,
    /// List lifecycle jobs (downloads, deletes, loads, swaps).
    Jobs {
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 30)]
        limit: i64,
    },
    /// Download a model from HuggingFace to this node's models dir.
    /// Picks the variant matching this node's runtime (llama.cpp / mlx / vllm).
    Download {
        /// Catalog id (use `ff model search` to find one).
        id: String,
        /// Override runtime (default: this node's runtime from DB).
        #[arg(long)]
        runtime: Option<String>,
        /// Override target node (default: this host).
        #[arg(long)]
        node: Option<String>,
        /// Force re-download even if files already exist.
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// Delete a model from a node's library (removes files from disk).
    Delete {
        /// Library id (UUID from `ff model library`).
        id: String,
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// Load a model: start a local inference server for it on the given port.
    Load {
        /// Library id (UUID from `ff model library`).
        id: String,
        /// Port to bind the inference server on (default: 51001).
        #[arg(long, default_value_t = 51001)]
        port: u16,
        /// Context window tokens (default 65536; per-slot ctx is ctx/parallel).
        #[arg(long)]
        ctx: Option<u32>,
        /// Parallel request slots (default 2 → 32K per slot at default ctx).
        #[arg(long)]
        parallel: Option<u32>,
    },
    /// Enqueue downloads of multiple catalog ids onto a node via the deferred queue.
    DownloadBatch {
        #[arg(long)]
        node: String,
        ids: Vec<String>,
    },
    /// Unload: stop a running inference server by deployment id.
    /// Kills the process ACTUALLY listening on the deployment's port (live
    /// kernel lookup), not the recorded PID — so it works even after an
    /// out-of-band restart.
    Unload {
        /// Deployment id (UUID from `ff model deployments`).
        id: String,
        /// Run the unload on this node instead of the local host (resolves
        /// user@ip from Postgres and SSHes `ff model unload <id>` there).
        #[arg(long)]
        node: Option<String>,
    },
    /// List inference-server processes running on this host.
    Ps,
    /// Sample this node's disk usage and write to fleet_disk_usage.
    DiskSample,
    /// Show full details for a catalog id, library row UUID, or deployment UUID.
    Info { id: String },
    /// Show a smart-LRU eviction plan for a node (dry-run).
    Prune {
        #[arg(long)]
        node: Option<String>,
        /// Min days since last use before a row can be considered cold.
        #[arg(long, default_value_t = 7)]
        min_cold_days: i64,
    },
    /// Health-check a running deployment by id.
    Ping { id: String },
    /// Transfer a model from one node to another (same-runtime, LAN rsync).
    Transfer {
        /// Library UUID on the source node.
        #[arg(long)]
        library_id: String,
        /// Source node name.
        #[arg(long)]
        from: String,
        /// Target node name.
        #[arg(long)]
        to: String,
    },
    /// Show where a model lives on the fleet (catalog_id, partial name, or library UUID).
    Where { id_or_name: String },
    /// List catalog models with newer HuggingFace revisions available (detected by
    /// the daily ModelUpstreamChecker tick). Use `ff model upgrade <id>` to act.
    UpgradeAvailable,
    /// Auto-distribute a model: pick the best destination host based on free disk +
    /// runtime fit + current load, then transfer. Default policy: avoid Taylor (leader),
    /// prefer hosts with most free disk that aren't already holding lots of models.
    Distribute {
        /// Library UUID OR catalog_id. If catalog_id and multiple copies exist,
        /// the most-recently-installed is chosen as source.
        id_or_catalog: String,
        /// Pin the destination host (default: auto-pick).
        #[arg(long)]
        to: Option<String>,
        /// Exclude these hosts (comma-separated). Default: taylor.
        #[arg(long, default_value = "taylor")]
        exclude: String,
        /// Dry run — show the plan without rsync'ing.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Auto-load a catalog model on this node: resolves library row, picks a free
    /// port, calls load_model. No-op if already deployed.
    Autoload {
        /// Catalog id (e.g. "qwen3-coder-30b").
        catalog_id: String,
        /// Override context size (default 32768).
        #[arg(long)]
        ctx: Option<u32>,
    },
    /// Convert a safetensors library entry to MLX on this Apple Silicon host.
    Convert {
        /// Library UUID (must be runtime=vllm i.e. safetensors).
        library_id: String,
        /// Quantization bits (4 or 8).
        #[arg(long, default_value_t = 4)]
        q_bits: u8,
    },
    /// Check HuggingFace for new upstream revisions of catalog models.
    /// Updates `upstream_latest_rev` + flips stale per-computer files
    /// to `revision_available`. Safe to run manually.
    CheckUpstream {
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show fleet task-coverage status: per required task, how many
    /// active deployments serve it and any gaps.
    Coverage {
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Model scout — walk fleet_task_coverage, query HF for the top-N
    /// downloaded models per task, filter by license/size/denylist, and
    /// insert survivors as `lifecycle_status='candidate'`.
    Scout {
        /// Trigger a scout pass right now (otherwise just prints
        /// recently-discovered candidates from the DB).
        #[arg(long, default_value_t = false)]
        run_now: bool,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// List catalog rows with `lifecycle_status='candidate'` awaiting
    /// operator review.
    ReviewCandidates {
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Promote a candidate row to `lifecycle_status='active'`.
    ///
    /// Default behavior: picks a compatible node (GPU first, then CPU) and
    /// runs the benchmark suite. The candidate is only promoted if the
    /// benchmark passes (tokens_per_sec >= 5 AND non-empty response AND
    /// no errors). Pass `--skip-benchmark` (or `--force`) to bypass and
    /// promote immediately.
    Approve {
        /// Catalog id.
        id: String,
        /// Skip the benchmark gate and promote immediately.
        #[arg(long, default_value_t = false)]
        skip_benchmark: bool,
        /// Alias for `--skip-benchmark`.
        #[arg(long, default_value_t = false)]
        force: bool,
        /// Run the benchmark on a specific computer instead of
        /// auto-picking. Ignored when `--skip-benchmark` is set.
        #[arg(long)]
        on_computer: Option<String>,
    },
    /// Reject a candidate row: drops it from the catalog and appends the
    /// upstream_id (if set) to the scout denylist.
    Reject {
        /// Catalog id.
        id: String,
    },
    /// Retire a model: flip `lifecycle_status='retired'` and optionally
    /// record which model supersedes it.
    Retire {
        /// Catalog id.
        id: String,
        /// Optional successor catalog id (populates `replaced_by`).
        #[arg(long)]
        replace_with: Option<String>,
        /// Human-readable retirement reason.
        #[arg(long)]
        reason: String,
    },
    /// Benchmark a model against a standard prompt suite. Writes results
    /// into `model_catalog.benchmark_results` keyed by computer + timestamp.
    Benchmark {
        /// Catalog id of the model to benchmark (e.g. `qwen3-coder`).
        model_id: String,
        /// Target computer (default: current host).
        #[arg(long)]
        computer: Option<String>,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show benchmark history for one model (or recent history across all).
    Benchmarks {
        /// Limit to a specific model catalog id.
        #[arg(long)]
        model: Option<String>,
    },
}

// ─── Phase 12: storage / power / train subcommands ─────────────────────────

#[derive(Debug, Clone, Subcommand)]
pub enum StorageCommand {
    /// Shared NFS volumes.
    Share {
        #[command(subcommand)]
        command: StorageShareCommand,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum StorageShareCommand {
    /// Register a new NFS export (writes DB row + best-effort configures
    /// the host's /etc/exports). The exact mount commands differ by OS —
    /// see module docs on `shared_storage`.
    Create {
        /// Human-readable share name (unique).
        #[arg(long)]
        name: String,
        /// Host computer that exports this path.
        #[arg(long)]
        host: String,
        /// Absolute path on the host that gets exported.
        #[arg(long)]
        path: String,
        /// Default mount path on clients (default: same as --path).
        #[arg(long)]
        mount_path: Option<String>,
        /// Purpose tag: "models" | "training_data" | "outputs" | ...
        #[arg(long)]
        purpose: Option<String>,
        /// Read-only mount.
        #[arg(long, default_value_t = false)]
        read_only: bool,
    },
    /// Mount a named share on a target computer.
    Mount {
        /// Share name.
        name: String,
        /// Target computer.
        #[arg(long)]
        computer: String,
        /// Optional override mount path (defaults to share's mount_path).
        #[arg(long)]
        path: Option<String>,
    },
    /// Unmount a named share on a target computer.
    Unmount {
        /// Share name.
        name: String,
        /// Target computer.
        #[arg(long)]
        computer: String,
    },
    /// List all registered shares and their mount status.
    #[command(alias = "ls")]
    List,
}

#[derive(Debug, Clone, Subcommand)]
pub enum PowerCommand {
    /// Cron-driven sleep / wake / restart schedules.
    Schedule {
        #[command(subcommand)]
        command: PowerScheduleCommand,
    },
    /// List all schedules.
    #[command(alias = "ls")]
    Schedules {
        #[arg(long)]
        computer: Option<String>,
    },
    /// Manually run the scheduler evaluation pass once (dry-fire).
    Tick,
}

#[derive(Debug, Clone, Subcommand)]
pub enum PowerScheduleCommand {
    /// Create a sleep / wake / restart schedule for a computer.
    Create {
        /// Computer name (e.g. "taylor").
        computer: String,
        /// Schedule kind.
        #[arg(value_parser = ["sleep", "wake", "restart"])]
        kind: String,
        /// 5-field cron expression (e.g. "0 0 * * *").
        #[arg(long)]
        cron: String,
        /// Optional condition — v1 supports only `idle_minutes > N`.
        #[arg(long = "if-idle")]
        if_idle: Option<i64>,
    },
    /// Delete a schedule by id.
    Delete { id: String },
}

#[derive(Debug, Clone, Subcommand)]
pub enum TrainCommand {
    /// Create a new training job in `queued` state.
    Create {
        /// Name for the run.
        #[arg(long)]
        name: String,
        /// Base model catalog id (e.g. "qwen3-8b").
        #[arg(long)]
        base: Option<String>,
        /// Training dataset path on the target computer.
        #[arg(long)]
        dataset: String,
        /// Optional output directory for the adapter.
        #[arg(long)]
        output: Option<String>,
        /// Training type.
        #[arg(long, default_value = "lora")]
        training_type: String,
        /// Target computer (where the script runs).
        #[arg(long)]
        computer: Option<String>,
        #[arg(long)]
        epochs: Option<u32>,
        #[arg(long = "lr")]
        learning_rate: Option<f64>,
        #[arg(long = "batch-size")]
        batch_size: Option<u32>,
        #[arg(long = "lora-rank")]
        lora_rank: Option<u32>,
        #[arg(long = "max-seq-len")]
        max_seq_len: Option<u32>,
    },
    /// Start a queued training job (enqueues a deferred_tasks row).
    Start { id: String },
    /// List training jobs.
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 25)]
        limit: i64,
    },
    /// Show details for one training job (by UUID).
    Show { id: String },
}

#[derive(Debug, Clone, Subcommand)]
pub enum AlertCommand {
    /// List all alert policies.
    #[command(alias = "ls")]
    List,
    /// List alert events (fired + resolved). Use --active to filter unresolved.
    Events {
        #[arg(long, default_value_t = false)]
        active: bool,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum MetricsCommand {
    /// Print recent metrics-history rows for a computer.
    History {
        /// Computer name (e.g. "taylor").
        computer: String,
        /// Lookback window (e.g. "5m", "1h", "24h"). Default: 1h.
        #[arg(long, default_value = "1h")]
        since: String,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum SecretsCommand {
    /// List secret keys (values are not printed).
    #[command(alias = "ls")]
    List,
    /// Print a secret value by key (careful — goes to stdout).
    Get { key: String },
    /// Set (or update) a secret.
    Set {
        key: String,
        value: String,
        #[arg(long)]
        description: Option<String>,
    },
    /// Delete a secret by key.
    #[command(alias = "rm")]
    Delete { key: String },
    /// Rotate a secret's value. If --value is given, uses it; otherwise
    /// generates a fresh 32-byte hex value and stores it. Also bumps
    /// rotation_count and extends expires_at by rotate_before_days.
    Rotate {
        key: String,
        #[arg(long)]
        value: Option<String>,
    },
    /// List secrets whose `expires_at` is within `rotate_before_days`.
    Expirations,
    /// Disable a safety-gate fleet_secret with a required TTL and reason.
    /// The kill-switch auto-restores after `--hours` so a forgotten flip
    /// can't silently outlive its purpose (V58 behavior). Use this instead
    /// of `ff secrets set <key> false` for any *_enabled gate.
    ///
    /// Example:
    ///   ff secrets disable-gate auto_upgrade_enabled \
    ///       --hours 6 \
    ///       --reason "wave dispatcher self-kill debug"
    DisableGate {
        /// Secret key (typically a `*_enabled` boolean gate).
        key: String,
        /// How long the disable should last. After this many hours, gate-
        /// check helpers (`pg_read_safety_gate`) auto-restore to the
        /// safe default.
        #[arg(long)]
        hours: u32,
        /// Required free-form reason. Lands in fleet_secrets.disabled_reason
        /// + audit log so a future operator can see why the switch
        ///   was flipped.
        #[arg(long)]
        reason: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum TaskCommand {
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

#[derive(Debug, Clone, Subcommand)]
pub enum EventsCommand {
    /// Subscribe to the NATS fleet event bus and print events as they arrive.
    /// Default subject is `fleet.events.>`; use `--subject` to narrow.
    Tail {
        /// NATS subject filter (supports wildcards `*` and `>`).
        #[arg(long, default_value = "fleet.events.>")]
        subject: String,
        /// Pretty-print JSON payloads instead of one-line compact.
        #[arg(long, default_value_t = false)]
        pretty: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // V43: install the panic capture hook BEFORE anything else, so any
    // panic in our own code gets queued for the next pulse beat to
    // report to the leader's fleet_bug_reports.
    ff_agent::panic_hook::install();

    let cli = Cli::parse();
    let config_path = lifecycle_cmd::resolve_config_path(cli.config)?;

    // Fast-path subcommands that don't need the inference router or any LLM probing.
    // Skips a network round-trip to the fleet + `/v1/models` HTTP fetch.
    match &cli.command {
        Some(Command::Version) => {
            print_ff_version_long();
            return Ok(());
        }
        Some(Command::Secrets { command }) => {
            return secrets_cmd::handle_secrets(command.clone()).await;
        }
        Some(Command::Defer { command }) => return defer_cmd::handle_defer(command.clone()).await,
        Some(Command::Model { command }) => return model_cmd::handle_model(command.clone()).await,
        Some(Command::DeferWorker {
            as_node,
            interval,
            scheduler,
            once,
        }) => {
            return daemon_cmd::handle_defer_worker(as_node.clone(), *interval, *scheduler, *once)
                .await;
        }
        Some(Command::Daemon {
            as_node,
            scheduler,
            defer_interval,
            disk_interval,
            reconcile_interval,
            once,
        }) => {
            return daemon_cmd::handle_daemon(
                as_node.clone(),
                *scheduler,
                *defer_interval,
                *disk_interval,
                *reconcile_interval,
                *once,
            )
            .await;
        }
        Some(Command::Config { command }) => {
            return config_cmd::handle_config(command.clone(), &config_path).await;
        }
        Some(Command::Status) => return status_cmd::handle_status(&config_path).await,
        Some(Command::Nodes { gpu, json }) => {
            return helpers::handle_nodes(gpu.as_deref(), *json).await;
        }
        Some(Command::Versions { node }) => {
            return versions_cmd::handle_versions(node.clone()).await;
        }
        Some(Command::Fleet { command }) => return fleet_cmd::handle_fleet(command.clone()).await,
        Some(Command::Llm { command }) => return llm_cmd::handle_llm(command.clone()).await,
        Some(Command::Software { command }) => {
            return software_cmd::handle_software(command.clone()).await;
        }
        Some(Command::Ext { command }) => return ext_cmd::handle_ext(command.clone()).await,
        Some(Command::Github { command }) => {
            return github_cmd::handle_github(command.clone()).await;
        }
        Some(Command::Mcp { command }) => {
            return mcp_cmd::handle_mcp(command.clone()).await;
        }
        Some(Command::Skills { command }) => {
            return skills_cmd::handle_skills(command.clone()).await;
        }
        Some(Command::Swarm { command }) => {
            return swarm_cmd::handle_swarm(command.clone()).await;
        }
        Some(Command::Onboard { command }) => {
            return onboard_cmd::handle_onboard(command.clone()).await;
        }
        Some(Command::VirtualBrain { command }) => {
            return brain_cmd::handle_brain(command.clone()).await;
        }
        Some(Command::Openclaw { command }) => {
            return openclaw_cmd::handle_openclaw(command.clone()).await;
        }
        Some(Command::Pm { command }) => return pm_cmd::handle_pm(command.clone()).await,
        Some(Command::Agent { command }) => return agent_cmd::handle_agent(command.clone()).await,
        Some(Command::Project { command }) => {
            return project_cmd::handle_project(command.clone()).await;
        }
        Some(Command::Alert { command }) => return alert_cmd::handle_alert(command.clone()).await,
        Some(Command::Metrics { command }) => {
            return metrics_cmd::handle_metrics(command.clone()).await;
        }
        Some(Command::Logs {
            computer,
            service,
            tail,
        }) => {
            return logs_cmd::handle_logs(computer.clone(), service.clone(), *tail).await;
        }
        Some(Command::Events { command }) => {
            return events_cmd::handle_events(command.clone()).await;
        }
        Some(Command::Storage { command }) => {
            return storage_cmd::handle_storage(command.clone()).await;
        }
        Some(Command::Power { command }) => return power_cmd::handle_power(command.clone()).await,
        Some(Command::Train { command }) => return train_cmd::handle_train(command.clone()).await,
        Some(Command::Ports { command }) => return ports_cmd::handle_ports(command.clone()).await,
        Some(Command::CloudLlm { command }) => {
            return cloud_llm_cmd::handle_cloud_llm(command.clone()).await;
        }
        Some(Command::Social { command }) => {
            return social_cmd::handle_social(command.clone()).await;
        }
        _ => {}
    }

    // Build the local-first inference router (probes localhost + fleet from DB).
    // If the user explicitly passed --llm, skip auto-routing and use that URL directly.
    let (llm, router) =
        if let Some(explicit_url) = cli.llm.or_else(|| env::var("FORGEFLEET_LLM_URL").ok()) {
            (explicit_url, None)
        } else {
            let r = ff_agent::inference_router::InferenceRouter::from_config(&config_path).await;
            let primary = if let Some(url) = r.active_url().await {
                url
            } else {
                helpers::detect_llm_from_db_or_local(&config_path).await
            };
            (primary, Some(std::sync::Arc::new(r)))
        };

    let mut model = cli
        .model
        .or_else(|| env::var("FORGEFLEET_MODEL").ok())
        .unwrap_or_else(|| "auto".into());

    // If model is "auto", query the LLM server for its actual model name
    static SHARED_HTTP: std::sync::LazyLock<reqwest::Client> =
        std::sync::LazyLock::new(|| reqwest::Client::new());

    if model == "auto" {
        let detect_url = format!("{}/v1/models", llm.trim_end_matches('/'));
        match SHARED_HTTP.get(&detect_url).send().await {
            Ok(resp) => {
                if let Ok(body) = resp.json::<serde_json::Value>().await
                    && let Some(id) = body
                        .get("data")
                        .and_then(|d| d.as_array())
                        .and_then(|arr| arr.last())
                        .and_then(|m| m.get("id"))
                        .and_then(|id| id.as_str())
                {
                    model = id.to_string();
                }
            }
            Err(_) => {
                if llm.contains("51005") {
                    model = "ForgeFleet-LoRA".into();
                }
            }
        }
    }
    let working_dir = cli
        .cwd
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));

    #[allow(unused_mut)]
    let mut agent_config = AgentSessionConfig {
        model,
        llm_base_url: llm,
        working_dir: working_dir.clone(),
        system_prompt: None,
        max_turns: 30,
        image_path: cli.image,
        inference_router: router,
        ..Default::default()
    };

    match cli.command {
        Some(Command::Start { leader }) => {
            lifecycle_cmd::handle_start(leader, &config_path, &working_dir).await
        }
        Some(Command::Stop) => lifecycle_cmd::handle_stop().await,
        Some(Command::Status) => status_cmd::handle_status(&config_path).await,
        Some(Command::Nodes { gpu, json }) => helpers::handle_nodes(gpu.as_deref(), json).await,
        Some(Command::Models) => lifecycle_cmd::handle_models(&agent_config).await,
        Some(Command::Health) => health_cmd::handle_health(&agent_config).await,
        Some(Command::Proxy { port }) => {
            println!("{CYAN}▶ Starting LLM proxy on 0.0.0.0:{port}{RESET}");
            Ok(())
        }
        Some(Command::Discover { subnet }) => {
            println!("{CYAN}▶ Discovering nodes on {subnet}{RESET}");
            Ok(())
        }
        Some(Command::Config { command }) => config_cmd::handle_config(command, &config_path).await,
        Some(Command::Version) => {
            print_ff_version_long();
            Ok(())
        }
        Some(Command::Run {
            prompt,
            output,
            mode,
            max_turns,
            backend,
            backend_args,
        }) => {
            // Layer-2 backend: spawn a vendor CLI directly (claude /
            // codex / gemini / kimi / grok) instead of the local agent
            // loop. `local` keeps existing behaviour.
            if !backend.eq_ignore_ascii_case("local") {
                let r = ff_agent::cli_executor::execute_cli(&backend, &prompt, &backend_args, None)
                    .await
                    .map_err(|e| anyhow::anyhow!("backend `{backend}`: {e}"))?;
                if !r.stderr.is_empty() {
                    eprintln!("{}", r.stderr);
                }
                println!("{}", r.stdout);
                if r.exit_code != 0 {
                    std::process::exit(r.exit_code as i32);
                }
                return Ok(());
            }
            let mode_norm = mode.to_lowercase();
            if mode_norm != "agent" && mode_norm != "oneshot" {
                eprintln!("{RED}✗ invalid --mode '{mode}' (expected 'agent' or 'oneshot'){RESET}");
                std::process::exit(2);
            }
            let oneshot = mode_norm == "oneshot";
            let mut cfg = agent_config;
            cfg.max_turns = max_turns.unwrap_or(if oneshot { 1 } else { 30 });
            if oneshot {
                // Oneshot: no tool-use loop, larger response budget.
                cfg.max_tokens = 8192;
            }
            // V67 agent hints: inject a "tools available on this machine"
            // section into the system prompt so the agent self-routes to
            // pre-installed software (open-design, etc.) without ff
            // needing a per-tool verb.
            cfg.system_prompt = inject_agent_hints(cfg.system_prompt.clone()).await;
            run_headless(&prompt, cfg, &output, oneshot).await
        }
        Some(Command::Task { command }) => task_cmd::handle_task(command, &config_path).await,
        Some(Command::Secrets { command }) => secrets_cmd::handle_secrets(command).await,
        Some(Command::Defer { command }) => defer_cmd::handle_defer(command).await,
        Some(Command::Model { command }) => model_cmd::handle_model(command).await,
        Some(Command::DeferWorker {
            as_node,
            interval,
            scheduler,
            once,
        }) => daemon_cmd::handle_defer_worker(as_node, interval, scheduler, once).await,
        Some(Command::Daemon {
            as_node,
            scheduler,
            defer_interval,
            disk_interval,
            reconcile_interval,
            once,
        }) => {
            daemon_cmd::handle_daemon(
                as_node,
                scheduler,
                defer_interval,
                disk_interval,
                reconcile_interval,
                once,
            )
            .await
        }
        Some(Command::Versions { node }) => versions_cmd::handle_versions(node).await,
        Some(Command::Fleet { command }) => fleet_cmd::handle_fleet(command).await,
        Some(Command::Llm { command }) => llm_cmd::handle_llm(command).await,
        Some(Command::Software { command }) => software_cmd::handle_software(command).await,
        Some(Command::Ext { command }) => ext_cmd::handle_ext(command).await,
        Some(Command::Github { command }) => github_cmd::handle_github(command).await,
        Some(Command::Mcp { command }) => mcp_cmd::handle_mcp(command).await,
        Some(Command::Skills { command }) => skills_cmd::handle_skills(command).await,
        Some(Command::Swarm { command }) => swarm_cmd::handle_swarm(command).await,
        Some(Command::Onboard { command }) => onboard_cmd::handle_onboard(command).await,
        Some(Command::VirtualBrain { command }) => brain_cmd::handle_brain(command).await,
        Some(Command::Openclaw { command }) => openclaw_cmd::handle_openclaw(command).await,
        Some(Command::Pm { command }) => pm_cmd::handle_pm(command).await,
        Some(Command::Agent { command }) => agent_cmd::handle_agent(command).await,
        Some(Command::Project { command }) => project_cmd::handle_project(command).await,
        Some(Command::Fabric { command }) => {
            let pool = ff_agent::fleet_info::get_fleet_pool()
                .await
                .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
            match command {
                FabricCommand::Pair { a, b, kind } => {
                    fabric_cmd::handle_fabric_pair(&pool, &a, &b, &kind).await
                }
                FabricCommand::Benchmark {
                    a,
                    b,
                    duration,
                    streams,
                    reverse_only,
                } => {
                    fabric_cmd::handle_fabric_benchmark(
                        &pool,
                        &a,
                        &b,
                        duration,
                        streams,
                        reverse_only,
                    )
                    .await
                }
                FabricCommand::Measurements { a, b, limit } => {
                    fabric_cmd::handle_fabric_measurements(&pool, a.as_deref(), b.as_deref(), limit)
                        .await
                }
                FabricCommand::BenchmarkAll { duration, streams } => {
                    fabric_cmd::handle_fabric_benchmark_all(&pool, duration, streams).await
                }
            }
        }
        Some(Command::Tasks { command }) => {
            let pool = ff_agent::fleet_info::get_fleet_pool()
                .await
                .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
            match command {
                TasksCommand::List {
                    computer,
                    status,
                    task_type,
                    show_id,
                } => {
                    tasks_cmd::handle_tasks_list(
                        &pool,
                        computer.as_deref(),
                        status.as_deref(),
                        task_type.as_deref(),
                        show_id,
                    )
                    .await
                }
                TasksCommand::Add {
                    summary,
                    command,
                    capability,
                    preferred,
                    priority,
                    timeout,
                } => {
                    let caps: Vec<String> = capability
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect();
                    let me = ff_agent::fleet_info::resolve_this_worker_name().await;
                    let my_id: Option<uuid::Uuid> =
                        sqlx::query_scalar("SELECT id FROM computers WHERE name = $1")
                            .bind(&me)
                            .fetch_optional(&pool)
                            .await
                            .ok()
                            .flatten();
                    // Use the _full enqueue so `--timeout` can set
                    // fleet_tasks.timeout_secs (V81), which the worker honors
                    // over its ~600s default — needed for agent/research tasks.
                    let id = ff_agent::task_runner::pg_enqueue_shell_task_full(
                        &pool,
                        &summary,
                        &command,
                        &caps,
                        preferred.as_deref(),
                        None,
                        priority,
                        my_id,
                        false,
                        &[],
                        timeout,
                        None,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("enqueue: {e}"))?;
                    println!("{id}");
                    Ok(())
                }
                TasksCommand::Get { id } => {
                    let task_id = uuid::Uuid::parse_str(&id)
                        .map_err(|e| anyhow::anyhow!("invalid uuid: {e}"))?;
                    tasks_cmd::handle_tasks_get(&pool, task_id).await
                }
                TasksCommand::Cancel { id, reason } => {
                    let task_id = uuid::Uuid::parse_str(&id)
                        .map_err(|e| anyhow::anyhow!("invalid uuid: {e}"))?;
                    let prev = ff_agent::task_runner::pg_cancel_task(&pool, task_id, &reason)
                        .await
                        .map_err(|e| anyhow::anyhow!("cancel: {e}"))?;
                    match prev {
                        Some(prev_status) => {
                            println!("{GREEN}✓{RESET} cancelled {task_id} (was {prev_status})");
                        }
                        None => {
                            println!(
                                "{YELLOW}—{RESET} {task_id} already terminal (completed/failed/cancelled); nothing to cancel"
                            );
                        }
                    }
                    Ok(())
                }
                TasksCommand::ComposeNodeBootstrap { target } => {
                    let me = ff_agent::fleet_info::resolve_this_worker_name().await;
                    let my_id: uuid::Uuid =
                        sqlx::query_scalar("SELECT id FROM computers WHERE name = $1")
                            .bind(&me)
                            .fetch_one(&pool)
                            .await?;
                    let parent =
                        ff_agent::task_runner::compose_node_bootstrap(&pool, &target, my_id)
                            .await
                            .map_err(|e| anyhow::anyhow!("compose: {e}"))?;
                    println!("composed parent task: {parent}");
                    println!("watch progress with: ff tasks list --status pending,running");
                    Ok(())
                }
                TasksCommand::ComposeFleetUpgrade {
                    software_id,
                    fanout,
                } => {
                    let me = ff_agent::fleet_info::resolve_this_worker_name().await;
                    let my_id: uuid::Uuid =
                        sqlx::query_scalar("SELECT id FROM computers WHERE name = $1")
                            .bind(&me)
                            .fetch_one(&pool)
                            .await?;
                    let parent = ff_agent::task_runner::compose_fleet_upgrade_wave(
                        &pool,
                        &software_id,
                        fanout,
                        my_id,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("compose: {e}"))?;
                    println!("composed parent task: {parent}");
                    println!("watch progress with: ff tasks list --status pending,running");
                    Ok(())
                }
            }
        }
        Some(Command::Session { command }) => {
            let pool = ff_agent::fleet_info::get_fleet_pool()
                .await
                .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
            ff_db::run_postgres_migrations(&pool)
                .await
                .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;
            match command {
                SessionCommand::Spawn { goal, budget } => {
                    let who = ff_agent::fleet_info::resolve_this_worker_name().await;
                    let id = ff_agent::session_runner::create_session(
                        &pool,
                        &goal,
                        None,
                        budget,
                        Some(&who),
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("create session: {e}"))?;
                    println!("{id}");
                    Ok(())
                }
                SessionCommand::AddStep {
                    session,
                    name,
                    role,
                    prompt,
                    depends_on,
                } => {
                    let session_id = uuid::Uuid::parse_str(&session)
                        .map_err(|e| anyhow::anyhow!("invalid session uuid: {e}"))?;
                    let dep_ids: Vec<uuid::Uuid> = depends_on
                        .iter()
                        .map(|s| uuid::Uuid::parse_str(s))
                        .collect::<Result<_, _>>()
                        .map_err(|e| anyhow::anyhow!("invalid --depends-on: {e}"))?;
                    let id = ff_agent::session_runner::add_step(
                        &pool,
                        session_id,
                        &name,
                        role.as_deref(),
                        &prompt,
                        &dep_ids,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("add step: {e}"))?;
                    println!("{id}");
                    Ok(())
                }
                SessionCommand::List { limit } => {
                    let rows = ff_agent::session_runner::list_sessions(&pool, limit)
                        .await
                        .map_err(|e| anyhow::anyhow!("list: {e}"))?;
                    println!(
                        "{:<36} {:<10} {:<6} {:<6} {:<6} GOAL",
                        "ID", "STATUS", "DONE", "FAIL", "TOTAL"
                    );
                    for r in rows {
                        let id = r.get("id").and_then(|v| v.as_str()).unwrap_or("-");
                        let status = r.get("status").and_then(|v| v.as_str()).unwrap_or("-");
                        let done = r.get("steps_done").and_then(|v| v.as_i64()).unwrap_or(0);
                        let failed = r.get("steps_failed").and_then(|v| v.as_i64()).unwrap_or(0);
                        let total = r.get("steps_total").and_then(|v| v.as_i64()).unwrap_or(0);
                        let goal = r.get("goal").and_then(|v| v.as_str()).unwrap_or("");
                        let goal_short: String = goal.chars().take(60).collect();
                        println!(
                            "{:<36} {:<10} {:<6} {:<6} {:<6} {}",
                            id, status, done, failed, total, goal_short
                        );
                    }
                    Ok(())
                }
                SessionCommand::Get { id } => {
                    let sid = uuid::Uuid::parse_str(&id)
                        .map_err(|e| anyhow::anyhow!("invalid uuid: {e}"))?;
                    let json = ff_agent::session_runner::get_session(&pool, sid)
                        .await
                        .map_err(|e| anyhow::anyhow!("get: {e}"))?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json).unwrap_or_default()
                    );
                    Ok(())
                }
                SessionCommand::BrainGet { session, key } => {
                    let sid = uuid::Uuid::parse_str(&session)
                        .map_err(|e| anyhow::anyhow!("invalid uuid: {e}"))?;
                    match ff_agent::session_runner::brain_get(&pool, sid, &key)
                        .await
                        .map_err(|e| anyhow::anyhow!("brain_get: {e}"))?
                    {
                        Some(v) => {
                            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                            Ok(())
                        }
                        None => {
                            println!("{YELLOW}—{RESET} key '{key}' not found");
                            Ok(())
                        }
                    }
                }
                SessionCommand::BrainSet {
                    session,
                    key,
                    value,
                    role,
                } => {
                    let sid = uuid::Uuid::parse_str(&session)
                        .map_err(|e| anyhow::anyhow!("invalid uuid: {e}"))?;
                    let parsed: serde_json::Value = serde_json::from_str(&value)
                        .unwrap_or_else(|_| serde_json::Value::String(value.clone()));
                    ff_agent::session_runner::brain_set(
                        &pool,
                        sid,
                        &key,
                        &parsed,
                        role.as_deref(),
                        None,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("brain_set: {e}"))?;
                    println!("{GREEN}✓{RESET} stored {key} ({} bytes)", value.len());
                    Ok(())
                }
                SessionCommand::BrainList { session } => {
                    let sid = uuid::Uuid::parse_str(&session)
                        .map_err(|e| anyhow::anyhow!("invalid uuid: {e}"))?;
                    let rows = ff_agent::session_runner::brain_list(&pool, sid)
                        .await
                        .map_err(|e| anyhow::anyhow!("brain_list: {e}"))?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&rows).unwrap_or_default()
                    );
                    Ok(())
                }
                SessionCommand::Plan { session } => {
                    let sid = uuid::Uuid::parse_str(&session)
                        .map_err(|e| anyhow::anyhow!("invalid uuid: {e}"))?;
                    let id = ff_agent::session_runner::add_planner_step(&pool, sid)
                        .await
                        .map_err(|e| anyhow::anyhow!("add planner step: {e}"))?;
                    println!("{GREEN}✓{RESET} planner step created: {id}");
                    println!(
                        "  next: wait for it to complete, then run `ff session apply-plan {session}`"
                    );
                    Ok(())
                }
                SessionCommand::ApplyPlan { session, from_step } => {
                    let sid = uuid::Uuid::parse_str(&session)
                        .map_err(|e| anyhow::anyhow!("invalid uuid: {e}"))?;
                    let from = match from_step {
                        Some(s) => Some(
                            uuid::Uuid::parse_str(&s)
                                .map_err(|e| anyhow::anyhow!("invalid --from-step: {e}"))?,
                        ),
                        None => None,
                    };
                    let ids = ff_agent::session_runner::apply_plan(&pool, sid, from)
                        .await
                        .map_err(|e| anyhow::anyhow!("apply plan: {e}"))?;
                    println!("{GREEN}✓{RESET} inserted {} planned step(s):", ids.len());
                    for id in ids {
                        println!("  {id}");
                    }
                    Ok(())
                }
                SessionCommand::Vote {
                    session,
                    name,
                    prompt,
                    voters,
                    tally_role,
                } => {
                    let sid = uuid::Uuid::parse_str(&session)
                        .map_err(|e| anyhow::anyhow!("invalid uuid: {e}"))?;
                    if voters.is_empty() {
                        return Err(anyhow::anyhow!(
                            "--voters required (comma-separated model names)"
                        ));
                    }
                    let (voter_ids, tally_id) = ff_agent::session_runner::create_vote(
                        &pool,
                        sid,
                        &name,
                        &prompt,
                        &voters,
                        tally_role.as_deref(),
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("create vote: {e}"))?;
                    println!(
                        "{GREEN}✓{RESET} created vote step graph: {} voter(s) + tally",
                        voter_ids.len()
                    );
                    for (i, id) in voter_ids.iter().enumerate() {
                        println!("  voter {i}: {id} ({})", voters[i]);
                    }
                    println!("  tally:    {tally_id}");
                    Ok(())
                }
                SessionCommand::VoteCollect { session, name } => {
                    let sid = uuid::Uuid::parse_str(&session)
                        .map_err(|e| anyhow::anyhow!("invalid uuid: {e}"))?;
                    let snap = ff_agent::session_runner::collect_vote_answers(&pool, sid, &name)
                        .await
                        .map_err(|e| anyhow::anyhow!("collect: {e}"))?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&snap).unwrap_or_default()
                    );
                    Ok(())
                }
                SessionCommand::Cancel { id } => {
                    let sid = uuid::Uuid::parse_str(&id)
                        .map_err(|e| anyhow::anyhow!("invalid uuid: {e}"))?;
                    ff_agent::session_runner::cancel_session(&pool, sid)
                        .await
                        .map_err(|e| anyhow::anyhow!("cancel session: {e}"))?;
                    println!("{GREEN}✓{RESET} session cancelled: {sid}");
                    Ok(())
                }
            }
        }
        Some(Command::Oauth { command }) => {
            let pool = ff_agent::fleet_info::get_fleet_pool()
                .await
                .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
            ff_db::run_postgres_migrations(&pool)
                .await
                .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;
            use ff_agent::oauth_distributor::{
                OAUTH_PROVIDERS, distribute_token, import_token, provider_by_name,
                spawn_refresh_watch, status,
            };
            // Resolve `all` to every catalog entry; otherwise look up the
            // single named provider.
            let resolve = |name: &str| -> anyhow::Result<
                Vec<&'static ff_agent::oauth_distributor::OauthProvider>,
            > {
                if name.eq_ignore_ascii_case("all") {
                    Ok(OAUTH_PROVIDERS.iter().collect())
                } else {
                    Ok(vec![provider_by_name(name).ok_or_else(|| anyhow::anyhow!(
                        "unknown provider {name}; expected one of: claude, codex, gemini, kimi, grok, all"
                    ))?])
                }
            };
            match command {
                OauthCommand::Import { provider } => {
                    for p in resolve(&provider)? {
                        match import_token(&pool, p).await {
                            Ok(()) => println!(
                                "{GREEN}✓{RESET} imported {} → fleet_secrets[{}]",
                                p.name, p.secret_key
                            ),
                            Err(e) => println!("{RED}✗{RESET} {}: {e}", p.name),
                        }
                    }
                    Ok(())
                }
                OauthCommand::Distribute { provider, yes } => {
                    println!(
                        "{YELLOW}!{RESET} TOS reminder: distributing one subscription's OAuth token \
                         to multiple machines is grey-area on most vendor TOS — running one Pro/Plus \
                         account on N concurrent fleet boxes may not be permitted under strict \
                         compliance. Use per-node logins (skip this command) for compliance, OR \
                         continue knowing the risk."
                    );
                    if !yes {
                        // Interactive confirmation. Non-tty callers (cron,
                        // deferred tasks) must pass --yes; we abort on EOF
                        // / non-tty to avoid silent hangs.
                        use std::io::{BufRead, IsTerminal, Write};
                        if !std::io::stdin().is_terminal() {
                            anyhow::bail!(
                                "non-interactive caller must pass --yes to acknowledge the TOS reminder"
                            );
                        }
                        print!("Continue and distribute? (y/N) ");
                        std::io::stdout().flush().ok();
                        let mut line = String::new();
                        std::io::stdin().lock().read_line(&mut line)?;
                        let answer = line.trim().to_ascii_lowercase();
                        if answer != "y" && answer != "yes" {
                            println!(
                                "{YELLOW}✗{RESET} aborted (pass --yes to skip prompt next time)"
                            );
                            return Ok(());
                        }
                    }
                    for p in resolve(&provider)? {
                        match distribute_token(&pool, p).await {
                            Ok(n) => println!(
                                "{GREEN}✓{RESET} {}: enqueued {n} distribute task(s); follow with `ff tasks list`",
                                p.name
                            ),
                            Err(e) => println!("{RED}✗{RESET} {}: {e}", p.name),
                        }
                    }
                    Ok(())
                }
                OauthCommand::Status => {
                    let snap = status(&pool)
                        .await
                        .map_err(|e| anyhow::anyhow!("status: {e}"))?;
                    println!(
                        "{:<10} {:<14} {:<18} {:<10} TOKEN PREVIEW",
                        "PROVIDER", "CRED FILE", "FILE MTIME", "IN SECRETS"
                    );
                    for s in snap {
                        let mtime = s
                            .cred_file_mtime_secs_ago
                            .map(|secs| {
                                if secs < 60 {
                                    format!("{secs}s ago")
                                } else if secs < 3600 {
                                    format!("{}m ago", secs / 60)
                                } else if secs < 86400 {
                                    format!("{}h ago", secs / 3600)
                                } else {
                                    format!("{}d ago", secs / 86400)
                                }
                            })
                            .unwrap_or_else(|| "-".into());
                        println!(
                            "{:<10} {:<14} {:<18} {:<10} {}",
                            s.name,
                            if s.cred_file_present {
                                "present"
                            } else {
                                "missing"
                            },
                            mtime,
                            if s.token_in_secrets { "yes" } else { "no" },
                            s.token_preview.unwrap_or_else(|| "-".into()),
                        );
                    }
                    Ok(())
                }
                OauthCommand::RefreshWatch => {
                    println!(
                        "{CYAN}▶ OAuth refresh-watch{RESET} polling every {}s; ctrl-C to stop",
                        ff_agent::oauth_distributor::REFRESH_POLL_SECS
                    );
                    let (_tx, rx) = tokio::sync::watch::channel(false);
                    let h = spawn_refresh_watch(pool, rx);
                    tokio::signal::ctrl_c().await.ok();
                    println!("{YELLOW}!{RESET} shutdown signal received");
                    drop(h);
                    Ok(())
                }
                OauthCommand::Probe { provider } => {
                    let providers = resolve(&provider)?;
                    println!("{:<10} {:<14} {:<5} detail", "provider", "status", "code");
                    for p in providers {
                        let r = ff_agent::oauth_distributor::probe_one(&pool, p).await;
                        let color = match r.status.as_str() {
                            "ok" => GREEN,
                            "unauthorized" | "forbidden" => RED,
                            _ => YELLOW,
                        };
                        let code = r
                            .http_status
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "-".into());
                        let detail = r.message.unwrap_or_default();
                        println!(
                            "{:<10} {color}{:<14}{RESET} {:<5} {}",
                            r.provider, r.status, code, detail
                        );
                    }
                    Ok(())
                }
                OauthCommand::Refresh { provider } => {
                    let providers = resolve(&provider)?;
                    for p in providers {
                        // Step 1: probe — spawns vendor CLI with a tiny
                        // prompt; that causes the CLI to refresh stale
                        // tokens against its own backend if needed.
                        let r = ff_agent::oauth_distributor::probe_one(&pool, p).await;
                        let probe_color = match r.status.as_str() {
                            "ok" => GREEN,
                            _ => YELLOW,
                        };
                        println!("{:<10} probe → {probe_color}{}{RESET}", p.name, r.status);
                        // Step 2: re-import — re-reads the cred source
                        // (file or Keychain), capturing any token the
                        // CLI just wrote during step 1.
                        match ff_agent::oauth_distributor::import_token(&pool, p).await {
                            Ok(()) => println!(
                                "{:<10} {GREEN}✓{RESET} re-imported to fleet_secrets[{}]",
                                p.name, p.secret_key
                            ),
                            Err(e) => println!("{:<10} {RED}✗{RESET} import: {e}", p.name),
                        }
                    }
                    Ok(())
                }
            }
        }
        Some(Command::SelfHeal { command }) => {
            let pool = ff_agent::fleet_info::get_fleet_pool()
                .await
                .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
            match command {
                SelfHealCommand::Status => self_heal_cmd::handle_status(&pool).await,
                SelfHealCommand::Pause => self_heal_cmd::handle_pause(&pool).await,
                SelfHealCommand::FreezeTier { tier, hours } => {
                    self_heal_cmd::handle_freeze_tier(&pool, &tier, hours).await
                }
                SelfHealCommand::Revert { bug_signature } => {
                    self_heal_cmd::handle_revert(&pool, &bug_signature).await
                }
                SelfHealCommand::TrustReset { computer } => {
                    self_heal_cmd::handle_trust_reset(&pool, &computer).await
                }
                SelfHealCommand::RunWriter { bug_sig } => {
                    self_heal_cmd::handle_run_writer(&pool, &bug_sig).await
                }
            }
        }
        Some(Command::Tools { command }) => {
            let pool = ff_agent::fleet_info::get_fleet_pool()
                .await
                .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
            match command {
                ToolsCommand::List {
                    node,
                    name,
                    unhealthy,
                } => tools_cmd::handle_list(&pool, node, name, unhealthy).await,
                ToolsCommand::Health => tools_cmd::handle_health(&pool).await,
                ToolsCommand::Register { node } => tools_cmd::handle_register(&pool, node).await,
            }
        }
        Some(Command::Stack { command }) => {
            let pool = ff_agent::fleet_info::get_fleet_pool()
                .await
                .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
            match command {
                StackCommand::HostRank {
                    min_ram_gb,
                    exclude,
                    all,
                } => handle_stack_host_rank(&pool, min_ram_gb, &exclude, all).await,
            }
        }
        Some(Command::Alert { command }) => alert_cmd::handle_alert(command).await,
        Some(Command::Metrics { command }) => metrics_cmd::handle_metrics(command).await,
        Some(Command::Logs {
            computer,
            service,
            tail,
        }) => logs_cmd::handle_logs(computer, service, tail).await,
        Some(Command::Events { command }) => events_cmd::handle_events(command).await,
        Some(Command::Storage { command }) => storage_cmd::handle_storage(command).await,
        Some(Command::Power { command }) => power_cmd::handle_power(command).await,
        Some(Command::Train { command }) => train_cmd::handle_train(command).await,
        Some(Command::Ports { command }) => ports_cmd::handle_ports(command).await,
        Some(Command::CloudLlm { command }) => cloud_llm_cmd::handle_cloud_llm(command).await,
        Some(Command::Social { command }) => social_cmd::handle_social(command).await,
        Some(Command::Supervise {
            prompt,
            max_attempts,
            verify_files,
            verify_no_placeholder,
            allowed_tools,
            backend,
            backend_args,
        }) => {
            // Layer-2 supervised: vendor CLI per attempt, ff still owns
            // failure-detect-and-retry. Implementation delegates to
            // cli_executor.rs and stat-checks verify_files between
            // attempts (same logic as the local supervisor uses).
            if !backend.eq_ignore_ascii_case("local") {
                eprintln!(
                    "{CYAN}▶ ForgeFleet Supervisor{RESET} (backend={backend}, {} attempt(s) max)",
                    max_attempts
                );
                let mut last_err = String::new();
                for attempt in 1..=max_attempts {
                    eprintln!("\x1b[2m  attempt {attempt}/{max_attempts}…{RESET}");
                    let r = match ff_agent::cli_executor::execute_cli(
                        &backend,
                        &prompt,
                        &backend_args,
                        None,
                    )
                    .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            last_err = format!("spawn: {e}");
                            continue;
                        }
                    };
                    let mut missing = Vec::new();
                    for p in &verify_files {
                        match tokio::fs::metadata(p).await {
                            Ok(m) if m.is_file() && m.len() > 0 => {}
                            _ => missing.push(p),
                        }
                    }
                    if r.exit_code == 0 && missing.is_empty() {
                        eprintln!(
                            "{GREEN}✓ Task completed on attempt {attempt}/{max_attempts}{RESET}"
                        );
                        if !r.stdout.is_empty() {
                            println!("{}", r.stdout);
                        }
                        return Ok(());
                    }
                    last_err = if r.exit_code != 0 {
                        format!(
                            "non-zero exit {}: {}",
                            r.exit_code,
                            r.stderr.chars().take(400).collect::<String>()
                        )
                    } else {
                        format!(
                            "{} declared deliverable(s) missing/empty: {}",
                            missing.len(),
                            missing
                                .iter()
                                .map(|p| p.display().to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    };
                }
                eprintln!("{RED}✗ Task failed after {max_attempts} attempt(s){RESET}");
                eprintln!("\x1b[2m  last error: {last_err}{RESET}");
                std::process::exit(1);
            }
            let sup_config = ff_agent::supervisor::SupervisorConfig {
                max_attempts,
                verify_files: verify_files.clone(),
                verify_no_placeholder: verify_no_placeholder.clone(),
                ..Default::default()
            };
            if !allowed_tools.is_empty() {
                agent_config.allowed_tools = Some(
                    allowed_tools
                        .iter()
                        .cloned()
                        .collect::<std::collections::HashSet<_>>(),
                );
            }
            let llm_display = agent_config.llm_base_url.trim_end_matches('/').to_string();
            eprintln!(
                "{CYAN}▶ ForgeFleet Supervisor{RESET}  \x1b[2m{llm_display} · model={}{RESET}",
                agent_config.model
            );
            let prompt_preview: String = prompt.chars().take(80).collect();
            eprintln!("\x1b[2m  Task: {}{RESET}", prompt_preview);
            eprintln!("\x1b[2m  Max attempts: {max_attempts}{RESET}");
            if !verify_files.is_empty() {
                eprintln!(
                    "\x1b[2m  Verify files: {}{RESET}",
                    verify_files
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            if !allowed_tools.is_empty() {
                eprintln!(
                    "\x1b[2m  Allowed tools: {}{RESET}",
                    allowed_tools.join(", ")
                );
            }
            eprintln!();

            // V67 agent hints: prepend "tools available" section so the
            // agent decides whether the prompt needs open-design, etc.
            agent_config.system_prompt =
                inject_agent_hints(agent_config.system_prompt.clone()).await;

            let result = ff_agent::supervisor::supervise(&prompt, agent_config, sup_config).await;

            eprintln!();
            if result.success {
                eprintln!(
                    "{GREEN}✓ Task completed on attempt {}/{max_attempts}{RESET}",
                    result.attempts
                );
            } else {
                eprintln!(
                    "{RED}✗ Task failed after {} attempt(s){RESET}",
                    result.attempts
                );
            }

            if !result.diagnoses.is_empty() {
                eprintln!();
                for d in &result.diagnoses {
                    let status = if d.attempt < result.attempts || result.success {
                        "✓"
                    } else {
                        "✗"
                    };
                    eprintln!(
                        "  \x1b[2mAttempt {}: [{status}] {} → {}\x1b[0m",
                        d.attempt, d.failure_type, d.fix_applied
                    );
                }
            }

            eprintln!();
            // Char-safe truncation: byte-slicing panics if the boundary falls
            // inside a multi-byte UTF-8 char (e.g. box-drawing '─' in cargo
            // output). See feedback_ff_supervise_utf8_panic.md.
            let preview: String = result.final_output.chars().take(500).collect();
            println!("{}", preview);
            Ok(())
        }
        Some(Command::Research {
            prompt,
            parallel,
            depth,
            output,
            gateway,
            planner_model,
            subagent_model,
            verbose,
        }) => {
            research_cmd::handle_research(
                &prompt,
                parallel,
                depth,
                output,
                gateway,
                planner_model,
                subagent_model,
                verbose,
            )
            .await
        }
        None => {
            let prompt_text = cli.prompt.join(" ");
            if !prompt_text.is_empty() {
                run_headless(&prompt_text, agent_config, "text", false).await
            } else {
                run_tui(agent_config).await
            }
        }
    }
}

// ─── TUI Mode ──────────────────────────────────────────────────────────────

async fn run_tui(config: AgentSessionConfig) -> Result<()> {
    // Set up panic hook to restore terminal on crash
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        original_hook(info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Mouse capture is OFF by default — prioritizes text selection +
    // clipboard copy which work unconditionally in every terminal when
    // the app isn't grabbing mouse events. Cost: scroll-wheel in TUI
    // panels doesn't work (use arrow / PgUp / PgDn instead).
    //
    // Terminals DO support a "bypass mouse capture" modifier (⌥ on
    // macOS, Shift on Alacritty/WezTerm, Ctrl+Shift on Kitty), but
    // coverage is inconsistent and operators have reported it not
    // working on some setups. Default-off is the safer UX.
    //
    // Set FF_MOUSE_CAPTURE=1 to opt back into mouse-driven scroll + tab
    // clicks, if your terminal honors the bypass modifier cleanly.
    let want_mouse = std::env::var("FF_MOUSE_CAPTURE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if want_mouse {
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    } else {
        execute!(stdout, EnterAlternateScreen)?;
    }
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
    // Only DisableMouseCapture if we actually enabled it at startup. Issuing
    // DisableMouseCapture when capture was never enabled is harmless on most
    // terminals but emits stray escape codes on a few (kitty, some older
    // xterm builds).
    if want_mouse {
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
    } else {
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    }
    terminal.show_cursor()?;
    result
}

/// Check fleet node health on startup.
async fn check_fleet_health(app: &mut App) {
    static SHARED_HTTP: std::sync::LazyLock<reqwest::Client> =
        std::sync::LazyLock::new(|| reqwest::Client::new());
    for node in &mut app.fleet_workers {
        // Check daemon
        let daemon_url = format!(
            "http://{}:{}/health",
            node.ip,
            ff_terminal::app::PORT_DAEMON
        );
        node.daemon_online = SHARED_HTTP
            .get(&daemon_url)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);

        // Check each model endpoint
        for model in &mut node.models {
            let model_url = format!("http://{}:{}/health", node.ip, model.port);
            model.online = SHARED_HTTP
                .get(&model_url)
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
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
    let mut agent_handle: Option<
        tokio::task::JoinHandle<(AgentSession, ff_agent::agent_loop::AgentOutcome)>,
    > = None;
    let mut event_rx: Option<tokio::sync::mpsc::Receiver<AgentEvent>> = None;

    loop {
        // Process agent events if running
        if let Some(rx) = &mut event_rx {
            while let Ok(ev) = rx.try_recv() {
                app.handle_event(ev);
            }
        }

        // Check if agent finished
        if let Some(handle) = &agent_handle
            && handle.is_finished()
        {
            if let Some(handle) = agent_handle.take()
                && let Ok((session, _)) = handle.await
            {
                app.tab_mut().session_id = session.id.to_string();
                app.tab_mut().session = Some(session);
            }
            event_rx = None;
            app.tab_mut().is_running = false;
            app.tab_mut().status = "Ready".into();

            // Auto-send queued message if one was waiting
            if let Some(queued) = app.tab_mut().queued_message.take() {
                let prompt = helpers::detect_dropped_content(&queued);
                // Show user message
                app.tab_mut().input.text = queued;
                app.submit_input();
                // Start agent with queued message
                let mut session = app
                    .tab_mut()
                    .session
                    .take()
                    .unwrap_or_else(|| AgentSession::new(config.clone()));
                let (tx, rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
                let handle = tokio::spawn(async move {
                    let outcome = session.run(&prompt, Some(tx)).await;
                    (session, outcome)
                });
                agent_handle = Some(handle);
                event_rx = Some(rx);
            }
        }

        // Poll any in-flight async picker load
        poll_picker_load(app);

        // Poll async fleet health refresh result (non-blocking).
        poll_fleet_health_refresh(app);

        // Kick off a fleet health refresh every ~30s (20 fps × 30s = 600 frames).
        if app.frame.is_multiple_of(600) && app.frame > 0 {
            kick_fleet_health_refresh(&app.fleet_workers);
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
                        app.tab_mut()
                            .messages
                            .push(ff_terminal::messages::render_status(
                                "Agent cancelled by user",
                            ));
                    }

                    // Ctrl+C: quit (only when not running, otherwise cancel)
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        if app.tab().is_running {
                            if let Some(handle) = agent_handle.take() {
                                handle.abort();
                            }
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
                    (KeyCode::Enter, m)
                        if m.contains(KeyModifiers::SHIFT) || m.contains(KeyModifiers::ALT) =>
                    {
                        app.tab_mut().input.insert_newline();
                    }

                    // Enter: accept suggestion if active, otherwise submit
                    (KeyCode::Enter, KeyModifiers::NONE) => {
                        // If a suggestion is selected, accept it first
                        if app.tab_mut().input.suggestion_index.is_some() {
                            app.tab_mut().input.accept_suggestion();
                            continue;
                        }

                        if app.tab_mut().input.text.trim().is_empty() {
                            continue;
                        }

                        let trimmed = app.tab_mut().input.text.trim().to_string();

                        // ── LOCAL SLASH COMMANDS — intercepted BEFORE queue check ─
                        // Slash commands are local TUI controls; they must NEVER be
                        // forwarded to the LLM, even while the agent is running.
                        if trimmed == "/exit" || trimmed == "/quit" {
                            app.should_quit = true;
                            continue;
                        }
                        if trimmed == "/clear" {
                            // Clear the local chat buffer. If the agent is running,
                            // also interrupt it (same as Esc — avoids deadlock where
                            // the user types /clear while waiting for AskUserQuestion
                            // but nothing ever gets delivered to the agent).
                            if app.tab().is_running {
                                app.tab_mut().cancel_current_agent();
                            }
                            app.tab_mut().messages.clear();
                            app.tab_mut().queued_message = None;
                            app.tab_mut().input.text.clear();
                            app.tab_mut().input.cursor = 0;
                            continue;
                        }
                        if trimmed == "/cancel" || trimmed == "/stop" {
                            if app.tab().is_running {
                                app.tab_mut().cancel_current_agent();
                                app.tab_mut()
                                    .messages
                                    .push(ff_terminal::messages::render_status(
                                        "Agent cancelled by /cancel.",
                                    ));
                            }
                            app.tab_mut().queued_message = None;
                            app.tab_mut().input.text.clear();
                            app.tab_mut().input.cursor = 0;
                            continue;
                        }

                        // If running, queue the message for after the agent finishes
                        if app.tab().is_running {
                            app.tab_mut().queued_message = Some(trimmed.clone());
                            let preview: String = trimmed.chars().take(60).collect();
                            app.tab_mut()
                                .messages
                                .push(ff_terminal::messages::render_status(&format!(
                                    "Queued: \"{}\" — will send when agent finishes.",
                                    preview
                                )));
                            app.tab_mut().input.text.clear();
                            app.tab_mut().input.cursor = 0;
                            continue;
                        }

                        // Built-in navigation commands
                        // Memory search command
                        if trimmed.starts_with("/memory ") || trimmed.starts_with("/search ") {
                            let query = trimmed.split_once(' ').map(|(_, q)| q).unwrap_or("");
                            if !query.is_empty() {
                                let results =
                                    ff_agent::brain::search_all(query, &config.working_dir).await;
                                if results.is_empty() {
                                    app.tab_mut().messages.push(
                                        ff_terminal::messages::render_status(&format!(
                                            "No memory entries match \"{query}\""
                                        )),
                                    );
                                } else {
                                    let mut output = format!(
                                        "Found {} results for \"{}\":\n",
                                        results.len(),
                                        query
                                    );
                                    for r in results.iter().take(10) {
                                        output.push_str(&format!(
                                            "\n[{}] ({}) {}",
                                            r.layer, r.category, r.content
                                        ));
                                    }
                                    app.tab_mut().messages.push(
                                        ff_terminal::messages::render_assistant_message(&output),
                                    );
                                }
                            }
                            app.tab_mut().input.text.clear();
                            app.tab_mut().input.cursor = 0;
                            continue;
                        }

                        if trimmed == "/new" || trimmed == "/new-session" {
                            let n = app.tabs.len() + 1;
                            app.tabs
                                .push(ff_terminal::app::SessionTab::new(&format!("Session {n}")));
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
                            let mut session = app
                                .tab_mut()
                                .session
                                .take()
                                .unwrap_or_else(|| AgentSession::new(config.clone()));
                            if let Some(output) = commands.try_execute(&trimmed, &mut session).await
                            {
                                // Handle Focus Stack / Backlog commands
                                if let Some(topic) = output.strip_prefix("PUSH:") {
                                    app.tab_mut().push_focus(
                                        topic,
                                        "",
                                        ff_agent::focus_stack::PushReason::Explicit,
                                    );
                                    app.tab_mut().messages.push(
                                        ff_terminal::messages::render_status(&format!(
                                            "Pushed to Focus Stack: {topic}"
                                        )),
                                    );
                                } else if output == "POP" {
                                    if let Some(topic) = app.tab_mut().pop_focus() {
                                        app.tab_mut().messages.push(
                                            ff_terminal::messages::render_status(&format!(
                                                "Resumed from Focus Stack: {topic}"
                                            )),
                                        );
                                    } else {
                                        app.tab_mut().messages.push(
                                            ff_terminal::messages::render_status(
                                                "Focus Stack is empty",
                                            ),
                                        );
                                    }
                                } else if let Some(item) = output.strip_prefix("BACKLOG_ADD:") {
                                    app.tab_mut().add_backlog(
                                        item,
                                        "",
                                        ff_agent::focus_stack::BacklogPriority::Medium,
                                    );
                                    app.tab_mut().messages.push(
                                        ff_terminal::messages::render_status(&format!(
                                            "Added to Backlog: {item}"
                                        )),
                                    );
                                } else if output == "BACKLOG_VIEW" {
                                    let items = app.tab().tracker.backlog.items();
                                    if items.is_empty() {
                                        app.tab_mut().messages.push(
                                            ff_terminal::messages::render_status(
                                                "Backlog is empty",
                                            ),
                                        );
                                    } else {
                                        let list: Vec<String> = items
                                            .iter()
                                            .enumerate()
                                            .map(|(i, item)| format!("  {}. {}", i + 1, item.title))
                                            .collect();
                                        app.tab_mut().messages.push(
                                            ff_terminal::messages::render_assistant_message(
                                                &format!("Backlog:\n{}", list.join("\n")),
                                            ),
                                        );
                                    }
                                } else {
                                    app.tab_mut()
                                        .messages
                                        .push(ff_terminal::messages::render_user_message(&trimmed));
                                    app.tab_mut().messages.push(
                                        ff_terminal::messages::render_assistant_message(&output),
                                    );
                                }
                                app.tab_mut().input.submit();
                            }
                            app.tab_mut().session = Some(session);
                            continue;
                        }

                        // Detect dragged file/folder paths and auto-contextualize
                        let prompt = helpers::detect_dropped_content(&trimmed);

                        // Agent run
                        app.submit_input();
                        let mut session = app
                            .tab_mut()
                            .session
                            .take()
                            .unwrap_or_else(|| AgentSession::new(config.clone()));
                        let (tx, rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);

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
                    (KeyCode::Char(c), mods)
                        if !mods.contains(KeyModifiers::CONTROL)
                            && !mods.contains(KeyModifiers::ALT) =>
                    {
                        app.tab_mut().input.insert_char(c);
                        if app.tab_mut().input.text.starts_with('/') {
                            app.tab_mut().input.compute_suggestions(command_list);
                        }
                    }
                    // Tab management
                    (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
                        app.new_tab();
                    }
                    (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                        app.close_tab();
                    }
                    // Ctrl+N/P for tab switching (works on macOS, emacs-style)
                    (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                        app.next_tab();
                    }
                    (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                        app.prev_tab();
                    }

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
                    (KeyCode::Left, m) if m.contains(KeyModifiers::ALT) => {
                        app.tab_mut().input.move_word_left()
                    }
                    (KeyCode::Right, m) if m.contains(KeyModifiers::ALT) => {
                        app.tab_mut().input.move_word_right()
                    }
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
                    (KeyCode::PageUp, _) => {
                        app.tab_mut().auto_scroll = false;
                        app.tab_mut().scroll_offset =
                            app.tab_mut().scroll_offset.saturating_add(10);
                    }
                    (KeyCode::PageDown, _) => {
                        let so = app.tab_mut().scroll_offset;
                        if so > 10 {
                            app.tab_mut().scroll_offset -= 10;
                        } else {
                            app.tab_mut().scroll_offset = 0;
                            app.tab_mut().auto_scroll = true;
                        }
                    }

                    _ => {}
                }
            }
        }

        if app.should_quit {
            break;
        }
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

    if !key.is_empty()
        && let Some(val) = v.get(key).and_then(|v| v.as_str())
    {
        return truncate_str(val, 60).replace('\n', " ");
    }

    // Fallback: first string value in the object
    if let Some(obj) = v.as_object() {
        for (_, val) in obj.iter().take(1) {
            if let Some(s) = val.as_str() {
                return truncate_str(s, 60).replace('\n', " ");
            }
        }
    }

    String::new()
}

// ─── Model Picker overlay ──────────────────────────────────────────────────

/// Open the model picker overlay and kick off async loading of fleet models.
fn open_model_picker(app: &mut ff_terminal::app::App) {
    use ff_terminal::app::ModelPicker;
    app.picker = Some(ModelPicker {
        loading: true,
        ..Default::default()
    });
    // Spawn background load. We poll `app.picker` synchronously, so write results into a shared slot.
    let slot = std::sync::Arc::new(std::sync::Mutex::new(
        None::<Result<Vec<ff_terminal::app::ModelPickerItem>, String>>,
    ));
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
static PICKER_LOAD_SLOT: std::sync::Mutex<
    Option<
        std::sync::Arc<
            std::sync::Mutex<Option<Result<Vec<ff_terminal::app::ModelPickerItem>, String>>>,
        >,
    >,
> = std::sync::Mutex::new(None);

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
            Ok(items) => {
                picker.items = items;
                picker.selected = 0;
            }
            Err(e) => {
                picker.error = Some(e);
            }
        }
    }
}

// ─── Periodic Fleet Health Refresh ─────────────────────────────────────────

/// Result slot for an in-flight health refresh. Keyed only by presence.
static FLEET_HEALTH_SLOT: std::sync::Mutex<
    Option<std::sync::Arc<std::sync::Mutex<Option<Vec<ff_terminal::app::FleetComputer>>>>>,
> = std::sync::Mutex::new(None);

/// Kick off a background task that pings every node + its model endpoints.
/// Idempotent — if one is already in flight, this does nothing.
pub fn kick_fleet_health_refresh(current_nodes: &[ff_terminal::app::FleetComputer]) {
    // Already a refresh in flight? Skip.
    {
        let guard = FLEET_HEALTH_SLOT.lock().unwrap();
        if guard.is_some() {
            return;
        }
    }
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
    *FLEET_HEALTH_SLOT.lock().unwrap() = Some(slot.clone());

    // Snapshot the current node list so the background task can work without sharing &mut.
    let nodes_snapshot = current_nodes.to_vec();

    static SHARED_HTTP: std::sync::LazyLock<reqwest::Client> =
        std::sync::LazyLock::new(|| reqwest::Client::new());

    tokio::spawn(async move {
        let mut refreshed = nodes_snapshot;
        for node in refreshed.iter_mut() {
            let daemon_url = format!(
                "http://{}:{}/health",
                node.ip,
                ff_terminal::app::PORT_DAEMON
            );
            node.daemon_online = SHARED_HTTP
                .get(&daemon_url)
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            for model in node.models.iter_mut() {
                let model_url = format!("http://{}:{}/health", node.ip, model.port);
                model.online = SHARED_HTTP
                    .get(&model_url)
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false);
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
    app.fleet_workers = fresh;
}

async fn load_picker_items() -> Result<Vec<ff_terminal::app::ModelPickerItem>, String> {
    use ff_terminal::app::{ModelPickerItem, PickerItemState};
    use std::collections::BTreeMap;

    // Connect to Postgres using ~/.forgefleet/fleet.toml (same pattern as fleet_nodes_from_db).
    let home = dirs::home_dir().ok_or_else(|| "no home dir".to_string())?;
    let config_path = home.join(".forgefleet/fleet.toml");
    let toml_str = tokio::fs::read_to_string(&config_path)
        .await
        .map_err(|e| format!("read fleet.toml: {e}"))?;
    let config: ff_core::config::FleetConfig =
        toml::from_str(&toml_str).map_err(|e| format!("parse fleet.toml: {e}"))?;
    let db_url = config.database.url.trim().to_string();
    if db_url.is_empty() {
        return Err("database.url is empty in fleet.toml".into());
    }
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
        .iter()
        .map(|n| (n.name.clone(), n.ip.clone()))
        .collect();

    // catalog_id -> CatMeta.
    #[derive(Clone)]
    struct CatMeta {
        name: String,
        tier: i32,
    }
    let cat_meta: std::collections::HashMap<String, CatMeta> = catalog
        .iter()
        .map(|c| {
            (
                c.id.clone(),
                CatMeta {
                    name: c.name.clone(),
                    tier: c.tier,
                },
            )
        })
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
    for c in &catalog {
        aggs.entry(c.id.clone()).or_default();
    }
    for l in &library {
        let a = aggs.entry(l.catalog_id.clone()).or_default();
        if !a.lib_nodes.contains(&l.worker_name) {
            a.lib_nodes.push(l.worker_name.clone());
        }
        a.lib_runtime.get_or_insert_with(|| l.runtime.clone());
        a.lib_size_bytes = a.lib_size_bytes.max(l.size_bytes);
    }
    for d in &deployments {
        let Some(cid) = d.catalog_id.as_ref() else {
            continue;
        };
        let a = aggs.entry(cid.clone()).or_default();
        let healthy = d.health_status == "healthy";
        if a.deploy.is_none() || (healthy && !a.deploy_healthy) {
            let ip = node_ip.get(&d.worker_name).cloned().unwrap_or_default();
            a.deploy = Some((d.worker_name.clone(), ip, d.port, d.runtime.clone()));
            a.deploy_healthy = healthy;
        }
    }
    for j in &jobs {
        if j.kind != "download" {
            continue;
        }
        let Some(cid) = j.target_catalog_id.as_ref() else {
            continue;
        };
        let a = aggs.entry(cid.clone()).or_default();
        if a.job
            .as_ref()
            .map(|(p, _)| j.progress_pct > *p)
            .unwrap_or(true)
        {
            a.job = Some((j.progress_pct, j.status.clone()));
        }
    }

    let mut items: Vec<ModelPickerItem> = Vec::new();
    for (cid, a) in aggs.into_iter() {
        let meta = cat_meta.get(&cid).cloned().unwrap_or(CatMeta {
            name: cid.clone(),
            tier: 0,
        });

        // State precedence: Loaded > Downloading > OnDisk > Catalog.
        let (state, endpoint, endpoint_display, progress_pct, detail, runtime, online) =
            if a.deploy_healthy {
                let (node, ip, port, runtime) = a.deploy.clone().unwrap();
                let endpoint = format!("http://{ip}:{port}");
                let disp = format!("{node} @ {ip}:{port}");
                (
                    PickerItemState::Loaded,
                    endpoint,
                    Some(disp),
                    None,
                    format!("on {node}"),
                    Some(runtime),
                    true,
                )
            } else if a.job.is_some() {
                let (pct, status) = a.job.clone().unwrap();
                let tag = if status == "queued" {
                    "queued"
                } else {
                    "downloading"
                };
                (
                    PickerItemState::Downloading,
                    String::new(),
                    None,
                    Some(pct),
                    format!("{tag} {pct:.0}%"),
                    a.lib_runtime.clone(),
                    false,
                )
            } else if !a.lib_nodes.is_empty() {
                let mut nodes_sorted = a.lib_nodes.clone();
                nodes_sorted.sort();
                let detail = if a.lib_size_bytes > 0 {
                    format!(
                        "on {} ({})",
                        nodes_sorted.join(", "),
                        human_bytes_i64(a.lib_size_bytes)
                    )
                } else {
                    format!("on {}", nodes_sorted.join(", "))
                };
                (
                    PickerItemState::OnDisk,
                    String::new(),
                    None,
                    None,
                    detail,
                    a.lib_runtime.clone(),
                    false,
                )
            } else if a.deploy.is_some() {
                let (node, _ip, _port, runtime) = a.deploy.clone().unwrap();
                (
                    PickerItemState::OnDisk,
                    String::new(),
                    None,
                    None,
                    format!("deploy unhealthy on {node}"),
                    Some(runtime),
                    false,
                )
            } else {
                (
                    PickerItemState::Catalog,
                    String::new(),
                    None,
                    None,
                    "not yet on fleet".into(),
                    None,
                    false,
                )
            };

        let mut nodes_v = a.lib_nodes.clone();
        nodes_v.sort();
        if let Some((n, _, _, _)) = a.deploy.as_ref()
            && !nodes_v.contains(n)
        {
            nodes_v.push(n.clone());
        }

        items.push(ModelPickerItem {
            name: meta.name,
            tier: meta.tier,
            nodes: nodes_v,
            endpoint,
            online,
            state,
            endpoint_display,
            progress_pct,
            detail,
            runtime,
        });
    }

    fn state_rank(s: ff_terminal::app::PickerItemState) -> u8 {
        use ff_terminal::app::PickerItemState::*;
        match s {
            Auto => 0,
            Loaded => 1,
            Downloading => 2,
            OnDisk => 3,
            Catalog => 4,
        }
    }
    items.sort_by(|a, b| {
        state_rank(a.state)
            .cmp(&state_rank(b.state))
            .then(b.tier.cmp(&a.tier))
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    // Build "auto" sentinel at the top.
    let leader_ip = nodes
        .iter()
        .find(|n| n.role == "leader")
        .map(|n| n.ip.clone())
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
/// Handle a key press while the model picker overlay is active.
fn handle_picker_key(app: &mut ff_terminal::app::App, key: crossterm::event::KeyEvent) {
    use crossterm::event::{KeyCode, KeyModifiers};
    let Some(picker) = app.picker.as_mut() else {
        return;
    };
    let visible = picker.visible_indices();
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            app.picker = None;
        }
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
                        app.tab_mut()
                            .messages
                            .push(ff_terminal::messages::render_status(&msg));
                        app.picker = None;
                    }
                    PickerItemState::Downloading => {
                        let msg = format!(
                            "{} is still downloading; wait for it to finish.",
                            chosen.name
                        );
                        app.tab_mut()
                            .messages
                            .push(ff_terminal::messages::render_status(&msg));
                        app.picker = None;
                    }
                    PickerItemState::OnDisk | PickerItemState::Catalog => {
                        let hint = if matches!(chosen.state, PickerItemState::OnDisk) {
                            format!(
                                "Model not loaded; use `ff model load {}` first.",
                                chosen.name
                            )
                        } else {
                            format!(
                                "Model not loaded; use `ff model download {}` and `ff model load {}` first.",
                                chosen.name, chosen.name
                            )
                        };
                        app.tab_mut()
                            .messages
                            .push(ff_terminal::messages::render_status(&hint));
                        app.picker = None;
                    }
                }
            } else {
                app.picker = None;
            }
        }
        (KeyCode::Char(c), mods)
            if !mods.contains(KeyModifiers::CONTROL) && !mods.contains(KeyModifiers::ALT) =>
        {
            picker.filter.push(c);
            picker.selected = 0;
        }
        _ => {}
    }
}

/// Whether to show a result preview for this tool.
fn should_show_result_preview(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "Bash"
            | "WebSearch"
            | "WebFetch"
            | "Orchestrate"
            | "TaskCreate"
            | "TaskList"
            | "TaskGet"
            | "SendMessage"
    )
}

/// V67/V68 helper: prepend two auto-discovered blocks to the agent's
/// system prompt before dispatch.
///
/// 1) **Skill catalog** (V68) — walks `<cwd>/.claude/skills/`,
///    `<cwd>/skills/`, `~/.claude/skills/`, and the fleet-installed
///    `~/.forgefleet/sub-agent-0/open-design/skills/`. Each `SKILL.md`'s
///    YAML frontmatter (name, description, triggers) is summarized into a
///    catalog the agent reads at decision time. The agent picks a skill
///    based on prompt match and uses the Read tool to load the full
///    SKILL.md before following its instructions. Mirrors how Claude
///    Code dynamically loads skills mid-conversation.
///
/// 2) **Agent hints** (V67) — pulls `software_registry.agent_hint` strings
///    for software at `status='ok'` on this host. DB-backed.
///
/// Both are best-effort. DB unreachable / no skills found / no hints
/// configured all return the prompt unchanged.
async fn inject_agent_hints(existing: Option<String>) -> Option<String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));

    // V69 DB-driven scan roots when reachable; legacy hardcoded set as fallback.
    let pool_result = ff_agent::fleet_info::get_fleet_pool().await;
    let skills_block = match &pool_result {
        Ok(pool) => ff_agent::skill_catalog::catalog_for_with_pool(pool, &cwd).await,
        Err(_) => ff_agent::skill_catalog::catalog_for(&cwd),
    };

    // V67 DB-backed agent hints (only when pool is available).
    let hints_block = match &pool_result {
        Ok(pool) => {
            let computer = ff_agent::fleet_info::resolve_this_worker_name().await;
            ff_agent::agent_hint::load_for_host(pool, &computer)
                .await
                .unwrap_or_default()
        }
        Err(_) => String::new(),
    };

    let combined = match (skills_block.is_empty(), hints_block.is_empty()) {
        (true, true) => return existing,
        (false, true) => skills_block,
        (true, false) => hints_block,
        (false, false) => format!("{skills_block}{hints_block}"),
    };
    ff_agent::agent_hint::prepend_to_system_prompt(&combined, existing)
}

async fn run_headless(
    prompt: &str,
    config: AgentSessionConfig,
    output_format: &str,
    oneshot: bool,
) -> Result<()> {
    let is_json = output_format == "json";

    // Print session header
    if !is_json {
        let llm_display = config.llm_base_url.trim_end_matches('/').to_string();
        let mode_label = if oneshot { " · mode=oneshot" } else { "" };
        eprintln!(
            "{CYAN}▶ ForgeFleet Agent{RESET}  \x1b[2m{llm_display} · model={}{mode_label}{RESET}",
            config.model
        );
        eprintln!();
    }

    let mut session = AgentSession::new(config);
    if oneshot {
        // Disable tool registration — the LLM will emit a plain text response
        // rather than calling tools. openai_tools is derived from session.tools
        // in run_agent_loop, so clearing here suppresses tool advertisement.
        session.tools.clear();
    }
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let prompt = prompt.to_string();

    let handle = tokio::spawn(async move { session.run(&prompt, Some(event_tx)).await });

    let mut events = Vec::new();
    while let Some(event) = event_rx.recv().await {
        if is_json {
            events.push(event);
        } else {
            match &event {
                AgentEvent::Status { message, .. } => {
                    eprintln!("\x1b[2m  → {message}\x1b[0m");
                }
                AgentEvent::TurnComplete { turn, .. } => {
                    eprintln!("\x1b[2m── turn {turn} ──────────────────────────────\x1b[0m");
                }
                AgentEvent::ToolStart {
                    tool_name,
                    input_json,
                    ..
                } => {
                    let input_summary = summarize_tool_input(tool_name, input_json);
                    eprint!("{YELLOW}⚡ {tool_name}{RESET}");
                    if !input_summary.is_empty() {
                        eprint!("\x1b[2m({input_summary})\x1b[0m");
                    }
                    eprint!(" ");
                }
                AgentEvent::ToolEnd {
                    tool_name,
                    result,
                    is_error,
                    duration_ms,
                    ..
                } => {
                    if *is_error {
                        eprintln!("{RED}✗ ({duration_ms}ms){RESET}");
                        let first_line = result.lines().next().unwrap_or("").trim();
                        if !first_line.is_empty() {
                            eprintln!("  {RED}{}{RESET}", truncate_str(first_line, 120));
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
                                        eprintln!("  \x1b[2m{}\x1b[0m", truncate_str(trimmed, 120));
                                    }
                                }
                            }
                        }
                    }
                }
                AgentEvent::AssistantText { text, .. } => {
                    print!("{text}");
                }
                AgentEvent::Compaction {
                    messages_before,
                    messages_after,
                    ..
                } => {
                    eprintln!(
                        "\x1b[2m  ⟳ context compacted: {messages_before} → {messages_after} messages\x1b[0m"
                    );
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
    } else if let ff_agent::agent_loop::AgentOutcome::EndTurn { final_message } = &outcome
        && !final_message.is_empty()
    {
        println!("{final_message}");
    }
    Ok(())
}

/// `ff stack host-rank` — rank fleet hosts for a docker/long-running workload.
///
/// Policy (encoded so we don't have to remember it every time):
///   - Skip Taylor (leader; used daily for hands-on work)
///   - Skip DGX hosts (os_family='linux-dgx'; reserved for training)
///   - Require host has total_ram_gb >= min_ram_gb
///   - Rank remaining by total_ram_gb DESC then existing-load ASC
///     (proxy for "free RAM" since we don't capture free_ram_gb yet)
///
/// Matches the rule used by `ff model distribute`: same reserved set, same
/// don't-pile-on heuristic. Use `--all` to see the full ranked list.
async fn handle_stack_host_rank(
    pool: &sqlx::PgPool,
    min_ram_gb: i64,
    exclude_csv: &str,
    show_all: bool,
) -> anyhow::Result<()> {
    let mut excludes: std::collections::HashSet<String> = exclude_csv
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    excludes.insert("taylor".to_string());

    let rows: Vec<(String, String, Option<String>, Option<i32>, i64)> = sqlx::query_as(
        r#"
        SELECT c.name,
               c.os_family,
               c.gpu_kind,
               c.total_ram_gb,
               COALESCE(d.cnt, 0) AS llm_load
          FROM computers c
          LEFT JOIN (
              SELECT worker_name, count(*)::bigint AS cnt
                FROM fleet_model_deployments
               WHERE desired_state = 'active'
               GROUP BY worker_name
          ) d ON d.worker_name = c.name
         WHERE c.os_family <> 'linux-dgx'
           AND COALESCE(c.total_ram_gb, 0) >= $1
         ORDER BY c.total_ram_gb DESC NULLS LAST,
                  COALESCE(d.cnt, 0) ASC
        "#,
    )
    .bind(min_ram_gb as i32)
    .fetch_all(pool)
    .await?;

    let filtered: Vec<&(String, String, Option<String>, Option<i32>, i64)> = rows
        .iter()
        .filter(|(name, _, _, _, _)| !excludes.contains(name))
        .collect();

    if filtered.is_empty() {
        anyhow::bail!(
            "no eligible host: need {} GB RAM, not Taylor, not DGX, not in excludes={:?}",
            min_ram_gb,
            excludes
        );
    }

    if !show_all {
        let pick = filtered[0];
        println!(
            "{CYAN}pick{RESET}      {} ({} GB RAM, {} class, {} LLMs)",
            pick.0,
            pick.3.unwrap_or(0),
            class_label(&pick.1, pick.2.as_deref()),
            pick.4
        );
        println!(
            "Reserved (skipped): {}",
            excludes.iter().cloned().collect::<Vec<_>>().join(", ")
        );
        return Ok(());
    }

    println!(
        "{CYAN}{:<10} {:<6} {:<18} {:<6} {}{RESET}",
        "HOST", "RAM_GB", "CLASS", "LLMS", "STATUS"
    );
    for (i, (name, os_family, gpu_kind, ram, load)) in filtered.iter().enumerate() {
        let marker = if i == 0 { "← pick" } else { "" };
        println!(
            "{:<10} {:<6} {:<18} {:<6} {}",
            name,
            ram.unwrap_or(0),
            class_label(os_family, gpu_kind.as_deref()),
            load,
            marker
        );
    }
    println!();
    println!(
        "Reserved (skipped): {}",
        excludes.iter().cloned().collect::<Vec<_>>().join(", ")
    );
    Ok(())
}

fn class_label(os_family: &str, gpu_kind: Option<&str>) -> &'static str {
    match (os_family, gpu_kind) {
        ("linux-dgx", _) => "DGX (training)",
        ("macos", _) => "macOS",
        (_, Some("amd_rocm")) => "AMD GMKtec",
        (_, Some("nvidia_cuda")) => "NVIDIA non-DGX",
        _ => "bare linux",
    }
}

// ─── Phase 10: alerts / metrics / logs ─────────────────────────────────
