//! Fleet Tool Registry commands (Phase 15a).

use anyhow::Result;
use sqlx::{PgPool, Row};

const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const CYAN: &str = "\x1b[36m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

pub async fn handle_list(
    pg: &PgPool,
    node: Option<String>,
    name: Option<String>,
    unhealthy: bool,
) -> Result<()> {
    let mut sql = String::from(
        "SELECT tool_name, node_name, description, health_checked_at, \
         call_count, avg_latency_ms, \
         (health_checked_at > NOW() - INTERVAL '5 minutes') as healthy \
         FROM fleet_tools WHERE 1=1",
    );
    let mut params: Vec<String> = vec![];

    if node.is_some() {
        params.push("node".to_string());
        sql.push_str(&format!(" AND node_name = ${}", params.len()));
    }
    if name.is_some() {
        params.push("name".to_string());
        sql.push_str(&format!(
            " AND tool_name ILIKE '%' || ${} || '%'",
            params.len()
        ));
    }
    if unhealthy {
        sql.push_str(" AND health_checked_at <= NOW() - INTERVAL '5 minutes'");
    }
    sql.push_str(" ORDER BY node_name, tool_name");

    let mut query = sqlx::query(&sql);
    if let Some(n) = &node {
        query = query.bind(n);
    }
    if let Some(n) = &name {
        query = query.bind(n);
    }

    let rows = query.fetch_all(pg).await?;

    println!("{GREEN}✓ Fleet Tools{RESET} ({} total)", rows.len());
    for row in &rows {
        let name: String = row.get("tool_name");
        let node: String = row.get("node_name");
        let healthy: bool = row.get("healthy");
        let status = if healthy {
            format!("{GREEN}●{RESET}")
        } else {
            format!("{RED}●{RESET}")
        };
        println!("  {status} {name:<30} on {node}",);
    }
    Ok(())
}

pub async fn handle_health(pg: &PgPool) -> Result<()> {
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fleet_tools")
        .fetch_one(pg)
        .await?;

    let healthy: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM fleet_tools WHERE health_checked_at > NOW() - INTERVAL '5 minutes'",
    )
    .fetch_one(pg)
    .await?;

    let rows = sqlx::query(
        "SELECT \
            node_name, \
            COUNT(*) as tool_count, \
            COUNT(*) FILTER (WHERE health_checked_at > NOW() - INTERVAL '5 minutes') as healthy_count, \
            COUNT(*) FILTER (WHERE health_checked_at <= NOW() - INTERVAL '5 minutes') as unhealthy_count \
         FROM fleet_tools \
         GROUP BY node_name \
         ORDER BY node_name",
    )
    .fetch_all(pg)
    .await?;

    println!("{GREEN}✓ Tool Registry Health{RESET}");
    println!("  total:     {}", total);
    println!("  healthy:   {GREEN}{}{RESET}", healthy);
    if total - healthy > 0 {
        println!("  unhealthy: {RED}{}{RESET}", total - healthy);
    }

    if !rows.is_empty() {
        println!("\n  By node:");
        for row in &rows {
            let name: String = row.get("node_name");
            let n_tools: i64 = row.get("tool_count");
            let n_healthy: i64 = row.get("healthy_count");
            let n_unhealthy: i64 = row.get("unhealthy_count");
            let status = if n_unhealthy == 0 {
                format!("{GREEN}✓{RESET}")
            } else {
                format!("{RED}✗{RESET}")
            };
            println!(
                "    {status} {name:<15} {n_tools} tools ({n_healthy} healthy, {n_unhealthy} unhealthy)",
            );
        }
    } else {
        println!(
            "\n  {YELLOW}No tools registered yet. Run `ff tools register` on each node.{RESET}"
        );
    }

    Ok(())
}

pub async fn handle_register(pg: &PgPool, node: Option<String>) -> Result<()> {
    let node_name = node.unwrap_or_else(|| {
        std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| {
                std::process::Command::new("hostname")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            })
    });

    // Check if node exists in fleet_nodes
    let node_exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM fleet_nodes WHERE name = $1)")
            .bind(&node_name)
            .fetch_one(pg)
            .await?;

    if !node_exists {
        println!("{RED}✗{RESET} Node '{node_name}' not found in fleet_nodes.");
        println!("  Register the node first: ff fleet enroll {node_name}");
        return Ok(());
    }

    // For now, register a placeholder set of core tools.
    // In production, ff-agent will enumerate its actual tools and send them.
    let core_tools = vec![
        ("Bash", "Execute shell commands"),
        ("Read", "Read file contents"),
        ("Write", "Write or overwrite files"),
        ("Edit", "Edit files in-place"),
        ("Glob", "Find files by glob pattern"),
        ("Grep", "Search file contents"),
        ("WebFetch", "Fetch web pages"),
        ("WebSearch", "Search the web"),
    ];

    let mut registered = 0;
    for (tool_name, description) in core_tools {
        let result = sqlx::query(
            "INSERT INTO fleet_tools (tool_name, node_name, description, parameters_schema, capabilities_required, health_checked_at) \
             VALUES ($1, $2, $3, '{}', '{}', NOW()) \
             ON CONFLICT (tool_name, node_name) \
             DO UPDATE SET description = EXCLUDED.description, health_checked_at = NOW()",
        )
        .bind(tool_name)
        .bind(&node_name)
        .bind(description)
        .execute(pg)
        .await?;

        if result.rows_affected() > 0 {
            registered += 1;
        }
    }

    println!("{GREEN}✓{RESET} Registered {registered} tools for {CYAN}{node_name}{RESET}");
    Ok(())
}
