#![allow(clippy::type_complexity)]
#![allow(clippy::too_many_arguments)]

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
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde::{Deserialize, Serialize};

use ff_agent::agent_loop::{AgentEvent, AgentSession, AgentSessionConfig};
use ff_agent::commands::CommandRegistry;
use ff_terminal::app::App;
use ff_terminal::render;

// V43/V44: multi-host deployment + self-heal + fleet-tasks CLI modules.
// Wired here as mod decls; Command enum integration lives in the separate
// V131/V132 PRs so this commit only delivers the handlers.
mod agent_cmd;
mod brain_cmd;
mod cloud_llm_cmd;
mod defer_cmd;
mod events_cmd;
mod ext_cmd;
mod fabric_cmd;
mod fleet_cmd;
mod health_cmd;
mod llm_cmd;
mod logs_cmd;
mod metrics_cmd;
mod model_cmd;
mod model_serve_cmd;
mod openclaw_cmd;
mod ports_cmd;
mod secrets_cmd;
mod self_heal_cmd;
mod social_cmd;
mod software_cmd;
mod storage_cmd;
mod tasks_cmd;
mod tools_cmd;
mod versions_cmd;
mod utils;

pub use utils::{
    CYAN, GREEN, RED, RESET, YELLOW, expand_tilde, human_bytes, human_bytes_i64, parse_duration_secs,
    pulse_reader, resolve_pulse_redis_url, shell_escape_single, trunc_for_status, truncate_for_col,
    truncate_str, whoami_tag,
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
    Nodes,
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
        #[arg(long, default_value_t = 3)]
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
    /// Tail fleet logs via NATS. Requires FORGEFLEET_NATS_URL (default nats://127.0.0.1:4222).
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
        /// When unset, the step uses the default LLM (qwen2.5-coder-32b).
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
    /// Deletes every DB row tied to the computer (fleet_nodes + computers
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
        /// Must match `hostname` / `fleet_nodes.name`.
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
enum OnboardCommand {
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
    /// Revoke a node: delete its fleet_nodes row, ssh keys, and mesh rows.
    Revoke {
        name: String,
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
}

#[derive(Debug, Clone, Subcommand)]
enum PmCommand {
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
enum ProjectCommand {
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
    },
    /// List current deployments (what's running, per node).
    Deployments {
        #[arg(long)]
        node: Option<String>,
    },
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
        /// Context window tokens (default 32768).
        #[arg(long)]
        ctx: Option<u32>,
        /// Parallel request slots (default 4).
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
        /// Catalog id of the model to benchmark (e.g. `qwen2.5-coder`).
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
enum StorageCommand {
    /// Shared NFS volumes.
    Share {
        #[command(subcommand)]
        command: StorageShareCommand,
    },
}

#[derive(Debug, Clone, Subcommand)]
enum StorageShareCommand {
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
enum PowerCommand {
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
enum PowerScheduleCommand {
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
enum TrainCommand {
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
enum AlertCommand {
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
    let config_path = resolve_config_path(cli.config)?;

    // Fast-path subcommands that don't need the inference router or any LLM probing.
    // Skips a network round-trip to the fleet + `/v1/models` HTTP fetch.
    match &cli.command {
        Some(Command::Version) => {
            print_ff_version_long();
            return Ok(());
        }
        Some(Command::Secrets { command }) => return secrets_cmd::handle_secrets(command.clone()).await,
        Some(Command::Defer { command }) => return defer_cmd::handle_defer(command.clone()).await,
        Some(Command::Model { command }) => return model_cmd::handle_model(command.clone()).await,
        Some(Command::DeferWorker {
            as_node,
            interval,
            scheduler,
            once,
        }) => {
            return handle_defer_worker(as_node.clone(), *interval, *scheduler, *once).await;
        }
        Some(Command::Daemon {
            as_node,
            scheduler,
            defer_interval,
            disk_interval,
            reconcile_interval,
            once,
        }) => {
            return handle_daemon(
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
            return handle_config(command.clone(), &config_path).await;
        }
        Some(Command::Status) => return handle_status(&config_path).await,
        Some(Command::Nodes) => return handle_nodes(&config_path),
        Some(Command::Versions { node }) => return versions_cmd::handle_versions(node.clone()).await,
        Some(Command::Fleet { command }) => return handle_fleet(command.clone()).await,
        Some(Command::Llm { command }) => return llm_cmd::handle_llm(command.clone()).await,
        Some(Command::Software { command }) => return handle_software(command.clone()).await,
        Some(Command::Ext { command }) => return handle_ext(command.clone()).await,
        Some(Command::Onboard { command }) => return handle_onboard(command.clone()).await,
        Some(Command::VirtualBrain { command }) => return brain_cmd::handle_brain(command.clone()).await,
        Some(Command::Openclaw { command }) => return openclaw_cmd::handle_openclaw(command.clone()).await,
        Some(Command::Pm { command }) => return handle_pm(command.clone()).await,
        Some(Command::Agent { command }) => return handle_agent(command.clone()).await,
        Some(Command::Project { command }) => {
            return handle_project(command.clone()).await;
        }
        Some(Command::Alert { command }) => return handle_alert(command.clone()).await,
        Some(Command::Metrics { command }) => return metrics_cmd::handle_metrics(command.clone()).await,
        Some(Command::Logs {
            computer,
            service,
            tail,
        }) => {
            return logs_cmd::handle_logs(computer.clone(), service.clone(), *tail).await;
        }
        Some(Command::Events { command }) => return events_cmd::handle_events(command.clone()).await,
        Some(Command::Storage { command }) => return handle_storage(command.clone()).await,
        Some(Command::Power { command }) => return handle_power(command.clone()).await,
        Some(Command::Train { command }) => return handle_train(command.clone()).await,
        Some(Command::Ports { command }) => return handle_ports(command.clone()).await,
        Some(Command::CloudLlm { command }) => return handle_cloud_llm(command.clone()).await,
        Some(Command::Social { command }) => return social_cmd::handle_social(command.clone()).await,
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
                detect_llm_from_db_or_local(&config_path).await
            };
            (primary, Some(std::sync::Arc::new(r)))
        };

    let mut model = cli
        .model
        .or_else(|| env::var("FORGEFLEET_MODEL").ok())
        .unwrap_or_else(|| "auto".into());

    // If model is "auto", query the LLM server for its actual model name
    if model == "auto" {
        let detect_url = format!("{}/v1/models", llm.trim_end_matches('/'));
        match reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default()
            .get(&detect_url)
            .send()
            .await
        {
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
        Some(Command::Start { leader }) => handle_start(leader, &config_path, &working_dir).await,
        Some(Command::Stop) => handle_stop().await,
        Some(Command::Status) => handle_status(&config_path).await,
        Some(Command::Nodes) => handle_nodes(&config_path),
        Some(Command::Models) => handle_models(&agent_config).await,
        Some(Command::Health) => health_cmd::handle_health(&agent_config).await,
        Some(Command::Proxy { port }) => {
            println!("{CYAN}▶ Starting LLM proxy on 0.0.0.0:{port}{RESET}");
            Ok(())
        }
        Some(Command::Discover { subnet }) => {
            println!("{CYAN}▶ Discovering nodes on {subnet}{RESET}");
            Ok(())
        }
        Some(Command::Config { command }) => handle_config(command, &config_path).await,
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
        Some(Command::Task { command }) => handle_task(command, &config_path).await,
        Some(Command::Secrets { command }) => secrets_cmd::handle_secrets(command).await,
        Some(Command::Defer { command }) => defer_cmd::handle_defer(command).await,
        Some(Command::Model { command }) => model_cmd::handle_model(command).await,
        Some(Command::DeferWorker {
            as_node,
            interval,
            scheduler,
            once,
        }) => handle_defer_worker(as_node, interval, scheduler, once).await,
        Some(Command::Daemon {
            as_node,
            scheduler,
            defer_interval,
            disk_interval,
            reconcile_interval,
            once,
        }) => {
            handle_daemon(
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
        Some(Command::Fleet { command }) => handle_fleet(command).await,
        Some(Command::Llm { command }) => llm_cmd::handle_llm(command).await,
        Some(Command::Software { command }) => handle_software(command).await,
        Some(Command::Ext { command }) => handle_ext(command).await,
        Some(Command::Onboard { command }) => handle_onboard(command).await,
        Some(Command::VirtualBrain { command }) => brain_cmd::handle_brain(command).await,
        Some(Command::Openclaw { command }) => openclaw_cmd::handle_openclaw(command).await,
        Some(Command::Pm { command }) => handle_pm(command).await,
        Some(Command::Agent { command }) => handle_agent(command).await,
        Some(Command::Project { command }) => handle_project(command).await,
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
                } => {
                    let caps: Vec<String> = capability
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect();
                    let me = ff_agent::fleet_info::resolve_this_node_name().await;
                    let my_id: Option<uuid::Uuid> =
                        sqlx::query_scalar("SELECT id FROM computers WHERE name = $1")
                            .bind(&me)
                            .fetch_optional(&pool)
                            .await
                            .ok()
                            .flatten();
                    let id = ff_agent::task_runner::pg_enqueue_shell_task(
                        &pool,
                        &summary,
                        &command,
                        &caps,
                        preferred.as_deref(),
                        None,
                        priority,
                        my_id,
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
                    let me = ff_agent::fleet_info::resolve_this_node_name().await;
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
                    let me = ff_agent::fleet_info::resolve_this_node_name().await;
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
                    let who = ff_agent::fleet_info::resolve_this_node_name().await;
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
        Some(Command::Alert { command }) => handle_alert(command).await,
        Some(Command::Metrics { command }) => metrics_cmd::handle_metrics(command).await,
        Some(Command::Logs {
            computer,
            service,
            tail,
        }) => logs_cmd::handle_logs(computer, service, tail).await,
        Some(Command::Events { command }) => events_cmd::handle_events(command).await,
        Some(Command::Storage { command }) => handle_storage(command).await,
        Some(Command::Power { command }) => handle_power(command).await,
        Some(Command::Train { command }) => handle_train(command).await,
        Some(Command::Ports { command }) => handle_ports(command).await,
        Some(Command::CloudLlm { command }) => handle_cloud_llm(command).await,
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
            handle_research(
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
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap_or_default();
    for node in &mut app.fleet_nodes {
        // Check daemon
        let daemon_url = format!(
            "http://{}:{}/health",
            node.ip,
            ff_terminal::app::PORT_DAEMON
        );
        node.daemon_online = client
            .get(&daemon_url)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);

        // Check each model endpoint
        for model in &mut node.models {
            let model_url = format!("http://{}:{}/health", node.ip, model.port);
            model.online = client
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
    let mut event_rx: Option<tokio::sync::mpsc::UnboundedReceiver<AgentEvent>> = None;

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
                let prompt = detect_dropped_content(&queued);
                // Show user message
                app.tab_mut().input.text = queued;
                app.submit_input();
                // Start agent with queued message
                let mut session = app
                    .tab_mut()
                    .session
                    .take()
                    .unwrap_or_else(|| AgentSession::new(config.clone()));
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
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
                        let prompt = detect_dropped_content(&trimmed);

                        // Agent run
                        app.submit_input();
                        let mut session = app
                            .tab_mut()
                            .session
                            .take()
                            .unwrap_or_else(|| AgentSession::new(config.clone()));
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
    Option<std::sync::Arc<std::sync::Mutex<Option<Vec<ff_terminal::app::FleetNode>>>>>,
> = std::sync::Mutex::new(None);

/// Kick off a background task that pings every node + its model endpoints.
/// Idempotent — if one is already in flight, this does nothing.
pub fn kick_fleet_health_refresh(current_nodes: &[ff_terminal::app::FleetNode]) {
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

    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap_or_default();
        let mut refreshed = nodes_snapshot;
        for node in refreshed.iter_mut() {
            let daemon_url = format!(
                "http://{}:{}/health",
                node.ip,
                ff_terminal::app::PORT_DAEMON
            );
            node.daemon_online = client
                .get(&daemon_url)
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            for model in node.models.iter_mut() {
                let model_url = format!("http://{}:{}/health", node.ip, model.port);
                model.online = client
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
    app.fleet_nodes = fresh;
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
        if !a.lib_nodes.contains(&l.node_name) {
            a.lib_nodes.push(l.node_name.clone());
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
            let ip = node_ip.get(&d.node_name).cloned().unwrap_or_default();
            a.deploy = Some((d.node_name.clone(), ip, d.port, d.runtime.clone()));
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
            let computer = ff_agent::fleet_info::resolve_this_node_name().await;
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
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
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

async fn handle_stop() -> Result<()> {
    println!("{CYAN}▶ Stopping ForgeFleet{RESET}");

    // Kill forgefleetd
    let kill = tokio::process::Command::new("pkill")
        .args(["-f", "forgefleetd"])
        .output()
        .await;
    match kill {
        Ok(o) if o.status.success() => println!("  {GREEN}✓ Daemon stopped{RESET}"),
        _ => println!("  {YELLOW}⚠ No daemon process found{RESET}"),
    }

    // Verify
    tokio::time::sleep(Duration::from_secs(1)).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()?;
    let still_running = client
        .get(format!(
            "http://127.0.0.1:{}/health",
            ff_terminal::app::PORT_DAEMON
        ))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);

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
        format!(
            "I've dropped a folder: {trimmed}\nPlease explore this directory and tell me what's in it. Use Glob and Read to understand the contents."
        )
    } else {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        match ext.as_str() {
            // Images
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" => {
                format!(
                    "I've dropped an image: {trimmed}\nPlease analyze this image using PhotoAnalysis with file_path=\"{trimmed}\""
                )
            }
            // Videos
            "mp4" | "mov" | "avi" | "mkv" | "webm" => {
                format!(
                    "I've dropped a video: {trimmed}\nPlease analyze this video using VideoAnalysis with file_path=\"{trimmed}\" action=\"info\""
                )
            }
            // Audio
            "mp3" | "wav" | "flac" | "m4a" | "ogg" => {
                format!(
                    "I've dropped an audio file: {trimmed}\nPlease analyze using AudioAnalysis with file_path=\"{trimmed}\" action=\"info\""
                )
            }
            // PDFs
            "pdf" => {
                format!(
                    "I've dropped a PDF: {trimmed}\nPlease extract and summarize the content using PdfExtract with file_path=\"{trimmed}\""
                )
            }
            // Spreadsheets
            "csv" | "xlsx" | "xls" => {
                format!(
                    "I've dropped a spreadsheet: {trimmed}\nPlease read and summarize using SpreadsheetQuery with file_path=\"{trimmed}\" action=\"head\""
                )
            }
            // Code/text files — just read them
            _ => {
                format!(
                    "I've dropped a file: {trimmed}\nPlease read and analyze this file using Read with file_path=\"{trimmed}\""
                )
            }
        }
    }
}

/// Detect the best LLM endpoint by querying Postgres for fleet nodes + models,
/// then probing each for a healthy connection. Falls back to localhost:55000.
async fn detect_llm_from_db_or_local(config_path: &std::path::Path) -> String {
    // Try to load fleet.toml to get the database URL
    if let Ok(toml_str) = tokio::fs::read_to_string(config_path).await
        && let Ok(config) = toml::from_str::<ff_core::config::FleetConfig>(&toml_str)
    {
        let db_url = config.database.url.trim();
        if !db_url.is_empty() {
            // Query Postgres for fleet nodes and their model ports
            if let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(Duration::from_secs(3))
                .connect(db_url)
                .await
                && let Ok(nodes) = ff_db::pg_list_nodes(&pool).await
            {
                // Also get models to find ports
                let models = ff_db::pg_list_models(&pool).await.unwrap_or_default();

                // Build (ip, port, cores, supports_tools) pairs
                // Prefer models that support tool calling (Qwen) over those that don't (Gemma)
                let mut endpoints: Vec<(String, u16, i32, bool)> = Vec::new();
                for node in &nodes {
                    let node_models: Vec<_> =
                        models.iter().filter(|m| m.node_name == node.name).collect();
                    if node_models.is_empty() {
                        endpoints.push((node.ip.clone(), 55000, node.cpu_cores, true));
                    } else {
                        for m in node_models {
                            // Qwen and Gemma-4 (via MLX) both support OpenAI tool calling.
                            // Check id/slug/name for "gemma-4" or "gemma4" to distinguish from older Gemma variants.
                            let fam = m.family.to_lowercase();
                            let id_lower = m.id.to_lowercase();
                            let name_lower = m.name.to_lowercase();
                            let is_gemma4 = (id_lower.contains("gemma-4")
                                || id_lower.contains("gemma4")
                                || name_lower.contains("gemma-4")
                                || name_lower.contains("gemma4"))
                                && fam.contains("gemma");
                            let supports_tools = fam.contains("qwen") || is_gemma4;
                            endpoints.push((
                                node.ip.clone(),
                                m.port as u16,
                                node.cpu_cores,
                                supports_tools,
                            ));
                        }
                    }
                }
                // Sort: tool-calling models first, then by cores descending
                endpoints.sort_by(|a, b| b.3.cmp(&a.3).then(b.2.cmp(&a.2)));

                for (ip, port, _, _) in &endpoints {
                    if let Ok(addr) = format!("{ip}:{port}").parse()
                        && std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200))
                            .is_ok()
                    {
                        tracing::info!(ip = %ip, port, "auto-detected LLM endpoint from database");
                        return format!("http://{ip}:{port}");
                    }
                }
            }
        }
    }

    // Fallback: probe localhost
    for port in [55000, 55001, 11434] {
        if let Ok(addr) = format!("127.0.0.1:{port}").parse()
            && std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok()
        {
            return format!("http://127.0.0.1:{port}");
        }
    }

    "http://localhost:55000".into()
}

fn resolve_config_path(p: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = p {
        return Ok(p);
    }
    Ok(PathBuf::from(env::var("HOME").context("HOME not set")?)
        .join(".forgefleet")
        .join("fleet.toml"))
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

fn load_config(p: &Path) -> Result<FleetConfig> {
    if !p.exists() {
        return Ok(FleetConfig::default());
    }
    Ok(toml::from_str(&fs::read_to_string(p)?)?)
}

async fn handle_start(leader: bool, config_path: &Path, working_dir: &Path) -> Result<()> {
    println!("{CYAN}▶ Starting ForgeFleet{RESET}");
    println!("  Config: {}", config_path.display());
    println!("  Mode:   {}", if leader { "leader" } else { "auto" });
    println!();

    // Check if daemon is already running (check web UI port — only daemon serves this)
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;
    let daemon_running = client
        .get(format!(
            "http://127.0.0.1:{}/health",
            ff_terminal::app::PORT_WEB
        ))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    if daemon_running {
        println!("{GREEN}✓ ForgeFleet daemon is already running{RESET}");
        println!(
            "  Daemon:    http://localhost:{}",
            ff_terminal::app::PORT_DAEMON
        );
        println!(
            "  Web UI:    http://localhost:{}",
            ff_terminal::app::PORT_WEB
        );
        println!("  WebSocket: ws://localhost:{}", ff_terminal::app::PORT_WS);
        return Ok(());
    }

    // Step 1: Find and start LLM server
    println!("{YELLOW}1/4{RESET} Checking LLM server...");
    let llm_running = std::net::TcpStream::connect_timeout(
        &"127.0.0.1:51000".parse().unwrap(),
        Duration::from_millis(500),
    )
    .is_ok();

    if llm_running {
        println!("  {GREEN}✓ LLM server already running on :51000{RESET}");
    } else {
        println!("  {YELLOW}⚠ No LLM server detected locally{RESET}");
        println!("  Start one with: ollama serve & ollama run qwen2.5-coder:32b");
        println!(
            "  Or: llama-server -m /path/to/model.gguf --host 0.0.0.0 --port 51000 --ctx-size 32768"
        );
    }

    // Step 2: Start ForgeFleet daemon
    println!("{YELLOW}2/4{RESET} Starting ForgeFleet daemon...");

    // Find the forgefleetd binary
    let daemon_binary = find_daemon_binary(working_dir);
    match daemon_binary {
        Some(bin) => {
            let mut cmd = tokio::process::Command::new(&bin);
            cmd.arg("--config").arg(config_path);
            if leader {
                cmd.arg("start").arg("--leader");
            }

            // Spawn as background process
            match cmd
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(child) => {
                    println!(
                        "  {GREEN}✓ Daemon started (PID: {}){RESET}",
                        child.id().unwrap_or(0)
                    );

                    // Wait a moment for it to boot
                    tokio::time::sleep(Duration::from_secs(2)).await;

                    // Verify it's running
                    let health = client
                        .get(format!(
                            "http://127.0.0.1:{}/health",
                            ff_terminal::app::PORT_DAEMON
                        ))
                        .send()
                        .await;
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
    let nodes = [
        ("Taylor", "192.168.5.100"),
        ("Marcus", "192.168.5.102"),
        ("Sophie", "192.168.5.103"),
        ("Priya", "192.168.5.104"),
        ("James", "192.168.5.108"),
    ];
    let mut online = 0;
    for (name, ip) in &nodes {
        let ok = client
            .get(format!("http://{ip}:51000/health"))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if ok {
            online += 1;
        }
        let icon = if ok {
            format!("{GREEN}●{RESET}")
        } else {
            format!("{RED}○{RESET}")
        };
        println!("  {icon} {name} ({ip})");
    }

    // Step 4: Summary
    println!("{YELLOW}4/4{RESET} Summary");
    println!();
    println!("  {GREEN}ForgeFleet v{}{RESET}", env!("CARGO_PKG_VERSION"));
    println!("  Fleet: {online}/{} nodes online", nodes.len());
    println!();
    println!(
        "  Daemon:    http://localhost:{}",
        ff_terminal::app::PORT_DAEMON
    );
    println!(
        "  LLM API:   http://localhost:{}",
        ff_terminal::app::PORT_LLM
    );
    println!(
        "  Web UI:    http://localhost:{}",
        ff_terminal::app::PORT_WEB
    );
    println!("  WebSocket: ws://localhost:{}", ff_terminal::app::PORT_WS);
    println!(
        "  Metrics:   http://localhost:{}",
        ff_terminal::app::PORT_METRICS
    );
    println!();
    println!(
        "  Run {CYAN}ff{RESET} for terminal, or open {CYAN}http://localhost:{}{RESET} for web UI",
        ff_terminal::app::PORT_WEB
    );

    Ok(())
}

/// Find the forgefleetd daemon binary.
fn find_daemon_binary(working_dir: &Path) -> Option<PathBuf> {
    // Check common locations
    let candidates = [
        working_dir.join("target/release/forgefleetd"),
        working_dir.join("target/debug/forgefleetd"),
        PathBuf::from("/usr/local/bin/forgefleetd"),
        dirs::home_dir()
            .unwrap_or_default()
            .join(".local/bin/forgefleetd"),
        dirs::home_dir()
            .unwrap_or_default()
            .join(".cargo/bin/forgefleetd"),
    ];

    for path in candidates.iter() {
        if path.exists() {
            return Some(path.to_path_buf());
        }
    }

    // Try which
    if let Ok(output) = std::process::Command::new("which")
        .arg("forgefleetd")
        .output()
        && output.status.success()
    {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
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
    )
    .await;
    let pool_opt: Option<sqlx::PgPool> = match pool_res {
        Ok(Ok(pool)) => {
            // Report the highest applied version number, not a row count.
            // COUNT(*) would return 39 on a V45 DB because Postgres migrations
            // start at V7 (V1-V6 are SQLite-only), giving 39 rows for 45 versions.
            let migs: Option<i64> = sqlx::query_scalar::<_, i64>(
                "SELECT COALESCE(MAX(version),0)::bigint FROM _migrations",
            )
            .fetch_one(&pool)
            .await
            .ok();
            match migs {
                Some(n) => println!("{GREEN}✓ connected{RESET} ({n} migrations applied)"),
                None => println!("{GREEN}✓ connected{RESET} (migrations table missing)"),
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
    let redis_url = fleet_cfg
        .as_ref()
        .map(|c| c.redis.url.clone())
        .unwrap_or_else(|| "redis://127.0.0.1:6380".to_string());
    match ping_redis(&redis_url).await {
        Ok(ms) => println!("{GREEN}✓ PONG{RESET} ({redis_url}, {ms}ms)"),
        Err(e) => println!(
            "{RED}✗ unreachable{RESET} ({redis_url}) — {}",
            truncate(&e, 50)
        ),
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
        let probes: Vec<_> = nodes
            .iter()
            .map(|n| {
                let ip = n.ip.clone();
                async move { tcp_probe(&ip, 22, Duration::from_secs(2)).await }
            })
            .collect();
        let online: Vec<bool> = futures::future::join_all(probes).await;
        for (n, up) in nodes.iter().zip(online.iter()) {
            let status = if *up {
                format!("{GREEN}online{RESET}")
            } else {
                format!("{RED}offline{RESET}")
            };
            println!("  {:<10} {:<16} {:<10} {}", n.name, n.ip, n.runtime, status);
        }
    }

    // ── 4. Deployments ─────────────────────────────────────────────────────
    print!("{CYAN}Deployments{RESET}: ");
    if let Some(pool) = &pool_opt {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT health_status, COUNT(*)::bigint FROM fleet_model_deployments \
             GROUP BY health_status ORDER BY health_status",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        if rows.is_empty() {
            println!("{YELLOW}(none){RESET}");
        } else {
            let parts: Vec<String> = rows
                .iter()
                .map(|(s, c)| {
                    let color = match s.as_str() {
                        "healthy" => GREEN,
                        "unhealthy" => RED,
                        "starting" => YELLOW,
                        _ => RESET,
                    };
                    format!("{color}{s}={c}{RESET}")
                })
                .collect();
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
        let n: Option<i64> = sqlx::query_scalar("SELECT COUNT(*)::bigint FROM fleet_model_catalog")
            .fetch_one(pool)
            .await
            .ok();
        match n {
            Some(n) => println!("{n} entries"),
            None => println!("{RED}✗ query failed{RESET}"),
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
             ORDER BY d.node_name, d.sampled_at DESC",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        if rows.is_empty() {
            println!("  {YELLOW}(no samples yet){RESET}");
        } else {
            for (name, total, used, models, quota) in rows {
                let total_gib = (total as f64) / 1024.0 / 1024.0 / 1024.0;
                let used_gib = (used as f64) / 1024.0 / 1024.0 / 1024.0;
                let models_gib = (models as f64) / 1024.0 / 1024.0 / 1024.0;
                let used_pct = if total > 0 {
                    (used as f64 / total as f64) * 100.0
                } else {
                    0.0
                };
                let over = used_pct >= quota as f64;
                let line = format!(
                    "  {:<10} {:5.1}/{:5.1} GiB ({:4.1}%)  models {:5.1} GiB  quota {}%",
                    name, used_gib, total_gib, used_pct, models_gib, quota
                );
                if over {
                    println!("{RED}{line}{RESET}");
                } else {
                    println!("{line}");
                }
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
             GROUP BY status ORDER BY status",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        if rows.is_empty() {
            println!("{YELLOW}(none){RESET}");
        } else {
            let parts: Vec<String> = rows
                .iter()
                .map(|(s, c)| {
                    if s == "failed" && *c > 0 {
                        format!("{RED}{s}={c}{RESET}")
                    } else {
                        format!("{s}={c}")
                    }
                })
                .collect();
            println!("{}", parts.join("  "));
        }
    } else {
        println!("{RED}✗ unreachable{RESET}");
    }

    // ── 9. In-flight jobs ──────────────────────────────────────────────────
    print!("{CYAN}Jobs{RESET}      : ");
    if let Some(pool) = &pool_opt {
        let n: Option<i64> = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM fleet_model_jobs WHERE status IN ('running','queued')",
        )
        .fetch_one(pool)
        .await
        .ok();
        match n {
            Some(0) => println!("0 in-flight"),
            Some(n) => println!("{YELLOW}{n} in-flight{RESET} (running or queued)"),
            None => println!("{RED}✗ query failed{RESET}"),
        }
    } else {
        println!("{RED}✗ unreachable{RESET}");
    }

    // ── 10. Secrets ───────────────────────────────────────────────────────
    print!("{CYAN}Secrets{RESET}   : ");
    if let Some(pool) = &pool_opt {
        let keys: Vec<(String,)> = sqlx::query_as("SELECT key FROM fleet_secrets ORDER BY key")
            .fetch_all(pool)
            .await
            .unwrap_or_default();
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
    // Char-safe — `&s[..max]` panicked on multi-byte codepoints (box-drawing,
    // emoji, CJK). Delegate to the canonical helper at line 14460 so this
    // never recurs. Both helpers exist for historical-callsite reasons;
    // future code should call `truncate_str` directly.
    truncate_str(s, max)
}

async fn tcp_probe(host: &str, port: u16, timeout: Duration) -> bool {
    let addr = format!("{host}:{port}");
    matches!(
        tokio::time::timeout(timeout, tokio::net::TcpStream::connect(&addr)).await,
        Ok(Ok(_))
    )
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
        // Host-facing default: docker-compose publishes Redis on 6380.
        Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(6380)),
        None => (host_port.to_string(), 6380),
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
    for (n, d) in cfg.nodes {
        println!("  - {n}: {d}");
    }
    Ok(())
}

async fn handle_models(c: &AgentSessionConfig) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let url = format!("{}/v1/models", c.llm_base_url.trim_end_matches('/'));
    match client.get(&url).send().await {
        Ok(r) => println!("{}", r.text().await.unwrap_or_default()),
        Err(e) => println!("{RED}Failed: {e}{RESET}"),
    }
    Ok(())
}






/// Pretty-print a byte size (KiB/MiB/GiB/TiB).
// ─── Deferred task worker ──────────────────────────────────────────────────

/// Probe each fleet node's SSH port (22) to determine reachability. Returns the list of reachable node names.
async fn probe_online_nodes(nodes: &[ff_db::FleetNodeRow]) -> Vec<String> {
    use tokio::net::TcpStream;
    use tokio::time::{Duration as TokDuration, timeout};
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
    if cfg!(target_os = "macos") {
        "macos".into()
    } else if cfg!(target_os = "linux") {
        "linux".into()
    } else {
        "unknown".into()
    }
}

/// Parse shorthand duration specs like "1h", "30m", "2d", "45s".
fn parse_duration(spec: &str) -> Option<chrono::Duration> {
    let spec = spec.trim();
    let (num, unit) = spec.split_at(spec.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = num.parse().ok()?;
    match unit {
        "s" | "sec" => Some(chrono::Duration::seconds(n)),
        "m" | "min" => Some(chrono::Duration::minutes(n)),
        "h" | "hr" => Some(chrono::Duration::hours(n)),
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
                None => {
                    return (
                        false,
                        None,
                        Some("shell payload missing 'command' field".into()),
                    );
                }
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
            let method = task
                .payload
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("GET");
            let body = task.payload.get("body").cloned();
            execute_http(method, url, body).await
        }
        "internal" => {
            // Internal ForgeFleet tasks dispatched by title. Requires DB pool —
            // we open a short-lived one here so execute_deferred stays pure.
            if task.title.starts_with("Mesh propagate SSH for ") {
                match ff_agent::fleet_info::get_fleet_pool().await {
                    Ok(pool) => match ff_agent::mesh_check::mesh_propagate(&pool, &task.payload)
                        .await
                    {
                        Ok((ok, fail)) => {
                            let result = serde_json::json!({"ok_peers": ok, "failed_peers": fail});
                            let success = fail == 0;
                            let err = if success {
                                None
                            } else {
                                Some(format!("{fail} peer(s) failed"))
                            };
                            (success, Some(result), err)
                        }
                        Err(e) => (false, None, Some(format!("mesh_propagate: {e}"))),
                    },
                    Err(e) => (false, None, Some(format!("pool: {e}"))),
                }
            } else {
                (
                    false,
                    None,
                    Some(format!("unknown internal task title: {}", task.title)),
                )
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
                None => {
                    return (
                        false,
                        None,
                        Some(format!("no playbook for tool={tool} os={os_family}")),
                    );
                }
            };
            let target = task.preferred_node.as_deref();
            execute_shell(target, &script, nodes, workspace).await
        }
        "mesh_retry" => {
            // Re-probe a specific (src, dst) pair and refresh fleet_mesh_status.
            let src = task
                .payload
                .get("src")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let dst = task
                .payload
                .get("dst")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if src.is_empty() || dst.is_empty() {
                return (false, None, Some("mesh_retry payload needs src+dst".into()));
            }
            match ff_agent::fleet_info::get_fleet_pool().await {
                Ok(pool) => match ff_agent::mesh_check::probe_single_pair(&pool, src, dst).await {
                    Ok(cell) => {
                        let ok = cell.status == "ok";
                        let result =
                            serde_json::json!({"status": cell.status, "error": cell.last_error});
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

/// Threshold for auto-upgrade `consecutive_failures` → `upgrade_blocked`.
/// Hit this count and the row stops getting auto-retried until an operator
/// clears the block manually. 3 = "transient flake retried twice, third
/// strike means there's a real problem".
const AUTO_UPGRADE_FAILURE_THRESHOLD: i32 = 3;

/// Post-completion hook for `meta.auto_upgrade` deferred tasks.
///
/// Runs whether the task succeeded or failed. Always:
///   1a. On success: writes `installed_version=$latest_version` (authoritative —
///       don't wait for the next beat to refresh it), resets
///       `consecutive_failures=0`, clears `last_upgrade_error`, sets `status='ok'`.
///   1b. On failure: bumps `consecutive_failures` and sets
///       `last_upgrade_error=$err`. If the bumped count reaches
///       `AUTO_UPGRADE_FAILURE_THRESHOLD`, flips `status='upgrade_blocked'`
///       so the next tick won't redispatch; otherwise sets
///       `status='upgrade_available'` for retry.
///   2. Publishes `fleet.events.software.upgrade_completed.{computer}` on NATS.
///   3. Fires a Telegram message via fleet_secrets (no-op if not configured).
async fn finalize_upgrade_event(
    pool: &sqlx::PgPool,
    task: &ff_db::DeferredTaskRow,
    ok: bool,
    meta: &serde_json::Value,
    err: Option<&str>,
) {
    let software_id = meta
        .get("software_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let display_name = meta
        .get("display_name")
        .and_then(|v| v.as_str())
        .unwrap_or(software_id);
    let computer = meta.get("computer").and_then(|v| v.as_str()).unwrap_or("");
    let old_version = meta
        .get("old_version")
        .and_then(|v| v.as_str())
        .unwrap_or("-");
    let latest_version = meta
        .get("latest_version")
        .and_then(|v| v.as_str())
        .unwrap_or("-");

    // 1. Record outcome.
    if ok {
        // Success path — write authoritative installed_version, reset counter.
        // Skip the installed_version update if meta didn't carry a usable
        // latest_version (placeholder "-" or empty); fall back to the next
        // beat's collector-reported version.
        let installed_version_to_write = if latest_version == "-" || latest_version.is_empty() {
            None
        } else {
            Some(latest_version.to_string())
        };
        let _ = sqlx::query(
            "UPDATE computer_software cs
                SET status               = 'ok',
                    installed_version    = COALESCE($3, cs.installed_version),
                    last_upgraded_at     = NOW(),
                    last_checked_at      = NOW(),
                    last_upgrade_error   = NULL,
                    consecutive_failures = 0
               FROM computers c
              WHERE cs.computer_id = c.id
                AND cs.software_id = $1
                AND LOWER(c.name)  = LOWER($2)",
        )
        .bind(software_id)
        .bind(computer)
        .bind(installed_version_to_write)
        .execute(pool)
        .await;
    } else {
        // Failure path — bump counter, flip to upgrade_blocked at threshold.
        // Only triggers when status is currently 'upgrading' (i.e. we're
        // finalizing a real dispatched run, not a phantom).
        let truncated_err = err.map(|s| s.chars().take(2000).collect::<String>());
        let _ = sqlx::query(
            "UPDATE computer_software cs
                SET consecutive_failures = cs.consecutive_failures + 1,
                    last_upgrade_error   = $3,
                    last_checked_at      = NOW(),
                    status = CASE
                        WHEN cs.consecutive_failures + 1 >= $4
                        THEN 'upgrade_blocked'
                        ELSE 'upgrade_available'
                    END
               FROM computers c
              WHERE cs.computer_id = c.id
                AND cs.software_id = $1
                AND LOWER(c.name)  = LOWER($2)
                AND cs.status      = 'upgrading'",
        )
        .bind(software_id)
        .bind(computer)
        .bind(truncated_err)
        .bind(AUTO_UPGRADE_FAILURE_THRESHOLD)
        .execute(pool)
        .await;
    }

    // 2. NATS event — everyone subscribed to fleet.events.software.> sees it.
    let status_word = if ok { "success" } else { "failed" };
    let subject = format!(
        "fleet.events.software.upgrade_completed.{}",
        if computer.is_empty() {
            "unknown"
        } else {
            computer
        },
    );
    let payload = serde_json::json!({
        "software_id":    software_id,
        "display_name":   display_name,
        "computer":       computer,
        "old_version":    old_version,
        "latest_version": latest_version,
        "status":         status_word,
        "error":          err,
        "defer_id":       task.id,
        "ts":             chrono::Utc::now().to_rfc3339(),
    });
    ff_agent::nats_client::publish_json(subject, &payload).await;

    // 3. Telegram (best-effort — never crashes the worker).
    let title = if ok {
        format!("✅ ForgeFleet upgraded {display_name} on {computer}")
    } else {
        format!("❌ ForgeFleet upgrade failed: {display_name} on {computer}")
    };
    let body = if ok {
        format!("{old_version} → {latest_version}\nNo operator action needed.",)
    } else {
        // Read the post-update consecutive_failures count so the message
        // tells the operator whether more retries are coming or the row
        // just got blocked.
        let count: i32 = sqlx::query_scalar::<_, i32>(
            "SELECT cs.consecutive_failures
               FROM computer_software cs
               JOIN computers c ON c.id = cs.computer_id
              WHERE cs.software_id = $1
                AND LOWER(c.name)  = LOWER($2)
              LIMIT 1",
        )
        .bind(software_id)
        .bind(computer)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(0);
        let tail = if count >= AUTO_UPGRADE_FAILURE_THRESHOLD {
            format!(
                "Hit {AUTO_UPGRADE_FAILURE_THRESHOLD} consecutive failures — \
                 status flipped to upgrade_blocked. Auto-retry stopped. \
                 Clear with: ff software auto-upgrade-run-once after fixing the root cause."
            )
        } else {
            format!(
                "Failure {count}/{AUTO_UPGRADE_FAILURE_THRESHOLD} — will retry on next hourly tick."
            )
        };
        format!(
            "Tried to bump {old_version} → {latest_version}\nerror: {}\n{tail}",
            err.unwrap_or("(unknown)"),
        )
    };
    if let Err(e) = ff_agent::telegram::send_telegram_from_secrets(pool, &title, &body).await {
        tracing::warn!(error = %e, software_id, computer, "telegram send failed");
    }
}

/// Best-effort register an external tool as an MCP stdio server in the
/// local `.mcp.json` config. The config is searched in the current working
/// directory first, then the user's home directory.
async fn register_mcp_server(tool_id: &str, server_command: &str) -> anyhow::Result<()> {
    let candidates = [
        std::path::PathBuf::from(".mcp.json"),
        dirs::home_dir()
            .map(|h| h.join(".mcp.json"))
            .unwrap_or_default(),
    ];

    let path = candidates.iter().find(|p| p.exists()).cloned();
    let path = match path {
        Some(p) => p,
        None => candidates[0].clone(), // create in cwd
    };

    let mut config: serde_json::Value = if path.exists() {
        let text = tokio::fs::read_to_string(&path).await?;
        serde_json::from_str(&text).unwrap_or_else(|_| serde_json::json!({ "mcpServers": {} }))
    } else {
        serde_json::json!({ "mcpServers": {} })
    };

    let servers = config
        .get_mut("mcpServers")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| anyhow::anyhow!(".mcp.json missing mcpServers object"))?;

    // Parse command into command + args (simple whitespace split).
    let parts: Vec<&str> = server_command.split_whitespace().collect();
    let (cmd, args) = parts.split_first().unwrap_or((&"", &[]));

    servers.insert(
        tool_id.to_string(),
        serde_json::json!({
            "command": cmd,
            "args": args,
            "type": "stdio",
        }),
    );

    let text = serde_json::to_string_pretty(&config)?;
    tokio::fs::write(&path, text).await?;

    Ok(())
}

/// Post-completion hook for `meta.external_tool` deferred tasks.
///
/// Runs whether the task succeeded or failed. Flips
/// `computer_external_tools.status` from `'installing'` / `'upgrading'`
/// to `'ok'` (success) or `'install_failed'` (failure), and makes a
/// best-effort attempt to parse `installed_version` / `install_path`
/// out of the task stdout. Also handles MCP auto-registration.
async fn finalize_external_tool_event(
    pool: &sqlx::PgPool,
    task: &ff_db::DeferredTaskRow,
    ok: bool,
    meta: &serde_json::Value,
    err: Option<&str>,
) {
    let tool_id = meta.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let display_name = meta
        .get("display_name")
        .and_then(|v| v.as_str())
        .unwrap_or(tool_id);
    let computer = meta.get("computer").and_then(|v| v.as_str()).unwrap_or("");
    let old_version = meta
        .get("old_version")
        .and_then(|v| v.as_str())
        .unwrap_or("-");
    let latest_version = meta
        .get("latest_version")
        .and_then(|v| v.as_str())
        .unwrap_or("-");

    // Best-effort: extract installed_version + install_path from task stdout.
    // The result JSON written by pg_finish_deferred stores the shell result
    // under `result` with `stdout`/`stderr`/`exit_code`.
    let stdout = task
        .result
        .as_ref()
        .and_then(|r| r.get("stdout"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Matches "installed X.Y.Z" / "version X.Y.Z" / "v1.2.3" patterns.
    let version_guess: Option<String> = stdout.lines().rev().find_map(|line| {
        let l = line.to_lowercase();
        if l.contains("installed") || l.contains("version") || l.contains("updated") {
            line.split_whitespace()
                .rev()
                .find(|tok| {
                    let s = tok.trim_start_matches('v');
                    s.chars()
                        .next()
                        .map(|c| c.is_ascii_digit())
                        .unwrap_or(false)
                })
                .map(|s| s.trim_start_matches('v').to_string())
        } else {
            None
        }
    });

    // Matches "Installing to /path/to/bin" or "/home/.../bin/<cli>".
    let path_guess: Option<String> = stdout.lines().rev().find_map(|line| {
        line.strip_prefix("Installing to ")
            .map(|rest| rest.trim().to_string())
    });

    let new_status = if ok { "ok" } else { "install_failed" };

    let register_as_mcp = meta
        .get("register_as_mcp")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mcp_server_command = meta
        .get("mcp_server_command")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());

    let mut mcp_registered = false;
    if ok
        && register_as_mcp
        && let Some(cmd) = mcp_server_command
    {
        match register_mcp_server(tool_id, cmd).await {
            Ok(_) => {
                mcp_registered = true;
                tracing::info!(tool_id, computer, "MCP auto-registration succeeded");
            }
            Err(e) => {
                tracing::warn!(tool_id, computer, error = %e, "MCP auto-registration failed");
            }
        }
    }

    let _ = sqlx::query(
        "UPDATE computer_external_tools cet
            SET status = $1,
                last_upgraded_at = CASE WHEN $1 = 'ok' THEN NOW() ELSE last_upgraded_at END,
                last_checked_at  = NOW(),
                installed_version = COALESCE($4, cet.installed_version),
                install_path      = COALESCE($5, cet.install_path),
                last_error        = CASE WHEN $1 = 'ok' THEN NULL ELSE $6 END,
                mcp_registered    = CASE WHEN $7 THEN true ELSE mcp_registered END
           FROM computers c
          WHERE cet.computer_id = c.id
            AND cet.tool_id     = $2
            AND LOWER(c.name)   = LOWER($3)",
    )
    .bind(new_status)
    .bind(tool_id)
    .bind(computer)
    .bind(version_guess.as_deref())
    .bind(path_guess.as_deref())
    .bind(err)
    .bind(mcp_registered)
    .execute(pool)
    .await;

    // NATS event on the same subject tree as software upgrades so dashboards
    // can subscribe to `fleet.events.software.>` and pick both up.
    let status_word = if ok { "success" } else { "failed" };
    let subject = format!(
        "fleet.events.external_tools.install_completed.{}",
        if computer.is_empty() {
            "unknown"
        } else {
            computer
        },
    );
    let payload = serde_json::json!({
        "tool_id":        tool_id,
        "display_name":   display_name,
        "computer":       computer,
        "old_version":    old_version,
        "latest_version": latest_version,
        "status":         status_word,
        "error":          err,
        "defer_id":       task.id,
        "ts":             chrono::Utc::now().to_rfc3339(),
    });
    ff_agent::nats_client::publish_json(subject, &payload).await;
}

/// Wrap a user shell command so any `&`-spawned children survive after the
/// wrapper exits. Without this, `nohup llama-server ... &` inside a defer
/// task would launch successfully and then be killed seconds later — either
/// by SIGHUP when the SSH session tears down, or by the parent's process
/// group cleanup on the local side.
///
/// Strategy: run the user command inside `setsid sh -c '...'` so it gets a
/// fresh session + process group. Children inherit that group and survive
/// the parent's exit. `setsid` is ubiquitous on Linux; on macOS it's not
/// present, so we fall back to plain `sh -c` (Taylor is the only macOS
/// defer-worker host, and it's the leader/human-in-loop — operators should
/// prefer `nohup <cmd> </dev/null >/dev/null 2>&1 & disown` there).
fn wrap_for_detachment(user_cmd: &str, is_linux_target: bool) -> String {
    if is_linux_target {
        // Single-quote-escape the user script for `setsid sh -c '...'`.
        let escaped = user_cmd.replace('\'', "'\\''");
        format!("setsid sh -c '{escaped}'")
    } else {
        // macOS or unknown — caller must detach manually.
        // TODO: background processes in shell payloads on macOS must use
        // `nohup <cmd> </dev/null >/dev/null 2>&1 & disown` — operator
        // responsibility (setsid is unavailable).
        user_cmd.to_string()
    }
}

/// Run a shell command either locally (when target is this host or None) or via SSH.
/// Max time a shell payload may run before it is killed.
const SHELL_TIMEOUT: Duration = Duration::from_secs(1800); // 30 min
/// Max bytes to capture per stream (stdout / stderr). Anything beyond this
/// is dropped and the pipe is closed so the child gets SIGPIPE.
const MAX_SHELL_OUTPUT_BYTES: usize = 10 * 1024 * 1024; // 10 MB

async fn execute_shell(
    target_node: Option<&str>,
    command: &str,
    nodes: &[ff_db::FleetNodeRow],
    workspace: Option<&std::path::Path>,
) -> (bool, Option<serde_json::Value>, Option<String>) {
    use tokio::io::AsyncReadExt;
    use tokio::process::Command as TokCmd;
    use tokio::time::timeout;

    let this_hostname = tokio::process::Command::new("hostname")
        .output()
        .await
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_default();

    // Local host is Linux if uname reports Linux.
    let local_is_linux = std::env::consts::OS == "linux";

    let mut local = true;
    let (program, args): (&str, Vec<String>) = match target_node {
        None => (
            "sh",
            vec!["-c".into(), wrap_for_detachment(command, local_is_linux)],
        ),
        Some(n) if this_hostname.starts_with(&n.to_lowercase()) => (
            "sh",
            vec!["-c".into(), wrap_for_detachment(command, local_is_linux)],
        ),
        Some(n) => {
            local = false;
            // SSH to target: look up user@ip from DB.
            let node = match nodes.iter().find(|x| x.name.eq_ignore_ascii_case(n)) {
                Some(n) => n,
                None => return (false, None, Some(format!("node '{n}' not in fleet_nodes"))),
            };
            let dest = format!("{}@{}", node.ssh_user, node.ip);
            // Assume remote targets are Linux (Marcus/Sophie/Priya are Ubuntu;
            // James is macOS — but gets same treatment: wrap_for_detachment
            // returns plain cmd for non-Linux, which is safe).
            // `-n` closes stdin so backgrounded children aren't wedged on it.
            let os_hint = node.os.to_lowercase();
            // Default to Linux (most fleet nodes): covers ubuntu, debian,
            // dgx-os, generic "linux". Exclude darwin/macos explicitly.
            let remote_is_linux = !(os_hint.contains("darwin") || os_hint.contains("macos"));
            (
                "ssh",
                vec![
                    "-n".into(),
                    "-o".into(),
                    "ConnectTimeout=8".into(),
                    "-o".into(),
                    "StrictHostKeyChecking=accept-new".into(),
                    "-o".into(),
                    "BatchMode=yes".into(),
                    dest,
                    wrap_for_detachment(command, remote_is_linux),
                ],
            )
        }
    };

    let mut cmd = TokCmd::new(program);
    cmd.args(&args);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    if local && let Some(ws) = workspace {
        cmd.current_dir(ws);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (false, None, Some(format!("spawn {program} failed: {e}"))),
    };

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let stdout_fut = async move {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stdout_pipe {
            let mut chunk = [0u8; 8192];
            while buf.len() < MAX_SHELL_OUTPUT_BYTES {
                match pipe.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let to_add = n.min(MAX_SHELL_OUTPUT_BYTES - buf.len());
                        buf.extend_from_slice(&chunk[..to_add]);
                    }
                    Err(_) => break,
                }
            }
            // Pipe dropped here → child gets SIGPIPE on further writes.
        }
        buf
    };

    let stderr_fut = async move {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stderr_pipe {
            let mut chunk = [0u8; 8192];
            while buf.len() < MAX_SHELL_OUTPUT_BYTES {
                match pipe.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let to_add = n.min(MAX_SHELL_OUTPUT_BYTES - buf.len());
                        buf.extend_from_slice(&chunk[..to_add]);
                    }
                    Err(_) => break,
                }
            }
        }
        buf
    };

    let (stdout, stderr, status) = match timeout(SHELL_TIMEOUT, async {
        let (stdout, stderr) = tokio::join!(stdout_fut, stderr_fut);
        let status = child.wait().await.map_err(|e| e.to_string())?;
        Ok::<_, String>((stdout, stderr, status))
    })
    .await
    {
        Ok(Ok(triple)) => triple,
        Ok(Err(e)) => return (false, None, Some(format!("shell execution failed: {e}"))),
        Err(_) => {
            let _ = child.start_kill();
            return (
                false,
                None,
                Some(format!(
                    "shell command timed out after {}s",
                    SHELL_TIMEOUT.as_secs()
                )),
            );
        }
    };

    let stdout = String::from_utf8_lossy(&stdout).to_string();
    let stderr = String::from_utf8_lossy(&stderr).to_string();
    let result = serde_json::json!({
        "exit_code": status.code(),
        "stdout": stdout,
        "stderr": stderr,
    });
    if status.success() {
        (true, Some(result), None)
    } else {
        let err = format!(
            "exit {}: {}",
            status.code().unwrap_or(-1),
            stderr.trim().lines().last().unwrap_or("")
        );
        (false, Some(result), Some(err))
    }
}

/// Shared reqwest client for HTTP deferred tasks (avoids creating a new
/// connection pool on every call).
static HTTP_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();

fn http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client build must succeed")
    })
}

/// Max HTTP response body we will load into memory (prevents unbounded
/// buffering if a server returns a massive payload).
const MAX_HTTP_RESPONSE_BYTES: usize = 10 * 1024 * 1024; // 10 MB

/// Execute an HTTP request task.
async fn execute_http(
    method: &str,
    url: &str,
    body: Option<serde_json::Value>,
) -> (bool, Option<serde_json::Value>, Option<String>) {
    let method_obj = match method.to_uppercase().as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "DELETE" => reqwest::Method::DELETE,
        "PATCH" => reqwest::Method::PATCH,
        other => return (false, None, Some(format!("bad http method: {other}"))),
    };
    let mut req = http_client().request(method_obj, url);
    if let Some(b) = body {
        req = req.json(&b);
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            // Reject early if the server advertises a body larger than our cap.
            if resp
                .content_length()
                .map_or(false, |len| len > MAX_HTTP_RESPONSE_BYTES as u64)
            {
                return (
                    false,
                    None,
                    Some(format!(
                        "HTTP response body exceeds {}MB (Content-Length)",
                        MAX_HTTP_RESPONSE_BYTES / 1_048_576
                    )),
                );
            }
            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => return (false, None, Some(format!("http body read: {e}"))),
            };
            if bytes.len() > MAX_HTTP_RESPONSE_BYTES {
                return (
                    false,
                    None,
                    Some(format!(
                        "HTTP response body exceeds {}MB",
                        MAX_HTTP_RESPONSE_BYTES / 1_048_576
                    )),
                );
            }
            let text = String::from_utf8_lossy(&bytes).to_string();
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


async fn handle_fleet(cmd: FleetCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        FleetCommand::SshMeshCheck {
            node,
            json,
            since,
            repair,
            yes,
        } => {
            if repair && !yes {
                anyhow::bail!(
                    "--repair rewrites authorized_keys / known_hosts on every failed peer — pass --yes to proceed"
                );
            }
            if repair {
                println!("{CYAN}▶ Repairing mesh before probing...{RESET}");
                let failed = ff_db::pg_list_mesh_status(&pool, None)
                    .await
                    .map_err(|e| anyhow::anyhow!("pg_list_mesh_status: {e}"))?
                    .into_iter()
                    .filter(|r| r.status == "failed")
                    .collect::<Vec<_>>();
                println!(
                    "  found {} failed pair(s) — re-enqueuing as mesh_retry tasks",
                    failed.len()
                );
                let created = ff_agent::mesh_check::enqueue_retries(&pool)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
                println!("  enqueued {} mesh_retry task(s)", created);
            }
            if let Some(spec) = &since {
                let age = parse_duration(spec).ok_or_else(|| {
                    anyhow::anyhow!("unrecognized --since value '{spec}' (try 1h, 30m, 2d)")
                })?;
                println!("{CYAN}▶ Refreshing pairs older than {spec}...{RESET}");
                let n = ff_agent::mesh_check::refresh_stale(&pool, age)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
                println!("  refreshed {n} stale pair(s)");
                return Ok(());
            }
            println!("{CYAN}▶ Running pairwise SSH mesh check...{RESET}");
            let matrix = match &node {
                Some(n) => ff_agent::mesh_check::pairwise_ssh_check_node(&pool, n).await,
                None => ff_agent::mesh_check::pairwise_ssh_check(&pool).await,
            }
            .map_err(|e| anyhow::anyhow!(e))?;
            if json {
                let arr: Vec<_> = matrix.cells.iter().map(|c| serde_json::json!({
                    "src": c.src, "dst": c.dst, "status": c.status, "last_error": c.last_error,
                })).collect();
                println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
            } else {
                let mut ok = 0;
                let mut fail = 0;
                for c in &matrix.cells {
                    let marker = if c.status == "ok" { "✓" } else { "✗" };
                    if c.status == "ok" {
                        ok += 1;
                    } else {
                        fail += 1;
                    }
                    let err = c.last_error.as_deref().unwrap_or("");
                    println!("  {:<10} → {:<10}  {}  {}", c.src, c.dst, marker, err);
                }
                println!(
                    "\n{ok} ok, {fail} failed — checked {} pairs",
                    matrix.cells.len()
                );
            }
        }
        FleetCommand::VerifyNode { name, json } => {
            println!("{CYAN}▶ Running verify-node battery for {name}...{RESET}");
            let report = ff_agent::verify_node::verify_node(&pool, &name)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).unwrap_or_default()
                );
            } else {
                println!(
                    "\nResults for {}: {} pass, {} fail, {} skip",
                    report.node, report.passed, report.failed, report.skipped
                );
                for r in &report.details {
                    let marker = match r.status.as_str() {
                        "pass" => "✓",
                        "fail" => "✗",
                        _ => "—",
                    };
                    let msg = r.message.as_deref().unwrap_or("");
                    println!("  {}  {:<28}  {}", marker, r.check, msg);
                }
            }
        }
        FleetCommand::Leader { json } => {
            fleet_cmd::handle_fleet_leader(&pool, json).await?;
        }
        FleetCommand::Health { json } => {
            fleet_cmd::handle_fleet_health(&pool, json).await?;
        }
        FleetCommand::Versions { verbose, live } => {
            fleet_cmd::handle_fleet_versions(&pool, verbose, live).await?;
        }
        FleetCommand::Gossip => {
            fleet_cmd::handle_fleet_gossip().await?;
        }
        FleetCommand::MigrateGithub {
            new_owner,
            skip_local,
            only,
            dry_run,
            yes,
        } => {
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
            println!(
                "  local node:      {local}{}",
                if skip_local { " (skipped)" } else { "" }
            );
            println!("  targets:         {} node(s)", targets.len());
            for n in &targets {
                println!(
                    "    {:<15} {:<16} {}",
                    n.name,
                    n.ip,
                    n.gh_account.clone().unwrap_or_else(|| "-".into())
                );
            }
            if targets.is_empty() {
                println!("{YELLOW}No nodes to enqueue. Nothing to do.{RESET}");
                return Ok(());
            }
            if dry_run || !yes {
                println!(
                    "\n{YELLOW}Dry run — not enqueuing. Pass --yes to actually enqueue.{RESET}"
                );
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
            println!(
                "\n{GREEN}✓ Enqueued {} migration task(s):{RESET}",
                enqueued.len()
            );
            for (node, id) in &enqueued {
                println!("  {:<15} {id}", node);
            }
            println!("\nTrack progress with: ff defer list");
        }
        FleetCommand::Revive {
            computer,
            wol_only,
            internal,
        } => {
            fleet_cmd::handle_fleet_revive(&pool, &computer, wol_only, internal).await?;
        }
        FleetCommand::TaskCoverage { command } => {
            fleet_cmd::handle_fleet_task_coverage(&pool, command).await?;
        }
        FleetCommand::RevokeTrust { computer, yes } => {
            fleet_cmd::handle_fleet_revoke_trust(&pool, &computer, yes).await?;
        }
        FleetCommand::RemoveComputer { name, yes } => {
            fleet_cmd::handle_fleet_remove_computer(&pool, &name, yes).await?;
        }
        FleetCommand::Disband {
            yes,
            i_know_what_im_doing,
        } => {
            fleet_cmd::handle_fleet_disband(&pool, yes, i_know_what_im_doing).await?;
        }
        FleetCommand::MigrateSourceTrees { dry_run, yes } => {
            fleet_cmd::handle_fleet_migrate_source_trees(&pool, dry_run, yes).await?;
        }
        FleetCommand::RotateSshKey { computer } => {
            let mgr = ff_agent::ssh_key_manager::SshKeyManager::new(pool.clone());
            match mgr.rotate_computer_keypair(&computer).await {
                Ok(()) => println!("{GREEN}✓ rotate complete{RESET}"),
                Err(e) => {
                    eprintln!("{YELLOW}Not yet implemented:{RESET} {e}");
                    std::process::exit(2);
                }
            }
        }
        FleetCommand::RotatePulseHmac { value } => {
            fleet_cmd::handle_fleet_rotate_pulse_hmac(&pool, value).await?;
        }
        FleetCommand::Backup { kind, force } => {
            fleet_cmd::handle_fleet_backup(&pool, &kind, force).await?;
        }
        FleetCommand::SetNetworkScope { computer, scope } => {
            fleet_cmd::handle_fleet_set_network_scope(&pool, &computer, &scope).await?;
        }
        FleetCommand::Db { command } => {
            fleet_cmd::handle_fleet_db(&pool, command).await?;
        }
        FleetCommand::PanicStop { yes, halt_dbs } => {
            fleet_cmd::handle_fleet_panic_stop(&pool, yes, halt_dbs).await?;
        }
        FleetCommand::Resume { yes } => {
            fleet_cmd::handle_fleet_resume(&pool, yes).await?;
        }
        FleetCommand::Quarantine { computer, yes } => {
            fleet_cmd::handle_fleet_quarantine(&pool, &computer, yes).await?;
        }
        FleetCommand::Unquarantine { computer, yes } => {
            fleet_cmd::handle_fleet_unquarantine(&pool, &computer, yes).await?;
        }
        FleetCommand::Upgrade {
            software_id,
            computer,
            all,
            dry_run,
            yes,
            force_dirty,
        } => {
            fleet_cmd::handle_fleet_upgrade(
                &pool,
                &software_id,
                computer,
                all,
                dry_run,
                yes,
                force_dirty,
            )
            .await?;
        }
    }
    Ok(())
}



async fn handle_software(cmd: SoftwareCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        SoftwareCommand::List {
            computer,
            software,
            json,
        } => software_cmd::handle_software_list(&pool, computer, software, json).await,
        SoftwareCommand::Drift { json } => software_cmd::handle_software_drift(&pool, json).await,
        SoftwareCommand::Add {
            id,
            kind,
            version_source,
            upgrade_playbook,
            display_name,
        } => {
            software_cmd::handle_software_add(
                &pool,
                &id,
                &kind,
                &version_source,
                &upgrade_playbook,
                display_name,
            )
            .await
        }
        SoftwareCommand::Remove { id, yes } => {
            software_cmd::handle_software_remove(&pool, &id, yes).await
        }
        SoftwareCommand::AutoUpgradeRunOnce { force } => {
            software_cmd::handle_auto_upgrade_run_once(&pool, force).await
        }
        SoftwareCommand::Unblock {
            computer,
            software_id,
        } => software_cmd::handle_software_unblock(&pool, &computer, &software_id).await,
    }
}

/// Implementation of `ff software unblock <computer> <software_id>`.
///
/// Resets the failure counter and flips the row from `upgrade_blocked`
/// (or any other status that's not `upgrading`) back to either `ok` or
/// `upgrade_available` — `flip_drift_status` recalculates on the next
/// auto-upgrade tick so the row gets the right post-clear state.

// ─── ff ext ────────────────────────────────────────────────────────────────
//
// Mirrors `ff software` / `ff fleet upgrade` but scoped to the V24
// `external_tools` + `computer_external_tools` tables.

async fn handle_ext(cmd: ExtCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        ExtCommand::List { json } => ext_cmd::handle_ext_list(&pool, json).await,
        ExtCommand::Installed {
            computer,
            tool,
            json,
        } => ext_cmd::handle_ext_installed(&pool, computer, tool, json).await,
        ExtCommand::Install {
            tool_id,
            computer,
            all,
            dry_run,
            yes,
        } => ext_cmd::handle_ext_install(&pool, &tool_id, computer, all, dry_run, yes).await,
        ExtCommand::Drift { json } => ext_cmd::handle_ext_drift(&pool, json).await,
    }
}

// ─── ff ports ──────────────────────────────────────────────────────────────

async fn handle_ports(cmd: PortsCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        PortsCommand::List { kind, scope, json } => {
            ports_cmd::handle_ports_list(&pool, kind, scope, json).await
        }
        PortsCommand::Scan { computer } => ports_cmd::handle_ports_scan(&pool, &computer).await,
    }
}

async fn handle_cloud_llm(cmd: CloudLlmCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        CloudLlmCommand::List { json } => cloud_llm_cmd::handle_cloud_llm_list(&pool, json).await,
        CloudLlmCommand::SetKey { provider_id, value } => {
            cloud_llm_cmd::handle_cloud_llm_set_key(&pool, &provider_id, value).await
        }
        CloudLlmCommand::Usage { since } => {
            cloud_llm_cmd::handle_cloud_llm_usage(&pool, &since).await
        }
        CloudLlmCommand::Test { provider_id, model } => {
            cloud_llm_cmd::handle_cloud_llm_test(&pool, &provider_id, model).await
        }
    }
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
# Retire ~/taylorProjects fully. If the legacy dir or symlink lingers, drop it.
rm -rf "$OLD_DIR" 2>/dev/null || true
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




async fn handle_agent(cmd: AgentCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        AgentCommand::Seed => {
            let n = ff_agent::agent_coordinator::seed_slot_zero_for_all(&pool)
                .await
                .map_err(|e| anyhow::anyhow!("seed: {e}"))?;
            println!("{GREEN}✓{RESET} seeded {n} new sub_agent row(s)");
            Ok(())
        }
        AgentCommand::SubAgents { json } => {
            let rows = ff_agent::agent_coordinator::list_sub_agents(&pool)
                .await
                .map_err(|e| anyhow::anyhow!("list: {e}"))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(());
            }
            if rows.is_empty() {
                println!("(no sub_agent rows — run `ff agent seed`)");
                return Ok(());
            }
            println!(
                "{:<14} {:<4} {:<8} {:<36} WORKSPACE",
                "COMPUTER", "SLOT", "STATUS", "ID"
            );
            for r in rows {
                println!(
                    "{:<14} {:<4} {:<8} {:<36} {}",
                    r.computer,
                    r.slot,
                    r.status,
                    r.id.to_string(),
                    r.workspace_dir
                );
            }
            Ok(())
        }
        AgentCommand::Dispatch {
            prompt,
            to_computer,
            work_item_id,
            json,
        } => {
            // Resolve or create the work_item.
            let wi_id = if let Some(id_str) = work_item_id.clone() {
                uuid::Uuid::parse_str(&id_str)
                    .map_err(|e| anyhow::anyhow!("invalid --work-item-id: {e}"))?
            } else {
                let created_by = ff_agent::fleet_info::resolve_this_node_name().await;
                ff_agent::agent_coordinator::create_transient_work_item(&pool, &prompt, &created_by)
                    .await
                    .map_err(|e| anyhow::anyhow!("create transient work_item: {e}"))?
            };

            // Build the coordinator.
            let redis_url = resolve_pulse_redis_url();
            let reader = ff_pulse::reader::PulseReader::new(&redis_url)
                .map_err(|e| anyhow::anyhow!("pulse reader: {e}"))?;
            let coord = ff_agent::agent_coordinator::AgentCoordinator::new(
                pool.clone(),
                std::sync::Arc::new(reader),
            );

            let receipt = coord
                .dispatch_task(wi_id, prompt.clone(), to_computer.clone())
                .await
                .map_err(|e| anyhow::anyhow!("dispatch: {e}"))?;

            if json {
                let out = serde_json::json!({
                    "work_item_id": receipt.work_item_id,
                    "sub_agent_id": receipt.sub_agent_id,
                    "work_output_id": receipt.work_output_id,
                    "computer": receipt.computer_name,
                    "model": receipt.model_id,
                    "duration_ms": receipt.duration_ms,
                    "response": receipt.response_text,
                });
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                println!("{GREEN}✓ dispatched{RESET}");
                println!("  work_item: {}", receipt.work_item_id);
                println!("  computer:  {}", receipt.computer_name);
                println!("  model:     {}", receipt.model_id);
                println!("  duration:  {}ms", receipt.duration_ms);
                if let Some(wo) = receipt.work_output_id {
                    println!("  output:    {wo}");
                }
                println!("\n{CYAN}── response ──{RESET}\n{}", receipt.response_text);
            }
            Ok(())
        }
        AgentCommand::CommitBack { session, push, pr } => {
            agent_cmd::handle_agent_commit_back(&pool, &session, push, pr).await
        }
        AgentCommand::Fanout {
            prompt,
            backend,
            fanout,
        } => agent_cmd::handle_agent_fanout(&pool, prompt, backend, fanout).await,
        AgentCommand::DispatchEach { prompt, backend } => {
            agent_cmd::handle_agent_dispatch_each(&pool, prompt, backend).await
        }
    }
}

/// Emit `fanout` shell tasks, each requiring capability `[backend]`.
/// Each task runs `ff run --backend <backend> "<prompt>"` on whichever
/// fleet worker grabs it. Workers compete via the existing SKIP LOCKED

async fn handle_pm(cmd: PmCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        PmCommand::List {
            project,
            status,
            assignee,
        } => {
            let rows: Vec<(
                uuid::Uuid,
                String,
                String,
                String,
                String,
                String,
                Option<String>,
                chrono::DateTime<chrono::Utc>,
            )> = sqlx::query_as(
                "SELECT wi.id, wi.project_id, wi.kind, wi.title, wi.status, wi.priority, \
                        wi.assigned_to, wi.created_at \
                 FROM work_items wi \
                 WHERE ($1::text IS NULL OR wi.project_id = $1) \
                   AND ($2::text IS NULL OR wi.status = $2) \
                   AND ($3::text IS NULL OR wi.assigned_to = $3) \
                 ORDER BY wi.created_at DESC \
                 LIMIT 200",
            )
            .bind(project.as_deref())
            .bind(status.as_deref())
            .bind(assignee.as_deref())
            .fetch_all(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("list work items: {e}"))?;

            if rows.is_empty() {
                println!("(no work items)");
                return Ok(());
            }

            println!(
                "{:<38} {:<14} {:<6} {:<10} {:<8} {:<14} TITLE",
                "ID", "PROJECT", "KIND", "STATUS", "PRIORITY", "ASSIGNEE"
            );
            for (id, pid, kind, title, st, prio, asgn, _created) in rows {
                let title_clip = if title.chars().count() > 60 {
                    format!("{}…", title.chars().take(59).collect::<String>())
                } else {
                    title
                };
                println!(
                    "{:<38} {:<14} {:<6} {:<10} {:<8} {:<14} {}",
                    id.to_string(),
                    pid,
                    kind,
                    st,
                    prio,
                    asgn.as_deref().unwrap_or("-"),
                    title_clip,
                );
            }
        }
        PmCommand::Create {
            project,
            kind,
            title,
            description,
            priority,
        } => {
            // Validate project exists first so we give a clear error instead of an FK violation.
            let exists: Option<(String,)> = sqlx::query_as("SELECT id FROM projects WHERE id = $1")
                .bind(&project)
                .fetch_optional(&pool)
                .await
                .map_err(|e| anyhow::anyhow!("query project: {e}"))?;
            if exists.is_none() {
                return Err(anyhow::anyhow!(
                    "unknown project '{project}' — run `ff project seed` or check `ff project list`"
                ));
            }

            let created_by = ff_agent::fleet_info::resolve_this_node_name().await;
            let prio = priority.unwrap_or_else(|| "normal".to_string());
            let row: (uuid::Uuid,) = sqlx::query_as(
                "INSERT INTO work_items (project_id, kind, title, description, priority, created_by) \
                 VALUES ($1, $2, $3, $4, $5, $6) \
                 RETURNING id",
            )
            .bind(&project)
            .bind(&kind)
            .bind(&title)
            .bind(description.as_deref())
            .bind(&prio)
            .bind(&created_by)
            .fetch_one(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("insert work item: {e}"))?;

            println!("{GREEN}✓ Created work item{RESET}");
            println!("  id:       {}", row.0);
            println!("  project:  {project}");
            println!("  kind:     {kind}");
            println!("  title:    {title}");
            println!("  priority: {prio}");
            println!("  created_by: {created_by}");
        }
        PmCommand::Show { id } => {
            let uid = uuid::Uuid::parse_str(&id)
                .map_err(|e| anyhow::anyhow!("invalid UUID '{id}': {e}"))?;
            let row: Option<(
                uuid::Uuid,
                String,
                String,
                String,
                Option<String>,
                String,
                String,
                Option<String>,
                Option<String>,
                String,
                chrono::DateTime<chrono::Utc>,
            )> = sqlx::query_as(
                "SELECT id, project_id, kind, title, description, status, priority, \
                        assigned_to, assigned_computer, created_by, created_at \
                 FROM work_items WHERE id = $1",
            )
            .bind(uid)
            .fetch_optional(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("query work item: {e}"))?;

            let Some((
                id,
                pid,
                kind,
                title,
                desc,
                status,
                prio,
                asgn,
                computer,
                created_by,
                created_at,
            )) = row
            else {
                return Err(anyhow::anyhow!("work item {uid} not found"));
            };

            println!("{CYAN}Work item{RESET} {id}");
            println!("  project:      {pid}");
            println!("  kind:         {kind}");
            println!("  title:        {title}");
            if let Some(d) = desc.as_deref() {
                println!("  description:  {d}");
            }
            println!("  status:       {status}");
            println!("  priority:     {prio}");
            println!("  assigned_to:  {}", asgn.as_deref().unwrap_or("-"));
            println!("  computer:     {}", computer.as_deref().unwrap_or("-"));
            println!("  created_by:   {created_by}");
            println!(
                "  created_at:   {}",
                created_at.format("%Y-%m-%d %H:%M UTC")
            );

            let outputs: Vec<(
                uuid::Uuid,
                String,
                Option<String>,
                Option<String>,
                chrono::DateTime<chrono::Utc>,
            )> = sqlx::query_as(
                "SELECT id, kind, title, file_path, produced_at \
                     FROM work_outputs WHERE work_item_id = $1 \
                     ORDER BY produced_at DESC",
            )
            .bind(uid)
            .fetch_all(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("query work outputs: {e}"))?;

            if !outputs.is_empty() {
                println!();
                println!("{CYAN}Outputs ({}){RESET}", outputs.len());
                for (oid, okind, otitle, opath, oat) in outputs {
                    println!(
                        "  {} [{okind}] {} {} — {}",
                        oid,
                        otitle.as_deref().unwrap_or("-"),
                        opath.as_deref().unwrap_or("-"),
                        oat.format("%Y-%m-%d %H:%M UTC"),
                    );
                }
            }
        }
        PmCommand::ImportClaudeTasks {
            session,
            project,
            dry_run,
        } => {
            handle_pm_import_claude_tasks(&pool, session, &project, dry_run).await?;
        }
    }
    Ok(())
}

/// `ff pm import-claude-tasks` — parses the Claude Code session JSONL
/// and upserts each task as a `work_items` row.
///
/// Claude Code doesn't persist its task list to a separate file; the
/// state is embedded in the session transcript as `tool_result` content
/// on TaskCreate/TaskList/TaskUpdate calls. The format per line is
/// `#<id> [<status>] <subject>`. We scan for the LAST occurrence of
/// this format in the transcript and treat that as the authoritative
/// snapshot (older lines are stale).
///
/// Dedupe key: the Claude task ID is stored in
/// `work_items.metadata->>'claude_task_id'`; repeat imports UPDATE the
/// same row rather than creating a new one.
async fn handle_pm_import_claude_tasks(
    pool: &sqlx::PgPool,
    session: Option<PathBuf>,
    project: &str,
    dry_run: bool,
) -> Result<()> {
    // Resolve session path. If the operator didn't pass --session, try
    // to find the most recently-modified .jsonl in the current project's
    // Claude dir. Encoding mirrors Claude's slug: `/Users/venkat/...` →
    // `-Users-venkat-...`.
    let resolved = if let Some(p) = session {
        p
    } else {
        let cwd = std::env::current_dir().map_err(|e| anyhow::anyhow!("cwd: {e}"))?;
        let slug = cwd.to_string_lossy().replace('/', "-");
        let home = std::env::var("HOME").unwrap_or_default();
        let project_dir = PathBuf::from(format!("{home}/.claude/projects/{slug}"));
        let mut newest: Option<(PathBuf, std::time::SystemTime)> = None;
        if let Ok(mut entries) = tokio::fs::read_dir(&project_dir).await {
            while let Some(e) = entries.next_entry().await.unwrap_or(None) {
                let path = e.path();
                if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }
                if let Ok(md) = e.metadata().await
                    && let Ok(mtime) = md.modified()
                    && newest.as_ref().map(|(_, t)| mtime > *t).unwrap_or(true)
                {
                    newest = Some((path, mtime));
                }
            }
        }
        newest
            .map(|(p, _)| p)
            .ok_or_else(|| anyhow::anyhow!("no session JSONL found under {:?}", project_dir))?
    };

    println!(
        "{CYAN}▶ Importing Claude tasks from{RESET} {}",
        resolved.display()
    );

    // Stream the JSONL, tracking the LAST task-list snapshot. Each line
    // we care about has content like `#<N> [<status>] <subject>` — we
    // find them inside tool_result `content` strings.
    let content = tokio::fs::read_to_string(&resolved)
        .await
        .map_err(|e| anyhow::anyhow!("read {}: {e}", resolved.display()))?;
    let task_line_re =
        regex::Regex::new(r"#(\d+)\s*(?:\.\s*)?\[(pending|in_progress|completed|deleted)\]\s+(.+)")
            .map_err(|e| anyhow::anyhow!("regex: {e}"))?;

    // Group by task_id — later occurrences overwrite earlier ones.
    let mut snapshot: std::collections::BTreeMap<String, (String, String)> = Default::default();
    for line in content.lines() {
        // Only look inside lines that mention system-reminder OR TaskList-shaped content.
        if !line.contains("[pending]")
            && !line.contains("[completed]")
            && !line.contains("[in_progress]")
        {
            continue;
        }
        for cap in task_line_re.captures_iter(line) {
            let id = cap[1].to_string();
            let status = cap[2].to_string();
            let mut subject = cap[3].trim().to_string();
            // Subject ends at the end of the match; may have trailing JSON
            // escape chars. Trim at the first of a few known terminators.
            for term in ["\\n", "\"", "\n"] {
                if let Some(pos) = subject.find(term) {
                    subject.truncate(pos);
                }
            }
            let subject = subject.trim_end().to_string();
            if !subject.is_empty() {
                snapshot.insert(id, (status, subject));
            }
        }
    }

    if snapshot.is_empty() {
        println!("  (no task lines recognized in transcript)");
        return Ok(());
    }

    println!("  found {} unique tasks", snapshot.len());
    // Confirm project exists.
    let project_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM projects WHERE id = $1)")
            .bind(project)
            .fetch_one(pool)
            .await
            .map_err(|e| anyhow::anyhow!("project probe: {e}"))?;
    if !project_exists {
        eprintln!(
            "{YELLOW}project '{project}' not found — create it first or pass --project X{RESET}"
        );
        std::process::exit(2);
    }

    if dry_run {
        println!("\n{YELLOW}Dry run — not writing.{RESET}");
        for (id, (status, subject)) in &snapshot {
            let clip = if subject.chars().count() > 60 {
                format!("{}…", subject.chars().take(59).collect::<String>())
            } else {
                subject.clone()
            };
            println!("  would upsert #{id:<3} [{status:<11}] {clip}");
        }
        return Ok(());
    }

    let mut inserted = 0usize;
    let mut updated = 0usize;
    for (id, (status, subject)) in &snapshot {
        // Upsert by (project_id, claude_task_id). work_items has no
        // unique constraint on that pair so we check-then-insert/update.
        let existing: Option<uuid::Uuid> = sqlx::query_scalar(
            "SELECT id FROM work_items
              WHERE project_id = $1
                AND metadata->>'claude_task_id' = $2
              LIMIT 1",
        )
        .bind(project)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|e| anyhow::anyhow!("lookup existing: {e}"))?;

        let wi_status = match status.as_str() {
            "pending" => "backlog",
            "in_progress" => "in_progress",
            "completed" => "done",
            _ => "backlog",
        };

        if let Some(wi_id) = existing {
            sqlx::query(
                "UPDATE work_items
                    SET status = $1,
                        title  = $2
                  WHERE id = $3",
            )
            .bind(wi_status)
            .bind(subject)
            .bind(wi_id)
            .execute(pool)
            .await
            .map_err(|e| anyhow::anyhow!("update work_item: {e}"))?;
            updated += 1;
        } else {
            sqlx::query(
                "INSERT INTO work_items
                    (project_id, kind, title, status, priority, created_by, metadata)
                 VALUES ($1, 'code', $2, $3, 'normal', 'claude_code',
                         jsonb_build_object('claude_task_id', $4::text,
                                            'imported_at', NOW()::text))",
            )
            .bind(project)
            .bind(subject)
            .bind(wi_status)
            .bind(id)
            .execute(pool)
            .await
            .map_err(|e| anyhow::anyhow!("insert work_item: {e}"))?;
            inserted += 1;
        }
    }

    println!(
        "{GREEN}✓ imported{RESET}: {inserted} new, {updated} updated ({} total from Claude)",
        snapshot.len()
    );
    Ok(())
}

async fn handle_project(cmd: ProjectCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        ProjectCommand::List => {
            let rows: Vec<(
                String,
                String,
                Option<String>,
                String,
                Option<String>,
                Option<chrono::DateTime<chrono::Utc>>,
                String,
            )> = sqlx::query_as(
                "SELECT id, display_name, repo_url, default_branch, main_commit_sha, \
                        main_last_synced_at, status \
                 FROM projects ORDER BY id",
            )
            .fetch_all(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("list projects: {e}"))?;

            if rows.is_empty() {
                println!("(no projects — run `ff project seed` to load config/projects.toml)");
                return Ok(());
            }

            println!(
                "{:<14} {:<14} {:<8} {:<10} {:<18} REPO",
                "ID", "NAME", "BRANCH", "SHA", "SYNCED"
            );
            let now = chrono::Utc::now();
            for (id, name, repo, branch, sha, synced, _status) in rows {
                let sha_s = sha.as_deref().map(|s| &s[..s.len().min(8)]).unwrap_or("-");
                let synced_s = match synced {
                    Some(t) => {
                        let age = now.signed_duration_since(t);
                        if age.num_days() > 0 {
                            format!("{}d ago", age.num_days())
                        } else if age.num_hours() > 0 {
                            format!("{}h ago", age.num_hours())
                        } else if age.num_minutes() > 0 {
                            format!("{}m ago", age.num_minutes())
                        } else {
                            "just now".to_string()
                        }
                    }
                    None => "never".to_string(),
                };
                println!(
                    "{:<14} {:<14} {:<8} {:<10} {:<18} {}",
                    id,
                    name,
                    branch,
                    sha_s,
                    synced_s,
                    repo.as_deref().unwrap_or("-"),
                );
            }
        }
        ProjectCommand::Status { id } => {
            let project: Option<(
                String,
                String,
                Option<String>,
                String,
                Option<String>,
                Option<String>,
                Option<chrono::DateTime<chrono::Utc>>,
                Option<String>,
            )> = sqlx::query_as(
                "SELECT id, display_name, repo_url, default_branch, \
                        main_commit_sha, main_commit_message, main_committed_at, main_committed_by \
                 FROM projects WHERE id = $1",
            )
            .bind(&id)
            .fetch_optional(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("query project: {e}"))?;

            let Some((id, name, repo, branch, sha, msg, committed_at, committed_by)) = project
            else {
                return Err(anyhow::anyhow!("project '{id}' not found"));
            };

            println!("{CYAN}Project{RESET} {id} — {name}");
            println!("  repo:          {}", repo.as_deref().unwrap_or("-"));
            println!("  default branch: {branch}");
            println!(
                "  main:          {} — {}",
                sha.as_deref().unwrap_or("-"),
                msg.as_deref().unwrap_or("-")
            );
            if let Some(at) = committed_at {
                println!(
                    "  committed:     {} by {}",
                    at.format("%Y-%m-%d %H:%M UTC"),
                    committed_by.as_deref().unwrap_or("-")
                );
            }

            let branches: Vec<(
                String,
                Option<String>,
                Option<i32>,
                Option<String>,
                Option<String>,
                String,
            )> = sqlx::query_as(
                "SELECT branch_name, last_commit_sha, pr_number, pr_state, pr_url, status \
                 FROM project_branches WHERE project_id = $1 \
                 ORDER BY branch_name",
            )
            .bind(&id)
            .fetch_all(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("query branches: {e}"))?;

            if !branches.is_empty() {
                println!();
                println!("{CYAN}Branches ({}){RESET}", branches.len());
                println!(
                    "  {:<30} {:<10} {:<6} {:<8} PR URL",
                    "BRANCH", "SHA", "PR#", "PR STATE"
                );
                for (br, sha, num, st, url, _status) in branches {
                    let sha_s = sha.as_deref().map(|s| &s[..s.len().min(8)]).unwrap_or("-");
                    let num_s = num.map(|n| n.to_string()).unwrap_or_else(|| "-".into());
                    println!(
                        "  {:<30} {:<10} {:<6} {:<8} {}",
                        br,
                        sha_s,
                        num_s,
                        st.as_deref().unwrap_or("-"),
                        url.as_deref().unwrap_or("-"),
                    );
                }
            }

            let envs: Vec<(
                String,
                Option<String>,
                Option<String>,
                Option<chrono::DateTime<chrono::Utc>>,
            )> = sqlx::query_as(
                "SELECT name, deployed_commit_sha, health_status, deployed_at \
                     FROM project_environments WHERE project_id = $1 \
                     ORDER BY name",
            )
            .bind(&id)
            .fetch_all(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("query environments: {e}"))?;

            if !envs.is_empty() {
                println!();
                println!("{CYAN}Environments ({}){RESET}", envs.len());
                for (name, sha, health, deployed_at) in envs {
                    let sha_s = sha.as_deref().map(|s| &s[..s.len().min(8)]).unwrap_or("-");
                    let deployed_s = deployed_at
                        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
                        .unwrap_or_else(|| "-".into());
                    println!(
                        "  {:<14} sha={sha_s:<10} health={} deployed={deployed_s}",
                        name,
                        health.as_deref().unwrap_or("-"),
                    );
                }
            }

            let ci: Vec<(
                String,
                String,
                String,
                Option<chrono::DateTime<chrono::Utc>>,
                Option<String>,
            )> = sqlx::query_as(
                "SELECT branch_name, commit_sha, status, started_at, run_url \
                     FROM project_ci_runs WHERE project_id = $1 \
                     ORDER BY started_at DESC NULLS LAST LIMIT 5",
            )
            .bind(&id)
            .fetch_all(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("query ci runs: {e}"))?;

            if !ci.is_empty() {
                println!();
                println!("{CYAN}Recent CI runs{RESET}");
                for (br, sha, st, at, url) in ci {
                    let sha_s = &sha[..sha.len().min(8)];
                    let at_s = at
                        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| "-".into());
                    println!(
                        "  {:<30} {sha_s:<10} {st:<10} {at_s:<18} {}",
                        br,
                        url.as_deref().unwrap_or("-"),
                    );
                }
            }
        }
        ProjectCommand::Sync { all: _ } => {
            // Today `--all` is the only behavior; we leave the flag in the schema so a
            // future single-project sync can coexist without breaking callers.
            println!("{CYAN}▶ Syncing projects from GitHub...{RESET}");
            let sync = ff_agent::project_github_sync::GitHubSync::new(pool.clone());
            let report = sync
                .sync_all_projects()
                .await
                .map_err(|e| anyhow::anyhow!("github sync: {e}"))?;
            println!("  total:              {}", report.total);
            println!("  main updated:       {}", report.updated_main);
            println!("  branches upserted:  {}", report.branches_upserted);
            println!("  PRs attached:       {}", report.prs_attached);
            println!("  skipped (no repo):  {}", report.skipped_no_repo);
            println!("  skipped (bad url):  {}", report.skipped_bad_url);
            if !report.missing_repos.is_empty() {
                println!(
                    "  {}missing on GitHub:{} {}",
                    YELLOW,
                    RESET,
                    report.missing_repos.join(", ")
                );
            }
            if !report.errors.is_empty() {
                println!("{RED}  errors:{RESET}");
                for (pid, msg) in &report.errors {
                    println!("    [{pid}] {msg}");
                }
            } else {
                println!("{GREEN}✓ Done{RESET}");
            }
        }
    }
    Ok(())
}

async fn handle_onboard(cmd: OnboardCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        OnboardCommand::Show {
            name,
            ip,
            ssh_user,
            role,
            runtime,
        } => {
            // Try to get token from fleet_secrets, fallback to env var.
            let token = ff_agent::fleet_info::fetch_secret("enrollment.shared_secret")
                .await
                .or_else(|| std::env::var("FORGEFLEET_ENROLLMENT_TOKEN").ok())
                .unwrap_or_else(|| "<SET-TOKEN-FIRST>".into());
            let leader =
                std::env::var("FORGEFLEET_LEADER_HOST").unwrap_or_else(|_| "192.168.5.100".into());
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
            println!(
                "{:<15} {:<16} {:<10} {:<6} GH",
                "NAME", "IP", "RUNTIME", "PRIO"
            );
            for n in sorted.into_iter().take(limit as usize) {
                println!(
                    "{:<15} {:<16} {:<10} {:<6} {}",
                    n.name,
                    n.ip,
                    n.runtime,
                    n.election_priority,
                    n.gh_account.clone().unwrap_or_else(|| "-".into())
                );
            }
        }
        OnboardCommand::Revoke { name, yes } => {
            if !yes {
                println!(
                    "This will DELETE fleet_nodes row '{name}', all its SSH keys, and mesh-status rows."
                );
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
            println!(
                "Revoked '{name}': {} ssh keys, {} mesh rows, {} node row(s)",
                removed_keys,
                removed_mesh,
                r.rows_affected()
            );
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
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    // Sub-agent concurrency slots — read fleet_nodes.sub_agent_count for this node.
    let slot_count = ff_db::pg_get_node(&pool, &worker_name)
        .await
        .ok()
        .flatten()
        .map(|n| n.sub_agent_count.max(1) as u32)
        .unwrap_or(1);
    let _ = ff_agent::sub_agents::ensure_workspaces(slot_count);
    let slots = ff_agent::sub_agents::Slots::new(slot_count);

    println!("{CYAN}▶ defer-worker starting{RESET}");
    println!("  node:      {worker_name}");
    println!("  scheduler: {scheduler}");
    println!("  interval:  {interval}s");
    println!(
        "  mode:      {}",
        if once { "single-pass" } else { "continuous" }
    );

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
                let current: std::collections::HashSet<String> = online.iter().cloned().collect();
                let (newly_online, newly_offline) = {
                    let mut prev = last_online.lock().unwrap();
                    let newly_online: Vec<String> = current.difference(&*prev).cloned().collect();
                    let newly_offline: Vec<String> = prev.difference(&current).cloned().collect();
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
                        println!(
                            "{CYAN}[sched]{RESET} promoted {n} task(s) to dispatchable (online: {})",
                            online.join(",")
                        );
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
            let (ok, result, err) = execute_deferred(&claimed, &nodes, Some(&workspace)).await;
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

            // Auto-upgrade finalizer: if this task was an auto-upgrade (or
            // ff fleet upgrade), publish the completion event + ping Telegram
            // and clear the `status='upgrading'` flag in computer_software.
            if let Some(meta) = claimed
                .payload
                .get("meta")
                .and_then(|v| v.get("auto_upgrade"))
            {
                finalize_upgrade_event(&pool2, &claimed, ok, meta, err.as_deref()).await;
            }

            // External-tool finalizer: `ff ext install` / auto drift →
            // install path. Flips computer_external_tools.status and
            // best-effort extracts installed_version from stdout.
            if let Some(meta) = claimed
                .payload
                .get("meta")
                .and_then(|v| v.get("external_tool"))
            {
                finalize_external_tool_event(&pool2, &claimed, ok, meta, err.as_deref()).await;
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
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    // Sub-agent concurrency slots — read fleet_nodes.sub_agent_count for this node.
    let slot_count = ff_db::pg_get_node(&pool, &worker_name)
        .await
        .ok()
        .flatten()
        .map(|n| n.sub_agent_count.max(1) as u32)
        .unwrap_or(1);
    let _ = ff_agent::sub_agents::ensure_workspaces(slot_count);
    let slots = ff_agent::sub_agents::Slots::new(slot_count);

    // Sub-agent DB rows — seed slot 0 for every computer so `ff agent dispatch`
    // has a worker row to claim. Scheduler-only (one node writes).
    if scheduler {
        match ff_agent::agent_coordinator::seed_slot_zero_for_all(&pool).await {
            Ok(n) if n > 0 => println!("{CYAN}[coord]{RESET} seeded {n} new sub_agent row(s)"),
            Ok(_) => {}
            Err(e) => eprintln!("{RED}[coord] seed error: {e}{RESET}"),
        }
    }

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
            )
            .await
            {
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
    // Project GitHub sync: every 5 minutes (leader-only to avoid rate-limit waste).
    let mut gh_sync_tick = tokio::time::interval(Duration::from_secs(5 * 60));
    // Fabric benchmark: every 24h (leader-only). Fires `ff fabric
    // benchmark-all` so `fabric_pairs.measured_bandwidth_gbps` stays
    // fresh across the fleet without operator intervention.
    let mut fabric_tick = tokio::time::interval(Duration::from_secs(24 * 3600));
    // OAuth probe: every 6h (leader-only). Hits each oauth_subscription
    // provider's /v1/models with the harvested token and logs the
    // result. Catches token expiry before the next inference call
    // surfaces it as a 401 to a user.
    let mut oauth_tick = tokio::time::interval(Duration::from_secs(6 * 3600));
    // First tick fires immediately for each — prime all nine.
    defer_tick.tick().await;
    disk_tick.tick().await;
    recon_tick.tick().await;
    sweep_tick.tick().await;
    version_tick.tick().await;
    brain_tick.tick().await;
    gh_sync_tick.tick().await;
    fabric_tick.tick().await;
    oauth_tick.tick().await;

    // Do an initial pass immediately on startup.
    let _ = defer_pass(&pool, &worker_name, scheduler, &slots).await;
    // Initial version check on daemon startup so operators see data within
    // seconds instead of waiting 6 hours for the first tick.
    match ff_agent::version_check::version_check_pass(&pool).await {
        Ok(s) if !s.drifted_keys.is_empty() => println!(
            "{CYAN}[versions]{RESET} drift: {}",
            s.drifted_keys.join(", ")
        ),
        Ok(s) => println!(
            "{CYAN}[versions]{RESET} initial pass: {} tools ✓",
            s.total_keys
        ),
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

    // ─── Phase 7: model portfolio intelligence ──────────────────────────
    // These three long-lived loops only run on the elected leader so we
    // don't burn HF API quota from every box. Non-leaders skip the spawn
    // entirely and rely on the leader to keep the catalog + coverage fresh.
    let (_portfolio_shutdown_tx, portfolio_shutdown_rx) = tokio::sync::watch::channel(false);

    // Local self-healer — runs on EVERY host (not leader-gated) so each
    // box restarts its own forgefleetd if it dies. Closes the split-brain
    // window where `ff daemon` keeps updating leader heartbeat while
    // forgefleetd is dead and peers have no reason to fail over.
    println!(
        "{CYAN}[healer]{RESET} spawning local forgefleetd self-healer (30s interval, 60s kickoff)"
    );
    let healer = ff_agent::local_healer::LocalHealer::new(worker_name.clone());
    let _healer_handle = healer.spawn(portfolio_shutdown_rx.clone());

    let is_leader = ff_db::pg_get_current_leader(&pool)
        .await
        .ok()
        .flatten()
        .map(|l| l.member_name == worker_name)
        .unwrap_or(false);
    if scheduler || is_leader {
        println!(
            "{CYAN}[portfolio]{RESET} spawning model-upstream (24h) + coverage-guard (15min) + scout (168h)"
        );
        let upstream = ff_agent::model_upstream::ModelUpstreamChecker::new(pool.clone());
        let _upstream_handle = upstream.spawn(24, portfolio_shutdown_rx.clone());

        let guard = ff_agent::coverage_guard::CoverageGuard::new_dbonly(pool.clone());
        let _guard_handle = guard.spawn(15, portfolio_shutdown_rx.clone());

        let scout = ff_agent::model_scout::ModelScout::new(pool.clone());
        let _scout_handle = scout.spawn(168, portfolio_shutdown_rx.clone());

        // Hourly auto-upgrade loop: dispatches drift → playbook → Telegram
        // without operator interaction. Gated by fleet_secrets.auto_upgrade_enabled.
        println!("{CYAN}[auto-upgrade]{RESET} spawning hourly drift→upgrade→telegram loop");
        let auto = ff_agent::auto_upgrade::AutoUpgradeTick::new(pool.clone(), worker_name.clone());
        let _auto_handle = auto.spawn(portfolio_shutdown_rx.clone());

        // External-tools upstream drift checker (6h). Scans the V24
        // `external_tools` catalog for new GitHub releases / brew / pip
        // versions and flips `computer_external_tools.status` rows to
        // `'upgrade_available'`. Pure detector — install dispatch is a
        // separate concern (see `ff ext install`).
        println!("{CYAN}[ext-upstream]{RESET} spawning 6h external-tools upstream checker");
        let ext_upstream =
            ff_agent::external_tools_upstream::ExternalToolsUpstreamChecker::new(pool.clone());
        let _ext_upstream_handle = ext_upstream.spawn(6, portfolio_shutdown_rx.clone());

        // Stuck-slot reaper: resets sub_agents rows stuck in 'error' or 'busy'
        // with a stale started_at so the dispatch queue can't lock up.
        println!(
            "{CYAN}[reaper]{RESET} spawning stuck-slot reaper (10min interval, 10min timeout)"
        );
        let reaper =
            ff_agent::sub_agent_reaper::SubAgentReaper::new(pool.clone(), worker_name.clone());
        let _reaper_handle = reaper.spawn(portfolio_shutdown_rx.clone());
    } else {
        println!("{CYAN}[portfolio]{RESET} skipping — not leader / scheduler");
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
            _ = gh_sync_tick.tick(), if scheduler => {
                let sync = ff_agent::project_github_sync::GitHubSync::new(pool.clone());
                match sync.sync_all_projects().await {
                    Ok(r) if r.updated_main > 0 || !r.errors.is_empty() => println!(
                        "{CYAN}[projects]{RESET} gh sync: {} main updated, {} branches, {} PRs, {} errors",
                        r.updated_main, r.branches_upserted, r.prs_attached, r.errors.len()),
                    Ok(_) => {}
                    Err(e) => eprintln!("{RED}[projects] gh sync error: {e}{RESET}"),
                }
            }
            _ = fabric_tick.tick(), if scheduler => {
                // Short duration (5s) — sweeping every pair, not benchmarking
                // throughput exhaustively. Operators run the full 30s probe
                // manually via `ff fabric benchmark <a> <b>` when needed.
                match fabric_cmd::handle_fabric_benchmark_all(&pool, 5, 1).await {
                    Ok(()) => println!("{CYAN}[fabric]{RESET} 24h benchmark sweep complete"),
                    Err(e) => eprintln!("{RED}[fabric] sweep error: {e}{RESET}"),
                }
            }
            _ = oauth_tick.tick(), if scheduler => {
                let results = ff_agent::oauth_distributor::probe_all(&pool).await;
                let mut bad = 0usize;
                for r in &results {
                    match r.status.as_str() {
                        "ok" => tracing::debug!(provider = %r.provider, "oauth_probe ok"),
                        "no_token" => tracing::debug!(
                            provider = %r.provider, "oauth_probe: no token configured"
                        ),
                        "unauthorized" | "forbidden" => {
                            tracing::error!(
                                provider = %r.provider,
                                status = %r.status,
                                http = ?r.http_status,
                                "oauth_probe: token rejected — re-import via `ff oauth import {} && ff oauth distribute {}`",
                                r.provider, r.provider
                            );
                            bad += 1;
                        }
                        _ => {
                            tracing::warn!(
                                provider = %r.provider,
                                status = %r.status,
                                http = ?r.http_status,
                                msg = ?r.message,
                                "oauth_probe: unexpected status"
                            );
                            bad += 1;
                        }
                    }
                }
                if bad > 0 {
                    println!(
                        "{YELLOW}[oauth]{RESET} probe: {}/{} provider(s) need attention — see logs",
                        bad,
                        results.len(),
                    );
                }
            }
        }
    }
}

async fn handle_task(cmd: TaskCommand, _config_path: &Path) -> Result<()> {
    // Tasks live in the agent in-memory store, exposed via the agent HTTP server on :50002.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let base = "http://127.0.0.1:50002";

    match cmd {
        TaskCommand::List { status, limit } => {
            let resp = client.get(format!("{base}/tasks")).send().await;
            let body = match resp {
                Ok(r) => r.json::<serde_json::Value>().await.unwrap_or_default(),
                Err(e) => {
                    println!(
                        "{RED}✗ Cannot reach agent HTTP server (is forgefleetd running?): {e}{RESET}"
                    );
                    return Ok(());
                }
            };

            let empty = vec![];
            let all_tasks = body
                .get("tasks")
                .and_then(|v| v.as_array())
                .unwrap_or(&empty);
            let tasks: Vec<&serde_json::Value> = all_tasks
                .iter()
                .filter(|t| {
                    if let Some(ref s) = status {
                        t.get("status").and_then(|v| v.as_str()) == Some(s.as_str())
                    } else {
                        true
                    }
                })
                .take(limit as usize)
                .collect();

            if tasks.is_empty() {
                println!("{YELLOW}No tasks found{RESET}");
                return Ok(());
            }

            println!("{GREEN}✓ Tasks ({} shown){RESET}", tasks.len());
            println!(
                "  {:<6} {:<40} {:<12} {:<16} CREATED",
                "ID", "SUBJECT", "STATUS", "NODE"
            );
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
                let short_subject = truncate_str(subject, 39);
                let short_created = truncate_str(created, 19);
                println!(
                    "  {id:<6} {short_subject:<40} {status_color}{status_str:<12}{RESET} {node:<16} {short_created}"
                );
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
            let task = body
                .get("tasks")
                .and_then(|v| v.as_array())
                .unwrap_or(&empty)
                .iter()
                .find(|t| {
                    t.get("id")
                        .and_then(|v| v.as_str())
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
                    if let Some(output) = t.get("output").and_then(|v| v.as_str())
                        && !output.is_empty()
                    {
                        println!("\n  Output:\n    {}", truncate_str(output, 500));
                    }
                }
            }
        }
        TaskCommand::Update { id, status } => {
            // POST a status update via the agent message endpoint
            let valid = ["pending", "in_progress", "completed", "failed", "cancelled"];
            if !valid.contains(&status.as_str()) {
                println!(
                    "{RED}✗ Invalid status '{status}'. Valid: {}{RESET}",
                    valid.join(", ")
                );
                return Ok(());
            }
            let payload = serde_json::json!({
                "task_id": id,
                "status": status,
                "output": "",
                "from": "ff-cli",
            });
            let r = client
                .post(format!("{base}/agent/message"))
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
        ConfigCommand::Show => {
            let c = load_config(p)?;
            println!("{}", toml::to_string_pretty(&c)?.trim_end());
            Ok(())
        }
        ConfigCommand::Set { key, value } => {
            let mut c = load_config(p)?;
            let v = value
                .parse::<toml::Value>()
                .unwrap_or(toml::Value::String(value.clone()));
            let parts: Vec<&str> = key.split('.').collect();
            if parts.len() < 2 {
                anyhow::bail!("Key must be dotted: section.key");
            }
            match parts[0] {
                "general" => {
                    c.general.insert(parts[1..].join("."), v);
                }
                "nodes" => {
                    c.nodes.insert(parts[1..].join("."), v);
                }
                _ => {
                    c.extra.insert(key.clone(), v);
                }
            }
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(p, toml::to_string_pretty(&c)?)?;
            println!("{GREEN}✓{RESET} {key}={value}");
            Ok(())
        }
        ConfigCommand::Nodes => {
            let pool = ff_agent::fleet_info::get_fleet_pool()
                .await
                .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
            ff_db::run_postgres_migrations(&pool)
                .await
                .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;
            let nodes = ff_db::pg_list_nodes(&pool).await?;
            if nodes.is_empty() {
                println!("(no fleet nodes registered)");
                return Ok(());
            }
            println!(
                "{:<12} {:<12} {:<24} {:>14}",
                "NODE", "RUNTIME", "MODELS_DIR", "DISK_QUOTA_PCT"
            );
            for n in &nodes {
                println!(
                    "{:<12} {:<12} {:<24} {:>14}",
                    n.name, n.runtime, n.models_dir, n.disk_quota_pct
                );
            }
            Ok(())
        }
        ConfigCommand::Node { name, key, value } => {
            let pool = ff_agent::fleet_info::get_fleet_pool()
                .await
                .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
            ff_db::run_postgres_migrations(&pool)
                .await
                .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;
            let mut row = ff_db::pg_get_node(&pool, &name)
                .await?
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
                    if value.trim().is_empty() {
                        anyhow::bail!("models_dir must be non-empty");
                    }
                    row.models_dir = value.clone();
                }
                "disk_quota_pct" => {
                    let n: i32 = value
                        .parse()
                        .map_err(|_| anyhow::anyhow!("disk_quota_pct must be an integer 1-100"))?;
                    if !(1..=100).contains(&n) {
                        anyhow::bail!("disk_quota_pct must be between 1 and 100");
                    }
                    row.disk_quota_pct = n;
                }
                _ => anyhow::bail!(
                    "unsupported key '{key}' (use runtime, models_dir, or disk_quota_pct)"
                ),
            }
            ff_db::pg_upsert_node(&pool, &row).await?;
            println!("{GREEN}✓{RESET} Updated {name}.{key} = {value}");
            Ok(())
        }
    }
}

// ─── Phase 10: alerts / metrics / logs ─────────────────────────────────

async fn handle_alert(cmd: AlertCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        AlertCommand::List => {
            let rows = sqlx::query_as::<
                _,
                (
                    uuid::Uuid,
                    String,
                    Option<String>,
                    String,
                    String,
                    String,
                    i32,
                    String,
                    i32,
                    String,
                    bool,
                ),
            >(
                "SELECT id, name, description, metric, scope, condition,
                        duration_secs, severity, cooldown_secs, channel, enabled
                 FROM alert_policies
                 ORDER BY name",
            )
            .fetch_all(&pool)
            .await?;

            if rows.is_empty() {
                println!("(no alert policies — run `ff alert policy seed`)");
                return Ok(());
            }
            println!(
                "{:<28} {:<10} {:<22} {:<15} {:<15} {:<10} {:<5}",
                "NAME", "SEVERITY", "METRIC", "CONDITION", "SCOPE", "CHANNEL", "ON?"
            );
            for (
                _id,
                name,
                _desc,
                metric,
                scope,
                condition,
                _duration,
                severity,
                _cooldown,
                channel,
                enabled,
            ) in rows
            {
                println!(
                    "{:<28} {:<10} {:<22} {:<15} {:<15} {:<10} {:<5}",
                    name,
                    severity,
                    metric,
                    condition,
                    scope,
                    channel,
                    if enabled { "yes" } else { "no" }
                );
            }
        }
        AlertCommand::Events { active, limit } => {
            let sql = if active {
                "SELECT e.id, p.name, c.name, e.fired_at, e.resolved_at,
                        e.value, e.value_text, e.message, e.channel_result
                 FROM alert_events e
                 JOIN alert_policies p ON p.id = e.policy_id
                 LEFT JOIN computers c ON c.id = e.computer_id
                 WHERE e.resolved_at IS NULL
                 ORDER BY e.fired_at DESC
                 LIMIT $1"
            } else {
                "SELECT e.id, p.name, c.name, e.fired_at, e.resolved_at,
                        e.value, e.value_text, e.message, e.channel_result
                 FROM alert_events e
                 JOIN alert_policies p ON p.id = e.policy_id
                 LEFT JOIN computers c ON c.id = e.computer_id
                 ORDER BY e.fired_at DESC
                 LIMIT $1"
            };

            let rows = sqlx::query_as::<
                _,
                (
                    uuid::Uuid,
                    String,
                    Option<String>,
                    chrono::DateTime<chrono::Utc>,
                    Option<chrono::DateTime<chrono::Utc>>,
                    Option<f64>,
                    Option<String>,
                    Option<String>,
                    Option<String>,
                ),
            >(sql)
            .bind(limit)
            .fetch_all(&pool)
            .await?;

            if rows.is_empty() {
                if active {
                    println!("(no active alerts)");
                } else {
                    println!("(no alert events recorded yet)");
                }
                return Ok(());
            }
            println!(
                "{:<20} {:<18} {:<12} {:<10} MESSAGE",
                "FIRED", "POLICY", "COMPUTER", "STATE"
            );
            for (_id, policy, computer, fired_at, resolved_at, _v, _vt, message, _cr) in rows {
                let state = if resolved_at.is_some() {
                    "resolved"
                } else {
                    "firing"
                };
                println!(
                    "{:<20} {:<18} {:<12} {:<10} {}",
                    fired_at.format("%Y-%m-%d %H:%M:%S"),
                    truncate_str(&policy, 18),
                    truncate_str(&computer.unwrap_or_else(|| "-".into()), 12),
                    state,
                    message.unwrap_or_default()
                );
            }
        }
    }
    Ok(())
}




/// Parse a duration like "5m", "1h", "24h", "30s" into seconds.


// ─── Phase 12: storage / power / train / benchmark handlers ────────────────

async fn handle_storage(cmd: StorageCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    let mgr = ff_agent::shared_storage::SharedStorageManager::new(pool.clone());

    match cmd {
        StorageCommand::Share { command } => match command {
            StorageShareCommand::Create {
                name,
                host,
                path,
                mount_path,
                purpose,
                read_only,
            } => {
                let mp = mount_path.unwrap_or_else(|| path.clone());
                let id = mgr
                    .create_share(&name, &host, &path, &mp, purpose.as_deref(), read_only)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                println!("{GREEN}✓ Registered shared volume {name}{RESET}");
                println!("  id:            {id}");
                println!("  host:          {host}");
                println!("  export_path:   {path}");
                println!("  mount_path:    {mp}");
                if let Some(p) = purpose {
                    println!("  purpose:       {p}");
                }
                if read_only {
                    println!("  read_only:     true");
                }
                println!();
                println!("NOTE: /etc/exports and NFS daemon setup are best-effort and");
                println!("      may require manual configuration on the host. See");
                println!("      `ff_agent::shared_storage` module docs for the exact");
                println!("      per-OS commands.");
                Ok(())
            }
            StorageShareCommand::Mount {
                name,
                computer,
                path,
            } => match mgr.mount(&name, &computer, path.as_deref()).await {
                Ok(mp) => {
                    println!("{GREEN}✓ Mounted {name} on {computer} at {mp}{RESET}");
                    Ok(())
                }
                Err(e) => {
                    eprintln!("{RED}✗ Mount failed: {e}{RESET}");
                    std::process::exit(1);
                }
            },
            StorageShareCommand::Unmount { name, computer } => {
                match mgr.unmount(&name, &computer).await {
                    Ok(()) => {
                        println!("{GREEN}✓ Unmounted {name} on {computer}{RESET}");
                        Ok(())
                    }
                    Err(e) => {
                        eprintln!("{RED}✗ Unmount failed: {e}{RESET}");
                        std::process::exit(1);
                    }
                }
            }
            StorageShareCommand::List => {
                let shares = mgr.list().await.map_err(|e| anyhow::anyhow!("{e}"))?;
                if shares.is_empty() {
                    println!("(no shared volumes registered)");
                    return Ok(());
                }
                println!(
                    "{:<18} {:<10} {:<22} {:<18} {:<7} MOUNTS",
                    "NAME", "HOST", "EXPORT", "PURPOSE", "RO"
                );
                for s in shares {
                    let mounts = if s.mounts.is_empty() {
                        "-".to_string()
                    } else {
                        s.mounts
                            .iter()
                            .map(|(c, st)| format!("{c}({st})"))
                            .collect::<Vec<_>>()
                            .join(",")
                    };
                    println!(
                        "{:<18} {:<10} {:<22} {:<18} {:<7} {}",
                        truncate_str(&s.name, 18),
                        truncate_str(&s.host, 10),
                        truncate_str(&s.export_path, 22),
                        truncate_str(s.purpose.as_deref().unwrap_or("-"), 18),
                        if s.read_only { "yes" } else { "no" },
                        mounts
                    );
                }
                Ok(())
            }
        },
    }
}

async fn handle_power(cmd: PowerCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        PowerCommand::Schedule { command } => match command {
            PowerScheduleCommand::Create {
                computer,
                kind,
                cron,
                if_idle,
            } => {
                let computer_id = sqlx::query_scalar::<_, sqlx::types::Uuid>(
                    "SELECT id FROM computers WHERE name = $1",
                )
                .bind(&computer)
                .fetch_optional(&pool)
                .await?
                .ok_or_else(|| anyhow::anyhow!("computer '{computer}' not found"))?;

                let condition = if_idle.map(|m| format!("idle_minutes > {m}"));
                let id = ff_db::pg_create_schedule(
                    &pool,
                    computer_id,
                    &kind,
                    &cron,
                    condition.as_deref(),
                    Some(&whoami_tag()),
                )
                .await?;
                println!("{GREEN}✓ Created schedule {id}{RESET}");
                println!("  computer:   {computer}");
                println!("  kind:       {kind}");
                println!("  cron:       {cron}");
                if let Some(c) = condition {
                    println!("  condition:  {c}");
                }
                Ok(())
            }
            PowerScheduleCommand::Delete { id } => {
                let uuid = sqlx::types::Uuid::parse_str(&id)
                    .map_err(|e| anyhow::anyhow!("bad uuid: {e}"))?;
                if ff_db::pg_delete_schedule(&pool, uuid).await? {
                    println!("{GREEN}✓ Deleted schedule {id}{RESET}");
                } else {
                    println!("No schedule with id '{id}'");
                }
                Ok(())
            }
        },
        PowerCommand::Schedules { computer } => {
            let computer_id = if let Some(c) = computer {
                Some(
                    sqlx::query_scalar::<_, sqlx::types::Uuid>(
                        "SELECT id FROM computers WHERE name = $1",
                    )
                    .bind(&c)
                    .fetch_optional(&pool)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("computer '{c}' not found"))?,
                )
            } else {
                None
            };
            let rows = ff_db::pg_list_schedules(&pool, computer_id, false).await?;
            if rows.is_empty() {
                println!("(no power schedules)");
                return Ok(());
            }
            println!(
                "{:<38} {:<10} {:<9} {:<18} {:<10} LAST",
                "ID", "COMPUTER", "KIND", "CRON", "ENABLED"
            );
            for r in rows {
                let last = r
                    .last_fired_at
                    .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
                    .unwrap_or_else(|| "-".into());
                println!(
                    "{:<38} {:<10} {:<9} {:<18} {:<10} {}",
                    r.id,
                    r.computer_name.unwrap_or_else(|| "?".into()),
                    r.kind,
                    r.cron_expr,
                    if r.enabled { "yes" } else { "no" },
                    last
                );
            }
            Ok(())
        }
        PowerCommand::Tick => {
            let sched = ff_agent::power_scheduler::PowerScheduler::new(pool.clone());
            let actions = sched
                .evaluate_once()
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if actions.is_empty() {
                println!("(no schedules matched this minute)");
            } else {
                for a in actions {
                    println!("{:<14} {:<9} {}", a.computer_name, a.kind, a.result);
                }
            }
            Ok(())
        }
    }
}

async fn handle_train(cmd: TrainCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    let orch = ff_agent::training_orchestrator::TrainingOrchestrator::new(pool.clone());

    match cmd {
        TrainCommand::Create {
            name,
            base,
            dataset,
            output,
            training_type,
            computer,
            epochs,
            learning_rate,
            batch_size,
            lora_rank,
            max_seq_len,
        } => {
            let spec = ff_agent::training_orchestrator::TrainingJobSpec {
                name: name.clone(),
                base_model_id: base,
                training_data_path: dataset,
                adapter_output_path: output,
                training_type,
                computer_name: computer,
                epochs,
                learning_rate,
                batch_size,
                lora_rank,
                max_seq_len,
                created_by: Some(whoami_tag()),
            };
            let id = orch
                .create_job(spec)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("{GREEN}✓ Created training job {id}{RESET}");
            println!("  name:   {name}");
            println!("  status: queued");
            println!();
            println!("Start it with: ff train start {id}");
            Ok(())
        }
        TrainCommand::Start { id } => {
            let uuid =
                sqlx::types::Uuid::parse_str(&id).map_err(|e| anyhow::anyhow!("bad uuid: {e}"))?;
            let deferred = orch
                .start_job(uuid)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("{GREEN}✓ Training job {id} dispatched{RESET}");
            println!("  deferred_task: {deferred}");
            Ok(())
        }
        TrainCommand::List { status, limit } => {
            let rows = ff_db::pg_list_training_jobs(&pool, status.as_deref(), limit).await?;
            if rows.is_empty() {
                println!("(no training jobs)");
                return Ok(());
            }
            println!(
                "{:<38} {:<22} {:<12} {:<10} {:<10} CREATED",
                "ID", "NAME", "STATUS", "TYPE", "COMPUTER"
            );
            for r in rows {
                let created = r.created_at.format("%Y-%m-%d %H:%M").to_string();
                println!(
                    "{:<38} {:<22} {:<12} {:<10} {:<10} {}",
                    r.id,
                    truncate_str(&r.name, 22),
                    r.status,
                    r.training_type,
                    truncate_str(r.computer_name.as_deref().unwrap_or("-"), 10),
                    created
                );
            }
            Ok(())
        }
        TrainCommand::Show { id } => {
            let uuid =
                sqlx::types::Uuid::parse_str(&id).map_err(|e| anyhow::anyhow!("bad uuid: {e}"))?;
            match ff_db::pg_get_training_job(&pool, uuid).await? {
                Some(r) => {
                    println!("ID:              {}", r.id);
                    println!("Name:            {}", r.name);
                    println!("Status:          {}", r.status);
                    println!("Type:            {}", r.training_type);
                    println!(
                        "Base model:      {}",
                        r.base_model_id.unwrap_or_else(|| "-".into())
                    );
                    println!("Dataset:         {}", r.training_data_path);
                    if let Some(out) = r.adapter_output_path {
                        println!("Adapter output:  {out}");
                    }
                    if let Some(c) = r.computer_name {
                        println!("Computer:        {c}");
                    }
                    if let Some(t) = r.started_at {
                        println!("Started:         {}", t.format("%Y-%m-%d %H:%M UTC"));
                    }
                    if let Some(t) = r.completed_at {
                        println!("Completed:       {}", t.format("%Y-%m-%d %H:%M UTC"));
                    }
                    if let Some(deferred) = r.deferred_task_id {
                        println!("Deferred task:   {deferred}");
                    }
                    if let Some(err) = r.error_message {
                        println!("Error:           {err}");
                    }
                    if let Some(rm) = r.result_model_id {
                        println!("Result model:    {rm}");
                    }
                    let loss_samples = r.loss_curve.as_array().map(|a| a.len()).unwrap_or(0);
                    println!("Loss samples:    {loss_samples}");
                    println!(
                        "Params:\n{}",
                        serde_json::to_string_pretty(&r.params).unwrap_or_default()
                    );
                }
                None => {
                    eprintln!("No training job with id '{id}'");
                    std::process::exit(1);
                }
            }
            Ok(())
        }
    }
}

/// Handler for `ff research "<query>"`.
///
/// Opens a Postgres pool, runs migrations (V42 lands the research tables),
/// constructs a [`ResearchConfig`], spins a [`ResearchSession`], and
/// streams progress to stderr while the planner → parallel sub-agents →
/// synthesizer pipeline runs. Final markdown is printed to stdout (and
/// optionally written to `--output path`).
async fn handle_research(
    prompt: &str,
    parallel: u32,
    depth: u32,
    output: Option<PathBuf>,
    gateway: Option<String>,
    planner_model: Option<String>,
    subagent_model: Option<String>,
    verbose: bool,
) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    let config = ff_agent::research::ResearchConfig {
        query: prompt.to_string(),
        parallel,
        depth,
        output_path: output,
        gateway_url: gateway.unwrap_or_default(),
        planner_model: planner_model.unwrap_or_default(),
        subagent_model: subagent_model.unwrap_or_default(),
        ..Default::default()
    };

    eprintln!(
        "{CYAN}▶ ff research{RESET}  \x1b[2mparallel={parallel} depth={depth} \
         planner={} subagent={}{RESET}",
        config.planner_model, config.subagent_model
    );
    eprintln!("\x1b[2m  Query: {}{RESET}\n", prompt);

    let session = ff_agent::research::ResearchSession::new(pool, config)
        .await
        .map_err(|e| anyhow::anyhow!("create research_session: {e}"))?;
    eprintln!("\x1b[2m  Session: {}{RESET}", session.id());

    // Progress channel: dump key events to stderr so the operator sees
    // forward motion without scrolling through raw LLM output.
    let (prog_tx, mut prog_rx) = tokio::sync::mpsc::unbounded_channel();
    let verbose_flag = verbose;
    let progress_task = tokio::spawn(async move {
        while let Some(ev) = prog_rx.recv().await {
            use ff_agent::research::ResearchProgress;
            match ev {
                ResearchProgress::Planning { query } => {
                    eprintln!(
                        "{CYAN}[planner]{RESET} decomposing: {}",
                        truncate_str(&query, 80)
                    );
                }
                ResearchProgress::Dispatching { sub_count } => {
                    eprintln!("{CYAN}[dispatch]{RESET} {sub_count} sub-agents running in parallel");
                }
                ResearchProgress::Synthesizing => {
                    eprintln!("{CYAN}[synthesizer]{RESET} merging sub-agent outputs");
                }
                ResearchProgress::Event(ev) if verbose_flag => {
                    eprintln!("\x1b[2m  · {ev:?}\x1b[0m");
                }
                ResearchProgress::Event(_) => {}
            }
        }
    });

    let report = session
        .run(Some(prog_tx))
        .await
        .map_err(|e| anyhow::anyhow!("research run: {e}"))?;
    let _ = progress_task.await;

    eprintln!();
    eprintln!(
        "{GREEN}✓ research complete{RESET}  \x1b[2m{}/{} sub-agents succeeded · {}ms · \
         session {}{RESET}",
        report.subtasks_succeeded, report.subtask_count, report.duration_ms, report.session_id,
    );
    eprintln!();
    println!("{}", report.markdown);
    Ok(())
}
