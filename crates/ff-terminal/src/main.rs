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
mod fabric_cmd;
mod model_serve_cmd;
mod self_heal_cmd;
mod storage_cmd;
mod tasks_cmd;

const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

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
enum DeferCommand {
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
enum FleetDbCommand {
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
enum TaskCoverageCommand {
    /// Show the current fleet_task_coverage table.
    #[command(alias = "ls")]
    List,
}

#[derive(Debug, Clone, Subcommand)]
enum LlmCommand {
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
enum SocialCommand {
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
enum OpenclawCommand {
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
enum OpenclawDevicesCommand {
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
enum ModelCommand {
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
enum MetricsCommand {
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
        /// + audit log so a future operator can see why the switch was
        /// flipped.
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
enum EventsCommand {
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
        Some(Command::Secrets { command }) => return handle_secrets(command.clone()).await,
        Some(Command::Defer { command }) => return handle_defer(command.clone()).await,
        Some(Command::Model { command }) => return handle_model(command.clone()).await,
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
        Some(Command::Versions { node }) => return handle_versions(node.clone()).await,
        Some(Command::Fleet { command }) => return handle_fleet(command.clone()).await,
        Some(Command::Llm { command }) => return handle_llm(command.clone()).await,
        Some(Command::Software { command }) => return handle_software(command.clone()).await,
        Some(Command::Ext { command }) => return handle_ext(command.clone()).await,
        Some(Command::Onboard { command }) => return handle_onboard(command.clone()).await,
        Some(Command::VirtualBrain { command }) => return handle_brain(command.clone()).await,
        Some(Command::Openclaw { command }) => return handle_openclaw(command.clone()).await,
        Some(Command::Pm { command }) => return handle_pm(command.clone()).await,
        Some(Command::Agent { command }) => return handle_agent(command.clone()).await,
        Some(Command::Project { command }) => {
            return handle_project(command.clone()).await;
        }
        Some(Command::Alert { command }) => return handle_alert(command.clone()).await,
        Some(Command::Metrics { command }) => return handle_metrics(command.clone()).await,
        Some(Command::Logs {
            computer,
            service,
            tail,
        }) => {
            return handle_logs(computer.clone(), service.clone(), *tail).await;
        }
        Some(Command::Events { command }) => return handle_events(command.clone()).await,
        Some(Command::Storage { command }) => return handle_storage(command.clone()).await,
        Some(Command::Power { command }) => return handle_power(command.clone()).await,
        Some(Command::Train { command }) => return handle_train(command.clone()).await,
        Some(Command::Ports { command }) => return handle_ports(command.clone()).await,
        Some(Command::CloudLlm { command }) => return handle_cloud_llm(command.clone()).await,
        Some(Command::Social { command }) => return handle_social(command.clone()).await,
        _ => {}
    }

    // Build the local-first inference router (probes localhost + fleet from DB).
    // If the user explicitly passed --llm, skip auto-routing and use that URL directly.
    let (llm, router) =
        if let Some(explicit_url) = cli.llm.or_else(|| env::var("FORGEFLEET_LLM_URL").ok()) {
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
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    if let Some(id) = body
                        .get("data")
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
        Some(Command::Health) => handle_health(&agent_config).await,
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
            run_headless(&prompt, cfg, &output, oneshot).await
        }
        Some(Command::Task { command }) => handle_task(command, &config_path).await,
        Some(Command::Secrets { command }) => handle_secrets(command).await,
        Some(Command::Defer { command }) => handle_defer(command).await,
        Some(Command::Model { command }) => handle_model(command).await,
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
        Some(Command::Versions { node }) => handle_versions(node).await,
        Some(Command::Fleet { command }) => handle_fleet(command).await,
        Some(Command::Llm { command }) => handle_llm(command).await,
        Some(Command::Software { command }) => handle_software(command).await,
        Some(Command::Ext { command }) => handle_ext(command).await,
        Some(Command::Onboard { command }) => handle_onboard(command).await,
        Some(Command::VirtualBrain { command }) => handle_brain(command).await,
        Some(Command::Openclaw { command }) => handle_openclaw(command).await,
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
                        "{:<10} {:<14} {:<18} {:<10} {}",
                        "PROVIDER", "CRED FILE", "FILE MTIME", "IN SECRETS", "TOKEN PREVIEW"
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
                    println!(
                        "{:<10} {:<14} {:<5} {}",
                        "provider", "status", "code", "detail"
                    );
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
            }
        }
        Some(Command::Alert { command }) => handle_alert(command).await,
        Some(Command::Metrics { command }) => handle_metrics(command).await,
        Some(Command::Logs {
            computer,
            service,
            tail,
        }) => handle_logs(computer, service, tail).await,
        Some(Command::Events { command }) => handle_events(command).await,
        Some(Command::Storage { command }) => handle_storage(command).await,
        Some(Command::Power { command }) => handle_power(command).await,
        Some(Command::Train { command }) => handle_train(command).await,
        Some(Command::Ports { command }) => handle_ports(command).await,
        Some(Command::CloudLlm { command }) => handle_cloud_llm(command).await,
        Some(Command::Social { command }) => handle_social(command).await,
        Some(Command::Supervise {
            prompt,
            max_attempts,
            verify_files,
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
                    let missing: Vec<_> = verify_files
                        .iter()
                        .filter(|p| match std::fs::metadata(p) {
                            Ok(m) => !m.is_file() || m.len() == 0,
                            Err(_) => true,
                        })
                        .collect();
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
                                if output.starts_with("PUSH:") {
                                    let topic = &output[5..];
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
                                } else if output.starts_with("BACKLOG_ADD:") {
                                    let item = &output[12..];
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

    if !key.is_empty() {
        if let Some(val) = v.get(key).and_then(|v| v.as_str()) {
            return truncate_str(val, 60).replace('\n', " ");
        }
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
    let toml_str =
        std::fs::read_to_string(&config_path).map_err(|e| format!("read fleet.toml: {e}"))?;
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
        if let Some((n, _, _, _)) = a.deploy.as_ref() {
            if !nodes_v.contains(n) {
                nodes_v.push(n.clone());
            }
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
fn human_bytes_i64(n: i64) -> String {
    if n < 0 {
        return "0 B".into();
    }
    human_bytes(n as u64)
}

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
    } else if let ff_agent::agent_loop::AgentOutcome::EndTurn { final_message } = &outcome {
        if !final_message.is_empty() {
            println!("{final_message}");
        }
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
                            if let Ok(addr) = format!("{ip}:{port}").parse() {
                                if std::net::TcpStream::connect_timeout(
                                    &addr,
                                    Duration::from_millis(200),
                                )
                                .is_ok()
                                {
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
    {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
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

async fn handle_health(c: &AgentSessionConfig) -> Result<()> {
    let nodes = load_fleet_nodes_for_health(c).await;
    let client = std::sync::Arc::new(
        reqwest::Client::builder()
            .timeout(Duration::from_millis(2500))
            .build()?,
    );

    // Check all nodes in parallel
    let futs: Vec<_> = nodes
        .iter()
        .map(|(name, ip, port)| {
            let client = client.clone();
            let url = format!("http://{ip}:{port}/health");
            let agent_url = format!("http://{ip}:50002/health");
            let name = name.clone();
            let ip = ip.clone();
            let port = *port;
            async move {
                let daemon_ok = client
                    .get(&url)
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false);
                let agent_ok = client
                    .get(&agent_url)
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false);
                (name, ip, port, daemon_ok, agent_ok)
            }
        })
        .collect();

    let results = futures::future::join_all(futs).await;

    println!("{GREEN}✓ ForgeFleet Health{RESET}");
    for (name, ip, port, daemon_ok, agent_ok) in results {
        let daemon_str = if daemon_ok {
            format!("{GREEN}ONLINE{RESET}")
        } else {
            format!("{RED}OFFLINE{RESET}")
        };
        let agent_str = if agent_ok {
            format!("  agent{GREEN}✓{RESET}")
        } else {
            format!("  agent{YELLOW}✗{RESET}")
        };
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
                        "SELECT name, ip FROM fleet_nodes ORDER BY election_priority, name",
                    )
                    .fetch_all(&pool)
                    .await
                    .unwrap_or_default();

                    if !rows.is_empty() {
                        return rows.into_iter().map(|(n, ip)| (n, ip, 51000u16)).collect();
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
        ("Priya".into(), "192.168.5.104".into(), 51000),
        ("James".into(), "192.168.5.108".into(), 51000),
        ("Logan".into(), "192.168.5.111".into(), 51000),
        ("Lily".into(), "192.168.5.113".into(), 51000),
        ("Veronica".into(), "192.168.5.112".into(), 51000),
        ("Duncan".into(), "192.168.5.114".into(), 51000),
        ("Aura".into(), "192.168.5.110".into(), 51000),
    ]
}

async fn handle_secrets(cmd: SecretsCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    // Ensure secrets table + other Postgres migrations are applied. Idempotent.
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;
    match cmd {
        SecretsCommand::List => {
            let rows = ff_db::pg_list_secrets(&pool).await?;
            if rows.is_empty() {
                println!("(no secrets stored)");
                return Ok(());
            }
            println!(
                "{:<28} {:<14} {:<20} {}",
                "KEY", "UPDATED BY", "UPDATED AT", "DESCRIPTION"
            );
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
        SecretsCommand::Get { key } => match ff_db::pg_get_secret(&pool, &key).await? {
            Some(value) => println!("{value}"),
            None => {
                eprintln!("No secret set for key: {key}");
                std::process::exit(1);
            }
        },
        SecretsCommand::Set {
            key,
            value,
            description,
        } => {
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
        SecretsCommand::Rotate { key, value } => {
            let rotator = ff_agent::secrets_rotation::SecretsRotator::new(pool.clone());
            match rotator.rotate(&key, value).await {
                Ok(out) => {
                    println!(
                        "Rotated '{}' ({} bytes, sha12={}, kind={})",
                        out.key, out.new_len, out.new_fingerprint, out.kind
                    );
                }
                Err(e) => {
                    eprintln!("Rotation failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        SecretsCommand::Expirations => {
            let rotator = ff_agent::secrets_rotation::SecretsRotator::new(pool.clone());
            let report = rotator.check_expirations().await?;
            if report.near_expiry.is_empty() && report.already_expired.is_empty() {
                println!("(no secrets near expiry)");
                return Ok(());
            }
            println!(
                "{:<30} {:>10} {:>5} {}",
                "KEY", "DAYS_LEFT", "ROT#", "EXPIRES_AT"
            );
            for row in report
                .already_expired
                .iter()
                .chain(report.near_expiry.iter())
            {
                let exp = row
                    .expires_at
                    .map(|t| t.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| "-".into());
                let days = row
                    .days_remaining
                    .map(|d| d.to_string())
                    .unwrap_or_else(|| "-".into());
                println!(
                    "{:<30} {:>10} {:>5} {}",
                    row.key, days, row.rotation_count, exp,
                );
            }
            println!(
                "\n{} alert(s) dispatched. near_expiry={} expired={}",
                report.alerts_dispatched,
                report.near_expiry.len(),
                report.already_expired.len(),
            );
        }
        SecretsCommand::DisableGate { key, hours, reason } => {
            if reason.trim().is_empty() {
                anyhow::bail!(
                    "--reason cannot be empty (the whole point of this verb is non-anonymous disables)"
                );
            }
            if hours == 0 {
                anyhow::bail!("--hours must be > 0 (zero would auto-restore immediately)");
            }
            let expires_at = chrono::Utc::now() + chrono::Duration::hours(hours as i64);
            let me = whoami_tag();
            ff_db::pg_disable_safety_gate(&pool, &key, &reason, expires_at, Some(&me)).await?;
            println!(
                "{YELLOW}!{RESET} {key} disabled until {} ({hours}h)\n  reason: {reason}\n  by:     {me}",
                expires_at.format("%Y-%m-%d %H:%M UTC"),
            );
            println!(
                "  After expiry, gate-check helpers (e.g. auto_upgrade_tick) auto-restore to the safe default.\n  To extend, re-run this verb with new --hours."
            );
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
    ff_db::run_postgres_migrations(&pool)
        .await
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
                        "node_online" => r
                            .trigger_spec
                            .get("node")
                            .and_then(|v| v.as_str())
                            .map(|n| format!("node={n}"))
                            .unwrap_or_else(|| "node_online".into()),
                        "at_time" => r
                            .trigger_spec
                            .get("at")
                            .and_then(|v| v.as_str())
                            .unwrap_or("at_time")
                            .to_string(),
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
        DeferCommand::AddShell {
            title,
            run,
            when_node_online,
            when_at,
            on_node,
            max_attempts,
        } => {
            let (trigger_type, trigger_spec, preferred_node) =
                if let Some(node) = when_node_online.clone() {
                    (
                        "node_online".to_string(),
                        serde_json::json!({"node": node}),
                        on_node.clone().or(Some(node)),
                    )
                } else if let Some(at) = when_at {
                    (
                        "at_time".to_string(),
                        serde_json::json!({"at": at}),
                        on_node.clone(),
                    )
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
            println!(
                "NOTE: executor loop is not yet running. Task is captured durably in Postgres"
            );
            println!("      and will begin processing once `forgefleetd defer-worker` is live.");
        }
        DeferCommand::Get { id } => match ff_db::pg_get_deferred(&pool, &id).await? {
            Some(r) => {
                println!("ID:            {}", r.id);
                println!("Title:         {}", r.title);
                println!("Status:        {}", r.status);
                println!("Kind:          {}", r.kind);
                println!("Trigger:       {} ({})", r.trigger_type, r.trigger_spec);
                println!(
                    "Preferred node:{}",
                    r.preferred_node.clone().unwrap_or_else(|| "-".into())
                );
                println!("Attempts:      {}/{}", r.attempts, r.max_attempts);
                println!(
                    "Created:       {}  by {}",
                    r.created_at.format("%Y-%m-%d %H:%M UTC"),
                    r.created_by.clone().unwrap_or_else(|| "-".into())
                );
                if let Some(ts) = r.next_attempt_at {
                    println!("Next attempt:  {}", ts.format("%Y-%m-%d %H:%M UTC"));
                }
                if let Some(n) = &r.claimed_by {
                    println!("Claimed by:    {n}");
                }
                if let Some(err) = &r.last_error {
                    println!("Last error:    {err}");
                }
                if let Some(res) = &r.result {
                    println!("Result:        {res}");
                }
                println!(
                    "\nPayload:\n{}",
                    serde_json::to_string_pretty(&r.payload).unwrap_or_default()
                );
            }
            None => {
                eprintln!("No deferred task with id '{id}'");
                std::process::exit(1);
            }
        },
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
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        ModelCommand::ServeTp2 {
            model_id,
            across,
            shared_vault,
            port,
            container_path,
            max_model_len,
            gpu_memory_utilization,
        } => {
            let (a, b) = match across.split_once('+') {
                Some(parts) => parts,
                None => anyhow::bail!("--across requires `<hostA>+<hostB>` (e.g. `sia+adele`)"),
            };
            let path_inside = container_path.unwrap_or_else(|| format!("/models/{}", model_id));
            model_serve_cmd::handle_model_serve_tp2(
                &pool,
                &model_id,
                a,
                b,
                &shared_vault,
                port,
                &path_inside,
                max_model_len,
                gpu_memory_utilization,
            )
            .await?;
        }
        ModelCommand::SyncCatalog => {
            let n = ff_agent::model_catalog::sync_catalog(&pool)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            println!("Synced {n} catalog entries from TOML to Postgres");
        }
        ModelCommand::Search { query } => {
            let rows = ff_db::pg_search_catalog(&pool, &query).await?;
            if rows.is_empty() {
                println!("(no catalog matches for \"{query}\")");
                return Ok(());
            }
            println!(
                "{:<28} {:<10} {:<6} {:<7} {}",
                "ID", "FAMILY", "TIER", "GATED", "NAME"
            );
            for r in rows {
                let gated = if r.gated { "yes" } else { "-" };
                println!(
                    "{:<28} {:<10} T{:<5} {:<7} {}",
                    r.id, r.family, r.tier, gated, r.name
                );
            }
        }
        ModelCommand::Catalog => {
            let rows = ff_db::pg_list_catalog(&pool).await?;
            if rows.is_empty() {
                println!("(catalog empty — run `ff model sync-catalog` first)");
                return Ok(());
            }
            println!(
                "{:<28} {:<10} {:<6} {:<7} {:<7} {}",
                "ID", "FAMILY", "TIER", "PARAMS", "GATED", "NAME"
            );
            for r in rows {
                let gated = if r.gated { "yes" } else { "-" };
                println!(
                    "{:<28} {:<10} T{:<5} {:<7} {:<7} {}",
                    r.id, r.family, r.tier, r.parameters, gated, r.name
                );
            }
        }
        ModelCommand::Library { node } => {
            let rows = ff_db::pg_list_library(&pool, node.as_deref()).await?;
            if rows.is_empty() {
                println!("(library empty — run `ff model scan` to index your local models dir)");
                return Ok(());
            }
            println!(
                "{:<10} {:<28} {:<10} {:<10} {:<10} {}",
                "NODE", "CATALOG_ID", "RUNTIME", "QUANT", "SIZE", "PATH"
            );
            for r in rows {
                let sz = human_bytes(r.size_bytes as u64);
                let quant = r.quant.clone().unwrap_or_else(|| "-".into());
                println!(
                    "{:<10} {:<28} {:<10} {:<10} {:<10} {}",
                    r.node_name, r.catalog_id, r.runtime, quant, sz, r.file_path
                );
            }
        }
        ModelCommand::Deployments { node } => {
            let rows = ff_db::pg_list_deployments(&pool, node.as_deref()).await?;
            if rows.is_empty() {
                println!("(no deployments recorded)");
                return Ok(());
            }
            println!(
                "{:<10} {:<28} {:<10} {:<6} {:<10} {}",
                "NODE", "CATALOG_ID", "RUNTIME", "PORT", "HEALTH", "STARTED"
            );
            for r in rows {
                let catalog = r.catalog_id.clone().unwrap_or_else(|| "-".into());
                println!(
                    "{:<10} {:<28} {:<10} {:<6} {:<10} {}",
                    r.node_name,
                    catalog,
                    r.runtime,
                    r.port,
                    r.health_status,
                    r.started_at.format("%Y-%m-%d %H:%M UTC")
                );
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
            let summary =
                ff_agent::model_library_scanner::scan_local_library(&pool, &node_name, &dir)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
            println!("  added:   {}", summary.added);
            println!("  updated: {}", summary.updated);
            println!("  removed: {}", summary.removed);
            println!(
                "  total:   {} across models dir",
                human_bytes(summary.total_bytes)
            );
        }
        ModelCommand::Disk => {
            let rows = ff_db::pg_latest_disk_usage(&pool).await?;
            if rows.is_empty() {
                println!("(no disk usage samples yet — the daemon records these periodically)");
                return Ok(());
            }
            println!(
                "{:<10} {:<24} {:<10} {:<10} {:<10} {}",
                "NODE", "MODELS_DIR", "FREE", "USED", "MODELS", "SAMPLED"
            );
            for (node, dir, total, used, free, models_sz, ts) in rows {
                let _ = total;
                println!(
                    "{:<10} {:<24} {:<10} {:<10} {:<10} {}",
                    node,
                    dir,
                    human_bytes(free as u64),
                    human_bytes(used as u64),
                    human_bytes(models_sz as u64),
                    ts.format("%Y-%m-%d %H:%M UTC")
                );
            }
        }
        ModelCommand::Download {
            id,
            runtime,
            node,
            force,
        } => {
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
                anyhow::bail!(
                    "node '{node_name}' has unknown runtime; set with: ff config set fleet.{node_name}.runtime mlx|llama.cpp|vllm"
                );
            }

            // Lookup catalog entry; pick variant for runtime.
            let catalog = ff_db::pg_get_catalog(&pool, &id).await?.ok_or_else(|| {
                anyhow::anyhow!("no catalog entry with id '{id}' (try `ff model search`)")
            })?;
            let variants = catalog
                .variants
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("catalog variants for '{id}' is not an array"))?;
            let variant = variants
                .iter()
                .find(|v| {
                    v.get("runtime").and_then(|x| x.as_str()) == Some(target_runtime.as_str())
                })
                .ok_or_else(|| {
                    let available: Vec<String> = variants
                        .iter()
                        .filter_map(|v| v.get("runtime").and_then(|x| x.as_str()).map(String::from))
                        .collect();
                    anyhow::anyhow!(
                        "no variant for runtime '{target_runtime}' on '{id}'. available: {}",
                        available.join(", ")
                    )
                })?;

            let hf_repo = variant
                .get("hf_repo")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("variant missing hf_repo"))?;
            let quant = variant
                .get("quant")
                .and_then(|v| v.as_str())
                .map(String::from);
            let size_gb = variant
                .get("size_gb")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);

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
                anyhow::bail!(
                    "model '{id}' is gated on HF; set token first with: ff secrets set huggingface.token <hf_xxx>"
                );
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
            let job_id =
                ff_db::pg_create_job(&pool, &node_name, "download", Some(&id), None, &params)
                    .await?;
            ff_db::pg_update_job_progress(
                &pool,
                &job_id,
                Some("running"),
                Some(0.0),
                None,
                None,
                None,
                None,
            )
            .await?;

            println!(
                "{CYAN}▶ Downloading {} ({})\n  source: {}\n  dest:   {}\n  job:    {}{RESET}",
                catalog.name,
                target_runtime,
                hf_repo,
                dest.display(),
                job_id
            );
            if size_gb > 0.0 {
                println!("  estimated size: {size_gb:.1} GB");
            }

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
                    eprint!(
                        "\r  [{bar}] {pct:>3}%  {done_mb}/{total_mb} MiB  {}",
                        trunc_for_status(&p.file, 40)
                    );
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
                            &pool2,
                            &jid,
                            None,
                            Some(pp),
                            Some(bd),
                            Some(bt),
                            None,
                            None,
                        )
                        .await;
                    });
                }
            })
            .await;
            eprintln!(); // newline after progress bar

            match result {
                Ok(files) => {
                    println!("{CYAN}✓ Downloaded {} file(s){RESET}", files.len());
                    let _ = ff_db::pg_update_job_progress(
                        &pool,
                        &job_id,
                        Some("completed"),
                        Some(100.0),
                        None,
                        None,
                        None,
                        None,
                    )
                    .await;
                    // Re-scan node so library reflects the new model.
                    println!("Re-scanning library...");
                    let summary = ff_agent::model_library_scanner::scan_local_library(
                        &pool,
                        &node_name,
                        &models_dir,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
                    println!("  added: {}, updated: {}", summary.added, summary.updated);
                }
                Err(e) => {
                    let _ = ff_db::pg_update_job_progress(
                        &pool,
                        &job_id,
                        Some("failed"),
                        None,
                        None,
                        None,
                        None,
                        Some(&e),
                    )
                    .await;
                    anyhow::bail!("download failed: {e}");
                }
            }
        }
        ModelCommand::DownloadBatch { node, ids } => {
            if ids.is_empty() {
                anyhow::bail!(
                    "no catalog ids provided; usage: ff model download-batch --node <name> <id>..."
                );
            }
            // Resolve target node + its runtime.
            let node_row = ff_db::pg_get_node(&pool, &node)
                .await?
                .ok_or_else(|| anyhow::anyhow!("node '{node}' not in fleet_nodes"))?;
            let target_runtime = node_row.runtime.clone();
            if target_runtime == "unknown" {
                anyhow::bail!(
                    "node '{node}' has unknown runtime; set with: ff config set fleet.{node}.runtime mlx|llama.cpp|vllm"
                );
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
                let title = format!("Download {} ({} variant) on {}", id, target_runtime, node);
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

            println!(
                "Enqueued {} cross-node downloads on '{}':",
                enqueued.len(),
                node
            );
            for (id, defer_id) in &enqueued {
                println!("  {defer_id}  {id}");
            }
            println!("Check status with: ff defer list");
        }
        ModelCommand::Delete { id, yes } => {
            // Look up library row.
            let all = ff_db::pg_list_library(&pool, None).await?;
            let row = all.iter().find(|r| r.id == id).ok_or_else(|| {
                anyhow::anyhow!("no library entry with id '{id}' (try `ff model library`)")
            })?;

            // Safety: refuse if a deployment references this library row.
            let deployments = ff_db::pg_list_deployments(&pool, Some(&row.node_name)).await?;
            let in_use = deployments
                .iter()
                .any(|d| d.library_id.as_deref() == Some(&id));
            if in_use {
                anyhow::bail!(
                    "model is currently deployed on {} — unload it first (`ff model unload <deployment_id>`)",
                    row.node_name
                );
            }

            // Cross-node delete not yet wired — only this host.
            let this_node = ff_agent::fleet_info::resolve_this_node_name().await;
            if row.node_name != this_node {
                anyhow::bail!(
                    "cross-node delete not yet implemented. run on '{}' instead.",
                    row.node_name
                );
            }

            if !yes {
                println!(
                    "This will delete {} ({}) from disk. Re-run with --yes to confirm.",
                    row.file_path,
                    human_bytes(row.size_bytes as u64)
                );
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
                    println!(
                        "Deleted {} ({}) from {}",
                        row.file_path,
                        human_bytes(row.size_bytes as u64),
                        row.node_name
                    );
                }
                Err(e) => anyhow::bail!("filesystem remove failed: {e}"),
            }
        }
        ModelCommand::Load {
            id,
            port,
            ctx,
            parallel,
        } => {
            let opts = ff_agent::model_runtime::LoadOptions {
                library_id: id.clone(),
                port,
                context_size: ctx,
                parallel,
            };
            println!("{CYAN}▶ Loading library {} on port {port}...{RESET}", id);
            match ff_agent::model_runtime::load_model(&pool, opts).await {
                Ok(res) => {
                    println!(
                        "{CYAN}✓ Loaded{RESET} — deployment {} pid {} @ http://127.0.0.1:{}",
                        res.deployment_id, res.pid, res.port
                    );
                }
                Err(e) => anyhow::bail!("load failed: {e}"),
            }
        }
        ModelCommand::Autoload { catalog_id, ctx } => {
            let node_name = ff_agent::fleet_info::resolve_this_node_name().await;

            // 1. Already deployed?
            let deps = ff_db::pg_list_deployments(&pool, Some(&node_name)).await?;
            if let Some(d) = deps.iter().find(|d| {
                d.catalog_id.as_deref() == Some(&catalog_id) && d.health_status == "healthy"
            }) {
                println!("Already deployed on port {} (deployment {})", d.port, d.id);
                return Ok(());
            }

            // 2. Find library row on this node for this catalog_id.
            let libs = ff_db::pg_list_library(&pool, Some(&node_name)).await?;
            let lib = libs.iter().find(|r| r.catalog_id == catalog_id)
                .ok_or_else(|| anyhow::anyhow!("model '{catalog_id}' not in library on '{node_name}'. Download it first: ff model download {catalog_id}"))?;

            // 3. Pick a free port via port_registry — canonical mapping
            //    (55000-55002 llama.cpp/mlx, 51001/51003 vllm, 11434 ollama).
            //    Fall back to legacy 51001..=51020 scan only if the registry
            //    lookup fails (e.g. fresh install where it hasn't seeded yet).
            let port: u16 = match ff_agent::ports_registry::pick_llm_port(
                &pool,
                &node_name,
                &lib.runtime,
            )
            .await
            {
                Ok(p) => p as u16,
                Err(_) => {
                    let used_ports: std::collections::HashSet<i32> =
                        deps.iter().map(|d| d.port).collect();
                    (51001u16..=51020)
                        .find(|p| !used_ports.contains(&(*p as i32)))
                        .ok_or_else(|| anyhow::anyhow!("no free port in registry or 51001-51020"))?
                }
            };

            // 4. Load.
            let res = ff_agent::model_runtime::load_model(
                &pool,
                ff_agent::model_runtime::LoadOptions {
                    library_id: lib.id.clone(),
                    port,
                    context_size: ctx,
                    parallel: None,
                },
            )
            .await
            .map_err(|e| anyhow::anyhow!(e))?;

            println!(
                "Autoloaded {} on port {} (deployment {})",
                catalog_id, res.port, res.deployment_id
            );
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
                println!(
                    "{:<8} {:<10} {:<8} {}",
                    p.pid,
                    p.runtime,
                    p.port.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
                    p.model_path.clone().unwrap_or_else(|| "-".into())
                );
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
                println!(
                    "Gated:        {}",
                    if c.gated {
                        "yes (HF license required)"
                    } else {
                        "no"
                    }
                );
                if let Some(d) = &c.description {
                    println!("Description:  {d}");
                }
                if let Some(arr) = c.preferred_workloads.as_array() {
                    let wl: Vec<String> = arr
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect();
                    if !wl.is_empty() {
                        println!("Workloads:    {}", wl.join(", "));
                    }
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
                let copies: Vec<&ff_db::ModelLibraryRow> =
                    lib.iter().filter(|r| r.catalog_id == c.id).collect();
                if !copies.is_empty() {
                    println!("\nOn disk:");
                    for r in &copies {
                        let q = r.quant.clone().unwrap_or_else(|| "-".into());
                        println!(
                            "  - {:<10} ({:<10} {:<6}) {}  [{}]",
                            r.node_name,
                            r.runtime,
                            q,
                            human_bytes(r.size_bytes as u64),
                            &r.id[..8]
                        );
                    }
                }
                let deps = ff_db::pg_list_deployments(&pool, None).await?;
                let live: Vec<&ff_db::ModelDeploymentRow> = deps
                    .iter()
                    .filter(|d| d.catalog_id.as_deref() == Some(&c.id))
                    .collect();
                if !live.is_empty() {
                    println!("\nDeployments:");
                    for d in &live {
                        println!(
                            "  - {:<10} port {:<5} {:<10} health={}  [{}]",
                            d.node_name,
                            d.port,
                            d.runtime,
                            d.health_status,
                            &d.id[..8]
                        );
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
                println!(
                    "Quant:        {}",
                    r.quant.clone().unwrap_or_else(|| "-".into())
                );
                println!("File path:    {}", r.file_path);
                println!("Size:         {}", human_bytes(r.size_bytes as u64));
                if let Some(s) = &r.sha256 {
                    println!("SHA256:       {s}");
                }
                println!(
                    "Downloaded:   {}",
                    r.downloaded_at.format("%Y-%m-%d %H:%M UTC")
                );
                if let Some(t) = r.last_used_at {
                    println!("Last used:    {}", t.format("%Y-%m-%d %H:%M UTC"));
                }
                if let Some(s) = &r.source_url {
                    println!("Source:       {s}");
                }
                return Ok(());
            }
            // Try as deployment UUID.
            let all_dep = ff_db::pg_list_deployments(&pool, None).await?;
            if let Some(d) = all_dep.iter().find(|d| d.id == id) {
                println!("{CYAN}━ Deployment ━{RESET}");
                println!("ID:           {}", d.id);
                println!("Node:         {}", d.node_name);
                println!(
                    "Catalog ID:   {}",
                    d.catalog_id.clone().unwrap_or_else(|| "-".into())
                );
                println!("Runtime:      {}", d.runtime);
                println!("Port:         {}", d.port);
                println!(
                    "PID:          {}",
                    d.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into())
                );
                println!("Health:       {}", d.health_status);
                println!(
                    "Started:      {}",
                    d.started_at.format("%Y-%m-%d %H:%M UTC")
                );
                if let Some(t) = d.last_health_at {
                    println!("Last health:  {}", t.format("%Y-%m-%d %H:%M UTC"));
                }
                if let Some(c) = d.context_window {
                    println!("Ctx window:   {c}");
                }
                println!("Tokens used:  {}", d.tokens_used);
                println!("Requests:     {}", d.request_count);
                return Ok(());
            }
            anyhow::bail!("'{id}' is not a known catalog id, library UUID, or deployment UUID");
        }
        ModelCommand::Prune {
            node,
            min_cold_days,
        } => {
            let node_name = match node {
                Some(n) => n,
                None => ff_agent::fleet_info::resolve_this_node_name().await,
            };
            let policy = ff_agent::smart_lru::LruPolicy {
                min_cold_days,
                ..Default::default()
            };
            let plan = ff_agent::smart_lru::plan_eviction(&pool, &node_name, &policy)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            if plan.candidates.is_empty() {
                println!("Node '{node_name}' is within quota — no eviction needed.");
                return Ok(());
            }
            println!(
                "Eviction plan for {node_name} (would free {}):\n",
                human_bytes(plan.total_bytes_freed)
            );
            println!(
                "{:<38} {:<24} {:<10} {:<10} {}",
                "LIBRARY_ID", "CATALOG", "RUNTIME", "SIZE", "REASONS"
            );
            for c in &plan.candidates {
                println!(
                    "{:<38} {:<24} {:<10} {:<10} {}",
                    c.library_id,
                    c.catalog_id,
                    c.runtime,
                    human_bytes(c.size_bytes),
                    c.reasons.join(", ")
                );
            }
            println!("\n(dry-run; use `ff model delete <library-id> --yes` to actually remove)");
        }
        ModelCommand::DiskSample => match ff_agent::disk_sampler::sample_local_disk(&pool).await {
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
        },
        ModelCommand::Ping { id } => {
            match ff_agent::model_runtime::health_check_deployment(&pool, &id).await {
                Ok(true) => println!("{CYAN}✓ healthy{RESET}"),
                Ok(false) => println!("{YELLOW}⚠ unhealthy (reachable but failing){RESET}"),
                Err(e) => anyhow::bail!("health check failed: {e}"),
            }
        }
        ModelCommand::Transfer {
            library_id,
            from,
            to,
        } => {
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
            println!("{CYAN}▶ Converting library {library_id} to MLX ({q_bits}-bit)...{RESET}");
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
            println!(
                "{:<38} {:<10} {:<12} {:<10} {:<7} {}",
                "ID", "NODE", "KIND", "STATUS", "PCT", "TARGET"
            );
            for r in rows {
                let target = r
                    .target_catalog_id
                    .clone()
                    .or(r.target_library_id.clone())
                    .unwrap_or_else(|| "-".into());
                println!(
                    "{:<38} {:<10} {:<12} {:<10} {:<6.1}% {}",
                    r.id, r.node_name, r.kind, r.status, r.progress_pct, target
                );
            }
        }
        ModelCommand::CheckUpstream { json } => {
            println!("{CYAN}▶ Checking HuggingFace for upstream model revisions...{RESET}");
            let checker = ff_agent::model_upstream::ModelUpstreamChecker::new(pool.clone());
            let report = checker
                .check_all()
                .await
                .map_err(|e| anyhow::anyhow!("model upstream check: {e}"))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).unwrap_or_default()
                );
            } else {
                println!(
                    "checked={} updated={} unchanged={} skipped={} errors={} flagged={}",
                    report.checked,
                    report.updated,
                    report.unchanged,
                    report.skipped,
                    report.errors.len(),
                    report.computer_rows_flagged,
                );
                if !report.errors.is_empty() {
                    println!("\n{YELLOW}Errors:{RESET}");
                    for (id, err) in &report.errors {
                        println!("  {id}: {err}");
                    }
                }
            }
        }
        ModelCommand::Coverage { json } => {
            let guard = ff_agent::coverage_guard::CoverageGuard::new_dbonly(pool.clone());
            let report = guard
                .check_once()
                .await
                .map_err(|e| anyhow::anyhow!("coverage check: {e}"))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).unwrap_or_default()
                );
            } else {
                println!("Tasks required:   {}", report.tasks_required);
                println!("Tasks covered:    {}", report.tasks_covered);
                println!("Gaps:             {}", report.gaps.len());
                println!("Auto-loaded:      {}", report.auto_loaded.len());
                if !report.gaps.is_empty() {
                    println!();
                    println!("{:<32} {:<6} {:<6}  CANDIDATES", "TASK", "MIN", "LOAD");
                    for g in &report.gaps {
                        let cands = if g.candidates.is_empty() {
                            "(none)".to_string()
                        } else {
                            g.candidates
                                .iter()
                                .take(3)
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", ")
                        };
                        println!(
                            "{:<32} {:<6} {:<6}  {}",
                            g.task, g.min_required, g.currently_loaded, cands
                        );
                    }
                }
                if !report.auto_loaded.is_empty() {
                    println!();
                    println!(
                        "{GREEN}Enqueued auto-load for:{RESET} {}",
                        report.auto_loaded.join(", ")
                    );
                }
            }
        }
        ModelCommand::Scout { run_now, json } => {
            if run_now {
                println!("{CYAN}▶ Running model scout pass...{RESET}");
                let scout = ff_agent::model_scout::ModelScout::new(pool.clone());
                let report = scout
                    .scout_once()
                    .await
                    .map_err(|e| anyhow::anyhow!("scout: {e}"))?;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&report).unwrap_or_default()
                    );
                } else {
                    println!(
                        "tasks_scanned={} discovered={} added={} filtered={}",
                        report.tasks_scanned,
                        report.discovered,
                        report.added_as_candidates,
                        report.filtered_out,
                    );
                }
            } else {
                let rows = sqlx::query(
                    "SELECT id, display_name, family, license
                     FROM model_catalog
                     WHERE lifecycle_status = 'candidate' AND added_by = 'scout'
                     ORDER BY id
                     LIMIT 100",
                )
                .fetch_all(&pool)
                .await?;
                if json {
                    let arr: Vec<_> = rows
                        .iter()
                        .map(|r| {
                            serde_json::json!({
                                "id": sqlx::Row::get::<String, _>(r, "id"),
                                "display_name": sqlx::Row::get::<String, _>(r, "display_name"),
                                "family": sqlx::Row::get::<String, _>(r, "family"),
                                "license": sqlx::Row::get::<Option<String>, _>(r, "license"),
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
                } else if rows.is_empty() {
                    println!("(no scout candidates — pass --run-now to trigger a pass)");
                } else {
                    println!("{:<40} {:<16} {:<20} NAME", "ID", "FAMILY", "LICENSE");
                    for r in &rows {
                        let id: String = sqlx::Row::get(r, "id");
                        let name: String = sqlx::Row::get(r, "display_name");
                        let fam: String = sqlx::Row::get(r, "family");
                        let lic: Option<String> = sqlx::Row::get(r, "license");
                        println!(
                            "{:<40} {:<16} {:<20} {}",
                            id,
                            fam,
                            lic.unwrap_or_else(|| "-".into()),
                            name
                        );
                    }
                }
            }
        }
        ModelCommand::ReviewCandidates { json } => {
            let rows = sqlx::query(
                "SELECT id, display_name, family, license, added_by, tasks
                 FROM model_catalog
                 WHERE lifecycle_status = 'candidate'
                 ORDER BY added_by, id",
            )
            .fetch_all(&pool)
            .await?;
            if json {
                let arr: Vec<_> = rows
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "id": sqlx::Row::get::<String, _>(r, "id"),
                            "display_name": sqlx::Row::get::<String, _>(r, "display_name"),
                            "family": sqlx::Row::get::<String, _>(r, "family"),
                            "license": sqlx::Row::get::<Option<String>, _>(r, "license"),
                            "added_by": sqlx::Row::get::<Option<String>, _>(r, "added_by"),
                            "tasks": sqlx::Row::get::<serde_json::Value, _>(r, "tasks"),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
            } else if rows.is_empty() {
                println!("(no candidates awaiting review)");
            } else {
                println!(
                    "{:<40} {:<10} {:<16} {:<20} TASKS",
                    "ID", "ADDED_BY", "FAMILY", "LICENSE"
                );
                for r in &rows {
                    let id: String = sqlx::Row::get(r, "id");
                    let fam: String = sqlx::Row::get(r, "family");
                    let lic: Option<String> = sqlx::Row::get(r, "license");
                    let added: Option<String> = sqlx::Row::get(r, "added_by");
                    let tasks: serde_json::Value = sqlx::Row::get(r, "tasks");
                    let tasks_str = tasks
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str())
                                .collect::<Vec<_>>()
                                .join(",")
                        })
                        .unwrap_or_default();
                    println!(
                        "{:<40} {:<10} {:<16} {:<20} {}",
                        id,
                        added.unwrap_or_else(|| "-".into()),
                        fam,
                        lic.unwrap_or_else(|| "-".into()),
                        tasks_str,
                    );
                }
                println!("\nApprove with: ff model approve <id>");
                println!("Reject with:  ff model reject <id>");
            }
        }
        ModelCommand::Approve {
            id,
            skip_benchmark,
            force,
            on_computer,
        } => {
            // 1. Verify the candidate exists and is still in review.
            let row = sqlx::query("SELECT lifecycle_status FROM model_catalog WHERE id = $1")
                .bind(&id)
                .fetch_optional(&pool)
                .await?;
            let Some(row) = row else {
                anyhow::bail!("no catalog row found for id '{id}'");
            };
            let status: String = sqlx::Row::get(&row, "lifecycle_status");
            if status != "candidate" {
                anyhow::bail!(
                    "model '{id}' is in lifecycle_status='{status}' — only 'candidate' rows can be approved"
                );
            }

            let skip = skip_benchmark || force;
            let mut bench_summary: Option<ff_agent::model_benchmark::BenchmarkReport> = None;

            // 2. Benchmark gate (unless skipped).
            if !skip {
                // Open a Pulse reader so we can pick a target and find
                // any healthy loaded endpoint.
                let redis_url = std::env::var("FORGEFLEET_REDIS_URL")
                    .unwrap_or_else(|_| "redis://127.0.0.1:6380".into());
                let pulse = match ff_pulse::reader::PulseReader::new(&redis_url) {
                    Ok(p) => p,
                    Err(e) => {
                        anyhow::bail!(
                            "can't open Pulse at {redis_url}: {e}\n\
                             Either fix Redis connectivity, or re-run with --skip-benchmark."
                        );
                    }
                };

                // Pick the target computer.
                let target = if let Some(c) = on_computer.clone() {
                    c
                } else {
                    match ff_agent::model_benchmark::pick_benchmark_target(&pool, &pulse, &id).await
                    {
                        Ok(Some(n)) => n,
                        Ok(None) => {
                            anyhow::bail!(
                                "no compatible node found to benchmark '{id}' \
                                 (check required_gpu_kind / min_vram_gb / file_size_gb \
                                 vs live Pulse beats). \
                                 Use --on-computer <name> to force one, or \
                                 --skip-benchmark to approve without benchmarking."
                            );
                        }
                        Err(e) => anyhow::bail!("pick_benchmark_target failed: {e}"),
                    }
                };

                println!("{CYAN}→{RESET} Benchmarking '{id}' on '{target}' before promotion…");

                let bencher = ff_agent::model_benchmark::ModelBenchmarker::new(pool.clone(), pulse);
                match bencher.benchmark(&id, &target).await {
                    Ok(report) => {
                        if !report.bench_pass {
                            eprintln!(
                                "{RED}✗ Benchmark failed:{RESET} {}\n  \
                                 tokens/sec: {:.2}\n  \
                                 ttft (ms):  {}\n  \
                                 endpoint:   {}\n\n\
                                 Inspect results with: ff model benchmarks --model {id}\n\
                                 Force anyway with:     ff model approve {id} --skip-benchmark",
                                report.bench_pass_reason,
                                report.tokens_per_sec,
                                report.ttft_ms,
                                report.endpoint,
                            );
                            std::process::exit(1);
                        }
                        bench_summary = Some(report);
                    }
                    Err(ff_agent::model_benchmark::BenchError::NotLoaded(m, c)) => {
                        eprintln!(
                            "{RED}✗ Cannot benchmark:{RESET} model '{m}' is not loaded \
                             on '{c}' (no active+healthy LLM server found in Pulse).\n\n\
                             Either:\n  \
                               • load it first:   ff model load <library_id> --port 51001\n  \
                               • pick a node that has it loaded: --on-computer <name>\n  \
                               • skip the benchmark: --skip-benchmark"
                        );
                        std::process::exit(1);
                    }
                    Err(e) => anyhow::bail!("benchmark error: {e}"),
                }
            }

            // 3. Promote to active (idempotent-safe: we re-check the gate).
            let result = sqlx::query(
                "UPDATE model_catalog
                    SET lifecycle_status = 'active'
                  WHERE id = $1 AND lifecycle_status = 'candidate'",
            )
            .bind(&id)
            .execute(&pool)
            .await?;
            if result.rows_affected() == 0 {
                anyhow::bail!("race: candidate '{id}' was changed by someone else during approval");
            }

            // 4. Report.
            println!("{GREEN}✓{RESET} Promoted '{id}' to lifecycle_status='active'");
            if let Some(r) = bench_summary {
                println!("  benchmark pass:   yes");
                println!("  computer:         {}", r.computer);
                println!("  endpoint:         {}", r.endpoint);
                println!("  tokens/sec:       {:.2}", r.tokens_per_sec);
                println!("  ttft (ms):        {}", r.ttft_ms);
                println!("  prompts:          {}", r.prompt_count);
            } else {
                println!("  benchmark pass:   (skipped)");
            }
        }
        ModelCommand::Reject { id } => {
            let row = sqlx::query(
                "SELECT upstream_id FROM model_catalog
                  WHERE id = $1 AND lifecycle_status = 'candidate'",
            )
            .bind(&id)
            .fetch_optional(&pool)
            .await?;
            let Some(row) = row else {
                anyhow::bail!("no candidate row found for id '{id}'");
            };
            let upstream_id: Option<String> = sqlx::Row::get(&row, "upstream_id");

            let deleted = sqlx::query(
                "DELETE FROM model_catalog
                  WHERE id = $1 AND lifecycle_status = 'candidate'",
            )
            .bind(&id)
            .execute(&pool)
            .await?;
            if deleted.rows_affected() == 0 {
                anyhow::bail!("failed to delete candidate '{id}'");
            }

            if let Some(up) = upstream_id {
                let inserted = sqlx::query(
                    "INSERT INTO model_scout_denylist (model_id, reason, added_by)
                     VALUES ($1, $2, $3)
                     ON CONFLICT (model_id) DO NOTHING",
                )
                .bind(up.to_ascii_lowercase())
                .bind(Some("ff model reject"))
                .bind(whoami_tag())
                .execute(&pool)
                .await?;
                if inserted.rows_affected() == 1 {
                    println!(
                        "{GREEN}✓{RESET} Rejected '{id}' and added upstream_id '{up}' to denylist"
                    );
                } else {
                    println!(
                        "{GREEN}✓{RESET} Rejected '{id}' (upstream '{up}' already in denylist)"
                    );
                }
            } else {
                println!("{GREEN}✓{RESET} Rejected '{id}' (no upstream_id to denylist)");
            }
        }
        ModelCommand::Retire {
            id,
            replace_with,
            reason,
        } => {
            let result = sqlx::query(
                "UPDATE model_catalog
                    SET lifecycle_status   = 'retired',
                        replaced_by        = COALESCE($2, replaced_by),
                        retirement_reason  = $3,
                        retirement_date    = CURRENT_DATE
                  WHERE id = $1",
            )
            .bind(&id)
            .bind(replace_with.as_deref())
            .bind(&reason)
            .execute(&pool)
            .await?;
            if result.rows_affected() == 0 {
                anyhow::bail!("no catalog row for id '{id}'");
            }
            match replace_with {
                Some(rep) => println!("{GREEN}✓{RESET} Retired '{id}' (replaced by '{rep}')"),
                None => println!("{GREEN}✓{RESET} Retired '{id}'"),
            }
        }
        ModelCommand::Benchmark {
            model_id,
            computer,
            json,
        } => {
            let computer = if let Some(c) = computer {
                c
            } else {
                ff_agent::fleet_info::resolve_this_node_name().await
            };
            match ff_agent::model_benchmark::benchmark_with_defaults(&pool, &model_id, &computer)
                .await
            {
                Ok(report) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&report).unwrap_or_default()
                        );
                    } else {
                        println!("{GREEN}✓ Benchmark complete{RESET}");
                        println!("  model:            {}", report.model_id);
                        println!("  computer:         {}", report.computer);
                        println!("  runtime:          {}", report.runtime);
                        println!("  endpoint:         {}", report.endpoint);
                        println!("  tokens/sec:       {:.2}", report.tokens_per_sec);
                        println!("  ttft (ms):        {}", report.ttft_ms);
                        println!("  prompt eval/sec:  {:.2}", report.prompt_eval_rate);
                        println!("  max ctx tokens:   {}", report.context_tokens_max);
                        println!("  prompt count:     {}", report.prompt_count);
                    }
                }
                Err(e) => {
                    eprintln!("{RED}✗ Benchmark failed: {e}{RESET}");
                    std::process::exit(1);
                }
            }
        }
        ModelCommand::Benchmarks { model } => {
            let target = model.unwrap_or_else(|| {
                eprintln!(
                    "{YELLOW}No --model specified; pass --model <catalog_id> to narrow.{RESET}"
                );
                String::new()
            });
            if target.is_empty() {
                return Ok(());
            }
            match ff_db::pg_get_benchmark_results(&pool, &target).await? {
                Some(v) => {
                    if let Some(obj) = v.as_object() {
                        if obj.is_empty() {
                            println!("(no benchmark runs recorded for '{target}')");
                        } else {
                            println!("{:<48} {:<12} {:<12}", "RUN KEY", "TOKENS/S", "TTFT(ms)");
                            for (key, run) in obj {
                                let tps = run
                                    .get("tokens_per_sec")
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0);
                                let ttft = run.get("ttft_ms").and_then(|v| v.as_u64()).unwrap_or(0);
                                println!("{:<48} {:<12.2} {:<12}", key, tps, ttft);
                            }
                        }
                    } else {
                        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                    }
                }
                None => {
                    eprintln!("No catalog row for id '{target}'");
                    std::process::exit(1);
                }
            }
        }
    }
    Ok(())
}

/// Pretty-print a byte size (KiB/MiB/GiB/TiB).
fn human_bytes(n: u64) -> String {
    let (unit, v) = if n >= 1 << 40 {
        ("TiB", n as f64 / (1u64 << 40) as f64)
    } else if n >= 1 << 30 {
        ("GiB", n as f64 / (1u64 << 30) as f64)
    } else if n >= 1 << 20 {
        ("MiB", n as f64 / (1u64 << 20) as f64)
    } else if n >= 1 << 10 {
        ("KiB", n as f64 / (1u64 << 10) as f64)
    } else {
        return format!("{n}B");
    };
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
    if s.chars().count() <= max {
        return s.to_string();
    }
    let take = max.saturating_sub(1);
    let suffix: String = s
        .chars()
        .rev()
        .take(take)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{suffix}")
}

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
        let installed_version_to_write =
            if latest_version == "-" || latest_version.is_empty() {
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
            format!("Failure {count}/{AUTO_UPGRADE_FAILURE_THRESHOLD} — will retry on next hourly tick.")
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

/// Post-completion hook for `meta.external_tool` deferred tasks.
///
/// Runs whether the task succeeded or failed. Flips
/// `computer_external_tools.status` from `'installing'` / `'upgrading'`
/// to `'ok'` (success) or `'install_failed'` (failure), and makes a
/// best-effort attempt to parse `installed_version` / `install_path`
/// out of the task stdout.
///
/// TODO: when MCP auto-registration lands (see project memory
/// `project_external_tools_subsystem.md`), also flip `mcp_registered=true`
/// after running the registration command on the target computer.
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
        if let Some(rest) = line.strip_prefix("Installing to ") {
            Some(rest.trim().to_string())
        } else {
            None
        }
    });

    let new_status = if ok { "ok" } else { "install_failed" };

    let _ = sqlx::query(
        "UPDATE computer_external_tools cet
            SET status = $1,
                last_upgraded_at = CASE WHEN $1 = 'ok' THEN NOW() ELSE last_upgraded_at END,
                last_checked_at  = NOW(),
                installed_version = COALESCE($4, cet.installed_version),
                install_path      = COALESCE($5, cet.install_path),
                last_error        = CASE WHEN $1 = 'ok' THEN NULL ELSE $6 END
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
                let err = format!(
                    "exit {}: {}",
                    o.status.code().unwrap_or(-1),
                    stderr.trim().lines().last().unwrap_or("")
                );
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
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
    {
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
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    let nodes = ff_db::pg_list_nodes(&pool).await?;
    let filtered: Vec<&ff_db::FleetNodeRow> = nodes
        .iter()
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
        println!(
            "(no tool-version data yet — run `ff daemon` for 6h or manually trigger version_check)"
        );
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
            // Compare on code-identity (SHA prefix) so the same SHA in two
            // different shapes (e.g. short SHA vs full ff-version string)
            // still reads as ✓ rather than ⚠. Without this the ff row
            // (which stores the full --version string) and the ff_git row
            // (which stores the raw SHA) compared against the same upstream
            // SHA would always show drift.
            use ff_core::build_version::display_version_short;
            let cur_short = display_version_short(cur);
            let marker = match lat {
                Some(l) if display_version_short(l) == cur_short => "✓",
                Some(_) => "⚠",
                None => " ",
            };
            let disp = format!("{} {}", cur_short, marker);
            print!(" {:<14}", disp);
        }
        println!();
    }
    Ok(())
}

fn truncate_for_col(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

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
            handle_fleet_leader(&pool, json).await?;
        }
        FleetCommand::Health { json } => {
            handle_fleet_health(&pool, json).await?;
        }
        FleetCommand::Versions { verbose, live } => {
            handle_fleet_versions(&pool, verbose, live).await?;
        }
        FleetCommand::Gossip => {
            handle_fleet_gossip().await?;
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
            handle_fleet_revive(&pool, &computer, wol_only, internal).await?;
        }
        FleetCommand::TaskCoverage { command } => {
            handle_fleet_task_coverage(&pool, command).await?;
        }
        FleetCommand::RevokeTrust { computer, yes } => {
            handle_fleet_revoke_trust(&pool, &computer, yes).await?;
        }
        FleetCommand::RemoveComputer { name, yes } => {
            handle_fleet_remove_computer(&pool, &name, yes).await?;
        }
        FleetCommand::Disband {
            yes,
            i_know_what_im_doing,
        } => {
            handle_fleet_disband(&pool, yes, i_know_what_im_doing).await?;
        }
        FleetCommand::MigrateSourceTrees { dry_run, yes } => {
            handle_fleet_migrate_source_trees(&pool, dry_run, yes).await?;
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
            handle_fleet_rotate_pulse_hmac(&pool, value).await?;
        }
        FleetCommand::Backup { kind, force } => {
            handle_fleet_backup(&pool, &kind, force).await?;
        }
        FleetCommand::SetNetworkScope { computer, scope } => {
            handle_fleet_set_network_scope(&pool, &computer, &scope).await?;
        }
        FleetCommand::Db { command } => {
            handle_fleet_db(&pool, command).await?;
        }
        FleetCommand::PanicStop { yes, halt_dbs } => {
            handle_fleet_panic_stop(&pool, yes, halt_dbs).await?;
        }
        FleetCommand::Resume { yes } => {
            handle_fleet_resume(&pool, yes).await?;
        }
        FleetCommand::Quarantine { computer, yes } => {
            handle_fleet_quarantine(&pool, &computer, yes).await?;
        }
        FleetCommand::Unquarantine { computer, yes } => {
            handle_fleet_unquarantine(&pool, &computer, yes).await?;
        }
        FleetCommand::Upgrade {
            software_id,
            computer,
            all,
            dry_run,
            yes,
            force_dirty,
        } => {
            handle_fleet_upgrade(
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

/// `ff fleet panic-stop` — emergency halt of every daemon.
///
/// The implementation initializes NATS best-effort before delegating to
/// `panic_stop::fleet_panic_stop` so observers on the bus see the event
/// (the stop itself doesn't need NATS but `--halt-dbs` users expect
/// downstream alerting to fire).
async fn handle_fleet_panic_stop(pool: &sqlx::PgPool, yes: bool, halt_dbs: bool) -> Result<()> {
    if !yes {
        eprintln!("{YELLOW}⚠ panic-stop halts EVERY ForgeFleet daemon across the fleet.{RESET}");
        eprintln!("  Use this only when the fleet is misbehaving (runaway loops, resource");
        eprintln!(
            "  exhaustion, task spam). Pass --yes to proceed. Recover via `ff fleet resume`."
        );
        std::process::exit(1);
    }

    // Fire-and-forget NATS init so the quarantine/halt events propagate.
    let _ = ff_agent::nats_client::init_nats(&ff_agent::nats_client::resolve_nats_url()).await;

    println!("{CYAN}▶ ff fleet panic-stop — halting every daemon…{RESET}");
    let local = ff_agent::fleet_info::resolve_this_node_name().await;
    let report = ff_agent::panic_stop::fleet_panic_stop(pool, &local)
        .await
        .map_err(|e| anyhow::anyhow!("panic_stop: {e}"))?;

    for e in &report.entries {
        let marker = if e.ok {
            format!("{GREEN}✓{RESET}")
        } else {
            format!("{RED}✗{RESET}")
        };
        println!("  {marker} {:<10} {}", e.name, e.detail);
    }
    println!(
        "\n{} of {} daemons stopped.{}",
        report.succeeded,
        report.total,
        if report.failed > 0 {
            format!(
                " {YELLOW}({} failure{}){RESET}",
                report.failed,
                if report.failed == 1 { "" } else { "s" }
            )
        } else {
            String::new()
        },
    );

    if halt_dbs {
        println!("\n{CYAN}▶ --halt-dbs — stopping local Docker data-plane containers…{RESET}");
        let (ok, detail) = ff_agent::panic_stop::stop_taylor_docker_stack().await;
        let marker = if ok {
            format!("{GREEN}✓{RESET}")
        } else {
            format!("{YELLOW}—{RESET}")
        };
        println!("  {marker} docker stack\n{detail}");
        if !ok {
            println!(
                "{YELLOW}(some containers weren't running locally — expected if this isn't Taylor){RESET}"
            );
        }
    }

    println!("\nRecover with: {CYAN}ff fleet resume --yes{RESET}");
    if report.failed > 0 {
        std::process::exit(3);
    }
    Ok(())
}

/// `ff fleet resume` — symmetric undo of panic-stop.
async fn handle_fleet_resume(pool: &sqlx::PgPool, yes: bool) -> Result<()> {
    if !yes {
        eprintln!(
            "{YELLOW}⚠ resume will (re)start every daemon across the fleet. Pass --yes to proceed.{RESET}"
        );
        std::process::exit(1);
    }

    println!("{CYAN}▶ ff fleet resume — starting every daemon…{RESET}");
    let local = ff_agent::fleet_info::resolve_this_node_name().await;
    let report = ff_agent::panic_stop::fleet_resume(pool, &local)
        .await
        .map_err(|e| anyhow::anyhow!("resume: {e}"))?;

    for e in &report.entries {
        let marker = if e.ok {
            format!("{GREEN}✓{RESET}")
        } else {
            format!("{RED}✗{RESET}")
        };
        println!("  {marker} {:<10} {}", e.name, e.detail);
    }
    println!(
        "\n{} of {} daemons (re)started.{}",
        report.succeeded,
        report.total,
        if report.failed > 0 {
            format!(
                " {YELLOW}({} failure{}){RESET}",
                report.failed,
                if report.failed == 1 { "" } else { "s" }
            )
        } else {
            String::new()
        },
    );
    if report.failed > 0 {
        std::process::exit(3);
    }
    Ok(())
}

/// `ff fleet quarantine <computer>` — stop daemons + flip status to
/// 'maintenance'. See module docs on `panic_stop.rs` for full flow.
async fn handle_fleet_quarantine(pool: &sqlx::PgPool, computer: &str, yes: bool) -> Result<()> {
    if !yes {
        eprintln!(
            "{YELLOW}⚠ quarantine will stop daemons on '{computer}' and mark it 'maintenance'.{RESET}"
        );
        eprintln!("  The node will be excluded from leader election and LLM routing.");
        eprintln!(
            "  Pass --yes to proceed. Reverse with `ff fleet unquarantine {computer} --yes`."
        );
        std::process::exit(1);
    }

    let _ = ff_agent::nats_client::init_nats(&ff_agent::nats_client::resolve_nats_url()).await;

    println!("{CYAN}▶ ff fleet quarantine {computer}{RESET}");
    let result = ff_agent::panic_stop::quarantine_computer(pool, computer)
        .await
        .map_err(|e| anyhow::anyhow!("quarantine: {e}"))?;

    if result.ssh_stop_ok {
        println!("  {GREEN}✓{RESET} ssh stop succeeded on '{}'", result.name);
    } else {
        println!(
            "  {YELLOW}—{RESET} ssh stop did NOT succeed on '{}' (detail: {}) — DB flip applied anyway",
            result.name, result.ssh_detail
        );
    }
    println!("  {GREEN}✓{RESET} status='maintenance' in computers table");
    println!(
        "  {GREEN}✓{RESET} openclaw_installations.mode='node', gateway_url cleared (if present)"
    );
    println!("  {GREEN}✓{RESET} published fleet.events.quarantine on NATS");
    println!();
    println!("Implications while '{}' is quarantined:", result.name);
    println!("  • will not participate in leader election");
    println!("  • will not receive LLM inference requests");
    println!("  • pulse beats still recorded but computer is excluded from healthy-member lists");
    println!();
    println!(
        "Reverse with: {CYAN}ff fleet unquarantine {} --yes{RESET}",
        result.name
    );
    Ok(())
}

/// `ff fleet unquarantine <computer>` — restart daemons + flip status back
/// to 'pending'. Next pulse beat moves it to 'online'.
async fn handle_fleet_unquarantine(pool: &sqlx::PgPool, computer: &str, yes: bool) -> Result<()> {
    if !yes {
        eprintln!(
            "{YELLOW}⚠ unquarantine will restart daemons on '{computer}' and reset its status. Pass --yes to proceed.{RESET}"
        );
        std::process::exit(1);
    }

    let _ = ff_agent::nats_client::init_nats(&ff_agent::nats_client::resolve_nats_url()).await;

    println!("{CYAN}▶ ff fleet unquarantine {computer}{RESET}");
    let result = ff_agent::panic_stop::unquarantine_computer(pool, computer)
        .await
        .map_err(|e| anyhow::anyhow!("unquarantine: {e}"))?;

    if result.ssh_stop_ok {
        println!("  {GREEN}✓{RESET} ssh start succeeded on '{}'", result.name);
    } else {
        println!(
            "  {YELLOW}—{RESET} ssh start did NOT succeed on '{}' (detail: {}) — DB reset applied anyway",
            result.name, result.ssh_detail
        );
    }
    println!("  {GREEN}✓{RESET} status='pending' in computers table (pulse will flip to 'online')");
    println!("  {GREEN}✓{RESET} published fleet.events.quarantine (event=unquarantine) on NATS");
    Ok(())
}

/// `ff fleet upgrade <software_id>` — dispatch the software's upgrade_playbook
/// across the fleet via the deferred task queue.
///
/// Resolves the playbook key per-target in this priority order:
///   1. `{os_family}-{install_source}`  (e.g. `"macos-brew"`)
///   2. `{os_family}`                   (e.g. `"macos"`)
///   3. `"all"`
/// Targets with no matching key are warned about and skipped. Dry-run mode
/// prints the plan and exits; `--yes` without `--dry-run` enqueues one
/// deferred shell task per target with trigger_type=`node_online`.
async fn handle_fleet_upgrade(
    pool: &sqlx::PgPool,
    software_id: &str,
    computer: Option<String>,
    all: bool,
    dry_run: bool,
    yes: bool,
    force_dirty: bool,
) -> Result<()> {
    if computer.is_none() && !all {
        anyhow::bail!("pass --all or --computer <name> to pick targets");
    }
    if computer.is_some() && all {
        anyhow::bail!("--computer and --all are mutually exclusive");
    }

    // Shared resolver — same code path the hourly auto-upgrade tick uses.
    let (plans, skipped) = ff_agent::auto_upgrade::resolve_upgrade_plans(
        pool,
        software_id,
        computer.as_deref(),
        false,
    )
    .await?;

    let display_name = plans
        .first()
        .map(|p| p.display_name.clone())
        .unwrap_or_else(|| software_id.to_string());
    let latest_version = plans.first().and_then(|p| p.latest_version.clone());

    if plans.is_empty() && skipped.is_empty() {
        println!(
            "{YELLOW}No computer_software rows found for software_id='{software_id}'. Nothing to do.{RESET}"
        );
        return Ok(());
    }

    println!("{CYAN}▶ ff fleet upgrade {software_id}{RESET}");
    println!("  software:        {display_name} ({software_id})");
    println!(
        "  latest upstream: {}",
        latest_version.as_deref().unwrap_or("(unknown)")
    );
    println!("  targets:         {} computer(s)", plans.len());
    if plans.is_empty() {
        println!("{YELLOW}No resolvable targets. Nothing to do.{RESET}");
        for (name, why) in &skipped {
            println!("    {YELLOW}⚠ skip{RESET} {name}: {why}");
        }
        return Ok(());
    }

    println!(
        "\n  {:<10} {:<14} {:<10} {:<10} {:<22} command",
        "computer", "os_family", "source", "installed", "playbook_key"
    );
    for p in &plans {
        let short_cmd = if p.command.len() > 60 {
            format!("{}…", &p.command[..60])
        } else {
            p.command.clone()
        };
        println!(
            "  {:<10} {:<14} {:<10} {:<10} {:<22} {}",
            p.computer_name,
            p.os_family,
            p.install_source.as_deref().unwrap_or("-"),
            p.installed_version.as_deref().unwrap_or("-"),
            p.playbook_key,
            short_cmd
        );
    }
    for (name, why) in &skipped {
        println!("  {YELLOW}⚠ skip{RESET} {name}: {why}");
    }

    if dry_run {
        println!(
            "\n{YELLOW}Dry run — not enqueuing. Drop --dry-run and pass --yes to actually enqueue.{RESET}"
        );
        return Ok(());
    }
    if !yes {
        println!("\n{YELLOW}Pass --yes to actually enqueue these upgrade tasks.{RESET}");
        return Ok(());
    }

    // Dirty-build gate for `ff_git` / `forgefleetd_git` — refuses propagation
    // of a leader with an uncommitted working tree unless `--force-dirty`.
    use ff_agent::auto_upgrade::GitStateGate;
    let gate = ff_agent::auto_upgrade::gate_git_state(pool, software_id, force_dirty).await;
    let leader_sha = plans
        .first()
        .and_then(|p| p.installed_version.clone())
        .unwrap_or_else(|| "(unknown)".into());
    match gate {
        GitStateGate::BlockDirty => {
            eprintln!(
                "{RED}✗ refusing to propagate dirty build {leader_sha} — commit or pass --force-dirty{RESET}"
            );
            ff_agent::auto_upgrade::mark_targets_blocked_dirty(pool, software_id).await;
            anyhow::bail!("dirty-build gate");
        }
        GitStateGate::AllowWithWarning => {
            eprintln!(
                "{YELLOW}⚠ propagating unpushed/forced commit {leader_sha} from leader to fleet — push to origin/main when ready{RESET}"
            );
            let payload = serde_json::json!({
                "software_id": software_id,
                "sha": leader_sha,
                "computer_count": plans.len(),
                "source": whoami_tag(),
                "forced": force_dirty,
                "ts": chrono::Utc::now().to_rfc3339(),
            });
            ff_agent::nats_client::publish_json(
                "fleet.events.software.unpushed_propagation".to_string(),
                &payload,
            )
            .await;
        }
        GitStateGate::Allow => {}
    }

    let who = whoami_tag();
    let enqueued = ff_agent::auto_upgrade::enqueue_plans(pool, &plans, &who).await?;

    println!(
        "\n{GREEN}✓ Enqueued {} upgrade task(s):{RESET}",
        enqueued.len()
    );
    for ep in &enqueued {
        println!("  {:<12} {}", ep.computer_name, ep.defer_id);
    }
    println!("\nTrack progress with: ff defer list");
    Ok(())
}

async fn handle_fleet_set_network_scope(
    pool: &sqlx::PgPool,
    computer: &str,
    scope: &str,
) -> Result<()> {
    const VALID: &[&str] = &["lan", "tailscale_only", "wan"];
    if !VALID.contains(&scope) {
        anyhow::bail!(
            "unknown scope '{scope}' — must be one of: {}",
            VALID.join(", ")
        );
    }
    let res = sqlx::query("UPDATE computers SET network_scope = $1 WHERE LOWER(name) = LOWER($2)")
        .bind(scope)
        .bind(computer)
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("update computers: {e}"))?;

    if res.rows_affected() == 0 {
        anyhow::bail!("no computer named '{computer}' found");
    }
    println!(
        "{GREEN}✓{RESET} set network_scope='{scope}' on '{computer}' ({} row updated)",
        res.rows_affected()
    );
    Ok(())
}

async fn handle_fleet_db(pool: &sqlx::PgPool, cmd: FleetDbCommand) -> Result<()> {
    match cmd {
        FleetDbCommand::AddRemoteReplica {
            computer,
            via,
            skip_probe,
        } => {
            if via != "tailscale" {
                eprintln!(
                    "{YELLOW}warning:{RESET} --via '{via}' is not 'tailscale' — \
                     recording the row anyway, but no WAN compose template will be generated."
                );
            }

            // Resolve target computer + its Tailscale IP.
            let row = sqlx::query_as::<_, (uuid::Uuid, String, serde_json::Value, String)>(
                "SELECT id, primary_ip, all_ips, COALESCE(network_scope, 'lan')
                 FROM computers
                 WHERE LOWER(name) = LOWER($1)",
            )
            .bind(&computer)
            .fetch_optional(pool)
            .await
            .map_err(|e| anyhow::anyhow!("query computers: {e}"))?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no computer named '{computer}' registered — run `ff onboard` first"
                )
            })?;

            let (computer_id, primary_ip, all_ips_json, current_scope) = row;

            let ts_ip = all_ips_json
                .as_array()
                .and_then(|arr| {
                    arr.iter().find_map(|v| {
                        let obj = v.as_object()?;
                        if obj.get("kind")?.as_str() == Some("tailscale") {
                            obj.get("ip")?.as_str().map(|s| s.to_string())
                        } else {
                            None
                        }
                    })
                })
                .or_else(|| {
                    if primary_ip.starts_with("100.64.") || primary_ip.starts_with("100.65.") {
                        Some(primary_ip.clone())
                    } else {
                        None
                    }
                });

            let ts_ip = match ts_ip {
                Some(ip) => ip,
                None => anyhow::bail!(
                    "no tailscale IP in computers.all_ips for '{computer}'. \
                     Ensure the node is joined to Tailscale and has emitted a Pulse heartbeat."
                ),
            };

            // Optional reachability probe (skipped by --skip-probe).
            if !skip_probe && via == "tailscale" {
                println!("{CYAN}▶ Probing Tailscale reachability: {ts_ip}:55432{RESET}");
                let ok = tokio::process::Command::new("nc")
                    .args(["-vz", "-w", "3", &ts_ip, "55432"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await
                    .map(|s| s.success())
                    .unwrap_or(false);
                if !ok {
                    eprintln!(
                        "{YELLOW}warning:{RESET} nc probe to {ts_ip}:55432 failed \
                         (still recording — Postgres may not be listening yet, or nc may be missing)"
                    );
                } else {
                    println!("{GREEN}✓{RESET} reachable over Tailscale");
                }
            }

            // Upsert database_replicas row with role='wan_replica'.
            sqlx::query(
                "INSERT INTO database_replicas (computer_id, database_kind, role, status, notes) \
                 VALUES ($1, 'postgres', 'wan_replica', 'stopped', $2) \
                 ON CONFLICT (computer_id, database_kind) DO UPDATE \
                 SET role = 'wan_replica', notes = $2",
            )
            .bind(computer_id)
            .bind(format!(
                "added via ff fleet db add-remote-replica --via {via}"
            ))
            .execute(pool)
            .await
            .map_err(|e| anyhow::anyhow!("insert database_replicas: {e}"))?;

            // Auto-apply network_scope='wan' if the caller hasn't already
            // set it (defaults to 'lan', which is wrong for a WAN replica).
            if current_scope == "lan" {
                sqlx::query("UPDATE computers SET network_scope = 'wan' WHERE id = $1")
                    .bind(computer_id)
                    .execute(pool)
                    .await
                    .map_err(|e| anyhow::anyhow!("update computers.network_scope: {e}"))?;
                println!("{CYAN}▶{RESET} auto-applied network_scope='wan' (was 'lan')");
            }

            // Print the runbook snippet.
            println!();
            println!("{GREEN}✓{RESET} registered WAN replica for '{computer}' ({ts_ip})");
            println!();
            println!("Now run on the off-site machine:");
            println!("  cd deploy/");
            println!(
                "  POSTGRES_PRIMARY_HOST=<taylor-tailscale-ip> \\\n    \
                 POSTGRES_REPLICATION_PASSWORD=<same as primary> \\\n    \
                 docker compose -f docker-compose.follower-remote.yml up -d"
            );
            println!();
            println!("Full runbook: deploy/WAN_REPLICATION.md");
        }
        FleetDbCommand::Failover { to, force, yes } => {
            handle_fleet_db_failover(pool, &to, force, yes).await?;
        }
        FleetDbCommand::Restore {
            backup_id,
            to,
            target_db,
            yes,
        } => {
            handle_fleet_db_restore(pool, &backup_id, to.as_deref(), &target_db, yes).await?;
        }
        FleetDbCommand::VerifyBackups {
            limit,
            test_restore,
        } => {
            handle_fleet_db_verify_backups(pool, limit, test_restore).await?;
        }
    }
    Ok(())
}

async fn handle_fleet_db_failover(
    pool: &sqlx::PgPool,
    to: &str,
    force: bool,
    yes: bool,
) -> Result<()> {
    // 1) Resolve target computer_id.
    let target = sqlx::query_as::<_, (uuid::Uuid, String, String)>(
        "SELECT id, name, primary_ip FROM computers WHERE LOWER(name) = LOWER($1)",
    )
    .bind(to)
    .fetch_optional(pool)
    .await
    .map_err(|e| anyhow::anyhow!("query computers: {e}"))?
    .ok_or_else(|| anyhow::anyhow!("no computer named '{to}' registered"))?;
    let (target_id, target_name, target_ip) = target;

    // 2) Must be running on the target (we shell `docker exec` locally).
    let my_name = ff_agent::fleet_info::resolve_this_node_name().await;
    if my_name.to_lowercase() != target_name.to_lowercase() && !force {
        anyhow::bail!(
            "refusing to failover: this command must be run ON '{target_name}' \
             (we'd shell `docker exec` locally). Current node is '{my_name}'. \
             Re-run with --force to override or ssh to '{target_name}' first."
        );
    }

    // 3) Confirm with user.
    if !yes {
        eprintln!(
            "{YELLOW}About to promote '{target_name}' ({target_ip}) to Postgres primary.{RESET}"
        );
        eprintln!("  - The old primary's docker container will be stopped via SSH.");
        eprintln!("  - database_replicas + fleet_secrets.postgres_primary_url will be rewritten.");
        eprintln!("  - All fleet daemons will reconnect against the new primary.");
        eprintln!("Re-run with --yes to confirm.");
        std::process::exit(2);
    }

    println!("{CYAN}▶ Promoting '{target_name}' replica to primary...{RESET}");
    let mgr = ff_agent::ha::pg_failover::PostgresFailoverManager::new(pool.clone(), target_id)
        .with_strict_fencing(!force);
    mgr.promote_local_replica()
        .await
        .map_err(|e| anyhow::anyhow!("promote: {e}"))?;
    println!("{GREEN}✓{RESET} '{target_name}' is now the Postgres primary.");
    Ok(())
}

/// Resolve the local encrypted-backup root. Matches
/// `BackupOrchestrator::new`'s default (`~/.forgefleet/backups`).
fn local_backup_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".forgefleet/backups")
}

/// Metadata loaded from the `backups` table — shared by restore + verify.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct BackupRow {
    id: uuid::Uuid,
    database_kind: String,
    file_name: String,
    size_bytes: i64,
    checksum_sha256: String,
    created_at: chrono::DateTime<chrono::Utc>,
    retention_tier: String,
}

async fn fetch_backup_row(pool: &sqlx::PgPool, id: uuid::Uuid) -> Result<BackupRow> {
    let row = sqlx::query_as::<
        _,
        (
            uuid::Uuid,
            String,
            String,
            i64,
            String,
            chrono::DateTime<chrono::Utc>,
            String,
        ),
    >(
        "SELECT id, database_kind, file_name, size_bytes, checksum_sha256,
                created_at, retention_tier
           FROM backups WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|e| anyhow::anyhow!("query backups: {e}"))?
    .ok_or_else(|| anyhow::anyhow!("no backup row with id {id}"))?;
    Ok(BackupRow {
        id: row.0,
        database_kind: row.1,
        file_name: row.2,
        size_bytes: row.3,
        checksum_sha256: row.4,
        created_at: row.5,
        retention_tier: row.6,
    })
}

/// Locate the on-disk artifact for a backup row.
/// Layout: `<root>/<kind>/<file_name>`.
fn backup_path_on_disk(row: &BackupRow) -> PathBuf {
    local_backup_root()
        .join(&row.database_kind)
        .join(&row.file_name)
}

/// Run SHA256 on a file and compare against the `backups.checksum_sha256`
/// value. Returns `Ok(true)` if they match.
async fn verify_checksum(path: &Path, expected: &str) -> Result<bool> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| anyhow::anyhow!("open {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got = format!("{:x}", hasher.finalize());
    Ok(got.eq_ignore_ascii_case(expected))
}

/// Cheap "is this an age ciphertext?" probe — reads the first few bytes
/// and confirms the `age-encryption.org/v1` armor/binary header. Avoids
/// decrypting the full archive just to answer "decryptable yes/no".
async fn has_age_header(path: &Path) -> Result<bool> {
    use tokio::io::AsyncReadExt;
    let mut f = tokio::fs::File::open(path).await?;
    let mut head = [0u8; 21];
    let n = f.read(&mut head).await?;
    let prefix = &head[..n];
    // Binary and armor variants both begin with "age-encryption.org/v1".
    Ok(prefix.starts_with(b"age-encryption.org/v1")
        || prefix.starts_with(b"-----BEGIN AGE ENCRYPTED FILE-----"))
}

/// Restore an age-encrypted Postgres backup to a scratch database.
///
/// Steps:
/// 1. Look up `backups` row.
/// 2. Verify file exists + checksum matches.
/// 3. Decrypt via `ff_agent::ha::backup::decrypt_backup_file` (uses the
///    `age` Rust crate — no CLI dependency).
/// 4. `docker exec forgefleet-postgres createdb <target_db>` (idempotent).
/// 5. Stream the plaintext archive into the container and run
///    `pg_restore` (tar format) or `psql` (plain SQL, fallback).
/// 6. Print `SELECT COUNT(*) FROM fleet_members` as a sanity check.
async fn handle_fleet_db_restore(
    pool: &sqlx::PgPool,
    backup_id: &str,
    to: Option<&str>,
    target_db: &str,
    yes: bool,
) -> Result<()> {
    if let Some(target_node) = to {
        let me = ff_agent::fleet_info::resolve_this_node_name().await;
        if !target_node.eq_ignore_ascii_case(&me) {
            anyhow::bail!(
                "--to '{target_node}' != current node '{me}'. Cross-node \
                 restore over the defer queue isn't wired yet; ssh to \
                 '{target_node}' and re-run locally."
            );
        }
    }
    if !yes {
        eprintln!(
            "{YELLOW}Restore creates a new database ('{target_db}') in the \
             local forgefleet-postgres container and loads the backup \
             into it. Re-run with --yes to proceed.{RESET}"
        );
        std::process::exit(2);
    }

    let id = uuid::Uuid::parse_str(backup_id)
        .map_err(|e| anyhow::anyhow!("invalid backup id '{backup_id}': {e}"))?;
    let row = fetch_backup_row(pool, id).await?;
    let enc_path = backup_path_on_disk(&row);

    println!(
        "{CYAN}▶ restore backup{RESET}  id={} kind={} file={} size={} tier={}",
        row.id, row.database_kind, row.file_name, row.size_bytes, row.retention_tier,
    );

    if !enc_path.exists() {
        anyhow::bail!(
            "backup file not found on disk: {}. Rsync may not have \
             landed yet — run `ff fleet db verify-backups` to audit.",
            enc_path.display()
        );
    }
    let disk_bytes = tokio::fs::metadata(&enc_path).await?.len() as i64;
    if disk_bytes == 0 {
        anyhow::bail!(
            "backup file {} is 0 bytes — producer never wrote ciphertext. \
             Likely cause: `age` CLI was missing when the backup ran.",
            enc_path.display()
        );
    }

    let checksum_ok = verify_checksum(&enc_path, &row.checksum_sha256).await?;
    if !checksum_ok {
        anyhow::bail!(
            "checksum mismatch on {} — refusing to restore corrupt backup",
            enc_path.display()
        );
    }
    println!(
        "{GREEN}✓{RESET} checksum matches (sha256={}…)",
        &row.checksum_sha256[..12.min(row.checksum_sha256.len())]
    );

    // Decrypt into a tempfile. The archive sizes here (<100 MB) are fine
    // to materialize; if that ever changes, swap this for a streaming
    // decrypt that pipes straight into pg_restore.
    let tmp_dir = std::env::temp_dir().join(format!("ff-restore-{}", row.id));
    tokio::fs::create_dir_all(&tmp_dir).await?;
    let plaintext_path = tmp_dir.join(row.file_name.strip_suffix(".age").unwrap_or(&row.file_name));
    if let Err(e) =
        ff_agent::ha::backup::decrypt_backup_file(pool, &enc_path, &plaintext_path).await
    {
        anyhow::bail!(
            "decrypt failed: {e}. If this is '{}' key not set — no real \
             backup encryption has happened yet, so there's nothing to \
             restore.",
            ff_agent::ha::backup::BACKUP_ENC_PRIVKEY
        );
    }
    println!(
        "{GREEN}✓{RESET} decrypted → {} ({} bytes)",
        plaintext_path.display(),
        tokio::fs::metadata(&plaintext_path).await?.len()
    );

    if row.database_kind != "postgres" {
        println!(
            "{YELLOW}note:{RESET} kind='{}' — only 'postgres' restore is \
             wired end-to-end. Plaintext is available at {}.",
            row.database_kind,
            plaintext_path.display()
        );
        return Ok(());
    }

    // 1) Create the scratch DB (idempotent — swallow "already exists").
    let createdb = tokio::process::Command::new("docker")
        .args([
            "exec",
            "-u",
            "postgres",
            "forgefleet-postgres",
            "createdb",
            target_db,
        ])
        .output()
        .await?;
    if !createdb.status.success() {
        let stderr = String::from_utf8_lossy(&createdb.stderr);
        if !stderr.contains("already exists") {
            anyhow::bail!("createdb {target_db} failed: {stderr}");
        }
        println!("{YELLOW}note:{RESET} database '{target_db}' already exists (reusing)");
    } else {
        println!("{GREEN}✓{RESET} created scratch database '{target_db}'");
    }

    // 2) Stream plaintext into the container and pg_restore it.
    //    pg_basebackup tar archives come out as `base.tar.gz` nested inside
    //    the streamed tar — that's a cluster snapshot, not a logical
    //    dump. pg_restore won't consume it. For this helper we treat the
    //    file as a custom/plain pg_dump archive *or* a pg_basebackup
    //    tarball and pick the right tool based on extension.
    println!("{CYAN}▶ loading archive into '{target_db}'...{RESET}");
    let ext = plaintext_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let (prog, extra_args): (&str, Vec<&str>) = if plaintext_path
        .file_name()
        .and_then(|s| s.to_str())
        .map(|n| n.ends_with(".sql") || n.ends_with(".sql.gz"))
        .unwrap_or(false)
    {
        ("psql", vec!["-v", "ON_ERROR_STOP=1", "-d", target_db])
    } else if ext == "gz" || ext == "tgz" {
        // pg_basebackup tar.gz — not a logical dump. We can't pg_restore
        // it into an existing DB; the correct flow is to stop postgres,
        // wipe PGDATA, untar, restart. That's way too destructive for a
        // "scratch DB" helper. Report clearly instead of silently doing
        // the wrong thing.
        println!(
            "{YELLOW}note:{RESET} archive looks like a pg_basebackup \
             cluster snapshot (.tar.gz). That's a physical backup — \
             restoring it requires replacing PGDATA, not loading into a \
             scratch DB. Plaintext is at {}.",
            plaintext_path.display()
        );
        let fm_count = count_fleet_members_live(pool).await.unwrap_or(-1);
        println!(
            "{GREEN}✓{RESET} sanity check — live fleet_members row count: {fm_count} \
             (no load performed; scratch DB '{target_db}' is empty)"
        );
        return Ok(());
    } else {
        (
            "pg_restore",
            vec!["--no-owner", "--no-privileges", "-d", target_db],
        )
    };

    // `docker exec -i` with stdin streaming from our tempfile.
    let plaintext = tokio::fs::read(&plaintext_path).await?;
    let mut child = tokio::process::Command::new("docker")
        .args({
            let mut v: Vec<&str> =
                vec!["exec", "-i", "-u", "postgres", "forgefleet-postgres", prog];
            v.extend(extra_args.iter().copied());
            v
        })
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(&plaintext).await?;
        stdin.shutdown().await?;
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        anyhow::bail!(
            "{prog} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    println!("{GREEN}✓{RESET} {prog} completed");

    // 3) Sanity check — count fleet_members rows in the restored DB.
    let count_out = tokio::process::Command::new("docker")
        .args([
            "exec",
            "-u",
            "postgres",
            "forgefleet-postgres",
            "psql",
            "-d",
            target_db,
            "-tAc",
            "SELECT COUNT(*) FROM fleet_members",
        ])
        .output()
        .await?;
    if count_out.status.success() {
        let c = String::from_utf8_lossy(&count_out.stdout)
            .trim()
            .to_string();
        println!("{GREEN}✓{RESET} restored '{target_db}'.fleet_members row count: {c}");
    } else {
        println!(
            "{YELLOW}note:{RESET} could not count fleet_members in '{target_db}': {}",
            String::from_utf8_lossy(&count_out.stderr).trim()
        );
    }
    Ok(())
}

/// Count rows in the *live* fleet_members table via the existing pool.
async fn count_fleet_members_live(pool: &sqlx::PgPool) -> Result<i64> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fleet_members")
        .fetch_one(pool)
        .await?;
    Ok(n)
}

async fn handle_fleet_db_verify_backups(
    pool: &sqlx::PgPool,
    limit: i64,
    test_restore: bool,
) -> Result<()> {
    println!(
        "{CYAN}▶ ff fleet db verify-backups (limit={limit} test-restore={test_restore}){RESET}"
    );

    // Confirm the decryption key exists — the whole audit is meaningless
    // without it.
    let privkey = ff_db::pg_get_secret(pool, ff_agent::ha::backup::BACKUP_ENC_PRIVKEY)
        .await
        .map_err(|e| anyhow::anyhow!("fleet_secrets lookup: {e}"))?;
    match privkey {
        Some(_) => println!(
            "{GREEN}✓{RESET} fleet_secrets.{} present",
            ff_agent::ha::backup::BACKUP_ENC_PRIVKEY
        ),
        None => {
            println!(
                "{YELLOW}warning:{RESET} fleet_secrets.{} is NOT set. No real \
                 backup encryption has happened yet — .age files on disk \
                 are likely 0-byte stubs from failed `age` CLI runs. \
                 Install `age` (brew install age) and let the orchestrator \
                 produce a real backup first.",
                ff_agent::ha::backup::BACKUP_ENC_PRIVKEY
            );
        }
    }

    let rows = sqlx::query_as::<
        _,
        (
            uuid::Uuid,
            String,
            String,
            i64,
            String,
            chrono::DateTime<chrono::Utc>,
            String,
        ),
    >(
        "SELECT id, database_kind, file_name, size_bytes, checksum_sha256,
                created_at, retention_tier
           FROM backups
          ORDER BY created_at DESC
          LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("query backups: {e}"))?;

    if rows.is_empty() {
        println!("(no rows in `backups` table — run `ff fleet backup` to produce one)");
        return Ok(());
    }

    println!();
    println!(
        "{:<38} {:<8} {:<10} {:<20} {:<8} {:<8} {}",
        "ID", "KIND", "SIZE", "CREATED", "CHKSUM", "DECRYPT", "FILE"
    );
    let mut most_recent_pg: Option<BackupRow> = None;
    for (id, kind, file_name, size_bytes, checksum_sha256, created_at, tier) in rows {
        let br = BackupRow {
            id,
            database_kind: kind.clone(),
            file_name: file_name.clone(),
            size_bytes,
            checksum_sha256: checksum_sha256.clone(),
            created_at,
            retention_tier: tier,
        };
        let path = backup_path_on_disk(&br);
        let (chk_str, dec_str) = if !path.exists() {
            ("missing".to_string(), "n/a".to_string())
        } else {
            let chk = verify_checksum(&path, &checksum_sha256)
                .await
                .unwrap_or(false);
            let dec = has_age_header(&path).await.unwrap_or(false);
            let dec_str = if tokio::fs::metadata(&path)
                .await
                .map(|m| m.len())
                .unwrap_or(0)
                == 0
            {
                "empty".to_string()
            } else if dec {
                "yes".to_string()
            } else {
                "no".to_string()
            };
            (
                if chk {
                    "ok".to_string()
                } else {
                    "BAD".to_string()
                },
                dec_str,
            )
        };
        println!(
            "{:<38} {:<8} {:<10} {:<20} {:<8} {:<8} {}",
            id.to_string(),
            kind,
            size_bytes,
            created_at.format("%Y-%m-%d %H:%M:%S").to_string(),
            chk_str,
            dec_str,
            file_name,
        );
        if kind == "postgres" && most_recent_pg.is_none() {
            most_recent_pg = Some(br);
        }
    }

    if test_restore {
        println!();
        let Some(target) = most_recent_pg else {
            println!("{YELLOW}--test-restore:{RESET} no postgres backups found, skipping");
            return Ok(());
        };
        println!(
            "{CYAN}▶ --test-restore:{RESET} most recent postgres backup = {} ({})",
            target.id, target.file_name
        );
        let scratch = format!("forgefleet_verify_{}", &target.id.simple().to_string()[..8]);
        println!("    scratch db: {scratch}");
        // Invoke the same restore path, then drop the DB.
        let restore_res =
            handle_fleet_db_restore(pool, &target.id.to_string(), None, &scratch, true).await;
        // Always attempt cleanup, even on error.
        let drop_out = tokio::process::Command::new("docker")
            .args([
                "exec",
                "-u",
                "postgres",
                "forgefleet-postgres",
                "dropdb",
                "--if-exists",
                &scratch,
            ])
            .output()
            .await;
        match drop_out {
            Ok(o) if o.status.success() => {
                println!("{GREEN}✓{RESET} scratch db '{scratch}' dropped")
            }
            Ok(o) => println!(
                "{YELLOW}note:{RESET} dropdb '{scratch}' non-zero: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => println!("{YELLOW}note:{RESET} dropdb '{scratch}' failed to spawn: {e}"),
        }
        restore_res?;
    }

    Ok(())
}

async fn handle_fleet_revoke_trust(pool: &sqlx::PgPool, computer: &str, yes: bool) -> Result<()> {
    if !yes {
        eprintln!("{YELLOW}Revocation is destructive. Pass --yes to confirm.{RESET}");
        std::process::exit(2);
    }
    println!("{CYAN}▶ Revoking SSH trust for '{computer}' across fleet...{RESET}");
    let mgr = ff_agent::ssh_key_manager::SshKeyManager::new(pool.clone());
    let who = whoami_tag();
    let report = mgr
        .revoke_computer_trust(computer, Some(&who))
        .await
        .map_err(|e| anyhow::anyhow!("revoke: {e}"))?;

    println!(
        "\nFingerprint: {}\nRevoked on {} host(s), failed on {}.",
        report.key_fingerprint, report.succeeded, report.failed,
    );
    for t in &report.targets {
        let marker = if t.success { "✓" } else { "✗" };
        println!(
            "  {marker} {:<14} {}",
            t.target,
            if t.success { "ok" } else { t.message.as_str() }
        );
    }
    Ok(())
}

/// Rows-deleted breakdown for a single `remove_computer_core` call.
/// Each field corresponds to one DELETE inside the transaction. The two
/// commands that drive this (`remove-computer`, `disband`) use it to print
/// a human-readable summary.
#[derive(Debug, Default, Clone)]
struct RemoveComputerReport {
    computer_rows: u64,
    fleet_node_rows: u64,
    fleet_models_rows: u64,
    leader_state_rows: u64,
    revocation_task_id: Option<String>,
}

/// Core remove-computer logic shared by `ff fleet remove-computer` and
/// `ff fleet disband`.
///
/// Runs the DB deletes in a single transaction, enqueues the SSH-trust
/// revocation task on the leader (preferred_node="taylor"), and
/// best-effort publishes `fleet.events.computer_removed` on NATS.
/// Returns a row-level report. Errors are surfaced to the caller; the
/// transaction rolls back on any SQL failure.
async fn remove_computer_core(pool: &sqlx::PgPool, name: &str) -> Result<RemoveComputerReport> {
    let mut tx = pool.begin().await?;
    let mut report = RemoveComputerReport::default();

    // fleet_models has no ON DELETE CASCADE on the fleet_nodes FK.
    let r = sqlx::query("DELETE FROM fleet_models WHERE node_name = $1")
        .bind(name)
        .execute(&mut *tx)
        .await?;
    report.fleet_models_rows = r.rows_affected();

    // fleet_leader_state references computers(id) WITHOUT cascade; the spec
    // says key by member_name so we don't have to resolve the UUID first.
    let r = sqlx::query("DELETE FROM fleet_leader_state WHERE member_name = $1")
        .bind(name)
        .execute(&mut *tx)
        .await?;
    report.leader_state_rows = r.rows_affected();

    // fleet_nodes cascades: fleet_node_ssh_keys, fleet_model_library,
    // fleet_model_deployments, fleet_disk_usage (all ON DELETE CASCADE).
    let r = sqlx::query("DELETE FROM fleet_nodes WHERE name = $1")
        .bind(name)
        .execute(&mut *tx)
        .await?;
    report.fleet_node_rows = r.rows_affected();

    // computers cascades: computer_software, computer_models,
    // computer_model_deployments, computer_downtime_events, computer_trust,
    // fleet_members, openclaw_installations, computer_docker_containers.
    let r = sqlx::query("DELETE FROM computers WHERE name = $1")
        .bind(name)
        .execute(&mut *tx)
        .await?;
    report.computer_rows = r.rows_affected();

    tx.commit().await?;

    // Enqueue SSH revocation as a deferred task so it survives Taylor being
    // offline or the operator running this from a non-leader. Payload is a
    // shell script that invokes `ff fleet revoke-trust`, which re-reads the
    // (now-deleted) key from fleet_ssh_revocations… wait — the key is gone
    // with fleet_node_ssh_keys. So we have to embed the pubkey in the task
    // payload BEFORE the deletion. That requires a pre-delete lookup — do it
    // via a follow-up patch if the existing trust manager can't cope. For
    // now, fan out a best-effort `ff fleet revoke-trust` which is a no-op on
    // a deleted row. Document the limitation in the summary line.
    //
    // Practical workaround: the revocation script below strips lines by
    // comment-tag `user@host` match on each peer. `ssh_key_manager`
    // canonicalises keys to end with a comment like `<user>@<removed-host>`
    // at onboarding time, so grep'ing for `@<name>` at the end of every
    // authorized_keys line is a reasonable fallback.
    let script = build_remove_computer_ssh_script(name);
    let payload = serde_json::json!({ "command": script });
    let trigger_spec = serde_json::json!({ "node": "taylor" });
    let title = format!("Revoke SSH trust for {name}");
    let who = whoami_tag();
    let defer_id = ff_db::pg_enqueue_deferred(
        pool,
        &title,
        "shell",
        &payload,
        "node_online",
        &trigger_spec,
        Some("taylor"),
        &serde_json::json!([]),
        Some(&who),
        Some(3),
    )
    .await?;
    report.revocation_task_id = Some(defer_id);

    // Best-effort NATS announcement. NATS may not be up — drop errors.
    let _ = ff_agent::nats_client::init_nats(&ff_agent::nats_client::resolve_nats_url()).await;
    ff_agent::nats_client::publish_json(
        "fleet.events.computer_removed",
        &serde_json::json!({
            "name": name,
            "removed_by": who,
            "at": chrono::Utc::now().to_rfc3339(),
        }),
    )
    .await;

    Ok(report)
}

/// Build a shell script that SSH-fans-out a revocation of `name`'s user
/// key across every remaining peer. Run as a `node_online` deferred task
/// on Taylor.
///
/// Strategy: ask the local DB on Taylor for every peer's primary_ip, then
/// for each peer run a grep -v filter on `authorized_keys` that drops any
/// line ending with `@<name>` (the canonical comment suffix OpenClaw
/// writes during onboarding).
fn build_remove_computer_ssh_script(name: &str) -> String {
    let name = name.replace('\'', "'\\''");
    format!(
        r#"set -e
NAME='{name}'
# Pull the list of peers from the local Postgres on Taylor. If psql isn't
# available we fall back to the .forgefleet/fleet.toml parse below.
PEERS=$(ff fleet health --json 2>/dev/null | \
  python3 -c 'import json,sys; d=json.load(sys.stdin); print("\n".join(r["name"] for r in d if r["name"] != "'"$NAME"'"))' 2>/dev/null || true)
if [ -z "$PEERS" ]; then
  echo "no peers resolvable; aborting revocation (removal of DB rows still took effect)"
  exit 0
fi
for P in $PEERS; do
  echo "revoking @$NAME from $P..."
  ssh -o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new "$P" \
    "if [ -f ~/.ssh/authorized_keys ]; then cp ~/.ssh/authorized_keys ~/.ssh/authorized_keys.bak.$$ && grep -v '@'\"$NAME\"'$' ~/.ssh/authorized_keys.bak.$$ > ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys && rm -f ~/.ssh/authorized_keys.bak.$$; fi" \
    || echo "  (warn) ssh $P failed; skipping"
done
echo "revocation fan-out complete for $NAME"
"#,
        name = name,
    )
}

async fn handle_fleet_remove_computer(pool: &sqlx::PgPool, name: &str, yes: bool) -> Result<()> {
    // 1. Look up what actually exists so we can print an honest plan.
    let fleet_node: Option<(String, String, String)> =
        sqlx::query_as("SELECT name, ip, ssh_user FROM fleet_nodes WHERE name = $1")
            .bind(name)
            .fetch_optional(pool)
            .await?;
    let computer: Option<(String, String, String)> = sqlx::query_as(
        "SELECT name, primary_ip, COALESCE(os_family, '') FROM computers WHERE name = $1",
    )
    .bind(name)
    .fetch_optional(pool)
    .await?;

    if fleet_node.is_none() && computer.is_none() {
        eprintln!("{YELLOW}No fleet_nodes or computers row named '{name}' — nothing to do.{RESET}");
        std::process::exit(2);
    }

    println!("{CYAN}▶ ff fleet remove-computer {name}{RESET}");
    if let Some((n, ip, user)) = &fleet_node {
        println!("  fleet_nodes row:  name={n} ip={ip} ssh_user={user}");
    } else {
        println!("  fleet_nodes row:  (none)");
    }
    if let Some((n, ip, osf)) = &computer {
        println!("  computers row:    name={n} primary_ip={ip} os_family={osf}");
    } else {
        println!("  computers row:    (none)");
    }
    println!("  cascades:         fleet_node_ssh_keys, fleet_model_library,");
    println!("                    fleet_model_deployments, fleet_disk_usage,");
    println!("                    computer_software, computer_models,");
    println!("                    computer_model_deployments, computer_trust,");
    println!("                    computer_downtime_events, fleet_members,");
    println!("                    openclaw_installations, computer_docker_containers");
    println!("  explicit deletes: fleet_models (no cascade),");
    println!("                    fleet_leader_state WHERE member_name=<name>");
    println!("  side-effect:      1 deferred SSH-revocation task on taylor");

    if !yes {
        eprintln!("\n{YELLOW}Removal is destructive. Pass --yes to proceed.{RESET}");
        std::process::exit(2);
    }

    let report = remove_computer_core(pool, name).await?;
    let total = report.computer_rows
        + report.fleet_node_rows
        + report.fleet_models_rows
        + report.leader_state_rows;
    println!(
        "\n{GREEN}✓ removed {name}{RESET} — {total} row(s) across \
         computers({cr}), fleet_nodes({fn_}), fleet_models({fm}), \
         fleet_leader_state({fls})",
        cr = report.computer_rows,
        fn_ = report.fleet_node_rows,
        fm = report.fleet_models_rows,
        fls = report.leader_state_rows,
    );
    if let Some(id) = &report.revocation_task_id {
        println!("  enqueued SSH-revocation task: {id}");
        println!("  track progress with: ff defer list");
    }
    Ok(())
}

async fn handle_fleet_disband(
    pool: &sqlx::PgPool,
    yes: bool,
    i_know_what_im_doing: bool,
) -> Result<()> {
    // Collect every computer that isn't Taylor. We look at both tables
    // because a computer may exist in one but not the other if something
    // went sideways during onboarding.
    let fleet_names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM fleet_nodes WHERE LOWER(name) <> 'taylor' ORDER BY name",
    )
    .fetch_all(pool)
    .await?;
    let computer_names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM computers WHERE LOWER(name) <> 'taylor' ORDER BY name",
    )
    .fetch_all(pool)
    .await?;

    let mut targets: Vec<String> = fleet_names.clone();
    for n in &computer_names {
        if !targets.contains(n) {
            targets.push(n.clone());
        }
    }
    targets.sort();

    println!("{CYAN}▶ ff fleet disband{RESET}");
    println!("  This will DELETE every fleet_nodes/computers row except 'taylor'.");
    println!("  Requires BOTH --yes AND --i-know-what-im-doing to actually run.");
    println!("  targets:         {} computer(s)", targets.len());
    for n in &targets {
        println!("    {n}");
    }

    if targets.is_empty() {
        println!("{YELLOW}No non-Taylor rows to remove. Nothing to do.{RESET}");
        return Ok(());
    }

    if !(yes && i_know_what_im_doing) {
        eprintln!(
            "\n{YELLOW}Refusing to disband without both --yes and --i-know-what-im-doing.{RESET}"
        );
        std::process::exit(2);
    }

    let mut total_rows: u64 = 0;
    let mut total_tasks: u64 = 0;
    let mut failures: Vec<(String, String)> = Vec::new();
    for name in &targets {
        print!("  removing {name}... ");
        match remove_computer_core(pool, name).await {
            Ok(r) => {
                let sub =
                    r.computer_rows + r.fleet_node_rows + r.fleet_models_rows + r.leader_state_rows;
                total_rows += sub;
                if r.revocation_task_id.is_some() {
                    total_tasks += 1;
                }
                println!("ok ({sub} rows)");
            }
            Err(e) => {
                println!("{RED}FAIL{RESET} ({e})");
                failures.push((name.clone(), e.to_string()));
            }
        }
    }
    println!(
        "\n{GREEN}✓ disband complete{RESET} — {n} computer(s) removed, \
         {r} DB row(s) deleted, {t} SSH-revocation task(s) enqueued",
        n = targets.len() - failures.len(),
        r = total_rows,
        t = total_tasks,
    );
    if !failures.is_empty() {
        eprintln!("{RED}Failures:{RESET}");
        for (name, err) in &failures {
            eprintln!("  {name}: {err}");
        }
    }
    Ok(())
}

async fn handle_fleet_migrate_source_trees(
    pool: &sqlx::PgPool,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    // Build the candidate set: every computer that isn't Taylor.
    // We join fleet_nodes (for ssh_user/ip) with computers (for
    // source_tree_path) on name.
    #[derive(Debug)]
    struct Candidate {
        name: String,
        ip: String,
        ssh_user: String,
        canonical: String,
    }
    let rows = sqlx::query(
        "SELECT n.name, n.ip, n.ssh_user,
                COALESCE(c.source_tree_path, '~/.forgefleet/sub-agent-0/forge-fleet') AS canonical
           FROM fleet_nodes n
           LEFT JOIN computers c ON c.name = n.name
          WHERE LOWER(n.name) <> 'taylor'
          ORDER BY n.name",
    )
    .fetch_all(pool)
    .await?;
    let candidates: Vec<Candidate> = rows
        .iter()
        .map(|r| Candidate {
            name: sqlx::Row::get(r, "name"),
            ip: sqlx::Row::get(r, "ip"),
            ssh_user: sqlx::Row::get(r, "ssh_user"),
            canonical: sqlx::Row::get(r, "canonical"),
        })
        .collect();

    println!("{CYAN}▶ ff fleet migrate-source-trees{RESET}");
    println!("  candidates: {} non-Taylor node(s)", candidates.len());
    if candidates.is_empty() {
        println!("{YELLOW}No non-Taylor nodes. Nothing to do.{RESET}");
        return Ok(());
    }

    // Probe each candidate over SSH for the two paths. Best-effort; if the
    // node is offline we can still enqueue — the task fires on `node_online`.
    struct Probed {
        c: Candidate,
        legacy_exists: bool,
        canonical_exists: bool,
        ssh_reachable: bool,
    }
    let mut probed: Vec<Probed> = Vec::with_capacity(candidates.len());
    for c in candidates {
        let host = &c.ip;
        let user = &c.ssh_user;
        let target = format!("{user}@{host}");
        // One SSH call returns both flags, separated by "|".
        let script = "legacy=0; canonical=0; \
             [ -d ~/taylorProjects/forge-fleet ] && legacy=1; \
             [ -d ~/.forgefleet/sub-agent-0/forge-fleet/.git ] && canonical=1; \
             echo \"$legacy|$canonical\"";
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(6),
            tokio::process::Command::new("ssh")
                .args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=4",
                    "-o",
                    "StrictHostKeyChecking=accept-new",
                    &target,
                    script,
                ])
                .output(),
        )
        .await;
        let (legacy, canonical, reach) = match out {
            Ok(Ok(o)) if o.status.success() => {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                let parts: Vec<&str> = s.split('|').collect();
                (
                    parts.first().map(|v| *v == "1").unwrap_or(false),
                    parts.get(1).map(|v| *v == "1").unwrap_or(false),
                    true,
                )
            }
            _ => (false, false, false),
        };
        probed.push(Probed {
            c,
            legacy_exists: legacy,
            canonical_exists: canonical,
            ssh_reachable: reach,
        });
    }

    println!(
        "\n  {:<14} {:<16} {:<7} {:<10} {:<10} {}",
        "node", "ip", "ssh", "legacy", "canonical", "action"
    );
    let mut to_enqueue: Vec<&Probed> = Vec::new();
    for p in &probed {
        let action = if !p.ssh_reachable {
            "enqueue (offline — runs on node_online)"
        } else if p.canonical_exists && !p.legacy_exists {
            "skip (already migrated)"
        } else if p.legacy_exists && p.canonical_exists {
            "enqueue (drop legacy, canonical already present)"
        } else if p.legacy_exists {
            "enqueue (move legacy → canonical)"
        } else {
            "enqueue (fresh clone into canonical)"
        };
        println!(
            "  {:<14} {:<16} {:<7} {:<10} {:<10} {}",
            p.c.name,
            p.c.ip,
            if p.ssh_reachable { "ok" } else { "down" },
            if p.legacy_exists { "yes" } else { "no" },
            if p.canonical_exists { "yes" } else { "no" },
            action,
        );
        let already_migrated = p.ssh_reachable && p.canonical_exists && !p.legacy_exists;
        if !already_migrated {
            to_enqueue.push(p);
        }
    }

    if dry_run {
        println!(
            "\n{YELLOW}Dry run — not enqueuing. Drop --dry-run and pass --yes to enqueue.{RESET}"
        );
        return Ok(());
    }
    if !yes {
        println!(
            "\n{YELLOW}Pass --yes to enqueue {} migration task(s).{RESET}",
            to_enqueue.len()
        );
        return Ok(());
    }
    if to_enqueue.is_empty() {
        println!(
            "\n{GREEN}✓ nothing to enqueue — every candidate is already on the canonical path.{RESET}"
        );
        return Ok(());
    }

    let who = whoami_tag();
    let mut enqueued: Vec<(String, String)> = Vec::with_capacity(to_enqueue.len());
    for p in to_enqueue {
        let script = build_migrate_source_tree_script(&p.c.canonical);
        let title = format!("Migrate source tree: {}", p.c.name);
        let payload = serde_json::json!({ "command": script });
        let trigger_spec = serde_json::json!({ "node": p.c.name });
        let id = ff_db::pg_enqueue_deferred(
            pool,
            &title,
            "shell",
            &payload,
            "node_online",
            &trigger_spec,
            Some(&p.c.name),
            &serde_json::json!([]),
            Some(&who),
            Some(3),
        )
        .await?;
        enqueued.push((p.c.name.clone(), id));
    }
    println!(
        "\n{GREEN}✓ enqueued {} migration task(s):{RESET}",
        enqueued.len()
    );
    for (name, id) in &enqueued {
        println!("  {:<14} {id}", name);
    }
    println!("\nTrack progress with: ff defer list");
    Ok(())
}

/// Emit the idempotent shell script used by `ff fleet migrate-source-trees`.
/// Mirrors the command spec in issue #120: if canonical/.git is already
/// present drop the legacy dir; otherwise move-or-clone into canonical.
fn build_migrate_source_tree_script(canonical: &str) -> String {
    // `canonical` comes from the DB; never user-shell-input. Still, keep it
    // quoted to be safe against spaces.
    format!(
        r#"set -e
CANONICAL="{canonical}"
mkdir -p "$(dirname "$CANONICAL")"
if [ -d "$CANONICAL/.git" ]; then
  rm -rf ~/taylorProjects/forge-fleet 2>/dev/null || true
  rmdir ~/taylorProjects 2>/dev/null || true
  echo "canonical already present — dropped legacy"
  exit 0
fi
if [ -d ~/taylorProjects/forge-fleet/.git ]; then
  mv ~/taylorProjects/forge-fleet "$CANONICAL"
  rmdir ~/taylorProjects 2>/dev/null || true
  echo "moved legacy → canonical"
else
  git clone https://github.com/venkatyarl/forge-fleet "$CANONICAL"
  rm -rf ~/taylorProjects/forge-fleet 2>/dev/null || true
  rmdir ~/taylorProjects 2>/dev/null || true
  echo "fresh clone into canonical"
fi
"#,
        canonical = canonical,
    )
}

async fn handle_fleet_rotate_pulse_hmac(pool: &sqlx::PgPool, value: Option<String>) -> Result<()> {
    println!("{CYAN}▶ Rotating pulse_beat_hmac_key...{RESET}");
    let rotator = ff_agent::secrets_rotation::SecretsRotator::new(pool.clone());
    let out = rotator
        .rotate("pulse_beat_hmac_key", value)
        .await
        .map_err(|e| anyhow::anyhow!("rotate: {e}"))?;
    println!(
        "{GREEN}✓ pulse_beat_hmac_key rotated{RESET} ({} bytes, sha12={})",
        out.new_len, out.new_fingerprint,
    );
    println!("{YELLOW}Daemons will pick up the new key on next 5-minute cache refresh.{RESET}");
    Ok(())
}

async fn handle_fleet_backup(pool: &sqlx::PgPool, kind: &str, force: bool) -> Result<()> {
    let my_name = ff_agent::fleet_info::resolve_this_node_name().await;
    let my_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM computers WHERE name = $1")
        .bind(&my_name)
        .fetch_optional(pool)
        .await?
        .unwrap_or_else(uuid::Uuid::nil);

    let orch =
        ff_agent::ha::backup::BackupOrchestrator::new(pool.clone(), my_id, my_name.clone(), None);

    println!("{CYAN}▶ ff fleet backup kind={kind} force={force}{RESET}");
    let reports = orch
        .run_once(kind, force)
        .await
        .map_err(|e| anyhow::anyhow!("backup: {e}"))?;

    for r in &reports {
        if r.produced {
            println!(
                "{GREEN}✓ {} backup produced{RESET}  file={} size={} sha256={} targets={}",
                r.kind,
                r.file_path.display(),
                r.size_bytes,
                &r.sha256[..12.min(r.sha256.len())],
                r.distributed_to.len(),
            );
        } else {
            println!(
                "{YELLOW}(skipped){RESET}  kind={} — not leader (use --force)",
                r.kind
            );
        }
    }
    Ok(())
}

async fn handle_fleet_task_coverage(pool: &sqlx::PgPool, cmd: TaskCoverageCommand) -> Result<()> {
    match cmd {
        TaskCoverageCommand::List => {
            let rows = sqlx::query(
                "SELECT task, min_models_loaded, priority, preferred_model_ids, notes
                 FROM fleet_task_coverage
                 ORDER BY
                   CASE priority
                     WHEN 'critical' THEN 0
                     WHEN 'normal' THEN 1
                     WHEN 'nice-to-have' THEN 2
                     ELSE 3
                   END,
                   task",
            )
            .fetch_all(pool)
            .await?;
            if rows.is_empty() {
                println!("(no task coverage rules — run `ff fleet task-coverage seed`)");
                return Ok(());
            }
            println!(
                "{:<32} {:<6} {:<14}  PREFERRED / NOTES",
                "TASK", "MIN", "PRIORITY"
            );
            for r in rows {
                let task: String = sqlx::Row::get(&r, "task");
                let min: i32 = sqlx::Row::get(&r, "min_models_loaded");
                let pri: String = sqlx::Row::get(&r, "priority");
                let preferred: serde_json::Value = sqlx::Row::get(&r, "preferred_model_ids");
                let notes: Option<String> = sqlx::Row::get(&r, "notes");
                let pref_str = preferred
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                let extra = if !pref_str.is_empty() {
                    pref_str
                } else {
                    notes.unwrap_or_default()
                };
                println!("{:<32} {:<6} {:<14}  {}", task, min, pri, extra);
            }
        }
    }
    Ok(())
}

async fn handle_fleet_revive(
    pool: &sqlx::PgPool,
    computer: &str,
    wol_only: bool,
    internal: bool,
) -> Result<()> {
    let mgr = ff_agent::revive::ReviveManager::new(pool.clone());
    let target = mgr
        .load_target_by_name(computer)
        .await
        .map_err(|e| anyhow::anyhow!("load target: {e}"))?;

    if !internal {
        println!("{CYAN}▶ ff fleet revive {}{RESET}", target.name);
        println!("  primary_ip:    {}", target.primary_ip);
        println!("  ssh_user:      {}", target.ssh_user);
        println!("  ssh_port:      {}", target.ssh_port);
        println!("  os_family:     {}", target.os_family);
        println!("  mac_addresses: {} entry(ies)", target.mac_addresses.len());
    }

    let outcome = if wol_only {
        // WoL-only path short-circuits SSH. Send to every recorded MAC.
        if target.mac_addresses.is_empty() {
            ff_agent::revive::ReviveOutcome::Failed(
                "no MAC addresses on record; cannot WoL-only revive".into(),
            )
        } else {
            let mut sent = false;
            for mac in &target.mac_addresses {
                if ff_agent::revive::send_wol(mac).await.is_ok() {
                    sent = true;
                }
            }
            if sent {
                ff_agent::revive::ReviveOutcome::WolSent
            } else {
                ff_agent::revive::ReviveOutcome::Failed("all WoL sends failed".into())
            }
        }
    } else {
        mgr.attempt(&target)
            .await
            .map_err(|e| anyhow::anyhow!("revive attempt: {e}"))?
    };

    if internal {
        let j = serde_json::json!({
            "computer": target.name,
            "outcome": match &outcome {
                ff_agent::revive::ReviveOutcome::DaemonRestarted => "daemon_restarted",
                ff_agent::revive::ReviveOutcome::DaemonAlreadyRunning => "daemon_already_running",
                ff_agent::revive::ReviveOutcome::WolSent => "wol_sent",
                ff_agent::revive::ReviveOutcome::Failed(_) => "failed",
                ff_agent::revive::ReviveOutcome::Skipped(_) => "skipped",
            },
            "detail": match &outcome {
                ff_agent::revive::ReviveOutcome::Failed(r)
                | ff_agent::revive::ReviveOutcome::Skipped(r) => Some(r.as_str()),
                _ => None,
            },
        });
        println!("{}", j);
    } else {
        match outcome {
            ff_agent::revive::ReviveOutcome::DaemonRestarted => {
                println!("{GREEN}✓ daemon restart kicked via SSH{RESET}");
            }
            ff_agent::revive::ReviveOutcome::DaemonAlreadyRunning => {
                println!("{GREEN}✓ daemon already running on target{RESET}");
            }
            ff_agent::revive::ReviveOutcome::WolSent => {
                println!("{CYAN}↻ Wake-on-LAN packet(s) sent — awaiting pulse{RESET}");
            }
            ff_agent::revive::ReviveOutcome::Skipped(reason) => {
                println!("{YELLOW}— skipped: {reason}{RESET}");
            }
            ff_agent::revive::ReviveOutcome::Failed(reason) => {
                println!("{}✗ failed: {reason}{RESET}", "\x1b[31m");
            }
        }
    }
    Ok(())
}

/// Resolve the Redis URL for Pulse reads. Prefers `$FORGEFLEET_REDIS_URL`,
/// then `~/.forgefleet/fleet.toml` `[redis] url`, then a localhost fallback.
fn resolve_pulse_redis_url() -> String {
    if let Ok(url) = std::env::var("FORGEFLEET_REDIS_URL") {
        if !url.trim().is_empty() {
            return url;
        }
    }
    const FALLBACK: &str = "redis://localhost:6380";
    let Some(home) = dirs::home_dir() else {
        return FALLBACK.to_string();
    };
    let path = home.join(".forgefleet/fleet.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return FALLBACK.to_string();
    };
    let Ok(val) = toml::from_str::<toml::Value>(&text) else {
        return FALLBACK.to_string();
    };
    val.get("redis")
        .and_then(|r| r.get("url"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| FALLBACK.to_string())
}

fn pulse_reader() -> Result<ff_pulse::reader::PulseReader> {
    let url = resolve_pulse_redis_url();
    ff_pulse::reader::PulseReader::new(&url)
        .map_err(|e| anyhow::anyhow!("pulse: connect {url}: {e}"))
}

fn secs_ago(ts: chrono::DateTime<chrono::Utc>) -> i64 {
    (chrono::Utc::now() - ts).num_seconds().max(0)
}

async fn handle_fleet_leader(pool: &sqlx::PgPool, json: bool) -> Result<()> {
    let leader = ff_db::pg_get_current_leader(pool)
        .await
        .map_err(|e| anyhow::anyhow!("pg_get_current_leader: {e}"))?;

    // Candidate pool: fleet_members × computers, sorted by election_priority.
    let cand_rows = sqlx::query(
        "SELECT c.name        AS name,
                fm.election_priority AS election_priority
         FROM fleet_members fm
         JOIN computers c ON c.id = fm.computer_id
         ORDER BY fm.election_priority ASC, c.name ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("list candidates: {e}"))?;

    let candidates: Vec<(String, i32)> = cand_rows
        .iter()
        .map(|r| {
            (
                sqlx::Row::get::<String, _>(r, "name"),
                sqlx::Row::get::<i32, _>(r, "election_priority"),
            )
        })
        .collect();

    // Pulse info: alive + yielding from beats.
    let mut alive_map: std::collections::HashMap<String, (bool, bool)> =
        std::collections::HashMap::new();
    if let Ok(reader) = pulse_reader() {
        if let Ok(beats) = reader.all_beats().await {
            for b in beats {
                alive_map.insert(b.computer_name.clone(), (!b.going_offline, b.is_yielding));
            }
        }
    }

    if json {
        let cur = leader.as_ref().map(|l| {
            serde_json::json!({
                "member_name": l.member_name,
                "computer_id": l.computer_id,
                "epoch":       l.epoch,
                "elected_at":  l.elected_at,
                "reason":      l.reason,
                "heartbeat_at": l.heartbeat_at,
                "heartbeat_age_secs": secs_ago(l.heartbeat_at),
            })
        });
        let cand: Vec<_> = candidates
            .iter()
            .map(|(name, prio)| {
                let (alive, yielding) = alive_map.get(name).copied().unwrap_or((false, false));
                serde_json::json!({
                    "name": name,
                    "election_priority": prio,
                    "alive": alive,
                    "yielding": yielding,
                    "is_current": leader.as_ref().map(|l| &l.member_name == name).unwrap_or(false),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "current_leader": cur,
                "candidates":     cand,
            }))
            .unwrap_or_default()
        );
        return Ok(());
    }

    match &leader {
        Some(l) => {
            println!("{CYAN}▶ Current fleet leader:{RESET}");
            println!("  name:          {}", l.member_name);
            println!("  computer_id:   {}", l.computer_id);
            println!("  epoch:         {}", l.epoch);
            println!(
                "  elected_at:    {}",
                l.elected_at.format("%Y-%m-%d %H:%M:%S UTC")
            );
            println!("  heartbeat age: {} seconds", secs_ago(l.heartbeat_at));
            println!("  reason:        {}", l.reason.as_deref().unwrap_or("-"));
        }
        None => {
            println!("{YELLOW}(no current leader in fleet_leader_state){RESET}");
        }
    }

    if !candidates.is_empty() {
        println!("\n  Candidates (by election_priority):");
        for (name, prio) in &candidates {
            let (alive, yielding) = alive_map.get(name).copied().unwrap_or((false, false));
            let alive_str = if alive { "yes" } else { "no" };
            let yield_str = if yielding { "yes" } else { "no" };
            let marker = match &leader {
                Some(l) if &l.member_name == name => "  (← current)",
                _ => "",
            };
            println!(
                "    {:<12} priority={:<5} alive={:<4} yielding={:<4}{}",
                name, prio, alive_str, yield_str, marker
            );
        }
    } else {
        println!("\n  (no candidates in fleet_members)");
    }
    Ok(())
}

async fn handle_fleet_health(pool: &sqlx::PgPool, json: bool) -> Result<()> {
    // Pull computer rows — name, primary_ip, status, last_seen_at.
    let rows = sqlx::query(
        "SELECT name, primary_ip, status, last_seen_at
         FROM computers
         ORDER BY name ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("list computers: {e}"))?;

    #[derive(Debug)]
    struct HealthRow {
        name: String,
        ip: String,
        status: String,
        last_beat_secs: Option<i64>,
        cpu_pct: Option<f64>,
        ram_pct: Option<f64>,
        llm_servers: Option<usize>,
        software_count: Option<i64>,
        sdown: bool,
        odown: bool,
    }

    // Pulse lookups.
    let reader = pulse_reader().ok();
    let beats_by_name: std::collections::HashMap<String, ff_pulse::beat_v2::PulseBeatV2> =
        if let Some(r) = &reader {
            r.beats_by_name().await.unwrap_or_default()
        } else {
            std::collections::HashMap::new()
        };

    // Software counts per computer (best-effort).
    let sw_rows = sqlx::query(
        "SELECT c.name AS name, COUNT(cs.software_id) AS cnt
         FROM computers c
         LEFT JOIN computer_software cs ON cs.computer_id = c.id
         GROUP BY c.name",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let mut sw_map: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for r in &sw_rows {
        let name: String = sqlx::Row::get(r, "name");
        let cnt: i64 = sqlx::Row::get(r, "cnt");
        sw_map.insert(name, cnt);
    }

    let mut out: Vec<HealthRow> = Vec::with_capacity(rows.len());
    for r in &rows {
        let name: String = sqlx::Row::get(r, "name");
        let ip: String = sqlx::Row::get(r, "primary_ip");
        let status: String = sqlx::Row::get(r, "status");
        let last_seen: Option<chrono::DateTime<chrono::Utc>> = sqlx::Row::get(r, "last_seen_at");

        let beat = beats_by_name.get(&name);
        let last_beat_secs = beat
            .map(|b| secs_ago(b.timestamp))
            .or_else(|| last_seen.map(secs_ago));

        let sdown = if let Some(r) = &reader {
            r.is_sdown(&name).await.unwrap_or(true)
        } else {
            true
        };
        let odown = if let Some(r) = &reader {
            r.is_odown(&name).await.unwrap_or(false)
        } else {
            false
        };

        out.push(HealthRow {
            name: name.clone(),
            ip,
            status,
            last_beat_secs,
            cpu_pct: beat.map(|b| b.load.cpu_pct),
            ram_pct: beat.map(|b| b.load.ram_pct),
            llm_servers: beat.map(|b| b.llm_servers.len()),
            software_count: sw_map.get(&name).copied(),
            sdown,
            odown,
        });
    }

    if json {
        let arr: Vec<_> = out
            .iter()
            .map(|h| {
                serde_json::json!({
                    "name": h.name,
                    "ip": h.ip,
                    "status": h.status,
                    "last_beat_secs": h.last_beat_secs,
                    "cpu_pct": h.cpu_pct,
                    "ram_pct": h.ram_pct,
                    "llm_servers": h.llm_servers,
                    "software_count": h.software_count,
                    "sdown": h.sdown,
                    "odown": h.odown,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
        return Ok(());
    }

    if out.is_empty() {
        println!("(no computers registered)");
        return Ok(());
    }

    println!(
        "{:<11} {:<14} {:<9} {:<10} {:<5} {:<5} {:<12} {:<8}",
        "NAME", "IP", "STATUS", "LAST_BEAT", "CPU%", "RAM%", "LLM SERVERS", "SOFTWARE"
    );
    for h in &out {
        let status = if h.odown {
            "odown".to_string()
        } else if h.sdown {
            "sdown".to_string()
        } else {
            h.status.clone()
        };
        let beat = h
            .last_beat_secs
            .map(|s| format!("{s}s ago"))
            .unwrap_or_else(|| "-".into());
        let cpu = h
            .cpu_pct
            .map(|v| format!("{:.1}", v))
            .unwrap_or_else(|| "-".into());
        let ram = h
            .ram_pct
            .map(|v| format!("{:.1}", v))
            .unwrap_or_else(|| "-".into());
        let llms = h
            .llm_servers
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".into());
        let sw = h
            .software_count
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<11} {:<14} {:<9} {:<10} {:<5} {:<5} {:<12} {:<8}",
            h.name, h.ip, status, beat, cpu, ram, llms, sw
        );
    }
    Ok(())
}

/// Show per-host code identity (SHA-first), with a convergence summary.
/// Designed so a glance at the table answers "are all hosts on the same
/// code?" — the per-machine build counter is only shown with --verbose.
///
/// `live=true` SSHes each host in parallel and reads `forgefleetd
/// --version` directly, so the view is accurate right after an upgrade.
/// `live=false` reads the DB-cached `computer_software.installed_version`
/// (refreshed every 6h) — fast but stale.
async fn handle_fleet_versions(pool: &sqlx::PgPool, verbose: bool, live: bool) -> Result<()> {
    use ff_core::build_version::{BuildVersion, display_version_short};

    if live {
        return handle_fleet_versions_live(pool, verbose).await;
    }

    // Pull the installed_version cell stored on each (computer, software_id)
    // pair. ff_git's installed_version is the full 40-char git SHA written
    // by version_check::collect_current; ff_terminal's regex-extracted
    // build_version is what predates the V56 cleanup but rare nodes may
    // still have it cached. Either path falls through code_identity().
    let rows = sqlx::query(
        "SELECT c.name AS name,
                cs.installed_version AS installed,
                sr.latest_version AS latest
           FROM computers c
           JOIN computer_software cs ON cs.computer_id = c.id
           JOIN software_registry sr ON sr.id = cs.software_id
          WHERE cs.software_id = 'ff_git'
          ORDER BY c.name",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("query versions: {e}"))?;

    if rows.is_empty() {
        println!(
            "(no ff_git rows in computer_software — fleet may not have run a version_check tick yet)"
        );
        return Ok(());
    }

    // Pick the most-common installed SHA as the "fleet target". A host
    // matches when its installed SHA equals that — regardless of build
    // counter, build date, or local-tree state.
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut hosts: Vec<(String, String, String)> = Vec::with_capacity(rows.len());
    for r in &rows {
        let name: String = sqlx::Row::try_get(r, "name").unwrap_or_default();
        let installed: Option<String> = sqlx::Row::try_get(r, "installed").ok();
        let latest: Option<String> = sqlx::Row::try_get(r, "latest").ok();
        let installed = installed.unwrap_or_default();
        let latest = latest.unwrap_or_default();
        if !installed.is_empty() {
            *counts.entry(installed.clone()).or_insert(0) += 1;
        }
        hosts.push((name, installed, latest));
    }
    let target_sha: Option<String> = counts
        .iter()
        .max_by_key(|(_, n)| *n)
        .map(|(sha, _)| sha.clone());

    // Route every cell through display_version_short — the unified
    // helper handles ff-shape strings, raw 40-char SHAs, and vendor
    // version strings consistently. Empty cells render as `-`.
    let short = |raw: &str| -> String {
        if raw.is_empty() {
            "-".to_string()
        } else {
            display_version_short(raw)
        }
    };

    if verbose {
        println!(
            "{:<12} {:<10} {:<10} {:<10} {:<8}",
            "NAME", "INSTALLED", "LATEST", "STATE", "BUILD#"
        );
    } else {
        println!(
            "{:<12} {:<10} {:<10} {:<8}",
            "NAME", "INSTALLED", "LATEST", "STATE"
        );
    }
    let mut converged = 0usize;
    for (name, installed, latest) in &hosts {
        let inst_short = short(installed);
        let lat_short = short(latest);
        let state = match target_sha.as_deref() {
            Some(t) if installed == t => {
                converged += 1;
                "✓"
            }
            Some(_) => "drift",
            None => "?",
        };
        if verbose {
            // Try to parse a build counter / date from any embedded
            // BuildVersion-shaped string. Pre-V56 cells may have one;
            // SHA-only cells legitimately don't.
            let parsed = BuildVersion::parse(installed);
            let count = parsed
                .as_ref()
                .map(|v| v.build_count.to_string())
                .unwrap_or_else(|| "-".into());
            println!(
                "{:<12} {:<10} {:<10} {:<10} {:<8}",
                name, inst_short, lat_short, state, count
            );
        } else {
            println!(
                "{:<12} {:<10} {:<10} {:<8}",
                name, inst_short, lat_short, state
            );
        }
    }

    let total = hosts.len();
    let target_disp = target_sha
        .as_deref()
        .map(|s| s.chars().take(8).collect::<String>())
        .unwrap_or_else(|| "-".into());
    println!();
    if converged == total {
        println!("{GREEN}✓ converged{RESET}: all {total} host(s) at {target_disp}");
    } else {
        println!(
            "{YELLOW}⚠ drift{RESET}: {}/{total} on {target_disp}; {} drifted",
            converged,
            total - converged,
        );
    }

    Ok(())
}

/// Live variant of `ff fleet versions` — SSHes every computer in
/// parallel and reads `forgefleetd --version` directly. Slower than the
/// cached path (one SSH round-trip per host, capped at ~5s each) but
/// truthful right after a fleet upgrade when the version_check tick
/// hasn't refreshed `installed_version` yet.
async fn handle_fleet_versions_live(pool: &sqlx::PgPool, verbose: bool) -> Result<()> {
    use ff_core::build_version::BuildVersion;
    use futures::stream::{FuturesUnordered, StreamExt};
    use tokio::process::Command;

    let nodes = ff_db::pg_list_nodes(pool)
        .await
        .map_err(|e| anyhow::anyhow!("pg_list_nodes: {e}"))?;
    if nodes.is_empty() {
        println!("(no computers registered)");
        return Ok(());
    }

    let me = ff_agent::fleet_info::resolve_this_node_name().await;
    let mut futs = FuturesUnordered::new();
    for n in nodes {
        let name = n.name.clone();
        let ip = n.ip.clone();
        let user = n.ssh_user.clone();
        let is_me = me.eq_ignore_ascii_case(&name);
        futs.push(async move {
            let cmd = "~/.local/bin/forgefleetd --version 2>&1 | head -1";
            let out = if is_me {
                Command::new("sh").args(["-c", cmd]).output().await
            } else {
                Command::new("ssh")
                    .args([
                        "-T",
                        "-o",
                        "BatchMode=yes",
                        "-o",
                        "ConnectTimeout=5",
                        &format!("{user}@{ip}"),
                        cmd,
                    ])
                    .output()
                    .await
            };
            let raw = match out {
                Ok(o) if o.status.success() => {
                    String::from_utf8_lossy(&o.stdout).trim().to_string()
                }
                Ok(o) => format!("ssh-exit:{}", o.status.code().unwrap_or(-1)),
                Err(e) => format!("ssh-error:{e}"),
            };
            (name, raw)
        });
    }

    let mut rows: Vec<(String, String, Option<BuildVersion>)> = Vec::new();
    while let Some((name, raw)) = futs.next().await {
        let parsed = BuildVersion::parse(&raw);
        rows.push((name, raw, parsed));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    // Pick the most-common SHA as the fleet target.
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (_, _, parsed) in &rows {
        if let Some(p) = parsed {
            *counts.entry(p.sha.clone()).or_insert(0) += 1;
        }
    }
    let target_sha: Option<String> = counts
        .iter()
        .max_by_key(|(_, n)| *n)
        .map(|(sha, _)| sha.clone());

    if verbose {
        println!(
            "{:<12} {:<10} {:<8} {:<8} {:<8}",
            "NAME", "SHA", "STATE", "BUILD#", "STATUS"
        );
    } else {
        println!("{:<12} {:<10} {:<8}", "NAME", "SHA", "STATUS");
    }
    let mut converged = 0usize;
    let mut unreachable = 0usize;
    for (name, raw, parsed) in &rows {
        match parsed {
            Some(v) => {
                let status = match target_sha.as_deref() {
                    Some(t) if v.sha == t => {
                        converged += 1;
                        "✓".to_string()
                    }
                    Some(_) => "drift".to_string(),
                    None => "?".to_string(),
                };
                if verbose {
                    println!(
                        "{:<12} {:<10} {:<8} {:<8} {:<8}",
                        name,
                        v.short_sha(),
                        v.state,
                        v.build_count,
                        status
                    );
                } else {
                    println!("{:<12} {:<10} {:<8}", name, v.short_sha(), status);
                }
            }
            None => {
                unreachable += 1;
                let snippet: String = raw.chars().take(20).collect();
                if verbose {
                    println!("{:<12} {:<10} {:<8} {:<8} {snippet}", name, "?", "?", "?");
                } else {
                    println!("{:<12} {:<10} {snippet}", name, "?");
                }
            }
        }
    }

    let total = rows.len();
    let target_disp = target_sha
        .as_deref()
        .map(|s| {
            let n = s.chars().count().min(8);
            s[..n].to_string()
        })
        .unwrap_or_else(|| "-".into());
    println!();
    if unreachable == 0 && converged == total {
        println!("{GREEN}✓ converged{RESET}: all {total} host(s) live at {target_disp}");
    } else {
        println!(
            "{YELLOW}⚠ {}/{total} live at {target_disp}{RESET}; {} drifted, {} unreachable",
            converged,
            total - converged - unreachable,
            unreachable,
        );
    }

    Ok(())
}

async fn handle_fleet_gossip() -> Result<()> {
    let reader = pulse_reader()?;
    let beats = reader
        .all_beats()
        .await
        .map_err(|e| anyhow::anyhow!("all_beats: {e}"))?;

    if beats.is_empty() {
        println!("(no beats present in Redis — is the daemon publishing pulses?)");
        return Ok(());
    }

    println!("{CYAN}▶ Fleet gossip dump — peers_seen per member:{RESET}");
    for b in &beats {
        let age = secs_ago(b.timestamp);
        println!(
            "\n  {} (epoch={}, role={}, {}s old, going_offline={}, yielding={})",
            b.computer_name, b.epoch, b.role_claimed, age, b.going_offline, b.is_yielding,
        );
        if b.peers_seen.is_empty() {
            println!("    (peers_seen empty)");
            continue;
        }
        for p in &b.peers_seen {
            let pa = secs_ago(p.last_beat_at);
            println!(
                "    ├─ {:<12} status={:<6} epoch_witnessed={:<4} last_beat={}s ago",
                p.name, p.status, p.epoch_witnessed, pa,
            );
        }
    }
    Ok(())
}

async fn handle_llm(cmd: LlmCommand) -> Result<()> {
    match cmd {
        LlmCommand::Status { json } => handle_llm_status(json).await,
    }
}

async fn handle_llm_status(json: bool) -> Result<()> {
    let reader = pulse_reader()?;
    let servers = reader
        .list_llm_servers()
        .await
        .map_err(|e| anyhow::anyhow!("list_llm_servers: {e}"))?;

    // Also grab all computer names so we can show "(no server)" rows.
    let all_computers = reader.list_computers().await.unwrap_or_default();

    if json {
        let arr: Vec<_> = servers
            .iter()
            .map(|(computer, s)| {
                serde_json::json!({
                    "computer":  computer,
                    "model":     s.model.id,
                    "runtime":   s.runtime,
                    "endpoint":  s.endpoint,
                    "queue_depth": s.queue_depth,
                    "active_requests": s.active_requests,
                    "tokens_per_sec_last_min": s.tokens_per_sec_last_min,
                    "is_healthy": s.is_healthy,
                    "status":    s.status,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
        return Ok(());
    }

    if servers.is_empty() {
        println!("(no running LLM servers)");
        if !all_computers.is_empty() {
            println!("computers present in pulse: {}", all_computers.join(", "));
        }
        return Ok(());
    }

    println!(
        "{:<10} {:<20} {:<10} {:<32} {:<5} {:<6} {:<7} {:<8}",
        "COMPUTER", "MODEL", "RUNTIME", "ENDPOINT", "QUEUE", "ACTIVE", "TOK/S", "HEALTH"
    );
    for (computer, s) in &servers {
        let health = if s.is_healthy { "healthy" } else { "unhealthy" };
        println!(
            "{:<10} {:<20} {:<10} {:<32} {:<5} {:<6} {:<7.1} {:<8}",
            truncate_for_col(computer, 10),
            truncate_for_col(&s.model.id, 20),
            truncate_for_col(&s.runtime, 10),
            truncate_for_col(&s.endpoint, 32),
            s.queue_depth,
            s.active_requests,
            s.tokens_per_sec_last_min,
            health
        );
    }

    // Also show computers that have NO running server.
    let hosts_with_server: std::collections::HashSet<&str> =
        servers.iter().map(|(c, _)| c.as_str()).collect();
    let mut missing: Vec<&String> = all_computers
        .iter()
        .filter(|c| !hosts_with_server.contains(c.as_str()))
        .collect();
    missing.sort();
    for c in &missing {
        println!("{:<10} (no server)", truncate_for_col(c, 10));
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
        } => handle_software_list(&pool, computer, software, json).await,
        SoftwareCommand::Drift { json } => handle_software_drift(&pool, json).await,
        SoftwareCommand::Add {
            id,
            kind,
            version_source,
            upgrade_playbook,
            display_name,
        } => {
            handle_software_add(
                &pool,
                &id,
                &kind,
                &version_source,
                &upgrade_playbook,
                display_name,
            )
            .await
        }
        SoftwareCommand::Remove { id, yes } => handle_software_remove(&pool, &id, yes).await,
        SoftwareCommand::AutoUpgradeRunOnce { force } => {
            handle_auto_upgrade_run_once(&pool, force).await
        }
        SoftwareCommand::Unblock {
            computer,
            software_id,
        } => handle_software_unblock(&pool, &computer, &software_id).await,
    }
}

/// Implementation of `ff software unblock <computer> <software_id>`.
///
/// Resets the failure counter and flips the row from `upgrade_blocked`
/// (or any other status that's not `upgrading`) back to either `ok` or
/// `upgrade_available` — `flip_drift_status` recalculates on the next
/// auto-upgrade tick so the row gets the right post-clear state.
async fn handle_software_unblock(
    pool: &sqlx::PgPool,
    computer: &str,
    software_id: &str,
) -> Result<()> {
    let updated = sqlx::query(
        "UPDATE computer_software cs
            SET status               = 'ok',
                consecutive_failures = 0,
                last_upgrade_error   = NULL
           FROM computers c
          WHERE cs.computer_id = c.id
            AND cs.software_id = $1
            AND LOWER(c.name)  = LOWER($2)
            AND cs.status      <> 'upgrading'",
    )
    .bind(software_id)
    .bind(computer)
    .execute(pool)
    .await?
    .rows_affected();

    if updated == 0 {
        println!(
            "{YELLOW}no row matched (computer={computer}, software_id={software_id}) \
             — or the row is currently 'upgrading' (refusing to clobber an in-flight task).{RESET}"
        );
    } else {
        println!(
            "{GREEN}✓ cleared {updated} row(s) — status='ok', consecutive_failures=0.{RESET}"
        );
        println!(
            "  Next auto-upgrade tick (`ff software auto-upgrade-run-once`) will \
             re-evaluate drift and dispatch if needed."
        );
    }
    Ok(())
}

/// Implementation of `ff software auto-upgrade-run-once`.
///
/// Bypasses the hourly scheduler by directly calling `AutoUpgradeTick::run_once()`
/// on the local process. The resulting deferred tasks land in the defer queue
/// same as the hourly tick — workers on each target computer pull + execute
/// them on their next poll.
async fn handle_auto_upgrade_run_once(pool: &sqlx::PgPool, force: bool) -> Result<()> {
    // Mirror the gate check the hourly tick uses so --force is meaningful.
    let enabled = ff_db::pg_get_secret(pool, "auto_upgrade_enabled")
        .await
        .ok()
        .flatten()
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "on"))
        .unwrap_or(false);
    if !enabled && !force {
        println!("{YELLOW}auto_upgrade_enabled is not set — pass --force to run anyway.{RESET}");
        println!("  (To enable persistently: ff secrets set auto_upgrade_enabled true)");
        return Ok(());
    }

    let worker = ff_agent::fleet_info::resolve_this_node_name().await;
    println!(
        "{CYAN}[auto-upgrade run-once]{RESET} triggering tick as worker={worker}{}",
        if force && !enabled {
            " (--force: gate bypassed)"
        } else {
            ""
        }
    );
    let tick = ff_agent::auto_upgrade::AutoUpgradeTick::new(pool.clone(), worker);
    let enqueued = tick
        .run_once(force)
        .await
        .map_err(|e| anyhow::anyhow!("auto_upgrade run_once: {e}"))?;
    println!("{GREEN}✓ dispatched {enqueued} upgrade task(s){RESET}");
    Ok(())
}

async fn handle_software_list(
    pool: &sqlx::PgPool,
    computer: Option<String>,
    software: Option<String>,
    json: bool,
) -> Result<()> {
    let mut sql = String::from(
        "SELECT c.name            AS computer,
                sr.id              AS software_id,
                sr.display_name    AS display_name,
                sr.kind            AS kind,
                cs.installed_version AS installed_version,
                sr.latest_version  AS latest_version,
                cs.install_source  AS install_source,
                cs.status          AS status,
                cs.last_checked_at AS last_checked_at
         FROM computer_software cs
         JOIN computers c          ON cs.computer_id = c.id
         JOIN software_registry sr ON cs.software_id = sr.id
         WHERE 1=1",
    );
    if computer.is_some() {
        sql.push_str(" AND c.name = $1");
    }
    if software.is_some() {
        sql.push_str(if computer.is_some() {
            " AND sr.id = $2"
        } else {
            " AND sr.id = $1"
        });
    }
    sql.push_str(" ORDER BY c.name ASC, sr.id ASC");

    let mut query = sqlx::query(&sql);
    if let Some(c) = &computer {
        query = query.bind(c);
    }
    if let Some(s) = &software {
        query = query.bind(s);
    }

    let rows = query
        .fetch_all(pool)
        .await
        .map_err(|e| anyhow::anyhow!("list software: {e}"))?;

    if json {
        let arr: Vec<_> = rows.iter().map(|r| serde_json::json!({
            "computer":          sqlx::Row::get::<String, _>(r, "computer"),
            "software_id":       sqlx::Row::get::<String, _>(r, "software_id"),
            "display_name":      sqlx::Row::get::<String, _>(r, "display_name"),
            "kind":              sqlx::Row::get::<String, _>(r, "kind"),
            "installed_version": sqlx::Row::get::<Option<String>, _>(r, "installed_version"),
            "latest_version":    sqlx::Row::get::<Option<String>, _>(r, "latest_version"),
            "install_source":    sqlx::Row::get::<Option<String>, _>(r, "install_source"),
            "status":            sqlx::Row::get::<String, _>(r, "status"),
        })).collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
        return Ok(());
    }

    if rows.is_empty() {
        println!("(no matching computer_software rows)");
        return Ok(());
    }

    println!(
        "{:<11} {:<16} {:<10} {:<16} {:<16} {:<10} {:<18}",
        "COMPUTER", "SOFTWARE", "KIND", "INSTALLED", "LATEST", "SOURCE", "STATUS"
    );
    for r in &rows {
        let computer: String = sqlx::Row::get(r, "computer");
        let sid: String = sqlx::Row::get(r, "software_id");
        let kind: String = sqlx::Row::get(r, "kind");
        let installed: Option<String> = sqlx::Row::get(r, "installed_version");
        let latest: Option<String> = sqlx::Row::get(r, "latest_version");
        let src: Option<String> = sqlx::Row::get(r, "install_source");
        let status: String = sqlx::Row::get(r, "status");
        println!(
            "{:<11} {:<16} {:<10} {:<16} {:<16} {:<10} {:<18}",
            truncate_for_col(&computer, 11),
            truncate_for_col(&sid, 16),
            truncate_for_col(&kind, 10),
            truncate_for_col(installed.as_deref().unwrap_or("-"), 16),
            truncate_for_col(latest.as_deref().unwrap_or("-"), 16),
            truncate_for_col(src.as_deref().unwrap_or("-"), 10),
            truncate_for_col(&status, 18),
        );
    }
    Ok(())
}

async fn handle_software_drift(pool: &sqlx::PgPool, json: bool) -> Result<()> {
    // Two drift signals: explicit status='upgrade_available' OR
    // installed_version differs from latest_version (both non-null).
    let rows = sqlx::query(
        "SELECT c.name              AS computer,
                sr.id                AS software_id,
                sr.display_name      AS display_name,
                cs.installed_version AS installed_version,
                sr.latest_version    AS latest_version,
                cs.install_source    AS install_source,
                cs.status            AS status
         FROM computer_software cs
         JOIN computers c          ON cs.computer_id = c.id
         JOIN software_registry sr ON cs.software_id = sr.id
         WHERE cs.status = 'upgrade_available'
            OR (cs.installed_version IS NOT NULL
                AND sr.latest_version IS NOT NULL
                AND cs.installed_version <> sr.latest_version)
         ORDER BY c.name ASC, sr.id ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("list drift: {e}"))?;

    if json {
        let arr: Vec<_> = rows.iter().map(|r| serde_json::json!({
            "computer":          sqlx::Row::get::<String, _>(r, "computer"),
            "software_id":       sqlx::Row::get::<String, _>(r, "software_id"),
            "display_name":      sqlx::Row::get::<String, _>(r, "display_name"),
            "installed_version": sqlx::Row::get::<Option<String>, _>(r, "installed_version"),
            "latest_version":    sqlx::Row::get::<Option<String>, _>(r, "latest_version"),
            "install_source":    sqlx::Row::get::<Option<String>, _>(r, "install_source"),
            "status":            sqlx::Row::get::<String, _>(r, "status"),
        })).collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
        return Ok(());
    }

    if rows.is_empty() {
        println!(
            "{GREEN}✓ No drift detected — every computer_software row matches its software_registry.latest_version.{RESET}"
        );
        return Ok(());
    }

    println!(
        "{:<11} {:<16} {:<18} {:<18} {:<10} {:<18}",
        "COMPUTER", "SOFTWARE", "INSTALLED", "LATEST", "SOURCE", "STATUS"
    );
    for r in &rows {
        let computer: String = sqlx::Row::get(r, "computer");
        let sid: String = sqlx::Row::get(r, "software_id");
        let installed: Option<String> = sqlx::Row::get(r, "installed_version");
        let latest: Option<String> = sqlx::Row::get(r, "latest_version");
        let src: Option<String> = sqlx::Row::get(r, "install_source");
        let status: String = sqlx::Row::get(r, "status");
        println!(
            "{:<11} {:<16} {:<18} {:<18} {:<10} {:<18}",
            truncate_for_col(&computer, 11),
            truncate_for_col(&sid, 16),
            truncate_for_col(installed.as_deref().unwrap_or("-"), 18),
            truncate_for_col(latest.as_deref().unwrap_or("-"), 18),
            truncate_for_col(src.as_deref().unwrap_or("-"), 10),
            truncate_for_col(&status, 18),
        );
    }
    println!("\n{} row(s) with drift.", rows.len());
    Ok(())
}

/// `ff software add` — upsert a `software_registry` row directly, bypassing
/// `config/software.toml`. The TOML seeder is still the boot-time source of
/// truth; this handler is for ad-hoc additions (new upstream tools, fleet
/// LLMs proposing catalog entries, etc).
async fn handle_software_add(
    pool: &sqlx::PgPool,
    id: &str,
    kind: &str,
    version_source_json: &str,
    upgrade_playbook_json: &str,
    display_name: Option<String>,
) -> Result<()> {
    let version_source: serde_json::Value = serde_json::from_str(version_source_json)
        .map_err(|e| anyhow::anyhow!("--version-source is not valid JSON: {e}"))?;
    let upgrade_playbook: serde_json::Value = serde_json::from_str(upgrade_playbook_json)
        .map_err(|e| anyhow::anyhow!("--upgrade-playbook is not valid JSON: {e}"))?;

    let display = display_name.unwrap_or_else(|| id.to_string());
    let who = whoami_tag();

    let result = sqlx::query(
        "INSERT INTO software_registry (id, display_name, kind, version_source, upgrade_playbook)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (id) DO UPDATE SET
             display_name     = EXCLUDED.display_name,
             kind             = EXCLUDED.kind,
             version_source   = EXCLUDED.version_source,
             upgrade_playbook = EXCLUDED.upgrade_playbook",
    )
    .bind(id)
    .bind(&display)
    .bind(kind)
    .bind(&version_source)
    .bind(&upgrade_playbook)
    .execute(pool)
    .await
    .map_err(|e| anyhow::anyhow!("upsert software_registry: {e}"))?;

    println!(
        "{GREEN}✓ software_registry upsert ok{RESET}  id={id}  display_name={display}  kind={kind}  rows_affected={}  by={who}",
        result.rows_affected()
    );
    Ok(())
}

/// `ff software remove` — delete a `software_registry` row. First cleans up
/// `computer_software` rows referencing it so the FK doesn't block, then the
/// registry row itself. Prints before-counts and requires `--yes` to commit.
async fn handle_software_remove(pool: &sqlx::PgPool, id: &str, confirm_yes: bool) -> Result<()> {
    let registry_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM software_registry WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .map_err(|e| anyhow::anyhow!("count software_registry: {e}"))?;

    if registry_count == 0 {
        println!("{YELLOW}No software_registry row with id='{id}' — nothing to remove.{RESET}");
        return Ok(());
    }

    let install_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM computer_software WHERE software_id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .map_err(|e| anyhow::anyhow!("count computer_software: {e}"))?;

    println!("About to remove software id='{id}':");
    println!("  software_registry rows:  {registry_count}");
    println!("  computer_software rows:  {install_count}");

    if !confirm_yes {
        eprintln!("{YELLOW}⚠ destructive. Re-run with --yes to confirm.{RESET}");
        return Ok(());
    }

    let cs = sqlx::query("DELETE FROM computer_software WHERE software_id = $1")
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("delete computer_software: {e}"))?;

    let sr = sqlx::query("DELETE FROM software_registry WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("delete software_registry: {e}"))?;

    println!(
        "{GREEN}✓ removed software id='{id}'{RESET}  computer_software_deleted={}  software_registry_deleted={}  by={}",
        cs.rows_affected(),
        sr.rows_affected(),
        whoami_tag()
    );
    Ok(())
}

// ─── ff social ─────────────────────────────────────────────────────────────

async fn handle_social(cmd: SocialCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        SocialCommand::Ingest { url, by } => {
            let id = ff_agent::social_ingest::ingest(pool.clone(), url, by).await?;
            println!("{GREEN}✓ ingest queued{RESET}  post_id = {id}");
            println!("  \x1b[2mUse `ff social show {id}` to check status.{RESET}");
            Ok(())
        }
        SocialCommand::List {
            status,
            platform,
            limit,
        } => {
            let mut sql = String::from(
                "SELECT id, url, platform, status, ingested_by, ingested_at \
                 FROM social_media_posts WHERE 1=1",
            );
            let mut idx = 1;
            if status.is_some() {
                sql.push_str(&format!(" AND status = ${idx}"));
                idx += 1;
            }
            if platform.is_some() {
                sql.push_str(&format!(" AND platform = ${idx}"));
            }
            sql.push_str(" ORDER BY ingested_at DESC LIMIT ");
            sql.push_str(&limit.to_string());

            let mut q = sqlx::query_as::<
                _,
                (
                    uuid::Uuid,
                    String,
                    String,
                    String,
                    Option<String>,
                    chrono::DateTime<chrono::Utc>,
                ),
            >(&sql);
            if let Some(s) = &status {
                q = q.bind(s);
            }
            if let Some(p) = &platform {
                q = q.bind(p);
            }
            let rows = q.fetch_all(&pool).await?;

            println!(
                "{:<38} {:<10} {:<10} {:<16} {}",
                "id", "platform", "status", "by", "ingested_at"
            );
            for (id, url, platform, status, by, at) in &rows {
                let url_short: String = url.chars().take(60).collect();
                println!(
                    "{id}  {:<10} {:<10} {:<16} {}",
                    platform,
                    status,
                    by.clone().unwrap_or_default(),
                    at.format("%Y-%m-%d %H:%M")
                );
                println!("  \x1b[2m{url_short}{RESET}");
            }
            println!("\n{} row(s).", rows.len());
            Ok(())
        }
        SocialCommand::Show { id } => {
            let post_id = uuid::Uuid::parse_str(&id)
                .map_err(|e| anyhow::anyhow!("invalid UUID '{id}': {e}"))?;
            let row: Option<(
                uuid::Uuid,
                String,
                String,
                Option<String>,
                Option<String>,
                serde_json::Value,
                Option<String>,
                Option<serde_json::Value>,
                String,
                Option<String>,
                chrono::DateTime<chrono::Utc>,
                Option<chrono::DateTime<chrono::Utc>>,
                Option<String>,
            )> = sqlx::query_as(
                "SELECT id, url, platform, author, caption, media_items, \
                        extracted_text, analysis, status, ingested_by, \
                        ingested_at, analyzed_at, last_error \
                 FROM social_media_posts WHERE id = $1",
            )
            .bind(post_id)
            .fetch_optional(&pool)
            .await?;
            let Some((
                id,
                url,
                platform,
                author,
                caption,
                media_items,
                extracted_text,
                analysis,
                status,
                ingested_by,
                ingested_at,
                analyzed_at,
                last_error,
            )) = row
            else {
                println!("{RED}✗ no social_media_posts row with id = {id}{RESET}");
                return Ok(());
            };

            println!("{CYAN}post{RESET}    {id}");
            println!("url      {url}");
            println!("platform {platform}");
            println!("status   {status}");
            println!("by       {}", ingested_by.unwrap_or_default());
            println!("ingested {}", ingested_at.format("%Y-%m-%d %H:%M:%S"));
            if let Some(a) = analyzed_at {
                println!("analyzed {}", a.format("%Y-%m-%d %H:%M:%S"));
            }
            if let Some(a) = author {
                println!("author   {a}");
            }
            if let Some(c) = caption {
                let trunc = if c.chars().count() > 400 {
                    format!("{}…", c.chars().take(400).collect::<String>())
                } else {
                    c
                };
                println!("caption  {trunc}");
            }
            let media_arr = media_items.as_array().cloned().unwrap_or_default();
            println!("media    {} item(s)", media_arr.len());
            for m in &media_arr {
                let kind = m.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
                let path = m.get("local_path").and_then(|v| v.as_str()).unwrap_or("?");
                println!("  • [{kind}] {path}");
            }
            if let Some(t) = extracted_text {
                if !t.trim().is_empty() {
                    println!("\n{CYAN}extracted_text{RESET}\n{t}");
                }
            }
            if let Some(a) = analysis {
                let pretty = serde_json::to_string_pretty(&a).unwrap_or_default();
                println!("\n{CYAN}analysis{RESET}\n{pretty}");
            }
            if let Some(e) = last_error {
                println!("\n{RED}last_error{RESET} {e}");
            }
            Ok(())
        }
    }
}

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
        ExtCommand::List { json } => handle_ext_list(&pool, json).await,
        ExtCommand::Installed {
            computer,
            tool,
            json,
        } => handle_ext_installed(&pool, computer, tool, json).await,
        ExtCommand::Install {
            tool_id,
            computer,
            all,
            dry_run,
            yes,
        } => handle_ext_install(&pool, &tool_id, computer, all, dry_run, yes).await,
        ExtCommand::Drift { json } => handle_ext_drift(&pool, json).await,
    }
}

async fn handle_ext_list(pool: &sqlx::PgPool, json: bool) -> Result<()> {
    let tools = ff_agent::external_tools_registry::list_tools(pool)
        .await
        .map_err(|e| anyhow::anyhow!("list external_tools: {e}"))?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&tools).unwrap_or_default()
        );
        return Ok(());
    }

    if tools.is_empty() {
        println!(
            "{YELLOW}(external_tools is empty — run `ff ext seed` to load config/external_tools.toml){RESET}"
        );
        return Ok(());
    }

    println!(
        "{:<22} {:<8} {:<14} {:<14} {:<14} MCP",
        "ID", "KIND", "METHOD", "CLI", "LATEST"
    );
    for t in &tools {
        println!(
            "{:<22} {:<8} {:<14} {:<14} {:<14} {}",
            truncate_for_col(&t.id, 22),
            truncate_for_col(&t.kind, 8),
            truncate_for_col(&t.install_method, 14),
            truncate_for_col(t.cli_entrypoint.as_deref().unwrap_or("-"), 14),
            truncate_for_col(t.latest_version.as_deref().unwrap_or("-"), 14),
            if t.register_as_mcp {
                "auto-register"
            } else {
                "-"
            },
        );
    }
    println!("\n{} tool(s) in catalog.", tools.len());
    Ok(())
}

async fn handle_ext_installed(
    pool: &sqlx::PgPool,
    computer: Option<String>,
    tool: Option<String>,
    json: bool,
) -> Result<()> {
    let mut sql = String::from(
        "SELECT c.name              AS computer,
                et.id                AS tool_id,
                et.display_name      AS display_name,
                et.kind              AS kind,
                cet.installed_version AS installed_version,
                et.latest_version    AS latest_version,
                cet.install_source   AS install_source,
                cet.install_path     AS install_path,
                cet.mcp_registered   AS mcp_registered,
                cet.status           AS status,
                cet.last_checked_at  AS last_checked_at
           FROM computer_external_tools cet
           JOIN computers c      ON cet.computer_id = c.id
           JOIN external_tools et ON cet.tool_id = et.id
          WHERE 1=1",
    );
    if computer.is_some() {
        sql.push_str(" AND c.name = $1");
    }
    if tool.is_some() {
        sql.push_str(if computer.is_some() {
            " AND et.id = $2"
        } else {
            " AND et.id = $1"
        });
    }
    sql.push_str(" ORDER BY c.name ASC, et.id ASC");

    let mut query = sqlx::query(&sql);
    if let Some(c) = &computer {
        query = query.bind(c);
    }
    if let Some(t) = &tool {
        query = query.bind(t);
    }

    let rows = query
        .fetch_all(pool)
        .await
        .map_err(|e| anyhow::anyhow!("list computer_external_tools: {e}"))?;

    if json {
        let arr: Vec<_> = rows.iter().map(|r| serde_json::json!({
            "computer":          sqlx::Row::get::<String, _>(r, "computer"),
            "tool_id":           sqlx::Row::get::<String, _>(r, "tool_id"),
            "display_name":      sqlx::Row::get::<String, _>(r, "display_name"),
            "kind":              sqlx::Row::get::<String, _>(r, "kind"),
            "installed_version": sqlx::Row::get::<Option<String>, _>(r, "installed_version"),
            "latest_version":    sqlx::Row::get::<Option<String>, _>(r, "latest_version"),
            "install_source":    sqlx::Row::get::<Option<String>, _>(r, "install_source"),
            "install_path":      sqlx::Row::get::<Option<String>, _>(r, "install_path"),
            "mcp_registered":    sqlx::Row::get::<bool, _>(r, "mcp_registered"),
            "status":            sqlx::Row::get::<String, _>(r, "status"),
        })).collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
        return Ok(());
    }

    if rows.is_empty() {
        println!("(no matching computer_external_tools rows)");
        return Ok(());
    }

    println!(
        "{:<11} {:<22} {:<10} {:<14} {:<14} {:<10} {:<18}",
        "COMPUTER", "TOOL", "KIND", "INSTALLED", "LATEST", "SOURCE", "STATUS"
    );
    for r in &rows {
        let computer: String = sqlx::Row::get(r, "computer");
        let tid: String = sqlx::Row::get(r, "tool_id");
        let kind: String = sqlx::Row::get(r, "kind");
        let installed: Option<String> = sqlx::Row::get(r, "installed_version");
        let latest: Option<String> = sqlx::Row::get(r, "latest_version");
        let src: Option<String> = sqlx::Row::get(r, "install_source");
        let status: String = sqlx::Row::get(r, "status");
        println!(
            "{:<11} {:<22} {:<10} {:<14} {:<14} {:<10} {:<18}",
            truncate_for_col(&computer, 11),
            truncate_for_col(&tid, 22),
            truncate_for_col(&kind, 10),
            truncate_for_col(installed.as_deref().unwrap_or("-"), 14),
            truncate_for_col(latest.as_deref().unwrap_or("-"), 14),
            truncate_for_col(src.as_deref().unwrap_or("-"), 10),
            truncate_for_col(&status, 18),
        );
    }
    Ok(())
}

async fn handle_ext_drift(pool: &sqlx::PgPool, json: bool) -> Result<()> {
    let rows = sqlx::query(
        "SELECT c.name              AS computer,
                et.id                AS tool_id,
                et.display_name      AS display_name,
                cet.installed_version AS installed_version,
                et.latest_version    AS latest_version,
                cet.install_source   AS install_source,
                cet.status           AS status
           FROM computer_external_tools cet
           JOIN computers c      ON cet.computer_id = c.id
           JOIN external_tools et ON cet.tool_id = et.id
          WHERE cet.status = 'upgrade_available'
             OR (cet.installed_version IS NOT NULL
                 AND et.latest_version IS NOT NULL
                 AND cet.installed_version <> et.latest_version)
          ORDER BY c.name ASC, et.id ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("list ext drift: {e}"))?;

    if json {
        let arr: Vec<_> = rows.iter().map(|r| serde_json::json!({
            "computer":          sqlx::Row::get::<String, _>(r, "computer"),
            "tool_id":           sqlx::Row::get::<String, _>(r, "tool_id"),
            "display_name":      sqlx::Row::get::<String, _>(r, "display_name"),
            "installed_version": sqlx::Row::get::<Option<String>, _>(r, "installed_version"),
            "latest_version":    sqlx::Row::get::<Option<String>, _>(r, "latest_version"),
            "install_source":    sqlx::Row::get::<Option<String>, _>(r, "install_source"),
            "status":            sqlx::Row::get::<String, _>(r, "status"),
        })).collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
        return Ok(());
    }

    if rows.is_empty() {
        println!(
            "{GREEN}✓ No external-tool drift — every computer_external_tools row matches external_tools.latest_version.{RESET}"
        );
        return Ok(());
    }

    println!(
        "{:<11} {:<22} {:<18} {:<18} {:<10} {:<18}",
        "COMPUTER", "TOOL", "INSTALLED", "LATEST", "SOURCE", "STATUS"
    );
    for r in &rows {
        let computer: String = sqlx::Row::get(r, "computer");
        let tid: String = sqlx::Row::get(r, "tool_id");
        let installed: Option<String> = sqlx::Row::get(r, "installed_version");
        let latest: Option<String> = sqlx::Row::get(r, "latest_version");
        let src: Option<String> = sqlx::Row::get(r, "install_source");
        let status: String = sqlx::Row::get(r, "status");
        println!(
            "{:<11} {:<22} {:<18} {:<18} {:<10} {:<18}",
            truncate_for_col(&computer, 11),
            truncate_for_col(&tid, 22),
            truncate_for_col(installed.as_deref().unwrap_or("-"), 18),
            truncate_for_col(latest.as_deref().unwrap_or("-"), 18),
            truncate_for_col(src.as_deref().unwrap_or("-"), 10),
            truncate_for_col(&status, 18),
        );
    }
    println!("\n{} external-tool row(s) with drift.", rows.len());
    Ok(())
}

async fn handle_ext_install(
    pool: &sqlx::PgPool,
    tool_id: &str,
    computer: Option<String>,
    all: bool,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    if computer.is_none() && !all {
        anyhow::bail!("pass --all or --computer <name> to pick targets");
    }
    if computer.is_some() && all {
        anyhow::bail!("--computer and --all are mutually exclusive");
    }

    let (plans, skipped) = ff_agent::external_tools_installer::resolve_install_plans(
        pool,
        tool_id,
        computer.as_deref(),
        all,
    )
    .await?;

    let display_name = plans
        .first()
        .map(|p| p.display_name.clone())
        .unwrap_or_else(|| tool_id.to_string());
    let latest_version = plans.first().and_then(|p| p.latest_version.clone());

    if plans.is_empty() && skipped.is_empty() {
        println!(
            "{YELLOW}No target computers found for tool_id='{tool_id}'. Nothing to do.{RESET}"
        );
        return Ok(());
    }

    println!("{CYAN}▶ ff ext install {tool_id}{RESET}");
    println!("  tool:            {display_name} ({tool_id})");
    println!(
        "  latest upstream: {}",
        latest_version.as_deref().unwrap_or("(unknown)")
    );
    println!("  targets:         {} computer(s)", plans.len());
    if plans.is_empty() {
        println!("{YELLOW}No resolvable targets. Nothing to do.{RESET}");
        for (name, why) in &skipped {
            println!("    {YELLOW}⚠ skip{RESET} {name}: {why}");
        }
        return Ok(());
    }

    println!(
        "\n  {:<10} {:<14} {:<14} {:<10} {:<22} command",
        "computer", "os_family", "method", "installed", "playbook_key"
    );
    for p in &plans {
        let short_cmd = if p.command.len() > 60 {
            format!("{}…", &p.command[..60])
        } else {
            p.command.clone()
        };
        println!(
            "  {:<10} {:<14} {:<14} {:<10} {:<22} {}",
            p.computer_name,
            p.os_family,
            p.install_method,
            p.installed_version.as_deref().unwrap_or("-"),
            p.playbook_key,
            short_cmd,
        );
    }
    for (name, why) in &skipped {
        println!("  {YELLOW}⚠ skip{RESET} {name}: {why}");
    }

    if dry_run {
        println!(
            "\n{YELLOW}Dry run — not enqueuing. Drop --dry-run and pass --yes to actually enqueue.{RESET}"
        );
        return Ok(());
    }
    if !yes {
        println!("\n{YELLOW}Pass --yes to actually enqueue these install tasks.{RESET}");
        return Ok(());
    }

    let who = whoami_tag();
    let enqueued = ff_agent::external_tools_installer::enqueue_plans(pool, &plans, &who).await?;

    println!(
        "\n{GREEN}✓ Enqueued {} install task(s):{RESET}",
        enqueued.len()
    );
    for ep in &enqueued {
        println!("  {:<12} {}", ep.computer_name, ep.defer_id);
    }
    println!("\nTrack progress with: ff defer list");
    Ok(())
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
            handle_ports_list(&pool, kind, scope, json).await
        }
        PortsCommand::Scan { computer } => handle_ports_scan(&pool, &computer).await,
    }
}

async fn handle_ports_list(
    pool: &sqlx::PgPool,
    kind: Option<String>,
    scope: Option<String>,
    json: bool,
) -> Result<()> {
    let mut sql = String::from(
        "SELECT port, service, kind, description, exposed_on, scope, managed_by, status
         FROM port_registry
         WHERE 1=1",
    );
    let mut idx = 1;
    if kind.is_some() {
        sql.push_str(&format!(" AND kind = ${idx}"));
        idx += 1;
    }
    if scope.is_some() {
        sql.push_str(&format!(" AND scope = ${idx}"));
    }
    sql.push_str(" ORDER BY kind ASC, port ASC");

    let mut q = sqlx::query(&sql);
    if let Some(k) = &kind {
        q = q.bind(k);
    }
    if let Some(s) = &scope {
        q = q.bind(s);
    }

    let rows = q
        .fetch_all(pool)
        .await
        .map_err(|e| anyhow::anyhow!("list port_registry: {e}"))?;

    if json {
        let arr: Vec<_> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "port":        sqlx::Row::get::<i32, _>(r, "port"),
                    "service":     sqlx::Row::get::<String, _>(r, "service"),
                    "kind":        sqlx::Row::get::<String, _>(r, "kind"),
                    "description": sqlx::Row::get::<String, _>(r, "description"),
                    "exposed_on":  sqlx::Row::get::<String, _>(r, "exposed_on"),
                    "scope":       sqlx::Row::get::<String, _>(r, "scope"),
                    "managed_by":  sqlx::Row::get::<Option<String>, _>(r, "managed_by"),
                    "status":      sqlx::Row::get::<String, _>(r, "status"),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
        return Ok(());
    }

    if rows.is_empty() {
        println!("{YELLOW}No rows in port_registry. Run `ff ports seed` first.{RESET}");
        return Ok(());
    }

    println!(
        "{:<6} {:<22} {:<18} {:<26} {:<10} {:<11} {}",
        "PORT", "SERVICE", "KIND", "EXPOSED_ON", "SCOPE", "STATUS", "DESCRIPTION",
    );
    println!("  {}", "-".repeat(130));

    let mut last_kind: Option<String> = None;
    for r in &rows {
        let port: i32 = sqlx::Row::get(r, "port");
        let service: String = sqlx::Row::get(r, "service");
        let k: String = sqlx::Row::get(r, "kind");
        let description: String = sqlx::Row::get(r, "description");
        let exposed_on: String = sqlx::Row::get(r, "exposed_on");
        let scp: String = sqlx::Row::get(r, "scope");
        let status: String = sqlx::Row::get(r, "status");

        if last_kind.as_deref() != Some(k.as_str()) {
            println!("\n{CYAN}── {k} ──{RESET}");
            last_kind = Some(k.clone());
        }

        let status_color = match status.as_str() {
            "active" => GREEN,
            "deprecated" => RED,
            "planned" => YELLOW,
            _ => "",
        };

        println!(
            "{:<6} {:<22} {:<18} {:<26} {:<10} {status_color}{:<11}{RESET} {}",
            port,
            truncate_for_col(&service, 22),
            truncate_for_col(&k, 18),
            truncate_for_col(&exposed_on, 26),
            truncate_for_col(&scp, 10),
            status,
            description,
        );
    }
    println!("\n{} port(s) registered.", rows.len());
    Ok(())
}

async fn handle_ports_scan(pool: &sqlx::PgPool, computer: &str) -> Result<()> {
    use tokio::process::Command as TokCmd;

    println!("{CYAN}▶ Scanning {computer} for listening ports{RESET}");

    let this_hostname = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_default();
    let is_local = this_hostname.starts_with(&computer.to_lowercase())
        || computer.eq_ignore_ascii_case("localhost")
        || computer.eq_ignore_ascii_case("local");

    // Prefer `ss -tlnH` on Linux; `lsof -iTCP -sTCP:LISTEN -n -P` on macOS.
    // Run both; whichever produces output wins.
    let probe_cmd = "sh -c 'ss -tlnH 2>/dev/null || lsof -iTCP -sTCP:LISTEN -n -P 2>/dev/null'";

    let output = if is_local {
        TokCmd::new("sh").args(["-c", probe_cmd]).output().await
    } else {
        let row = sqlx::query("SELECT ssh_user, ip FROM fleet_nodes WHERE name = $1 LIMIT 1")
            .bind(computer)
            .fetch_optional(pool)
            .await
            .map_err(|e| anyhow::anyhow!("lookup fleet_nodes: {e}"))?;

        let (ssh_user, ip) = match row {
            Some(r) => (
                sqlx::Row::get::<String, _>(&r, "ssh_user"),
                sqlx::Row::get::<String, _>(&r, "ip"),
            ),
            None => {
                println!("{RED}✗ Unknown computer '{computer}' — not in fleet_nodes.{RESET}");
                return Ok(());
            }
        };

        let dest = format!("{ssh_user}@{ip}");
        TokCmd::new("ssh")
            .args([
                "-o",
                "ConnectTimeout=8",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "BatchMode=yes",
                &dest,
                probe_cmd,
            ])
            .output()
            .await
    };

    let probe_stdout = match output {
        Ok(o) if o.status.success() || !o.stdout.is_empty() => {
            String::from_utf8_lossy(&o.stdout).to_string()
        }
        Ok(o) => {
            println!(
                "{RED}✗ probe exited {}:{RESET}\n{}",
                o.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&o.stderr),
            );
            return Ok(());
        }
        Err(e) => {
            println!("{RED}✗ probe failed: {e}{RESET}");
            return Ok(());
        }
    };

    // Parse listening ports from either `ss` or `lsof` output.
    let mut listening: std::collections::BTreeSet<u16> = std::collections::BTreeSet::new();
    for line in probe_stdout.lines() {
        for tok in line.split_whitespace() {
            if let Some(colon) = tok.rfind(':') {
                let tail = &tok[colon + 1..];
                let num_end = tail
                    .find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(tail.len());
                if num_end == 0 {
                    continue;
                }
                if let Ok(p) = tail[..num_end].parse::<u16>() {
                    listening.insert(p);
                }
            }
        }
    }

    let reg_rows = sqlx::query(
        "SELECT port, service, kind, exposed_on, status FROM port_registry ORDER BY port ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("load port_registry: {e}"))?;

    let mut registered: std::collections::BTreeMap<u16, (String, String, String, String)> =
        std::collections::BTreeMap::new();
    for r in &reg_rows {
        let port: i32 = sqlx::Row::get(r, "port");
        let service: String = sqlx::Row::get(r, "service");
        let kind: String = sqlx::Row::get(r, "kind");
        let exposed_on: String = sqlx::Row::get(r, "exposed_on");
        let status: String = sqlx::Row::get(r, "status");
        registered.insert(port as u16, (service, kind, exposed_on, status));
    }

    println!(
        "\n{CYAN}Listening on {computer}:{RESET} {} port(s)",
        listening.len()
    );
    let mut unexpected: Vec<u16> = Vec::new();
    for p in &listening {
        match registered.get(p) {
            Some((svc, kind, _exposed, status)) => {
                let color = if status == "deprecated" { RED } else { GREEN };
                println!("  {color}✓ {:<6}{RESET} {svc}  ({kind}, {status})", p);
            }
            None => unexpected.push(*p),
        }
    }
    if !unexpected.is_empty() {
        println!("\n{YELLOW}⚠ Unexpected listeners (not in port_registry):{RESET}");
        for p in unexpected {
            println!("  {YELLOW}? {}{RESET}", p);
        }
    }

    // Which registered services should be on this box but aren't?
    // Heuristic: exposed_on contains the computer name, or is a group label.
    let mut missing: Vec<(u16, String, String)> = Vec::new();
    let key = computer.to_ascii_lowercase();
    for (port, (svc, kind, exposed_on, status)) in &registered {
        if status != "active" {
            continue;
        }
        let eo = exposed_on.to_ascii_lowercase();
        let relevant = eo.contains(&key)
            || eo == "all_members"
            || eo == "all_members_with_gguf"
            || eo == "nats_cluster_members"
            || eo == "gpu_members";
        if !relevant {
            continue;
        }
        if !listening.contains(port) {
            missing.push((*port, svc.clone(), kind.clone()));
        }
    }
    if !missing.is_empty() {
        println!("\n{YELLOW}⚠ Expected but not listening:{RESET}");
        for (port, svc, kind) in missing {
            println!("  {YELLOW}∅ {:<6}{RESET} {svc}  ({kind})", port);
        }
    } else {
        println!(
            "\n{GREEN}✓ Every active port_registry entry relevant to {computer} is listening.{RESET}"
        );
    }

    Ok(())
}

// ─── ff cloud-llm ──────────────────────────────────────────────────────────

async fn handle_cloud_llm(cmd: CloudLlmCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        CloudLlmCommand::List { json } => handle_cloud_llm_list(&pool, json).await,
        CloudLlmCommand::SetKey { provider_id, value } => {
            handle_cloud_llm_set_key(&pool, &provider_id, value).await
        }
        CloudLlmCommand::Usage { since } => handle_cloud_llm_usage(&pool, &since).await,
        CloudLlmCommand::Test { provider_id, model } => {
            handle_cloud_llm_test(&pool, &provider_id, model).await
        }
    }
}

async fn handle_cloud_llm_list(pool: &sqlx::PgPool, json: bool) -> Result<()> {
    let providers = ff_agent::cloud_llm_registry::list_providers(pool)
        .await
        .map_err(|e| anyhow::anyhow!("list cloud_llm_providers: {e}"))?;

    if providers.is_empty() {
        println!(
            "{YELLOW}No cloud providers registered. Migration V35 seeds them at startup.{RESET}"
        );
        return Ok(());
    }

    // Enrich with a per-provider "secret set?" flag.
    let mut enriched: Vec<(ff_agent::cloud_llm_registry::Provider, bool)> = Vec::new();
    for p in providers {
        let has_key = ff_db::pg_get_secret(pool, &p.secret_key)
            .await
            .map(|v| v.map(|s| !s.is_empty()).unwrap_or(false))
            .unwrap_or(false);
        enriched.push((p, has_key));
    }

    if json {
        let arr: Vec<_> = enriched
            .iter()
            .map(|(p, has_key)| {
                serde_json::json!({
                    "id": p.id,
                    "display_name": p.display_name,
                    "base_url": p.base_url,
                    "auth_kind": p.auth_kind,
                    "model_prefix": p.model_prefix,
                    "request_format": p.request_format,
                    "enabled": p.enabled,
                    "secret_key": p.secret_key,
                    "secret_set": has_key,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
        return Ok(());
    }

    println!(
        "{:<12} {:<22} {:<14} {:<10} {:<22} {:<7} {}",
        "ID", "NAME", "MODEL_PREFIX", "AUTH", "REQUEST_FORMAT", "ENABLED", "SECRET",
    );
    println!("  {}", "-".repeat(110));
    for (p, has_key) in &enriched {
        let secret_col = if *has_key {
            format!("{GREEN}set{RESET}")
        } else {
            format!("{RED}missing{RESET}")
        };
        let enabled = if p.enabled {
            format!("{GREEN}yes{RESET}")
        } else {
            format!("{RED}no{RESET}")
        };
        println!(
            "{:<12} {:<22} {:<14} {:<10} {:<22} {:<7} {}  ({})",
            p.id,
            truncate_for_col(&p.display_name, 22),
            truncate_for_col(&p.model_prefix, 14),
            p.auth_kind,
            truncate_for_col(&p.request_format, 22),
            enabled,
            secret_col,
            p.secret_key,
        );
    }
    println!("\n{} provider(s) registered.", enriched.len());
    Ok(())
}

async fn handle_cloud_llm_set_key(
    pool: &sqlx::PgPool,
    provider_id: &str,
    value_override: Option<String>,
) -> Result<()> {
    // Resolve the provider so we know which secret_key to write to.
    let providers = ff_agent::cloud_llm_registry::list_providers(pool)
        .await
        .map_err(|e| anyhow::anyhow!("list providers: {e}"))?;
    let Some(provider) = providers.into_iter().find(|p| p.id == provider_id) else {
        eprintln!("{RED}✗ Unknown provider '{provider_id}'. Try `ff cloud-llm list`.{RESET}");
        std::process::exit(1);
    };

    let key_value = match value_override {
        Some(v) if !v.is_empty() => v,
        _ => {
            eprintln!(
                "{CYAN}▶ Enter API key for {} (input hidden via terminal; paste + Enter):{RESET}",
                provider.id
            );
            let mut buf = String::new();
            io::stdin()
                .read_line(&mut buf)
                .context("read API key from stdin")?;
            buf.trim().to_string()
        }
    };

    if key_value.is_empty() {
        eprintln!("{RED}✗ Empty API key, aborting.{RESET}");
        std::process::exit(2);
    }

    let who = whoami_tag();
    ff_db::pg_set_secret(
        pool,
        &provider.secret_key,
        &key_value,
        Some(&format!("cloud LLM api key for {}", provider.id)),
        Some(&who),
    )
    .await
    .map_err(|e| anyhow::anyhow!("store secret: {e}"))?;

    println!(
        "{GREEN}✓ Stored API key for '{}' at secret `{}` ({} bytes, by {who}).{RESET}",
        provider.id,
        provider.secret_key,
        key_value.len(),
    );
    println!("Test it: ff cloud-llm test {}", provider.id);
    Ok(())
}

async fn handle_cloud_llm_usage(pool: &sqlx::PgPool, since: &str) -> Result<()> {
    let secs = parse_since_to_secs(since).unwrap_or(24 * 3600);
    let rows = sqlx::query(
        r#"SELECT provider_id,
                  COUNT(*) AS calls,
                  COALESCE(SUM(tokens_input), 0)::BIGINT  AS tokens_in,
                  COALESCE(SUM(tokens_output), 0)::BIGINT AS tokens_out,
                  COALESCE(AVG(request_duration_ms), 0)::FLOAT8 AS avg_ms
             FROM cloud_llm_usage
             WHERE used_at > NOW() - ($1::BIGINT * INTERVAL '1 second')
             GROUP BY provider_id
             ORDER BY calls DESC"#,
    )
    .bind(secs as i64)
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("query cloud_llm_usage: {e}"))?;

    if rows.is_empty() {
        println!(
            "{YELLOW}No cloud LLM calls recorded in the last {since} (window: {secs}s).{RESET}"
        );
        return Ok(());
    }

    println!(
        "{:<12} {:>8} {:>12} {:>12} {:>10}",
        "PROVIDER", "CALLS", "TOKENS_IN", "TOKENS_OUT", "AVG_MS",
    );
    println!("  {}", "-".repeat(60));
    for r in &rows {
        let id: String = sqlx::Row::get(r, "provider_id");
        let calls: i64 = sqlx::Row::get(r, "calls");
        let ti: i64 = sqlx::Row::get(r, "tokens_in");
        let to: i64 = sqlx::Row::get(r, "tokens_out");
        let avg_ms: f64 = sqlx::Row::get(r, "avg_ms");
        println!(
            "{:<12} {:>8} {:>12} {:>12} {:>10.1}",
            id, calls, ti, to, avg_ms,
        );
    }
    println!("\nWindow: last {since} ({secs}s).");
    Ok(())
}

/// Parse a window string like `24h`, `15m`, `7d`, `3600s` into seconds.
fn parse_since_to_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, suffix) = s.split_at(s.len().saturating_sub(1));
    let n: u64 = num.parse().ok()?;
    match suffix {
        "s" => Some(n),
        "m" => Some(n * 60),
        "h" => Some(n * 3600),
        "d" => Some(n * 86400),
        _ => s.parse().ok(),
    }
}

async fn handle_cloud_llm_test(
    pool: &sqlx::PgPool,
    provider_id: &str,
    model_override: Option<String>,
) -> Result<()> {
    let providers = ff_agent::cloud_llm_registry::list_providers(pool)
        .await
        .map_err(|e| anyhow::anyhow!("list providers: {e}"))?;
    let Some(provider) = providers.into_iter().find(|p| p.id == provider_id) else {
        eprintln!("{RED}✗ Unknown provider '{provider_id}'.{RESET}");
        std::process::exit(1);
    };

    let key = ff_db::pg_get_secret(pool, &provider.secret_key)
        .await
        .map_err(|e| anyhow::anyhow!("read secret: {e}"))?;
    let Some(api_key) = key.filter(|k| !k.is_empty()) else {
        eprintln!(
            "{RED}✗ No API key set for '{}'. Run `ff cloud-llm set-key {}`.{RESET}",
            provider.id, provider.id,
        );
        std::process::exit(1);
    };

    let probe_model = model_override.unwrap_or_else(|| match provider.id.as_str() {
        "openai" => "openai/gpt-4o-mini".to_string(),
        "anthropic" => "claude-3-5-haiku-latest".to_string(),
        "moonshot" => "kimi/moonshot-v1-8k".to_string(),
        "google" => "gemini/gemini-1.5-flash".to_string(),
        _ => "test".to_string(),
    });

    println!(
        "{CYAN}▶ Probing {} ({}) with model '{}' (api_key=<redacted>){RESET}",
        provider.id, provider.request_format, probe_model,
    );

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default();

    // Strip the `gemini/` prefix for Google since that's a ForgeFleet-level
    // routing hint, not a real Google model id.
    let wire_model = if provider.request_format == "google_generate_content" {
        probe_model
            .strip_prefix("gemini/")
            .unwrap_or(&probe_model)
            .to_string()
    } else {
        probe_model.clone()
    };

    let result = probe_cloud_provider(
        &http,
        &provider.request_format,
        &provider.base_url,
        &api_key,
        &wire_model,
    )
    .await;

    match result {
        Ok(reply) => {
            println!("{GREEN}✓ {} replied:{RESET} {}", provider.id, reply);
            Ok(())
        }
        Err(msg) => {
            eprintln!("{RED}✗ {} probe failed:{RESET} {msg}", provider.id);
            std::process::exit(1);
        }
    }
}

/// Dispatch a "reply OK" probe in whatever wire format the provider expects.
/// Mirrors the translation the gateway's cloud_llm module does.
async fn probe_cloud_provider(
    http: &reqwest::Client,
    fmt: &str,
    base_url: &str,
    api_key: &str,
    model: &str,
) -> Result<String, String> {
    let base = base_url.trim_end_matches('/');
    let prompt = "Reply with the word OK.";

    let (url, body, headers): (String, serde_json::Value, Vec<(&str, String)>) = match fmt {
        "openai_chat" => (
            format!("{base}/chat/completions"),
            serde_json::json!({"model": model,
                "messages":[{"role":"user","content":prompt}], "max_tokens":16}),
            vec![("authorization", format!("Bearer {api_key}"))],
        ),
        "anthropic_messages" => (
            format!("{base}/messages"),
            serde_json::json!({"model": model, "max_tokens":16,
                "messages":[{"role":"user","content":prompt}]}),
            vec![
                ("x-api-key", api_key.to_string()),
                ("anthropic-version", "2023-06-01".to_string()),
            ],
        ),
        "google_generate_content" => (
            format!("{base}/models/{model}:generateContent?key={api_key}"),
            serde_json::json!({
                "contents":[{"role":"user","parts":[{"text":prompt}]}],
                "generationConfig":{"maxOutputTokens":16}}),
            vec![],
        ),
        other => return Err(format!("unsupported request_format '{other}'")),
    };

    let mut req = http.post(&url).json(&body);
    for (k, v) in &headers {
        req = req.header(*k, v);
    }
    let resp = req.send().await.map_err(|e| format!("send: {e}"))?;
    let status = resp.status();
    let v: serde_json::Value = resp.json().await.map_err(|e| format!("json: {e}"))?;
    if !status.is_success() {
        let msg = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("(no error message)");
        return Err(format!("HTTP {} — {msg}", status.as_u16()));
    }
    let text = match fmt {
        "google_generate_content" => v["candidates"][0]["content"]["parts"][0]["text"].as_str(),
        "anthropic_messages" => v["content"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|p| p.get("text"))
            .and_then(|t| t.as_str()),
        _ => v["choices"][0]["message"]["content"].as_str(),
    };
    Ok(text.unwrap_or("(no content)").to_string())
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

async fn handle_brain(cmd: BrainCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        BrainCommand::Index {
            vault_path,
            subfolder,
        } => {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/venkat".into());
            let vault =
                vault_path.unwrap_or_else(|| format!("{home}/projects/Yarli_KnowledgeBase"));
            let sub = subfolder.unwrap_or_default();
            let config = ff_brain::VaultConfig {
                vault_path: std::path::PathBuf::from(&vault),
                brain_subfolder: sub.clone(),
            };
            let root = if sub.is_empty() {
                vault.clone()
            } else {
                format!("{vault}/{sub}")
            };
            println!("{CYAN}▶ Indexing vault: {root}{RESET}");
            let report = ff_brain::index_vault(&pool, &config)
                .await
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
            let summary = ff_brain::detect_communities(&pool)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            println!("  communities: {}", summary.communities_found);
            println!("  largest:     {} nodes", summary.largest_community);
        }
        BrainCommand::Stats => {
            let nodes = ff_db::pg_list_brain_vault_nodes_current(&pool, None)
                .await
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

async fn handle_openclaw(cmd: OpenclawCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        OpenclawCommand::Status { json } => {
            // Join computers → openclaw_installations → computer_software
            // (software_id = 'openclaw'). LEFT JOINs so every computer shows
            // up even if OpenClaw hasn't been installed/configured yet.
            let rows: Vec<(
                String,                                // name
                String,                                // primary_ip
                Option<String>,                        // mode
                Option<String>,                        // gateway_url
                Option<chrono::DateTime<chrono::Utc>>, // last_reconfigured_at
                Option<String>,                        // installed_version
            )> = sqlx::query_as(
                "SELECT c.name, \
                        c.primary_ip, \
                        oi.mode, \
                        oi.gateway_url, \
                        oi.last_reconfigured_at, \
                        cs.installed_version AS openclaw_version \
                 FROM computers c \
                 LEFT JOIN openclaw_installations oi ON oi.computer_id = c.id \
                 LEFT JOIN computer_software cs \
                        ON cs.computer_id = c.id AND cs.software_id = 'openclaw' \
                 ORDER BY c.name",
            )
            .fetch_all(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("query openclaw status: {e}"))?;

            if json {
                let items: Vec<serde_json::Value> = rows
                    .into_iter()
                    .map(|(name, ip, mode, url, reconfigured, version)| {
                        serde_json::json!({
                            "name": name,
                            "primary_ip": ip,
                            "mode": mode,
                            "gateway_url": url,
                            "last_reconfigured_at": reconfigured.map(|t| t.to_rfc3339()),
                            "openclaw_version": version,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&items)?);
                return Ok(());
            }

            if rows.is_empty() {
                println!("(no computers registered)");
                return Ok(());
            }

            println!(
                "{:<14} {:<16} {:<8} {:<34} {:<22} {}",
                "NAME", "IP", "MODE", "GATEWAY URL", "LAST RECONFIG", "OPENCLAW"
            );
            for (name, ip, mode, url, reconfigured, version) in rows {
                let mode_s = mode.as_deref().unwrap_or("-");
                let url_s = url.as_deref().unwrap_or("-");
                let ts_s = reconfigured
                    .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
                    .unwrap_or_else(|| "-".into());
                let ver_s = version.as_deref().unwrap_or("-");
                let mode_colored = match mode_s {
                    "gateway" => format!("{GREEN}{mode_s}{RESET}"),
                    "node" => format!("{CYAN}{mode_s}{RESET}"),
                    _ => mode_s.to_string(),
                };
                // Account for color escape width when padding.
                let mode_pad = if matches!(mode_s, "gateway" | "node") {
                    format!("{:<8}", mode_colored)
                } else {
                    format!("{:<8}", mode_s)
                };
                println!(
                    "{:<14} {:<16} {} {:<34} {:<22} {}",
                    name, ip, mode_pad, url_s, ts_s, ver_s
                );
            }
        }
        OpenclawCommand::Devices { command } => {
            handle_openclaw_devices(&pool, command).await?;
        }
    }
    Ok(())
}

async fn handle_openclaw_devices(pool: &sqlx::PgPool, cmd: OpenclawDevicesCommand) -> Result<()> {
    // Need a local OpenClawManager — the my_computer_id / my_primary_ip
    // arguments aren't consulted by export_devices/import_devices (those
    // just shell out to the local `openclaw` CLI), so we pass placeholders.
    let local_name = ff_agent::fleet_info::resolve_this_node_name().await;
    let (computer_id, primary_ip) = sqlx::query_as::<_, (uuid::Uuid, String)>(
        "SELECT id, primary_ip FROM computers WHERE LOWER(name) = LOWER($1)",
    )
    .bind(&local_name)
    .fetch_optional(pool)
    .await
    .map_err(|e| anyhow::anyhow!("query computers: {e}"))?
    .unwrap_or((uuid::Uuid::nil(), "127.0.0.1".to_string()));

    let mgr = ff_agent::openclaw::OpenClawManager::new(pool.clone(), computer_id, primary_ip);

    match cmd {
        OpenclawDevicesCommand::Export { stash } => {
            let export = mgr
                .export_devices()
                .await
                .map_err(|e| anyhow::anyhow!("export_devices: {e}"))?;

            if stash {
                sqlx::query(
                    "INSERT INTO fleet_secrets (key, value, updated_by, updated_at) \
                     VALUES ($1, $2, 'ff openclaw devices export', NOW()) \
                     ON CONFLICT (key) DO UPDATE \
                     SET value = $2, updated_at = NOW()",
                )
                .bind(ff_agent::openclaw::DEVICE_PAIRINGS_SECRET_KEY)
                .bind(&export)
                .execute(pool)
                .await
                .map_err(|e| anyhow::anyhow!("stash secret: {e}"))?;
                eprintln!(
                    "{GREEN}✓{RESET} stashed {} bytes into fleet_secrets.{}",
                    export.len(),
                    ff_agent::openclaw::DEVICE_PAIRINGS_SECRET_KEY,
                );
            }
            // Emit the JSON to stdout so a caller can pipe it.
            print!("{export}");
            if !export.ends_with('\n') {
                println!();
            }
        }
        OpenclawDevicesCommand::Import { from_secret } => {
            let json = if from_secret {
                match ff_agent::openclaw::lookup_device_pairings_export(pool)
                    .await
                    .map_err(|e| anyhow::anyhow!("lookup stash: {e}"))?
                {
                    Some(v) => v,
                    None => {
                        eprintln!(
                            "{YELLOW}no stashed export found in fleet_secrets.{}{RESET}",
                            ff_agent::openclaw::DEVICE_PAIRINGS_SECRET_KEY
                        );
                        return Ok(());
                    }
                }
            } else {
                use std::io::Read;
                let mut buf = String::new();
                std::io::stdin()
                    .read_to_string(&mut buf)
                    .map_err(|e| anyhow::anyhow!("read stdin: {e}"))?;
                buf
            };

            let n = mgr
                .import_devices(&json)
                .await
                .map_err(|e| anyhow::anyhow!("import_devices: {e}"))?;

            println!("{GREEN}✓{RESET} imported {n} device(s)");

            if from_secret {
                if let Err(e) = ff_agent::openclaw::clear_device_pairings_export(pool).await {
                    eprintln!("{YELLOW}warning:{RESET} failed to clear stashed secret: {e}");
                }
            }
        }
    }
    Ok(())
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
                "{:<14} {:<4} {:<8} {:<36} {}",
                "COMPUTER", "SLOT", "STATUS", "ID", "WORKSPACE"
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
            handle_agent_commit_back(&pool, &session, push, pr).await
        }
        AgentCommand::Fanout {
            prompt,
            backend,
            fanout,
        } => handle_agent_fanout(&pool, prompt, backend, fanout).await,
        AgentCommand::DispatchEach { prompt, backend } => {
            handle_agent_dispatch_each(&pool, prompt, backend).await
        }
    }
}

/// Emit `fanout` shell tasks, each requiring capability `[backend]`.
/// Each task runs `ff run --backend <backend> "<prompt>"` on whichever
/// fleet worker grabs it. Workers compete via the existing SKIP LOCKED
/// claim — natural parallelism up to the count of capable members.
async fn handle_agent_fanout(
    pool: &sqlx::PgPool,
    prompt: String,
    backend: String,
    fanout: u32,
) -> Result<()> {
    use ff_agent::cli_executor::backend_by_name;
    let cfg = backend_by_name(&backend).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown backend '{backend}'; expected one of: claude, codex, gemini, kimi, grok"
        )
    })?;

    // Parent compound task — gives the user a single UUID to watch.
    let leader_computer_id: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT computer_id FROM fleet_leader_state LIMIT 1")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    let parent: uuid::Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO fleet_tasks (
            task_type, summary, payload, priority, created_by_computer_id
        )
        VALUES ('compound', $1, $2, 80, $3)
        RETURNING id
        "#,
    )
    .bind(format!(
        "agent-fanout: {} copies via backend={}",
        fanout, cfg.name
    ))
    .bind(serde_json::json!({
        "kind": "agent_fanout",
        "backend": cfg.name,
        "fanout": fanout,
        "prompt_preview": prompt.chars().take(200).collect::<String>(),
    }))
    .bind(leader_computer_id)
    .fetch_one(pool)
    .await
    .map_err(|e| anyhow::anyhow!("insert parent: {e}"))?;

    // Encode the prompt as a single-quoted shell argument. Replace any
    // single-quote with `'\''` so embedded quotes survive.
    let shell_safe_prompt = prompt.replace('\'', "'\\''");
    let cmd = format!("ff run --backend {} '{shell_safe_prompt}'", cfg.name);
    for i in 0..fanout {
        ff_agent::task_runner::pg_enqueue_shell_task(
            pool,
            &format!("agent-fanout/{i}: {} backend={}", cfg.name, cfg.name),
            &cmd,
            &[cfg.name.to_string()],
            None,
            Some(parent),
            70,
            leader_computer_id,
        )
        .await
        .map_err(|e| anyhow::anyhow!("enqueue child {i}: {e}"))?;
    }

    println!("composed parent task: {parent}");
    println!("watch progress with: ff tasks list --status pending,running --show-id");
    Ok(())
}

/// One shell task per capable member: the same prompt runs on every
/// member that advertises capability `[backend]`. Useful for "have
/// every member summarise their own logs in parallel" patterns.
async fn handle_agent_dispatch_each(
    pool: &sqlx::PgPool,
    prompt: String,
    backend: String,
) -> Result<()> {
    use ff_agent::cli_executor::backend_by_name;
    let cfg = backend_by_name(&backend).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown backend '{backend}'; expected one of: claude, codex, gemini, kimi, grok"
        )
    })?;

    // Find every member whose advertised capability set includes the
    // backend tag. Capabilities are computed on daemon startup (see
    // src/main.rs ~line 2152) and stored implicitly in fleet_workers
    // via the worker registration. Here we approximate by querying
    // computers whose status='ok' — the per-task `requires_capability`
    // matcher will skip incapable members at claim time anyway, so a
    // task to a member without the backend simply stays pending.
    let members: Vec<(uuid::Uuid, String)> =
        sqlx::query_as("SELECT id, name FROM computers WHERE status IN ('ok', 'pending')")
            .fetch_all(pool)
            .await
            .map_err(|e| anyhow::anyhow!("list computers: {e}"))?;

    let leader_computer_id: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT computer_id FROM fleet_leader_state LIMIT 1")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();

    let parent: uuid::Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO fleet_tasks (task_type, summary, payload, priority, created_by_computer_id)
        VALUES ('compound', $1, $2, 80, $3)
        RETURNING id
        "#,
    )
    .bind(format!(
        "agent-dispatch-each: {} member(s) via backend={}",
        members.len(),
        cfg.name
    ))
    .bind(serde_json::json!({
        "kind": "agent_dispatch_each",
        "backend": cfg.name,
        "members": members.iter().map(|(_, n)| n.clone()).collect::<Vec<_>>(),
        "prompt_preview": prompt.chars().take(200).collect::<String>(),
    }))
    .bind(leader_computer_id)
    .fetch_one(pool)
    .await
    .map_err(|e| anyhow::anyhow!("insert parent: {e}"))?;

    let shell_safe_prompt = prompt.replace('\'', "'\\''");
    let cmd = format!("ff run --backend {} '{shell_safe_prompt}'", cfg.name);
    for (_id, name) in &members {
        ff_agent::task_runner::pg_enqueue_shell_task(
            pool,
            &format!("agent-dispatch-each: {} on {}", cfg.name, name),
            &cmd,
            &[cfg.name.to_string()],
            Some(name),
            Some(parent),
            70,
            leader_computer_id,
        )
        .await
        .map_err(|e| anyhow::anyhow!("enqueue task on {name}: {e}"))?;
    }

    println!("composed parent task: {parent}");
    println!("watch progress with: ff tasks list --status pending,running --show-id");
    Ok(())
}

// ─── #118: ff agent commit-back — fleet-LLM work → PR on origin/main ────────
//
// Lifts code produced by a fleet LLM in a sub-agent workspace back to Taylor's
// canonical repo via a feature branch + (optional) PR against origin/main.
//
// Flow:
//   1. Look up `work_outputs` WHERE agent_session_id = <session>. Pick the
//      latest row. Extract `produced_on_computer`, `modified_files`, title.
//   2. Resolve the worker's ssh_user + primary_ip from `fleet_nodes`.
//      Resolve the canonical source-tree path via `software_registry.install_path`
//      (falls back to `~/.forgefleet/sub-agent-0/forge-fleet` per convention).
//   3. SSH into the worker and run git checkout -b / add / commit / (push / gh pr create).
//   4. Persist the resulting branch + PR URL back into `work_items.pr_url`
//      (via the work_item linked to the work_output).
//   5. Best-effort publish `fleet.events.agent.commit_back_completed` on NATS.
async fn handle_agent_commit_back(
    pool: &sqlx::PgPool,
    session_id: &str,
    push: bool,
    pr: bool,
) -> Result<()> {
    use tokio::process::Command;

    // 1. Look up the latest work_output for this session.
    let row: Option<(
        uuid::Uuid,        // work_output.id
        uuid::Uuid,        // work_item_id
        Option<String>,    // title
        Option<String>,    // produced_on_computer
        serde_json::Value, // modified_files
        Option<String>,    // llm_model_id
        Option<i32>,       // llm_tokens_input
        Option<i32>,       // llm_tokens_output
    )> = sqlx::query_as(
        "SELECT id, work_item_id, title, produced_on_computer, modified_files, \
                llm_model_id, llm_tokens_input, llm_tokens_output \
         FROM work_outputs \
         WHERE agent_session_id = $1 \
         ORDER BY produced_at DESC \
         LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| anyhow::anyhow!("query work_outputs: {e}"))?;

    let (wo_id, work_item_id, title, worker, modified_files_json, model_id, tok_in, tok_out) = row
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no work_outputs row with agent_session_id={session_id} — \
             was the session persisted, and did it produce a work_output?"
            )
        })?;

    let worker = worker.ok_or_else(|| {
        anyhow::anyhow!("work_output {wo_id} has no produced_on_computer — cannot locate worker")
    })?;

    let modified_files: Vec<String> = serde_json::from_value(modified_files_json.clone())
        .map_err(|e| anyhow::anyhow!("modified_files is not a JSON string array: {e}"))?;
    if modified_files.is_empty() {
        return Err(anyhow::anyhow!(
            "work_output {wo_id} has no modified_files — nothing to commit"
        ));
    }

    // 2. Resolve SSH target + workspace path.
    let (ssh_user, primary_ip): (String, String) =
        sqlx::query_as("SELECT ssh_user, ip FROM fleet_nodes WHERE name = $1")
            .bind(&worker)
            .fetch_optional(pool)
            .await
            .map_err(|e| anyhow::anyhow!("lookup fleet_nodes: {e}"))?
            .ok_or_else(|| anyhow::anyhow!("no fleet_nodes row for computer={worker}"))?;

    // Per reference_source_tree_locations.md: non-Taylor members use
    // ~/.forgefleet/sub-agent-0/forge-fleet. Taylor itself uses ~/projects/forge-fleet.
    let workspace = if worker.eq_ignore_ascii_case("taylor") {
        "~/projects/forge-fleet"
    } else {
        "~/.forgefleet/sub-agent-0/forge-fleet"
    };

    // 3. Build branch name: fleet/<worker>/<yyyymmdd>-<slug>.
    let now = chrono::Utc::now();
    let stamp = now.format("%Y%m%d-%H%M%S").to_string();
    let title_slug = slugify_for_branch(title.as_deref().unwrap_or("agent-session"));
    let branch_name = format!("fleet/{}/{stamp}-{title_slug}", worker);

    let commit_msg = format!(
        "{}\n\nProduced by ff agent on {worker} in session {session_id}.\n\n\
         Co-Authored-By: ForgeFleet Agent <agent@forgefleet.local>",
        title.as_deref().unwrap_or("ff agent commit-back")
    );

    eprintln!("{CYAN}▶ ff agent commit-back{RESET}");
    eprintln!("  session:   {session_id}");
    eprintln!("  worker:    {worker} ({ssh_user}@{primary_ip})");
    eprintln!("  workspace: {workspace}");
    eprintln!("  branch:    {branch_name}");
    eprintln!("  files:     {} modified", modified_files.len());
    for f in &modified_files {
        eprintln!("             {f}");
    }

    // Build the remote shell script. Do NOT stage via `git add .` — use the
    // recorded list, so concurrent unrelated edits on the worker don't leak in.
    let mut script = String::new();
    script.push_str(&format!("cd {workspace} && "));
    script.push_str(&format!(
        "git fetch origin main >/dev/null 2>&1 || true && \
         git checkout -b {shell_branch} 2>&1 && ",
        shell_branch = shell_quote(&branch_name)
    ));
    for f in &modified_files {
        script.push_str(&format!("git add -- {} && ", shell_quote(f)));
    }
    script.push_str(&format!(
        "git commit -m {msg} 2>&1",
        msg = shell_quote(&commit_msg)
    ));

    let target = format!("{ssh_user}@{primary_ip}");
    let out = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "StrictHostKeyChecking=accept-new",
            &target,
            &script,
        ])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("ssh commit: {e}"))?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        return Err(anyhow::anyhow!(
            "remote git checkout/add/commit failed (rc={:?}):\n  stdout: {}\n  stderr: {}",
            out.status.code(),
            stdout.trim(),
            stderr.trim()
        ));
    }
    eprintln!("{GREEN}✓ committed{RESET}");

    // 4. Optional push.
    let should_push = push || pr;
    if should_push {
        let push_cmd = format!(
            "cd {workspace} && git push -u origin {br}",
            br = shell_quote(&branch_name)
        );
        let out = Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "StrictHostKeyChecking=accept-new",
                &target,
                &push_cmd,
            ])
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("ssh push: {e}"))?;
        if !out.status.success() {
            return Err(anyhow::anyhow!(
                "remote git push failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        eprintln!("{GREEN}✓ pushed{RESET} origin/{branch_name}");
    }

    // 5. Optional PR via gh on the worker.
    let mut pr_url: Option<String> = None;
    if pr {
        // Confirm gh auth before attempting.
        let auth_check = Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "StrictHostKeyChecking=accept-new",
                &target,
                "gh auth status >/dev/null 2>&1 && echo ok || echo missing",
            ])
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("ssh gh auth status: {e}"))?;
        let auth_ok = String::from_utf8_lossy(&auth_check.stdout).trim() == "ok";
        if !auth_ok {
            return Err(anyhow::anyhow!(
                "gh CLI is not authenticated on {worker}. \
                 Run `ssh {target} gh auth login` first, or skip --pr."
            ));
        }

        let body = format!(
            "Produced by ff agent on {worker} in session {session_id}.\n\n\
             - Worker: {worker}\n\
             - Model:  {}\n\
             - Tokens: prompt={} completion={}\n\
             - Files:  {} modified\n\n\
             Generated by `ff agent commit-back`.",
            model_id.as_deref().unwrap_or("(unknown)"),
            tok_in.unwrap_or(0),
            tok_out.unwrap_or(0),
            modified_files.len(),
        );
        let pr_title = title.as_deref().unwrap_or("ff agent commit-back");

        let gh_cmd = format!(
            "cd {workspace} && gh pr create --base main --head {br} \
             --title {title_q} --body {body_q}",
            br = shell_quote(&branch_name),
            title_q = shell_quote(pr_title),
            body_q = shell_quote(&body),
        );
        let out = Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=30",
                "-o",
                "StrictHostKeyChecking=accept-new",
                &target,
                &gh_cmd,
            ])
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("ssh gh pr create: {e}"))?;
        if !out.status.success() {
            return Err(anyhow::anyhow!(
                "remote `gh pr create` failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !url.is_empty() {
            pr_url = Some(url.clone());
            eprintln!("{GREEN}✓ PR opened{RESET} {url}");
        } else {
            eprintln!("{YELLOW}! PR created but no URL returned{RESET}");
        }
    }

    // Persist branch + PR URL onto the work_item.
    let _ = sqlx::query(
        "UPDATE work_items SET branch_name = COALESCE(branch_name, $2), \
                                pr_url = COALESCE($3, pr_url) \
         WHERE id = $1",
    )
    .bind(work_item_id)
    .bind(&branch_name)
    .bind(pr_url.as_deref())
    .execute(pool)
    .await;

    // Best-effort NATS event.
    let payload = serde_json::json!({
        "session_id": session_id,
        "work_item_id": work_item_id,
        "worker": worker,
        "branch": branch_name,
        "pr_url": pr_url,
        "files": modified_files,
        "ts": now.to_rfc3339(),
    });
    ff_agent::nats_client::publish_json(
        "fleet.events.agent.commit_back_completed".to_string(),
        &payload,
    )
    .await;

    eprintln!();
    eprintln!("{GREEN}✓ ff agent commit-back complete{RESET}");
    if let Some(url) = pr_url {
        println!("{url}");
    } else {
        println!("{branch_name}");
    }
    Ok(())
}

/// Slugify a title for use in a git branch name: lowercase, ASCII-only,
/// non-alphanumerics collapsed to '-', max 40 chars.
fn slugify_for_branch(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(40));
    let mut prev_dash = false;
    for c in s.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
        if out.len() >= 40 {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "session".to_string()
    } else {
        trimmed
    }
}

/// Wrap a string as a single-quoted POSIX shell argument.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            // Close the quote, append an escaped apostrophe, reopen.
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

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
                "{:<38} {:<14} {:<6} {:<10} {:<8} {:<14} {}",
                "ID", "PROJECT", "KIND", "STATUS", "PRIORITY", "ASSIGNEE", "TITLE"
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
        if let Ok(entries) = std::fs::read_dir(&project_dir) {
            for e in entries.flatten() {
                let path = e.path();
                if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }
                if let Ok(md) = e.metadata() {
                    if let Ok(mtime) = md.modified() {
                        if newest.as_ref().map(|(_, t)| mtime > *t).unwrap_or(true) {
                            newest = Some((path, mtime));
                        }
                    }
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
    let content = std::fs::read_to_string(&resolved)
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
                "{:<14} {:<14} {:<8} {:<10} {:<18} {}",
                "ID", "NAME", "BRANCH", "SHA", "SYNCED", "REPO"
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
                    "  {:<30} {:<10} {:<6} {:<8} {}",
                    "BRANCH", "SHA", "PR#", "PR STATE", "PR URL"
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
                "{:<15} {:<16} {:<10} {:<6} {}",
                "NAME", "IP", "RUNTIME", "PRIO", "GH"
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
                "  {:<6} {:<40} {:<12} {:<16} {}",
                "ID", "SUBJECT", "STATUS", "NODE", "CREATED"
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
                    if let Some(output) = t.get("output").and_then(|v| v.as_str()) {
                        if !output.is_empty() {
                            println!("\n  Output:\n    {}", truncate_str(output, 500));
                        }
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
                "{:<20} {:<18} {:<12} {:<10} {}",
                "FIRED", "POLICY", "COMPUTER", "STATE", "MESSAGE"
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

async fn handle_metrics(cmd: MetricsCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        MetricsCommand::History { computer, since } => {
            let secs = parse_duration_secs(&since).unwrap_or(3600);
            let rows =
                ff_agent::metrics_downsampler::history_for_computer(&pool, &computer, secs as i64)
                    .await?;

            if rows.is_empty() {
                println!(
                    "(no metrics rows for {computer} in the last {since} — downsampler writes at minute boundaries on the leader)"
                );
                return Ok(());
            }
            println!(
                "{:<20} {:>6} {:>6} {:>7} {:>8} {:>6} {:>4} {:>4} {:>6}",
                "TIME", "CPU%", "RAM%", "RAM-GB", "DISK-GB", "GPU%", "Q", "ACT", "TOK/S"
            );
            for r in rows {
                println!(
                    "{:<20} {:>6.1} {:>6.1} {:>7.1} {:>8.1} {:>6.1} {:>4} {:>4} {:>6.1}",
                    r.recorded_at.format("%Y-%m-%d %H:%M:%S"),
                    r.cpu_pct.unwrap_or(0.0),
                    r.ram_pct.unwrap_or(0.0),
                    r.ram_used_gb.unwrap_or(0.0),
                    r.disk_free_gb.unwrap_or(0.0),
                    r.gpu_pct.unwrap_or(0.0),
                    r.llm_queue_depth.unwrap_or(0),
                    r.llm_active_requests.unwrap_or(0),
                    r.llm_tokens_per_sec.unwrap_or(0.0),
                );
            }
        }
    }
    Ok(())
}

async fn handle_logs(
    computer: Option<String>,
    service: Option<String>,
    _tail: usize,
) -> Result<()> {
    // Subscribe to `logs.{computer}.{service}.>` on NATS. Both fragments
    // are optional — missing pieces fall back to the `*` wildcard so the
    // user can tail everything or narrow on either axis.
    use futures::StreamExt;
    let url = std::env::var("FORGEFLEET_NATS_URL")
        .unwrap_or_else(|_| "nats://127.0.0.1:4222".to_string());
    let client = match async_nats::connect(&url).await {
        Ok(c) => c,
        Err(e) => {
            println!("{YELLOW}Could not connect to NATS at {url}: {e}{RESET}");
            println!(
                "Set FORGEFLEET_NATS_URL or ensure nats:// is reachable (docker: `forgefleet-nats`)."
            );
            return Ok(());
        }
    };

    let computer = computer.as_deref().unwrap_or("*");
    let service = service.as_deref().unwrap_or("*");
    let subject = format!("logs.{computer}.{service}.>");
    println!("{CYAN}▶ Tailing NATS subject `{subject}` (Ctrl-C to exit){RESET}");

    let mut sub = match client.subscribe(subject.clone()).await {
        Ok(s) => s,
        Err(e) => {
            println!("{YELLOW}NATS subscribe({subject}) failed: {e}{RESET}");
            return Ok(());
        }
    };

    while let Some(msg) = sub.next().await {
        let subject = msg.subject.to_string();
        let body = String::from_utf8_lossy(&msg.payload);
        // Pretty-render: if JSON, show level + message; else raw.
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&msg.payload) {
            let ts = v.get("ts").and_then(|t| t.as_str()).unwrap_or("?");
            let lvl = v.get("level").and_then(|l| l.as_str()).unwrap_or("info");
            let msg_txt = v.get("message").and_then(|m| m.as_str()).unwrap_or("");
            let target = v.get("target").and_then(|t| t.as_str()).unwrap_or("");
            println!("[{ts}] {lvl:<5} {target} {msg_txt}  ({subject})");
        } else {
            println!("[{subject}] {body}");
        }
    }
    Ok(())
}

async fn handle_events(cmd: EventsCommand) -> Result<()> {
    use futures::StreamExt;
    let EventsCommand::Tail { subject, pretty } = cmd;

    let url = std::env::var("FORGEFLEET_NATS_URL")
        .unwrap_or_else(|_| "nats://127.0.0.1:4222".to_string());
    let client = match async_nats::connect(&url).await {
        Ok(c) => c,
        Err(e) => {
            println!("{YELLOW}Could not connect to NATS at {url}: {e}{RESET}");
            println!(
                "Hint: start NATS via `docker compose up -d nats` or set FORGEFLEET_NATS_URL."
            );
            return Ok(());
        }
    };

    println!("{CYAN}▶ Tailing NATS subject `{subject}` (Ctrl-C to exit){RESET}");
    let mut sub = match client.subscribe(subject.clone()).await {
        Ok(s) => s,
        Err(e) => {
            println!("{YELLOW}NATS subscribe({subject}) failed: {e}{RESET}");
            return Ok(());
        }
    };

    while let Some(msg) = sub.next().await {
        let subj = msg.subject.to_string();
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&msg.payload) {
            let rendered = if pretty {
                serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
            } else {
                v.to_string()
            };
            println!("[{subj}] {rendered}");
        } else {
            println!("[{subj}] {}", String::from_utf8_lossy(&msg.payload));
        }
    }
    Ok(())
}

/// Parse a duration like "5m", "1h", "24h", "30s" into seconds.
fn parse_duration_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, unit) = match s.chars().position(|c| !c.is_ascii_digit() && c != '.') {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, "s"),
    };
    let n: f64 = num.parse().ok()?;
    let mult: f64 = match unit {
        "s" | "" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        "d" => 86400.0,
        _ => return None,
    };
    Some((n * mult).round() as u64)
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

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
                    "{:<18} {:<10} {:<22} {:<18} {:<7} {}",
                    "NAME", "HOST", "EXPORT", "PURPOSE", "RO", "MOUNTS"
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
                "{:<38} {:<10} {:<9} {:<18} {:<10} {}",
                "ID", "COMPUTER", "KIND", "CRON", "ENABLED", "LAST"
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
                "{:<38} {:<22} {:<12} {:<10} {:<10} {}",
                "ID", "NAME", "STATUS", "TYPE", "COMPUTER", "CREATED"
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

    let mut config = ff_agent::research::ResearchConfig::default();
    config.query = prompt.to_string();
    config.parallel = parallel;
    config.depth = depth;
    config.output_path = output;
    if let Some(g) = gateway {
        config.gateway_url = g;
    }
    if let Some(m) = planner_model {
        config.planner_model = m;
    }
    if let Some(m) = subagent_model {
        config.subagent_model = m;
    }

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
