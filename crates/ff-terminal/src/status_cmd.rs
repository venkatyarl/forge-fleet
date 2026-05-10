use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use crate::{CYAN, GREEN, RED, RESET, YELLOW, truncate_str};

pub async fn handle_status(p: &Path) -> Result<()> {
    // Cap total runtime at 15s.
    let fut = handle_status_inner(p.to_path_buf());
    match tokio::time::timeout(Duration::from_secs(15), fut).await {
        Ok(r) => r,
        Err(_) => {
            println!("{RED}✗ ff status timed out after 15s{RESET}");
            Ok(())
        }
    }
}
pub async fn handle_status_inner(p: PathBuf) -> Result<()> {
    println!("{CYAN}━━━ ForgeFleet Status ━━━{RESET}");

    // Load fleet.toml (needed for redis URL and as a fallback for DB URL).
    let fleet_cfg: Option<ff_core::config::FleetConfig> = fs::read_to_string(&p)
        .ok()
        .and_then(|s| toml::from_str(&s).ok());

    // ── 1. Database ────────────────────────────────────────────────────────
    print!("{CYAN}Database{RESET}  : ");
    let pool_res = tokio::time::timeout(
        Duration::from_secs(3),
        ff_agent::fleet_info::get_fleet_pool(),
    )
    .await;
    let pool_opt: Option<sqlx::PgPool> = match pool_res {
        Ok(Ok(pool)) => {
            // Report the highest applied version number, not a row count.
            // COUNT(*) would return 39 on a V45 DB because Postgres migrations
            // start at V7 (V1-V6 are SQLite-only), giving 39 rows for 45 versions.
            let migs: Option<i64> = sqlx::query_scalar::<_, i64>(
                "SELECT COALESCE(MAX(version),0)::bigint FROM _migrations",
            )
            .fetch_one(&pool)
            .await
            .ok();
            match migs {
                Some(n) => println!("{GREEN}✓ connected{RESET} ({n} migrations applied)"),
                None => println!("{GREEN}✓ connected{RESET} (migrations table missing)"),
            }
            Some(pool)
        }
        Ok(Err(e)) => {
            println!("{RED}✗ unreachable{RESET} ({})", truncate_str(&e, 60));
            None
        }
        Err(_) => {
            println!("{RED}✗ unreachable{RESET} (timeout)");
            None
        }
    };

    // ── 2. Redis ───────────────────────────────────────────────────────────
    print!("{CYAN}Redis{RESET}     : ");
    let redis_url = fleet_cfg
        .as_ref()
        .map(|c| c.redis.url.clone())
        .unwrap_or_else(|| "redis://127.0.0.1:6380".to_string());
    match ping_redis(&redis_url).await {
        Ok(ms) => println!("{GREEN}✓ PONG{RESET} ({redis_url}, {ms}ms)"),
        Err(e) => println!(
            "{RED}✗ unreachable{RESET} ({redis_url}) — {}",
            truncate_str(&e, 50)
        ),
    }

    // ── 3. Fleet nodes ─────────────────────────────────────────────────────
    println!("{CYAN}Nodes{RESET}     :");
    let nodes: Vec<ff_db::FleetNodeRow> = match &pool_opt {
        Some(pool) => ff_db::pg_list_nodes(pool).await.unwrap_or_default(),
        None => Vec::new(),
    };
    if nodes.is_empty() {
        println!("  {YELLOW}(no nodes — DB unavailable or empty){RESET}");
    } else {
        // Probe SSH port 22 on each node in parallel.
        let probes: Vec<_> = nodes
            .iter()
            .map(|n| {
                let ip = n.ip.clone();
                async move { tcp_probe(&ip, 22, Duration::from_secs(2)).await }
            })
            .collect();
        let online: Vec<bool> = futures::future::join_all(probes).await;
        for (n, up) in nodes.iter().zip(online.iter()) {
            let status = if *up {
                format!("{GREEN}online{RESET}")
            } else {
                format!("{RED}offline{RESET}")
            };
            println!("  {:<10} {:<16} {:<10} {}", n.name, n.ip, n.runtime, status);
        }
    }

    // ── 4. Deployments ─────────────────────────────────────────────────────
    print!("{CYAN}Deployments{RESET}: ");
    if let Some(pool) = &pool_opt {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT health_status, COUNT(*)::bigint FROM fleet_model_deployments \
             GROUP BY health_status ORDER BY health_status",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        if rows.is_empty() {
            println!("{YELLOW}(none){RESET}");
        } else {
            let parts: Vec<String> = rows
                .iter()
                .map(|(s, c)| {
                    let color = match s.as_str() {
                        "healthy" => GREEN,
                        "unhealthy" => RED,
                        "starting" => YELLOW,
                        _ => RESET,
                    };
                    format!("{color}{s}={c}{RESET}")
                })
                .collect();
            println!("{}", parts.join("  "));
        }
    } else {
        println!("{RED}✗ unreachable{RESET}");
    }

    // ── 5. Model library ───────────────────────────────────────────────────
    print!("{CYAN}Library{RESET}   : ");
    if let Some(pool) = &pool_opt {
        let row: Option<(i64, i64)> = sqlx::query_as(
            "SELECT COUNT(*)::bigint, COALESCE(SUM(size_bytes), 0)::bigint FROM fleet_model_library"
        ).fetch_one(pool).await.ok();
        match row {
            Some((n, bytes)) => {
                let gib = (bytes as f64) / 1024.0 / 1024.0 / 1024.0;
                println!("{n} models, {gib:.1} GiB across fleet");
            }
            None => println!("{RED}✗ query failed{RESET}"),
        }
    } else {
        println!("{RED}✗ unreachable{RESET}");
    }

    // ── 6. Catalog ─────────────────────────────────────────────────────────
    print!("{CYAN}Catalog{RESET}   : ");
    if let Some(pool) = &pool_opt {
        let n: Option<i64> = sqlx::query_scalar("SELECT COUNT(*)::bigint FROM fleet_model_catalog")
            .fetch_one(pool)
            .await
            .ok();
        match n {
            Some(n) => println!("{n} entries"),
            None => println!("{RED}✗ query failed{RESET}"),
        }
    } else {
        println!("{RED}✗ unreachable{RESET}");
    }

    // ── 7. Disk usage ──────────────────────────────────────────────────────
    println!("{CYAN}Disk{RESET}      :");
    if let Some(pool) = &pool_opt {
        // Latest sample per node.
        let rows: Vec<(String, i64, i64, i64, i32)> = sqlx::query_as(
            "SELECT DISTINCT ON (d.node_name) \
                    d.node_name, d.total_bytes, d.used_bytes, d.models_bytes, \
                    COALESCE(n.disk_quota_pct, 80) \
             FROM fleet_disk_usage d \
             LEFT JOIN fleet_nodes n ON n.name = d.node_name \
             ORDER BY d.node_name, d.sampled_at DESC",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        if rows.is_empty() {
            println!("  {YELLOW}(no samples yet){RESET}");
        } else {
            for (name, total, used, models, quota) in rows {
                let total_gib = (total as f64) / 1024.0 / 1024.0 / 1024.0;
                let used_gib = (used as f64) / 1024.0 / 1024.0 / 1024.0;
                let models_gib = (models as f64) / 1024.0 / 1024.0 / 1024.0;
                let used_pct = if total > 0 {
                    (used as f64 / total as f64) * 100.0
                } else {
                    0.0
                };
                let over = used_pct >= quota as f64;
                let line = format!(
                    "  {:<10} {:5.1}/{:5.1} GiB ({:4.1}%)  models {:5.1} GiB  quota {}%",
                    name, used_gib, total_gib, used_pct, models_gib, quota
                );
                if over {
                    println!("{RED}{line}{RESET}");
                } else {
                    println!("{line}");
                }
            }
        }
    } else {
        println!("  {RED}✗ unreachable{RESET}");
    }

    // ── 8. Deferred tasks ──────────────────────────────────────────────────
    print!("{CYAN}Deferred{RESET}  : ");
    if let Some(pool) = &pool_opt {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT status, COUNT(*)::bigint FROM deferred_tasks \
             GROUP BY status ORDER BY status",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        if rows.is_empty() {
            println!("{YELLOW}(none){RESET}");
        } else {
            let parts: Vec<String> = rows
                .iter()
                .map(|(s, c)| {
                    if s == "failed" && *c > 0 {
                        format!("{RED}{s}={c}{RESET}")
                    } else {
                        format!("{s}={c}")
                    }
                })
                .collect();
            println!("{}", parts.join("  "));
        }
    } else {
        println!("{RED}✗ unreachable{RESET}");
    }

    // ── 9. In-flight jobs ──────────────────────────────────────────────────
    print!("{CYAN}Jobs{RESET}      : ");
    if let Some(pool) = &pool_opt {
        let n: Option<i64> = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM fleet_model_jobs WHERE status IN ('running','queued')",
        )
        .fetch_one(pool)
        .await
        .ok();
        match n {
            Some(0) => println!("0 in-flight"),
            Some(n) => println!("{YELLOW}{n} in-flight{RESET} (running or queued)"),
            None => println!("{RED}✗ query failed{RESET}"),
        }
    } else {
        println!("{RED}✗ unreachable{RESET}");
    }

    // ── 10. Secrets ───────────────────────────────────────────────────────
    print!("{CYAN}Secrets{RESET}   : ");
    if let Some(pool) = &pool_opt {
        let keys: Vec<(String,)> = sqlx::query_as("SELECT key FROM fleet_secrets ORDER BY key")
            .fetch_all(pool)
            .await
            .unwrap_or_default();
        if keys.is_empty() {
            println!("{YELLOW}(none){RESET}");
        } else {
            let list: Vec<String> = keys.into_iter().map(|(k,)| k).collect();
            println!("{}", list.join(", "));
        }
    } else {
        println!("{RED}✗ unreachable{RESET}");
    }

    Ok(())
}
pub async fn tcp_probe(host: &str, port: u16, timeout: Duration) -> bool {
    let addr = format!("{host}:{port}");
    matches!(
        tokio::time::timeout(timeout, tokio::net::TcpStream::connect(&addr)).await,
        Ok(Ok(_))
    )
}
pub async fn ping_redis(url: &str) -> std::result::Result<u128, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Parse redis://host:port (ignore auth/db for this health ping).
    let rest = url.strip_prefix("redis://").unwrap_or(url);
    let host_port = rest.split('/').next().unwrap_or(rest);
    // Strip userinfo if present.
    let host_port = host_port.rsplit('@').next().unwrap_or(host_port);
    let (host, port) = match host_port.rsplit_once(':') {
        // Host-facing default: docker-compose publishes Redis on 6380.
        Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(6380)),
        None => (host_port.to_string(), 6380),
    };

    let start = std::time::Instant::now();
    let connect = tokio::net::TcpStream::connect((host.as_str(), port));
    let mut stream = tokio::time::timeout(Duration::from_secs(3), connect)
        .await
        .map_err(|_| "connect timeout".to_string())?
        .map_err(|e| format!("connect: {e}"))?;

    tokio::time::timeout(Duration::from_secs(3), stream.write_all(b"PING\r\n"))
        .await
        .map_err(|_| "write timeout".to_string())?
        .map_err(|e| format!("write: {e}"))?;

    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buf))
        .await
        .map_err(|_| "read timeout".to_string())?
        .map_err(|e| format!("read: {e}"))?;

    let reply = String::from_utf8_lossy(&buf[..n]);
    if reply.starts_with("+PONG") {
        Ok(start.elapsed().as_millis())
    } else {
        Err(format!("unexpected reply: {}", reply.trim()))
    }
}
