use crate::{RESET, YELLOW, whoami_tag};
use anyhow::Result;

/// Lossless JSON projection of one `fleet_secrets` metadata row for
/// `ff secrets list --json`. Pure (no DB/clock). The secret VALUE is
/// deliberately absent — `pg_list_secrets` never fetches it, and `ff secrets
/// get <key>` remains the only path that prints a value. RFC3339 `updated_at`
/// (the table renders a coarser `%Y-%m-%d %H:%M`); nullable description /
/// updated_by are JSON null, not omitted, so the shape is stable.
fn secret_list_json_row(
    key: &str,
    description: Option<&str>,
    updated_by: Option<&str>,
    updated_at: &chrono::DateTime<chrono::Utc>,
) -> serde_json::Value {
    serde_json::json!({
        "key": key,
        "description": description,
        "updated_by": updated_by,
        "updated_at": updated_at.to_rfc3339(),
    })
}

pub async fn handle_secrets(cmd: crate::SecretsCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;
    match cmd {
        crate::SecretsCommand::List { json } => {
            let rows = ff_db::pg_list_secrets(&pool).await?;
            if json {
                // Metadata only — the secret VALUE is never fetched by
                // pg_list_secrets, so it can't leak here. RFC3339 timestamp
                // (the table uses a coarser %Y-%m-%d %H:%M; JSON is lossless).
                let arr: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|(key, desc, updated_by, updated_at)| {
                        secret_list_json_row(
                            key,
                            desc.as_deref(),
                            updated_by.as_deref(),
                            updated_at,
                        )
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
                return Ok(());
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_list_json_row_omits_value_and_is_lossless() {
        let ts = chrono::DateTime::parse_from_rfc3339("2026-06-13T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let v = secret_list_json_row(
            "huggingface.token",
            Some("HuggingFace API token"),
            Some("venkat@Taylor-5.local"),
            &ts,
        );
        assert_eq!(v["key"], "huggingface.token");
        assert_eq!(v["description"], "HuggingFace API token");
        assert_eq!(v["updated_by"], "venkat@Taylor-5.local");
        // RFC3339, lossless vs the table's coarser minute-granularity render.
        assert_eq!(v["updated_at"], "2026-06-13T10:00:00+00:00");
        // The secret VALUE must never appear in the list projection.
        assert!(v.get("value").is_none());
    }

    #[test]
    fn secret_list_json_row_nulls_missing_optionals() {
        let ts = chrono::DateTime::parse_from_rfc3339("2026-06-13T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        // A row with no description and no updated_by → JSON null, not omitted.
        let v = secret_list_json_row("orphan_key", None, None, &ts);
        assert!(v["description"].is_null());
        assert!(v["updated_by"].is_null());
        assert_eq!(v["key"], "orphan_key");
    }
}
