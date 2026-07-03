use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use ff_agent::pr_merge_executor::PrMergeReport;
use ff_api::config::ApiConfig;
use ff_api::registry::{BackendEndpoint, BackendRegistry};
use ff_control::{BootstrapOptions, ControlPlane};
use ff_core::config::{self, ConfigHandle, DatabaseMode, FleetConfig, spawn_watcher};
use ff_db::{OperationalStore, RuntimeRegistryStore};
use ff_discovery::health::HealthStatus;
use ff_discovery::{
    NodeRegistry, NodeScanner, ScanTarget, ScannerConfig, build_scan_targets, scan_subnet,
};
use ff_evolution::{
    EvolutionEngine, FailureObservation, FailureSource, VerificationInput, VerificationModel,
};
use ff_gateway::server::GatewayConfig;
use ff_mcp::federation;
use ff_observability::{TelemetryConfig, init_telemetry};
use ff_runtime::{ProcessManager, ProcessManagerConfig};
use ff_updater::builder::BuilderConfig;
use ff_updater::checker::{CheckerConfig, UpdateChecker};
use ff_updater::orchestrator::{OrchestratorConfig, RestartSignal, UpdateOrchestrator};
use ff_updater::rollback::RollbackConfig;
use ff_updater::swapper::SwapperConfig;
use ff_updater::verifier::VerifierConfig;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tracing::{error, info, warn};

/// clap's `--version` output. Mirrors the `Command::Version` subcommand
/// branch so the drift collector sees the same `YYYY.M.D_N (STATE sha)`
/// shape on either path.
const FORGEFLEET_LONG_VERSION: &str = concat!(
    env!("FF_BUILD_VERSION"),
    " (",
    env!("FF_GIT_STATE"),
    " ",
    env!("FF_GIT_SHA"),
    ")"
);

#[derive(Debug, Parser)]
#[command(name = "forgefleet", version = FORGEFLEET_LONG_VERSION, about = "ForgeFleet unified daemon")]
struct Cli {
    /// Config file path (defaults to ~/.forgefleet/fleet.toml)
    #[arg(long)]
    config: Option<PathBuf>,

    /// Worker name override for startup banner and telemetry tagging.
    /// Accepts `--worker-name` (canonical) or `--node-name` (legacy alias
    /// retained so existing deploy scripts + systemd units don't break).
    #[arg(long, alias = "node-name")]
    worker_name: Option<String>,

    /// Role override for startup banner
    #[arg(long)]
    role: Option<String>,

    /// Log level when RUST_LOG is not set
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Emit structured JSON logs
    #[arg(long, default_value_t = false)]
    json_logs: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Start(StartArgs),
    Status,
    Version,
}

#[derive(Debug, Args)]
struct StartArgs {
    /// Force leader mode in banner metadata
    #[arg(long, default_value_t = false)]
    leader: bool,

    /// Disable Pulse v2 subsystems (heartbeat v2, materializer, leader_tick).
    /// v1 heartbeat is unaffected.
    #[arg(long, default_value_t = false)]
    disable_pulse_v2: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let command = cli.command.as_ref().unwrap_or(&Command::Start(StartArgs {
        leader: false,
        disable_pulse_v2: false,
    }));

    match command {
        Command::Start(args) => run_daemon(&cli, args).await,
        Command::Status => run_status(&cli),
        Command::Version => {
            println!("forgefleet {FORGEFLEET_LONG_VERSION}");
            Ok(())
        }
    }
}

async fn run_daemon(cli: &Cli, start: &StartArgs) -> Result<()> {
    let config_path = resolve_config_path(cli.config.clone())?;
    let config = load_or_default_config(&config_path)?;

    let worker_name = resolve_node_name(cli, &config);
    let role = resolve_role(cli, start, &config, &worker_name);

    // Publish node identity for in-process consumers (agent, MCP tools, callbacks).
    // SAFETY: single-threaded at this point — daemon subsystems haven't spawned yet.
    #[allow(unused_unsafe)]
    unsafe {
        std::env::set_var("FORGEFLEET_NODE_NAME", &worker_name);
    }

    // ─── NATS client (optional) ─────────────────────────────────────────────
    // Initialize the process-global NATS client BEFORE tracing so the
    // NATS log-forwarding layer can be attached to the subscriber. NATS is
    // optional — if the connection fails we warn and continue on Redis +
    // Postgres alone (telemetry falls back to file + stdout only).
    //
    // We can't emit tracing events here because the global subscriber is
    // not yet installed; stash the outcome and log it right after
    // `init_logging` succeeds.
    let nats_url = ff_agent::nats_client::resolve_nats_url();
    #[allow(unused_unsafe)]
    unsafe {
        std::env::set_var("FORGEFLEET_NATS_URL", &nats_url);
    }
    let nats_init_outcome: Result<(), String> = ff_agent::nats_client::init_nats(&nats_url)
        .await
        .map_err(|e| e.to_string());

    init_logging(cli, &worker_name).await?;
    match &nats_init_outcome {
        Ok(_) => info!(url = %nats_url, "NATS connected"),
        Err(e) => {
            warn!(url = %nats_url, error = %e, "NATS unavailable — continuing without event bus")
        }
    }
    print_startup_banner(&worker_name, &role, &config_path);

    enforce_database_mode_preflight(&config)?;

    // ─── Shared Postgres pool ───────────────────────────────────────────────
    // Single pool reused across OperationalStore + RuntimeRegistryStore so
    // each daemon opens one set of connections, not two. Before this, each
    // store called PgPoolOptions::new().connect() independently — the same
    // database URL produced two pools × 15 daemons = 30 baseline conns just
    // for the two stores, and that's before fleet_resolver / get_fleet_pool
    // / per-call sqlx::query helpers piled on.
    let url = config.database.url.trim();
    if url.is_empty() {
        anyhow::bail!(
            "database.mode={} requires non-empty [database].url",
            config.database.mode.as_str()
        );
    }
    let shared_pg_pool: std::sync::Arc<sqlx::PgPool> = std::sync::Arc::new(
        ff_core::db::create_pool_with_dsn_failover(&config.database)
            .await
            .with_context(|| {
                format!(
                    "failed to open shared Postgres pool ({})",
                    redact_database_url(url)
                )
            })?,
    );

    // ─── Operational persistence backend (Postgres) ─────────────────────────
    let operational_store = initialize_operational_store(&config, shared_pg_pool.clone()).await?;

    // ─── Runtime registry persistence backend ────────────────────────────────
    let runtime_registry = initialize_runtime_registry(&config, shared_pg_pool.clone()).await?;
    log_database_mode_summary(&config, &operational_store, &runtime_registry);

    // ─── Postgres fleet config seed (fleet.toml → Postgres, first boot only) ──
    if let Some(pg_pool) = operational_store.pg_pool() {
        ff_db::run_postgres_migrations(pg_pool)
            .await
            .context("postgres fleet-config migrations failed")?;

        // Only seed if Postgres fleet_workers table is empty (first boot)
        let existing = ff_db::pg_list_nodes(pg_pool).await.unwrap_or_default();
        if existing.is_empty() {
            info!("first boot: seeding Postgres from fleet.toml");
            ff_db::seed_from_fleet_toml(pg_pool, &config)
                .await
                .context("failed to seed postgres from fleet.toml")?;
        } else {
            info!(
                nodes = existing.len(),
                "Postgres fleet tables already populated, skipping seed"
            );
        }

        // ─── Port registry seed (config/ports.toml → port_registry) ──
        //
        // Runs on every startup (the seed is idempotent — it UPSERTs).
        // Mirrors how software_registry / model_catalog / task_coverage
        // are seeded when their CLI entry points run; doing it here
        // guarantees the registry is fresh without requiring an operator
        // to remember to run `ff ports seed`.
        let ports_path = ff_agent::ports_registry::resolve_ports_path();
        if ports_path.exists() {
            match ff_agent::ports_registry::seed_from_toml(pg_pool, &ports_path).await {
                Ok(rep) => info!(
                    path = %ports_path.display(),
                    total = rep.total,
                    inserted = rep.inserted,
                    updated = rep.updated,
                    unchanged = rep.unchanged,
                    "port_registry seeded from config/ports.toml"
                ),
                Err(e) => warn!(
                    path = %ports_path.display(),
                    error = %e,
                    "port_registry seed failed (continuing)"
                ),
            }
        } else {
            warn!(
                path = %ports_path.display(),
                "config/ports.toml not found — skipping port_registry seed"
            );
        }
    }

    // ─── Config hot-reload handle ────────────────────────────────────────────
    let (config_handle, config_tx) = ConfigHandle::new(config.clone(), config_path.clone());
    let (_config_shutdown_tx, config_shutdown_rx) = tokio::sync::watch::channel(false);
    let config_watcher = spawn_watcher(config_handle, config_tx, config_shutdown_rx);

    // ─── Control-plane bootstrap ─────────────────────────────────────────────
    let bootstrap_opts = BootstrapOptions {
        require_nodes: false,
        require_models: false,
        ..Default::default()
    };

    let control_plane = ControlPlane::bootstrap(config.clone(), bootstrap_opts)
        .context("failed to bootstrap control plane")?;

    for event in control_plane.startup_events() {
        info!(
            subsystem = ?event.subsystem,
            started_at = %event.started_at,
            completed_at = %event.completed_at,
            "bootstrap step ready"
        );
    }

    for warning in control_plane.startup_warnings() {
        warn!(%warning, "bootstrap warning");
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let mut subsystem_tasks: Vec<JoinHandle<()>> = Vec::new();

    // ─── Pre-seed registry from fleet.toml + Postgres ─────────────────────────
    let registry = control_plane.handles.discovery.registry.clone();
    seed_registry_from_config(&config, &registry);

    // If Postgres is available, also seed from DB (more authoritative)
    if let Some(pg_pool) = operational_store.pg_pool()
        && let Ok(db_nodes) = ff_db::pg_list_nodes(pg_pool).await
    {
        let default_port = config.fleet.api_port;
        for node in &db_nodes {
            if let Ok(ip) = node.ip.parse::<std::net::IpAddr>() {
                let port = default_port;
                let priority = node.election_priority as u32;
                registry.upsert_config_node(&node.name, ip, port, priority);
                info!(
                    node = %node.name,
                    ip = %node.ip,
                    port,
                    priority,
                    "seeded node from Postgres"
                );
            }
        }
    }

    // 0b) Self-sync the methodology fallback block into this node's global TUI
    // configs on boot (roadmap D). Idempotent + best-effort — every node keeps
    // its ~/.claude/CLAUDE.md / ~/.codex/AGENTS.md / ~/.kimi/AGENTS.md in sync
    // without an SSH fan-out, so `ff fleet deploy --all` (which restarts each
    // daemon) propagates the latest methodology fleet-wide.
    match ff_agent::instructions_sync::sync_local() {
        Ok(paths) => info!(
            files = paths.len(),
            "methodology fallback synced to global configs"
        ),
        Err(e) => warn!(error = %e, "methodology self-sync failed (non-fatal)"),
    }

    // 1) discovery — fleet node scanning + subnet scanning
    info!("starting subsystem: discovery");
    let scan_targets = build_fleet_scan_targets(&config).await;
    info!(
        target_count = scan_targets.len(),
        "discovery: resolved scan targets from FleetResolver"
    );
    subsystem_tasks.push(start_discovery_subsystem(
        control_plane.handles.discovery.scanner_config.clone(),
        scan_targets,
        registry.clone(),
        shutdown_rx.clone(),
        Duration::from_secs(config.discovery.subnet_scan_interval_secs),
        config.discovery.subnet_scan_enabled,
    ));

    // 2) leader election — REMOVED 2026-06-24 (deep-review #4). The
    // discovery/TOML election engine (start_leader_election_subsystem) ran
    // concurrently with the authoritative LeaderTick (Postgres
    // fleet_leader_state) and could disagree with it — leaking a stale winner
    // into the gateway's leader_hint. Its only effects were a redundant
    // heartbeat bump (LeaderTick already heartbeats) and that stale hint
    // (which already falls back to the authoritative DB snapshot). LeaderTick
    // is now the single source of truth for leadership fleet-wide.

    // 3) api proxy — build shared backend registry from config + Postgres
    info!("starting subsystem: api proxy");
    let api_config = build_api_config(&config, operational_store.pg_pool()).await;
    let backend_registry = std::sync::Arc::new(BackendRegistry::new(api_config.backends.clone()));
    subsystem_tasks.push(start_api_proxy_subsystem(api_config));

    // 4) agent
    info!("starting subsystem: agent");
    let mut embedded_agent_config = build_embedded_agent_config(&config, worker_name.clone());
    // Wire in the inference router so autonomous LLM tasks use local-first fleet routing.
    let inference_router =
        Arc::new(ff_agent::inference_router::InferenceRouter::from_config(&config_path).await);
    embedded_agent_config.inference_router = Some(inference_router);
    subsystem_tasks.push(start_agent_subsystem(
        embedded_agent_config,
        operational_store.clone(),
        shutdown_rx.clone(),
    ));

    // 5) cron
    info!("starting subsystem: cron");
    let cron_engine = control_plane.handles.scheduler.engine.clone();
    let cron_task = cron_engine.clone().start();

    // 6) gateway — pass shared state
    info!("starting subsystem: gateway");
    subsystem_tasks.push(start_gateway_subsystem(
        config.clone(),
        config_path.to_string_lossy().to_string(),
        backend_registry.clone(),
        registry.clone(),
        operational_store.clone(),
        runtime_registry.clone(),
    ));

    // 6.4) Tool registry auto-prune — removes stale fleet_tools rows every 60s.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        subsystem_tasks.push(start_tool_prune_subsystem(pg_pool, shutdown_rx.clone()));
    }

    // 6.45) Tool registry registration + heartbeat — register local tools
    // with the fleet-wide tool registry and keep them alive.
    {
        let name = worker_name.clone();
        let shutdown = shutdown_rx.clone();
        let tool_registry_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        subsystem_tasks.push(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let gateway = "http://127.0.0.1:51002".to_string();
            // Initial registration
            let tools: Vec<serde_json::Value> = ff_agent::tools::all_tools_arc()
                .iter()
                .map(|tool| {
                    serde_json::json!({
                        "name": tool.name(),
                        "description": tool.description(),
                        "parameters_schema": tool.parameters_schema(),
                        "capabilities_required": [],
                    })
                })
                .collect();
            let register_body = serde_json::json!({
                "worker_name": name,
                "tools": tools,
            });
            match tool_registry_client
                .post(format!("{}/api/tools/register", gateway))
                .json(&register_body)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    info!(count = tools.len(), "fleet tools registered from daemon");
                }
                Ok(resp) => warn!(status = %resp.status(), "tool registration failed"),
                Err(e) => warn!(error = %e, "tool registration request failed"),
            }
            // Heartbeat loop
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            let mut shutdown = shutdown;
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let _ = tool_registry_client.post(format!("{}/api/tools/heartbeat", gateway))
                            .json(&serde_json::json!({"worker_name": name}))
                            .send().await;
                    }
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() { break; }
                    }
                }
            }
        }));
    }

    // 6.5) CLI bridge daemons (Layer 3 of the multi-LLM CLI integration).
    // Per-port (51100-51104) listener that translates OpenAI chat shape
    // to a vendor CLI invocation. Each port is gated on the
    // corresponding binary being on `$PATH`, so a member with no Claude
    // installed simply doesn't open 51100. Bridges are bound to
    // 127.0.0.1 only — never publicly reachable.
    {
        let handles = ff_gateway::cli_bridge::spawn_all_bridges();
        for h in handles {
            subsystem_tasks.push(h);
        }
    }

    // 6.55) Screen-control daemon (Pillar 1 — Computer Use). Listens
    // on 127.0.0.1:51200 and exposes screenshot / click / type / key
    // / goto endpoints by shelling out to platform tools
    // (screencapture / cliclick on macOS, scrot / xdotool on Linux).
    // Endpoints return 503 with install hints when the underlying
    // tool isn't on $PATH.
    subsystem_tasks.push(ff_gateway::screen_ctrl::spawn());

    // 6.6) Brain mirror — watches per-CLI memory dirs (Claude Code,
    // Codex, Gemini) and copies new markdown into the Obsidian vault's
    // `Inbox/<source>/` folder. AI writes only to Inbox per the V13
    // design; operator promotes from there. Runs on every fleet member
    // since each has its own per-CLI state when the user works there
    // directly.
    if let Some(pg_pool) = operational_store.pg_pool() {
        let (_brain_mirror_tx, brain_mirror_rx) = tokio::sync::watch::channel(false);
        let h = ff_agent::brain_mirror::spawn_brain_mirror(pg_pool.clone(), brain_mirror_rx);
        subsystem_tasks.push(h);
    }

    // 6.7) Session orchestrator — V54 outcome-driven multi-LLM sessions.
    // Walks each session's step DAG, dispatches each step's prompt as
    // a fleet_task, reconciles the result back. Runs on every member
    // (the SQL UPDATEs are not yet atomic; concurrent runners may race
    // on dispatch — fine for early-stage; future PR-L follow-up adds
    // SKIP LOCKED on the pending-step claim).
    if let Some(pg_pool) = operational_store.pg_pool() {
        let (_session_runner_tx, session_runner_rx) = tokio::sync::watch::channel(false);
        let h = ff_agent::session_runner::spawn(pg_pool.clone(), session_runner_rx);
        subsystem_tasks.push(h);
    }

    // 7) telegram polling transport (bidirectional control channel).
    // Gate on worker_name == "taylor": Telegram only allows a single
    // long-poll holder per bot token. Without this gate, 15 fleet
    // daemons race for getUpdates() and each gets 409 Conflict every
    // ~5s; nothing usable comes through. Taylor (leader) holds the
    // session; other daemons skip the subsystem entirely.
    let telegram_owner =
        std::env::var("FORGEFLEET_TELEGRAM_OWNER").unwrap_or_else(|_| "taylor".to_string());
    if worker_name == telegram_owner
        && config
            .transport
            .telegram
            .as_ref()
            .is_some_and(|telegram| telegram.enabled)
    {
        // Fallback: if the token env var / inline config is empty, pull the
        // bot token from fleet_secrets (`telegram.bot_token`) and export it
        // via the configured env var so resolve_bot_token() finds it. Keeps
        // secrets out of shell rc files and launchd plists.
        if let Some(tg) = config.transport.telegram.as_ref()
            && tg.resolve_bot_token().is_none()
            && let Some(pg_pool) = operational_store.pg_pool()
        {
            // Canonical key is `openclaw.telegram_bot_token` (what `ff secrets
            // set` writes and what the gateway reads); `telegram.bot_token` is
            // tried second for back-compat. Using the wrong key here meant the
            // fleet_secrets path silently failed, forcing the token to live in
            // the launchd plist as a plaintext env var — defeating the whole
            // "secrets out of plists" point of this block.
            let mut loaded = false;
            for secret_key in ["openclaw.telegram_bot_token", "telegram.bot_token"] {
                match ff_db::pg_get_secret(pg_pool, secret_key).await {
                    Ok(Some(token)) if !token.trim().is_empty() => {
                        let key = if tg.bot_token_env.trim().is_empty() {
                            "FORGEFLEET_TELEGRAM_BOT_TOKEN"
                        } else {
                            tg.bot_token_env.as_str()
                        };
                        unsafe {
                            std::env::set_var(key, token.trim());
                        }
                        info!(secret_key, "telegram bot token loaded from fleet_secrets");
                        loaded = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => error!(error = %e, secret_key, "fleet_secrets lookup failed"),
                }
            }
            if !loaded {
                info!(
                    "telegram bot token absent in fleet_secrets (tried openclaw.telegram_bot_token, telegram.bot_token)"
                );
            }
        }
        info!("starting subsystem: telegram transport");
        subsystem_tasks.push(start_telegram_transport_subsystem(
            config.clone(),
            operational_store.clone(),
            worker_name.clone(),
            shutdown_rx.clone(),
        ));
    } else {
        info!("subsystem disabled: telegram transport");
    }

    // 8) evolution runtime loop
    if config.loops.evolution.enabled {
        info!("starting subsystem: evolution loop");
        subsystem_tasks.push(start_evolution_subsystem(
            config.clone(),
            registry.clone(),
            operational_store.pg_pool().cloned(),
            shutdown_rx.clone(),
        ));
    } else {
        info!("subsystem disabled: evolution loop");
    }

    // 9) updater runtime loop
    if config.loops.updater.enabled {
        info!("starting subsystem: updater loop");
        subsystem_tasks.push(start_updater_subsystem(
            config.clone(),
            worker_name.clone(),
            shutdown_rx.clone(),
        ));
    } else {
        info!("subsystem disabled: updater loop");
    }

    // 10) runtime self-heal loop
    if config.loops.self_heal.enabled {
        info!("starting subsystem: self-heal loop");
        subsystem_tasks.push(start_self_heal_subsystem(
            config.clone(),
            worker_name.clone(),
            operational_store.clone(),
            shutdown_rx.clone(),
        ));
    } else {
        info!("subsystem disabled: self-heal loop");
    }

    // 11) MCP federation + topology validation loop
    if config.loops.mcp_federation.enabled {
        info!("starting subsystem: mcp federation loop");
        subsystem_tasks.push(start_mcp_federation_subsystem(
            config.clone(),
            shutdown_rx.clone(),
        ));
    } else {
        info!("subsystem disabled: mcp federation loop");
    }

    // 12) MCP HTTP server
    info!("starting subsystem: mcp http server");
    subsystem_tasks.push(start_mcp_http_subsystem(config.clone()));

    // 13) Fleet Pulse heartbeat (Redis real-time metrics)
    {
        let redis_url = config.redis.url.clone();
        let pulse_node = worker_name.clone();
        let pulse_shutdown = shutdown_rx.clone();
        if !redis_url.is_empty() {
            info!("starting subsystem: fleet pulse heartbeat");
            subsystem_tasks.push(tokio::spawn(async move {
                match ff_pulse::PulseClient::connect(&redis_url).await {
                    Ok(client) => {
                        let publisher = ff_pulse::HeartbeatPublisher::new(
                            client,
                            pulse_node,
                            Duration::from_secs(15),
                        );
                        let _ = publisher.start(pulse_shutdown).await;
                    }
                    Err(e) => {
                        warn!(error = %e, "fleet pulse: Redis connection failed, heartbeat disabled");
                    }
                }
            }));
        } else {
            info!("subsystem disabled: fleet pulse (no redis.url configured)");
        }
    }

    // 14) Pulse v2 — heartbeat_v2 + materializer + leader_tick
    //
    // Preconditions: Postgres pool (for computer_id / fleet_members lookup)
    // and a non-empty redis URL. If the computer isn't yet enrolled in the
    // `computers` table we log and skip v2 entirely — new hosts come up
    // clean without errors, enrollment backfills them later.
    //
    // NOTE (leader-gating): the materializer writes ALL computers' rows, so
    // it is correct only on the leader. For this phase we still spawn it on
    // every daemon; it reads all beats but the DB writes from non-leaders
    // are acceptable as idempotent no-ops against the delta-check snapshot.
    // A future pass will tie spawn/stop to leader_tick's on_became_leader /
    // on_lost_leader callbacks — until then, keeping it always-on is the
    // simpler of the two alternatives the design notes considered.
    if !start.disable_pulse_v2 {
        let redis_url = config.redis.url.clone();
        if redis_url.is_empty() {
            info!("subsystem disabled: pulse v2 (no redis.url configured)");
        } else if let Some(pg_pool) = operational_store.pg_pool().cloned() {
            match start_pulse_v2_subsystems(
                pg_pool,
                redis_url,
                worker_name.clone(),
                shutdown_rx.clone(),
            )
            .await
            {
                Ok(handles) => {
                    subsystem_tasks.extend(handles);
                }
                Err(err) => {
                    warn!(error = %err, "pulse v2 startup failed; v1 heartbeat continues");
                }
            }
        } else {
            info!("subsystem disabled: pulse v2 (requires postgres_runtime or postgres_full mode)");
        }
    } else {
        info!("subsystem disabled: pulse v2 (--disable-pulse-v2)");
    }

    // 15) Project scheduler tick — every 60s, leader-gated.
    // Phase 10: evaluates cron expressions in project_schedules and enqueues fleet_tasks.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!("starting subsystem: project scheduler tick (60s, leader-gated)");
        subsystem_tasks.push(ff_agent::scheduler_tick::spawn_scheduler_tick(
            pg_pool,
            worker_name.clone(),
            60,
            shutdown_rx.clone(),
        ));
    }

    // 15b) Pillar 4 work_item scheduler — every 10s, leader-gated.
    // Assigns status='ready' work_items to free fleet slots via work_item_leases
    // (reaps stale leases first). Only touches execution-flagged items.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!("starting subsystem: work_item scheduler (10s, leader-gated)");
        subsystem_tasks.push(ff_agent::work_item_scheduler::spawn_work_item_scheduler(
            pg_pool,
            worker_name.clone(),
            10,
            shutdown_rx.clone(),
        ));
    }

    // 15b.1) Pillar 4 lease takeover — every 60s, leader-gated.
    // Reclaims active work_item leases whose builder host stopped heartbeating,
    // freeing the slot and returning the item to ready for another fleet slot.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!("starting subsystem: work_item lease takeover (60s, leader-gated)");
        subsystem_tasks.push(ff_agent::lease_takeover::spawn_lease_takeover(
            pg_pool,
            worker_name.clone(),
            60,
            shutdown_rx.clone(),
        ));
    }

    // 15c) Pillar 4 work_item dispatch — every 15s, PER-HOST (not leader-gated).
    // Each host executes ITS OWN assigned slots: detect current_work_item_id →
    // git worktree → ff cli dispatch → heartbeat lease → PR + merge-queue on done.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!("starting subsystem: work_item dispatch (15s, per-host)");
        subsystem_tasks.push(ff_agent::work_item_dispatch::spawn_work_item_dispatch(
            pg_pool,
            worker_name.clone(),
            15,
            shutdown_rx.clone(),
        ));
    }

    // 15d) Pillar 4 merge-queue drain — every 30s, leader-gated.
    // Closes the loop: drains work_item_merge_queue one PR/project at a time,
    // waits its CI green, `gh pr merge --squash --delete-branch`, marks merged.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!("starting subsystem: work_item merge drain (30s, leader-gated)");
        subsystem_tasks.push(
            ff_agent::work_item_merge_drain::spawn_work_item_merge_drain(
                pg_pool,
                worker_name.clone(),
                30,
                shutdown_rx.clone(),
            ),
        );
    }

    // 15e) Pillar 4 worktree reaper — every 5min, PER-HOST.
    // Removes on-disk git worktrees whose work_item is terminal (cancelled/
    // merged/failed/done) and marks them cleaned. Each host reaps its own.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!("starting subsystem: work_item worktree reaper (5min, per-host)");
        subsystem_tasks.push(ff_agent::work_item_dispatch::spawn_worktree_reaper(
            pg_pool,
            worker_name.clone(),
            300,
            shutdown_rx.clone(),
        ));
    }

    // 15f) Backend availability detector — every 1h, PER-HOST (capability A2).
    // Each host probes which LLM-CLI backends (claude/codex/kimi/gemini/grok) are
    // installed AND authenticated locally and upserts computer_backends, so the
    // dispatch picker can route a build to a node+backend that actually works.
    // Hourly (not 15s): auth probes are real CLI invocations; dispatch re-probes
    // when a cached row is stale.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!("starting subsystem: backend availability detector (1h, per-host)");
        let detector_worker = worker_name.clone();
        subsystem_tasks.push(ff_agent::tick_registry::TickRegistry::register(
            "backend-detector",
            std::time::Duration::from_secs(3600),
            shutdown_rx.clone(),
            move |_run| {
                let pg_pool = pg_pool.clone();
                let detector_worker = detector_worker.clone();
                async move {
                    ff_agent::backend_detect::run_backend_detector_tick(&pg_pool, &detector_worker)
                        .await;
                }
            },
        ));
    }

    // 16) Procedural memory consolidation — every 6h, leader-gated.
    // Phase 14: scans completed sessions, extracts successful patterns into agent_procedures.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!("starting subsystem: procedural memory consolidation (6h, leader-gated)");
        subsystem_tasks.push(ff_brain::procedural_memory::spawn_consolidation_loop(
            pg_pool,
            worker_name.clone(),
            6 * 3600,
            shutdown_rx.clone(),
        ));
    }

    // 16a) Cortex reindex — every hour, leader-gated.
    // The embed (16b) + summary (16c) ticks maintain metadata over already-
    // indexed nodes but NEVER re-parse changed source, so the graph structure
    // silently drifts from HEAD once nobody runs `ff cortex index` by hand
    // (observed 2026-06-19: the forge-fleet corpus was 4 days stale and
    // `cortex_find fleet_oneshot` returned 0 hits). This tick re-scans +
    // incrementally re-indexes the self corpus (hash-diffed — unchanged files
    // skipped, cheap); 16b then embeds the freshly-indexed nodes. Pure graph
    // maintenance — runs by DEFAULT; opt out with
    // `fleet_secrets.cortex_index_mode=off`.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!(
            "starting subsystem: cortex reindex tick (hourly, leader-gated, gate=fleet_secrets.cortex_index_mode default on)"
        );
        subsystem_tasks.push(ff_brain::spawn_reindex_loop(
            pg_pool,
            worker_name.clone(),
            3600,
            shutdown_rx.clone(),
        ));
    }

    // 16b) Cortex embed-refresh — every hour, leader-gated.
    // A freshly-(re)indexed code symbol lands with a NULL `embedding`, so
    // `ff cortex find --semantic` goes stale on just-changed code until someone
    // manually runs `ff cortex embed`. This tick drains the unembedded backlog
    // automatically (bounded per pass, resumable, bails when no bge-m3 endpoint
    // is live). Pure maintenance over the `embedding` column — no serving state
    // is mutated — so it runs by DEFAULT; opt out with
    // `fleet_secrets.cortex_embed_mode=off`.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!(
            "starting subsystem: cortex embed-refresh tick (hourly, leader-gated, gate=fleet_secrets.cortex_embed_mode default on)"
        );
        subsystem_tasks.push(ff_brain::spawn_embed_refresh_loop(
            pg_pool,
            worker_name.clone(),
            3600,
            shutdown_rx.clone(),
        ));
    }

    // 16c) Cortex community-summary refresh — every hour, leader-gated.
    // The embed tick (16b) keeps embeddings fresh but never re-detects
    // communities or fills their summaries; once it removed the reason to run
    // `ff cortex embed` by hand, community detection lost its only trigger, so
    // clusters + their natural-language summaries silently went stale (5,406 of
    // 5,414 communities had no summary). This tick re-detects communities at
    // HEAD (cheap, idempotent w.r.t. summaries via the stable member_hash) and
    // drains the un-summarized backlog via a warm fleet LLM (bounded per pass,
    // biggest-first, bails when no tool-capable endpoint is live). Pure
    // maintenance over the brain_communities / community_id graph metadata — no
    // serving state is mutated — so it runs by DEFAULT; opt out with
    // `fleet_secrets.cortex_summary_mode=off`.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!(
            "starting subsystem: cortex community-summary refresh tick (hourly, leader-gated, gate=fleet_secrets.cortex_summary_mode default on)"
        );
        subsystem_tasks.push(ff_brain::spawn_summary_refresh_loop(
            pg_pool,
            worker_name.clone(),
            3600,
            shutdown_rx.clone(),
        ));
    }

    // 17) Deferred-task worker — claim + execute tasks from `deferred_tasks`.
    // Historically lived in the separate `ff daemon` CLI; the split caused
    // dispatched tasks to pile up forever when nobody started that process.
    // Folding the worker into forgefleetd eliminates that gap. See module
    // doc on ff_agent::defer_worker for scope (handles shell/http/upgrade;
    // other kinds remain `ff daemon`'s domain).
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!("starting subsystem: deferred-task worker (10s poll, 4 concurrent)");
        subsystem_tasks.push(ff_agent::defer_worker::spawn_defer_worker(
            pg_pool,
            worker_name.clone(),
            10,
            4,
            shutdown_rx.clone(),
        ));
    }

    // 18) Demand sensor tick — every 30s, leader-gated.
    // Orchestrator P2: rolls the per-session work-kind signals into a
    // fleet-wide demand vector and snapshots it into `fleet_demand_snapshot`
    // (the contract P3's adaptive serving-mix autoscaler consumes). Produces
    // the demand signal only — never loads/unloads a model.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!("starting subsystem: demand sensor tick (30s, leader-gated)");
        subsystem_tasks.push(ff_agent::demand_sensor::spawn_demand_tick(
            pg_pool,
            worker_name.clone(),
            30,
            shutdown_rx.clone(),
        ));
    }

    // 18b) Deployment-staleness tick — every 60s, leader-gated.
    // Write-side companion to #369: that fix RENDERS `stale` in `ff model
    // deployments` for offline-owned rows, but the stored health_status stays
    // `healthy` because only the owning node's reconciler writes it and a dead
    // node never runs it. This leader tick marks `health_status='stale'` in the
    // DB for active deployments whose owning computer has been silent > 5min, so
    // the router/MCP/coverage (which read the column directly) stop seeing a
    // dead endpoint as serving. Self-correcting: the node's reconciler reclaims
    // the row on return. Never masks on a global signal loss (skips if the
    // materializer looks dead).
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!("starting subsystem: deployment-staleness tick (60s, leader-gated)");
        subsystem_tasks.push(
            ff_agent::deployment_staleness::spawn_deployment_staleness_tick(
                pg_pool,
                worker_name.clone(),
                60,
                shutdown_rx.clone(),
            ),
        );
    }

    // 18c) Fleet-integrity verify tick — every 15min, leader-gated, gate
    // `fleet_secrets.fleet_integrity_mode` (off|report, DEFAULT off).
    // `revive_scan` already repairs DEAD nodes; this covers the blind spot of an
    // ALIVE-but-misconfigured member (half-configured enrollment / config drift)
    // by running the `verify_computer` battery across all online members on a
    // schedule and firing `fleet_integrity_degraded` on drift. Detection-only
    // (never mutates a target); per-gap auto-repair is a tracked follow-up.
    // Closes PROD_READINESS item 23 (enrollment self-heal — detection half).
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!(
            "starting subsystem: fleet-integrity tick (15min, leader-gated, gate=fleet_secrets.fleet_integrity_mode default off)"
        );
        subsystem_tasks.push(ff_agent::fleet_integrity::spawn_fleet_integrity_tick(
            pg_pool,
            worker_name.clone(),
            900,
            shutdown_rx.clone(),
        ));
    }

    // 18b) Staged upgrade rollout + auto-halt — every 60s, leader-gated.
    // PROD_READINESS item 26: a bad build must be caught on a canary host
    // instead of rolling all 14 non-leader hosts. For each in_progress
    // `upgrade_rollouts` row the tick gates stage progression on the current
    // stage's fleet_tasks all reaching a terminal state, halts (+ alerts) when a
    // stage's failure rate crosses its threshold (canary halts on the first
    // failure), and otherwise composes ONLY the next stage's targets — never
    // more than one stage in flight (preserves the V62 wave singleton). DEFAULTS
    // TO OFF: with `fleet_secrets.staged_rollout_mode` at `off`/unset the tick is
    // a pure no-op; `dry-run` logs the decision; `active` actuates.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!(
            "starting subsystem: staged-rollout tick (60s, leader-gated, gate=fleet_secrets.staged_rollout_mode default off)"
        );
        subsystem_tasks.push(ff_agent::upgrade_rollout::spawn_upgrade_rollout_tick(
            pg_pool,
            worker_name.clone(),
            60,
            shutdown_rx.clone(),
        ));
    }

    // 19) Adaptive serving-mix autoscaler — every 120s, leader-gated.
    // Orchestrator P3: compares the P2 demand vector against live supply and,
    // when the `fleet_secrets.autoscaler_mode` gate is set to `active`, loads
    // or unloads models so the deployed mix follows demand (placement-scored
    // across the fleet, hysteresis-gated). DEFAULTS TO OFF: with the gate at
    // `off` (or unset) the tick is a pure no-op, so deploying this is harmless.
    // `dry-run` logs the plan without actuating; `active` actuates.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!(
            "starting subsystem: serving-mix autoscaler tick (120s, leader-gated, gate=fleet_secrets.autoscaler_mode default off)"
        );
        subsystem_tasks.push(ff_agent::autoscaler::spawn_autoscaler_tick(
            pg_pool,
            worker_name.clone(),
            120,
            shutdown_rx.clone(),
        ));
    }

    // 19b) Fleet task liveness watchdog — every 60s, PER-NODE.
    //
    // Restores the legacy-only actuating liveness loop from `ff daemon`: each
    // node probes and evaluates only the tasks currently running on itself.
    // Dead/stuck tasks are audited, fed into the host circuit breaker, and may
    // notify the operator according to the failure taxonomy policy.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!("starting subsystem: fleet task liveness watchdog (60s, per-node)");
        let name = worker_name.clone();
        let mut shutdown_rx_liveness = shutdown_rx.clone();
        subsystem_tasks.push(tokio::spawn(async move {
            #[derive(sqlx::FromRow)]
            struct RunningTaskRow {
                id: uuid::Uuid,
                kind: String,
            }

            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx_liveness.changed() => break,
                    _ = tick.tick() => {
                        match ff_agent::task_probe::probe_all_running(&pg_pool, &name).await {
                            Ok(n) if n > 0 => tracing::debug!(
                                node = %name,
                                probed = n,
                                "fleet task liveness probes written"
                            ),
                            Ok(_) => {}
                            Err(e) => warn!(node = %name, error = %e, "fleet task liveness probe failed"),
                        }

                        let running: std::result::Result<Vec<RunningTaskRow>, _> = sqlx::query_as(
                            "SELECT t.id, t.kind
                               FROM fleet_tasks t
                               JOIN computers c ON c.id = t.claimed_by_computer_id
                              WHERE c.name = $1
                                AND t.status = 'running'",
                        )
                        .bind(&name)
                        .fetch_all(&pg_pool)
                        .await;

                        let rows = match running {
                            Ok(rows) => rows,
                            Err(e) => {
                                warn!(node = %name, error = %e, "fleet task liveness query failed");
                                continue;
                            }
                        };

                        for row in rows {
                            let state = match ff_agent::watchdog::evaluate_task(&pg_pool, row.id, 600).await {
                                Ok(state) => state,
                                Err(e) => {
                                    warn!(
                                        task_id = %row.id,
                                        node = %name,
                                        error = %e,
                                        "fleet task liveness evaluation failed"
                                    );
                                    continue;
                                }
                            };

                            let category = match state {
                                ff_agent::watchdog::TaskLiveness::Dead => "dead_zombie",
                                ff_agent::watchdog::TaskLiveness::Stuck => "genuinely_stuck",
                                _ => continue,
                            };

                            warn!(
                                task_id = %row.id,
                                node = %name,
                                kind = %row.kind,
                                category,
                                "fleet task liveness watchdog classified task as unhealthy"
                            );

                            if let Err(e) = sqlx::query(
                                "INSERT INTO task_failures (task_id, category, attempt, action_taken)
                                 VALUES ($1, $2, 0, 'liveness_kill')",
                            )
                            .bind(row.id)
                            .bind(category)
                            .execute(&pg_pool)
                            .await
                            {
                                warn!(
                                    task_id = %row.id,
                                    node = %name,
                                    category,
                                    error = %e,
                                    "fleet task liveness watchdog failed to record task failure"
                                );
                            }

                            let tripped = ff_agent::circuit_breaker::record_failure(
                                &pg_pool,
                                &name,
                                category,
                            )
                            .await
                            .unwrap_or(false);
                            if tripped {
                                warn!(
                                    node = %name,
                                    category,
                                    "fleet task liveness watchdog tripped host circuit breaker"
                                );
                            }

                            let should_notify =
                                ff_agent::notification::should_notify(&pg_pool, &name, category)
                                    .await
                                    .unwrap_or(false);
                            if should_notify {
                                let _ = ff_agent::telegram::send_telegram_from_secrets(
                                    &pg_pool,
                                    &format!("ff liveness: {category}"),
                                    &format!(
                                        "Task {} on {} marked {category}. Circuit breaker {}.",
                                        row.id,
                                        name,
                                        if tripped { "TRIPPED" } else { "still closed" },
                                    ),
                                )
                                .await;
                                let _ = ff_agent::notification::record_notification(
                                    &pg_pool,
                                    Some(row.id),
                                    category,
                                    serde_json::json!({
                                        "worker": &name,
                                        "circuit_tripped": tripped,
                                    }),
                                )
                                .await;
                            }
                        }
                    }
                }
            }
        }));
    }

    // 20) Disk sampler tick — every 5min, PER-NODE (not leader-gated).
    //
    // Historically the disk sampler ran ONLY in the legacy `ff daemon`
    // (crates/ff-terminal/src/daemon_cmd.rs) — which production hosts running
    // `forgefleetd` never start. Result: `fleet_disk_usage` was stale on every
    // forgefleetd-only host (7 of 13 nodes hadn't sampled since 2026-05-13).
    // Each node samples ITS OWN disk, so this is per-node like the deployment
    // reconciler — NOT leader-gated. Without fresh samples the quota check, the
    // over-quota alert, and the V118 disk-reconcile tick all run on stale data.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!("starting subsystem: disk sampler tick (5min, per-node)");
        // Fire once promptly so a freshly-(re)started daemon writes a row
        // immediately instead of waiting a full 5min — this is what makes a
        // previously-stale host show a FRESH fleet_disk_usage row right after
        // deploy.
        subsystem_tasks.push(ff_agent::tick_registry::TickRegistry::register(
            "disk-sampler",
            std::time::Duration::from_secs(300),
            shutdown_rx.clone(),
            move |_run| {
                let pg_pool = pg_pool.clone();
                async move {
                    match ff_agent::disk_sampler::sample_local_disk(&pg_pool).await {
                        Ok(s) => tracing::debug!(
                            node = %s.worker_name,
                            used_mb = s.used_bytes / 1_048_576,
                            free_mb = s.free_bytes / 1_048_576,
                            over_quota = s.over_quota,
                            "disk sample written"
                        ),
                        Err(e) => warn!(error = %e, "disk sampler failed"),
                    }
                }
            },
        ));
    }

    // 20b) Version-check tick — every 6h, PER-NODE (not leader-gated).
    //
    // Same legacy-daemon gap as the disk sampler above: `version_check_pass`
    // (which writes THIS host's `fleet_workers.tooling` = installed-vs-latest
    // tool versions) ran ONLY in `ff daemon`, so every forgefleetd-only host had
    // stale or empty tooling — beyonce/rihanna (never ran the legacy daemon)
    // persistently failed the `tool_versions_reported` integrity check. Each
    // node reports its OWN tooling, so this is per-node. The first tick fires
    // promptly so a freshly-(re)started daemon populates tooling within ~a
    // minute instead of waiting 6h. (Pairs with the resolve_own_bin fix so the
    // collector finds ff/forgefleetd even when PATH omits ~/.local/bin.)
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!("starting subsystem: version-check tick (6h, per-node)");
        let mut shutdown_rx_ver = shutdown_rx.clone();
        let ver_worker = worker_name.clone();
        subsystem_tasks.push(tokio::spawn(async move {
            // Settle briefly after startup, then run an initial pass + every 6h.
            tokio::time::sleep(std::time::Duration::from_secs(45)).await;
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(6 * 3600));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx_ver.changed() => break,
                    _ = tick.tick() => {
                        match ff_agent::version_check::version_check_pass(&pg_pool).await {
                            Ok(s) => info!(
                                node = %ver_worker,
                                tools = s.total_keys,
                                drift = s.drifted_keys.len(),
                                "version-check pass complete"
                            ),
                            Err(e) => warn!(node = %ver_worker, error = %e, "version-check pass failed"),
                        }
                        // Self-built LATEST refresh — UN-GATED (version CHECK is
                        // not the auto-UPGRADE; otherwise `ff fleet versions`
                        // shows a frozen phantom LATEST whenever auto-upgrade is
                        // paused). Leader-only: it git-resolves the leader's tree.
                        let is_leader: bool = sqlx::query_scalar::<_, bool>(
                            "SELECT EXISTS(SELECT 1 FROM fleet_leader_state \
                              WHERE member_name = $1 \
                                AND heartbeat_at > NOW() - INTERVAL '60 seconds')",
                        )
                        .bind(&ver_worker)
                        .fetch_one(&pg_pool)
                        .await
                        .unwrap_or(false);
                        if is_leader {
                            match ff_agent::auto_upgrade::refresh_self_built_latest_versions(
                                &pg_pool,
                            )
                            .await
                            {
                                Ok(n) => info!(updated = n, "self-built LATEST refreshed (un-gated)"),
                                Err(e) => warn!(error = %e, "self-built LATEST refresh failed"),
                            }
                        }
                    }
                }
            }
        }));
    }

    // 20b1) DSN-of-record cache-mirror tick — per-node, ONLY when the operator
    // opted into HA Phase 3 DSN failover (`config.database.dsn_failover`). While
    // Postgres is reachable, mirror the current DSN of record into this node's
    // local cache (`~/.forgefleet/db_dsn_of_record`) so that — when the primary
    // later MOVES and the static DSN goes dead — `create_pool_with_dsn_failover`
    // (already wired at startup) has a last-known-good address to reconnect to.
    // Not spawned at all by default (dsn_failover=false), so it is fully inert
    // until an operator enables Phase 3. The connect-path read is itself fail-safe.
    if config.database.dsn_failover
        && let Some(pg_pool) = operational_store.pg_pool().cloned()
    {
        info!(
            "starting subsystem: dsn-of-record cache-mirror tick (5min, per-node, dsn_failover opted in)"
        );
        let mut shutdown_rx_dsn = shutdown_rx.clone();
        subsystem_tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(300));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx_dsn.changed() => break,
                    _ = tick.tick() => {
                        match ff_db::dsn_of_record::read_dsn_of_record(&pg_pool).await {
                            Ok(Some(dsn)) => ff_core::db::write_dsn_cache(&dsn),
                            Ok(None) => {}
                            Err(e) => warn!(error = %e, "dsn-of-record cache-mirror read failed"),
                        }
                    }
                }
            }
        }));
    }

    // 20b2) Mesh-refresh tick — every 6h, leader-gated. Re-probes SSH-mesh pairs
    // whose stored status is stale so `fleet_ssh_mesh` reflects reality and the
    // integrity `mesh_ssh_complete` check stops reporting a FALSE failure forever
    // after a node was briefly unreachable mid-deploy. Same legacy-only gap as
    // the version-check tick above (#396): mesh probing ran only on-demand / in
    // `ff daemon`, never in forgefleetd, so a stale "failed" pair (sia↔adele,
    // while SSH worked by IP) persisted indefinitely.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!("starting subsystem: mesh-refresh tick (6h, leader-gated)");
        subsystem_tasks.push(ff_agent::mesh_check::spawn_mesh_refresh_tick(
            pg_pool,
            worker_name.clone(),
            6 * 3600,
            12,
            shutdown_rx.clone(),
        ));
    }

    // 20b3) SSH mesh auto-repair tick — every 10min, leader-gated.
    //
    // Restores the legacy-only repair dispatcher. The leader finds failed mesh
    // pairs with repeated failures and enqueues the same repair command an
    // operator would run by hand.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!("starting subsystem: ssh mesh auto-repair tick (10min, leader-gated)");
        let name = worker_name.clone();
        let mut shutdown_rx_mesh_repair = shutdown_rx.clone();
        subsystem_tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(10 * 60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx_mesh_repair.changed() => break,
                    _ = tick.tick() => {
                        let is_leader: bool = sqlx::query_scalar(
                            r#"
                            SELECT EXISTS (
                                SELECT 1 FROM fleet_leader_state
                                WHERE member_name = $1
                                  AND heartbeat_at > NOW() - INTERVAL '60 seconds'
                            )
                            "#,
                        )
                        .bind(&name)
                        .fetch_one(&pg_pool)
                        .await
                        .unwrap_or(false);
                        if !is_leader {
                            continue;
                        }

                        let bad: std::result::Result<Option<(String, String, i32)>, _> = sqlx::query_as(
                            "SELECT src_node, dst_node, attempts
                               FROM fleet_mesh_status
                              WHERE status = 'failed'
                                AND attempts >= 3
                              ORDER BY attempts DESC, last_checked ASC NULLS FIRST
                              LIMIT 1",
                        )
                        .fetch_optional(&pg_pool)
                        .await;

                        match bad {
                            Ok(Some((src, dst, attempts))) => {
                                info!(
                                    src = %src,
                                    dst = %dst,
                                    attempts,
                                    "dispatching ssh mesh auto-repair"
                                );
                                let command = format!(
                                    "ff fleet ssh-mesh-check --node {} --repair --yes 2>&1 | tail -10",
                                    dst
                                );
                                if let Err(e) = ff_agent::task_runner::pg_enqueue_shell_task(
                                    &pg_pool,
                                    &format!("auto-mesh-repair: {} -> {}", src, dst),
                                    &command,
                                    &["ff".to_string()],
                                    Some(&name),
                                    None,
                                    50,
                                    None,
                                )
                                .await
                                {
                                    warn!(
                                        src = %src,
                                        dst = %dst,
                                        error = %e,
                                        "failed to enqueue ssh mesh auto-repair task"
                                    );
                                }
                                let _ = ff_agent::telegram::send_telegram_from_secrets(
                                    &pg_pool,
                                    "SSH mesh auto-repair",
                                    &format!("Repair dispatched: {} -> {} (attempts={})", src, dst, attempts),
                                )
                                .await;
                            }
                            Ok(None) => {}
                            Err(e) => warn!(error = %e, "ssh mesh auto-repair query failed"),
                        }
                    }
                }
            }
        }));
    }

    // 20b4) Model library scan tick — every 10min, PER-NODE.
    //
    // Restores the legacy-only scan of this node's ~/models directory so
    // fleet_model_library follows local disk reality without operator action.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!("starting subsystem: model library scan tick (10min, per-node)");
        let name = worker_name.clone();
        let mut shutdown_rx_model_scan = shutdown_rx.clone();
        subsystem_tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(10 * 60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx_model_scan.changed() => break,
                    _ = tick.tick() => {
                        let Some(home) = std::env::var_os("HOME") else {
                            warn!(node = %name, "model library scan skipped: HOME is not set");
                            continue;
                        };
                        let models_dir = std::path::PathBuf::from(home).join("models");
                        if !models_dir.exists() {
                            continue;
                        }

                        match ff_agent::model_library_scanner::scan_local_library(
                            &pg_pool,
                            &name,
                            &models_dir,
                        )
                        .await
                        {
                            Ok(summary) if summary.added + summary.updated + summary.removed > 0 => {
                                info!(
                                    node = %name,
                                    added = summary.added,
                                    updated = summary.updated,
                                    removed = summary.removed,
                                    total_mb = summary.total_bytes / 1_048_576,
                                    "model library scan reconciled local models"
                                );
                            }
                            Ok(_) => {}
                            Err(e) => warn!(node = %name, error = %e, "model library scan failed"),
                        }
                    }
                }
            }
        }));
    }

    // 20b5) Model auto-upgrade download tick — every 6h, leader-gated.
    //
    // Restores the legacy-only download dispatcher for cold models whose
    // upstream revision is available. Active deployments are left alone; the
    // leader enqueues bounded force-download tasks for eligible hosts.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!("starting subsystem: model auto-upgrade download tick (6h, leader-gated)");
        let name = worker_name.clone();
        let mut shutdown_rx_model_upgrade = shutdown_rx.clone();
        subsystem_tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(6 * 3600));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx_model_upgrade.changed() => break,
                    _ = tick.tick() => {
                        let is_leader: bool = sqlx::query_scalar(
                            r#"
                            SELECT EXISTS (
                                SELECT 1 FROM fleet_leader_state
                                WHERE member_name = $1
                                  AND heartbeat_at > NOW() - INTERVAL '60 seconds'
                            )
                            "#,
                        )
                        .bind(&name)
                        .fetch_one(&pg_pool)
                        .await
                        .unwrap_or(false);
                        if !is_leader {
                            continue;
                        }

                        let rows = sqlx::query(
                            r#"
                            SELECT c.name AS host, cm.model_id, mc.upstream_latest_rev
                              FROM computer_models cm
                              JOIN computers c      ON c.id = cm.computer_id
                              JOIN model_catalog mc ON mc.id = cm.model_id
                             WHERE cm.status = 'revision_available'
                               AND NOT EXISTS (
                                 SELECT 1
                                   FROM fleet_model_deployments dep
                                   JOIN fleet_model_library lib ON lib.id = dep.library_id
                                  WHERE lib.catalog_id = cm.model_id
                                    AND dep.desired_state = 'active'
                               )
                             LIMIT 3
                            "#,
                        )
                        .fetch_all(&pg_pool)
                        .await;

                        match rows {
                            Ok(rows) => {
                                for row in rows {
                                    use sqlx::Row;

                                    let host: String = row.get("host");
                                    let model_id: String = row.get("model_id");
                                    let revision: Option<String> = row.get("upstream_latest_rev");
                                    let revision_short = revision
                                        .as_deref()
                                        .map(|s| s.chars().take(10).collect::<String>())
                                        .unwrap_or_default();

                                    let _ = sqlx::query(
                                        "UPDATE computer_models cm
                                            SET status = 'upgrading'
                                          FROM computers c
                                         WHERE cm.computer_id = c.id
                                           AND c.name = $1
                                           AND cm.model_id = $2",
                                    )
                                    .bind(&host)
                                    .bind(&model_id)
                                    .execute(&pg_pool)
                                    .await;

                                    let command = format!(
                                        "ff model download {} --force --node {}",
                                        model_id, host
                                    );
                                    if let Err(e) = ff_agent::task_runner::pg_enqueue_shell_task(
                                        &pg_pool,
                                        &format!(
                                            "model-auto-upgrade: {} on {} -> rev {}",
                                            model_id, host, revision_short
                                        ),
                                        &command,
                                        &["ff".to_string()],
                                        Some(&host),
                                        None,
                                        65,
                                        None,
                                    )
                                    .await
                                    {
                                        warn!(
                                            model_id = %model_id,
                                            host = %host,
                                            error = %e,
                                            "failed to enqueue model auto-upgrade download task"
                                        );
                                    }
                                    let _ = ff_agent::telegram::send_telegram_from_secrets(
                                        &pg_pool,
                                        "Model auto-upgrade",
                                        &format!(
                                            "Re-downloading {} on {} (HF rev {})",
                                            model_id, host, revision_short
                                        ),
                                    )
                                    .await;
                                }
                            }
                            Err(e) => warn!(error = %e, "model auto-upgrade download query failed"),
                        }
                    }
                }
            }
        }));
    }

    // 20b6) Vault re-index tick — every 30min. Leader-gating happens inside
    // the tick (live `fleet_leader_state` check), so spawn unconditionally —
    // every node runs the loop but only the live leader does the work.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!("starting subsystem: vault re-index tick (30min, leader-gated)");
        subsystem_tasks.push(ff_brain::spawn_vault_index_tick(
            pg_pool,
            worker_name.clone(),
            30 * 60,
            shutdown_rx.clone(),
        ));
    }

    // 20b7) GitHub project sync tick — every 5min, leader-gated inside the tick.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!("starting subsystem: github project sync tick (5min, leader-gated)");
        subsystem_tasks.push(
            ff_agent::project_github_sync::GitHubSync::new(pg_pool).spawn(
                worker_name.clone(),
                5,
                shutdown_rx.clone(),
            ),
        );
    }

    // 20b7a) Fleet PR auto-merge tick — every 120s, leader-gated.
    // DEFAULT OFF: with `fleet_secrets.pr_automerge_mode` unset or `off`, the
    // loop is a quiet no-op. When enabled, the current leader evaluates open
    // fleet-authored `wi/` PRs and lands the ones the decision core clears.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!(
            "starting subsystem: fleet PR auto-merge tick (120s, leader-gated, gate=fleet_secrets.pr_automerge_mode default off)"
        );
        let mut shutdown_rx_pr_merge = shutdown_rx.clone();
        let pr_merge_worker = worker_name.clone();
        subsystem_tasks.push(tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx_pr_merge.changed() => break,
                    _ = tokio::time::sleep(Duration::from_secs(120)) => {
                        let pr_automerge_enabled = matches!(
                            ff_db::pg_read_gate_value(&pg_pool, "pr_automerge_mode", "off", "off")
                                .await
                                .as_deref(),
                            Ok("on") | Ok("true") | Ok("1") | Ok("active")
                        );
                        if !pr_automerge_enabled {
                            continue;
                        }

                        let is_leader = match ff_db::leader_state::pg_get_current_leader(&pg_pool).await {
                            Ok(Some(leader)) => leader.member_name == pr_merge_worker,
                            _ => false,
                        };
                        if !is_leader {
                            continue;
                        }

                        match ff_agent::pr_merge_executor::run_pr_merge_pass(&pg_pool).await {
                            Ok(report) => {
                                let report: PrMergeReport = report;
                                info!(
                                    considered = report.considered,
                                    merged = report.merged,
                                    held = report.held,
                                    blocked = report.blocked,
                                    "fleet PR auto-merge pass complete"
                                );
                            }
                            Err(e) => warn!(error = %e, "fleet PR auto-merge pass failed"),
                        }
                    }
                }
            }
        }));
    }

    // 20b8) OAuth probe tick — every 6h, leader-gated inside the tick.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!("starting subsystem: oauth probe tick (6h, leader-gated)");
        subsystem_tasks.push(ff_agent::oauth_distributor::spawn_oauth_probe_tick(
            pg_pool,
            worker_name.clone(),
            6 * 3600,
            shutdown_rx.clone(),
        ));
    }

    // 20c) Stale-task reaper tick — every 10min, per-node (idempotent).
    //
    // Tasks claimed by a worker that then died or restarted mid-run (common
    // during upgrade waves — workers restart themselves) sit in `running`
    // forever; nothing reclaims them. 613 such zombies (594 backup-rsync) were
    // found 2026-06-01, and they block the per-family upgrade singleton. This
    // reaps any task `running` longer than FORGEFLEET_TASK_REAP_SECS (default
    // 7200s = 2h, safely above the ~45min cold cargo build and the rsync
    // --timeout=3600) back to `pending` for retry, or terminal `failed` once
    // max_attempts is exhausted. The UPDATE is idempotent (row-locked), so
    // running per-node is harmless — no leader gate needed.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        let reap_secs: i64 = std::env::var("FORGEFLEET_TASK_REAP_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&s| s > 0)
            .unwrap_or(7200);
        info!(
            reap_secs,
            "starting subsystem: stale-task reaper tick (10min, per-node)"
        );
        let mut shutdown_rx_reap = shutdown_rx.clone();
        subsystem_tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(600));
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx_reap.changed() => break,
                    _ = tick.tick() => {
                        match ff_db::pg_reap_stale_running(&pg_pool, reap_secs).await {
                            Ok(0) => {}
                            Ok(n) => warn!(
                                reaped = n,
                                max_age_secs = reap_secs,
                                "reaped stale 'running' deferred tasks (orphaned by dead/restarted workers)"
                            ),
                            Err(e) => warn!(error = %e, "stale-task reaper failed"),
                        }
                    }
                }
            }
        }));
    }

    // 20d) Daemon-log rotation tick — every 10min, per-node (idempotent).
    //
    // forgefleetd does NOT own its log file — systemd `StandardOutput=append:`
    // (or launchd `StandardOutPath`) redirects our stdout/stderr into it, so
    // there's no tracing rolling-appender bounding its size. Left alone it grows
    // without limit (1.87 GiB observed on rihanna 2026-06-13 — a recurring
    // disk-pressure root cause; #212 stopped the openclaw restart-spam SOURCE
    // but the file still grows from normal logging and never shrinks). The
    // redirect opens the file in append mode, so truncating it in place reclaims
    // space and the next write lands at the fresh EOF. This copytruncates any
    // `forgefleetd*.log` over FORGEFLEET_LOG_MAX_MB (default 256 MiB), keeping a
    // bounded tail in `<name>.1`. Filesystem-only + per-node, like the disk
    // sampler — no DB, no leader gate.
    {
        let max_bytes = ff_agent::log_rotate::max_bytes();
        info!(
            cap_mb = max_bytes / 1_048_576,
            "starting subsystem: daemon-log rotation tick (10min, per-node)"
        );
        let mut shutdown_rx_logrot = shutdown_rx.clone();
        subsystem_tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(600));
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx_logrot.changed() => break,
                    _ = tick.tick() => {
                        if let Some(dir) = ff_agent::log_rotate::default_log_dir() {
                            ff_agent::log_rotate::rotate_dir(&dir, max_bytes);
                        }
                    }
                }
            }
        }));
    }

    // 20e) Leaked-orphan reaper tick — every 1h, per-node (idempotent).
    //
    // Before PR #215, the task runner SIGKILLed only the direct child shell on
    // timeout; every grandchild (ssh / git / rsync) it had spawned reparented
    // to pid 1 and ran forever (sophie carried 430 such orphans 2026-06-13,
    // saturating the host so every task it claimed wedged). #215 stops NEW
    // local leaks via a process-group kill, but cannot retroactively reap the
    // orphans already accumulated fleet-wide, nor orphans still produced by
    // hosts not yet on #215, nor a grandchild that `setsid`s out of the group.
    // This SIGKILLs any process that is (1) PPID==1, (2) an allow-listed
    // task-runner spawn (rsync / git-fetch), AND (3) older than
    // FORGEFLEET_ORPHAN_REAP_SECS (default 7200s). The triple gate makes a
    // false positive essentially impossible — a live tool has a live parent
    // (PPID≠1) and the allow-list excludes every daemon/model server that
    // legitimately reparents to init. Filesystem/`ps`-only, no DB, no leader
    // gate — like the disk sampler. Set FORGEFLEET_ORPHAN_REAP_SECS=0 to
    // disable.
    if let Some(min_age) = ff_agent::orphan_reaper::min_age_secs() {
        info!(
            min_age_secs = min_age,
            "starting subsystem: leaked-orphan reaper tick (1h, per-node)"
        );
        let mut shutdown_rx_orphan = shutdown_rx.clone();
        subsystem_tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx_orphan.changed() => break,
                    _ = tick.tick() => {
                        match ff_agent::orphan_reaper::reap_once(min_age) {
                            0 => {}
                            n => warn!(
                                reaped = n,
                                min_age_secs = min_age,
                                "reaped leaked orphan processes (pre-#215 task-runner grandchildren)"
                            ),
                        }
                    }
                }
            }
        }));
    }

    // 20f) Legacy `ff daemon` reaper tick — every 1h, per-node (idempotent).
    //
    // The worker loops (defer-worker, disk sampler, deployment reconciler) that
    // once lived in the sibling `ff daemon` CLI are now folded into forgefleetd
    // itself, so `ff daemon` is pure legacy (feedback_two_daemons). But the old
    // `ff daemon` processes were never stopped when hosts migrated: on
    // 2026-06-14, 12 of 15 hosts still ran a multi-week-old `ff daemon` (james
    // carried two, 15 days old). These stale supervisors race forgefleetd's
    // worker (a 15-day-old `ff` binary that wins a deferred-task claim runs the
    // task with pre-fix logic) and duplicate its reconciler/disk ticks. This
    // SIGTERMs any process whose command is `<…>/ff daemon …` (basename exactly
    // `ff`, first arg `daemon`, not `--once`) older than
    // FORGEFLEET_LEGACY_DAEMON_REAP_SECS (default 300s). `forgefleetd` is a
    // different basename so it can never match. The interval's first tick fires
    // immediately, so a freshly-(re)started forgefleetd reaps the legacy daemon
    // on this host at startup. Set FORGEFLEET_LEGACY_DAEMON_REAP_SECS=0 to
    // disable.
    if let Some(min_age) = ff_agent::legacy_daemon_reaper::min_age_secs() {
        info!(
            min_age_secs = min_age,
            "starting subsystem: legacy `ff daemon` reaper tick (1h, per-node)"
        );
        let mut shutdown_rx_legacy = shutdown_rx.clone();
        subsystem_tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx_legacy.changed() => break,
                    _ = tick.tick() => {
                        match ff_agent::legacy_daemon_reaper::reap_once(min_age) {
                            0 => {}
                            n => warn!(
                                reaped = n,
                                min_age_secs = min_age,
                                "reaped legacy `ff daemon` supervisor process(es) superseded by forgefleetd"
                            ),
                        }
                    }
                }
            }
        }));
    }

    // 21) Disk-reconcile tick — every 5min, leader-gated.
    //
    // V118 active disk management: reads `fleet_disk_usage`, finds over-quota
    // nodes, and runs the MOVE-vs-DELETE policy. SAFETY: gated by
    // `fleet_secrets.disk_policy_mode` (off|dry-run|active), DEFAULT off — the
    // tick is a pure no-op until an operator opts in, so deploying it is
    // harmless. `dry-run` logs the classified plan; `active` deletes
    // wrong-runtime/retired/peer-backed copies and relocates last copies of
    // still-wanted models (transfer-then-delete-after-verify). Leader-gated like
    // the autoscaler — disk policy is global state.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!(
            "starting subsystem: disk-reconcile tick (5min, leader-gated, gate=fleet_secrets.disk_policy_mode default off)"
        );
        subsystem_tasks.push(ff_agent::disk_reconcile::spawn_disk_reconcile_tick(
            pg_pool,
            worker_name.clone(),
            300,
            shutdown_rx.clone(),
        ));
    }

    // 20) Resource arbiter tick — every 60s, leader-gated.
    // Backlog #7 (V119): EXPLICIT-declaration host reservation. Reaps expired
    // leases (runs each owner's restore plan), walks the pending work_intents
    // FIFO, runs idempotent prework, and attempts set-atomic grants — applying
    // the priority-based preemption policy. Gate = `fleet_secrets.arbiter_mode`
    // DEFAULT OFF: with the gate at `off` (or unset) the tick is a pure no-op,
    // so deploying this is harmless. `dry-run` logs the full grant/prework/queue/
    // restore plan without actuating; `active` actuates.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!(
            "starting subsystem: resource arbiter tick (60s, leader-gated, gate=fleet_secrets.arbiter_mode default off)"
        );
        subsystem_tasks.push(ff_agent::arbiter::spawn_arbiter_tick(
            pg_pool,
            worker_name.clone(),
            60,
            shutdown_rx.clone(),
        ));
    }

    // 22) Fleet conformance tick — every 300s, leader-gated.
    //
    // BUILD #9/#10 increment 1: runs the V120 VERIFY GATES (gpu_bind /
    // amd_arch / kfd_access / pkg_version) against amd-training hosts and
    // records a MEASURED conformant bool + the SPECIFIC reason — catching what
    // "version string parsed = ok" misses (a +cu wheel on an AMD box; a daemon
    // user that can't open /dev/kfd). SAFETY: gated by
    // `fleet_secrets.conformance_mode` (off|dry-run|active), DEFAULT off — the
    // tick is a pure no-op until an operator opts in, so deploying it is
    // harmless. Increment 1 NEVER remediates a host (the apply-reconciler is a
    // deferred follow-up): dry-run and active are both record-only. Leader-gated
    // like the autoscaler / disk-policy ticks — conformance is global state.
    if let Some(pg_pool) = operational_store.pg_pool().cloned() {
        info!(
            "starting subsystem: fleet conformance tick (5min, leader-gated, gate=fleet_secrets.conformance_mode default off)"
        );
        subsystem_tasks.push(ff_agent::conformance::spawn_conformance_tick(
            pg_pool,
            worker_name.clone(),
            300,
            shutdown_rx.clone(),
        ));
    }

    info!("all subsystems started; waiting for shutdown signal");
    wait_for_shutdown_signal().await;

    info!("shutdown signal received; draining subsystems");
    let _ = shutdown_tx.send(true);

    cron_engine.shutdown();
    if let Err(join_err) = cron_task.await {
        warn!(error = %join_err, "cron task join failed during shutdown");
    }

    for task in subsystem_tasks {
        task.abort();
        let _ = task.await;
    }

    config_watcher.abort();
    let _ = config_watcher.await;

    info!("forgefleet shutdown complete");
    Ok(())
}

fn run_status(cli: &Cli) -> Result<()> {
    let config_path = resolve_config_path(cli.config.clone())?;
    let config = load_or_default_config(&config_path)?;

    println!("ForgeFleet status");
    println!("  version: {}", env!("CARGO_PKG_VERSION"));
    println!("  config: {}", config_path.display());
    println!("  fleet: {}", config.fleet.name);
    println!("  nodes: {}", config.nodes.len());
    println!("  models: {}", config.models.len());
    Ok(())
}

fn resolve_config_path(override_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(path);
    }

    let home = std::env::var("HOME").context("HOME environment variable not set")?;
    Ok(PathBuf::from(home).join(".forgefleet").join("fleet.toml"))
}

fn load_or_default_config(path: &Path) -> Result<FleetConfig> {
    if path.exists() {
        return config::load_config(path)
            .with_context(|| format!("failed to load config from {}", path.display()));
    }

    warn!(path = %path.display(), "config file missing; booting with defaults");
    let cfg: FleetConfig = toml::from_str("").context("failed to construct default config")?;
    Ok(cfg)
}

fn postgres_full_sqlite_blockers(_config: &FleetConfig) -> Vec<String> {
    // The ff-db SQLite replication/backup helpers were removed (Postgres HA now
    // covers replication), so there are no SQLite-helper blockers to report.
    Vec::new()
}

fn enforce_database_mode_preflight(config: &FleetConfig) -> Result<()> {
    if !config.database.requires_postgres_full_cutover() {
        return Ok(());
    }

    let mut issues = Vec::new();

    if config.database.url.trim().is_empty() {
        issues.push(
            "[database].url is empty (postgres_full requires an explicit Postgres URL)".to_string(),
        );
    }

    if config.database.cutover_evidence_ref().is_none() {
        issues.push(
            "missing [database].cutover_evidence (record backup + validation evidence before cutover)"
                .to_string(),
        );
    }

    let sqlite_blockers = postgres_full_sqlite_blockers(config);
    if !sqlite_blockers.is_empty() {
        issues.push(format!(
            "SQLite-only blockers still present:\n  - {}",
            sqlite_blockers.join("\n  - ")
        ));
    }

    if issues.is_empty() {
        return Ok(());
    }

    let mut message = String::from("database.mode=postgres_full preflight failed:");
    for issue in issues {
        message.push_str("\n- ");
        message.push_str(&issue);
    }
    message.push_str("\n\nDo not delete SQLite files yet.");
    message.push_str(
        "\nFollow docs/checklists/POSTGRES_FULL_CUTOVER_CHECKLIST.md and complete backup/validation evidence gates before final cutover.",
    );

    anyhow::bail!(message)
}

async fn initialize_operational_store(
    config: &FleetConfig,
    shared_pg_pool: std::sync::Arc<sqlx::PgPool>,
) -> Result<OperationalStore> {
    let database_url = config.database.url.trim();
    OperationalStore::postgres_with_pool(shared_pg_pool)
        .await
        .with_context(|| {
            format!(
                "failed to initialize Postgres operational store ({})",
                redact_database_url(database_url)
            )
        })
}

async fn initialize_runtime_registry(
    config: &FleetConfig,
    shared_pg_pool: std::sync::Arc<sqlx::PgPool>,
) -> Result<RuntimeRegistryStore> {
    let database_url = config.database.url.trim();
    RuntimeRegistryStore::postgres_with_pool(shared_pg_pool)
        .await
        .with_context(|| {
            format!(
                "failed to initialize Postgres runtime registry ({})",
                redact_database_url(database_url)
            )
        })
}

fn log_database_mode_summary(
    config: &FleetConfig,
    operational_store: &OperationalStore,
    runtime_registry: &RuntimeRegistryStore,
) {
    match config.database.mode {
        DatabaseMode::PostgresRuntime => {
            info!(
                mode = "postgres_runtime",
                postgres_url = %redact_database_url(&config.database.url),
                operational_store = operational_store.backend_label(),
                runtime_registry = runtime_registry.backend_label(),
                "database mode active: Postgres-backed operational/runtime persistence"
            );
        }
        DatabaseMode::PostgresFull => {
            info!(
                mode = "postgres_full",
                postgres_url = %redact_database_url(&config.database.url),
                operational_store = operational_store.backend_label(),
                runtime_registry = runtime_registry.backend_label(),
                cutover_evidence = %config.database.cutover_evidence_ref().unwrap_or("<missing>"),
                "database mode active: full-postgres operational persistence achieved"
            );
        }
    }
}

fn redact_database_url(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "<empty>".to_string();
    }

    if let Some((scheme, rest)) = trimmed.split_once("://")
        && let Some((userinfo, host_part)) = rest.split_once('@')
    {
        let redacted_userinfo = if let Some((user, _)) = userinfo.split_once(':') {
            format!("{user}:***")
        } else {
            "***".to_string()
        };
        return format!("{scheme}://{redacted_userinfo}@{host_part}");
    }

    "***".to_string()
}

async fn init_logging(cli: &Cli, worker_name: &str) -> Result<()> {
    let telemetry = TelemetryConfig {
        level: cli.log_level.clone(),
        json: cli.json_logs,
        worker_name: Some(worker_name.to_string()),
        ..Default::default()
    };

    // If the process-global NATS client is available, attach a
    // NatsLogLayer so every tracing event is mirrored onto
    // `logs.<node>.forgefleetd.<level>`. Otherwise fall back to the
    // plain file + stdout subscriber.
    let nats_client = ff_agent::nats_client::get_nats().await;
    let nats_layer = nats_client.map(|c| {
        ff_agent::nats_log_layer::NatsLogLayer::with_client(
            c.clone(),
            worker_name.to_string(),
            "forgefleetd".to_string(),
        )
    });

    use tracing_subscriber::Layer;
    let mut layers: Vec<Box<dyn Layer<tracing_subscriber::Registry> + Send + Sync>> = Vec::new();
    if let Some(layer) = nats_layer {
        layers.push(layer.boxed());
    }

    if layers.is_empty() {
        init_telemetry(&telemetry)
    } else {
        ff_observability::init_telemetry_with_extra_layer(&telemetry, layers)
    }
}

fn resolve_node_name(cli: &Cli, config: &FleetConfig) -> String {
    if let Some(worker_name) = &cli.worker_name {
        return worker_name.clone();
    }

    // FORGEFLEET_NODE_NAME — the canonical identity override. Mirrors
    // ff_agent::fleet_info::resolve_this_worker_name (priority 1). The DGX
    // outage of 2026-04-22 traced back to this check being absent here:
    // the systemd unit set the env but main.rs ignored it, so every DGX
    // fell through to `unknown-node` and pulse v2 refused to publish.
    if let Ok(v) = std::env::var("FORGEFLEET_NODE_NAME") {
        let t = v.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }

    if let Ok(hostname) = std::env::var("HOSTNAME")
        && !hostname.trim().is_empty()
    {
        return hostname;
    }

    // `hostname` shell command — systemd user services don't inherit
    // $HOSTNAME, so this is the realistic Linux fallback. On a cleanly
    // enrolled member the short hostname matches the `computers.name`
    // row (taylor, sia, marcus, …).
    if let Ok(out) = std::process::Command::new("hostname").output()
        && out.status.success()
    {
        let name = String::from_utf8_lossy(&out.stdout)
            .trim()
            .split('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if !name.is_empty() {
            return name;
        }
    }

    // Look for the gateway node (usually "taylor").
    if let Some((name, _node)) = config.nodes.iter().find(|(_, n)| n.role.is_leader_like()) {
        return name.clone();
    }

    config
        .nodes
        .keys()
        .next()
        .cloned()
        .unwrap_or_else(|| "unknown-node".to_string())
}

fn resolve_role(cli: &Cli, start: &StartArgs, config: &FleetConfig, worker_name: &str) -> String {
    if let Some(role) = &cli.role {
        return role.clone();
    }

    if start.leader {
        return "leader".to_string();
    }

    config
        .nodes
        .get(worker_name)
        .map(|n| format!("{:?}", n.role).to_ascii_lowercase())
        .unwrap_or_else(|| "auto".to_string())
}

fn print_startup_banner(worker_name: &str, role: &str, config_path: &Path) {
    println!(
        "\nForgeFleet v{}\n  node: {}\n  role: {}\n  config: {}\n",
        env!("CARGO_PKG_VERSION"),
        worker_name,
        role,
        config_path.display()
    );
}

// ─── Pre-seed registry from fleet.toml ───────────────────────────────────────

/// Register all configured fleet nodes into the discovery registry.
fn seed_registry_from_config(config: &FleetConfig, registry: &NodeRegistry) {
    for (name, node_cfg) in &config.nodes {
        if let Ok(ip) = node_cfg.ip.parse::<std::net::IpAddr>() {
            let port = node_cfg.port.unwrap_or(config.fleet.api_port);
            let priority = node_cfg.priority();

            registry.upsert_config_node(name, ip, port, priority);
            info!(
                node = %name,
                ip = %node_cfg.ip,
                port,
                priority,
                "seeded node from fleet.toml"
            );
        } else {
            warn!(
                node = %name,
                ip = %node_cfg.ip,
                "skipping node with unparseable IP"
            );
        }
    }
}

// ─── Build scan targets from the canonical FleetResolver ────────────────────

/// Build [`ScanTarget`]s by resolving the fleet through the canonical
/// [`ff_core::FleetResolver`] (Postgres → fleet.toml → SSH config → fleet.json).
///
/// Previously this read only from `config.nodes` (i.e. the `[nodes.*]` section
/// of `fleet.toml`). When that section was empty — which is the steady state
/// post-website-onboarding, since the source of truth is Postgres — the
/// discovery loop scanned an empty list and reported `total=0` indefinitely.
async fn build_fleet_scan_targets(config: &FleetConfig) -> Vec<ScanTarget> {
    let resolver = ff_core::FleetResolver::new();
    let computers = match resolver.resolve().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "fleet resolver failed; falling back to fleet.toml [nodes.*]");
            return build_scan_targets(
                config
                    .nodes
                    .iter()
                    .map(|(n, node)| (n.as_str(), node.ip.as_str(), node.port, node.priority())),
                config.fleet.api_port,
            );
        }
    };
    build_scan_targets(
        computers.iter().map(|c| {
            // Priority: keep the leader (50 default) hot, others 100.
            let prio: u32 = if c.role.eq_ignore_ascii_case("leader") {
                50
            } else {
                100
            };
            (c.name.as_str(), c.ip.as_str(), None, prio)
        }),
        config.fleet.api_port,
    )
}

// ─── Discovery subsystem ─────────────────────────────────────────────────────

fn start_discovery_subsystem(
    scanner_config: ScannerConfig,
    scan_targets: Vec<ScanTarget>,
    registry: Arc<NodeRegistry>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    subnet_scan_interval: Duration,
    subnet_scan_enabled: bool,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut health_ticker = tokio::time::interval(Duration::from_secs(30));
        let mut subnet_ticker = if subnet_scan_enabled {
            let mut t = tokio::time::interval(subnet_scan_interval);
            // Ensure the first tick fires after the interval, not immediately.
            t.reset();
            Some(t)
        } else {
            None
        };
        let node_scanner = NodeScanner::new(scan_targets);

        // Stale threshold: 90 seconds without heartbeat.
        let stale_threshold_secs: i64 = 90;

        loop {
            tokio::select! {
                _ = health_ticker.tick() => {
                    // 1) Fleet node health scan (HTTP /health).
                    let scan_results = node_scanner.scan_once().await;
                    let online = scan_results.iter().filter(|r| r.status == ff_discovery::NodeScanStatus::Online).count();
                    let degraded = scan_results.iter().filter(|r| r.status == ff_discovery::NodeScanStatus::Degraded).count();
                    let offline = scan_results.iter().filter(|r| r.status == ff_discovery::NodeScanStatus::Offline).count();

                    registry.apply_scan_results(&scan_results);
                    info!(
                        total = scan_results.len(),
                        online,
                        degraded,
                        offline,
                        "fleet node scan completed"
                    );

                    // 3) Mark stale nodes.
                    let stale = registry.mark_stale_nodes(stale_threshold_secs);
                    if !stale.is_empty() {
                        warn!(stale_nodes = ?stale, "marked nodes as stale (no heartbeat > 90s)");
                    }
                }
                _ = async {
                    match subnet_ticker.as_mut() {
                        Some(t) => t.tick().await,
                        None => std::future::pending().await,
                    }
                } => {
                    // 2) Subnet scan for new/unknown nodes.
                    match scan_subnet(&scanner_config).await {
                        Ok(nodes) => {
                            let count = nodes.len();
                            registry.upsert_many_discovered(nodes);
                            info!(count, "subnet discovery scan completed");
                        }
                        Err(err) => {
                            warn!(error = %err, "subnet discovery scan failed");
                        }
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        info!("discovery subsystem stopping");
                        break;
                    }
                }
            }
        }
    })
}

// ─── Other subsystems ────────────────────────────────────────────────────────

fn start_api_proxy_subsystem(api_config: ApiConfig) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(err) = ff_api::run(api_config).await {
            error!(error = %err, "api proxy subsystem exited with error");
        }
    })
}

fn start_agent_subsystem(
    agent_config: ff_agent::EmbeddedAgentConfig,
    operational_store: OperationalStore,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(err) = ff_agent::run(agent_config, operational_store, shutdown_rx).await {
            error!(error = %err, "agent subsystem exited with error");
        }
    })
}

fn start_gateway_subsystem(
    config: FleetConfig,
    config_path: String,
    backend_registry: std::sync::Arc<BackendRegistry>,
    discovery_registry: Arc<ff_discovery::NodeRegistry>,
    operational_store: OperationalStore,
    runtime_registry: RuntimeRegistryStore,
) -> JoinHandle<()> {
    let gateway_config = GatewayConfig {
        bind_addr: format!("0.0.0.0:{}", config.fleet.api_port.saturating_add(2)), // Web UI on api_port + 2 (51002)
        fleet_config: Some(config.clone()),
        config_path: Some(config_path),
        backend_registry: Some(backend_registry),
        discovery_registry: Some(discovery_registry),
        operational_store: Some(operational_store),
        runtime_registry: Some(runtime_registry),
        ..GatewayConfig::default()
    };

    // Report THIS binary's git SHA on /health (build_sha). Sourced from the
    // root build script's always-fresh FF_GIT_SHA — ff-gateway's own
    // compile-time bake can go stale across deploys (its build script's
    // `.git/HEAD` watch is a branch ref that never changes when `main` advances),
    // which made /health lie about the running code. (2026-06-25.)
    ff_gateway::server::set_runtime_build_sha(env!("FF_GIT_SHA"));

    tokio::spawn(async move {
        if let Err(err) = ff_gateway::run(gateway_config).await {
            error!(error = %err, "gateway subsystem exited with error");
        }
    })
}

fn start_tool_prune_subsystem(
    pg_pool: ff_db::PgPool,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        info!("starting subsystem: tool registry auto-prune");
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    match sqlx::query(
                        "DELETE FROM fleet_tools WHERE health_checked_at < NOW() - INTERVAL '5 minutes'",
                    )
                    .execute(&pg_pool)
                    .await
                    {
                        Ok(result) => {
                            if result.rows_affected() > 0 {
                                info!(pruned = result.rows_affected(), "auto-pruned stale fleet_tools");
                            }
                        }
                        Err(err) => {
                            warn!(error = %err, "tool registry auto-prune query failed");
                        }
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        info!("tool registry auto-prune subsystem stopping");
                        break;
                    }
                }
            }
        }
    })
}

fn start_telegram_transport_subsystem(
    config: FleetConfig,
    operational_store: OperationalStore,
    worker_name: String,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Some(telegram_cfg) = config.transport.telegram.clone() else {
            info!("telegram transport config missing; subsystem skipped");
            return;
        };

        let router = ff_gateway::MessageRouter::new(
            vec!["forgefleet".to_string(), "taylor".to_string()],
            vec!['/', '!'],
        );

        match ff_gateway::TelegramPollingTransport::new(
            telegram_cfg,
            operational_store.clone(),
            worker_name,
            router,
        ) {
            Ok(transport) => {
                if let Err(err) = transport.run(shutdown_rx).await {
                    error!(error = %err, "telegram transport subsystem exited with error");
                }
            }
            Err(err) => {
                error!(error = %err, "failed to initialize telegram transport subsystem");

                let _ = operational_store
                    .config_set("transport.telegram.enabled", "true")
                    .await;
                let _ = operational_store
                    .config_set("transport.telegram.running", "false")
                    .await;
                let _ = operational_store
                    .config_set("transport.telegram.last_error", &err.to_string())
                    .await;
            }
        }
    })
}

fn start_evolution_subsystem(
    config: FleetConfig,
    registry: Arc<NodeRegistry>,
    pg_pool: Option<sqlx::PgPool>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let loop_cfg = config.loops.evolution.clone();
        let mut engine = EvolutionEngine::default();
        engine.verifier = VerificationModel::new(loop_cfg.minimum_improvement_ratio);

        // Hydrate the recurrence backlog from Postgres so occurrence counters
        // survive restarts (without this, every restart resets them to zero and
        // a recurring issue never reaches promotion). Best-effort: a DB hiccup
        // must not stop the loop from running in-memory.
        if let Some(pool) = &pg_pool {
            match engine.backlog.load_from_pg(pool).await {
                Ok(n) if n > 0 => info!(loaded = n, "evolution backlog hydrated from Postgres"),
                Ok(_) => {}
                Err(e) => {
                    warn!(error = %e, "evolution backlog hydrate failed (continuing in-memory)")
                }
            }
        }

        let mut ticker = tokio::time::interval(Duration::from_secs(loop_cfg.interval_secs.max(10)));
        let mut previous_error_rate = 0.0f32;

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let nodes = registry.list_nodes();
                    if nodes.is_empty() {
                        continue;
                    }

                    let total_nodes = nodes.len();
                    let unhealthy_nodes: Vec<_> = nodes
                        .iter()
                        .filter(|node| {
                            !matches!(
                                node.health.as_ref().map(|h| h.status.clone()),
                                Some(HealthStatus::Healthy | HealthStatus::Degraded)
                            )
                        })
                        .collect();

                    let unhealthy_count = unhealthy_nodes.len();
                    let current_error_rate = unhealthy_count as f32 / total_nodes as f32;

                    if unhealthy_count == 0 && previous_error_rate <= f32::EPSILON {
                        continue;
                    }

                    let summary = format!(
                        "fleet health observation: {}/{} unhealthy nodes",
                        unhealthy_count,
                        total_nodes
                    );

                    let log = unhealthy_nodes
                        .iter()
                        .map(|node| {
                            let status = node
                                .health
                                .as_ref()
                                .map(|h| format!("{:?}", h.status))
                                .unwrap_or_else(|| "unknown".to_string());
                            format!(
                                "node={} ip={} status={} last_seen={}",
                                node.config_name.clone().unwrap_or_else(|| node.ip.to_string()),
                                node.ip,
                                status,
                                node.last_seen
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");

                    let observation = FailureObservation::new(FailureSource::Runtime, summary, log);

                    let verification_input = VerificationInput {
                        build_passed: true,
                        tests_passed: true,
                        health_checks_passed: unhealthy_count == 0,
                        error_rate_before: if previous_error_rate > 0.0 {
                            previous_error_rate
                        } else {
                            (current_error_rate + 0.05).min(1.0)
                        },
                        error_rate_after: current_error_rate,
                        regression_detected: previous_error_rate > 0.0
                            && current_error_rate > (previous_error_rate + 0.2),
                        notes: vec![format!(
                            "observed unhealthy nodes: {} of {}",
                            unhealthy_count, total_nodes
                        )],
                    };

                    match engine.run_once(observation, verification_input) {
                        Ok(run) => {
                            info!(
                                phase = ?run.state.phase,
                                backlog_items = run.durable_backlog_items_created,
                                unhealthy_nodes = unhealthy_count,
                                "evolution loop cycle complete"
                            );
                            // Write-through the (possibly updated) backlog so the
                            // recurrence state is durable across restarts.
                            if let Some(pool) = &pg_pool
                                && let Err(e) = engine.backlog.persist_all(pool).await
                            {
                                warn!(error = %e, "evolution backlog persist failed");
                            }
                        }
                        Err(err) => {
                            warn!(error = %err, "evolution loop cycle failed");
                        }
                    }

                    previous_error_rate = current_error_rate;
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        info!("evolution loop subsystem stopping");
                        break;
                    }
                }
            }
        }
    })
}

fn start_updater_subsystem(
    config: FleetConfig,
    worker_name: String,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let loop_cfg = config.loops.updater.clone();
        let repo_path = loop_cfg
            .repo_path
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let mut checker = UpdateChecker::new(CheckerConfig {
            repo_path: repo_path.clone(),
            remote: loop_cfg.git_remote.clone(),
            branch: loop_cfg.git_branch.clone(),
            check_interval_secs: loop_cfg.check_interval_secs.max(60),
            github_repo: None,
            github_token: None,
        });

        let mut ticker =
            tokio::time::interval(Duration::from_secs(loop_cfg.check_interval_secs.max(60)));

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !checker.should_check() {
                        continue;
                    }

                    match checker.check_git() {
                        Ok(check) => {
                            info!(
                                update_available = check.update_available,
                                commits_behind = check.commits_behind,
                                local_sha = %check.local_sha,
                                remote_sha = %check.remote_sha,
                                "updater loop check completed"
                            );

                            if check.update_available && loop_cfg.auto_apply {
                                let mut orchestrator = UpdateOrchestrator::new(
                                    build_updater_orchestrator_config(&worker_name, &repo_path, &loop_cfg),
                                    RestartSignal::None,
                                );

                                match orchestrator.run_update() {
                                    Ok(record) => {
                                        if matches!(record.state, ff_updater::orchestrator::UpdateState::Complete) {
                                            info!("updater loop applied update successfully");
                                        } else {
                                            warn!(state = ?record.state, error = ?record.error, "updater loop apply did not fully complete");
                                        }
                                    }
                                    Err(err) => {
                                        warn!(error = %err, "updater loop apply failed");
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            warn!(error = %err, "updater loop check failed");
                        }
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        info!("updater loop subsystem stopping");
                        break;
                    }
                }
            }
        }
    })
}

fn build_updater_orchestrator_config(
    worker_name: &str,
    repo_path: &Path,
    settings: &ff_core::config::UpdaterLoopSettings,
) -> OrchestratorConfig {
    let current_binary = settings
        .current_binary_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| PathBuf::from("forgefleetd"));

    OrchestratorConfig {
        worker_name: worker_name.to_string(),
        checker: CheckerConfig {
            repo_path: repo_path.to_path_buf(),
            remote: settings.git_remote.clone(),
            branch: settings.git_branch.clone(),
            check_interval_secs: settings.check_interval_secs.max(60),
            github_repo: None,
            github_token: None,
        },
        builder: BuilderConfig {
            repo_path: repo_path.to_path_buf(),
            remote: settings.git_remote.clone(),
            branch: settings.git_branch.clone(),
            binary_name: "forgefleetd".to_string(),
            ..BuilderConfig::default()
        },
        verifier: VerifierConfig::default(),
        swapper: SwapperConfig {
            current_binary: current_binary.clone(),
            ..SwapperConfig::default()
        },
        rollback: RollbackConfig {
            binary_path: current_binary,
            ..RollbackConfig::default()
        },
        rolling: Default::default(),
        canary: Default::default(),
        auto_update: settings.auto_apply,
    }
}

fn expected_model_ports_for_node(config: &FleetConfig, worker_name: &str) -> Vec<u16> {
    let mut ports = HashSet::new();

    if let Some(node_cfg) = config.nodes.get(worker_name) {
        for model_cfg in node_cfg.models.values() {
            if let Some(port) = model_cfg.port.or(node_cfg.port) {
                ports.insert(port);
            }
        }
    }

    let mut sorted: Vec<u16> = ports.into_iter().collect();
    sorted.sort_unstable();
    sorted
}

fn start_self_heal_subsystem(
    config: FleetConfig,
    worker_name: String,
    operational_store: OperationalStore,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let loop_cfg = config.loops.self_heal.clone();
        let manager = ProcessManager::with_config(ProcessManagerConfig {
            max_health_failures: loop_cfg.max_health_failures,
            health_check_interval_secs: loop_cfg.interval_secs.max(5),
            stop_timeout_secs: loop_cfg.stop_timeout_secs.max(1),
            health_probe_timeout_secs: loop_cfg.health_probe_timeout_secs.max(1),
        });

        let expected_ports = expected_model_ports_for_node(&config, &worker_name);
        if expected_ports.is_empty() {
            info!(node = %worker_name, "self-heal loop started with no expected local model ports");
        }

        // Hardcoded restart commands as fallback — matches the old behaviour
        let fallback_restart_commands: HashMap<String, (String, String)> = [
            ("marcus",   "192.168.5.102", "nohup ~/llama.cpp/build-new/bin/llama-server -m ~/models/qwen3-coder-30b-a3b/Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf --host 0.0.0.0 --port 55000 --ctx-size 32768 --threads 12 --jinja --no-warmup --parallel 4 > /tmp/llama-server.log 2>&1 &"),
            ("sophie",   "192.168.5.103", "nohup ~/llama.cpp/build-new/bin/llama-server -m ~/models/qwen3-coder-30b-a3b/Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf --host 0.0.0.0 --port 55000 --ctx-size 32768 --threads 4 --jinja --no-warmup --parallel 1 > /tmp/llama-server.log 2>&1 &"),
            ("priya",    "192.168.5.104", "nohup ~/llama.cpp/build-new/bin/llama-server -m ~/models/qwen3-coder-30b-a3b/Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf --host 0.0.0.0 --port 55000 --ctx-size 32768 --threads 8 --jinja --no-warmup --parallel 2 > /tmp/llama-server.log 2>&1 &"),
            ("james",    "192.168.5.108", "nohup ~/llama.cpp/build/bin/llama-server -m ~/models/qwen3.5-35b-a3b/Qwen3.5-35B-A3B-Q4_K_M.gguf --host 0.0.0.0 --port 55000 --ctx-size 32768 --threads 8 --jinja --no-warmup --parallel 2 > /tmp/llama-server.log 2>&1 &"),
            ("logan",    "192.168.5.111", "nohup ~/llama.cpp/build/bin/llama-server -m ~/models/qwen3.5-35b-a3b/Qwen3.5-35B-A3B-Q4_K_M.gguf --host 0.0.0.0 --port 55000 --ctx-size 32768 --threads 28 --jinja --no-warmup --parallel 4 > /tmp/llama-server.log 2>&1 &"),
            ("veronica", "192.168.5.112", "nohup env LD_LIBRARY_PATH=~/llama.cpp/build/bin ~/llama.cpp/build/bin/llama-server -m ~/models/qwen3.5-35b-a3b/Qwen3.5-35B-A3B-Q4_K_M.gguf --host 0.0.0.0 --port 55000 --ctx-size 32768 --threads 28 --jinja --no-warmup --parallel 4 > /tmp/llama-server.log 2>&1 &"),
            ("lily",     "192.168.5.113", "nohup env LD_LIBRARY_PATH=~/llama.cpp/build/bin ~/llama.cpp/build/bin/llama-server -m ~/models/qwen3.5-35b-a3b/Qwen3.5-35B-A3B-Q4_K_M.gguf --host 0.0.0.0 --port 55000 --ctx-size 32768 --threads 28 --jinja --no-warmup --parallel 4 > /tmp/llama-server.log 2>&1 &"),
            ("duncan",   "192.168.5.114", "nohup env LD_LIBRARY_PATH=~/llama.cpp/build/bin ~/llama.cpp/build/bin/llama-server -m ~/models/qwen3.5-35b-a3b/Qwen3.5-35B-A3B-Q4_K_M.gguf --host 0.0.0.0 --port 55000 --ctx-size 32768 --threads 28 --jinja --no-warmup --parallel 4 > /tmp/llama-server.log 2>&1 &"),
        ].iter().map(|(name, ip, cmd)| (name.to_string(), (ip.to_string(), cmd.to_string()))).collect();

        let mut ticker = tokio::time::interval(Duration::from_secs(loop_cfg.interval_secs.max(5)));

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if expected_ports.is_empty() {
                        continue;
                    }

                    if loop_cfg.auto_adopt {
                        let before = manager.model_count().await;
                        match manager.scan_and_adopt(&expected_ports).await {
                            Ok(_) => {
                                let after = manager.model_count().await;
                                if after > before {
                                    info!(adopted = after - before, managed = after, "self-heal adopted unmanaged llama-server process(es)");
                                }
                            }
                            Err(err) => {
                                warn!(error = %err, "self-heal scan/adopt failed");
                            }
                        }
                    }

                    let healthy = manager.health_check_all().await;
                    let managed = manager.model_count().await;
                    let restarted = manager.restart_crashed().await;

                    if !restarted.is_empty() {
                        warn!(restarted = ?restarted, "self-heal restarted unhealthy model process(es)");
                    } else {
                        info!(healthy, managed, "self-heal health sweep complete");
                    }

                    // Fleet-wide health check — read node list from Postgres if available,
                    // restart commands from fleet_settings or hardcoded fallback.
                    let fleet_workers: Vec<(String, String, String)> = {
                        let mut nodes = Vec::new();

                        // Try to read restart_commands from Postgres fleet_settings
                        let pg_restart_commands: HashMap<String, String> =
                            if let Some(pool) = operational_store.pg_pool() {
                                ff_db::pg_get_setting(pool, "restart_commands")
                                    .await
                                    .ok()
                                    .flatten()
                                    .and_then(|v| serde_json::from_value::<HashMap<String, String>>(v).ok())
                                    .unwrap_or_default()
                            } else {
                                HashMap::new()
                            };

                        // Try to get node list from Postgres
                        let pg_nodes = if let Some(pool) = operational_store.pg_pool() {
                            ff_db::pg_list_nodes(pool).await.unwrap_or_default()
                        } else {
                            Vec::new()
                        };

                        if !pg_nodes.is_empty() {
                            for db_node in &pg_nodes {
                                // Skip the leader node itself
                                if db_node.name == worker_name {
                                    continue;
                                }
                                // Use Postgres restart_commands setting if available,
                                // else fall back to hardcoded map
                                let restart_cmd = pg_restart_commands
                                    .get(&db_node.name)
                                    .cloned()
                                    .or_else(|| fallback_restart_commands.get(&db_node.name).map(|(_, cmd)| cmd.clone()))
                                    .unwrap_or_default();

                                nodes.push((
                                    db_node.name.clone(),
                                    db_node.ip.clone(),
                                    restart_cmd,
                                ));
                            }
                        } else {
                            // Fallback: use hardcoded list
                            for (name, (ip, cmd)) in &fallback_restart_commands {
                                nodes.push((name.clone(), ip.clone(), cmd.clone()));
                            }
                        }

                        nodes
                    };

                    let mut fleet_healthy = 0u32;
                    let mut fleet_issues = Vec::new();
                    let client = reqwest::Client::builder()
                        .timeout(Duration::from_secs(5))
                        .build()
                        .unwrap_or_default();

                    for (name, ip, restart_cmd) in &fleet_workers {
                        let url = format!("http://{}:55000/health", ip);
                        match client.get(&url).send().await {
                            Ok(r) if r.status().is_success() => { fleet_healthy += 1; }
                            _ => {
                                fleet_issues.push(format!("{name} ({ip})"));

                                // Attempt remote restart via SSH with node-specific command
                                if loop_cfg.auto_adopt && !restart_cmd.is_empty() {
                                    let ssh_cmd = format!(
                                        "ssh -o ConnectTimeout=5 -o StrictHostKeyChecking=no {}@{} \
                                         'pgrep -f llama-server || ({})' 2>/dev/null",
                                        name, ip, restart_cmd
                                    );
                                    let _ = tokio::process::Command::new("bash")
                                        .arg("-c")
                                        .arg(&ssh_cmd)
                                        .output()
                                        .await;
                                    info!(node = %name, ip = %ip, "self-heal attempted remote LLM restart on port 55000");
                                }
                            }
                        }
                    }

                    if !fleet_issues.is_empty() {
                        warn!(
                            healthy = fleet_healthy,
                            total = fleet_workers.len(),
                            issues = ?fleet_issues,
                            "fleet health: some nodes unhealthy"
                        );
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        info!("self-heal loop subsystem stopping");
                        break;
                    }
                }
            }
        }
    })
}

fn start_mcp_federation_subsystem(
    config: FleetConfig,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let loop_cfg = config.loops.mcp_federation.clone();
        let mut ticker = tokio::time::interval(Duration::from_secs(loop_cfg.interval_secs.max(10)));

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let snapshot = federation::collect_federation_snapshot(
                        &config,
                        loop_cfg.request_timeout_secs.max(1),
                    )
                    .await;

                    if snapshot.topology.valid {
                        info!(
                            services = snapshot.services.len(),
                            tools = snapshot.tools.len(),
                            "mcp federation topology validated"
                        );
                    } else {
                        warn!(
                            errors = ?snapshot.topology.errors,
                            warnings = ?snapshot.topology.warnings,
                            "mcp federation topology issues detected"
                        );
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        info!("mcp federation loop subsystem stopping");
                        break;
                    }
                }
            }
        }
    })
}

// ─── Pulse v2 subsystems (heartbeat_v2 + materializer + leader_tick) ────────
//
// Does three things, in order:
//   1) Look up this computer's `id` and `fleet_workers.election_priority`.
//      If the host isn't enrolled, log warn and return Ok(empty) — v2 stays
//      disabled for that host until enrollment.
//   2) Start HeartbeatV2Publisher unconditionally when the computer row
//      exists (it writes only its own `pulse:computer:{name}` key).
//   3) Start the Materializer on every daemon (see NOTE at call-site), and
//      start LeaderTick only if the computer is enrolled in `fleet_workers`.
async fn start_pulse_v2_subsystems(
    pg_pool: ff_db::PgPool,
    redis_url: String,
    worker_name: String,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<Vec<JoinHandle<()>>> {
    let mut handles: Vec<JoinHandle<()>> = Vec::new();

    // (1a) computer_id — required for everything else.
    let computer_id_row: Option<(uuid::Uuid,)> =
        sqlx::query_as::<_, (uuid::Uuid,)>("SELECT id FROM computers WHERE name = $1")
            .bind(&worker_name)
            .fetch_optional(&pg_pool)
            .await
            .context("pulse v2: failed to query computers by name")?;

    let Some((computer_id,)) = computer_id_row else {
        warn!(
            node = %worker_name,
            "pulse v2: no `computers` row for this host; Pulse v2 disabled until enrollment"
        );
        return Ok(handles);
    };

    // (1b) election_priority — optional; the worker row may not exist yet.
    // Query fleet_workers directly (V83/V86: fleet_members was dropped, its
    // election_priority lives on fleet_workers now, joined by name).
    let priority_row: Option<(i32,)> = sqlx::query_as::<_, (i32,)>(
        "SELECT fw.election_priority FROM fleet_workers fw \
         JOIN computers c ON c.name = fw.name \
         WHERE c.id = $1",
    )
    .bind(computer_id)
    .fetch_optional(&pg_pool)
    .await
    .context("pulse v2: failed to query fleet_workers")?;
    let enrolled_in_fleet = priority_row.is_some();
    let election_priority = priority_row.map(|(p,)| p).unwrap_or(1000);

    // (All pg_pool / shutdown_rx / worker_name / redis_url clones are done
    // inline at the call site — PgPool and watch::Receiver are cheap to clone.)

    // Build the redis::Client once — both publisher and materializer need one.
    let redis_client =
        redis::Client::open(redis_url.as_str()).context("pulse v2: failed to open redis client")?;

    // (V66) Spawn the detection-registry refresher so SoftwareCollector
    // has rules loaded before the first beat fires. Initial load happens
    // immediately, then every 5 min. Empty cache = empty inventory; the
    // refresher closes that gap on its first successful query.
    ff_pulse::detection_registry::spawn_refresher(pg_pool.clone());

    // (2) HeartbeatV2Publisher — always runs when computer row exists.
    info!(
        node = %worker_name,
        computer_id = %computer_id,
        election_priority,
        "starting subsystem: pulse v2 heartbeat"
    );
    let v2_pub = ff_pulse::HeartbeatV2Publisher::with_defaults(
        redis_client.clone(),
        worker_name.clone(),
        election_priority,
    )
    .with_build_sha(env!("FF_GIT_SHA"));
    // epoch_handle + role_handle are shared with leader_tick below when
    // available; for now we just spawn the publisher.
    let _epoch_handle = v2_pub.epoch_handle();
    let _role_handle = v2_pub.role_handle();
    // HA Phase 1: the publisher's voluntary step-down flag, driven by
    // leader_tick from the `leader_yield_request` fleet_secret (`ff fleet
    // leader step-down`). Captured before `spawn` consumes the publisher.
    let yield_handle = v2_pub.yielding_handle();
    handles.push(v2_pub.spawn(shutdown_rx.clone()));

    // HMAC key refresher — publishers sign beats with this key, materializer
    // + reader verify against the same cache. Refresh every 5 minutes.
    // If no key is configured we publish unsigned (rollout compat).
    {
        let cache = ff_pulse::pulse_hmac::KeyCache::global().clone();
        handles.push(cache.spawn_refresher(pg_pool.clone(), std::time::Duration::from_secs(300)));
    }

    // (3) Materializer — runs on every daemon for this phase. See NOTE
    // at call-site in run_daemon.
    info!("starting subsystem: pulse v2 materializer");
    let materializer =
        ff_pulse::materializer::Materializer::new(pg_pool.clone(), redis_client.clone());
    handles.push(materializer.spawn(shutdown_rx.clone()));

    // OpenClawManager — built BEFORE the fleet_members gate so the
    // reconciler runs on every daemon, including unenrolled workers.
    // Election callbacks (promote/demote) are wired only inside the
    // LeaderTick branch since they fire on transitions, but the
    // periodic reconcile_role on every node closes the gap for nodes
    // that never saw a transition.
    let my_primary_ip: String =
        sqlx::query_scalar("SELECT primary_ip FROM computers WHERE id = $1")
            .bind(computer_id)
            .fetch_one(&pg_pool)
            .await
            .unwrap_or_else(|_| "127.0.0.1".to_string());

    let openclaw = std::sync::Arc::new(ff_agent::openclaw::OpenClawManager::new(
        pg_pool.clone(),
        computer_id,
        my_primary_ip,
    ));

    // Reconciler — runs on EVERY daemon, every 60s. Reads
    // fleet_leader_state and ensures local OpenClaw role matches.
    // Idempotent. Doesn't depend on fleet_members enrollment.
    let oc_reconciler = openclaw.clone();
    let oc_reconciler_shutdown = shutdown_rx.clone();
    handles.push(tokio::spawn(async move {
        oc_reconciler
            .run_reconciler(oc_reconciler_shutdown, std::time::Duration::from_secs(60))
            .await;
    }));

    // Deployment reconciler — drives fleet_model_deployments desired_state
    // toward live process reality, every 60s. Without this tick, a dead
    // llama-server child stays dead until an operator manually re-runs
    // `ff model load`. Self-heal lives here. See V90 +
    // crates/ff-agent/src/deployment_reconciler.rs.
    {
        let pool = pg_pool.clone();
        let mut shutdown_rx_dep = shutdown_rx.clone();
        handles.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            // Skip the first immediate fire so we don't race forgefleetd
            // startup before pulse + workers are ready.
            tick.tick().await;
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx_dep.changed() => break,
                    _ = tick.tick() => {
                        match ff_agent::deployment_reconciler::reconcile_local(&pool).await {
                            Ok(s) => {
                                if s.respawned > 0
                                    || s.recovered > 0
                                    || s.reaped > 0
                                    || s.killed > 0
                                    || s.adopted > 0
                                    || s.removed > 0
                                    || s.port_violations > 0
                                {
                                    info!(
                                        adopted = s.adopted,
                                        respawned = s.respawned,
                                        recovered = s.recovered,
                                        reaped = s.reaped,
                                        killed = s.killed,
                                        removed = s.removed,
                                        port_violations = s.port_violations,
                                        refreshed = s.refreshed,
                                        "deployment reconciler pass"
                                    );
                                }
                            }
                            Err(e) => warn!("deployment reconciler failed: {e}"),
                        }
                    }
                }
            }
        }));
    }

    // Deferred-task scheduler — leader-gated, every 15s.
    //
    // Without this tick, tasks enqueued via `ff fleet upgrade`, the auto-
    // upgrade hourly tick, and any operator `ff defer add-shell` sit at
    // status='pending' forever — no worker claims pending tasks; they
    // need to be promoted to 'dispatchable' first by a scheduler.
    //
    // Before this was inlined into forgefleetd, the only way to drain
    // the queue was to manually run `ff daemon --scheduler` somewhere.
    // 2026-05-16 the queue had accumulated 951 pending tasks (up to 7
    // days old) because no scheduler was running.
    //
    // Leader-gated because the scheduler is global state — only one node
    // should promote, otherwise multiple promotion attempts race on the
    // same row. When the leader fails over, the new leader's forgefleetd
    // picks up the scheduler duty automatically.
    {
        let pool = pg_pool.clone();
        let me = worker_name.clone();
        let mut shutdown_rx_sched = shutdown_rx.clone();
        handles.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
            // Lead-time skip — first tick after 15s gives pulse v2 + leader
            // election a chance to settle before we ask "am I leader?".
            tick.tick().await;
            // Track online/offline transitions so we publish wake events
            // to Redis (workers waiting on `node_online` can claim
            // immediately instead of waiting up to 15s).
            let mut last_online: std::collections::HashSet<String> = Default::default();
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx_sched.changed() => break,
                    _ = tick.tick() => {
                        // 1. Am I the elected leader?
                        let is_leader = match ff_db::leader_state::pg_get_current_leader(&pool).await {
                            Ok(Some(l)) => l.member_name == me,
                            _ => false,
                        };
                        if !is_leader {
                            continue;
                        }
                        // 2. Online set from pulse (computers beaten within 60s).
                        let online: Vec<String> = match sqlx::query_as::<_, (String,)>(
                            "SELECT name FROM computers \
                             WHERE last_seen_at IS NOT NULL \
                               AND last_seen_at > NOW() - interval '60 seconds'",
                        )
                        .fetch_all(&pool)
                        .await
                        {
                            Ok(rows) => rows.into_iter().map(|(n,)| n).collect(),
                            Err(e) => {
                                warn!("scheduler: list online: {e}");
                                continue;
                            }
                        };
                        // 3. Publish online/offline transitions to Redis so
                        //    workers waiting on `node_online` wake immediately.
                        let current: std::collections::HashSet<String> = online.iter().cloned().collect();
                        for n in current.difference(&last_online) {
                            let _ = ff_agent::fleet_events::publish_node_online(n).await;
                        }
                        for n in last_online.difference(&current) {
                            let _ = ff_agent::fleet_events::publish_node_offline(n).await;
                        }
                        last_online = current;
                        // 4. Promote pending → dispatchable.
                        let now = chrono::Utc::now();
                        match ff_db::pg_scheduler_pass(&pool, &online, now).await {
                            Ok(n) if n > 0 => info!(
                                promoted = n,
                                online = online.len(),
                                "scheduler tick promoted pending → dispatchable"
                            ),
                            Ok(_) => {}
                            Err(e) => warn!("scheduler pg_scheduler_pass: {e}"),
                        }
                    }
                }
            }
        }));
    }

    // (4) LeaderTick — only when enrolled in fleet_members.
    if enrolled_in_fleet {
        info!(
            node = %worker_name,
            election_priority,
            "starting subsystem: pulse v2 leader_tick"
        );
        let pulse_reader = ff_pulse::reader::PulseReader::new(&redis_url)
            .context("pulse v2: failed to build PulseReader for leader_tick")?;

        let oc_promote = openclaw.clone();
        let oc_demote = openclaw.clone();
        let pool_for_url = pg_pool.clone();
        let pool_for_promote = pg_pool.clone();
        let pool_for_demote = pg_pool.clone();

        let my_name_for_promote = worker_name.clone();
        let my_name_for_demote = worker_name.clone();

        let on_became: ff_agent::leader_tick::OnBecameLeader = std::sync::Arc::new(
            move |prev: Option<String>| {
                let oc = oc_promote.clone();
                let my_name = my_name_for_promote.clone();
                let pool = pool_for_promote.clone();
                tokio::spawn(async move {
                    // Publish leader-change event to NATS (best-effort).
                    ff_agent::fleet_events_nats::FleetEventBus::publish_leader_change(
                        prev.as_deref(),
                        &my_name,
                        0,
                    )
                    .await;

                    if let Err(e) = oc.promote_to_gateway(prev.as_deref()).await {
                        tracing::error!(error = %e, "openclaw: promote_to_gateway failed");
                    } else {
                        // Surface promotion as a deployment.started event for the openclaw-gateway.
                        ff_agent::fleet_events_nats::FleetEventBus::publish_deployment_change(
                            &my_name,
                            uuid::Uuid::nil(),
                            "started",
                            "openclaw-gateway",
                        )
                        .await;

                        // Re-import paired devices stashed by the previous
                        // leader on its way out (if any). Best-effort: if
                        // the secret is missing or the import fails, just
                        // log — phones/IoT will need to re-pair but the
                        // gateway is still functional.
                        match ff_agent::openclaw::lookup_device_pairings_export(&pool).await {
                            Ok(Some(export)) if !export.trim().is_empty() => {
                                match oc.import_devices(&export).await {
                                    Ok(n) => {
                                        tracing::info!(
                                            count = n,
                                            "openclaw: imported paired devices from previous leader"
                                        );
                                        if let Err(e) =
                                            ff_agent::openclaw::clear_device_pairings_export(&pool)
                                                .await
                                        {
                                            tracing::warn!(
                                                error = %e,
                                                "openclaw: failed to clear device pairings secret"
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            error = %e,
                                            "openclaw: import_devices failed — devices may need to re-pair"
                                        );
                                    }
                                }
                            }
                            Ok(_) => {
                                tracing::debug!(
                                    "openclaw: no device pairings stashed (cold start or previous leader crashed before export)"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    "openclaw: could not read device pairings secret"
                                );
                            }
                        }
                    }
                });
            },
        );

        let on_lost: ff_agent::leader_tick::OnLostLeader = std::sync::Arc::new(
            move |new_leader_name: String| {
                let oc = oc_demote.clone();
                let pool = pool_for_url.clone();
                let pool_export = pool_for_demote.clone();
                let my_name = my_name_for_demote.clone();
                tokio::spawn(async move {
                    // Publish leader-change event to NATS (best-effort).
                    let new_name = if new_leader_name.is_empty() {
                        "unknown"
                    } else {
                        new_leader_name.as_str()
                    };
                    ff_agent::fleet_events_nats::FleetEventBus::publish_leader_change(
                        Some(&my_name),
                        new_name,
                        0,
                    )
                    .await;

                    // Export paired devices BEFORE demoting so the new
                    // leader can re-import them. Best-effort: a failure
                    // here degrades to "devices must re-pair", not a
                    // gateway outage.
                    match oc.export_devices().await {
                        Ok(export) => {
                            if let Err(e) = sqlx::query(
                                "INSERT INTO fleet_secrets (key, value, updated_by, updated_at) \
                                 VALUES ($1, $2, 'openclaw-manager', NOW()) \
                                 ON CONFLICT (key) DO UPDATE \
                                 SET value = $2, updated_at = NOW()",
                            )
                            .bind(ff_agent::openclaw::DEVICE_PAIRINGS_SECRET_KEY)
                            .bind(&export)
                            .execute(&pool_export)
                            .await
                            {
                                tracing::warn!(
                                    error = %e,
                                    "openclaw: failed to stash device pairings to fleet_secrets"
                                );
                            } else {
                                tracing::info!(
                                    bytes = export.len(),
                                    "openclaw: stashed paired-device export for new leader"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "openclaw: export_devices failed during demotion — devices will lose pairing"
                            );
                        }
                    }

                    let url = ff_agent::openclaw::lookup_gateway_url(&pool)
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                    if url.is_empty() {
                        tracing::warn!("openclaw: lost leader but no gateway URL published yet");
                        return;
                    }
                    if let Err(e) = oc.demote_to_node(&url).await {
                        tracing::error!(error = %e, "openclaw: demote_to_node failed");
                    } else {
                        ff_agent::fleet_events_nats::FleetEventBus::publish_deployment_change(
                            &my_name,
                            uuid::Uuid::nil(),
                            "stopped",
                            "openclaw-gateway",
                        )
                        .await;
                    }
                });
            },
        );

        // Phase 6 HA — auto Postgres failover manager. Runs inside every
        // leader tick (only when we are the current fleet leader). The
        // whole path is no-op'd by the env var
        // FORGEFLEET_DISABLE_AUTO_PG_FAILOVER=true for safety drills.
        let pg_failover_manager = std::sync::Arc::new(
            ff_agent::ha::pg_failover::PostgresFailoverManager::new(pg_pool.clone(), computer_id),
        );
        info!(
            node = %worker_name,
            computer_id = %computer_id,
            disabled = ff_agent::ha::pg_failover::DISABLE_ENV,
            "pg_failover manager constructed (auto-failover enabled unless {} is set)",
            ff_agent::ha::pg_failover::DISABLE_ENV
        );

        let leader_tick = ff_agent::leader_tick::LeaderTick::new(
            pg_pool.clone(),
            pulse_reader,
            computer_id,
            worker_name.clone(),
            election_priority,
        )
        .with_on_became_leader(on_became)
        .with_on_lost_leader(on_lost)
        .with_pg_failover(pg_failover_manager)
        .with_yield_flag(yield_handle);
        handles.push(leader_tick.spawn(15, shutdown_rx.clone()));
    } else {
        info!(
            node = %worker_name,
            "pulse v2: no fleet_members row — leader_tick NOT started (materializer + heartbeat_v2 still running)"
        );
    }

    // (5) Backup orchestrator (Phase 6 HA).
    //
    // Runs on EVERY daemon that has a computers row — the orchestrator
    // internally short-circuits when we're not the current leader, so
    // startup is cheap on followers. This avoids having to re-wire the
    // backup task into leader_tick's on_became_leader / on_lost_leader
    // callbacks.
    info!(
        node = %worker_name,
        computer_id = %computer_id,
        "starting subsystem: backup orchestrator (pg=4h, redis=2h)"
    );
    let backup = ff_agent::ha::backup::BackupOrchestrator::new(
        pg_pool.clone(),
        computer_id,
        worker_name.clone(),
        None,
    );
    handles.push(backup.spawn(shutdown_rx.clone()));

    // (6) Phase 10 — metrics downsampler. Each tick is gated internally on
    // leadership via `fleet_leader_state`, so we start it on every daemon and
    // it no-ops on followers.
    match ff_pulse::reader::PulseReader::new(&redis_url.clone()) {
        Ok(metrics_reader) => {
            info!(
                node = %worker_name.clone(),
                "starting subsystem: metrics downsampler (60s, leader-gated)"
            );
            let dsamp = ff_agent::metrics_downsampler::MetricsDownsampler::new(
                pg_pool.clone(),
                metrics_reader,
                worker_name.clone(),
            );
            handles.push(dsamp.spawn(shutdown_rx.clone()));
        }
        Err(e) => warn!(error = %e, "metrics downsampler: failed to build PulseReader"),
    }

    // (7) Phase 10 — alert evaluator. Also leader-gated internally.
    match ff_pulse::reader::PulseReader::new(&redis_url.clone()) {
        Ok(alert_reader) => {
            info!(
                node = %worker_name.clone(),
                "starting subsystem: alert evaluator (60s, leader-gated)"
            );
            let evaluator = ff_agent::alert_evaluator::AlertEvaluator::new(
                pg_pool.clone(),
                alert_reader,
                worker_name.clone(),
            );
            handles.push(evaluator.spawn(shutdown_rx.clone()));
        }
        Err(e) => warn!(error = %e, "alert evaluator: failed to build PulseReader"),
    }

    // (7b) DB integrity guard — runs `amcheck` over every btree unique index
    // every 6h on the leader and raises the `db_index_corruption` alert on
    // corruption. Catches glibc/ICU collation drift (which silently corrupted
    // indexes on 2026-05-30) automatically. Leader-gated INSIDE the tick on
    // every fire, so it's safe to start on every daemon. Alert-only — it
    // never auto-REINDEXes (updates are never auto-applied).
    info!(
        node = %worker_name.clone(),
        "starting subsystem: db integrity guard (amcheck, 6h, leader-gated)"
    );
    let amcheck_tick =
        ff_agent::db_integrity::AmcheckTick::new(pg_pool.clone(), worker_name.clone());
    handles.push(amcheck_tick.spawn(shutdown_rx.clone()));

    // (7b') Backup restore-drill — daily on the leader. Takes the newest
    // Postgres backup all the way through decrypt → extract → PGDATA-structure
    // validation, records the outcome in `backup_drills`, and fires the
    // `backup_restore_drill_failed` alert on failure or "no successful drill in
    // N days". A backup that has never been test-restored is the silent
    // 2026-04-18-wipe risk; this proves restorability automatically. Leader-
    // gated inside the tick on every fire (safe to start on every daemon) and
    // self-cleaning (temp extract dir removed on every path).
    info!(
        node = %worker_name.clone(),
        "starting subsystem: backup restore-drill (24h, leader-gated)"
    );
    let restore_drill =
        ff_agent::ha::restore_drill::RestoreDrillTick::new(pg_pool.clone(), worker_name.clone());
    handles.push(restore_drill.spawn(shutdown_rx.clone()));

    // (7c) Stale-job sweeper — every 5min on the leader. Recovers
    // `fleet_model_jobs` + `deferred_tasks` rows stuck in `running` (crashed
    // process, stalled download, or a worker restarted mid-task by the upgrade
    // wave leaving its own claimed rows orphaned). This used to run ONLY inside
    // the legacy `ff daemon`; PR #298's legacy-daemon reaper disabled every
    // legacy `ff daemon` fleet-wide, which silently killed the sweep and let
    // orphaned deferred tasks leak. Relocated here (forgefleetd is production)
    // with the SAME SweepPolicy thresholds — a pure relocation. Leader-gated
    // inside the tick on every fire, so it's safe to start on every daemon.
    info!(
        node = %worker_name.clone(),
        "starting subsystem: stale-job sweeper (5min, leader-gated)"
    );
    let stale_job_sweeper =
        ff_agent::job_sweeper::StaleJobSweeperTick::new(pg_pool.clone(), worker_name.clone());
    handles.push(stale_job_sweeper.spawn(shutdown_rx.clone()));

    // (7d) Research runner — every 30s on the leader. Drives detached
    // (`ff research --detach`) sessions to completion inside forgefleetd: the
    // CLI inserts a `queued` session and exits, this tick claims it and runs the
    // planner→dispatch→synthesis pipeline in the daemon so the run survives the
    // originating CLI being killed. Complements the sweeper + auto-recover, which
    // only salvage *completed* sub-agent work after a crash. Leader-gated inside
    // the tick on every fire, so safe to start on every daemon.
    info!(
        node = %worker_name.clone(),
        "starting subsystem: research runner (detached --detach runs, 30s, leader-gated)"
    );
    let research_runner =
        ff_agent::research::ResearchRunnerTick::new(pg_pool.clone(), worker_name.clone());
    handles.push(research_runner.spawn(shutdown_rx.clone()));

    // (8) Auto-upgrade hourly tick — runs on every daemon, internally
    // gated on leader + fleet_secrets.auto_upgrade_enabled. Refreshes
    // upstream versions (npm/pypi/github_release/self_built), flips
    // drift status, and dispatches upgrade tasks. Without this, version
    // checking only happens when an operator runs
    // `ff software auto-upgrade-run-once --force` manually.
    info!(
        node = %worker_name.clone(),
        "starting subsystem: auto-upgrade tick (hourly, leader-gated)"
    );
    let auto_upgrade_tick = ff_agent::auto_upgrade::AutoUpgradeTick::new(
        pg_pool.clone(),
        worker_name.clone(),
        env!("FF_GIT_SHA").to_string(),
    );
    handles.push(auto_upgrade_tick.spawn(shutdown_rx.clone()));

    // (8b) Portfolio + drift maintenance ticks — model-upstream (24h),
    // model-scout (168h), external-tools upstream (6h), coverage gap detection
    // (15min, read-only), and the stuck agent-slot reaper (10min). Each is
    // leader-gated (re-checked per tick). These ran ONLY in the legacy
    // `ff daemon` before, so in production new models were never discovered and
    // model/tool drift was never detected. `software_upstream` is intentionally
    // excluded (AutoUpgradeTick already refreshes software_registry.latest_version
    // inline). See ff_agent::portfolio_maintenance for the full rationale.
    info!(
        node = %worker_name.clone(),
        "starting subsystem: portfolio + drift maintenance (leader-gated)"
    );
    handles.extend(
        ff_agent::portfolio_maintenance::spawn_portfolio_maintenance(
            pg_pool.clone(),
            worker_name.clone(),
            shutdown_rx.clone(),
        ),
    );

    // (9) fleet_tasks worker — every daemon polls fleet_tasks for shell
    // payloads whose `requires_capability` ⊆ this computer's set, claims
    // via SKIP LOCKED, and runs them. Cooperative work-stealing across
    // the fleet. Capabilities derived below from os_family + name +
    // local probes for redis-cli / hf-cli / etc.
    info!(
        node = %worker_name.clone(),
        "starting subsystem: fleet_tasks worker (every 10s)"
    );
    {
        let pool = pg_pool.clone();
        let name = worker_name.clone();
        let shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            // Look up our computer_id + capabilities once.
            let row: Option<(uuid::Uuid, String, Option<String>)> = sqlx::query_as(
                "SELECT id, COALESCE(os_family, 'unknown') as os_family, name \
                 FROM computers WHERE name = $1",
            )
            .bind(&name)
            .fetch_optional(&pool)
            .await
            .ok()
            .flatten();
            let Some((my_id, os_family, _)) = row else {
                warn!(node = %name, "task_runner: no computers row, worker disabled");
                return;
            };
            let mut caps: std::collections::HashSet<String> = std::collections::HashSet::new();
            caps.insert(os_family.clone());
            caps.insert(name.clone());
            // Cross-cutting capability flags. Detected by looking for
            // the binary on $PATH; cheap, runs once per daemon start.
            // The CLI tools (claude / codex / gemini / kimi / grok) gate
            // Phase-3 of the multi-LLM CLI integration roadmap — tasks
            // with `requires_capability=[claude]` only dispatch to
            // members where the binary is present.
            for tool in [
                "redis-cli",
                "hf",
                "ssh",
                "iperf3",
                "nc",
                "curl",
                "git",
                "ff",
                "claude",
                "codex",
                "gemini",
                "kimi",
                "grok",
                // Screen-control helpers (PR-G). Members with these
                // can run computer_use MCP actions; tasks with
                // requires_capability=[screen] route only there.
                "cliclick", // macOS click/type/key driver
                "xdotool",  // Linux click/type/key driver
                "scrot",    // Linux screenshot
            ] {
                let out = std::process::Command::new("/bin/sh")
                    .arg("-lc")
                    .arg(format!("command -v {tool} >/dev/null 2>&1"))
                    .status();
                if matches!(out, Ok(s) if s.success()) {
                    caps.insert(tool.to_string());
                }
            }
            // Synthetic `screen` capability: fleet member can drive
            // its own screen if either the macOS path (cliclick +
            // built-in screencapture) or the Linux path
            // (xdotool + scrot) is fully present. Lets task dispatch
            // route screen work to the right member without
            // hardcoding per-tool requirements.
            let macos_screen = caps.contains("cliclick"); // screencapture is always present on macOS
            let linux_screen = caps.contains("xdotool") && caps.contains("scrot");
            if macos_screen || linux_screen {
                caps.insert("screen".to_string());
            }
            // Leader gets a separate tag — composers can reserve work
            // that only the elected leader should run (e.g. coordinated
            // bootstraps).
            let is_leader: Option<String> =
                sqlx::query_scalar("SELECT member_name FROM fleet_leader_state LIMIT 1")
                    .fetch_optional(&pool)
                    .await
                    .ok()
                    .flatten();
            if let Some(l) = is_leader
                && l.eq_ignore_ascii_case(&name)
            {
                caps.insert("leader".to_string());
            }
            // FF_* env bag (FF_NODE, FF_SOURCE_TREE, FF_LEADER_NAME,
            // FF_GATEWAY_URL, …) — resolved from the DB so shell tasks
            // never have to embed IPs / paths / users in source.
            let task_env =
                match ff_agent::task_runner::TaskRunner::resolve_env_from_db(&pool, &name).await {
                    Ok(e) => e,
                    Err(e) => {
                        warn!(node = %name, error = %e, "task_runner: env resolve failed");
                        Vec::new()
                    }
                };
            info!(
                node = %name,
                computer_id = %my_id,
                capabilities = ?caps,
                env_keys = ?task_env.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
                "task_runner ready"
            );
            let runner = ff_agent::task_runner::TaskRunner::new(pool, my_id, name, caps, task_env);
            let _ = runner.spawn(10, shutdown).await;
        });
    }

    // (10) fleet_tasks watchdog — every daemon runs the distributed
    // handoff sweep. Re-queues stalled `running` tasks whose worker has
    // gone quiet for >120s.
    info!("starting subsystem: fleet_tasks leader watchdog (every 60s)");
    handles.push(ff_agent::task_runner::spawn_leader_watchdog(
        pg_pool.clone(),
        worker_name.clone(),
        shutdown_rx.clone(),
    ));

    // (10d) Wave-reaper — rolls up fleet-upgrade-wave parent rows whose
    // children have all reached a terminal state. Without this, every
    // wave leaves a zombie parent row in 'pending' forever. Watches any
    // pending parent whose children sit in `fleet_tasks` linked via
    // parent_task_id — the actual fan-out pattern used by
    // `compose_fleet_upgrade_wave`.
    info!("starting subsystem: wave reaper (every 10min, leader-only)");
    handles.push(ff_agent::wave_reaper::spawn_reaper(
        pg_pool.clone(),
        worker_name.clone(),
        10 * 60,
        shutdown_rx.clone(),
    ));

    // (11) Shared workspace cleanup — daily temp/artifact purge for
    // sub-agent workspaces. Leader-gated to avoid N-way races.
    info!("starting subsystem: shared workspace cleanup (daily, leader-gated)");
    {
        let pool = pg_pool.clone();
        let name = worker_name.clone();
        let mut shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(24 * 3600));
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() { break; }
                    }
                }
                // Only run on leader
                let is_leader: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM fleet_leader_state WHERE member_name = $1)",
                )
                .bind(&name)
                .fetch_one(&pool)
                .await
                .unwrap_or(false);
                if !is_leader {
                    continue;
                }

                // Run cleanup for all agent workspaces
                let agents = match ff_agent::shared_workspace::list_agent_workspaces().await {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to list agent workspaces");
                        continue;
                    }
                };
                let node_id: Option<uuid::Uuid> =
                    sqlx::query_scalar("SELECT id FROM computers WHERE name = $1")
                        .bind(&name)
                        .fetch_optional(&pool)
                        .await
                        .ok()
                        .flatten();
                for agent_id in agents {
                    if let Err(e) =
                        ff_agent::shared_workspace::run_cleanup(&agent_id, Some(&pool), node_id)
                            .await
                    {
                        tracing::warn!(agent_id = %agent_id, error = %e, "workspace cleanup failed");
                    }
                }
            }
        });
    }

    // (12) Vault sync — hourly index regeneration + TODO scan.
    // Leader-gated to avoid N-way races.
    info!("starting subsystem: vault sync (hourly, leader-gated)");
    {
        let pool = pg_pool.clone();
        let name = worker_name.clone();
        let mut shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
            let vault_path =
                std::path::PathBuf::from(std::env::var("FF_VAULT_PATH").unwrap_or_else(|_| {
                    std::env::var("HOME")
                        .map(|h| {
                            std::path::PathBuf::from(h)
                                .join("projects")
                                .join("Yarli_KnowledgeBase")
                        })
                        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/vault"))
                        .to_string_lossy()
                        .to_string()
                }));
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() { break; }
                    }
                }
                let is_leader: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM fleet_leader_state WHERE member_name = $1)",
                )
                .bind(&name)
                .fetch_one(&pool)
                .await
                .unwrap_or(false);
                if !is_leader {
                    continue;
                }

                if let Err(e) = ff_agent::vault_sync::setup_forgefleet_vault(&vault_path).await {
                    tracing::warn!(error = %e, "vault setup failed");
                }
                if let Err(e) =
                    ff_agent::vault_sync::regenerate_index_md(&vault_path, &name, &pool).await
                {
                    tracing::warn!(error = %e, "vault index regeneration failed");
                }
                if let Err(e) = ff_agent::vault_sync::scan_vault_todos(&vault_path, &pool).await {
                    tracing::warn!(error = %e, "vault TODO scan failed");
                }
            }
        });
    }

    Ok(handles)
}

fn start_mcp_http_subsystem(config: FleetConfig) -> JoinHandle<()> {
    tokio::spawn(async move {
        let server = ff_mcp::McpServer::new();
        let transport = ff_mcp::transport::HttpTransport::new(server);
        let addr = format!(
            "0.0.0.0:{}",
            config
                .mcp
                .get("forgefleet")
                .and_then(|m| m.port)
                .unwrap_or(50001)
        );
        info!(addr = %addr, "MCP HTTP server starting");
        if let Err(e) = transport.run(&addr).await {
            error!(error = %e, "MCP HTTP server failed");
        }
    })
}

fn build_embedded_agent_config(
    config: &FleetConfig,
    worker_name: String,
) -> ff_agent::EmbeddedAgentConfig {
    ff_agent::EmbeddedAgentConfig {
        worker_name,
        autonomous_mode: config.agent.autonomous_mode,
        poll_interval_secs: config.agent.poll_interval_secs,
        ownership_api_base_url: config.agent.ownership_api_base_url.clone(),
        llm_base_url: std::env::var("FF_PIPELINE_LLM_BASE_URL").ok(),
        llm_model: std::env::var("FF_PIPELINE_LLM_MODEL").ok(),
        // InferenceRouter is wired in at the call site (async context).
        inference_router: None,
    }
}

async fn build_api_config(config: &FleetConfig, pg_pool: Option<&ff_db::PgPool>) -> ApiConfig {
    let mut backends = Vec::new();

    // 1) Node-level model mapping from fleet.toml (may be empty when all config is in Postgres)
    for (worker_name, node_cfg) in &config.nodes {
        for (model_slug, model_cfg) in &node_cfg.models {
            let port = model_cfg.port.unwrap_or(config.fleet.api_port);
            let is_local = !model_slug.starts_with("gpt")
                && !model_slug.starts_with("claude")
                && !model_slug.starts_with("gemini");
            backends.push(BackendEndpoint {
                id: format!("{}:{}:{}", worker_name, model_slug, port),
                node: worker_name.clone(),
                host: node_cfg.ip.clone(),
                port,
                model: model_slug.clone(),
                tier: model_cfg.tier as u8,
                healthy: true,
                busy: false,
                scheme: "http".to_string(),
                is_local,
                cost_per_1k_input: if is_local { 0.0 } else { 0.001 },
                cost_per_1k_output: if is_local { 0.0 } else { 0.003 },
            });
        }
    }

    // 2) Legacy [[models]] entries (backward compatibility)
    let node_by_name: HashMap<&str, (&str, u16)> = config
        .nodes
        .iter()
        .map(|(name, node)| {
            (
                name.as_str(),
                (node.ip.as_str(), node.port.unwrap_or(config.fleet.api_port)),
            )
        })
        .collect();

    for model in &config.models {
        for worker_name in &model.nodes {
            let (host, port) = node_by_name
                .get(worker_name.as_str())
                .copied()
                .unwrap_or(("127.0.0.1", config.fleet.api_port));

            let id = format!("{}:{}:{}", worker_name, model.id, port);
            if backends.iter().any(|b| b.id == id) {
                continue;
            }

            let is_local = !model.id.starts_with("gpt")
                && !model.id.starts_with("claude")
                && !model.id.starts_with("gemini");
            backends.push(BackendEndpoint {
                id,
                node: worker_name.clone(),
                host: host.to_string(),
                port,
                model: model.id.clone(),
                tier: model.tier.as_u8(),
                healthy: true,
                busy: false,
                scheme: "http".to_string(),
                is_local,
                cost_per_1k_input: if is_local { 0.0 } else { 0.001 },
                cost_per_1k_output: if is_local { 0.0 } else { 0.003 },
            });
        }
    }

    // 3) Primary source: Postgres fleet_models + fleet_workers (authoritative when daemon runs in
    //    postgres_full mode — fleet.toml [nodes] sections will be empty).
    if let Some(pool) = pg_pool {
        // Build a node-name → IP map from fleet_workers.
        let node_ips: HashMap<String, String> = ff_db::pg_list_nodes(pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|n| (n.name, n.ip))
            .collect();

        if let Ok(db_models) = ff_db::pg_list_models(pool).await {
            for m in db_models {
                if let Some(host) = node_ips.get(&m.worker_name) {
                    let port = m.port as u16;
                    let id = format!("{}:{}:{}", m.worker_name, m.slug, port);
                    if backends.iter().any(|b| b.id == id) {
                        continue;
                    }
                    let is_local = !m.slug.starts_with("gpt")
                        && !m.slug.starts_with("claude")
                        && !m.slug.starts_with("gemini");
                    backends.push(BackendEndpoint {
                        id,
                        node: m.worker_name.clone(),
                        host: host.clone(),
                        port,
                        model: m.slug.clone(),
                        tier: m.tier as u8,
                        healthy: true,
                        busy: false,
                        scheme: "http".to_string(),
                        is_local,
                        cost_per_1k_input: if is_local { 0.0 } else { 0.001 },
                        cost_per_1k_output: if is_local { 0.0 } else { 0.003 },
                    });
                    info!(
                        node = %m.worker_name,
                        model = %m.slug,
                        host,
                        port,
                        "registered backend from Postgres fleet_models"
                    );
                }
            }
        }
    }

    info!(backend_count = backends.len(), "built API backend registry");

    ApiConfig {
        host: "0.0.0.0".to_string(),
        port: config.fleet.api_port,
        backends,
    }
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(sig) => sig,
            Err(err) => {
                warn!(error = %err, "failed to register SIGTERM handler; using Ctrl+C only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_agent_config_defaults_to_heartbeat_mode() {
        let config: FleetConfig = toml::from_str("").expect("default config");
        let embedded = build_embedded_agent_config(&config, "taylor".to_string());

        assert!(!embedded.autonomous_mode);
        assert_eq!(embedded.poll_interval_secs, 8);
        assert_eq!(embedded.worker_name, "taylor");
    }

    #[test]
    fn embedded_agent_config_honors_agent_section() {
        let config: FleetConfig = toml::from_str(
            r#"
[agent]
autonomous_mode = true
poll_interval_secs = 3
ownership_api_base_url = "http://127.0.0.1:7777"
"#,
        )
        .expect("parse config");

        let embedded = build_embedded_agent_config(&config, "james".to_string());

        assert!(embedded.autonomous_mode);
        assert_eq!(embedded.poll_interval_secs, 3);
        assert_eq!(
            embedded.ownership_api_base_url.as_deref(),
            Some("http://127.0.0.1:7777")
        );
        assert_eq!(embedded.worker_name, "james");
    }

    #[test]
    fn postgres_full_preflight_passes_when_required_fields_exist() {
        let mut config: FleetConfig = toml::from_str("").expect("default config");
        config.database.mode = DatabaseMode::PostgresFull;
        config.database.url = "postgresql://forgefleet:secret@127.0.0.1:55432/forgefleet".into();
        config.database.cutover_evidence = Some("CHANGE-12345".into());

        let result = enforce_database_mode_preflight(&config);
        assert!(result.is_ok(), "unexpected preflight failure: {result:?}");
    }

    #[test]
    fn postgres_full_preflight_requires_cutover_evidence() {
        let mut config: FleetConfig = toml::from_str("").expect("default config");
        config.database.mode = DatabaseMode::PostgresFull;
        config.database.url = "postgresql://forgefleet:secret@127.0.0.1:55432/forgefleet".into();
        config.database.cutover_evidence = None;

        let result = enforce_database_mode_preflight(&config).expect_err("expected failure");
        let message = format!("{result:#}");
        assert!(message.contains("cutover_evidence"));
    }

    #[test]
    fn postgres_full_preflight_requires_database_url() {
        let mut config: FleetConfig = toml::from_str("").expect("default config");
        config.database.mode = DatabaseMode::PostgresFull;
        config.database.url = "   ".into();
        config.database.cutover_evidence = Some("CHANGE-12345".into());

        let result = enforce_database_mode_preflight(&config).expect_err("expected failure");
        let message = format!("{result:#}");
        assert!(message.contains("[database].url is empty"));
    }
}
