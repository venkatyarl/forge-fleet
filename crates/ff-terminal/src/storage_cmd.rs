use crate::{GREEN, RED, RESET, YELLOW, truncate_str};
use anyhow::Result;

pub async fn handle_storage(cmd: crate::StorageCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    let mgr = ff_agent::shared_storage::SharedStorageManager::new(pool.clone());

    match cmd {
        crate::StorageCommand::PeerMounts { command } => match command {
            crate::PeerMountCommand::Inventory => {
                let (recorded, failed) = mgr
                    .inventory_peer_mounts()
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                println!("{GREEN}✓ Inventoried peer mounts{RESET}");
                println!("  recorded: {recorded}");
                println!("  failed_nodes: {failed}");
                if failed > 0 {
                    eprintln!("{YELLOW}⚠ {failed} node(s) could not be scanned{RESET}");
                }
                Ok(())
            }
            crate::PeerMountCommand::List => {
                let rows = ff_db::pg_list_node_peer_mounts(&pool, None, None)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                if rows.is_empty() {
                    println!(
                        "(no peer mounts inventoried; run `ff storage peer-mounts inventory`)"
                    );
                    return Ok(());
                }
                println!(
                    "{:<14} {:<14} {:<24} {:<10} {}",
                    "COMPUTER", "PEER", "MOUNT", "FS", "OPTIONS"
                );
                for r in rows {
                    println!(
                        "{:<14} {:<14} {:<24} {:<10} {}",
                        truncate_str(r.computer_name.as_deref().unwrap_or("?"), 14),
                        truncate_str(&r.peer_name, 14),
                        truncate_str(&r.mount_path, 24),
                        truncate_str(&r.fs_type, 10),
                        r.mount_options.as_deref().unwrap_or("-"),
                    );
                }
                Ok(())
            }
        },
        crate::StorageCommand::Share { command } => match command {
            crate::StorageShareCommand::Create {
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
            crate::StorageShareCommand::Mount {
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
            crate::StorageShareCommand::Unmount { name, computer } => {
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
            crate::StorageShareCommand::List => {
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
