use crate::{CYAN, GREEN, RESET, YELLOW};
use anyhow::Result;

pub async fn handle_openclaw(cmd: crate::OpenclawCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        crate::OpenclawCommand::Status { json } => {
            let rows: Vec<(
                String,
                String,
                Option<String>,
                Option<String>,
                Option<chrono::DateTime<chrono::Utc>>,
                Option<String>,
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
                "{:<14} {:<16} {:<8} {:<34} {:<22} OPENCLAW",
                "NAME", "IP", "MODE", "GATEWAY URL", "LAST RECONFIG"
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
        crate::OpenclawCommand::Devices { command } => {
            handle_openclaw_devices(&pool, command).await?;
        }
    }
    Ok(())
}

pub async fn handle_openclaw_devices(
    pool: &sqlx::PgPool,
    cmd: crate::OpenclawDevicesCommand,
) -> Result<()> {
    let local_name = ff_agent::fleet_info::resolve_this_worker_name().await;
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
        crate::OpenclawDevicesCommand::Export { stash } => {
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
            print!("{export}");
            if !export.ends_with('\n') {
                println!();
            }
        }
        crate::OpenclawDevicesCommand::Import { from_secret } => {
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

            if from_secret
                && let Err(e) = ff_agent::openclaw::clear_device_pairings_export(pool).await
            {
                eprintln!("{YELLOW}warning:{RESET} failed to clear stashed secret: {e}");
            }
        }
    }
    Ok(())
}
