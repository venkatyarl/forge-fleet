//! `ff ext` subcommand implementations.

use anyhow::Result;

use crate::{CYAN, GREEN, RESET, YELLOW, truncate_for_col, whoami_tag};

pub async fn handle_ext_list(pool: &sqlx::PgPool, json: bool) -> Result<()> {
    let tools = ff_agent::external_tools_registry::list_tools(pool)
        .await
        .map_err(|e| anyhow::anyhow!("list external_tools: {e}"))?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&tools).unwrap_or_default()
        );
        return Ok(());
    }

    if tools.is_empty() {
        println!(
            "{YELLOW}(external_tools is empty — run `ff ext seed` to load config/external_tools.toml){RESET}"
        );
        return Ok(());
    }

    println!(
        "{:<22} {:<8} {:<14} {:<14} {:<14} MCP",
        "ID", "KIND", "METHOD", "CLI", "LATEST"
    );
    for t in &tools {
        println!(
            "{:<22} {:<8} {:<14} {:<14} {:<14} {}",
            truncate_for_col(&t.id, 22),
            truncate_for_col(&t.kind, 8),
            truncate_for_col(&t.install_method, 14),
            truncate_for_col(t.cli_entrypoint.as_deref().unwrap_or("-"), 14),
            truncate_for_col(t.latest_version.as_deref().unwrap_or("-"), 14),
            if t.register_as_mcp {
                "auto-register"
            } else {
                "-"
            },
        );
    }
    println!("\n{} tool(s) in catalog.", tools.len());
    Ok(())
}

pub async fn handle_ext_installed(
    pool: &sqlx::PgPool,
    computer: Option<String>,
    tool: Option<String>,
    json: bool,
) -> Result<()> {
    let mut sql = String::from(
        "SELECT c.name              AS computer,
                c.primary_ip         AS primary_ip,
                et.id                AS tool_id,
                et.display_name      AS display_name,
                et.kind              AS kind,
                cet.installed_version AS installed_version,
                et.latest_version    AS latest_version,
                cet.install_source   AS install_source,
                cet.install_path     AS install_path,
                cet.mcp_registered   AS mcp_registered,
                cet.status           AS status,
                cet.last_checked_at  AS last_checked_at
           FROM computer_external_tools cet
           JOIN computers c      ON cet.computer_id = c.id
           JOIN external_tools et ON cet.tool_id = et.id
          WHERE 1=1",
    );
    if computer.is_some() {
        sql.push_str(" AND c.name = $1");
    }
    if tool.is_some() {
        sql.push_str(if computer.is_some() {
            " AND et.id = $2"
        } else {
            " AND et.id = $1"
        });
    }
    sql.push_str(" ORDER BY c.name ASC, et.id ASC");

    let mut query = sqlx::query(&sql);
    if let Some(c) = &computer {
        query = query.bind(c);
    }
    if let Some(t) = &tool {
        query = query.bind(t);
    }

    let mut rows = query
        .fetch_all(pool)
        .await
        .map_err(|e| anyhow::anyhow!("list computer_external_tools: {e}"))?;
    // Subnet order (per-computer table convention) — numeric octets, not the
    // lexical `c.name`. SQL ORDER BY name,id stays the stable secondary key.
    crate::helpers::sort_rows_by_primary_ip(&mut rows);

    if json {
        let arr: Vec<_> = rows.iter().map(|r| serde_json::json!({
            "computer":          sqlx::Row::get::<String, _>(r, "computer"),
            "tool_id":           sqlx::Row::get::<String, _>(r, "tool_id"),
            "display_name":      sqlx::Row::get::<String, _>(r, "display_name"),
            "kind":              sqlx::Row::get::<String, _>(r, "kind"),
            "installed_version": sqlx::Row::get::<Option<String>, _>(r, "installed_version"),
            "latest_version":    sqlx::Row::get::<Option<String>, _>(r, "latest_version"),
            "install_source":    sqlx::Row::get::<Option<String>, _>(r, "install_source"),
            "install_path":      sqlx::Row::get::<Option<String>, _>(r, "install_path"),
            "mcp_registered":    sqlx::Row::get::<bool, _>(r, "mcp_registered"),
            "status":            sqlx::Row::get::<String, _>(r, "status"),
        })).collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
        return Ok(());
    }

    if rows.is_empty() {
        println!("(no matching computer_external_tools rows)");
        return Ok(());
    }

    println!(
        "{:<11} {:<22} {:<10} {:<14} {:<14} {:<10} {:<18}",
        "COMPUTER", "TOOL", "KIND", "INSTALLED", "LATEST", "SOURCE", "STATUS"
    );
    for r in &rows {
        let computer: String = sqlx::Row::get(r, "computer");
        let tid: String = sqlx::Row::get(r, "tool_id");
        let kind: String = sqlx::Row::get(r, "kind");
        let installed: Option<String> = sqlx::Row::get(r, "installed_version");
        let latest: Option<String> = sqlx::Row::get(r, "latest_version");
        let src: Option<String> = sqlx::Row::get(r, "install_source");
        let status: String = sqlx::Row::get(r, "status");
        println!(
            "{:<11} {:<22} {:<10} {:<14} {:<14} {:<10} {:<18}",
            truncate_for_col(&computer, 11),
            truncate_for_col(&tid, 22),
            truncate_for_col(&kind, 10),
            truncate_for_col(installed.as_deref().unwrap_or("-"), 14),
            truncate_for_col(latest.as_deref().unwrap_or("-"), 14),
            truncate_for_col(src.as_deref().unwrap_or("-"), 10),
            truncate_for_col(&status, 18),
        );
    }
    Ok(())
}

pub async fn handle_ext_drift(pool: &sqlx::PgPool, json: bool) -> Result<()> {
    let mut rows = sqlx::query(
        "SELECT c.name              AS computer,
                c.primary_ip         AS primary_ip,
                et.id                AS tool_id,
                et.display_name      AS display_name,
                cet.installed_version AS installed_version,
                et.latest_version    AS latest_version,
                cet.install_source   AS install_source,
                cet.status           AS status
           FROM computer_external_tools cet
           JOIN computers c      ON cet.computer_id = c.id
           JOIN external_tools et ON cet.tool_id = et.id
          WHERE cet.status = 'upgrade_available'
             OR (cet.installed_version IS NOT NULL
                 AND et.latest_version IS NOT NULL
                 AND cet.installed_version <> et.latest_version)
          ORDER BY c.name ASC, et.id ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("list ext drift: {e}"))?;
    crate::helpers::sort_rows_by_primary_ip(&mut rows);

    if json {
        let arr: Vec<_> = rows.iter().map(|r| serde_json::json!({
            "computer":          sqlx::Row::get::<String, _>(r, "computer"),
            "tool_id":           sqlx::Row::get::<String, _>(r, "tool_id"),
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
            "{GREEN}✓ No external-tool drift — every computer_external_tools row matches external_tools.latest_version.{RESET}"
        );
        return Ok(());
    }

    println!(
        "{:<11} {:<22} {:<18} {:<18} {:<10} {:<18}",
        "COMPUTER", "TOOL", "INSTALLED", "LATEST", "SOURCE", "STATUS"
    );
    for r in &rows {
        let computer: String = sqlx::Row::get(r, "computer");
        let tid: String = sqlx::Row::get(r, "tool_id");
        let installed: Option<String> = sqlx::Row::get(r, "installed_version");
        let latest: Option<String> = sqlx::Row::get(r, "latest_version");
        let src: Option<String> = sqlx::Row::get(r, "install_source");
        let status: String = sqlx::Row::get(r, "status");
        println!(
            "{:<11} {:<22} {:<18} {:<18} {:<10} {:<18}",
            truncate_for_col(&computer, 11),
            truncate_for_col(&tid, 22),
            truncate_for_col(installed.as_deref().unwrap_or("-"), 18),
            truncate_for_col(latest.as_deref().unwrap_or("-"), 18),
            truncate_for_col(src.as_deref().unwrap_or("-"), 10),
            truncate_for_col(&status, 18),
        );
    }
    println!("\n{} external-tool row(s) with drift.", rows.len());
    Ok(())
}

pub async fn handle_ext_install(
    pool: &sqlx::PgPool,
    tool_id: &str,
    computer: Option<String>,
    all: bool,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    if computer.is_none() && !all {
        anyhow::bail!("pass --all or --computer <name> to pick targets");
    }
    if computer.is_some() && all {
        anyhow::bail!("--computer and --all are mutually exclusive");
    }

    let (plans, skipped) = ff_agent::external_tools_installer::resolve_install_plans(
        pool,
        tool_id,
        computer.as_deref(),
        all,
    )
    .await?;

    let display_name = plans
        .first()
        .map(|p| p.display_name.clone())
        .unwrap_or_else(|| tool_id.to_string());
    let latest_version = plans.first().and_then(|p| p.latest_version.clone());

    if plans.is_empty() && skipped.is_empty() {
        println!(
            "{YELLOW}No target computers found for tool_id='{tool_id}'. Nothing to do.{RESET}"
        );
        return Ok(());
    }

    println!("{CYAN}▶ ff ext install {tool_id}{RESET}");
    println!("  tool:            {display_name} ({tool_id})");
    println!(
        "  latest upstream: {}",
        latest_version.as_deref().unwrap_or("(unknown)")
    );
    println!("  targets:         {} computer(s)", plans.len());
    if plans.is_empty() {
        println!("{YELLOW}No resolvable targets. Nothing to do.{RESET}");
        for (name, why) in &skipped {
            println!("    {YELLOW}⚠ skip{RESET} {name}: {why}");
        }
        return Ok(());
    }

    println!(
        "\n  {:<10} {:<14} {:<14} {:<10} {:<22} command",
        "computer", "os_family", "method", "installed", "playbook_key"
    );
    for p in &plans {
        let short_cmd = if p.command.len() > 60 {
            format!("{}…", &p.command[..60])
        } else {
            p.command.clone()
        };
        println!(
            "  {:<10} {:<14} {:<14} {:<10} {:<22} {}",
            p.computer_name,
            p.os_family,
            p.install_method,
            p.installed_version.as_deref().unwrap_or("-"),
            p.playbook_key,
            short_cmd,
        );
    }
    for (name, why) in &skipped {
        println!("  {YELLOW}⚠ skip{RESET} {name}: {why}");
    }

    if dry_run {
        println!(
            "\n{YELLOW}Dry run — not enqueuing. Drop --dry-run and pass --yes to actually enqueue.{RESET}"
        );
        return Ok(());
    }
    if !yes {
        println!("\n{YELLOW}Pass --yes to actually enqueue these install tasks.{RESET}");
        return Ok(());
    }

    let who = whoami_tag();
    let enqueued = ff_agent::external_tools_installer::enqueue_plans(pool, &plans, &who).await?;

    println!(
        "\n{GREEN}✓ Enqueued {} install task(s):{RESET}",
        enqueued.len()
    );
    for ep in &enqueued {
        println!("  {:<12} {}", ep.computer_name, ep.defer_id);
    }
    println!("\nTrack progress with: ff defer list");
    Ok(())
}

pub async fn handle_ext(cmd: crate::ExtCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        crate::ExtCommand::List { json } => handle_ext_list(&pool, json).await,
        crate::ExtCommand::Installed {
            computer,
            tool,
            json,
        } => handle_ext_installed(&pool, computer, tool, json).await,
        crate::ExtCommand::Install {
            tool_id,
            computer,
            all,
            dry_run,
            yes,
        } => handle_ext_install(&pool, &tool_id, computer, all, dry_run, yes).await,
        crate::ExtCommand::Drift { json } => handle_ext_drift(&pool, json).await,
    }
}
