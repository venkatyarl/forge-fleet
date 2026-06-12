use crate::{CYAN, RESET};
use anyhow::Result;

pub async fn handle_brain(cmd: crate::BrainCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        crate::BrainCommand::Index {
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
        crate::BrainCommand::Communities => {
            println!("{CYAN}▶ Running community detection...{RESET}");
            let summary = ff_brain::detect_communities(&pool)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            println!("  communities: {}", summary.communities_found);
            println!("  largest:     {} nodes", summary.largest_community);
            println!(
                "  persisted:   {} registry rows",
                summary.communities_persisted
            );
        }
        crate::BrainCommand::Stats => {
            let node_count = ff_db::pg_count_brain_vault_nodes_current(&pool, None)
                .await
                .map_err(|e| anyhow::anyhow!("count nodes: {e}"))?;
            let total_edges: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM brain_vault_edges")
                .fetch_one(&pool)
                .await
                .unwrap_or(0);
            let communities: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM brain_communities")
                .fetch_one(&pool)
                .await
                .unwrap_or(0);
            println!("Vault graph stats:");
            println!("  nodes (current): {node_count}");
            println!("  edges:           {total_edges}");
            println!("  communities:     {communities}");
        }
        crate::BrainCommand::Corpus(cmd) => {
            crate::corpus_cmd::handle_corpus(&pool, cmd).await?;
        }
        crate::BrainCommand::Cortex(cmd) => {
            crate::cortex_cmd::handle_cortex(&pool, cmd).await?;
        }
        crate::BrainCommand::Callers {
            corpus,
            symbol,
            format,
        } => {
            crate::cortex_cmd::handle_cortex(
                &pool,
                crate::CortexCommand::Callers {
                    corpus,
                    symbol,
                    format,
                },
            )
            .await?;
        }
        crate::BrainCommand::Callees {
            corpus,
            symbol,
            format,
        } => {
            crate::cortex_cmd::handle_cortex(
                &pool,
                crate::CortexCommand::Callees {
                    corpus,
                    symbol,
                    format,
                },
            )
            .await?;
        }
        crate::BrainCommand::Impact {
            corpus,
            symbol,
            max_depth,
            format,
        } => {
            crate::cortex_cmd::handle_cortex(
                &pool,
                crate::CortexCommand::Impact {
                    corpus,
                    symbol,
                    max_depth,
                    format,
                },
            )
            .await?;
        }
        crate::BrainCommand::Query {
            org,
            entities,
            products,
            roles,
            statuses,
            modalities,
            facets,
            format,
        } => {
            crate::corpus_cmd::handle_query(
                &pool,
                &org,
                &entities,
                &products,
                &roles,
                &statuses,
                &modalities,
                &facets,
                &format,
            )
            .await?;
        }
    }
    Ok(())
}
