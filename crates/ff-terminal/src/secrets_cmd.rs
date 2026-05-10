use anyhow::Result;
use crate::{whoami_tag, RESET, YELLOW};

pub async fn handle_secrets(cmd: crate::SecretsCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;
    match cmd {
        crate::SecretsCommand::List => {
            let rows = ff_db::pg_list_secrets(&pool).await?;
            if rows.is_empty() {
                println!("(no secrets stored)");
                return Ok(());
            }
            println!(
                "{:<28} {:<14} {:<20} DESCRIPTION",
                "KEY", "UPDATED BY", "UPDATED AT"
            );
            for (key, desc, updated_by, updated_at) in rows {
                let ts = updated_at.format("%Y-%m-%d %H:%M UTC").to_string();
                println!(
                    "{:<28} {:<14} {:<20} {}",
                    key,
                    updated_by.unwrap_or_else(|| "-".into()),
                    ts,
                    desc.unwrap_or_default()
                );
            }
        }
        crate::SecretsCommand::Get { key } => match ff_db::pg_get_secret(&pool, &key).await? {
            Some(value) => println!("{value}"),
            None => {
                eprintln!("No secret set for key: {key}");
                std::process::exit(1);
            }
        },
        crate::SecretsCommand::Set {
            key,
            value,
            description,
        } => {
            let who = whoami_tag();
            ff_db::pg_set_secret(&pool, &key, &value, description.as_deref(), Some(&who)).await?;
            println!("Secret '{key}' stored ({} bytes) by {who}", value.len());
        }
        crate::SecretsCommand::Delete { key } => {
            let deleted = ff_db::pg_delete_secret(&pool, &key).await?;
            if deleted {
                println!("Deleted secret '{key}'");
            } else {
                println!("No secret with key '{key}' to delete");
            }
        }
        crate::SecretsCommand::Rotate { key, value } => {
            let rotator = ff_agent::secrets_rotation::SecretsRotator::new(pool.clone());
            match rotator.rotate(&key, value).await {
                Ok(out) => {
                    println!(
                        "Rotated '{}' ({} bytes, sha12={}, kind={})",
                        out.key, out.new_len, out.new_fingerprint, out.kind
                    );
                }
                Err(e) => {
                    eprintln!("Rotation failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        crate::SecretsCommand::Expirations => {
            let rotator = ff_agent::secrets_rotation::SecretsRotator::new(pool.clone());
            let report = rotator.check_expirations().await?;
            if report.near_expiry.is_empty() && report.already_expired.is_empty() {
                println!("(no secrets near expiry)");
                return Ok(());
            }
            println!("{:<30} {:>10} {:>5} EXPIRES_AT", "KEY", "DAYS_LEFT", "ROT#");
            for row in report
                .already_expired
                .iter()
                .chain(report.near_expiry.iter())
            {
                let exp = row
                    .expires_at
                    .map(|t| t.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| "-".into());
                let days = row
                    .days_remaining
                    .map(|d| d.to_string())
                    .unwrap_or_else(|| "-".into());
                println!(
                    "{:<30} {:>10} {:>5} {}",
                    row.key, days, row.rotation_count, exp,
                );
            }
            println!(
                "\n{} alert(s) dispatched. near_expiry={} expired={}",
                report.alerts_dispatched,
                report.near_expiry.len(),
                report.already_expired.len(),
            );
        }
        crate::SecretsCommand::DisableGate { key, hours, reason } => {
            if reason.trim().is_empty() {
                anyhow::bail!(
                    "--reason cannot be empty (the whole point of this verb is non-anonymous disables)"
                );
            }
            if hours == 0 {
                anyhow::bail!("--hours must be > 0 (zero would auto-restore immediately)");
            }
            let expires_at = chrono::Utc::now() + chrono::Duration::hours(hours as i64);
            let me = whoami_tag();
            ff_db::pg_disable_safety_gate(&pool, &key, &reason, expires_at, Some(&me)).await?;
            println!(
                "{YELLOW}!{RESET} {key} disabled until {} ({hours}h)\n  reason: {reason}\n  by:     {me}",
                expires_at.format("%Y-%m-%d %H:%M UTC"),
            );
            println!(
                "  After expiry, gate-check helpers (e.g. auto_upgrade_tick) auto-restore to the safe default.\n  To extend, re-run this verb with new --hours."
            );
        }
    }
    Ok(())
}
