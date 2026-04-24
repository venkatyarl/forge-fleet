use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use ff_api::config::ApiConfig;
use ff_api::registry::{BackendEndpoint, BackendRegistry};
use ff_control::{BootstrapOptions, ControlPlane};
use ff_core::config::{self, ConfigHandle, DatabaseMode, FleetConfig, spawn_watcher};
use ff_db::{
    DbPool, DbPoolConfig, OperationalStore, ReplicationBackupHelperAvailability,
    RuntimeRegistryStore, run_migrations,
};
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
use tokio::task::JoinHandle;
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

    /// Node name override for startup banner and telemetry tagging
    #[arg(long)]
    node_name: Option<String>,

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
    let command = cli
        .command
        .as_ref()
        .unwrap_or(&Command::Start(StartArgs {
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

    let node_name = resolve_node_name(cli, &config);
    let role = resolve_role(cli, start, &config, &node_name);

    // Publish node identity for in-process consumers (agent, MCP tools, callbacks).
    // SAFETY: single-threaded at this point — daemon subsystems haven't spawned yet.
    #[allow(unused_unsafe)]
    unsafe { std::env::set_var("FORGEFLEET_NODE_NAME", &node_name); }

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
    unsafe { std::env::set_var("FORGEFLEET_NATS_URL", &nats_url); }
    let nats_init_outcome: Result<(), String> = ff_agent::nats_client::init_nats(&nats_url)
        .await
        .map_err(|e| e.to_string());

    init_logging(cli, &node_name).await?;
    match &nats_init_outcome {
        Ok(_) => info!(url = %nats_url, "NATS connected"),
        Err(e) => warn!(url = %nats_url, error = %e, "NATS unavailable — continuing without event bus"),
    }
    print_startup_banner(&node_name, &role, &config_path);

    enforce_database_mode_preflight(&config)?;

    // ─── Operational persistence backend (SQLite or Postgres) ──────────────
    let (operational_store, sqlite_pool, sqlite_path) =
        initialize_operational_store(&config, &config_path).await?;

    // ─── Runtime registry persistence backend ────────────────────────────────
    let runtime_registry = initialize_runtime_registry(&config, sqlite_pool.clone()).await?;
    log_database_mode_summary(
        &config,
        sqlite_path.as_deref(),
        &operational_store,
        &runtime_registry,
    );

    // ─── Postgres fleet config seed (fleet.toml → Postgres, first boot only) ──
    if config.database.mode != DatabaseMode::EmbeddedSqlite {
        if let Some(pg_pool) = operational_store.pg_pool() {
            ff_db::run_postgres_migrations(pg_pool)
                .await
                .context("postgres fleet-config migrations failed")?;

            // Only seed if Postgres fleet_nodes table is empty (first boot)
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
    }

    // ─── Config hot-reload handle ────────────────────────────────────────────
    let (config_handle, config_tx) = ConfigHandle::new(config.clone(), config_path.clone());
    let config_watcher = spawn_watcher(config_handle, config_tx);

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
    if let Some(pg_pool) = operational_store.pg_pool() {
        if let Ok(db_nodes) = ff_db::pg_list_nodes(pg_pool).await {
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
    }

    // 1) discovery — fleet node scanning + subnet scanning
    info!("starting subsystem: discovery");
    let scan_targets = build_fleet_scan_targets(&config);
    subsystem_tasks.push(start_discovery_subsystem(
        control_plane.handles.discovery.scanner_config.clone(),
        scan_targets,
        registry.clone(),
        shutdown_rx.clone(),
    ));

    // 2) leader election — periodic election using discovery data
    info!("starting subsystem: leader election");
    let election_config = Arc::new(config.clone());
    subsystem_tasks.push(start_leader_election_subsystem(
        election_config,
        node_name.clone(),
        registry.clone(),
        shutdown_rx.clone(),
    ));

    // 3) api proxy — build shared backend registry from config + Postgres
    info!("starting subsystem: api proxy");
    let api_config = build_api_config(&config, operational_store.pg_pool()).await;
    let backend_registry = std::sync::Arc::new(BackendRegistry::new(api_config.backends.clone()));
    subsystem_tasks.push(start_api_proxy_subsystem(api_config));

    // 4) agent
    info!("starting subsystem: agent");
    let mut embedded_agent_config = build_embedded_agent_config(&config, node_name.clone());
    // Wire in the inference router so autonomous LLM tasks use local-first fleet routing.
    let inference_router = Arc::new(
        ff_agent::inference_router::InferenceRouter::from_config(&config_path).await,
    );
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
    let mc_db_path = match config.database.mode {
        DatabaseMode::EmbeddedSqlite => config_path
            .parent()
            .map(|p| p.join("mission-control.db").to_string_lossy().to_string()),
        DatabaseMode::PostgresRuntime | DatabaseMode::PostgresFull => None,
    };
    subsystem_tasks.push(start_gateway_subsystem(
        config.clone(),
        config_path.to_string_lossy().to_string(),
        backend_registry.clone(),
        registry.clone(),
        operational_store.clone(),
        runtime_registry.clone(),
        mc_db_path,
    ));

    // 7) telegram polling transport (bidirectional control channel)
    if config
        .transport
        .telegram
        .as_ref()
        .is_some_and(|telegram| telegram.enabled)
    {
        // Fallback: if the token env var / inline config is empty, pull the
        // bot token from fleet_secrets (`telegram.bot_token`) and export it
        // via the configured env var so resolve_bot_token() finds it. Keeps
        // secrets out of shell rc files and launchd plists.
        if let Some(tg) = config.transport.telegram.as_ref() {
            if tg.resolve_bot_token().is_none() {
                if let Some(pg_pool) = operational_store.pg_pool() {
                    match ff_db::pg_get_secret(pg_pool, "telegram.bot_token").await {
                        Ok(Some(token)) if !token.trim().is_empty() => {
                            let key = if tg.bot_token_env.trim().is_empty() {
                                "FORGEFLEET_TELEGRAM_BOT_TOKEN"
                            } else {
                                tg.bot_token_env.as_str()
                            };
                            unsafe { std::env::set_var(key, token.trim()); }
                            info!("telegram bot token loaded from fleet_secrets");
                        }
                        Ok(_) => info!("telegram bot token absent in fleet_secrets"),
                        Err(e) => error!(error = %e, "fleet_secrets lookup failed"),
                    }
                }
            }
        }
        info!("starting subsystem: telegram transport");
        subsystem_tasks.push(start_telegram_transport_subsystem(
            config.clone(),
            operational_store.clone(),
            node_name.clone(),
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
            node_name.clone(),
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
            node_name.clone(),
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
        let pulse_node = node_name.clone();
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
                node_name.clone(),
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

fn postgres_full_sqlite_blockers(config: &FleetConfig) -> Vec<String> {
    let mut blockers = Vec::new();

    let helper_availability =
        ReplicationBackupHelperAvailability::for_database_mode(&config.database.mode);
    if helper_availability.is_enabled() {
        blockers.push(
            "ff-db replication/backup helpers are scoped to embedded_sqlite mode only".to_string(),
        );
    }

    blockers
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

fn resolve_embedded_db_path(config: &FleetConfig, config_path: &Path) -> Result<PathBuf> {
    if let Some(raw) = config
        .database
        .sqlite_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let candidate = PathBuf::from(raw);
        if candidate.is_absolute() {
            return Ok(candidate);
        }

        let Some(parent) = config_path.parent() else {
            anyhow::bail!("unable to resolve config parent directory for sqlite path");
        };
        return Ok(parent.join(candidate));
    }

    let Some(parent) = config_path.parent() else {
        anyhow::bail!("unable to resolve config parent directory for sqlite path");
    };

    Ok(parent.join("forgefleet.db"))
}

async fn initialize_operational_store(
    config: &FleetConfig,
    config_path: &Path,
) -> Result<(OperationalStore, Option<DbPool>, Option<PathBuf>)> {
    match config.database.mode {
        DatabaseMode::EmbeddedSqlite => {
            let db_path = resolve_embedded_db_path(config, config_path)?;
            let pool = DbPool::open(DbPoolConfig::with_path(&db_path)).with_context(|| {
                format!("failed to open embedded sqlite at {}", db_path.display())
            })?;

            let conn = pool
                .open_raw_connection()
                .context("failed to open sqlite migration connection")?;
            let applied = run_migrations(&conn).context("database migration failed")?;
            info!(path = %db_path.display(), applied, "embedded sqlite ready");

            Ok((
                OperationalStore::sqlite(pool.clone()),
                Some(pool),
                Some(db_path),
            ))
        }
        DatabaseMode::PostgresRuntime | DatabaseMode::PostgresFull => {
            let database_url = config.database.url.trim();
            if database_url.is_empty() {
                anyhow::bail!(
                    "database.mode={} requires non-empty [database].url",
                    config.database.mode.as_str()
                );
            }

            let store = OperationalStore::postgres(database_url, config.database.max_connections)
                .await
                .with_context(|| {
                    format!(
                        "failed to initialize Postgres operational store ({})",
                        redact_database_url(database_url)
                    )
                })?;

            Ok((store, None, None))
        }
    }
}

async fn initialize_runtime_registry(
    config: &FleetConfig,
    sqlite_pool: Option<DbPool>,
) -> Result<RuntimeRegistryStore> {
    match config.database.mode {
        DatabaseMode::EmbeddedSqlite => {
            let Some(pool) = sqlite_pool else {
                anyhow::bail!(
                    "database.mode=embedded_sqlite requires initialized embedded sqlite pool"
                );
            };
            Ok(RuntimeRegistryStore::sqlite(pool))
        }
        DatabaseMode::PostgresRuntime | DatabaseMode::PostgresFull => {
            let database_url = config.database.url.trim();
            if database_url.is_empty() {
                anyhow::bail!(
                    "database.mode={} requires non-empty [database].url",
                    config.database.mode.as_str()
                );
            }

            RuntimeRegistryStore::postgres(database_url, config.database.max_connections)
                .await
                .with_context(|| {
                    format!(
                        "failed to initialize Postgres runtime registry ({})",
                        redact_database_url(database_url)
                    )
                })
        }
    }
}

fn log_database_mode_summary(
    config: &FleetConfig,
    sqlite_path: Option<&Path>,
    operational_store: &OperationalStore,
    runtime_registry: &RuntimeRegistryStore,
) {
    let replication_backup_helpers =
        ReplicationBackupHelperAvailability::for_database_mode(&config.database.mode);

    match config.database.mode {
        DatabaseMode::EmbeddedSqlite => {
            let path_display = sqlite_path
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<unknown>".to_string());
            info!(
                mode = "embedded_sqlite",
                sqlite_path = %path_display,
                operational_store = operational_store.backend_label(),
                runtime_registry = runtime_registry.backend_label(),
                replication_backup_helpers = replication_backup_helpers.summary(),
                "database mode active"
            );
        }
        DatabaseMode::PostgresRuntime => {
            if let Some(path) = sqlite_path {
                warn!(
                    sqlite_path = %path.display(),
                    "sqlite path still configured but ignored in postgres_runtime mode"
                );
            }

            info!(
                mode = "postgres_runtime",
                postgres_url = %redact_database_url(&config.database.url),
                operational_store = operational_store.backend_label(),
                runtime_registry = runtime_registry.backend_label(),
                replication_backup_helpers = replication_backup_helpers.summary(),
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
                replication_backup_helpers = replication_backup_helpers.summary(),
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

    if let Some((scheme, rest)) = trimmed.split_once("://") {
        if let Some((userinfo, host_part)) = rest.split_once('@') {
            let redacted_userinfo = if let Some((user, _)) = userinfo.split_once(':') {
                format!("{user}:***")
            } else {
                "***".to_string()
            };
            return format!("{scheme}://{redacted_userinfo}@{host_part}");
        }
    }

    "***".to_string()
}

async fn init_logging(cli: &Cli, node_name: &str) -> Result<()> {
    let telemetry = TelemetryConfig {
        level: cli.log_level.clone(),
        json: cli.json_logs,
        node_name: Some(node_name.to_string()),
        ..Default::default()
    };

    // If the process-global NATS client is available, attach a
    // NatsLogLayer so every tracing event is mirrored onto
    // `logs.<node>.forgefleetd.<level>`. Otherwise fall back to the
    // plain file + stdout subscriber.
    if let Some(nats_client) = ff_agent::nats_client::get_nats().await {
        let nats_layer = ff_agent::nats_log_layer::NatsLogLayer::with_client(
            nats_client.clone(),
            node_name.to_string(),
            "forgefleetd".to_string(),
        );
        ff_observability::init_telemetry_with_extra_layer(&telemetry, nats_layer)
    } else {
        init_telemetry(&telemetry)
    }
}

fn resolve_node_name(cli: &Cli, config: &FleetConfig) -> String {
    if let Some(node_name) = &cli.node_name {
        return node_name.clone();
    }

    // FORGEFLEET_NODE_NAME — the canonical identity override. Mirrors
    // ff_agent::fleet_info::resolve_this_node_name (priority 1). The DGX
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

fn resolve_role(cli: &Cli, start: &StartArgs, config: &FleetConfig, node_name: &str) -> String {
    if let Some(role) = &cli.role {
        return role.clone();
    }

    if start.leader {
        return "leader".to_string();
    }

    config
        .nodes
        .get(node_name)
        .map(|n| format!("{:?}", n.role).to_ascii_lowercase())
        .unwrap_or_else(|| "auto".to_string())
}

fn print_startup_banner(node_name: &str, role: &str, config_path: &Path) {
    println!(
        "\nForgeFleet v{}\n  node: {}\n  role: {}\n  config: {}\n",
        env!("CARGO_PKG_VERSION"),
        node_name,
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

// ─── Build scan targets from fleet.toml ──────────────────────────────────────

fn build_fleet_scan_targets(config: &FleetConfig) -> Vec<ScanTarget> {
    build_scan_targets(
        config
            .nodes
            .iter()
            .map(|(name, node)| (name.as_str(), node.ip.as_str(), node.port, node.priority())),
        config.fleet.api_port,
    )
}

// ─── Discovery subsystem ─────────────────────────────────────────────────────

fn start_discovery_subsystem(
    scanner_config: ScannerConfig,
    scan_targets: Vec<ScanTarget>,
    registry: Arc<NodeRegistry>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        let node_scanner = NodeScanner::new(scan_targets);

        // Stale threshold: 90 seconds without heartbeat.
        let stale_threshold_secs: i64 = 90;

        loop {
            tokio::select! {
                _ = ticker.tick() => {
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

                    // 3) Mark stale nodes.
                    let stale = registry.mark_stale_nodes(stale_threshold_secs);
                    if !stale.is_empty() {
                        warn!(stale_nodes = ?stale, "marked nodes as stale (no heartbeat > 90s)");
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

// ─── Leader election subsystem ───────────────────────────────────────────────

/// Announces this node as leader to all known fleet nodes via HTTP POST.
async fn announce_leader_to_fleet(
    leader_name: &str,
    config: &FleetConfig,
    registry: &NodeRegistry,
) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let payload = serde_json::json!({
        "leader": leader_name,
        "announced_at": chrono::Utc::now().to_rfc3339(),
    });

    // Announce to all config-sourced nodes (except ourselves).
    for (node_name, node_cfg) in &config.nodes {
        if node_name == leader_name {
            continue;
        }

        let port = node_cfg.port.unwrap_or(config.fleet.api_port);
        let url = format!("http://{}:{}/api/fleet/leader", node_cfg.ip, port);

        match client.post(&url).json(&payload).send().await {
            Ok(resp) if resp.status().is_success() => {
                info!(
                    target_node = %node_name,
                    "leader announcement accepted"
                );
            }
            Ok(resp) => {
                warn!(
                    target_node = %node_name,
                    status = resp.status().as_u16(),
                    "leader announcement rejected"
                );
            }
            Err(err) => {
                warn!(
                    target_node = %node_name,
                    error = %err,
                    "leader announcement failed (node may be offline)"
                );
            }
        }
    }

    // Record the leader in the registry.
    registry.set_leader(leader_name.to_string());
}

fn start_leader_election_subsystem(
    config: Arc<FleetConfig>,
    _node_name: String,
    registry: Arc<NodeRegistry>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        use ff_core::leader::{check_failover, elect_leader};

        let election_interval = Duration::from_secs(config.leader.election_interval_secs.max(5));
        let mut ticker = tokio::time::interval(election_interval);
        let mut current_leader: Option<String> = None;
        let mut initial_election_done = false;

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    // Build health tuples from registry.
                    let health = registry.node_health_for_election();

                    if health.is_empty() {
                        // No configured nodes in registry — skip election.
                        continue;
                    }

                    if !initial_election_done {
                        // Run a full initial election.
                        let result = elect_leader(&config, &health);
                        if let Some(ref elected) = result.elected {
                            info!(
                                leader = %elected,
                                reason = %result.reason,
                                "initial leader election completed"
                            );
                            current_leader = Some(elected.clone());
                            announce_leader_to_fleet(elected, &config, &registry).await;
                        } else {
                            warn!(
                                reason = %result.reason,
                                "initial election: no leader could be elected"
                            );
                        }
                        initial_election_done = true;
                        continue;
                    }

                    // Periodic election check — failover or preferred-return.
                    if let Some(ref leader) = current_leader {
                        if let Some(result) = check_failover(leader, &config, &health) {
                            if let Some(ref new_leader) = result.elected {
                                if new_leader != leader {
                                    info!(
                                        old_leader = %leader,
                                        new_leader = %new_leader,
                                        reason = %result.reason,
                                        "leader change detected"
                                    );
                                    current_leader = Some(new_leader.clone());
                                    announce_leader_to_fleet(new_leader, &config, &registry).await;
                                }
                            }
                        }
                    } else {
                        // No current leader — try to elect one.
                        let result = elect_leader(&config, &health);
                        if let Some(ref elected) = result.elected {
                            info!(
                                leader = %elected,
                                reason = %result.reason,
                                "leader elected (was none)"
                            );
                            current_leader = Some(elected.clone());
                            announce_leader_to_fleet(elected, &config, &registry).await;
                        }
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        info!("leader election subsystem stopping");
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
    mc_db_path: Option<String>,
) -> JoinHandle<()> {
    let gateway_config = GatewayConfig {
        bind_addr: format!("0.0.0.0:{}", config.fleet.api_port.saturating_add(2)), // Web UI on api_port + 2 (51002)
        fleet_config: Some(config.clone()),
        config_path: Some(config_path),
        backend_registry: Some(backend_registry),
        discovery_registry: Some(discovery_registry),
        mc_db_path,
        operational_store: Some(operational_store),
        runtime_registry: Some(runtime_registry),
        ..GatewayConfig::default()
    };

    tokio::spawn(async move {
        if let Err(err) = ff_gateway::run(gateway_config).await {
            error!(error = %err, "gateway subsystem exited with error");
        }
    })
}

fn start_telegram_transport_subsystem(
    config: FleetConfig,
    operational_store: OperationalStore,
    node_name: String,
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
            node_name,
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
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let loop_cfg = config.loops.evolution.clone();
        let mut engine = EvolutionEngine::default();
        engine.verifier = VerificationModel::new(loop_cfg.minimum_improvement_ratio);

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
    node_name: String,
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
                                    build_updater_orchestrator_config(&node_name, &repo_path, &loop_cfg),
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
    node_name: &str,
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
        node_name: node_name.to_string(),
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

fn expected_model_ports_for_node(config: &FleetConfig, node_name: &str) -> Vec<u16> {
    let mut ports = HashSet::new();

    if let Some(node_cfg) = config.nodes.get(node_name) {
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
    node_name: String,
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

        let expected_ports = expected_model_ports_for_node(&config, &node_name);
        if expected_ports.is_empty() {
            info!(node = %node_name, "self-heal loop started with no expected local model ports");
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
                    let fleet_nodes: Vec<(String, String, String)> = {
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
                                if db_node.name == node_name {
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

                    let http = reqwest::Client::builder()
                        .timeout(Duration::from_secs(5))
                        .build()
                        .unwrap_or_default();

                    let mut fleet_healthy = 0u32;
                    let mut fleet_issues = Vec::new();

                    for (name, ip, restart_cmd) in &fleet_nodes {
                        let url = format!("http://{}:55000/health", ip);
                        match http.get(&url).send().await {
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
                            total = fleet_nodes.len(),
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
//   1) Look up this computer's `id` and `fleet_members.election_priority`.
//      If the host isn't enrolled, log warn and return Ok(empty) — v2 stays
//      disabled for that host until enrollment.
//   2) Start HeartbeatV2Publisher unconditionally when the computer row
//      exists (it writes only its own `pulse:computer:{name}` key).
//   3) Start the Materializer on every daemon (see NOTE at call-site), and
//      start LeaderTick only if the computer is enrolled in `fleet_members`.
async fn start_pulse_v2_subsystems(
    pg_pool: ff_db::PgPool,
    redis_url: String,
    node_name: String,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<Vec<JoinHandle<()>>> {
    let mut handles: Vec<JoinHandle<()>> = Vec::new();

    // (1a) computer_id — required for everything else.
    let computer_id_row: Option<(uuid::Uuid,)> =
        sqlx::query_as::<_, (uuid::Uuid,)>("SELECT id FROM computers WHERE name = $1")
            .bind(&node_name)
            .fetch_optional(&pg_pool)
            .await
            .context("pulse v2: failed to query computers by name")?;

    let Some((computer_id,)) = computer_id_row else {
        warn!(
            node = %node_name,
            "pulse v2: no `computers` row for this host; Pulse v2 disabled until enrollment"
        );
        return Ok(handles);
    };

    // (1b) election_priority — optional; fleet_members may not exist yet.
    let priority_row: Option<(i32,)> = sqlx::query_as::<_, (i32,)>(
        "SELECT election_priority FROM fleet_members WHERE computer_id = $1",
    )
    .bind(computer_id)
    .fetch_optional(&pg_pool)
    .await
    .context("pulse v2: failed to query fleet_members")?;
    let enrolled_in_fleet = priority_row.is_some();
    let election_priority = priority_row.map(|(p,)| p).unwrap_or(1000);

    // Clone pg_pool + shutdown_rx for the backup orchestrator *before*
    // the LeaderTick branch below consumes its copy (Phase 6 HA).
    let pg_pool_for_backup = pg_pool.clone();
    let shutdown_rx_for_backup = shutdown_rx.clone();

    // Additional clones for Phase 10 observability subsystems (metrics
    // downsampler + alert evaluator). They start after leader_tick and also
    // need their own PulseReaders, built from `redis_url`.
    let pg_pool_for_metrics = pg_pool.clone();
    let pg_pool_for_alerts = pg_pool.clone();
    let shutdown_rx_for_metrics = shutdown_rx.clone();
    let shutdown_rx_for_alerts = shutdown_rx.clone();
    let redis_url_for_metrics = redis_url.clone();
    let redis_url_for_alerts = redis_url.clone();
    let node_name_for_metrics = node_name.clone();
    let node_name_for_alerts = node_name.clone();

    // Build the redis::Client once — both publisher and materializer need one.
    let redis_client = redis::Client::open(redis_url.as_str())
        .context("pulse v2: failed to open redis client")?;

    // (2) HeartbeatV2Publisher — always runs when computer row exists.
    info!(
        node = %node_name,
        computer_id = %computer_id,
        election_priority,
        "starting subsystem: pulse v2 heartbeat"
    );
    let v2_pub = ff_pulse::HeartbeatV2Publisher::with_defaults(
        redis_client.clone(),
        node_name.clone(),
        election_priority,
    );
    // epoch_handle + role_handle are shared with leader_tick below when
    // available; for now we just spawn the publisher.
    let _epoch_handle = v2_pub.epoch_handle();
    let _role_handle = v2_pub.role_handle();
    handles.push(v2_pub.spawn(shutdown_rx.clone()));

    // HMAC key refresher — publishers sign beats with this key, materializer
    // + reader verify against the same cache. Refresh every 5 minutes.
    // If no key is configured we publish unsigned (rollout compat).
    {
        let cache = ff_pulse::pulse_hmac::KeyCache::global().clone();
        handles.push(cache.spawn_refresher(
            pg_pool.clone(),
            std::time::Duration::from_secs(300),
        ));
    }

    // (3) Materializer — runs on every daemon for this phase. See NOTE
    // at call-site in run_daemon.
    info!("starting subsystem: pulse v2 materializer");
    let materializer = ff_pulse::materializer::Materializer::new(pg_pool.clone(), redis_client.clone());
    handles.push(materializer.spawn(shutdown_rx.clone()));

    // (4) LeaderTick — only when enrolled in fleet_members.
    if enrolled_in_fleet {
        info!(
            node = %node_name,
            election_priority,
            "starting subsystem: pulse v2 leader_tick"
        );
        let pulse_reader = ff_pulse::reader::PulseReader::new(&redis_url)
            .context("pulse v2: failed to build PulseReader for leader_tick")?;

        // Resolve my primary IP from the computers table — OpenClawManager
        // needs it to publish the gateway URL on promotion.
        let my_primary_ip: String =
            sqlx::query_scalar("SELECT primary_ip FROM computers WHERE id = $1")
                .bind(computer_id)
                .fetch_one(&pg_pool)
                .await
                .unwrap_or_else(|_| "127.0.0.1".to_string());

        // Build the OpenClaw manager that will be driven by leader-election
        // callbacks (promote_to_gateway on became-leader, demote_to_node on
        // lost-leader).
        let openclaw = std::sync::Arc::new(ff_agent::openclaw::OpenClawManager::new(
            pg_pool.clone(),
            computer_id,
            my_primary_ip,
        ));

        let oc_promote = openclaw.clone();
        let oc_demote = openclaw.clone();
        let pool_for_url = pg_pool.clone();
        let pool_for_promote = pg_pool.clone();
        let pool_for_demote = pg_pool.clone();

        let my_name_for_promote = node_name.clone();
        let my_name_for_demote = node_name.clone();

        let on_became: ff_agent::leader_tick::OnBecameLeader = std::sync::Arc::new(move |prev: Option<String>| {
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
        });

        let on_lost: ff_agent::leader_tick::OnLostLeader =
            std::sync::Arc::new(move |new_leader_name: String| {
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
                        tracing::warn!(
                            "openclaw: lost leader but no gateway URL published yet"
                        );
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
            });

        // Phase 6 HA — auto Postgres failover manager. Runs inside every
        // leader tick (only when we are the current fleet leader). The
        // whole path is no-op'd by the env var
        // FORGEFLEET_DISABLE_AUTO_PG_FAILOVER=true for safety drills.
        let pg_failover_manager = std::sync::Arc::new(
            ff_agent::ha::pg_failover::PostgresFailoverManager::new(
                pg_pool.clone(),
                computer_id,
            ),
        );
        info!(
            node = %node_name,
            computer_id = %computer_id,
            disabled = ff_agent::ha::pg_failover::DISABLE_ENV,
            "pg_failover manager constructed (auto-failover enabled unless {} is set)",
            ff_agent::ha::pg_failover::DISABLE_ENV
        );

        let leader_tick = ff_agent::leader_tick::LeaderTick::new(
            pg_pool,
            pulse_reader,
            computer_id,
            node_name.clone(),
            election_priority,
        )
        .with_on_became_leader(on_became)
        .with_on_lost_leader(on_lost)
        .with_pg_failover(pg_failover_manager);
        handles.push(leader_tick.spawn(15, shutdown_rx));
    } else {
        info!(
            node = %node_name,
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
        node = %node_name,
        computer_id = %computer_id,
        "starting subsystem: backup orchestrator (pg=4h, redis=2h)"
    );
    let backup = ff_agent::ha::backup::BackupOrchestrator::new(
        pg_pool_for_backup,
        computer_id,
        node_name.clone(),
        None,
    );
    handles.push(backup.spawn(shutdown_rx_for_backup));

    // (6) Phase 10 — metrics downsampler. Each tick is gated internally on
    // leadership via `fleet_leader_state`, so we start it on every daemon and
    // it no-ops on followers.
    match ff_pulse::reader::PulseReader::new(&redis_url_for_metrics) {
        Ok(metrics_reader) => {
            info!(
                node = %node_name_for_metrics,
                "starting subsystem: metrics downsampler (60s, leader-gated)"
            );
            let dsamp = ff_agent::metrics_downsampler::MetricsDownsampler::new(
                pg_pool_for_metrics,
                metrics_reader,
                node_name_for_metrics.clone(),
            );
            handles.push(dsamp.spawn(shutdown_rx_for_metrics));
        }
        Err(e) => warn!(error = %e, "metrics downsampler: failed to build PulseReader"),
    }

    // (7) Phase 10 — alert evaluator. Also leader-gated internally.
    match ff_pulse::reader::PulseReader::new(&redis_url_for_alerts) {
        Ok(alert_reader) => {
            info!(
                node = %node_name_for_alerts,
                "starting subsystem: alert evaluator (60s, leader-gated)"
            );
            let evaluator = ff_agent::alert_evaluator::AlertEvaluator::new(
                pg_pool_for_alerts,
                alert_reader,
                node_name_for_alerts.clone(),
            );
            handles.push(evaluator.spawn(shutdown_rx_for_alerts));
        }
        Err(e) => warn!(error = %e, "alert evaluator: failed to build PulseReader"),
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
    node_name: String,
) -> ff_agent::EmbeddedAgentConfig {
    ff_agent::EmbeddedAgentConfig {
        node_name,
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
    for (node_name, node_cfg) in &config.nodes {
        for (model_slug, model_cfg) in &node_cfg.models {
            let port = model_cfg.port.unwrap_or(config.fleet.api_port);
            backends.push(BackendEndpoint {
                id: format!("{}:{}:{}", node_name, model_slug, port),
                node: node_name.clone(),
                host: node_cfg.ip.clone(),
                port,
                model: model_slug.clone(),
                tier: model_cfg.tier as u8,
                healthy: true,
                busy: false,
                scheme: "http".to_string(),
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
        for node_name in &model.nodes {
            let (host, port) = node_by_name
                .get(node_name.as_str())
                .copied()
                .unwrap_or(("127.0.0.1", config.fleet.api_port));

            let id = format!("{}:{}:{}", node_name, model.id, port);
            if backends.iter().any(|b| b.id == id) {
                continue;
            }

            backends.push(BackendEndpoint {
                id,
                node: node_name.clone(),
                host: host.to_string(),
                port,
                model: model.id.clone(),
                tier: model.tier.as_u8(),
                healthy: true,
                busy: false,
                scheme: "http".to_string(),
            });
        }
    }

    // 3) Primary source: Postgres fleet_models + fleet_nodes (authoritative when daemon runs in
    //    postgres_full mode — fleet.toml [nodes] sections will be empty).
    if let Some(pool) = pg_pool {
        // Build a node-name → IP map from fleet_nodes.
        let node_ips: HashMap<String, String> = ff_db::pg_list_nodes(pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|n| (n.name, n.ip))
            .collect();

        if let Ok(db_models) = ff_db::pg_list_models(pool).await {
            for m in db_models {
                if let Some(host) = node_ips.get(&m.node_name) {
                    let port = m.port as u16;
                    let id = format!("{}:{}:{}", m.node_name, m.slug, port);
                    if backends.iter().any(|b| b.id == id) {
                        continue;
                    }
                    backends.push(BackendEndpoint {
                        id,
                        node: m.node_name.clone(),
                        host: host.clone(),
                        port,
                        model: m.slug.clone(),
                        tier: m.tier as u8,
                        healthy: true,
                        busy: false,
                        scheme: "http".to_string(),
                    });
                    info!(
                        node = %m.node_name,
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
        assert_eq!(embedded.node_name, "taylor");
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
        assert_eq!(embedded.node_name, "james");
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
