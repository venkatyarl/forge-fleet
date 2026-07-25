//! Every-node RAM/CPU/OOM monitor and local pressure response.

use std::{
    collections::BTreeMap,
    fs,
    process::Stdio,
    sync::{Mutex, OnceLock},
};

use anyhow::{Context, Result};
use serde_json::json;
use sqlx::{PgPool, Row};
use tokio::process::Command;
use tracing::{info, warn};

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
const CRITICAL_AVAILABLE_GB: f64 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pressure {
    Healthy,
    Pressure,
    Critical,
}

impl Pressure {
    fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Pressure => "pressure",
            Self::Critical => "critical",
        }
    }
}

#[derive(Debug)]
struct Sample {
    mem_total: u64,
    mem_available: u64,
    swap_total: u64,
    swap_free: u64,
    cpu_pct: f64,
    load: [f64; 3],
    service_rss: BTreeMap<String, u64>,
    oom_kills: i32,
}

/// Shared memory guardrail for build admission and model placement.
///
/// Small hosts retain at least 8 GiB; larger builders retain 25% up to the
/// fleet's canonical 40 GiB build reserve.
pub fn build_reserve_gb(total_ram_gb: f64) -> f64 {
    (total_ram_gb * 0.25).clamp(8.0, 40.0)
}

fn classify(available_gb: f64, reserve_gb: f64, oom_kills: i32) -> Pressure {
    if available_gb < CRITICAL_AVAILABLE_GB || oom_kills > 0 {
        Pressure::Critical
    } else if available_gb < reserve_gb {
        Pressure::Pressure
    } else {
        Pressure::Healthy
    }
}

pub async fn run_node_health_tick(pg: &PgPool, worker_name: &str) -> Result<()> {
    let sample = tokio::task::spawn_blocking(sample_local)
        .await
        .context("join node-health sampler")??;
    let total_gb = sample.mem_total as f64 / GIB;
    let available_gb = sample.mem_available as f64 / GIB;
    let reserve_gb = build_reserve_gb(total_gb);
    let pressure = classify(available_gb, reserve_gb, sample.oom_kills);
    let mut actions = verify_services().await;

    if actions.iter().any(|action| action.starts_with("restart")) {
        let body = format!("{worker_name}: {}", actions.join(", "));
        if let Err(error) =
            crate::telegram::send_telegram_from_secrets(pg, "⚕️ Node service self-heal", &body)
                .await
        {
            warn!(%error, "node_health: service self-heal alert failed");
        }
    }
    if pressure != Pressure::Healthy {
        if let Some(action) = shed_model(pg, worker_name).await {
            actions.push(action);
        }
    }
    if pressure == Pressure::Critical {
        let body = format!(
            "{worker_name}: available={available_gb:.1}GiB reserve={reserve_gb:.1}GiB oom_kills={}",
            sample.oom_kills
        );
        if let Err(error) =
            crate::telegram::send_telegram_from_secrets(pg, "🚨 Node memory critical", &body).await
        {
            warn!(%error, "node_health: Telegram alert failed");
        }
    }

    sqlx::query(
        "INSERT INTO node_health
         (worker_name, mem_total_bytes, mem_available_bytes, swap_total_bytes,
          swap_free_bytes, cpu_pct, load_1, load_5, load_15, service_rss,
          oom_kills, pressure, build_reserve_gb, builds_allowed, actions)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)",
    )
    .bind(worker_name)
    .bind(sample.mem_total as i64)
    .bind(sample.mem_available as i64)
    .bind(sample.swap_total as i64)
    .bind(sample.swap_free as i64)
    .bind(sample.cpu_pct)
    .bind(sample.load[0])
    .bind(sample.load[1])
    .bind(sample.load[2])
    .bind(json!(sample.service_rss))
    .bind(sample.oom_kills)
    .bind(pressure.as_str())
    .bind(reserve_gb)
    .bind(pressure == Pressure::Healthy)
    .bind(json!(actions))
    .execute(pg)
    .await?;

    info!(
        worker_name,
        pressure = pressure.as_str(),
        available_gb,
        reserve_gb,
        "node health sampled"
    );
    Ok(())
}

fn sample_local() -> Result<Sample> {
    let mem = fs::read_to_string("/proc/meminfo").context("read /proc/meminfo")?;
    let kb = |key: &str| -> Result<u64> {
        mem.lines()
            .find(|line| line.starts_with(key))
            .and_then(|line| line.split_whitespace().nth(1))
            .context("missing meminfo field")?
            .parse::<u64>()
            .context("parse meminfo field")
            .map(|value| value * 1024)
    };
    let load_text = fs::read_to_string("/proc/loadavg").context("read /proc/loadavg")?;
    let mut loads = load_text.split_whitespace();
    let load = [
        loads.next().context("load1")?.parse()?,
        loads.next().context("load5")?.parse()?,
        loads.next().context("load15")?.parse()?,
    ];

    Ok(Sample {
        mem_total: kb("MemTotal:")?,
        mem_available: kb("MemAvailable:")?,
        swap_total: kb("SwapTotal:")?,
        swap_free: kb("SwapFree:")?,
        cpu_pct: sample_cpu_pct()?,
        load,
        service_rss: service_rss(),
        oom_kills: recent_oom_kills(),
    })
}

fn sample_cpu_pct() -> Result<f64> {
    static PREVIOUS: OnceLock<Mutex<Option<(u64, u64)>>> = OnceLock::new();
    let stat = fs::read_to_string("/proc/stat").context("read /proc/stat")?;
    let values: Vec<u64> = stat
        .lines()
        .next()
        .context("missing cpu line")?
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse().ok())
        .collect();
    let total: u64 = values.iter().sum();
    let idle = values.get(3).copied().unwrap_or(0) + values.get(4).copied().unwrap_or(0);
    let mut previous = PREVIOUS.get_or_init(|| Mutex::new(None)).lock().unwrap();
    let pct = previous.map_or(0.0, |(old_total, old_idle)| {
        let delta = total.saturating_sub(old_total);
        if delta == 0 {
            0.0
        } else {
            100.0 * (delta.saturating_sub(idle.saturating_sub(old_idle))) as f64 / delta as f64
        }
    });
    *previous = Some((total, idle));
    Ok(pct)
}

fn service_rss() -> BTreeMap<String, u64> {
    let mut rss = BTreeMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return rss;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
            continue;
        }
        let path = entry.path();
        let Ok(comm) = fs::read_to_string(path.join("comm")) else {
            continue;
        };
        let name = comm.trim();
        if !matches!(name, "forgefleetd" | "llama-server" | "ollama" | "ff-rpc") {
            continue;
        }
        let bytes = fs::read_to_string(path.join("status"))
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find(|line| line.starts_with("VmRSS:"))
                    .and_then(|line| line.split_whitespace().nth(1))
                    .and_then(|v| v.parse::<u64>().ok())
            })
            .unwrap_or(0)
            * 1024;
        *rss.entry(name.to_string()).or_default() += bytes;
    }
    rss
}

fn recent_oom_kills() -> i32 {
    let output = std::process::Command::new("dmesg")
        .args(["--since", "35 seconds ago"])
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else { return 0 };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("out of memory") || lower.contains("killed process")
        })
        .count()
        .try_into()
        .unwrap_or(i32::MAX)
}

async fn verify_services() -> Vec<String> {
    let mut actions = Vec::new();
    for unit in [
        "forgefleet-mcp.service",
        "forgefleet-rpc.service",
        "llama.service",
    ] {
        let enabled = Command::new("systemctl")
            .args(["--user", "is-enabled", "--quiet", unit])
            .status()
            .await;
        if !enabled.is_ok_and(|status| status.success()) {
            continue;
        }
        let active = Command::new("systemctl")
            .args(["--user", "is-active", "--quiet", unit])
            .status()
            .await;
        if active.is_ok_and(|status| status.success()) {
            continue;
        }
        match Command::new("systemctl")
            .args(["--user", "restart", unit])
            .status()
            .await
        {
            Ok(status) if status.success() => actions.push(format!("restarted:{unit}")),
            Ok(status) => actions.push(format!(
                "restart_failed:{unit}:{}",
                status.code().unwrap_or(-1)
            )),
            Err(error) => actions.push(format!("restart_failed:{unit}:{error}")),
        }
    }
    actions
}

async fn shed_model(pg: &PgPool, worker_name: &str) -> Option<String> {
    let row = sqlx::query(
        "SELECT id::text
           FROM fleet_model_deployments
          WHERE worker_name = $1 AND health_status IN ('healthy','starting')
          ORDER BY request_count ASC, last_health_at ASC NULLS FIRST
          LIMIT 1",
    )
    .bind(worker_name)
    .fetch_optional(pg)
    .await
    .ok()??;
    let id: String = row.get(0);
    match crate::model_runtime::unload_model(pg, &id).await {
        Ok(()) => Some(format!("unloaded_model:{id}")),
        Err(error) => Some(format!("unload_failed:{id}:{error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_formula_scales_and_caps() {
        assert_eq!(build_reserve_gb(16.0), 8.0);
        assert_eq!(build_reserve_gb(64.0), 16.0);
        assert_eq!(build_reserve_gb(256.0), 40.0);
    }

    #[test]
    fn pressure_thresholds_and_oom_are_fail_closed() {
        assert_eq!(classify(20.0, 16.0, 0), Pressure::Healthy);
        assert_eq!(classify(8.0, 16.0, 0), Pressure::Pressure);
        assert_eq!(classify(3.9, 16.0, 0), Pressure::Critical);
        assert_eq!(classify(20.0, 16.0, 1), Pressure::Critical);
    }
}
