//! `ff software` subcommand implementations.

use anyhow::Result;

use crate::{CYAN, GREEN, RESET, YELLOW, truncate_for_col, whoami_tag};

pub async fn handle_software_unblock(
    pool: &sqlx::PgPool,
    computer: &str,
    software_id: &str,
) -> Result<()> {
    let updated = sqlx::query(
        "UPDATE computer_software cs
            SET status               = 'ok',
                consecutive_failures = 0,
                last_upgrade_error   = NULL
           FROM computers c
          WHERE cs.computer_id = c.id
            AND cs.software_id = $1
            AND LOWER(c.name)  = LOWER($2)
            AND cs.status      <> 'upgrading'",
    )
    .bind(software_id)
    .bind(computer)
    .execute(pool)
    .await?
    .rows_affected();

    if updated == 0 {
        println!(
            "{YELLOW}no row matched (computer={computer}, software_id={software_id}) \
             — or the row is currently 'upgrading' (refusing to clobber an in-flight task).{RESET}"
        );
    } else {
        println!("{GREEN}✓ cleared {updated} row(s) — status='ok', consecutive_failures=0.{RESET}");
        println!(
            "  Next auto-upgrade tick (`ff software auto-upgrade-run-once`) will \
             re-evaluate drift and dispatch if needed."
        );
    }
    Ok(())
}

/// Implementation of `ff software auto-upgrade-run-once`.
///
/// Bypasses the hourly scheduler by directly calling `AutoUpgradeTick::run_once()`
/// on the local process. The resulting deferred tasks land in the defer queue
/// same as the hourly tick — workers on each target computer pull + execute
/// them on their next poll.
pub async fn handle_auto_upgrade_run_once(pool: &sqlx::PgPool, force: bool) -> Result<()> {
    // Mirror the gate check the hourly tick uses so --force is meaningful.
    let enabled = ff_db::pg_get_secret(pool, "auto_upgrade_enabled")
        .await
        .ok()
        .flatten()
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "on"))
        .unwrap_or(false);
    if !enabled && !force {
        println!("{YELLOW}auto_upgrade_enabled is not set — pass --force to run anyway.{RESET}");
        println!("  (To enable persistently: ff secrets set auto_upgrade_enabled true)");
        return Ok(());
    }

    let worker = ff_agent::fleet_info::resolve_this_node_name().await;
    println!(
        "{CYAN}[auto-upgrade run-once]{RESET} triggering tick as worker={worker}{}",
        if force && !enabled {
            " (--force: gate bypassed)"
        } else {
            ""
        }
    );
    let tick = ff_agent::auto_upgrade::AutoUpgradeTick::new(pool.clone(), worker);
    let enqueued = tick
        .run_once(force)
        .await
        .map_err(|e| anyhow::anyhow!("auto_upgrade run_once: {e}"))?;
    println!("{GREEN}✓ dispatched {enqueued} upgrade task(s){RESET}");
    Ok(())
}

pub async fn handle_software_list(
    pool: &sqlx::PgPool,
    computer: Option<String>,
    software: Option<String>,
    json: bool,
) -> Result<()> {
    let mut sql = String::from(
        "SELECT c.name            AS computer,
                sr.id              AS software_id,
                sr.display_name    AS display_name,
                sr.kind            AS kind,
                cs.installed_version AS installed_version,
                sr.latest_version  AS latest_version,
                cs.install_source  AS install_source,
                cs.status          AS status,
                cs.last_checked_at AS last_checked_at
         FROM computer_software cs
         JOIN computers c          ON cs.computer_id = c.id
         JOIN software_registry sr ON cs.software_id = sr.id
         WHERE 1=1",
    );
    if computer.is_some() {
        sql.push_str(" AND c.name = $1");
    }
    if software.is_some() {
        sql.push_str(if computer.is_some() {
            " AND sr.id = $2"
        } else {
            " AND sr.id = $1"
        });
    }
    sql.push_str(" ORDER BY c.name ASC, sr.id ASC");

    let mut query = sqlx::query(&sql);
    if let Some(c) = &computer {
        query = query.bind(c);
    }
    if let Some(s) = &software {
        query = query.bind(s);
    }

    let rows = query
        .fetch_all(pool)
        .await
        .map_err(|e| anyhow::anyhow!("list software: {e}"))?;

    if json {
        let arr: Vec<_> = rows.iter().map(|r| serde_json::json!({
            "computer":          sqlx::Row::get::<String, _>(r, "computer"),
            "software_id":       sqlx::Row::get::<String, _>(r, "software_id"),
            "display_name":      sqlx::Row::get::<String, _>(r, "display_name"),
            "kind":              sqlx::Row::get::<String, _>(r, "kind"),
            "installed_version": sqlx::Row::get::<Option<String>, _>(r, "installed_version"),
            "latest_version":    sqlx::Row::get::<Option<String>, _>(r, "latest_version"),
            "install_source":    sqlx::Row::get::<Option<String>, _>(r, "install_source"),
            "status":            sqlx::Row::get::<String, _>(r, "status"),
        })).collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
        return Ok(());
    }

    if rows.is_empty() {
        println!("(no matching computer_software rows)");
        return Ok(());
    }

    println!(
        "{:<11} {:<16} {:<10} {:<16} {:<16} {:<10} {:<18}",
        "COMPUTER", "SOFTWARE", "KIND", "INSTALLED", "LATEST", "SOURCE", "STATUS"
    );
    for r in &rows {
        let computer: String = sqlx::Row::get(r, "computer");
        let sid: String = sqlx::Row::get(r, "software_id");
        let kind: String = sqlx::Row::get(r, "kind");
        let installed: Option<String> = sqlx::Row::get(r, "installed_version");
        let latest: Option<String> = sqlx::Row::get(r, "latest_version");
        let src: Option<String> = sqlx::Row::get(r, "install_source");
        let status: String = sqlx::Row::get(r, "status");
        println!(
            "{:<11} {:<16} {:<10} {:<16} {:<16} {:<10} {:<18}",
            truncate_for_col(&computer, 11),
            truncate_for_col(&sid, 16),
            truncate_for_col(&kind, 10),
            truncate_for_col(installed.as_deref().unwrap_or("-"), 16),
            truncate_for_col(latest.as_deref().unwrap_or("-"), 16),
            truncate_for_col(src.as_deref().unwrap_or("-"), 10),
            truncate_for_col(&status, 18),
        );
    }
    Ok(())
}

pub async fn handle_software_drift(pool: &sqlx::PgPool, json: bool) -> Result<()> {
    let rows = sqlx::query(
        "SELECT c.name              AS computer,
                sr.id                AS software_id,
                sr.display_name      AS display_name,
                cs.installed_version AS installed_version,
                sr.latest_version    AS latest_version,
                cs.install_source    AS install_source,
                cs.status            AS status
         FROM computer_software cs
         JOIN computers c          ON cs.computer_id = c.id
         JOIN software_registry sr ON cs.software_id = sr.id
         WHERE cs.status = 'upgrade_available'
            OR (cs.installed_version IS NOT NULL
                AND sr.latest_version IS NOT NULL
                AND cs.installed_version <> sr.latest_version)
         ORDER BY c.name ASC, sr.id ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("list drift: {e}"))?;

    if json {
        let arr: Vec<_> = rows.iter().map(|r| serde_json::json!({
            "computer":          sqlx::Row::get::<String, _>(r, "computer"),
            "software_id":       sqlx::Row::get::<String, _>(r, "software_id"),
            "display_name":      sqlx::Row::get::<String, _>(r, "display_name"),
            "installed_version": sqlx::Row::get::<Option<String>, _>(r, "installed_version"),
            "latest_version":    sqlx::Row::get::<Option<String>, _>(r, "latest_version"),
            "install_source":    sqlx::Row::get::<Option<String>, _>(r, "install_source"),
            "status":            sqlx::Row::get::<String, _>(r, "status"),
        })).collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
        return Ok(());
    }

    if rows.is_empty() {
        println!(
            "{GREEN}✓ No drift detected — every computer_software row matches its software_registry.latest_version.{RESET}"
        );
        return Ok(());
    }

    println!(
        "{:<11} {:<16} {:<18} {:<18} {:<10} {:<18}",
        "COMPUTER", "SOFTWARE", "INSTALLED", "LATEST", "SOURCE", "STATUS"
    );
    for r in &rows {
        let computer: String = sqlx::Row::get(r, "computer");
        let sid: String = sqlx::Row::get(r, "software_id");
        let installed: Option<String> = sqlx::Row::get(r, "installed_version");
        let latest: Option<String> = sqlx::Row::get(r, "latest_version");
        let src: Option<String> = sqlx::Row::get(r, "install_source");
        let status: String = sqlx::Row::get(r, "status");
        println!(
            "{:<11} {:<16} {:<18} {:<18} {:<10} {:<18}",
            truncate_for_col(&computer, 11),
            truncate_for_col(&sid, 16),
            truncate_for_col(installed.as_deref().unwrap_or("-"), 18),
            truncate_for_col(latest.as_deref().unwrap_or("-"), 18),
            truncate_for_col(src.as_deref().unwrap_or("-"), 10),
            truncate_for_col(&status, 18),
        );
    }
    println!("\n{} row(s) with drift.", rows.len());
    Ok(())
}

/// `ff software add` — upsert a `software_registry` row directly, bypassing
/// `config/software.toml`. The TOML seeder is still the boot-time source of
/// truth; this handler is for ad-hoc additions.
pub async fn handle_software_add(
    pool: &sqlx::PgPool,
    id: &str,
    kind: &str,
    version_source_json: &str,
    upgrade_playbook_json: &str,
    display_name: Option<String>,
) -> Result<()> {
    let version_source: serde_json::Value = serde_json::from_str(version_source_json)
        .map_err(|e| anyhow::anyhow!("--version-source is not valid JSON: {e}"))?;
    let upgrade_playbook: serde_json::Value = serde_json::from_str(upgrade_playbook_json)
        .map_err(|e| anyhow::anyhow!("--upgrade-playbook is not valid JSON: {e}"))?;

    let display = display_name.unwrap_or_else(|| id.to_string());
    let who = whoami_tag();

    let result = sqlx::query(
        "INSERT INTO software_registry (id, display_name, kind, version_source, upgrade_playbook)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (id) DO UPDATE SET
             display_name     = EXCLUDED.display_name,
             kind             = EXCLUDED.kind,
             version_source   = EXCLUDED.version_source,
             upgrade_playbook = EXCLUDED.upgrade_playbook",
    )
    .bind(id)
    .bind(&display)
    .bind(kind)
    .bind(&version_source)
    .bind(&upgrade_playbook)
    .execute(pool)
    .await
    .map_err(|e| anyhow::anyhow!("upsert software_registry: {e}"))?;

    println!(
        "{GREEN}✓ software_registry upsert ok{RESET}  id={id}  display_name={display}  kind={kind}  rows_affected={}  by={who}",
        result.rows_affected()
    );
    Ok(())
}

/// `ff software remove` — delete a `software_registry` row. First cleans up
/// `computer_software` rows referencing it so the FK doesn't block.
pub async fn handle_software_remove(
    pool: &sqlx::PgPool,
    id: &str,
    confirm_yes: bool,
) -> Result<()> {
    let registry_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM software_registry WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .map_err(|e| anyhow::anyhow!("count software_registry: {e}"))?;

    if registry_count == 0 {
        println!("{YELLOW}No software_registry row with id='{id}' — nothing to remove.{RESET}");
        return Ok(());
    }

    let install_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM computer_software WHERE software_id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .map_err(|e| anyhow::anyhow!("count computer_software: {e}"))?;

    println!("About to remove software id='{id}':");
    println!("  software_registry rows:  {registry_count}");
    println!("  computer_software rows:  {install_count}");

    if !confirm_yes {
        eprintln!("{YELLOW}⚠ destructive. Re-run with --yes to confirm.{RESET}");
        return Ok(());
    }

    let cs = sqlx::query("DELETE FROM computer_software WHERE software_id = $1")
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("delete computer_software: {e}"))?;

    let sr = sqlx::query("DELETE FROM software_registry WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("delete software_registry: {e}"))?;

    println!(
        "{GREEN}✓ removed software id='{id}'{RESET}  computer_software_deleted={}  software_registry_deleted={}  by={}",
        cs.rows_affected(),
        sr.rows_affected(),
        whoami_tag()
    );
    Ok(())
}


pub async fn handle_software(cmd: crate::SoftwareCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        crate::SoftwareCommand::List {
            computer,
            software,
            json,
        } => handle_software_list(&pool, computer, software, json).await,
        crate::SoftwareCommand::Drift { json } => handle_software_drift(&pool, json).await,
        crate::SoftwareCommand::Add {
            id,
            kind,
            version_source,
            upgrade_playbook,
            display_name,
        } => {
            handle_software_add(
                &pool,
                &id,
                &kind,
                &version_source,
                &upgrade_playbook,
                display_name,
            )
            .await
        }
        crate::SoftwareCommand::Remove { id, yes } => {
            handle_software_remove(&pool, &id, yes).await
        }
        crate::SoftwareCommand::AutoUpgradeRunOnce { force } => {
            handle_auto_upgrade_run_once(&pool, force).await
        }
        crate::SoftwareCommand::Unblock {
            computer,
            software_id,
        } => handle_software_unblock(&pool, &computer, &software_id).await,
    }
}
