use anyhow::Result;
use std::path::Path;
use crate::{GREEN, RESET, load_config};

pub async fn handle_config(cmd: crate::ConfigCommand, p: &Path) -> Result<()> {
    match cmd {
        crate::ConfigCommand::Show => {
            let c = load_config(p)?;
            println!("{}", toml::to_string_pretty(&c)?.trim_end());
            Ok(())
        }
        crate::ConfigCommand::Set { key, value } => {
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
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(p, toml::to_string_pretty(&c)?)?;
            println!("{GREEN}✓{RESET} {key}={value}");
            Ok(())
        }
        crate::ConfigCommand::Nodes => {
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
        crate::ConfigCommand::Node { name, key, value } => {
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
