//! `ff brain` subcommand implementation.

use anyhow::Result;

use crate::{CYAN, RESET};

pub async fn handle_brain(cmd: ff_terminal::BrainCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        ff_terminal::BrainCommand::Index {
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
        ff_terminal::BrainCommand::Communities => {
            println!("{CYAN}▶ Running community detection...{RESET}");
            let summary = ff_brain::detect_communities(&pool)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            println!("  communities: {}", summary.communities_found);
            println!("  largest:     {} nodes", summary.largest_community);
        }
        ff_terminal::BrainCommand::Stats => {
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
