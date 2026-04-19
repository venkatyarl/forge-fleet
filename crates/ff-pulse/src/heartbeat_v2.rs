//! HeartbeatV2Publisher — emits the richer PulseBeatV2 payload.
//!
//! Runs alongside the existing v1 publisher. v2 uses:
//!   - Redis key `pulse:computer:{name}` with 45s TTL (SETEX).
//!   - Redis pub/sub channel `pulse:events` for instant consumers.
//!
//! Ephemeral fields (load, queue depth, tokens/sec) are included in the
//! beat but are NEVER persisted to Postgres by the materializer — only
//! stable config changes (IPs, installed software, deployment topology)
//! result in DB writes. See materializer.rs.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::Utc;
use redis::AsyncCommands;
use sysinfo::{Disks, System};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::beat_v2::{
    Capabilities, DbTopology, DockerStatus, HardwareInfo, Ip, LoadInfo, MemoryInfo,
    NetworkInfo, PulseBeatV2,
};
use crate::software_collector::SoftwareCollector;

/// Publisher for PulseBeatV2 — richer payload used by Pulse v2 subsystems.
pub struct HeartbeatV2Publisher {
    redis: redis::Client,
    computer_name: String,
    interval: Duration,
    /// Shared epoch counter (bumped by leader_tick on claim).
    epoch: Arc<AtomicU64>,
    /// Current role_claimed — shared with leader_tick.
    role: Arc<parking_lot_compat::RwLock<String>>,
    /// Cached election priority from fleet_members (set at startup).
    election_priority: i32,
}

// Small compatibility shim — we use std RwLock to avoid pulling parking_lot.
mod parking_lot_compat {
    pub use std::sync::RwLock;
}

impl HeartbeatV2Publisher {
    pub fn new(
        redis: redis::Client,
        computer_name: String,
        interval: Duration,
        election_priority: i32,
    ) -> Self {
        Self {
            redis,
            computer_name,
            interval,
            epoch: Arc::new(AtomicU64::new(0)),
            role: Arc::new(parking_lot_compat::RwLock::new("member".to_string())),
            election_priority,
        }
    }

    pub fn with_defaults(
        redis: redis::Client,
        computer_name: String,
        election_priority: i32,
    ) -> Self {
        Self::new(
            redis,
            computer_name,
            Duration::from_secs(15),
            election_priority,
        )
    }

    /// Share the epoch atomic with leader_tick so both agree.
    pub fn epoch_handle(&self) -> Arc<AtomicU64> {
        self.epoch.clone()
    }

    /// Share the role RwLock with leader_tick.
    pub fn role_handle(&self) -> Arc<parking_lot_compat::RwLock<String>> {
        self.role.clone()
    }

    /// Build a single beat from local system state.
    pub fn build_beat(&self) -> PulseBeatV2 {
        let mut beat = PulseBeatV2::skeleton(&self.computer_name);
        beat.epoch = self.epoch.load(Ordering::Relaxed);
        beat.role_claimed = self.role.read().map(|r| r.clone()).unwrap_or_else(|_| "member".to_string());
        beat.election_priority = self.election_priority;
        beat.timestamp = Utc::now();

        // ── Hardware + memory snapshot ─────────────────────────────────────
        let mut sys = System::new_all();
        sys.refresh_all();

        let cpu_cores = std::thread::available_parallelism()
            .map(|n| n.get() as i32)
            .unwrap_or(1);
        let ram_total_gb = (sys.total_memory() as f64 / 1_073_741_824.0).round() as i32;
        let ram_used_bytes = sys.used_memory();
        let ram_used_gb = ram_used_bytes as f64 / 1_073_741_824.0;
        let ram_free_gb = (sys.total_memory() - ram_used_bytes) as f64 / 1_073_741_824.0;
        let ram_pct = if sys.total_memory() > 0 {
            (ram_used_bytes as f64 / sys.total_memory() as f64) * 100.0
        } else {
            0.0
        };

        let disks = Disks::new_with_refreshed_list();
        let (disk_total, disk_used) = disks.iter().fold((0u64, 0u64), |(t, u), d| {
            (
                t + d.total_space(),
                u + (d.total_space() - d.available_space()),
            )
        });
        let disk_total_gb = (disk_total as f64 / 1_073_741_824.0) as i32;
        let disk_free_gb = (disk_total - disk_used) as f64 / 1_073_741_824.0;

        beat.hardware = HardwareInfo {
            cpu_cores,
            ram_gb: ram_total_gb,
            disk_gb: disk_total_gb,
            gpu: detect_gpu_model(),
        };

        beat.load = LoadInfo {
            cpu_pct: sys.global_cpu_usage() as f64,
            ram_pct,
            disk_free_gb,
            gpu_pct: 0.0, // Phase 7 fills this in
            active_inference_requests: 0,
            active_agent_sessions: 0,
        };

        beat.memory = MemoryInfo {
            ram_total_gb: ram_total_gb as f64,
            ram_used_gb,
            ram_free_gb,
            llm_ram_allocated_gb: 0.0, // Phase 7
            ram_available_for_new_llm_gb: (ram_free_gb - 3.0).max(0.0), // reserve 3GB for OS
            vram_total_gb: None,
            vram_used_gb: None,
            vram_free_gb: None,
            llm_vram_allocated_gb: None,
        };

        // ── Network identity ─────────────────────────────────────────────
        beat.network = NetworkInfo {
            primary_ip: detect_primary_ip(),
            all_ips: detect_all_ips(),
        };

        // ── Capabilities ─────────────────────────────────────────────────
        let gpu_kind = detect_gpu_kind();
        let gpu_count = if gpu_kind == "none" { 0 } else { 1 };
        let can_run_metal = gpu_kind == "apple_silicon";
        let can_run_cuda = gpu_kind == "nvidia_cuda";
        let can_run_rocm = gpu_kind == "amd_rocm";

        let recommended_runtimes: Vec<String> = match gpu_kind.as_str() {
            "apple_silicon" => vec!["mlx_lm".into(), "llama.cpp".into()],
            "nvidia_cuda" => vec!["vllm".into(), "llama.cpp".into(), "ollama".into()],
            "amd_rocm" => vec!["llama.cpp".into(), "ollama".into()],
            _ => vec!["llama.cpp".into()],
        };

        beat.capabilities = Capabilities {
            can_serve_ff_gateway: true,
            can_serve_openclaw_gateway: true,
            can_host_postgres_replica: disk_free_gb > 100.0,
            can_host_redis_replica: ram_total_gb >= 16,
            gpu_kind: gpu_kind.clone(),
            gpu_count,
            gpu_vram_gb: None,
            gpu_total_vram_gb: None,
            can_run_cuda,
            can_run_metal,
            can_run_rocm,
            recommended_runtimes,
            max_runnable_model_gb: None,
        };

        // ── Installed software inventory ─────────────────────────────────
        beat.installed_software = SoftwareCollector::new().detect();

        // ── Placeholders awaiting later phases ───────────────────────────
        // llm_servers, available_models — empty vecs
        // docker — default empty status
        // peers_seen — populated by a separate loop (reader_tick)
        beat.docker = DockerStatus {
            daemon_running: false, // Phase 10 probes this
            total_cpu_pct: 0.0,
            total_memory_mb: 0.0,
            memory_limit_mb: 0.0,
            projects: Vec::new(),
        };

        beat.db_topology = DbTopology {
            postgres_primary: None,
            postgres_replicas: Vec::new(),
            redis_primary: None,
            redis_replicas: Vec::new(),
        };

        beat
    }

    /// Spawn the publisher loop. Emits a final beat with `going_offline=true`
    /// on shutdown.
    pub fn spawn(self, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut conn = match self.redis.get_multiplexed_async_connection().await {
                Ok(c) => c,
                Err(e) => {
                    error!("heartbeat_v2: failed to connect Redis: {e}");
                    return;
                }
            };

            info!(
                "heartbeat_v2 publisher started for '{}' (interval: {:?})",
                self.computer_name, self.interval
            );

            loop {
                tokio::select! {
                    _ = tokio::time::sleep(self.interval) => {
                        let beat = self.build_beat();
                        if let Err(e) = publish_beat(&mut conn, &self.computer_name, &beat).await {
                            error!("heartbeat_v2: publish failed: {e}");
                        } else {
                            debug!("heartbeat_v2 published for '{}'", self.computer_name);
                        }
                    }
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            info!("heartbeat_v2 for '{}' emitting final LWT beat", self.computer_name);
                            let mut final_beat = self.build_beat();
                            final_beat.going_offline = true;
                            if let Err(e) = publish_beat(&mut conn, &self.computer_name, &final_beat).await {
                                warn!("heartbeat_v2: LWT publish failed: {e}");
                            }
                            break;
                        }
                    }
                }
            }
        })
    }
}

async fn publish_beat(
    conn: &mut redis::aio::MultiplexedConnection,
    name: &str,
    beat: &PulseBeatV2,
) -> Result<(), redis::RedisError> {
    let json = serde_json::to_string(beat).map_err(|e| {
        redis::RedisError::from((redis::ErrorKind::TypeError, "serialize beat", e.to_string()))
    })?;

    // SET pulse:computer:{name} <json> EX 45
    let key = format!("pulse:computer:{}", name);
    let _: () = conn.set_ex(&key, &json, 45).await?;

    // PUBLISH pulse:events <json>
    let _: () = conn.publish("pulse:events", &json).await?;

    Ok(())
}

// ─── GPU / network / OS detection helpers ───────────────────────────────

fn detect_gpu_kind() -> String {
    // macOS: aarch64 = Apple Silicon (Metal/MLX), x86_64 = Intel (no useful GPU).
    if std::env::consts::OS == "macos" {
        return if std::env::consts::ARCH == "aarch64" {
            "apple_silicon".to_string()
        } else {
            "none".to_string()
        };
    }

    // Linux: probe for nvidia-smi, then rocm-smi.
    if std::env::consts::OS == "linux" {
        if std::process::Command::new("nvidia-smi")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return "nvidia_cuda".to_string();
        }
        if std::process::Command::new("rocm-smi")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return "amd_rocm".to_string();
        }
    }

    "none".to_string()
}

fn detect_gpu_model() -> Option<String> {
    match std::env::consts::OS {
        "macos" if std::env::consts::ARCH == "aarch64" => {
            // Could parse `system_profiler SPDisplaysDataType` but that's slow.
            // Return a generic label; precise model filled in by a later phase.
            Some("Apple Silicon GPU (Metal)".to_string())
        }
        "linux" => {
            std::process::Command::new("nvidia-smi")
                .args(["--query-gpu=name", "--format=csv,noheader"])
                .output()
                .ok()
                .and_then(|o| {
                    if o.status.success() {
                        Some(
                            String::from_utf8_lossy(&o.stdout)
                                .lines()
                                .next()
                                .unwrap_or("")
                                .trim()
                                .to_string(),
                        )
                    } else {
                        None
                    }
                })
                .filter(|s| !s.is_empty())
        }
        _ => None,
    }
}

fn detect_primary_ip() -> String {
    // Try env override first.
    if let Ok(ip) = std::env::var("FORGEFLEET_PRIMARY_IP") {
        if !ip.is_empty() {
            return ip;
        }
    }
    // Fall back to the first non-loopback IPv4 from `ifconfig`/`ip addr`.
    detect_all_ips()
        .into_iter()
        .find(|ip| ip.kind == "lan")
        .map(|ip| ip.ip)
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

fn detect_all_ips() -> Vec<Ip> {
    let output = if std::env::consts::OS == "macos" {
        std::process::Command::new("ifconfig").output()
    } else {
        std::process::Command::new("ip")
            .args(["-4", "-o", "addr", "show"])
            .output()
    };

    let stdout = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => return Vec::new(),
    };

    let mut result = Vec::new();
    if std::env::consts::OS == "macos" {
        // Parse `ifconfig` macOS output.
        let mut current_iface = String::new();
        for line in stdout.lines() {
            if !line.starts_with('\t') && !line.starts_with(' ') && line.contains(':') {
                current_iface = line.split(':').next().unwrap_or("").to_string();
            } else if line.trim_start().starts_with("inet ") {
                if let Some(ip) = line.split_whitespace().nth(1) {
                    if !ip.starts_with("127.") && !ip.starts_with("169.254.") {
                        let kind = classify_iface(&current_iface, ip);
                        result.push(Ip {
                            iface: current_iface.clone(),
                            ip: ip.to_string(),
                            kind,
                        });
                    }
                }
            }
        }
    } else {
        // Linux `ip -4 -o addr show` output:
        //   2: eth0    inet 192.168.5.100/24 brd ...
        for line in stdout.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 4 {
                let iface = parts[1];
                if let Some(addr) = parts[3].split('/').next() {
                    if !addr.starts_with("127.") && !addr.starts_with("169.254.") {
                        let kind = classify_iface(iface, addr);
                        result.push(Ip {
                            iface: iface.to_string(),
                            ip: addr.to_string(),
                            kind,
                        });
                    }
                }
            }
        }
    }
    result
}

fn classify_iface(iface: &str, ip: &str) -> String {
    if iface.starts_with("utun") || iface.starts_with("tailscale") || ip.starts_with("100.64.") || ip.starts_with("100.65.") {
        "tailscale".to_string()
    } else if iface.starts_with("wg") {
        "wireguard".to_string()
    } else if ip.starts_with("10.") || ip.starts_with("192.168.") || ip.starts_with("172.") {
        "lan".to_string()
    } else {
        "public".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_beat_roundtrips_through_json() {
        let client = redis::Client::open("redis://localhost:6380").unwrap();
        let pub_ = HeartbeatV2Publisher::new(client, "test-computer".into(), Duration::from_secs(15), 100);
        let beat = pub_.build_beat();
        assert_eq!(beat.pulse_protocol_version, 2);
        assert_eq!(beat.computer_name, "test-computer");
        assert!(beat.hardware.cpu_cores > 0);
        let json = serde_json::to_string(&beat).expect("serialize");
        let decoded: PulseBeatV2 = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.computer_name, "test-computer");
        assert_eq!(decoded.hardware.cpu_cores, beat.hardware.cpu_cores);
    }

    #[test]
    fn gpu_detection_returns_known_value() {
        let kind = detect_gpu_kind();
        assert!(
            matches!(kind.as_str(), "apple_silicon" | "nvidia_cuda" | "amd_rocm" | "none"),
            "unexpected gpu_kind: {}",
            kind
        );
    }

    #[test]
    fn classify_iface_handles_common_cases() {
        assert_eq!(classify_iface("en0", "192.168.5.100"), "lan");
        assert_eq!(classify_iface("utun3", "100.64.5.100"), "tailscale");
        assert_eq!(classify_iface("eth0", "10.0.0.5"), "lan");
    }
}
