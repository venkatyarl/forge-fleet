//! Node health sampler — local orchestrator tick that samples RAM, swap,
//! load-average, per-service RSS, and dmesg OOM-kills into Postgres.
//!
//! Runs on every node (like `disk_sampler`) and drives pressure actions:
//!   - `healthy`: business as usual.
//!   - `build_paused`: available RAM dropped below the build-reserve threshold;
//!     refuse new builds and warn operators.
//!   - `critical`: available RAM near exhaustion; emergency alert.
//!
//! Also verifies node-local services (`forgefleetd`, `rpc`, `llama-server-*`)
//! that systemd `Restart=` should already be healing — if a service is down we
//! attempt to restart it and alert.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use sqlx::PgPool;

/// Default interval between node-health samples.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// One node-health snapshot for the current node.
#[derive(Debug, Clone)]
pub struct NodeHealthSample {
    pub worker_name: String,
    pub mem_total_kb: u64,
    pub mem_available_kb: u64,
    pub mem_available_gb: f64,
    pub swap_total_kb: u64,
    pub swap_free_kb: u64,
    pub load_avg_1m: Option<f64>,
    pub load_avg_5m: Option<f64>,
    pub load_avg_15m: Option<f64>,
    pub service_rss: Vec<ServiceRss>,
    pub oom_kills: Vec<OomKill>,
    pub dmesg_cursor: Option<String>,
    pub pressure_state: PressureState,
    pub build_reserve_gb: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureState {
    Healthy,
    BuildPaused,
    Critical,
}

impl PressureState {
    fn as_str(self) -> &'static str {
        match self {
            PressureState::Healthy => "healthy",
            PressureState::BuildPaused => "build_paused",
            PressureState::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ServiceRss {
    pub name: String,
    pub rss_kb: u64,
    pub status: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct OomKill {
    pub ts: String,
    pub pid: i64,
    pub comm: String,
    pub oom_score: Option<i64>,
}

/// Default build-reserve floor in GiB. A box with less than this much available
/// RAM should not start new builds.
const DEFAULT_BUILD_RESERVE_GB: f64 = 4.0;

/// Fraction of total RAM used to scale the build reserve on large-memory nodes.
const BUILD_RESERVE_RAM_FRAC: f64 = 0.05;

/// Critical threshold: below this available RAM we treat the node as
/// memory-exhausted and emit an emergency alert.
const CRITICAL_AVAILABLE_GB: f64 = 1.0;

/// Services the local orchestrator is responsible for watching. Patterns are
/// matched against process command lines (`/proc/<pid>/cmdline`).
const WATCHED_SERVICE_PATTERNS: &[&str] = &["forgefleetd", "rpc", "llama-server"];

/// Path to a small local cursor file so we do not re-parse the whole dmesg
/// buffer on every tick.
fn dmesg_cursor_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    PathBuf::from(home).join(".forgefleet/node_health_dmesg_cursor")
}

/// Memory-guardrail reserve formula (405ba187). Keeps a fixed floor for small
/// nodes and scales with total RAM so large-memory boxes do not overcommit.
pub fn build_reserve_gb(total_ram_gb: f64) -> f64 {
    (DEFAULT_BUILD_RESERVE_GB).max(total_ram_gb * BUILD_RESERVE_RAM_FRAC)
}

/// Sample the current host's health and insert a row into `node_health`.
pub async fn sample_local_node_health(pool: &PgPool) -> Result<NodeHealthSample, String> {
    let worker_name = crate::fleet_info::resolve_this_worker_name().await;

    // /proc/meminfo gives us the ground-truth Linux memory counters.
    let meminfo = read_proc_meminfo().await?;
    let mem_total_kb = meminfo.get("MemTotal").copied().unwrap_or(0);
    let mem_available_kb = meminfo.get("MemAvailable").copied().unwrap_or(0);
    let swap_total_kb = meminfo.get("SwapTotal").copied().unwrap_or(0);
    let swap_free_kb = meminfo.get("SwapFree").copied().unwrap_or(0);

    let mem_available_gb = kb_to_gb(mem_available_kb);
    let total_ram_gb = kb_to_gb(mem_total_kb);
    let build_reserve_gb = build_reserve_gb(total_ram_gb);

    let load = read_proc_loadavg().await;

    let services = sample_service_rss().await;
    let (oom_kills, dmesg_cursor) = sample_dmesg_oom_kills().await;

    let pressure_state = if mem_available_gb < CRITICAL_AVAILABLE_GB {
        PressureState::Critical
    } else if mem_available_gb < build_reserve_gb {
        PressureState::BuildPaused
    } else {
        PressureState::Healthy
    };

    // Persist before acting, so the row is authoritative even if an action
    // handler fails.
    ff_db::pg_insert_node_health(
        pool,
        &worker_name,
        mem_total_kb as i64,
        mem_available_kb as i64,
        mem_available_gb,
        swap_total_kb as i64,
        swap_free_kb as i64,
        load.0,
        load.1,
        load.2,
        &json!(services),
        &json!(oom_kills),
        dmesg_cursor.as_deref(),
        pressure_state.as_str(),
        build_reserve_gb,
    )
    .await
    .map_err(|e| format!("pg_insert_node_health: {e}"))?;

    let sample = NodeHealthSample {
        worker_name: worker_name.clone(),
        mem_total_kb,
        mem_available_kb,
        mem_available_gb,
        swap_total_kb,
        swap_free_kb,
        load_avg_1m: load.0,
        load_avg_5m: load.1,
        load_avg_15m: load.2,
        service_rss: services,
        oom_kills,
        dmesg_cursor,
        pressure_state,
        build_reserve_gb,
    };

    // Pressure actions + self-heal are best-effort; never let them fail the tick.
    if let Err(e) = run_pressure_actions(pool, &sample).await {
        tracing::warn!(error = %e, "node_health pressure actions failed");
    }
    if let Err(e) = self_heal_services(pool, &worker_name).await {
        tracing::warn!(error = %e, "node_health self-heal failed");
    }

    Ok(sample)
}

/// Take pressure actions based on the current sample.
async fn run_pressure_actions(pool: &PgPool, sample: &NodeHealthSample) -> Result<(), String> {
    match sample.pressure_state {
        PressureState::Healthy => Ok(()),
        PressureState::BuildPaused => {
            tracing::warn!(
                worker = %sample.worker_name,
                avail_gb = %sample.mem_available_gb,
                reserve_gb = %sample.build_reserve_gb,
                "node health: available RAM below build reserve"
            );
            let title = format!(
                "⚠ {} build-paused: {:.1} GiB available < {:.1} GiB reserve",
                sample.worker_name, sample.mem_available_gb, sample.build_reserve_gb
            );
            let body = format!(
                "Node '{}' has {:.1} GiB RAM available, below the {:.1} GiB build-reserve threshold. \
                 New builds will be refused until memory recovers.",
                sample.worker_name, sample.mem_available_gb, sample.build_reserve_gb
            );
            let _ = crate::telegram::send_telegram_from_secrets(pool, &title, &body).await;
            maybe_enqueue_build_pause_alert(pool, &sample.worker_name, sample).await;
            Ok(())
        }
        PressureState::Critical => {
            tracing::error!(
                worker = %sample.worker_name,
                avail_gb = %sample.mem_available_gb,
                "node health: critical RAM pressure"
            );
            let title = format!(
                "🚨 {} CRITICAL RAM: {:.1} GiB available",
                sample.worker_name, sample.mem_available_gb
            );
            let body = format!(
                "Node '{}' is critically low on memory ({:.1} GiB available). \
                 Consider unloading the lowest-priority model immediately.",
                sample.worker_name, sample.mem_available_gb
            );
            let _ = crate::telegram::send_telegram_from_secrets(pool, &title, &body).await;
            maybe_enqueue_emergency_unload(pool, &sample.worker_name, sample).await;
            Ok(())
        }
    }
}

/// Enqueue a deferred task so the leader sees the build-pause condition and
/// can refuse new work on this node.
async fn maybe_enqueue_build_pause_alert(
    pool: &PgPool,
    worker_name: &str,
    sample: &NodeHealthSample,
) {
    let title = format!("⚠ build-paused on {worker_name}");
    let payload = json!({
        "avail_gb": sample.mem_available_gb,
        "reserve_gb": sample.build_reserve_gb,
        "note": format!(
            "Node '{}' is below build-reserve RAM ({:.1} < {:.1} GiB). Refuse new builds.",
            worker_name, sample.mem_available_gb, sample.build_reserve_gb
        ),
    });
    let _ = ff_db::pg_enqueue_deferred(
        pool,
        &title,
        "manual",
        &payload,
        "manual",
        &json!({}),
        Some(worker_name),
        &json!([]),
        Some("node-health"),
        Some(1),
    )
    .await;
}

/// Enqueue an emergency deferred task asking an operator to unload the
/// lowest-priority model.
async fn maybe_enqueue_emergency_unload(
    pool: &PgPool,
    worker_name: &str,
    sample: &NodeHealthSample,
) {
    let title = format!("🚨 emergency RAM unload needed on {worker_name}");
    let payload = json!({
        "avail_gb": sample.mem_available_gb,
        "note": format!(
            "Node '{}' is critically low on memory ({:.1} GiB available). \
             Unload the lowest-priority model with: ff model unload <node> <model>",
            worker_name, sample.mem_available_gb
        ),
    });
    let _ = ff_db::pg_enqueue_deferred(
        pool,
        &title,
        "manual",
        &payload,
        "manual",
        &json!({}),
        Some(worker_name),
        &json!([]),
        Some("node-health"),
        Some(1),
    )
    .await;
}

/// Verify watched services are active. If systemd reports a service as inactive,
/// attempt to start it and alert. systemd Restart= should handle most cases;
/// this layer is the verification + observability backstop.
async fn self_heal_services(pool: &PgPool, worker_name: &str) -> Result<(), String> {
    for pattern in WATCHED_SERVICE_PATTERNS {
        let service_name = guess_systemd_service(pattern);
        match service_status(&service_name).await {
            Some("active") | Some("activating") => continue,
            Some(status) => {
                tracing::warn!(
                    service = %service_name,
                    status = %status,
                    "watched service not active; attempting restart"
                );
                let restart_result = systemctl_start(&service_name).await;
                let title = format!("🔧 {worker_name}: restarted {service_name}");
                let body = format!(
                    "Service '{service_name}' on '{worker_name}' was in state '{status}'; \
                     systemd restart attempted. Result: {result}.",
                    result = if restart_result.is_ok() {
                        "ok"
                    } else {
                        "failed"
                    }
                );
                let _ = crate::telegram::send_telegram_from_secrets(pool, &title, &body).await;
            }
            None => {
                tracing::debug!(service = %service_name, "could not determine service status");
            }
        }
    }
    Ok(())
}

fn guess_systemd_service(pattern: &str) -> String {
    match pattern {
        "forgefleetd" => "forgefleetd".into(),
        "rpc" => "forgefleetd-rpc".into(),
        _ => format!("{pattern}.service"),
    }
}

const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(5);
const DMESG_TIMEOUT: Duration = Duration::from_secs(5);

async fn service_status(service: &str) -> Option<&'static str> {
    let output = tokio::time::timeout(
        SYSTEMCTL_TIMEOUT,
        tokio::process::Command::new("systemctl")
            .args(["is-active", service])
            .output(),
    )
    .await
    .ok()
    .and_then(|r| r.ok())?;
    let text = String::from_utf8_lossy(&output.stdout);
    let status = text.lines().next()?.trim();
    match status {
        "active" => Some("active"),
        "inactive" => Some("inactive"),
        "failed" => Some("failed"),
        "activating" => Some("activating"),
        _ => Some("unknown"),
    }
}

async fn systemctl_start(service: &str) -> Result<(), String> {
    let output = tokio::time::timeout(
        SYSTEMCTL_TIMEOUT,
        tokio::process::Command::new("systemctl")
            .args(["start", service])
            .output(),
    )
    .await
    .map_err(|_| format!("systemctl start {service}: timed out after {SYSTEMCTL_TIMEOUT:?}"))?
    .map_err(|e| format!("systemctl start {service}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "systemctl start {service} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

async fn read_proc_meminfo() -> Result<HashMap<String, u64>, String> {
    tokio::task::spawn_blocking(|| {
        let text = std::fs::read_to_string("/proc/meminfo")
            .map_err(|e| format!("read /proc/meminfo: {e}"))?;
        let mut map = HashMap::new();
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            let Some(key) = parts.next() else { continue };
            let key = key.trim_end_matches(':');
            let Some(val) = parts.next() else { continue };
            if let Ok(v) = val.parse::<u64>() {
                map.insert(key.to_string(), v);
            }
        }
        Ok(map)
    })
    .await
    .map_err(|e| format!("spawn_blocking /proc/meminfo: {e}"))?
}

async fn read_proc_loadavg() -> (Option<f64>, Option<f64>, Option<f64>) {
    tokio::task::spawn_blocking(|| {
        let text = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
        let parts: Vec<&str> = text.split_whitespace().collect();
        let f = |i: usize| parts.get(i).and_then(|s| s.parse::<f64>().ok());
        (f(0), f(1), f(2))
    })
    .await
    .unwrap_or((None, None, None))
}

/// Sum RSS (resident set size) for processes whose command line matches any of
/// the watched service patterns. Falls back to `ps` when /proc is unreadable.
async fn sample_service_rss() -> Vec<ServiceRss> {
    let mut by_pattern: HashMap<String, u64> = HashMap::new();

    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if !name.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let pid_dir = entry.path();
            let cmdline_path = pid_dir.join("cmdline");
            let Ok(cmdline) = std::fs::read_to_string(&cmdline_path) else {
                continue;
            };
            if cmdline.is_empty() {
                continue;
            }
            for pattern in WATCHED_SERVICE_PATTERNS {
                if cmdline.contains(pattern) {
                    let statm_path = pid_dir.join("statm");
                    let rss_pages = std::fs::read_to_string(&statm_path)
                        .ok()
                        .and_then(|s| {
                            s.split_whitespace()
                                .nth(1)
                                .and_then(|v| v.parse::<u64>().ok())
                        })
                        .unwrap_or(0);
                    let rss_kb = rss_pages * page_size_kb();
                    *by_pattern.entry((*pattern).to_string()).or_insert(0) += rss_kb;
                }
            }
        }
    }

    // Fallback: if /proc gave us nothing, try `ps aux` once.
    if by_pattern.is_empty() {
        if let Ok(output) = tokio::process::Command::new("ps")
            .args(["aux"])
            .output()
            .await
        {
            let text = String::from_utf8_lossy(&output.stdout);
            for line in text.lines().skip(1) {
                let cols: Vec<&str> = line.split_whitespace().collect();
                if cols.len() < 11 {
                    continue;
                }
                let cmd = cols[10..].join(" ");
                let Ok(rss_kb) = cols[5].parse::<u64>() else {
                    continue;
                };
                for pattern in WATCHED_SERVICE_PATTERNS {
                    if cmd.contains(pattern) {
                        *by_pattern.entry((*pattern).to_string()).or_insert(0) += rss_kb;
                    }
                }
            }
        }
    }

    let mut services: Vec<ServiceRss> = by_pattern
        .into_iter()
        .map(|(name, rss_kb)| ServiceRss {
            name,
            rss_kb,
            status: "running".into(),
        })
        .collect();
    services.sort_by(|a, b| a.name.cmp(&b.name));
    services
}

fn page_size_kb() -> u64 {
    // Linux standard page size is 4 KiB. Reading getpagesize is overkill for a
    // best-effort RSS estimate; this matches `ps` RSS units on x86_64/arm64.
    4
}

/// Parse dmesg for OOM-kill lines, using a cursor file to avoid re-processing.
/// Returns newly seen OOM kills and the new cursor value.
async fn sample_dmesg_oom_kills() -> (Vec<OomKill>, Option<String>) {
    let output = match tokio::time::timeout(
        DMESG_TIMEOUT,
        tokio::process::Command::new("dmesg")
            .args(["--level=err", "--time-format=iso"])
            .output(),
    )
    .await
    {
        Ok(Ok(o)) if o.status.success() => o,
        _ => return (Vec::new(), None),
    };

    let text = String::from_utf8_lossy(&output.stdout);
    let cursor = read_dmesg_cursor();
    let mut new_kills = Vec::new();
    let mut last_ts: Option<String> = None;

    for line in text.lines() {
        // ISO format begins with a timestamp like "2026-07-26T18:08:49,...".
        let Some(ts_end) = line.find(' ') else {
            continue;
        };
        let ts = &line[..ts_end];
        last_ts = Some(ts.to_string());

        if let Some(ref c) = cursor {
            if ts <= c.as_str() {
                continue;
            }
        }

        if !line.contains("Out of memory") && !line.contains("Killed process") {
            continue;
        }

        let pid = parse_dmesg_pid(line).unwrap_or(0);
        let comm = parse_dmesg_comm(line).unwrap_or_else(|| "?".into());
        new_kills.push(OomKill {
            ts: ts.to_string(),
            pid,
            comm,
            oom_score: None,
        });
    }

    let new_cursor = last_ts.filter(|_| !new_kills.is_empty() || cursor.is_some());
    if let Some(ref c) = new_cursor {
        let _ = write_dmesg_cursor(c);
    }

    (new_kills, new_cursor)
}

fn parse_dmesg_pid(line: &str) -> Option<i64> {
    // "Killed process 12345 (foo)" or "oom-kill:constraint=CONSTRAINT_NONE...,task=foo,pid=12345,..."
    if let Some(start) = line.find("Killed process ") {
        let rest = &line[start + 14..];
        return rest.split_whitespace().next()?.parse().ok();
    }
    line.split(',').find_map(|part| {
        let (k, v) = part.split_once('=')?;
        if k.trim() == "pid" {
            v.trim().parse().ok()
        } else {
            None
        }
    })
}

fn parse_dmesg_comm(line: &str) -> Option<String> {
    if let Some(start) = line.find("Killed process ") {
        let rest = &line[start + 14..];
        let mut parts = rest.split_whitespace();
        let _pid = parts.next()?;
        let comm = parts.next()?;
        return Some(
            comm.trim_start_matches('(')
                .trim_end_matches(')')
                .to_string(),
        );
    }
    line.split(',').find_map(|part| {
        let (k, v) = part.split_once('=')?;
        if k.trim() == "task" {
            Some(v.trim().to_string())
        } else {
            None
        }
    })
}

fn read_dmesg_cursor() -> Option<String> {
    std::fs::read_to_string(dmesg_cursor_path())
        .ok()
        .map(|s| s.trim().to_string())
}

fn write_dmesg_cursor(cursor: &str) -> Result<(), std::io::Error> {
    let path = dmesg_cursor_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, cursor)
}

fn kb_to_gb(kb: u64) -> f64 {
    kb as f64 / 1_048_576.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_reserve_scales_with_total_ram() {
        assert!((build_reserve_gb(2.0) - 4.0).abs() < f64::EPSILON);
        assert!((build_reserve_gb(64.0) - 4.0).abs() < f64::EPSILON);
        assert!((build_reserve_gb(256.0) - 12.8).abs() < 0.01);
    }

    #[test]
    fn parse_dmesg_oom_lines() {
        let line = "Killed process 12345 (llama-server) total-vm:123456kB, anon-rss:65432kB";
        assert_eq!(parse_dmesg_pid(line), Some(12345));
        assert_eq!(parse_dmesg_comm(line).as_deref(), Some("llama-server"));

        let line2 = "task=forgefleetd,pid=99999,uid=1000";
        assert_eq!(parse_dmesg_pid(line2), Some(99999));
        assert_eq!(parse_dmesg_comm(line2).as_deref(), Some("forgefleetd"));
    }

    #[test]
    fn pressure_state_from_ram() {
        let reserve = 4.0;
        assert_eq!(pressure_state_for(10.0, reserve), PressureState::Healthy);
        assert_eq!(pressure_state_for(2.5, reserve), PressureState::BuildPaused);
        assert_eq!(pressure_state_for(0.5, reserve), PressureState::Critical);
    }

    fn pressure_state_for(avail_gb: f64, reserve_gb: f64) -> PressureState {
        if avail_gb < CRITICAL_AVAILABLE_GB {
            PressureState::Critical
        } else if avail_gb < reserve_gb {
            PressureState::BuildPaused
        } else {
            PressureState::Healthy
        }
    }

    // -- DB test: early-return (skip) when no Postgres is configured; CI's
    //    `cargo test --lib` has no database and must never panic here.

    fn temp_db_urls() -> Option<(String, String, String)> {
        let base_url = std::env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
            .ok()?;
        let (prefix, _) = base_url.rsplit_once('/')?;
        let db_name = format!("ff_node_health_{}", uuid::Uuid::new_v4().simple());
        Some((
            format!("{prefix}/postgres"),
            format!("{prefix}/{db_name}"),
            db_name,
        ))
    }

    async fn create_temp_db() -> Option<(sqlx::PgPool, sqlx::PgPool, String)> {
        let (admin_url, db_url, db_name) = temp_db_urls()?;
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&admin_url)
            .await
            .expect("connect admin db");
        sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
            .execute(&admin)
            .await
            .expect("create temp db");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect(&db_url)
            .await
            .expect("connect temp db");
        sqlx::raw_sql(
            "CREATE TABLE IF NOT EXISTS fleet_workers (
                 name TEXT PRIMARY KEY,
                 ip TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS node_health (
                 worker_name          TEXT NOT NULL,
                 sampled_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                 mem_total_kb         BIGINT NOT NULL,
                 mem_available_kb     BIGINT NOT NULL,
                 mem_available_gb     DOUBLE PRECISION NOT NULL,
                 swap_total_kb        BIGINT NOT NULL DEFAULT 0,
                 swap_free_kb         BIGINT NOT NULL DEFAULT 0,
                 load_avg_1m          DOUBLE PRECISION,
                 load_avg_5m          DOUBLE PRECISION,
                 load_avg_15m         DOUBLE PRECISION,
                 service_rss_json     JSONB NOT NULL DEFAULT '[]'::jsonb,
                 oom_kills_json       JSONB NOT NULL DEFAULT '[]'::jsonb,
                 dmesg_cursor         TEXT,
                 pressure_state       TEXT NOT NULL DEFAULT 'healthy',
                 build_reserve_gb     DOUBLE PRECISION NOT NULL DEFAULT 4.0,
                 PRIMARY KEY (worker_name, sampled_at)
             );",
        )
        .execute(&pool)
        .await
        .expect("create minimal node_health schema");
        Some((admin, pool, db_name))
    }

    async fn drop_temp_db(admin: sqlx::PgPool, pool: sqlx::PgPool, db_name: &str) {
        pool.close().await;
        sqlx::query(
            "SELECT pg_terminate_backend(pid)
               FROM pg_stat_activity
              WHERE datname = $1
                AND pid <> pg_backend_pid()",
        )
        .bind(db_name)
        .execute(&admin)
        .await
        .ok();
        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db_name}\""))
            .execute(&admin)
            .await
            .ok();
        admin.close().await;
    }

    #[tokio::test]
    async fn sample_local_node_health_inserts_row() {
        let Some((admin, pool, db_name)) = create_temp_db().await else {
            eprintln!("skipping: FORGEFLEET_POSTGRES_URL/FORGEFLEET_DATABASE_URL not set");
            return;
        };

        // This test requires a Linux /proc/meminfo. Skip gracefully elsewhere.
        if std::fs::read_to_string("/proc/meminfo").is_err() {
            eprintln!("skipping: /proc/meminfo not readable (non-Linux)");
            drop_temp_db(admin, pool, &db_name).await;
            return;
        }

        // Setting an env var in a test is technically unsafe in Rust 2024;
        // this is the only test in this process and no other thread reads it.
        unsafe {
            std::env::set_var("FORGEFLEET_NODE_NAME", "node-health-test-node");
        }
        sqlx::query(
            "INSERT INTO fleet_workers (name, ip) VALUES ('node-health-test-node', '127.0.0.1')",
        )
        .execute(&pool)
        .await
        .expect("insert test node");

        let sample = sample_local_node_health(&pool)
            .await
            .expect("sample local node health");

        assert_eq!(sample.worker_name, "node-health-test-node");
        assert!(sample.mem_total_kb > 0);

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM node_health WHERE worker_name = $1")
                .bind("node-health-test-node")
                .fetch_one(&pool)
                .await
                .expect("count rows");
        assert_eq!(count, 1);

        drop_temp_db(admin, pool, &db_name).await;
    }
}
