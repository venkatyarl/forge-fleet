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
    json: bool,
) -> Result<()> {
    // Validate --node against the (drift-free) computers table BEFORE running the
    // query, so a typo errors loudly instead of silently returning an empty list
    // that's indistinguishable from "this node has no tools registered". Uses the
    // same `worker_name = $N` exact match the query below applies (every
    // fleet_tools.worker_name is a computers.name). Same foot-gun class as
    // `ff software list --computer` (#241) and `ff tasks list --computer` (#239).
    // --name is a substring search (ILIKE '%..%'), so an empty result there is a
    // genuine no-match, not a typo, and is left untouched.
    if let Some(n) = &node {
        let known: i64 = sqlx::query_scalar("SELECT count(*) FROM computers WHERE name = $1")
            .bind(n)
            .fetch_one(pg)
            .await
            .map_err(|e| anyhow::anyhow!("validate --node: {e}"))?;
        if known == 0 {
            anyhow::bail!("unknown node '{n}' — run 'ff fleet health' to list computers");
        }
    }

    let mut sql = String::from(
        "SELECT id, tool_name, worker_name, description, capabilities_required, \
         parameters_schema, health_checked_at, created_at, \
         call_count, avg_latency_ms, \
         (health_checked_at > NOW() - INTERVAL '5 minutes') as healthy \
         FROM fleet_tools WHERE 1=1",
    );
    let mut params: Vec<String> = vec![];

    if node.is_some() {
        params.push("node".to_string());
        sql.push_str(&format!(" AND worker_name = ${}", params.len()));
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
    sql.push_str(" ORDER BY worker_name, tool_name");

    let mut query = sqlx::query(&sql);
    if let Some(n) = &node {
        query = query.bind(n);
    }
    if let Some(n) = &name {
        query = query.bind(n);
    }

    let rows = query.fetch_all(pg).await?;

    if json {
        let out: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                let id: uuid::Uuid = row.get("id");
                tool_list_json_row(
                    &id.to_string(),
                    &row.get::<String, _>("tool_name"),
                    &row.get::<String, _>("worker_name"),
                    &row.get::<String, _>("description"),
                    &row.get::<Vec<String>, _>("capabilities_required"),
                    &row.get::<serde_json::Value, _>("parameters_schema"),
                    row.get::<i32, _>("call_count"),
                    row.get::<Option<f32>, _>("avg_latency_ms"),
                    row.get::<bool, _>("healthy"),
                    row.get::<chrono::DateTime<chrono::Utc>, _>("health_checked_at"),
                    row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
                )
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!("{GREEN}✓ Fleet Tools{RESET} ({} total)", rows.len());
    for row in &rows {
        let name: String = row.get("tool_name");
        let node: String = row.get("worker_name");
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

/// Build one lossless JSON object for an `ff tools list --json` row.
/// Pure (no DB/clock) so it can be unit-tested; emits every field the
/// fixed-width table elides — the tool UUID, description, the raw
/// capabilities_required/parameters_schema, call_count, avg_latency_ms, and
/// RFC3339 timestamps. `avg_latency_ms` is JSON null when never measured
/// (kept as a key for a stable shape).
#[allow(clippy::too_many_arguments)]
fn tool_list_json_row(
    id: &str,
    tool_name: &str,
    node: &str,
    description: &str,
    capabilities_required: &[String],
    parameters_schema: &serde_json::Value,
    call_count: i32,
    avg_latency_ms: Option<f32>,
    healthy: bool,
    health_checked_at: chrono::DateTime<chrono::Utc>,
    created_at: chrono::DateTime<chrono::Utc>,
) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "tool_name": tool_name,
        "node": node,
        "description": description,
        "capabilities_required": capabilities_required,
        "parameters_schema": parameters_schema,
        "call_count": call_count,
        "avg_latency_ms": avg_latency_ms,
        "healthy": healthy,
        "health_checked_at": health_checked_at.to_rfc3339(),
        "created_at": created_at.to_rfc3339(),
    })
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
            worker_name, \
            COUNT(*) as tool_count, \
            COUNT(*) FILTER (WHERE health_checked_at > NOW() - INTERVAL '5 minutes') as healthy_count, \
            COUNT(*) FILTER (WHERE health_checked_at <= NOW() - INTERVAL '5 minutes') as unhealthy_count \
         FROM fleet_tools \
         GROUP BY worker_name \
         ORDER BY worker_name",
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
            let name: String = row.get("worker_name");
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
    let worker_name = node.unwrap_or_else(|| {
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

    // Check if node exists in fleet_workers
    let node_exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM fleet_workers WHERE name = $1)")
            .bind(&worker_name)
            .fetch_one(pg)
            .await?;

    if !node_exists {
        println!("{RED}✗{RESET} Node '{worker_name}' not found in fleet_workers.");
        println!("  Register the node first: ff fleet enroll {worker_name}");
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
            "INSERT INTO fleet_tools (tool_name, worker_name, description, parameters_schema, capabilities_required, health_checked_at) \
             VALUES ($1, $2, $3, '{}', '{}', NOW()) \
             ON CONFLICT (tool_name, worker_name) \
             DO UPDATE SET description = EXCLUDED.description, health_checked_at = NOW()",
        )
        .bind(tool_name)
        .bind(&worker_name)
        .bind(description)
        .execute(pg)
        .await?;

        if result.rows_affected() > 0 {
            registered += 1;
        }
    }

    println!("{GREEN}✓{RESET} Registered {registered} tools for {CYAN}{worker_name}{RESET}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_list_json_row_is_lossless_incl_elided_fields() {
        let ts = chrono::DateTime::parse_from_rfc3339("2026-06-13T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let created = chrono::DateTime::parse_from_rfc3339("2026-06-01T08:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let caps = vec!["fs".to_string(), "net".to_string()];
        let schema =
            serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}});
        let v = tool_list_json_row(
            "11111111-1111-1111-1111-111111111111",
            "Read",
            "marcus",
            "Read file contents",
            &caps,
            &schema,
            42,
            Some(12.5),
            true,
            ts,
            created,
        );
        assert_eq!(v["id"], "11111111-1111-1111-1111-111111111111");
        assert_eq!(v["tool_name"], "Read");
        assert_eq!(v["node"], "marcus");
        // Fields the fixed-width table never shows are carried through.
        assert_eq!(v["description"], "Read file contents");
        assert_eq!(v["call_count"], 42);
        assert_eq!(v["avg_latency_ms"], 12.5);
        assert_eq!(v["capabilities_required"][1], "net");
        assert_eq!(
            v["parameters_schema"]["properties"]["path"]["type"],
            "string"
        );
        assert_eq!(v["healthy"], true);
        assert_eq!(v["health_checked_at"], "2026-06-13T12:00:00+00:00");
        assert_eq!(v["created_at"], "2026-06-01T08:30:00+00:00");
    }

    #[test]
    fn tool_list_json_row_null_latency_is_stable_null() {
        let ts = chrono::DateTime::parse_from_rfc3339("2026-06-13T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        // A never-measured tool: avg_latency_ms is JSON null, not omitted.
        let v = tool_list_json_row(
            "22222222-2222-2222-2222-222222222222",
            "Glob",
            "sophie",
            "",
            &[],
            &serde_json::json!({}),
            0,
            None,
            false,
            ts,
            ts,
        );
        assert!(v["avg_latency_ms"].is_null());
        assert!(v.get("avg_latency_ms").is_some());
        // Empty capabilities serialize to an empty array (stable shape).
        assert_eq!(v["capabilities_required"], serde_json::json!([]));
    }
}
