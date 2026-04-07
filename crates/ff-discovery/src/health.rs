use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::future::Future;
use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tokio::time::{Duration, Instant, sleep, timeout};

// ─── Health Snapshot (used by ff-agent) ───────────────────────────────────────

/// Point-in-time resource snapshot for heartbeat reporting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthSnapshot {
    pub timestamp: DateTime<Utc>,
    pub cpu_usage_percent: Option<f32>,
    pub memory_used_mb: Option<u64>,
    pub memory_total_mb: Option<u64>,
    pub gpu_usage_percent: Option<f32>,
    pub active_tasks: usize,
    pub running_models: Vec<String>,
    pub temperature_c: Option<f32>,
    pub load_avg_1m: Option<f32>,
}

/// Collect a health snapshot with current system metrics.
pub fn collect_health_snapshot(active_tasks: usize, running_models: Vec<String>) -> HealthSnapshot {
    let profile = crate::profile::detect_hardware_profile();
    let memory_total_mb = profile.memory.total_mb;
    let load_avg_1m = detect_load_avg_1m();
    let cores = std::thread::available_parallelism()
        .map(|c| c.get())
        .unwrap_or(1) as f32;

    let cpu_usage_percent = load_avg_1m.map(|load| (load / cores * 100.0).clamp(0.0, 100.0));

    let memory_used_mb = if cfg!(target_os = "linux") {
        memory_used_linux_mb()
    } else if cfg!(target_os = "macos") {
        memory_used_macos_mb(memory_total_mb)
    } else {
        None
    };

    HealthSnapshot {
        timestamp: Utc::now(),
        cpu_usage_percent,
        memory_used_mb,
        memory_total_mb: Some(memory_total_mb),
        gpu_usage_percent: None,
        active_tasks,
        running_models,
        temperature_c: None,
        load_avg_1m,
    }
}

fn detect_load_avg_1m() -> Option<f32> {
    if cfg!(target_os = "linux") {
        std::fs::read_to_string("/proc/loadavg").ok().and_then(|s| {
            s.split_whitespace()
                .next()
                .and_then(|v| v.parse::<f32>().ok())
        })
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("sysctl")
            .args(["-n", "vm.loadavg"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| {
                let s = String::from_utf8_lossy(&o.stdout).to_string();
                s.replace(['{', '}'], "")
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse::<f32>().ok())
            })
    } else {
        None
    }
}

fn memory_used_linux_mb() -> Option<u64> {
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut total_kb = 0u64;
    let mut available_kb = 0u64;
    for line in content.lines() {
        if let Some(val) = line.strip_prefix("MemTotal:") {
            total_kb = val.split_whitespace().next()?.parse().ok()?;
        } else if let Some(val) = line.strip_prefix("MemAvailable:") {
            available_kb = val.split_whitespace().next()?.parse().ok()?;
        }
    }
    if total_kb > 0 {
        Some((total_kb.saturating_sub(available_kb)) / 1024)
    } else {
        None
    }
}

fn memory_used_macos_mb(total_mb: u64) -> Option<u64> {
    let output = std::process::Command::new("vm_stat")
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let text = String::from_utf8_lossy(&output.stdout);
    let page_size: u64 = 16384; // Apple Silicon default
    let mut active = 0u64;
    let mut wired = 0u64;
    let mut compressed = 0u64;
    for line in text.lines() {
        let parse_pages = |l: &str| -> u64 {
            l.split(':')
                .nth(1)
                .and_then(|v| v.trim().trim_end_matches('.').parse().ok())
                .unwrap_or(0)
        };
        if line.starts_with("Pages active") {
            active = parse_pages(line);
        } else if line.starts_with("Pages wired") {
            wired = parse_pages(line);
        } else if line.starts_with("Pages occupied by compressor") {
            compressed = parse_pages(line);
        }
    }
    let used_mb = (active + wired + compressed) * page_size / (1024 * 1024);
    Some(used_mb.min(total_mb))
}

// ─── Health Check Types ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Healthy,
    Degraded,
    Unreachable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthTarget {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub check_http_health: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckResult {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub checked_at: DateTime<Utc>,
    pub latency_ms: u128,
    pub tcp_ok: bool,
    pub http_ok: Option<bool>,
    pub http_status: Option<u16>,
    pub status: HealthStatus,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HealthMonitor {
    http_client: Client,
    pub interval: Duration,
    pub tcp_timeout: Duration,
    pub http_timeout: Duration,
}

impl Default for HealthMonitor {
    fn default() -> Self {
        Self::new(
            Duration::from_secs(10),
            Duration::from_millis(600),
            Duration::from_millis(1200),
        )
    }
}

impl HealthMonitor {
    pub fn new(interval: Duration, tcp_timeout: Duration, http_timeout: Duration) -> Self {
        let http_client = Client::builder()
            .timeout(http_timeout)
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            http_client,
            interval,
            tcp_timeout,
            http_timeout,
        }
    }

    pub async fn check_target(&self, target: &HealthTarget) -> HealthCheckResult {
        let started = Instant::now();
        let checked_at = Utc::now();

        let tcp_ok = timeout(
            self.tcp_timeout,
            TcpStream::connect((target.host.as_str(), target.port)),
        )
        .await
        .is_ok_and(|res| res.is_ok());

        let mut http_ok = None;
        let mut http_status = None;
        let mut error = None;

        if target.check_http_health && tcp_ok {
            let url = format!(
                "http://{}:{}{}",
                target.host,
                target.port,
                crate::ports::HEALTH_PATH
            );

            match self
                .http_client
                .get(url)
                .timeout(self.http_timeout)
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status();
                    http_status = Some(status.as_u16());
                    http_ok = Some(status.is_success());
                }
                Err(err) => {
                    error = Some(err.to_string());
                    http_ok = Some(false);
                }
            }
        }

        let status = if tcp_ok && (!target.check_http_health || http_ok == Some(true)) {
            HealthStatus::Healthy
        } else if tcp_ok {
            HealthStatus::Degraded
        } else {
            HealthStatus::Unreachable
        };

        HealthCheckResult {
            name: target.name.clone(),
            host: target.host.clone(),
            port: target.port,
            checked_at,
            latency_ms: started.elapsed().as_millis(),
            tcp_ok,
            http_ok,
            http_status,
            status,
            error,
        }
    }

    pub async fn check_all(&self, targets: &[HealthTarget]) -> Vec<HealthCheckResult> {
        let mut tasks = JoinSet::new();

        for target in targets {
            let target = target.clone();
            let monitor = self.clone();
            tasks.spawn(async move { monitor.check_target(&target).await });
        }

        let mut results = Vec::with_capacity(targets.len());
        while let Some(result) = tasks.join_next().await {
            if let Ok(health) = result {
                results.push(health);
            }
        }

        results.sort_by(|a, b| a.name.cmp(&b.name));
        results
    }

    /// Run periodic health checks forever, invoking `on_round` after each full sweep.
    pub async fn run_periodic<F, Fut>(&self, targets: Vec<HealthTarget>, mut on_round: F)
    where
        F: FnMut(Vec<HealthCheckResult>) -> Fut,
        Fut: Future<Output = ()>,
    {
        loop {
            let results = self.check_all(&targets).await;
            on_round(results).await;
            sleep(self.interval).await;
        }
    }
}
