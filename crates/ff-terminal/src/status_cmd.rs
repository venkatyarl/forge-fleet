use crate::{CYAN, GREEN, RED, RESET, YELLOW, truncate_str};
use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
        .unwrap_or_else(|| "redis://127.0.0.1:56379".to_string());
    match ping_redis(&redis_url).await {
        Ok(ms) => println!("{GREEN}✓ PONG{RESET} ({redis_url}, {ms}ms)"),
        Err(e) => println!(
            "{RED}✗ unreachable{RESET} ({redis_url}) — {}",
            truncate_str(&e, 50)
        ),
    }

    // ── 3. Fleet ───────────────────────────────────────────────────────────
    // Print one expanded block per computer pulling from the rich `computers`
    // + `fleet_workers` + `computer_model_deployments` + `llm_clusters` joins
    // — role, OS, CPU/RAM, GPU, unified-memory flag, and currently-deployed
    // models. The old single-line "Nodes" output buried all of this behind
    // `runtime='native'` (which is a bogus enrollment default for every host).
    if let Some(pool) = &pool_opt {
        render_fleet_section(pool).await;
    } else {
        println!("{CYAN}Fleet{RESET}     :");
        println!("  {YELLOW}(no fleet data — DB unavailable){RESET}");
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
            "SELECT DISTINCT ON (d.worker_name) \
                    d.worker_name, d.total_bytes, d.used_bytes, d.models_bytes, \
                    COALESCE(n.disk_quota_pct, 80) \
             FROM fleet_disk_usage d \
             LEFT JOIN fleet_workers n ON n.name = d.worker_name \
             ORDER BY d.worker_name, d.sampled_at DESC",
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
async fn render_fleet_section(pool: &sqlx::PgPool) {
    // One row per computer with the fields we need to display.
    // We sort by role priority (leader > standby > worker) then ip so the
    // leader is always at the top — matches `ff fleet computers`.
    #[derive(sqlx::FromRow)]
    struct Row {
        name: String,
        primary_ip: String,
        role: String,
        os_family: String,
        cpu_cores: i32,
        total_ram_gb: i32,
        has_gpu: bool,
        gpu_kind: String,
        gpu_model: Option<String>,
        gpu_total_vram_gb: Option<i32>,
    }

    let rows: Vec<Row> = match sqlx::query_as::<_, Row>(
        "SELECT c.name, c.primary_ip,
                COALESCE(fw.role, 'unknown') AS role,
                COALESCE(c.os_family, 'unknown') AS os_family,
                COALESCE(c.cpu_cores, 0) AS cpu_cores,
                COALESCE(c.total_ram_gb, 0) AS total_ram_gb,
                COALESCE(c.has_gpu, false) AS has_gpu,
                COALESCE(c.gpu_kind, 'none') AS gpu_kind,
                c.gpu_model,
                c.gpu_total_vram_gb
         FROM computers c
         LEFT JOIN fleet_workers fw ON fw.name = c.name
         ORDER BY
            CASE COALESCE(fw.role,'')
                WHEN 'leader' THEN 0
                WHEN 'standby' THEN 1
                WHEN 'worker' THEN 2
                ELSE 9
            END,
            string_to_array(c.primary_ip, '.')::int[]",
    )
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            println!("{CYAN}Fleet{RESET}     : {RED}✗ query failed: {}{RESET}", e);
            return;
        }
    };

    // Counts for the header.
    let leaders = rows.iter().filter(|r| r.role == "leader").count();
    let standbys = rows.iter().filter(|r| r.role == "standby").count();
    let workers = rows.iter().filter(|r| r.role == "worker").count();
    println!(
        "{CYAN}Fleet{RESET}     : {} total — {} leader, {} standby, {} worker",
        rows.len(),
        leaders,
        standbys,
        workers
    );

    // SSH probes in parallel.
    let probes: Vec<_> = rows
        .iter()
        .map(|r| {
            let ip = r.primary_ip.clone();
            async move { tcp_probe(&ip, 22, Duration::from_secs(2)).await }
        })
        .collect();
    let online: Vec<bool> = futures::future::join_all(probes).await;

    // Models per computer (one row per deployment).
    #[derive(sqlx::FromRow)]
    struct DepRow {
        computer_name: String,
        model_id: String,
        port: i32,
        status: String,
    }
    let deps: Vec<DepRow> = sqlx::query_as::<_, DepRow>(
        "SELECT c.name AS computer_name, d.model_id, \
                COALESCE((string_to_array(d.endpoint, ':'))[3]::int, 0) AS port, \
                d.status \
         FROM computer_model_deployments d \
         JOIN computers c ON c.id = d.computer_id \
         ORDER BY c.name, port",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    // Cluster membership: which computers are in which cluster, and as what role.
    #[derive(sqlx::FromRow)]
    struct ClusterMembership {
        computer_name: String,
        cluster_id: String,
        cluster_role: String,
        model_id: String,
        topology: String,
    }
    let clusters: Vec<ClusterMembership> = sqlx::query_as::<_, ClusterMembership>(
        "SELECT c.name AS computer_name, cl.id AS cluster_id, \
                'head' AS cluster_role, cl.model_id, cl.topology \
         FROM llm_clusters cl JOIN computers c ON c.id = cl.head_computer_id \
         UNION ALL \
         SELECT c.name, cl.id, 'worker' AS cluster_role, cl.model_id, cl.topology \
         FROM llm_clusters cl \
         CROSS JOIN LATERAL jsonb_array_elements_text(cl.worker_computer_ids) AS wid(id) \
         JOIN computers c ON c.id = wid.id::uuid",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    for (r, up) in rows.iter().zip(online.iter()) {
        let status_tag = if *up {
            format!("{GREEN}online{RESET}")
        } else {
            format!("{RED}offline{RESET}")
        };
        let role_tag = match r.role.as_str() {
            "leader" => format!("{GREEN}leader{RESET}"),
            "standby" => format!("{YELLOW}standby{RESET}"),
            "worker" => "worker".to_string(),
            other => other.to_string(),
        };
        let hw = if r.has_gpu {
            // Friendlier label per gpu_kind. Unified-memory architectures
            // (Apple Silicon, NVIDIA GB10 Grace+Blackwell) share host RAM as
            // VRAM — flag them so the operator knows total_ram_gb is the
            // usable model capacity, not separate from GPU memory.
            let (label, unified) = match r.gpu_kind.as_str() {
                "apple_silicon" => ("Apple Silicon", true),
                "gb10" => ("NVIDIA GB10", true),
                "nvidia_cuda" => ("NVIDIA CUDA", false),
                "amd_rocm" => ("AMD ROCm", false),
                other => (other, false),
            };
            let detail = match (&r.gpu_model, r.gpu_total_vram_gb) {
                (Some(m), Some(v)) if !m.is_empty() => format!(" {m} {v}GB"),
                (Some(m), None) if !m.is_empty() => format!(" {m}"),
                (_, Some(v)) => format!(" {v}GB VRAM"),
                _ => String::new(),
            };
            let unified_tag = if unified { " (unified)" } else { "" };
            format!("{label}{detail}{unified_tag}")
        } else {
            "(no GPU)".to_string()
        };
        println!(
            "  {name:<10} {ip:<16} {role:<8}  {os:<14} {cores}C/{ram}GB  {hw}  {status}",
            name = r.name,
            ip = r.primary_ip,
            role = role_tag,
            os = r.os_family,
            cores = r.cpu_cores,
            ram = r.total_ram_gb,
            hw = hw,
            status = status_tag,
        );
        // Deployments line (only if this computer has any).
        let my_deps: Vec<&DepRow> = deps.iter().filter(|d| d.computer_name == r.name).collect();
        if !my_deps.is_empty() {
            let parts: Vec<String> = my_deps
                .iter()
                .map(|d| {
                    let short = d.model_id.rsplit('/').next().unwrap_or(&d.model_id);
                    let short = short.strip_suffix(".gguf").unwrap_or(short);
                    let color = if d.status == "active" { GREEN } else { YELLOW };
                    format!("{color}{}@{}{RESET}", short, d.port)
                })
                .collect();
            println!("             models: {}", parts.join(", "));
        }
        // Cluster line (only if this computer is in any cluster).
        let my_clusters: Vec<&ClusterMembership> = clusters
            .iter()
            .filter(|c| c.computer_name == r.name)
            .collect();
        if !my_clusters.is_empty() {
            let parts: Vec<String> = my_clusters
                .iter()
                .map(|c| {
                    format!(
                        "{CYAN}{}{RESET} ({} {} {})",
                        c.cluster_id, c.cluster_role, c.topology, c.model_id
                    )
                })
                .collect();
            println!("             cluster: {}", parts.join(", "));
        }
    }
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
