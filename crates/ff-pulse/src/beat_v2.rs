//! Pulse v2 beat payload.
//!
//! This module defines the richer heartbeat schema used by Pulse v2 publishers.
//! The top-level [`PulseBeatV2`] struct is what each node publishes once per
//! interval; sub-structs break out hardware, memory, LLM server, docker and
//! peer information so the dashboard can render detailed views without
//! additional queries.
//!
//! The module is intentionally self-contained — it does not modify the
//! existing v1 [`crate::heartbeat::HeartbeatPublisher`] flow. Publishers that
//! want to emit v2 beats can build a [`PulseBeatV2`] from
//! [`PulseBeatV2::skeleton`] and progressively fill fields.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Operating system descriptor reported per beat. Optional on the wire so
/// daemons running old code can publish beats without an `os` field; the
/// materializer treats absence as "unknown, don't touch the computers row".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OsInfo {
    /// "macos", "linux-ubuntu", "linux-dgx", "linux-debian", "windows", "unknown".
    /// Pre-classified by the daemon so consumers (auto-upgrade playbook
    /// resolver, etc.) don't have to re-derive it.
    pub family: String,
    /// Distribution ID from /etc/os-release ID= field on Linux, "macOS" on Mac,
    /// "Windows" on Windows, "" otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub distribution: String,
    /// VERSION_ID from /etc/os-release on Linux, product version on Mac, "" otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    /// Output of `uname -r` (kernel release). DGX OS is detected by this
    /// ending in `-nvidia`; see memory: dgx-spark-specs.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kernel: String,
    /// CPU architecture from `std::env::consts::ARCH` ("aarch64", "x86_64").
    /// Key column of the server-policy resolver; empty on beats from older
    /// daemons — the materializer then derives it from os_family/gpu_kind.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub arch: String,
}

/// Top-level Pulse v2 beat payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PulseBeatV2 {
    /// Always [`crate::PULSE_SCHEMA_VERSION`] for beats built by this crate;
    /// readers gate on [`crate::is_schema_compatible`] (one-generation rule).
    pub pulse_protocol_version: u32,
    /// Populated after enrollment; `None` until the node is fully enrolled.
    pub computer_id: Option<Uuid>,
    pub computer_name: String,
    pub timestamp: DateTime<Utc>,
    /// Monotonic epoch counter, per-computer.
    pub epoch: u64,
    /// `"leader"` or `"member"`.
    pub role_claimed: String,
    pub election_priority: i32,
    pub is_yielding: bool,
    /// Last-Will-and-Testament flag: set true when publishing a graceful-exit beat.
    pub going_offline: bool,
    pub maintenance_mode: bool,
    pub network: NetworkInfo,
    /// V87+: OS family + distribution. Default-empty for backward compat with
    /// daemons that publish beats before this field was added.
    #[serde(default)]
    pub os: OsInfo,
    /// V89+: 10-char git SHA prefix of the binary publishing this beat.
    /// Materializer writes it to `computer_software.installed_version` for
    /// `ff_git` + `forgefleetd_git` rows so `ff fleet versions` reflects
    /// live state without an explicit post-upgrade refresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_sha: Option<String>,
    /// Ground-truth absolute path of this node's ForgeFleet source tree —
    /// the dir the daemon builds/self-upgrades from (Taylor: `~/projects/
    /// forge-fleet`; workers: `~/.forgefleet/sub-agents/sub-agent-0/forge-fleet`).
    /// The leader's auto-upgrade reads `computers.source_tree_path` to know
    /// what to `cd` into; if it's NULL the leader self-upgrade silently skips
    /// (surfaced 2026-06-08 — only Taylor had it set, so leadership moving to
    /// any other node would break self-upgrade). Reported here so the
    /// materializer heals the column from ground truth, same idiom as
    /// `primary_ip`. `None` from daemons running pre-this-field code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_tree_path: Option<String>,
    pub hardware: HardwareInfo,
    pub load: LoadInfo,
    pub memory: MemoryInfo,
    pub capabilities: Capabilities,
    pub llm_servers: Vec<LlmServer>,
    pub available_models: Vec<AvailableModel>,
    pub installed_software: Vec<InstalledSoftware>,
    pub docker: DockerStatus,
    pub peers_seen: Vec<PeerSeen>,
    pub db_topology: DbTopology,
    /// Leader-only: the config version this node is serving.
    pub config_version: Option<u64>,
    /// V43+: multi-host deployment participation (ray clusters, NFS mounts).
    /// Absent on single-host-only daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multi_host_participation: Option<MultiHostParticipation>,
    /// V43+: bugs/panics this daemon hit since its last beat, to be
    /// aggregated by the leader into `fleet_bug_reports`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub encountered_bugs: Vec<EncounteredBug>,
    /// V43+: compact snapshot of tasks this daemon is actively working on
    /// or waiting on. Reported so the leader/CLI/TUI/web can show a
    /// fleet-wide task view. Individual task details live in the authoritative
    /// `fleet_tasks` table (forthcoming V44); this field is a liveness hint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_tasks: Vec<LocalTaskSnapshot>,
    /// V174+: last time this daemon's work-item dispatch loop ticked, as
    /// reported by the heartbeat publisher. The materializer persists it to
    /// `computers.dispatch_tick_at` so the scheduler/reaper can detect a
    /// heartbeat-healthy node whose dispatch loop has stalled. Older daemons
    /// omit this field; it defaults to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_tick_at: Option<DateTime<Utc>>,
    /// Intended recipient computer names for targeted pulse routing. Empty
    /// means no specific target (broadcast to all consumers). Older beats do
    /// not include this field, so it defaults to empty for backward
    /// compatibility during mixed-generation deployments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub receivers: Vec<String>,
}

// -----------------------------------------------------------------------------
// Network
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInfo {
    pub primary_ip: String,
    pub all_ips: Vec<Ip>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ip {
    pub iface: String,
    pub ip: String,
    /// `"v4"` | `"v6"` | `"loopback"` | `"cx7-fabric"` | `"ib-fabric"` |
    /// `"roce-fabric"` | `"tailscale"` | `"public"` | `"lan"`.
    /// Fabric kinds (`*-fabric`) are private to a paired-host link — never
    /// route API traffic to them; they're plumbing for NCCL / ray / etc.
    pub kind: String,
    /// For fabric-kind IPs, the name of the computer on the other end of
    /// the private link. Used by the materializer to auto-upsert a
    /// `fabric_pairs` row when both sides claim each other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paired_with: Option<String>,
    /// Physical link speed in Gbps, if known (from ethtool or similar).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_speed_gbps: Option<u32>,
    /// Physical medium: `"ethernet"` | `"wifi"` | `"cx7"` | `"usb-eth"` |
    /// `"thunderbolt"` | `"loopback"`. Distinct from `kind` (routing
    /// semantics) — `medium` is the link layer so ff can prefer faster
    /// paths for bulk transfers and avoid wifi for tensor-parallel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub medium: Option<String>,
}

// -----------------------------------------------------------------------------
// Hardware / Load / Memory
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareInfo {
    pub cpu_cores: i32,
    pub ram_gb: i32,
    pub disk_gb: i32,
    pub gpu: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadInfo {
    pub cpu_pct: f64,
    pub ram_pct: f64,
    pub disk_free_gb: f64,
    pub gpu_pct: f64,
    pub active_inference_requests: i32,
    pub active_agent_sessions: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryInfo {
    pub ram_total_gb: f64,
    pub ram_used_gb: f64,
    pub ram_free_gb: f64,
    pub llm_ram_allocated_gb: f64,
    pub ram_available_for_new_llm_gb: f64,
    pub vram_total_gb: Option<f64>,
    pub vram_used_gb: Option<f64>,
    pub vram_free_gb: Option<f64>,
    pub llm_vram_allocated_gb: Option<f64>,
}

// -----------------------------------------------------------------------------
// Capabilities
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub can_serve_ff_gateway: bool,
    pub can_host_postgres_replica: bool,
    pub can_host_redis_replica: bool,
    /// `"none" | "integrated" | "apple_silicon" | "nvidia_cuda" | "amd_rocm"`.
    pub gpu_kind: String,
    pub gpu_count: i32,
    pub gpu_vram_gb: Option<f64>,
    pub gpu_total_vram_gb: Option<f64>,
    pub can_run_cuda: bool,
    pub can_run_metal: bool,
    pub can_run_rocm: bool,
    pub recommended_runtimes: Vec<String>,
    pub max_runnable_model_gb: Option<f64>,
}

// -----------------------------------------------------------------------------
// LLM servers
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmServer {
    pub deployment_id: Uuid,
    /// `"llama.cpp" | "mlx_lm" | "vllm" | "ollama"`.
    pub runtime: String,
    pub endpoint: String,
    pub openai_compatible: bool,
    pub model: LlmServerModel,
    /// `"loading" | "active" | "idle" | "error" | "stopping"`.
    pub status: String,
    pub pid: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub cluster: ClusterInfo,
    pub queue_depth: i32,
    pub active_requests: i32,
    pub tokens_per_sec_last_min: f64,
    pub gpu_memory_used_gb: Option<f64>,
    pub is_healthy: bool,
    pub last_probed_at: DateTime<Utc>,
    pub memory_used: LlmMemoryUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmServerModel {
    pub id: String,
    pub display_name: String,
    pub loaded_path: String,
    pub context_window: i32,
    pub parallel_slots: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterInfo {
    pub cluster_id: Option<String>,
    pub role: String,
    pub tensor_parallel_size: i32,
    pub pipeline_parallel_size: i32,
    pub peers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMemoryUsage {
    pub model_weights_gb: f64,
    pub kv_cache_gb: f64,
    pub overhead_gb: f64,
    pub total_gb: f64,
}

// -----------------------------------------------------------------------------
// Inventory
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailableModel {
    pub id: String,
    pub size_gb: f64,
    pub runtime_compat: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledSoftware {
    pub id: String,
    pub version: String,
    pub install_source: Option<String>,
    pub install_path: Option<String>,
    /// Optional JSON metadata carried through to `computer_software.metadata`.
    /// Used for signals that don't fit any other column — currently only
    /// `{ "git_state": "pushed|unpushed|dirty|unknown" }` for `ff_git` /
    /// `forgefleetd_git` rows so the auto-upgrade gate can refuse dirty
    /// builds without re-probing the leader.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

// -----------------------------------------------------------------------------
// Docker
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerStatus {
    pub daemon_running: bool,
    pub total_cpu_pct: f64,
    pub total_memory_mb: f64,
    pub memory_limit_mb: f64,
    pub projects: Vec<DockerProject>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerProject {
    pub name: String,
    pub compose_file: Option<String>,
    pub status: String,
    pub containers: Vec<DockerContainer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerContainer {
    pub name: String,
    pub container_id: String,
    pub image: String,
    pub ports: Vec<String>,
    pub status: String,
    pub health: Option<String>,
    pub cpu_pct: f64,
    pub memory_mb: f64,
    pub memory_limit_mb: f64,
    pub uptime_sec: u64,
}

// -----------------------------------------------------------------------------
// Peers / DB topology
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerSeen {
    pub name: String,
    pub last_beat_at: DateTime<Utc>,
    pub status: String,
    pub epoch_witnessed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbTopology {
    pub postgres_primary: Option<String>,
    pub postgres_replicas: Vec<String>,
    pub redis_primary: Option<String>,
    pub redis_replicas: Vec<String>,
}

// -----------------------------------------------------------------------------
// V43: multi-host deployment participation + bug reporting
// -----------------------------------------------------------------------------

/// Reports cross-host resources this daemon participates in — ray clusters
/// it's a member of, shared NFS mounts consumed, etc. Empty by default;
/// filled in by the heartbeat collector when it detects these locally.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MultiHostParticipation {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ray_clusters: Vec<RayClusterMembership>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared_mounts: Vec<SharedMountInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RayClusterMembership {
    /// Matches `llm_clusters.id` in Postgres.
    pub cluster_id: String,
    /// `"head"` | `"worker"` | `"standalone"`.
    pub role: String,
    /// Head node's ray GCS endpoint, e.g. `"10.42.0.1:6379"`.
    pub head_endpoint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedMountInfo {
    /// Matches `shared_volumes.name`.
    pub volume_name: String,
    /// Name of the computer exporting the share.
    pub export_host: String,
    /// Where this computer has the share mounted, e.g. `/home/sia/models`.
    pub local_path: String,
    /// `"nfs4"` | `"sshfs"` | `"ceph"` | ...
    pub protocol: String,
}

/// A single bug / panic this daemon hit since its last beat. The leader
/// deduplicates by `signature` into `fleet_self_heal_queue` per
/// `self-heal-coordination.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncounteredBug {
    /// Stable hash of `(file_path, line, error_class)` — identical across
    /// daemons that hit the same bug so the leader can count occurrences.
    pub signature: String,
    pub file_path: Option<String>,
    pub line_number: Option<u32>,
    /// Coarse taxonomy: `"panic:str_index"`, `"cargo:type_mismatch"`,
    /// `"runtime:nccl"`, `"vllm:cutlass_scaled_mm"`, etc.
    pub error_class: String,
    /// Truncated stack excerpt (chars-bounded, not bytes, to avoid the same
    /// utf8 panic we're instrumenting).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack_excerpt: Option<String>,
    /// `ff --version` value when this bug was hit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_version: Option<String>,
    /// Severity tier per plan. Defaults to `"T1"` (auto-fix-eligible crashes).
    #[serde(default = "default_tier")]
    pub tier: String,
}

fn default_tier() -> String {
    "T1".to_string()
}

/// A compact task snapshot for the beat. Full detail lives in `fleet_tasks`
/// (V44); this is a liveness hint so the operator sees what each daemon is
/// actively doing without hitting the DB.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalTaskSnapshot {
    /// UUID referencing `fleet_tasks.id`, or a local-only id for tasks
    /// that haven't been promoted to the fleet queue yet.
    pub task_id: String,
    /// `"code_fix"` | `"research_subtask"` | `"model_benchmark"` |
    /// `"self_heal_writer"` | `"self_heal_reviewer"` | `"user_session"` etc.
    pub task_type: String,
    /// Human-readable one-liner. Kept short (< 200 chars) for beat size.
    pub summary: String,
    /// `"pending"` | `"running"` | `"awaiting_peer"` | `"awaiting_review"` |
    /// `"blocked"` | `"completing"`.
    pub status: String,
    pub progress_pct: Option<f32>,
    /// Latest progress message this daemon recorded. Small (< 120 chars).
    pub progress_message: Option<String>,
    /// When it started on this daemon.
    pub started_at: Option<DateTime<Utc>>,
}

// -----------------------------------------------------------------------------
// Skeleton builder
// -----------------------------------------------------------------------------

impl PulseBeatV2 {
    /// Produce a default beat with sensible zero-values so the publisher can
    /// fill fields progressively. `timestamp` is set to `Utc::now()` at call
    /// time; callers that want a deterministic timestamp should overwrite it.
    pub fn skeleton(computer_name: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            pulse_protocol_version: crate::PULSE_SCHEMA_VERSION,
            computer_id: None,
            computer_name: computer_name.into(),
            timestamp: now,
            epoch: 0,
            role_claimed: "member".to_string(),
            election_priority: 0,
            is_yielding: false,
            going_offline: false,
            maintenance_mode: false,
            network: NetworkInfo {
                primary_ip: String::new(),
                all_ips: Vec::new(),
            },
            os: OsInfo::default(),
            build_sha: None,
            source_tree_path: None,
            hardware: HardwareInfo {
                cpu_cores: 0,
                ram_gb: 0,
                disk_gb: 0,
                gpu: None,
            },
            load: LoadInfo {
                cpu_pct: 0.0,
                ram_pct: 0.0,
                disk_free_gb: 0.0,
                gpu_pct: 0.0,
                active_inference_requests: 0,
                active_agent_sessions: 0,
            },
            memory: MemoryInfo {
                ram_total_gb: 0.0,
                ram_used_gb: 0.0,
                ram_free_gb: 0.0,
                llm_ram_allocated_gb: 0.0,
                ram_available_for_new_llm_gb: 0.0,
                vram_total_gb: None,
                vram_used_gb: None,
                vram_free_gb: None,
                llm_vram_allocated_gb: None,
            },
            capabilities: Capabilities {
                can_serve_ff_gateway: false,
                can_host_postgres_replica: false,
                can_host_redis_replica: false,
                gpu_kind: "none".to_string(),
                gpu_count: 0,
                gpu_vram_gb: None,
                gpu_total_vram_gb: None,
                can_run_cuda: false,
                can_run_metal: false,
                can_run_rocm: false,
                recommended_runtimes: Vec::new(),
                max_runnable_model_gb: None,
            },
            llm_servers: Vec::new(),
            available_models: Vec::new(),
            installed_software: Vec::new(),
            docker: DockerStatus {
                daemon_running: false,
                total_cpu_pct: 0.0,
                total_memory_mb: 0.0,
                memory_limit_mb: 0.0,
                projects: Vec::new(),
            },
            peers_seen: Vec::new(),
            db_topology: DbTopology {
                postgres_primary: None,
                postgres_replicas: Vec::new(),
                redis_primary: None,
                redis_replicas: Vec::new(),
            },
            config_version: None,
            multi_host_participation: None,
            encountered_bugs: Vec::new(),
            local_tasks: Vec::new(),
            dispatch_tick_at: None,
            receivers: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_has_protocol_version_2() {
        let beat = PulseBeatV2::skeleton("taylor");
        assert_eq!(beat.pulse_protocol_version, 2);
        assert_eq!(beat.computer_name, "taylor");
        assert_eq!(beat.role_claimed, "member");
        assert!(beat.computer_id.is_none());
        assert!(beat.llm_servers.is_empty());
        assert!(!beat.docker.daemon_running);
    }

    #[test]
    fn skeleton_roundtrips_through_json() {
        let beat = PulseBeatV2::skeleton("marcus");
        let json = serde_json::to_string(&beat).expect("serialize");
        let parsed: PulseBeatV2 = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.computer_name, "marcus");
        assert_eq!(parsed.pulse_protocol_version, 2);
    }

    /// Backward-compatibility guard: a beat serialized by an older daemon
    /// (before newer optional fields were added) must still deserialize into
    /// the current schema. This protects rolling deployments where the fleet
    /// runs mixed versions and older beats may still be in Redis/Postgres.
    #[test]
    fn legacy_beat_schema_deserializes_with_defaults() {
        let legacy_json = r#"{
            "pulse_protocol_version": 2,
            "computer_id": null,
            "computer_name": "legacy-node",
            "timestamp": "2026-01-01T00:00:00Z",
            "epoch": 1,
            "role_claimed": "member",
            "election_priority": 0,
            "is_yielding": false,
            "going_offline": false,
            "maintenance_mode": false,
            "network": {
                "primary_ip": "10.0.0.1",
                "all_ips": [
                    { "iface": "eth0", "ip": "10.0.0.1", "kind": "lan" }
                ]
            },
            "hardware": { "cpu_cores": 8, "ram_gb": 32, "disk_gb": 500, "gpu": null },
            "load": {
                "cpu_pct": 12.5,
                "ram_pct": 34.0,
                "disk_free_gb": 123.4,
                "gpu_pct": 0.0,
                "active_inference_requests": 0,
                "active_agent_sessions": 0
            },
            "memory": {
                "ram_total_gb": 32.0,
                "ram_used_gb": 10.0,
                "ram_free_gb": 22.0,
                "llm_ram_allocated_gb": 0.0,
                "ram_available_for_new_llm_gb": 19.0,
                "vram_total_gb": null,
                "vram_used_gb": null,
                "vram_free_gb": null,
                "llm_vram_allocated_gb": null
            },
            "capabilities": {
                "can_serve_ff_gateway": true,
                "can_host_postgres_replica": false,
                "can_host_redis_replica": false,
                "gpu_kind": "none",
                "gpu_count": 0,
                "gpu_vram_gb": null,
                "gpu_total_vram_gb": null,
                "can_run_cuda": false,
                "can_run_metal": false,
                "can_run_rocm": false,
                "recommended_runtimes": [],
                "max_runnable_model_gb": null
            },
            "llm_servers": [],
            "available_models": [],
            "installed_software": [],
            "docker": {
                "daemon_running": false,
                "total_cpu_pct": 0.0,
                "total_memory_mb": 0.0,
                "memory_limit_mb": 0.0,
                "projects": []
            },
            "peers_seen": [],
            "db_topology": {
                "postgres_primary": null,
                "postgres_replicas": [],
                "redis_primary": null,
                "redis_replicas": []
            },
            "config_version": null
        }"#;

        let parsed: PulseBeatV2 = serde_json::from_str(legacy_json)
            .expect("legacy beat schema must deserialize into current struct");

        assert_eq!(parsed.computer_name, "legacy-node");
        assert_eq!(parsed.pulse_protocol_version, 2);
        assert_eq!(parsed.network.primary_ip, "10.0.0.1");

        // Fields added after the original schema must default safely.
        assert!(parsed.os.family.is_empty());
        assert!(parsed.os.distribution.is_empty());
        assert!(parsed.os.version.is_empty());
        assert!(parsed.os.kernel.is_empty());
        assert!(parsed.os.arch.is_empty());
        assert!(parsed.build_sha.is_none());
        assert!(parsed.source_tree_path.is_none());
        assert!(parsed.multi_host_participation.is_none());
        assert!(parsed.encountered_bugs.is_empty());
        assert!(parsed.local_tasks.is_empty());
        assert!(parsed.dispatch_tick_at.is_none());
    }

    #[test]
    fn beat_without_receivers_field_defaults_to_empty() {
        let mut beat = PulseBeatV2::skeleton("marcus");
        beat.receivers = vec!["node-a".to_string(), "node-b".to_string()];
        let mut value = serde_json::to_value(&beat).expect("serialize");
        value.as_object_mut().expect("object").remove("receivers");
        let json = serde_json::to_string(&value).expect("re-serialize");
        let parsed: PulseBeatV2 = serde_json::from_str(&json).expect("deserialize older beat");
        assert!(parsed.receivers.is_empty());
    }

    /// Cross-generation compat contract (the 2026-07-19 mixed-fleet incident:
    /// receivers on an older binary must parse beats from newer senders and
    /// vice versa during a rolling deploy).
    ///
    /// Backward: a beat JSON written by an OLDER daemon — i.e. with every
    /// `#[serde(default)]`-guarded later-generation field absent — must still
    /// deserialize. Forward: a beat carrying an unknown future field must also
    /// deserialize (serde ignores unknown keys by default; this pins that no
    /// `deny_unknown_fields` ever sneaks onto the struct).
    #[test]
    fn beat_deserializes_across_schema_generations() {
        let beat = PulseBeatV2::skeleton("sia");
        let mut json: serde_json::Value = serde_json::to_value(&beat).expect("to_value");
        let obj = json.as_object_mut().expect("beat serializes to an object");

        // Older-generation sender: later added-with-default fields absent.
        for later_field in [
            "os",
            "build_sha",
            "source_tree_path",
            "multi_host_participation",
            "encountered_bugs",
            "local_tasks",
            "dispatch_tick_at",
            "receivers",
        ] {
            obj.remove(later_field);
        }
        let parsed: PulseBeatV2 =
            serde_json::from_value(json.clone()).expect("older-format beat must deserialize");
        assert_eq!(parsed.computer_name, "sia");
        assert!(parsed.encountered_bugs.is_empty());
        assert!(parsed.build_sha.is_none());

        // Newer-generation sender: an unknown field must be ignored, not fatal.
        json.as_object_mut().unwrap().insert(
            "field_from_the_future".into(),
            serde_json::json!({"nested": true}),
        );
        let parsed: PulseBeatV2 =
            serde_json::from_value(json).expect("beat with unknown future field must deserialize");
        assert_eq!(parsed.pulse_protocol_version, 2);
    }
}
