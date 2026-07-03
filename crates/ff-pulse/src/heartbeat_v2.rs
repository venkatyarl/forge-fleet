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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// Voluntary step-down flag (HA Phase 1) — shared with leader_tick, which
    /// sets it from the `leader_yield_request` fleet_secret. When true, this
    /// node publishes `is_yielding=true` so every peer's election skips it and
    /// the next-preferred follower takes over (a clean, operator-driven handoff
    /// without waiting 45s for a stale-heartbeat takeover).
    is_yielding: Arc<AtomicBool>,
    /// Cached election priority from fleet_workers (set at startup).
    election_priority: i32,
    /// 10-char git SHA of the binary, captured at compile time and
    /// published on every beat so the materializer can refresh
    /// computer_software.installed_version without an explicit upgrade.
    build_sha: Option<String>,
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
            is_yielding: Arc::new(AtomicBool::new(false)),
            election_priority,
            build_sha: None,
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

    /// Attach the build-SHA that every published beat should carry.
    /// Pass `env!("FF_GIT_SHA")` from the daemon entrypoint.
    pub fn with_build_sha(mut self, sha: impl Into<String>) -> Self {
        let sha = sha.into();
        if !sha.is_empty() && sha != "unknown" {
            self.build_sha = Some(sha);
        }
        self
    }

    /// Share the epoch atomic with leader_tick so both agree.
    pub fn epoch_handle(&self) -> Arc<AtomicU64> {
        self.epoch.clone()
    }

    /// Share the role RwLock with leader_tick.
    pub fn role_handle(&self) -> Arc<parking_lot_compat::RwLock<String>> {
        self.role.clone()
    }

    /// Share the voluntary step-down flag with leader_tick, which drives it
    /// from the `leader_yield_request` fleet_secret (HA Phase 1).
    pub fn yielding_handle(&self) -> Arc<AtomicBool> {
        self.is_yielding.clone()
    }

    /// Build a single beat from local system state.
    ///
    /// **Blocking safety**: all synchronous blocking work (sysinfo, subprocess
    /// probes, disk enumeration) is moved into `tokio::task::spawn_blocking`.
    /// This prevents the async runtime from stalling when `networksetup` or
    /// `ifconfig` hang — confirmed on ace 2026-05-04 where beats never
    /// reached Redis because `build_beat()` blocked the publisher loop.
    pub async fn build_beat(&self) -> PulseBeatV2 {
        let computer_name = self.computer_name.clone();
        let epoch = self.epoch.clone();
        let role = self.role.clone();
        let is_yielding = self.is_yielding.clone();
        let election_priority = self.election_priority;
        let build_sha = self.build_sha.clone();

        // Phase A: blocking system probes — must not block the async runtime.
        // Hard timeout: if any macOS framework call (IOKit, DiskArbitration,
        // SystemConfiguration via networksetup) hangs in a launchd context,
        // we degrade gracefully rather than stall the heartbeat forever.
        const BUILD_BEAT_TIMEOUT: Duration = Duration::from_secs(30);
        let blocking_result = tokio::time::timeout(
            BUILD_BEAT_TIMEOUT,
            tokio::task::spawn_blocking(move || {
                let t0 = std::time::Instant::now();
                let mut beat = PulseBeatV2::skeleton(&computer_name);
                beat.epoch = epoch.load(Ordering::Relaxed);
                beat.role_claimed = role
                    .read()
                    .map(|r| r.clone())
                    .unwrap_or_else(|_| "member".to_string());
                beat.election_priority = election_priority;
                beat.is_yielding = is_yielding.load(Ordering::Relaxed);
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

                // ── Hardware + memory snapshot ─────────────────────────────────
                // Inner timeouts for macOS framework calls that can hang in a
                // launchd context (IOKit, DiskArbitration, etc.).
                let sys = run_with_timeout(
                    || {
                        // Skip process enumeration — it can hang on macOS when
                        // run from a launchd agent (no user session / WindowServer).
                        let mut s = System::new_with_specifics(
                            sysinfo::RefreshKind::everything().without_processes(),
                        );
                        s.refresh_all();
                        s
                    },
                    5,
                )
                .unwrap_or_else(|| {
                    tracing::debug!("System::new_all() timed out; using fallback");
                    System::new()
                });

                let cpu_cores = std::thread::available_parallelism()
                    .map(|n| n.get() as i32)
                    .unwrap_or(1);
                let ram_total_gb = (sys.total_memory() as f64 / 1_073_741_824.0).round() as i32;
                let ram_used_bytes = sys.used_memory();
                let ram_used_gb = ram_used_bytes as f64 / 1_073_741_824.0;
                // saturating: used should be <= total, but never underflow-panic
                // the per-heartbeat hot path if a platform reports otherwise.
                let ram_free_gb =
                    sys.total_memory().saturating_sub(ram_used_bytes) as f64 / 1_073_741_824.0;
                let ram_pct = if sys.total_memory() > 0 {
                    (ram_used_bytes as f64 / sys.total_memory() as f64) * 100.0
                } else {
                    0.0
                };

                let disks =
                    run_with_timeout(Disks::new_with_refreshed_list, 5).unwrap_or_else(|| {
                        tracing::debug!(
                            "Disks::new_with_refreshed_list() timed out; using empty list"
                        );
                        Disks::new()
                    });
                let (disk_total, disk_used) = aggregate_disk_bytes(
                    disks.iter().map(|d| (d.total_space(), d.available_space())),
                );
                let disk_total_gb = (disk_total as f64 / 1_073_741_824.0) as i32;
                let disk_free_gb = disk_total.saturating_sub(disk_used) as f64 / 1_073_741_824.0;

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

                // ── Network identity ─────────────────────────────────────────
                let mut all_ips = detect_all_ips();
                // V43: annotate mlx5_core NICs with cx7-fabric kind + paired_with + link_speed.
                for ip in all_ips.iter_mut() {
                    crate::cx7_detect::enrich_ip(ip, &computer_name);
                }
                beat.network = NetworkInfo {
                    primary_ip: detect_primary_ip(),
                    all_ips,
                };

                // ── OS classification (V87+) ─────────────────────────────────
                // Pre-classified here so the materializer can write
                // computers.os_family directly without re-deriving from
                // kernel + /etc/os-release on the leader. Computed before
                // capabilities because the GPU-VRAM probe consults os_family
                // to take the GB10/DGX unified-memory fallback path.
                beat.os = detect_os_info();
                beat.build_sha = build_sha.clone();
                beat.source_tree_path = detect_source_tree_path();

                // ── Capabilities ─────────────────────────────────────────────
                let gpu_kind = detect_gpu_kind();
                let gpu_count = if gpu_kind == "none" { 0 } else { 1 };
                let (gpu_vram_gb, gpu_total_vram_gb) =
                    detect_gpu_vram_gb(&gpu_kind, &beat.os.family);
                let can_run_metal = gpu_kind == "apple_silicon";
                let can_run_cuda = gpu_kind == "nvidia_cuda";
                let can_run_rocm = gpu_kind == "amd_rocm";

                let recommended_runtimes: Vec<String> = match gpu_kind.as_str() {
                    "apple_silicon" => {
                        vec!["mlx_lm".into(), "llama.cpp".into()]
                    }
                    "nvidia_cuda" => {
                        vec!["vllm".into(), "llama.cpp".into(), "ollama".into()]
                    }
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
                    gpu_vram_gb,
                    gpu_total_vram_gb,
                    can_run_cuda,
                    can_run_metal,
                    can_run_rocm,
                    recommended_runtimes,
                    max_runnable_model_gb: None,
                };

                // ── Installed software inventory ─────────────────────────────
                beat.installed_software = SoftwareCollector::new().detect();

                // ── Available models (sync probe of ~/models) ────────────────
                beat.available_models = LlmProbe::available_models();

                debug!("build_beat blocking phase completed in {:?}", t0.elapsed());
                beat
            }),
        )
        .await;

        let (mut beat, blocking_ok) = match blocking_result {
            Ok(Ok(beat)) => (beat, true),
            Ok(Err(e)) => {
                error!("build_beat spawn_blocking panicked: {e}");
                let mut skeleton = PulseBeatV2::skeleton(&self.computer_name);
                skeleton.epoch = self.epoch.load(Ordering::Relaxed);
                skeleton.role_claimed = self
                    .role
                    .read()
                    .map(|r| r.clone())
                    .unwrap_or_else(|_| "member".to_string());
                skeleton.election_priority = self.election_priority;
                skeleton.is_yielding = self.is_yielding.load(Ordering::Relaxed);
                skeleton.timestamp = Utc::now();
                (skeleton, false)
            }
            Err(_) => {
                warn!(
                    "build_beat: blocking probe timed out after {:?}; \
                     returning skeleton beat (macOS framework hang in launchd context)",
                    BUILD_BEAT_TIMEOUT
                );
                let mut skeleton = PulseBeatV2::skeleton(&self.computer_name);
                skeleton.epoch = self.epoch.load(Ordering::Relaxed);
                skeleton.role_claimed = self
                    .role
                    .read()
                    .map(|r| r.clone())
                    .unwrap_or_else(|_| "member".to_string());
                skeleton.election_priority = self.election_priority;
                skeleton.is_yielding = self.is_yielding.load(Ordering::Relaxed);
                skeleton.timestamp = Utc::now();
                (skeleton, false)
            }
        };

        // Phase B: async probes — safe to .await because blocking work above
        // is now on a separate thread.
        // If the blocking phase timed out, skip the async probes to avoid
        // cascading hangs (e.g. DockerProbe::detect() also uses spawn_blocking
        // and may stall if the blocking pool is saturated by the hung thread).
        if blocking_ok {
            beat.llm_servers = LlmProbe::new().detect().await;
            beat.docker = DockerProbe::detect().await;
            beat.multi_host_participation = crate::ray_detect::detect_ray_membership().await;
        } else {
            beat.llm_servers = Vec::new();
            beat.docker = crate::beat_v2::DockerStatus {
                daemon_running: false,
                total_cpu_pct: 0.0,
                total_memory_mb: 0.0,
                memory_limit_mb: 0.0,
                projects: Vec::new(),
            };
            beat.multi_host_participation = None;
        }

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
    ///
    /// Reconnect-on-error: the multiplexed Redis connection is wrapped in
    /// `Option`. On any publish error we drop the connection and the next
    /// tick rebuilds it. Without this, a single network event (sleep/wake,
    /// NIC change, NAT timeout) leaves the daemon stuck publishing on a
    /// broken pipe forever — confirmed 2026-04-28 on aura, where 7+ hours
    /// of silent broken-pipe errors hid the heartbeat from the leader.
    pub fn spawn(self, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            // Initial connect — failure here is fatal because we never had
            // a working connection. Operator must investigate Redis before
            // the daemon starts at all.
            let mut conn: Option<redis::aio::MultiplexedConnection> =
                match self.redis.get_multiplexed_async_connection().await {
                    Ok(c) => Some(c),
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

            // Reconnect bookkeeping. Suppresses the warn-spam loop when
            // Redis is genuinely unreachable for an extended window.
            let mut consecutive_reconnect_failures: u32 = 0;
            const WARN_RECONNECT_EVERY_N: u32 = 10;

            loop {
                tokio::select! {
                    _ = tokio::time::sleep(self.interval) => {
                        // Rebuild the connection if a previous publish dropped it.
                        if conn.is_none() {
                            match self.redis.get_multiplexed_async_connection().await {
                                Ok(c) => {
                                    if consecutive_reconnect_failures > 0 {
                                        info!(
                                            after_failures = consecutive_reconnect_failures,
                                            "heartbeat_v2: redis connection re-established for '{}'",
                                            self.computer_name
                                        );
                                    }
                                    conn = Some(c);
                                    consecutive_reconnect_failures = 0;
                                }
                                Err(e) => {
                                    consecutive_reconnect_failures =
                                        consecutive_reconnect_failures.saturating_add(1);
                                    if consecutive_reconnect_failures == 1
                                        || consecutive_reconnect_failures
                                            .is_multiple_of(WARN_RECONNECT_EVERY_N)
                                    {
                                        warn!(
                                            attempt = consecutive_reconnect_failures,
                                            error = %e,
                                            "heartbeat_v2: redis reconnect failed; will retry next tick"
                                        );
                                    }
                                    // Skip this tick. Try again next interval.
                                    // NATS mirror also skipped — without redis the
                                    // beat is incomplete anyway.
                                    continue;
                                }
                            }
                        }

                        let beat = self.build_beat().await;
                        // Borrow the connection for the publish; on error,
                        // drop it so the next iteration rebuilds.
                        let publish_result = if let Some(c) = conn.as_mut() {
                            publish_beat(c, &self.computer_name, &beat).await
                        } else {
                            // Should never hit this branch given the reconnect
                            // block above, but defensive — treat as transient.
                            continue;
                        };
                        match publish_result {
                            Ok(()) => {
                                beat_count = beat_count.wrapping_add(1);
                                debug!("heartbeat_v2 published for '{}'", self.computer_name);
                                if beat_count == 1
                                    || beat_count.is_multiple_of(INFO_LOG_EVERY_N_BEATS)
                                {
                                    info!(
                                        "heartbeat_v2: published beat #{} to redis+nats for '{}'",
                                        beat_count, self.computer_name
                                    );
                                }
                            }
                            Err(e) => {
                                error!(
                                    error = %e,
                                    "heartbeat_v2: publish failed; dropping connection for rebuild on next tick"
                                );
                                conn = None;
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
                            if let Some(c) = conn.as_mut() {
                                if let Err(e) = publish_beat(c, &self.computer_name, &final_beat).await {
                                    warn!("heartbeat_v2: LWT publish failed: {e}");
                                }
                            } else {
                                warn!("heartbeat_v2: LWT skipped (no live redis connection)");
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

/// Sum `(total, used)` bytes across disks, guarding against pseudo-filesystems
/// that report `available_space > total_space` (overlay/tmpfs/FUSE mounts, or
/// mounts reporting `total = 0` with `available > 0`). A plain
/// `total - available` underflows there — panicking in debug, and in release
/// (how `forgefleetd` ships) silently wrapping to a garbage huge `u64` that
/// corrupts the disk metrics reported in the beat.
pub(crate) fn aggregate_disk_bytes(disks: impl IntoIterator<Item = (u64, u64)>) -> (u64, u64) {
    disks
        .into_iter()
        .fold((0u64, 0u64), |(total_acc, used_acc), (total, available)| {
            (
                total_acc.saturating_add(total),
                used_acc.saturating_add(total.saturating_sub(available)),
            )
        })
}

// ─── GPU / network / OS detection helpers ───────────────────────────────

/// Detect the operating system family + distribution + kernel for this host.
///
/// Returns an [`OsInfo`] populated with one of:
///   - `family = "macos"` on Darwin (with `version` from `sw_vers`)
///   - `family = "linux-dgx"` on Linux when `uname -r` ends in `-nvidia`
///     (NVIDIA DGX OS / Spark — see memory: dgx-spark-specs)
///   - `family = "linux-ubuntu"` on Linux when /etc/os-release ID=ubuntu
///   - `family = "linux-debian"` on Linux when ID=debian
///   - `family = "linux"` otherwise on Linux
///   - `family = "windows"` on Windows
///   - `family = "unknown"` everywhere else
///
/// The materializer writes this to `computers.os_family` so the
/// auto-upgrade playbook resolver can pick the right key
/// (linux-ubuntu vs linux-dgx) without manual classification.
/// Detect this node's ForgeFleet source tree — the absolute path of the repo
/// root the daemon builds/self-upgrades from. Returned in the beat so the
/// materializer can heal `computers.source_tree_path` from ground truth
/// (the leader's auto-upgrade `cd`s into this column; NULL → self-upgrade
/// silently skips). Probes the same locations as
/// `ff_core::db_health::locate_compose_file`, picking the first that exists,
/// and returns the canonicalized absolute path. `None` when no tree is found
/// (e.g. binary-only install) — the materializer then leaves the column alone.
fn detect_source_tree_path() -> Option<String> {
    use std::path::PathBuf;
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    let candidates: Vec<PathBuf> = [
        std::env::var("FORGEFLEET_REPO").ok().map(PathBuf::from),
        home.as_ref().map(|h| h.join("projects/forge-fleet")),
        home.as_ref()
            .map(|h| h.join(".forgefleet/sub-agents/sub-agent-0/forge-fleet")),
    ]
    .into_iter()
    .flatten()
    .collect();
    // A real tree has the deploy compose file (and a .git dir); require the
    // compose file so we don't latch onto an empty placeholder directory.
    candidates
        .into_iter()
        .find(|root| root.join("deploy/docker-compose.yml").exists())
        .map(|root| {
            std::fs::canonicalize(&root)
                .unwrap_or(root)
                .to_string_lossy()
                .into_owned()
        })
}

fn detect_os_info() -> crate::beat_v2::OsInfo {
    use crate::beat_v2::OsInfo;
    if cfg!(target_os = "macos") {
        let version = std::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let kernel = std::process::Command::new("uname")
            .arg("-r")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        return OsInfo {
            family: "macos".to_string(),
            distribution: "macOS".to_string(),
            version,
            kernel,
        };
    }
    if cfg!(target_os = "windows") {
        return OsInfo {
            family: "windows".to_string(),
            distribution: "Windows".to_string(),
            version: String::new(),
            kernel: String::new(),
        };
    }
    if cfg!(target_os = "linux") {
        let kernel = std::process::Command::new("uname")
            .arg("-r")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let osr = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        let distribution = osr
            .lines()
            .find_map(|l| l.strip_prefix("ID="))
            .map(|v| v.trim_matches('"').to_string())
            .unwrap_or_else(|| "linux".to_string());
        let version = osr
            .lines()
            .find_map(|l| l.strip_prefix("VERSION_ID="))
            .map(|v| v.trim_matches('"').to_string())
            .unwrap_or_default();
        // DGX OS layers an `-nvidia` kernel onto Ubuntu — detect via uname
        // since /etc/os-release still reads `ID=ubuntu`.
        let family = if kernel.ends_with("-nvidia") {
            "linux-dgx".to_string()
        } else {
            match distribution.as_str() {
                "ubuntu" => "linux-ubuntu".to_string(),
                "debian" => "linux-debian".to_string(),
                _ => "linux".to_string(),
            }
        };
        return OsInfo {
            family,
            distribution,
            version,
            kernel,
        };
    }
    OsInfo {
        family: "unknown".to_string(),
        ..Default::default()
    }
}

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
        if command_output_with_timeout(
            std::process::Command::new("nvidia-smi")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null()),
            3,
        )
        .map(|o| o.status.success())
        .unwrap_or(false)
        {
            return "nvidia_cuda".to_string();
        }
        if command_output_with_timeout(
            std::process::Command::new("rocm-smi")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null()),
            3,
        )
        .map(|o| o.status.success())
        .unwrap_or(false)
        {
            return "amd_rocm".to_string();
        }
    }

    "none".to_string()
}

/// Probe the GPU VRAM (in GB) for this host, dispatched by `gpu_kind`.
///
/// Returns `(gpu_vram_gb, gpu_total_vram_gb)`:
///   - `gpu_vram_gb` is the per-device VRAM when a discrete value exists
///     (single-GPU hosts: same as total; multi-GPU: left `None` here since
///     we only report one device today and don't want to misattribute).
///   - `gpu_total_vram_gb` is the addressable pool across all GPUs.
///
/// Runs every beat, so each probe is cheap (single short-lived subprocess
/// or a `sysctl`/`/proc` read) with a hard timeout. If a probe fails or
/// returns `N/A` with no fallback, both values stay `None` — we never
/// fabricate a number.
///
/// `os_family` is the pre-classified family from [`detect_os_info`] and is
/// only consulted for the GB10 / DGX-Spark special path (see below).
fn detect_gpu_vram_gb(gpu_kind: &str, os_family: &str) -> (Option<f64>, Option<f64>) {
    match gpu_kind {
        // ── NVIDIA CUDA ──────────────────────────────────────────────────
        // Query memory.total directly. On Blackwell GB10 (DGX Spark) the GPU
        // shares the system's unified memory and nvidia-smi reports "N/A" for
        // memory.total — see memory: dgx-spark-specs. In that case fall back
        // to total system RAM, which is the GPU-addressable unified pool.
        "nvidia_cuda" => {
            let smi = command_output_with_timeout(
                std::process::Command::new("nvidia-smi")
                    .args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"]),
                3,
            )
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();

            // First non-empty line: MiB integer, or "[N/A]" / "N/A" on GB10.
            let first = smi.lines().map(str::trim).find(|l| !l.is_empty());
            if let Some(mib) = first.and_then(|l| l.parse::<f64>().ok())
                && mib > 0.0
            {
                let gb = mib / 1024.0;
                return (Some(gb), Some(gb));
            }

            // memory.total was N/A (or nvidia-smi missing): on DGX/GB10 the
            // unified system RAM IS the GPU-addressable pool, so use it as the
            // total. We deliberately leave per-device `gpu_vram_gb` None here
            // because the unified pool is not a discrete VRAM bank.
            if os_family == "linux-dgx"
                && let Some(ram_gb) = local_total_ram_gb()
            {
                return (None, Some(ram_gb));
            }
            (None, None)
        }

        // ── AMD ROCm ─────────────────────────────────────────────────────
        // Best-effort: ask rocm-smi for the VRAM total. Output format varies
        // across rocm versions, so we scan for the first byte-count we can
        // parse. If rocm-smi isn't usable, leave None (no /sys fallback that's
        // reliable across cards).
        "amd_rocm" => {
            let out = command_output_with_timeout(
                std::process::Command::new("rocm-smi").args(["--showmeminfo", "vram", "--csv"]),
                3,
            )
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();

            // CSV rows look like: `card0,<total_bytes>,<used_bytes>`. Grab the
            // largest plausible byte-count on any data line.
            let bytes = out
                .lines()
                .flat_map(|l| l.split(','))
                .filter_map(|f| f.trim().parse::<f64>().ok())
                .filter(|b| *b > 1_000_000.0) // ignore small ints (ids, used==0)
                .fold(0.0_f64, f64::max);
            if bytes > 0.0 {
                let gb = bytes / 1e9;
                return (Some(gb), Some(gb));
            }
            (None, None)
        }

        // ── Apple Silicon ────────────────────────────────────────────────
        // No discrete VRAM: the GPU addresses the unified memory pool, so the
        // addressable total is the system RAM (`sysctl -n hw.memsize`). Report
        // it as the total only; per-device `gpu_vram_gb` stays None since the
        // pool is shared with the CPU.
        "apple_silicon" => match local_total_ram_gb() {
            Some(ram_gb) => (None, Some(ram_gb)),
            None => (None, None),
        },

        // "none" / "integrated" / anything else: no VRAM to report.
        _ => (None, None),
    }
}

/// Best-effort total system RAM in GB. macOS: `sysctl -n hw.memsize` (bytes);
/// Linux: `/proc/meminfo` MemTotal (kB). Returns `None` if undetectable so
/// callers never fabricate a value. Used as the GPU-addressable unified pool
/// for Apple Silicon and the GB10/DGX nvidia-smi-N/A fallback.
fn local_total_ram_gb() -> Option<f64> {
    if std::env::consts::OS == "macos" {
        let out = command_output_with_timeout(
            std::process::Command::new("sysctl").args(["-n", "hw.memsize"]),
            3,
        )
        .filter(|o| o.status.success())?;
        let bytes: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        return Some(bytes / 1e9);
    }
    let txt = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in txt.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: f64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
            return Some(kb / 1e6);
        }
    }
    None
}

fn detect_gpu_model() -> Option<String> {
    match std::env::consts::OS {
        "macos" if std::env::consts::ARCH == "aarch64" => {
            // Could parse `system_profiler SPDisplaysDataType` but that's slow.
            // Return a generic label; precise model filled in by a later phase.
            Some("Apple Silicon GPU (Metal)".to_string())
        }
        "linux" => command_output_with_timeout(
            std::process::Command::new("nvidia-smi")
                .args(["--query-gpu=name", "--format=csv,noheader"]),
            3,
        )
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty()),
        _ => None,
    }
}

fn detect_primary_ip() -> String {
    // Try env override first.
    if let Ok(ip) = std::env::var("FORGEFLEET_PRIMARY_IP")
        && !ip.is_empty()
    {
        return ip;
    }
    pick_primary_lan_ip(&detect_all_ips()).unwrap_or_else(|| "127.0.0.1".to_string())
}

/// Pick the most-stable LAN IP from a list of detected interfaces.
///
/// Wired beats wireless: a wifi address is unstable (DHCP renewals, mesh
/// roaming, signal drops all change the address) and breaks
/// `computers.primary_ip` as the canonical address of record. When a host
/// is dual-homed, ethernet/thunderbolt/usb-eth wins — wifi is the fallback
/// only when no wired interface is up. Aura specifically: ethernet (.110)
/// is canonical, wifi is the tiebreaker.
///
/// Tiebreaker within each group is the order returned by ifconfig/ip addr
/// (i.e. lower interface index first).
fn pick_primary_lan_ip(ips: &[Ip]) -> Option<String> {
    let lan_ips: Vec<&Ip> = ips.iter().filter(|ip| ip.kind == "lan").collect();
    if let Some(wired) = lan_ips
        .iter()
        .find(|ip| ip.medium.as_deref() != Some("wifi"))
    {
        return Some(wired.ip.clone());
    }
    lan_ips.first().map(|ip| ip.ip.clone())
}

/// Run a subprocess with a hard wall-clock timeout (blocking thread).
/// Returns None if the command doesn't complete within `timeout_secs`.
/// Run a closure on a new thread with a wall-clock timeout.
/// Returns None if the closure doesn't complete within `timeout_secs`.
fn run_with_timeout<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
    timeout_secs: u64,
) -> Option<T> {
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(std::time::Duration::from_secs(timeout_secs)) {
        Ok(v) => Some(v),
        Err(_) => {
            tracing::debug!("run_with_timeout: closure did not complete within {timeout_secs}s");
            None
        }
    }
}

fn command_output_with_timeout(
    cmd: &mut std::process::Command,
    timeout_secs: u64,
) -> Option<std::process::Output> {
    use std::time::Duration;
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let pid = child.id();
    let start = std::time::Instant::now();
    loop {
        match child.try_wait().ok()? {
            Some(status) => {
                let mut out = std::process::Output {
                    status,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                };
                // best-effort read stdout/stderr; ignore errors
                let _ = child.stdout.take().map(|mut r| {
                    use std::io::Read;
                    let _ = r.read_to_end(&mut out.stdout);
                });
                let _ = child.stderr.take().map(|mut r| {
                    use std::io::Read;
                    let _ = r.read_to_end(&mut out.stderr);
                });
                return Some(out);
            }
            None => {
                if start.elapsed() > Duration::from_secs(timeout_secs) {
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::debug!(
                        pid,
                        ?cmd,
                        "command_output_with_timeout: killed subprocess after {timeout_secs}s"
                    );
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn detect_all_ips() -> Vec<Ip> {
    // Use absolute paths because launchd / systemd user services often
    // strip /sbin from PATH, and `ifconfig` lives there on macOS. Without
    // this, Command::new("ifconfig") returns "not found" → empty all_ips.
    // (Hit on ace 2026-04-25 — primary_ip wrongly fell back to 127.0.0.1.)
    let output = if std::env::consts::OS == "macos" {
        command_output_with_timeout(&mut std::process::Command::new("/sbin/ifconfig"), 3)
    } else {
        // /sbin/ip on Debian/Ubuntu, /usr/sbin/ip on RHEL — try both.
        command_output_with_timeout(
            std::process::Command::new("ip")
                .args(["-4", "-o", "addr", "show"])
                .env("PATH", "/usr/sbin:/sbin:/usr/bin:/bin"),
            3,
        )
    };

    let stdout = match output {
        Some(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => return Vec::new(),
    };

    let mut result = Vec::new();
    if std::env::consts::OS == "macos" {
        // Parse `ifconfig` macOS output.
        let mut current_iface = String::new();
        for line in stdout.lines() {
            if !line.starts_with('\t') && !line.starts_with(' ') && line.contains(':') {
                current_iface = line.split(':').next().unwrap_or("").to_string();
            } else if line.trim_start().starts_with("inet ")
                && let Some(ip) = line.split_whitespace().nth(1)
                && !ip.starts_with("127.")
                && !ip.starts_with("169.254.")
            {
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
                if let Some(addr) = parts[3].split('/').next()
                    && !addr.starts_with("127.")
                    && !addr.starts_with("169.254.")
                {
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
        let rate = command_output_with_timeout(
            std::process::Command::new("iw")
                .args(["dev", iface, "link"])
                .env("PATH", "/usr/sbin:/sbin:/usr/bin:/bin"),
            3,
        )
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
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
    let media =
        command_output_with_timeout(std::process::Command::new("/sbin/ifconfig").arg(iface), 3)
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();

    // Hardware-port lookup: AirPort/Wi-Fi → wifi medium.
    let hw_ports = command_output_with_timeout(
        std::process::Command::new("/usr/sbin/networksetup").args(["-listallhardwareports"]),
        3,
    )
    .filter(|o| o.status.success())
    .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    .unwrap_or_default();

    let medium = {
        // Walk hardware ports list looking for "Hardware Port: Foo / Device: <iface>".
        let mut hw_for_iface: Option<&str> = None;
        let mut last_hw: Option<&str> = None;
        for line in hw_ports.lines() {
            if let Some(rest) = line.strip_prefix("Hardware Port: ") {
                last_hw = Some(rest.trim());
            } else if let Some(rest) = line.strip_prefix("Device: ")
                && rest.trim() == iface
            {
                hw_for_iface = last_hw;
                break;
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

    #[test]
    fn aggregate_disk_bytes_sums_normal_mounts() {
        // (total, available) → used = total - available.
        let (total, used) = aggregate_disk_bytes([
            (1_000u64, 400u64), // used 600
            (2_000u64, 500u64), // used 1500
        ]);
        assert_eq!(total, 3_000);
        assert_eq!(used, 2_100);
        assert_eq!(total.saturating_sub(used), 900); // free
    }

    #[test]
    fn aggregate_disk_bytes_survives_pseudo_filesystems() {
        // A pseudo-fs reporting available > total (or total == 0) must NOT
        // underflow — used contribution clamps to 0, never a garbage huge u64.
        let (total, used) = aggregate_disk_bytes([
            (0u64, 100u64),     // total=0, available=100 → used 0
            (500u64, 900u64),   // available > total → used 0
            (1_000u64, 250u64), // normal → used 750
        ]);
        assert_eq!(total, 1_500);
        assert_eq!(used, 750);
        assert_eq!(total.saturating_sub(used), 750);
    }

    #[test]
    fn aggregate_disk_bytes_empty_is_zero() {
        assert_eq!(aggregate_disk_bytes(std::iter::empty()), (0, 0));
    }

    #[tokio::test]
    async fn build_beat_roundtrips_through_json() {
        let client = redis::Client::open("redis://localhost:56379").unwrap();
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
    fn gpu_vram_probe_none_kind_yields_no_values() {
        // "none"/"integrated" must never fabricate a VRAM figure.
        assert_eq!(detect_gpu_vram_gb("none", "linux-ubuntu"), (None, None));
        assert_eq!(
            detect_gpu_vram_gb("integrated", "linux-ubuntu"),
            (None, None)
        );
    }

    #[test]
    fn gpu_vram_probe_returns_consistent_tuple() {
        // Probe the real host: whatever gpu_kind we detect, the returned tuple
        // must be self-consistent — a per-device value implies a total, and
        // we never emit a negative/zero magnitude.
        let kind = detect_gpu_kind();
        let os = detect_os_info().family;
        let (per_device, total) = detect_gpu_vram_gb(&kind, &os);
        if let Some(v) = per_device {
            assert!(v > 0.0, "per-device vram must be positive when present");
            assert!(
                total.is_some(),
                "a discrete per-device value must imply a total"
            );
        }
        if let Some(t) = total {
            assert!(t > 0.0, "total vram must be positive when present");
        }
    }

    #[test]
    fn classify_iface_handles_common_cases() {
        assert_eq!(classify_iface("en0", "192.168.5.100"), "lan");
        assert_eq!(classify_iface("utun3", "100.64.5.100"), "tailscale");
        assert_eq!(classify_iface("eth0", "10.0.0.5"), "lan");
    }

    fn lan_ip(iface: &str, ip: &str, medium: &str) -> Ip {
        Ip {
            iface: iface.to_string(),
            ip: ip.to_string(),
            kind: "lan".to_string(),
            paired_with: None,
            link_speed_gbps: None,
            medium: Some(medium.to_string()),
        }
    }

    #[test]
    fn pick_primary_lan_ip_prefers_ethernet_over_wifi() {
        // Wifi listed first by ifconfig — ethernet must still win.
        let ips = vec![
            lan_ip("en1", "192.168.5.50", "wifi"),
            lan_ip("en0", "192.168.5.110", "ethernet"),
        ];
        assert_eq!(
            pick_primary_lan_ip(&ips),
            Some("192.168.5.110".to_string()),
            "ethernet must beat wifi regardless of ifconfig order"
        );
    }

    #[test]
    fn pick_primary_lan_ip_falls_back_to_wifi_when_only_choice() {
        let ips = vec![lan_ip("en1", "192.168.5.50", "wifi")];
        assert_eq!(pick_primary_lan_ip(&ips), Some("192.168.5.50".to_string()));
    }

    #[test]
    fn pick_primary_lan_ip_prefers_thunderbolt_over_wifi() {
        // Mediums that aren't "wifi" all count as wired for this selection.
        let ips = vec![
            lan_ip("en1", "192.168.5.50", "wifi"),
            lan_ip("usb0", "192.168.5.110", "usb-eth"),
        ];
        assert_eq!(pick_primary_lan_ip(&ips), Some("192.168.5.110".to_string()));
    }

    #[test]
    fn pick_primary_lan_ip_returns_none_for_empty() {
        assert_eq!(pick_primary_lan_ip(&[]), None);
    }
}
