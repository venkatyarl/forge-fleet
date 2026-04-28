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

use crate::nats::{get_or_init_nats, publish_pulse_beat};

use crate::beat_v2::{
    Capabilities, DbTopology, HardwareInfo, Ip, LoadInfo, MemoryInfo, NetworkInfo, PulseBeatV2,
};
use crate::docker_probe::DockerProbe;
use crate::llm_probe::LlmProbe;
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
    pub async fn build_beat(&self) -> PulseBeatV2 {
        let mut beat = PulseBeatV2::skeleton(&self.computer_name);
        beat.epoch = self.epoch.load(Ordering::Relaxed);
        beat.role_claimed = self
            .role
            .read()
            .map(|r| r.clone())
            .unwrap_or_else(|_| "member".to_string());
        beat.election_priority = self.election_priority;
        beat.timestamp = Utc::now();

        // V43: drain any queued panics from the local panic_hook into the
        // beat. Leader's materializer deduplicates into fleet_bug_reports.
        let captured = ff_core::panic_hook::drain();
        if !captured.is_empty() {
            beat.encountered_bugs = captured
                .into_iter()
                .map(|b| crate::beat_v2::EncounteredBug {
                    signature: b.signature,
                    file_path: b.file_path,
                    line_number: b.line_number,
                    error_class: b.error_class,
                    stack_excerpt: b.stack_excerpt,
                    binary_version: b.binary_version,
                    tier: b.tier,
                })
                .collect();
        }

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
        let mut all_ips = detect_all_ips();
        // V43: annotate mlx5_core NICs with cx7-fabric kind + paired_with + link_speed.
        for ip in all_ips.iter_mut() {
            crate::cx7_detect::enrich_ip(ip, &self.computer_name);
        }
        beat.network = NetworkInfo {
            primary_ip: detect_primary_ip(),
            all_ips,
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

        // ── LLM servers + available models (probe localhost) ─────────────
        beat.llm_servers = LlmProbe::detect().await;
        beat.available_models = LlmProbe::available_models();

        // ── Docker daemon probe ──────────────────────────────────────────
        beat.docker = DockerProbe::detect().await;

        // ── Ray cluster membership (item 4.6) — populates
        //    multi_host_participation so the leader-side materializer
        //    auto-fills the llm_clusters table without operator action.
        //    Returns None on hosts that aren't ray members (most fleet
        //    machines today), which is the correct shape.
        beat.multi_host_participation = crate::ray_detect::detect_ray_membership().await;

        // ── peers_seen populated by a separate loop (reader_tick) ────────

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

            // Best-effort: initialize the NATS client so publishes reach
            // fleet.pulse.{name}. A failure here just means NATS is offline;
            // pulse still flows via Redis.
            let _ = get_or_init_nats().await;

            info!(
                "heartbeat_v2 publisher started for '{}' (interval: {:?})",
                self.computer_name, self.interval
            );

            // Counter so we can emit a periodic info-level "alive" log
            // (every 60 beats ≈ every 15 min at the 15s default interval).
            // Per-beat logs stay at debug! to avoid flooding the log at INFO.
            let mut beat_count: u64 = 0;
            const INFO_LOG_EVERY_N_BEATS: u64 = 60;

            loop {
                tokio::select! {
                    _ = tokio::time::sleep(self.interval) => {
                        let beat = self.build_beat().await;
                        if let Err(e) = publish_beat(&mut conn, &self.computer_name, &beat).await {
                            error!("heartbeat_v2: publish failed: {e}");
                        } else {
                            beat_count = beat_count.wrapping_add(1);
                            debug!("heartbeat_v2 published for '{}'", self.computer_name);
                            // Emit a heartbeat-of-life line at INFO every N beats so
                            // operators can confirm the publisher is alive without
                            // raising the whole crate to debug.
                            if beat_count == 1 || beat_count % INFO_LOG_EVERY_N_BEATS == 0 {
                                info!(
                                    "heartbeat_v2: published beat #{} to redis+nats for '{}'",
                                    beat_count, self.computer_name
                                );
                            }
                        }
                        // Fire-and-forget NATS mirror. Best-effort — never errors the loop.
                        publish_pulse_beat(&self.computer_name, &beat).await;
                    }
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            info!("heartbeat_v2 for '{}' emitting final LWT beat", self.computer_name);
                            let mut final_beat = self.build_beat().await;
                            final_beat.going_offline = true;
                            if let Err(e) = publish_beat(&mut conn, &self.computer_name, &final_beat).await {
                                warn!("heartbeat_v2: LWT publish failed: {e}");
                            }
                            publish_pulse_beat(&self.computer_name, &final_beat).await;
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

    // ── HMAC sign (if a pulse_beat_hmac_key is cached) ──────────────
    // The cache is refreshed every 5 minutes by a background task in
    // `pulse_hmac::KeyCache::spawn_refresher`. If the cache is empty
    // (no secret set), we publish unsigned — subscribers will accept
    // unsigned beats while no key is configured (rollout compat).
    let signed = match crate::pulse_hmac::KeyCache::global().get().await {
        Some(key) => crate::pulse_hmac::sign_json(&key, &json),
        None => json,
    };

    // SET pulse:computer:{name} <json> EX 45
    let key = format!("pulse:computer:{}", name);
    let _: () = conn.set_ex(&key, &signed, 45).await?;

    // PUBLISH pulse:events <json>
    let _: () = conn.publish("pulse:events", &signed).await?;

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
        "linux" => std::process::Command::new("nvidia-smi")
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
            .filter(|s| !s.is_empty()),
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
    // Use absolute paths because launchd / systemd user services often
    // strip /sbin from PATH, and `ifconfig` lives there on macOS. Without
    // this, Command::new("ifconfig") returns "not found" → empty all_ips.
    // (Hit on ace 2026-04-25 — primary_ip wrongly fell back to 127.0.0.1.)
    let output = if std::env::consts::OS == "macos" {
        std::process::Command::new("/sbin/ifconfig").output()
    } else {
        // /sbin/ip on Debian/Ubuntu, /usr/sbin/ip on RHEL — try both.
        std::process::Command::new("ip")
            .args(["-4", "-o", "addr", "show"])
            .env("PATH", "/usr/sbin:/sbin:/usr/bin:/bin")
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
                        let (medium, link_speed_gbps) = probe_iface_macos(&current_iface);
                        result.push(Ip {
                            iface: current_iface.clone(),
                            ip: ip.to_string(),
                            kind,
                            paired_with: None,
                            link_speed_gbps,
                            medium,
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
                // Skip container / virtual / bridge interfaces that pollute
                // the fleet's network identity. These aren't "the computer's
                // address," they're internal docker/k8s/libvirt plumbing.
                if iface.starts_with("docker")
                    || iface.starts_with("br-")
                    || iface == "br0"
                    || iface.starts_with("virbr")
                    || iface.starts_with("veth")
                    || iface.starts_with("cni")
                    || iface.starts_with("flannel")
                    || iface.starts_with("cali")
                    || iface.starts_with("tap")
                    || iface.starts_with("tun")
                {
                    continue;
                }
                if let Some(addr) = parts[3].split('/').next() {
                    if !addr.starts_with("127.") && !addr.starts_with("169.254.") {
                        let kind = classify_iface(iface, addr);
                        let (medium, link_speed_gbps) = probe_iface_linux(iface);
                        result.push(Ip {
                            iface: iface.to_string(),
                            ip: addr.to_string(),
                            kind,
                            paired_with: None,
                            link_speed_gbps,
                            medium,
                        });
                    }
                }
            }
        }
    }
    result
}

/// Linux: read `/sys/class/net/<iface>/speed` (Mbps for ethernet) and
/// detect wifi via `/sys/class/net/<iface>/wireless` directory presence.
fn probe_iface_linux(iface: &str) -> (Option<String>, Option<u32>) {
    let wireless = std::path::Path::new("/sys/class/net")
        .join(iface)
        .join("wireless")
        .exists();
    if wireless {
        // Wifi link rate via `iw dev <iface> link` if available; otherwise leave None.
        let rate = std::process::Command::new("iw")
            .args(["dev", iface, "link"])
            .env("PATH", "/usr/sbin:/sbin:/usr/bin:/bin")
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    Some(String::from_utf8_lossy(&o.stdout).into_owned())
                } else {
                    None
                }
            })
            .and_then(|s| {
                s.lines()
                    .find(|l| l.trim_start().starts_with("tx bitrate:"))
                    .and_then(|l| l.split_whitespace().nth(2).map(str::to_string))
                    .and_then(|n| n.parse::<f64>().ok())
                    .map(|mbps| (mbps / 1000.0).round() as u32)
            });
        return (Some("wifi".to_string()), rate);
    }
    let speed_mbps: Option<u32> = std::fs::read_to_string(
        std::path::Path::new("/sys/class/net")
            .join(iface)
            .join("speed"),
    )
    .ok()
    .and_then(|s| s.trim().parse::<i32>().ok())
    .filter(|n| *n > 0)
    .map(|n| (n as u32) / 1000);
    let medium = if iface.starts_with("thunderbolt") || iface.starts_with("tbt") {
        "thunderbolt"
    } else if iface.starts_with("rocep") || iface.starts_with("ib") {
        "cx7"
    } else if iface.starts_with("usb") {
        "usb-eth"
    } else if iface.starts_with("eno") || iface.starts_with("enp") || iface.starts_with("eth") {
        "ethernet"
    } else {
        "ethernet"
    };
    // /sys/class/net/thunderbolt0/speed reports something the kernel
    // can't determine for TB-IP. Use the measured ceiling (20 Gbps for TB3,
    // observed empirically with iperf3 on 2018-Mac-mini ↔ Mac-Studio-M3-Ultra)
    // when speed_mbps came back as None for a TB interface.
    let speed_final = if speed_mbps.is_none() && medium == "thunderbolt" {
        Some(20)
    } else {
        speed_mbps
    };
    (Some(medium.to_string()), speed_final)
}

/// macOS: parse `ifconfig <iface>` for the `media:` line which encodes
/// link speed (e.g. `media: autoselect (1000baseT <full-duplex>)`).
/// Cross-references `networksetup -listallhardwareports` to detect wifi.
fn probe_iface_macos(iface: &str) -> (Option<String>, Option<u32>) {
    let media = std::process::Command::new("/sbin/ifconfig")
        .arg(iface)
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).into_owned())
            } else {
                None
            }
        })
        .unwrap_or_default();

    // Hardware-port lookup: AirPort/Wi-Fi → wifi medium.
    let hw_ports = std::process::Command::new("/usr/sbin/networksetup")
        .args(["-listallhardwareports"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).into_owned())
            } else {
                None
            }
        })
        .unwrap_or_default();

    let medium = {
        // Walk hardware ports list looking for "Hardware Port: Foo / Device: <iface>".
        let mut hw_for_iface: Option<&str> = None;
        let mut last_hw: Option<&str> = None;
        for line in hw_ports.lines() {
            if let Some(rest) = line.strip_prefix("Hardware Port: ") {
                last_hw = Some(rest.trim());
            } else if let Some(rest) = line.strip_prefix("Device: ") {
                if rest.trim() == iface {
                    hw_for_iface = last_hw;
                    break;
                }
            }
        }
        let hw_lc = hw_for_iface
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        if hw_lc.contains("wi-fi") || hw_lc.contains("airport") {
            "wifi"
        } else if hw_lc.contains("thunderbolt") && hw_lc.contains("bridge") {
            "thunderbolt"
        } else if hw_lc.contains("usb") {
            "usb-eth"
        } else {
            "ethernet"
        }
    };

    // Speed parse — looks for 1000baseT / 10GbaseT / etc in the active media.
    let speed_gbps: Option<u32> = media
        .lines()
        .find(|l| l.trim_start().starts_with("media:"))
        .and_then(|line| {
            // Match patterns: 100baseT (0.1G), 1000baseT (1G), 2.5GBase (2G/3G),
            // 10GBase-T (10G), etc.
            let lc = line.to_ascii_lowercase();
            if lc.contains("10gbase") {
                Some(10)
            } else if lc.contains("5gbase") {
                Some(5)
            } else if lc.contains("2.5gbase") {
                Some(2)
            }
            // round to 2
            else if lc.contains("1000base") || lc.contains("1gbase") {
                Some(1)
            } else if lc.contains("100base") {
                Some(0)
            }
            // <1 Gbps → round to 0
            else {
                None
            }
        });

    (Some(medium.to_string()), speed_gbps)
}

fn classify_iface(iface: &str, ip: &str) -> String {
    if iface.starts_with("utun")
        || iface.starts_with("tailscale")
        || ip.starts_with("100.64.")
        || ip.starts_with("100.65.")
    {
        "tailscale".to_string()
    } else if iface.starts_with("wg") {
        "wireguard".to_string()
    } else if ip.starts_with("10.42.") || ip.starts_with("10.43.") {
        // CX-7 fabric subnets — sia↔adele on 10.42, rihanna↔beyonce on 10.43.
        // Already kind-classified by cx7_detect::enrich_ip after creation,
        // but we tag here too so even non-cx7-detected nodes get the right
        // kind on first beat.
        "cx7-fabric".to_string()
    } else if ip.starts_with("10.44.") {
        // Thunderbolt fabric subnet — taylor↔james 2026-04-25.
        "tb-fabric".to_string()
    } else if ip.starts_with("10.") || ip.starts_with("192.168.") || ip.starts_with("172.") {
        "lan".to_string()
    } else {
        "public".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn build_beat_roundtrips_through_json() {
        let client = redis::Client::open("redis://localhost:6380").unwrap();
        let pub_ =
            HeartbeatV2Publisher::new(client, "test-computer".into(), Duration::from_secs(15), 100);
        let beat = pub_.build_beat().await;
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
            matches!(
                kind.as_str(),
                "apple_silicon" | "nvidia_cuda" | "amd_rocm" | "none"
            ),
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
