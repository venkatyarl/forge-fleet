use crate::{RESET, YELLOW, whoami_tag};
use anyhow::{Context, Result};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct OnePasswordItem {
    #[serde(default)]
    fields: Vec<OnePasswordField>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct OnePasswordField {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    label: String,
    #[serde(default)]
    value: Option<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

fn parse_onepassword_mappings(mappings: &[String]) -> Result<Vec<(&str, &str)>> {
    mappings
        .iter()
        .map(|mapping| {
            let (key, label) = mapping.split_once('=').ok_or_else(|| {
                anyhow::anyhow!("invalid --map '{mapping}'; expected fleet_key=field_label")
            })?;
            let (key, label) = (key.trim(), label.trim());
            if key.is_empty() || label.is_empty() {
                anyhow::bail!("invalid --map '{mapping}'; neither side may be empty");
            }
            Ok((key, label))
        })
        .collect()
}

fn onepassword_client() -> Result<(reqwest::Client, String)> {
    let host = std::env::var("OP_CONNECT_HOST")
        .context("OP_CONNECT_HOST is required for 1Password Connect")?;
    let token = std::env::var("OP_CONNECT_TOKEN")
        .context("OP_CONNECT_TOKEN is required for 1Password Connect")?;
    let mut headers = HeaderMap::new();
    let auth = HeaderValue::from_str(&format!("Bearer {token}"))
        .context("OP_CONNECT_TOKEN is not a valid HTTP header value")?;
    headers.insert(AUTHORIZATION, auth);
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .context("build 1Password HTTP client")?;
    Ok((client, host.trim_end_matches('/').to_string()))
}

async fn fetch_onepassword_item(
    vault: &str,
    item: &str,
) -> Result<(reqwest::Client, String, OnePasswordItem)> {
    let (client, host) = onepassword_client()?;
    let url = format!("{host}/v1/vaults/{vault}/items/{item}");
    let response = client
        .get(&url)
        .send()
        .await
        .context("request 1Password item")?;
    if !response.status().is_success() {
        anyhow::bail!(
            "1Password item request failed with HTTP {}",
            response.status()
        );
    }
    let item = response.json().await.context("decode 1Password item")?;
    Ok((client, url, item))
}

/// Resolve the value for `ff secrets set`: from the positional arg, or from
/// stdin when `--stdin` is passed or no value arg was given. Reading from stdin
/// keeps the secret out of shell history AND the process argument list (`ps`) —
/// the safe way to set a token. Trailing newline(s) from an `echo`/paste are
/// trimmed; interior content is preserved verbatim.
fn resolve_secret_value(value: Option<String>, stdin: bool) -> Result<String> {
    match (value, stdin) {
        (Some(_), true) => {
            anyhow::bail!("pass the value as an argument OR --stdin, not both")
        }
        (Some(v), false) => Ok(v),
        (None, _) => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("read secret value from stdin")?;
            let v = buf.trim_end_matches(['\n', '\r']).to_string();
            if v.is_empty() {
                anyhow::bail!("no secret value provided on stdin");
            }
            Ok(v)
        }
    }
}

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
            stdin,
        } => {
            let value = resolve_secret_value(value, stdin)?;
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
        crate::SecretsCommand::ImportOnePassword {
            vault,
            item,
            mappings,
        } => {
            let mappings = parse_onepassword_mappings(&mappings)?;
            let (_, _, source) = fetch_onepassword_item(&vault, &item).await?;
            let who = whoami_tag();
            // Resolve every requested field before writing any of them so a
            // miss cannot leave the fleet with a partially imported item.
            let values = mappings
                .iter()
                .map(|(key, label)| {
                    let value = source
                        .fields
                        .iter()
                        .find(|field| field.label == *label)
                        .and_then(|field| field.value.as_deref())
                        .ok_or_else(|| {
                            anyhow::anyhow!("1Password item has no value for field '{label}'")
                        })?;
                    Ok((*key, *label, value))
                })
                .collect::<Result<Vec<_>>>()?;
            for (key, label, value) in values {
                ff_db::pg_set_secret(
                    &pool,
                    key,
                    value,
                    Some(&format!("Imported from 1Password field '{label}'")),
                    Some(&who),
                )
                .await?;
            }
            println!("Imported {} secret(s) from 1Password", mappings.len());
        }
        crate::SecretsCommand::ExportOnePassword {
            vault,
            item,
            mappings,
        } => {
            let mappings = parse_onepassword_mappings(&mappings)?;
            let (client, url, mut destination) = fetch_onepassword_item(&vault, &item).await?;
            for (key, label) in &mappings {
                let value = ff_db::pg_get_secret(&pool, key)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("no fleet secret set for key '{key}'"))?;
                let field = destination
                    .fields
                    .iter_mut()
                    .find(|field| field.label == *label)
                    .ok_or_else(|| {
                        anyhow::anyhow!("1Password item has no field labeled '{label}'")
                    })?;
                field.value = Some(value);
            }
            let response = client
                .put(url)
                .json(&destination)
                .send()
                .await
                .context("update 1Password item")?;
            if !response.status().is_success() {
                anyhow::bail!(
                    "1Password item update failed with HTTP {}",
                    response.status()
                );
            }
            println!("Exported {} secret(s) to 1Password", mappings.len());
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

    #[test]
    fn onepassword_mappings_are_explicit_and_validated() {
        let mappings = vec![
            "github.token=credential".to_string(),
            "api_key=API key".to_string(),
        ];
        assert_eq!(
            parse_onepassword_mappings(&mappings).unwrap(),
            vec![("github.token", "credential"), ("api_key", "API key")]
        );
        assert!(parse_onepassword_mappings(&["missing-separator".into()]).is_err());
        assert!(parse_onepassword_mappings(&["key= ".into()]).is_err());
    }

    #[test]
    fn onepassword_item_preserves_unknown_fields_when_updated() {
        let mut item: OnePasswordItem = serde_json::from_value(serde_json::json!({
            "id": "item-id",
            "title": "ForgeFleet",
            "fields": [{"id": "password", "label": "credential", "type": "CONCEALED", "value": "old"}]
        }))
        .unwrap();
        item.fields[0].value = Some("new".into());
        let value = serde_json::to_value(item).unwrap();
        assert_eq!(value["id"], "item-id");
        assert_eq!(value["title"], "ForgeFleet");
        assert_eq!(value["fields"][0]["type"], "CONCEALED");
        assert_eq!(value["fields"][0]["value"], "new");
    }
}
