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

/// Top-level Pulse v2 beat payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PulseBeatV2 {
    /// Always `2` for this schema revision.
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
    /// `"v4"` | `"v6"` | `"loopback"` | etc.
    pub kind: String,
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
    pub can_serve_openclaw_gateway: bool,
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
// Skeleton builder
// -----------------------------------------------------------------------------

impl PulseBeatV2 {
    /// Produce a default beat with sensible zero-values so the publisher can
    /// fill fields progressively. `timestamp` is set to `Utc::now()` at call
    /// time; callers that want a deterministic timestamp should overwrite it.
    pub fn skeleton(computer_name: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            pulse_protocol_version: 2,
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
                can_serve_openclaw_gateway: false,
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
}
