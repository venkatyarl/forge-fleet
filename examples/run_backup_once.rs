//! Manual one-shot trigger for the backup orchestrator.
//!
//! Usage:
//!   cargo run --release --example run_backup_once -- [postgres|redis|all] [--force]
//!
//! This exists because `ff fleet backup` lives in the ff-terminal crate,
//! which has unrelated in-progress changes that currently don't compile.
//! Once those land, `ff fleet backup --kind postgres --force` is the
//! primary entrypoint. Meanwhile, this example gives a clean way to
//! exercise BackupOrchestrator::run_once end-to-end against a live DB.

use sqlx::postgres::PgPoolOptions;
use sqlx::Row;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let kind = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "all".to_string());
    let force = args.iter().any(|a| a == "--force");

    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        "postgres://forgefleet:forgefleet@localhost:55432/forgefleet".to_string()
    });
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await?;

    let node_name = ff_agent::fleet_info::resolve_this_node_name().await;
    let computer_id: sqlx::types::Uuid =
        sqlx::query("SELECT id FROM computers WHERE name = $1")
            .bind(&node_name)
            .fetch_one(&pool)
            .await?
            .get("id");

    println!("▶ run_backup_once");
    println!("  node:        {node_name}");
    println!("  computer_id: {computer_id}");
    println!("  kind:        {kind}");
    println!("  force:       {force}");

    let orch = ff_agent::ha::backup::BackupOrchestrator::new(
        pool.clone(),
        computer_id,
        node_name,
        None,
    );

    let reports = orch.run_once(&kind, force).await?;

    for r in &reports {
        if !r.produced {
            println!("\n• {} skipped — not current leader (pass --force to override)", r.kind);
            continue;
        }
        println!("\n✓ {} backup produced", r.kind);
        println!("  file:        {}", r.file_name);
        println!("  path:        {}", r.file_path.display());
        println!("  size_bytes:  {}", r.size_bytes);
        println!("  sha256:      {}", r.sha256);
        println!("  backup_id:   {}", r.backup_id);
        println!("  distributed: {} peer(s)", r.distributed_to.len());
        for t in &r.distributed_to {
            println!("    → {t}");
        }
    }

    Ok(())
}
