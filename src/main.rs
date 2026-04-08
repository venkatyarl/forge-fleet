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

#[derive(Debug, Parser)]
#[command(name = "forgefleet", version, about = "ForgeFleet unified daemon")]
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let command = cli
        .command
        .as_ref()
        .unwrap_or(&Command::Start(StartArgs { leader: false }));

    match command {
        Command::Start(args) => run_daemon(&cli, args).await,
        Command::Status => run_status(&cli),
        Command::Version => {
            println!("forgefleet {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

async fn run_daemon(cli: &Cli, start: &StartArgs) -> Result<()> {
    let config_path = resolve_config_path(cli.config.clone())?;
    let config = load_or_default_config(&config_path)?;

    let node_name = resolve_node_name(cli, &config);
    let role = resolve_role(cli, start, &config, &node_name);

    init_logging(cli, &node_name)?;
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

    // ─── Pre-seed registry from fleet.toml ───────────────────────────────────
    let registry = control_plane.handles.discovery.registry.clone();
    seed_registry_from_config(&config, &registry);

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

    // 3) api proxy — build shared backend registry from config
    info!("starting subsystem: api proxy");
    let api_config = build_api_config(&config);
    let backend_registry = std::sync::Arc::new(BackendRegistry::new(api_config.backends.clone()));
    subsystem_tasks.push(start_api_proxy_subsystem(api_config));

    // 4) agent
    info!("starting subsystem: agent");
    let embedded_agent_config = build_embedded_agent_config(&config, node_name.clone());
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

fn init_logging(cli: &Cli, node_name: &str) -> Result<()> {
    let telemetry = TelemetryConfig {
        level: cli.log_level.clone(),
        json: cli.json_logs,
        node_name: Some(node_name.to_string()),
        ..Default::default()
    };

    init_telemetry(&telemetry)
}

fn resolve_node_name(cli: &Cli, config: &FleetConfig) -> String {
    if let Some(node_name) = &cli.node_name {
        return node_name.clone();
    }

    if let Ok(hostname) = std::env::var("HOSTNAME")
        && !hostname.trim().is_empty()
    {
        return hostname;
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
    }
}

fn build_api_config(config: &FleetConfig) -> ApiConfig {
    let mut backends = Vec::new();

    // 1) Node-level model mapping (primary source)
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
