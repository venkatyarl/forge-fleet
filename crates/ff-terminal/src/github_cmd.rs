//! `ff github` — fleet-wide GitHub SSH identity registry.
//!
//! The DB owns:
//!   - `github_ssh_aliases` rows (one per `Host github.com-*` block)
//!   - `fleet_secrets` entries `github_ssh_<file>_priv` + `_pub`
//!
//! `ff github sync` on a fleet computer pulls both, materializes the
//! aliases into `~/.ssh/config` (append-only, idempotent), and writes
//! the key files to `~/.ssh/` with the correct permissions. Intended
//! to run on enrollment and to be safely re-runnable.

use anyhow::{Context, Result};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use crate::GithubCommand;
use crate::utils::{CYAN, GREEN, RESET, YELLOW};

#[derive(sqlx::FromRow, Debug)]
struct AliasRow {
    alias_name: String,
    hostname: String,
    ssh_user: String,
    identity_file: String,
    identities_only: bool,
    description: Option<String>,
}

pub async fn handle_github(cmd: GithubCommand) -> Result<()> {
    let pool = open_pool().await?;
    match cmd {
        GithubCommand::List { json } => handle_list(&pool, json).await,
        GithubCommand::Sync { dry_run } => handle_sync(&pool, dry_run).await,
    }
}

async fn handle_list(pool: &sqlx::PgPool, json: bool) -> Result<()> {
    let aliases: Vec<AliasRow> = sqlx::query_as(
        "SELECT alias_name, hostname, ssh_user, identity_file, identities_only, description
         FROM github_ssh_aliases ORDER BY alias_name",
    )
    .fetch_all(pool)
    .await?;

    if json {
        let mut arr: Vec<serde_json::Value> = Vec::with_capacity(aliases.len());
        for a in &aliases {
            let secret_base = secret_base_for(&a.identity_file);
            let priv_key = format!("{secret_base}_priv");
            let pub_key = format!("{secret_base}_pub");
            let priv_present = secret_present(pool, &priv_key).await?;
            let pub_present = secret_present(pool, &pub_key).await?;
            let fp = if pub_present {
                fingerprint_from_db(pool, &pub_key).await.ok()
            } else {
                None
            };
            arr.push(serde_json::json!({
                "alias_name": a.alias_name,
                "hostname": a.hostname,
                "ssh_user": a.ssh_user,
                "identity_file": a.identity_file,
                "identities_only": a.identities_only,
                "description": a.description,
                "priv_present": priv_present,
                "pub_present": pub_present,
                "fingerprint": fp,
            }));
        }
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }

    println!(
        "{GREEN}✓ GitHub SSH aliases{RESET} ({} from Postgres)",
        aliases.len()
    );
    for a in &aliases {
        println!("  {CYAN}{}{RESET}", a.alias_name);
        println!(
            "    Host {} / User {} / IdentityFile {}",
            a.hostname, a.ssh_user, a.identity_file
        );
        if let Some(d) = &a.description {
            println!("    {d}");
        }
        let secret_base = secret_base_for(&a.identity_file);
        let priv_key = format!("{secret_base}_priv");
        let pub_key = format!("{secret_base}_pub");
        let priv_present = secret_present(pool, &priv_key).await?;
        let pub_present = secret_present(pool, &pub_key).await?;
        let fp = if pub_present {
            fingerprint_from_db(pool, &pub_key).await.ok()
        } else {
            None
        };
        let tag_priv = if priv_present {
            format!("{GREEN}priv✓{RESET}")
        } else {
            format!("{YELLOW}priv✗{RESET}")
        };
        let tag_pub = if pub_present {
            format!("{GREEN}pub✓{RESET}")
        } else {
            format!("{YELLOW}pub✗{RESET}")
        };
        let fp_tag = fp.unwrap_or_else(|| "-".into());
        println!("    keys: {tag_priv} {tag_pub}   fingerprint: {fp_tag}");
    }
    Ok(())
}

async fn handle_sync(pool: &sqlx::PgPool, dry_run: bool) -> Result<()> {
    let aliases: Vec<AliasRow> = sqlx::query_as(
        "SELECT alias_name, hostname, ssh_user, identity_file, identities_only, description
         FROM github_ssh_aliases ORDER BY alias_name",
    )
    .fetch_all(pool)
    .await?;

    let home = dirs::home_dir().context("HOME not set")?;
    let ssh_dir = home.join(".ssh");
    let ssh_config = ssh_dir.join("config");

    if !dry_run {
        std::fs::create_dir_all(&ssh_dir)
            .with_context(|| format!("mkdir {}", ssh_dir.display()))?;
        // ~/.ssh must be 0700 or ssh refuses to use IdentityFile entries.
        let mut perms = std::fs::metadata(&ssh_dir)?.permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&ssh_dir, perms)?;
    }

    // 1. Write key files (priv 0600, pub 0644). Skip files already
    //    identical to the DB copy.
    let mut written_keys: usize = 0;
    let mut skipped_keys: usize = 0;
    for a in &aliases {
        let base = secret_base_for(&a.identity_file);
        for (suffix, mode) in [("_priv", 0o600), ("_pub", 0o644)] {
            let secret_key = format!("{base}{suffix}");
            let Some(value) = read_secret(pool, &secret_key).await? else {
                println!("  {YELLOW}skip{RESET} {secret_key} — not in fleet_secrets");
                continue;
            };
            let target_path = resolve_identity_path(&home, &a.identity_file, suffix);
            let existing = std::fs::read_to_string(&target_path).ok();
            if existing.as_deref() == Some(value.as_str()) {
                skipped_keys += 1;
                continue;
            }
            if dry_run {
                println!(
                    "  [dry-run] would write {} ({} bytes, mode {:o})",
                    target_path.display(),
                    value.len(),
                    mode
                );
            } else {
                std::fs::write(&target_path, &value)
                    .with_context(|| format!("write {}", target_path.display()))?;
                let mut p = std::fs::metadata(&target_path)?.permissions();
                p.set_mode(mode);
                std::fs::set_permissions(&target_path, p)?;
                println!("  {GREEN}wrote{RESET} {}", target_path.display());
                written_keys += 1;
            }
        }
    }

    // 2. Append any missing alias blocks to ~/.ssh/config. We only
    //    add — we never overwrite or remove. If the user has hand-edited
    //    an alias, ff leaves it alone.
    let existing_cfg = std::fs::read_to_string(&ssh_config).unwrap_or_default();
    let mut to_append = String::new();
    let mut added_count: usize = 0;
    let mut skipped_count: usize = 0;
    for a in &aliases {
        if alias_already_present(&existing_cfg, &a.alias_name) {
            skipped_count += 1;
            continue;
        }
        to_append.push_str(&render_alias(a));
        added_count += 1;
    }
    if !to_append.is_empty() {
        if dry_run {
            println!(
                "  [dry-run] would append {} alias block(s) to {}:",
                added_count,
                ssh_config.display()
            );
            for line in to_append.lines() {
                println!("    {line}");
            }
        } else {
            let mut combined = existing_cfg;
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&to_append);
            std::fs::write(&ssh_config, &combined)
                .with_context(|| format!("write {}", ssh_config.display()))?;
            let mut p = std::fs::metadata(&ssh_config)?.permissions();
            p.set_mode(0o600);
            std::fs::set_permissions(&ssh_config, p)?;
            println!(
                "  {GREEN}appended{RESET} {} alias block(s) to {}",
                added_count,
                ssh_config.display()
            );
        }
    }

    println!(
        "{GREEN}✓ sync{RESET}{} — keys: {} written, {} unchanged; aliases: {} added, {} already present",
        if dry_run { " (dry-run)" } else { "" },
        written_keys,
        skipped_keys,
        added_count,
        skipped_count,
    );
    Ok(())
}

fn render_alias(a: &AliasRow) -> String {
    let mut s = format!(
        "Host {}\n    HostName {}\n    User {}\n    IdentityFile {}\n",
        a.alias_name, a.hostname, a.ssh_user, a.identity_file
    );
    if a.identities_only {
        s.push_str("    IdentitiesOnly yes\n");
    }
    s.push('\n');
    s
}

fn alias_already_present(cfg: &str, alias_name: &str) -> bool {
    cfg.lines().any(|line| {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Host ") {
            rest.split_whitespace().any(|tok| tok == alias_name)
        } else {
            false
        }
    })
}

fn resolve_identity_path(home: &std::path::Path, identity_file: &str, suffix: &str) -> PathBuf {
    let expanded = if let Some(rest) = identity_file.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(identity_file)
    };
    if suffix == "_pub" {
        let mut s = expanded.into_os_string();
        s.push(".pub");
        PathBuf::from(s)
    } else {
        expanded
    }
}

fn secret_base_for(identity_file: &str) -> String {
    let file_name = identity_file.rsplit('/').next().unwrap_or(identity_file);
    format!("github_ssh_{file_name}")
}

async fn read_secret(pool: &sqlx::PgPool, key: &str) -> Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT value FROM fleet_secrets WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(v,)| v))
}

async fn secret_present(pool: &sqlx::PgPool, key: &str) -> Result<bool> {
    let row: Option<(i64,)> = sqlx::query_as("SELECT 1::bigint FROM fleet_secrets WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

async fn fingerprint_from_db(pool: &sqlx::PgPool, key: &str) -> Result<String> {
    let val = read_secret(pool, key)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no secret {key}"))?;
    // Pull the base64 key body out of the ssh-rsa / ssh-ed25519 line and
    // produce the standard SHA256 fingerprint.
    let body = val
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("malformed pubkey"))?;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use sha2::{Digest, Sha256};
    let raw = STANDARD
        .decode(body)
        .map_err(|e| anyhow::anyhow!("b64 decode: {e}"))?;
    let digest = Sha256::digest(&raw);
    let b64 = STANDARD.encode(digest);
    Ok(format!("SHA256:{}", b64.trim_end_matches('=')))
}

async fn open_pool() -> Result<sqlx::PgPool> {
    let home = dirs::home_dir().context("HOME not set")?;
    let cfg_path = home.join(".forgefleet/fleet.toml");
    let toml_str = std::fs::read_to_string(&cfg_path)
        .with_context(|| format!("read {}", cfg_path.display()))?;
    let cfg: ff_core::config::FleetConfig =
        toml::from_str(&toml_str).with_context(|| format!("parse {}", cfg_path.display()))?;
    ff_core::db_health::ensure_postgres_up(&cfg.database.url)
        .await
        .map_err(|e| anyhow::anyhow!("postgres unavailable: {e}"))?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&cfg.database.url)
        .await?;
    Ok(pool)
}
