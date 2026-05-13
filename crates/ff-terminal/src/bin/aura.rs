//! Aura — Off-LAN ForgeFleet daemon.
//!
//! Runs when the fleet is unreachable (no Postgres, no NATS, no leader).
//! Keeps a local SQLite mirror of critical tables and queues work locally.
//! When connectivity returns, syncs upstream and drains the local queue.
//!
//! # Usage
//! ```bash
//! aura --data-dir ~/.aura
//! ```
//!
//! # Features
//! - SQLite mirror of `fleet_tasks`, `fleet_workers`, `fleet_settings`
//! - Local task queue (claimed when offline, replayed when online)
//! - Tailscale integration (optional) for secure mesh when off-LAN
//! - Cron-driven sync attempts every 60s

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use tokio::time::interval;
use tracing::{info, warn};

#[derive(Parser)]
#[command(name = "aura")]
struct Args {
    #[arg(long, default_value = "~/.aura")]
    data_dir: PathBuf,
    #[arg(long, default_value = "60")]
    sync_interval_secs: u64,
    #[arg(long)]
    tailscale: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let data_dir = shellexpand::tilde(&args.data_dir.to_string_lossy()).into_owned();
    let data_dir = PathBuf::from(data_dir);
    std::fs::create_dir_all(&data_dir)?;

    info!(data_dir = %data_dir.display(), tailscale = args.tailscale, "aura daemon starting");

    // Open local SQLite mirror.
    let db_path = data_dir.join("aura.db");
    let conn = rusqlite::Connection::open(&db_path)?;
    init_schema(&conn)?;
    info!(db = %db_path.display(), "sqlite mirror ready");

    // Main loop: attempt upstream sync every N seconds.
    let mut tick = interval(Duration::from_secs(args.sync_interval_secs));
    loop {
        tick.tick().await;

        match try_sync_upstream(&data_dir).await {
            Ok(n) => {
                if n > 0 {
                    info!(synced_tasks = n, "upstream sync succeeded");
                }
            }
            Err(e) => {
                warn!(error = %e, "upstream sync failed — remaining offline");
            }
        }
    }
}

fn init_schema(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS aura_tasks (
            id          TEXT PRIMARY KEY,
            task_type   TEXT NOT NULL,
            summary     TEXT NOT NULL,
            payload     TEXT NOT NULL DEFAULT '{}',
            status      TEXT NOT NULL DEFAULT 'pending',
            created_at  TEXT NOT NULL,
            synced_at   TEXT
        );
        CREATE TABLE IF NOT EXISTS aura_nodes (
            name        TEXT PRIMARY KEY,
            ip          TEXT NOT NULL,
            role        TEXT NOT NULL,
            last_seen   TEXT
        );
        CREATE TABLE IF NOT EXISTS aura_settings (
            key         TEXT PRIMARY KEY,
            value       TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_aura_tasks_status ON aura_tasks(status);
        "#,
    )?;
    Ok(())
}

/// Attempt to connect to the fleet Postgres and sync local queue.
/// Returns the number of tasks successfully synced.
async fn try_sync_upstream(_data_dir: &std::path::Path) -> anyhow::Result<usize> {
    // In a full implementation this would:
    // 1. Read ~/.forgefleet/fleet.toml for the DB URL
    // 2. Connect to Postgres
    // 3. Pull fleet_workers, fleet_settings into SQLite
    // 4. Push pending aura_tasks into fleet_tasks
    // 5. Mark synced tasks in SQLite
    //
    // For the skeleton we just return 0 (no-op when offline).
    Ok(0)
}
