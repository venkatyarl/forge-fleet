//! ForgeFleet shared types.
//!
//! Every struct here is `Serialize + Deserialize` for TOML/JSON round-tripping
//! and includes `Clone + Debug` at minimum.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ─── Operating System ────────────────────────────────────────────────────────

/// Supported operating systems in the fleet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OsType {
    MacOs,
    Linux,
    Windows,
}

impl std::fmt::Display for OsType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MacOs => write!(f, "macOS"),
            Self::Linux => write!(f, "Linux"),
            Self::Windows => write!(f, "Windows"),
        }
    }
}

// ─── GPU ─────────────────────────────────────────────────────────────────────

/// GPU accelerator type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuType {
    /// Apple Silicon (M1/M2/M3/M4/etc.) — unified memory, Metal
    AppleSilicon,
    /// NVIDIA discrete GPU — CUDA
    NvidiaCuda,
    /// AMD discrete or integrated GPU — ROCm / Vulkan
    AmdRdna,
    /// Intel integrated or Arc GPU — Vulkan / oneAPI
    IntelGpu,
    /// No GPU — CPU-only inference
    None,
}

impl std::fmt::Display for GpuType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AppleSilicon => write!(f, "Apple Silicon (Metal)"),
            Self::NvidiaCuda => write!(f, "NVIDIA (CUDA)"),
            Self::AmdRdna => write!(f, "AMD (RDNA)"),
            Self::IntelGpu => write!(f, "Intel GPU"),
            Self::None => write!(f, "CPU only"),
        }
    }
}

// ─── Memory ──────────────────────────────────────────────────────────────────

/// Memory type classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryType {
    /// Apple unified memory (shared CPU/GPU)
    Unified,
    /// Standard DDR4
    Ddr4,
    /// Standard DDR5
    Ddr5,
    /// NVIDIA HBM (High Bandwidth Memory)
    Hbm,
    /// Low-power DDR
    Lpddr,
    /// Unknown / undetected
    Unknown,
}

impl std::fmt::Display for MemoryType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unified => write!(f, "Unified"),
            Self::Ddr4 => write!(f, "DDR4"),
            Self::Ddr5 => write!(f, "DDR5"),
            Self::Hbm => write!(f, "HBM"),
            Self::Lpddr => write!(f, "LPDDR"),
            Self::Unknown => write!(f, "Unknown"),
        }
    }
}

// ─── Interconnect ────────────────────────────────────────────────────────────

/// Network / inter-node interconnect type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Interconnect {
    /// 10 Gbit Ethernet
    #[serde(rename = "ethernet_10g")]
    Ethernet10g,
    /// 2.5 Gbit Ethernet
    #[serde(rename = "ethernet_2.5g")]
    Ethernet2_5g,
    /// 1 Gbit Ethernet
    #[serde(rename = "ethernet_1g")]
    Ethernet1g,
    /// NVIDIA ConnectX / NVLink (DGX Spark pair)
    #[serde(rename = "nvlink")]
    NvLink,
    /// Thunderbolt bridge
    #[serde(rename = "thunderbolt")]
    Thunderbolt,
    /// Wi-Fi (not recommended for inference)
    #[serde(rename = "wifi")]
    Wifi,
    /// Unknown / undetected
    #[serde(rename = "unknown")]
    Unknown,
}

impl std::fmt::Display for Interconnect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ethernet10g => write!(f, "10GbE"),
            Self::Ethernet2_5g => write!(f, "2.5GbE"),
            Self::Ethernet1g => write!(f, "1GbE"),
            Self::NvLink => write!(f, "NVLink"),
            Self::Thunderbolt => write!(f, "Thunderbolt"),
            Self::Wifi => write!(f, "Wi-Fi"),
            Self::Unknown => write!(f, "Unknown"),
        }
    }
}

// ─── Inference Runtime ───────────────────────────────────────────────────────

/// Inference runtime engine that a node can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    LlamaCpp,
    Vllm,
    Mlx,
    TensorRt,
    Ollama,
}

impl std::fmt::Display for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LlamaCpp => write!(f, "llama.cpp"),
            Self::Vllm => write!(f, "vLLM"),
            Self::Mlx => write!(f, "MLX"),
            Self::TensorRt => write!(f, "TensorRT-LLM"),
            Self::Ollama => write!(f, "Ollama"),
        }
    }
}

// ─── Model Tier ──────────────────────────────────────────────────────────────

/// ForgeFleet model tiering system.
///
/// Requests start at the lowest sufficient tier and escalate on failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// ~9B params — fast, simple tasks
    #[serde(rename = "tier1")]
    Tier1 = 1,
    /// ~32B params — code generation
    #[serde(rename = "tier2")]
    Tier2 = 2,
    /// ~72B params — complex reasoning / review
    #[serde(rename = "tier3")]
    Tier3 = 3,
    /// ~200B+ params — expert / frontier
    #[serde(rename = "tier4")]
    Tier4 = 4,
}

impl Tier {
    /// Return the numeric tier (1–4).
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Try to parse from a numeric value.
    pub fn from_u8(n: u8) -> Option<Self> {
        match n {
            1 => Some(Self::Tier1),
            2 => Some(Self::Tier2),
            3 => Some(Self::Tier3),
            4 => Some(Self::Tier4),
            _ => None,
        }
    }
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tier1 => write!(f, "Tier 1 (9B fast)"),
            Self::Tier2 => write!(f, "Tier 2 (32B code)"),
            Self::Tier3 => write!(f, "Tier 3 (72B review)"),
            Self::Tier4 => write!(f, "Tier 4 (200B+ expert)"),
        }
    }
}

// ─── Node Role ───────────────────────────────────────────────────────────────

/// A node's role in the fleet.
///
/// The real fleet.toml uses "gateway" and "builder" while the election system
/// uses "leader" and "worker". All four are supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Cluster leader — routes tasks, manages state (election role)
    Leader,
    /// Standard worker — runs inference and tools (election role)
    #[default]
    Worker,
    /// Gateway node — primary control plane (fleet.toml role for taylor)
    Gateway,
    /// Builder node — runs builds, inference, tools (fleet.toml role for workers)
    Builder,
}

impl Role {
    /// Returns `true` if this role is a leader-like role (Leader or Gateway).
    pub fn is_leader_like(&self) -> bool {
        matches!(self, Self::Leader | Self::Gateway)
    }

    /// Returns `true` if this role is a worker-like role (Worker or Builder).
    pub fn is_worker_like(&self) -> bool {
        matches!(self, Self::Worker | Self::Builder)
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Leader => write!(f, "Leader"),
            Self::Worker => write!(f, "Worker"),
            Self::Gateway => write!(f, "Gateway"),
            Self::Builder => write!(f, "Builder"),
        }
    }
}

// ─── Node Status ─────────────────────────────────────────────────────────────

/// Liveness / operational status of a fleet node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    /// Node is up and accepting work
    #[default]
    Online,
    /// Node is up but not accepting new work (draining, updating, etc.)
    Degraded,
    /// Node is unreachable
    Offline,
    /// Node is booting or registering
    Starting,
    /// Node is in maintenance mode
    Maintenance,
}

impl std::fmt::Display for NodeStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Online => write!(f, "Online"),
            Self::Degraded => write!(f, "Degraded"),
            Self::Offline => write!(f, "Offline"),
            Self::Starting => write!(f, "Starting"),
            Self::Maintenance => write!(f, "Maintenance"),
        }
    }
}

// ─── Hardware ────────────────────────────────────────────────────────────────

/// Detected hardware profile for a fleet node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hardware {
    /// Operating system
    pub os: OsType,
    /// CPU model string (e.g. "Apple M4 Max")
    pub cpu_model: String,
    /// Number of CPU cores (logical)
    pub cpu_cores: u32,
    /// GPU type
    pub gpu: GpuType,
    /// GPU model string if detected (e.g. "NVIDIA GB110")
    pub gpu_model: Option<String>,
    /// Total system memory in GiB
    pub memory_gib: u64,
    /// Memory type
    pub memory_type: MemoryType,
    /// Network interconnect type
    pub interconnect: Interconnect,
    /// Primary inference runtimes available
    pub runtimes: Vec<Runtime>,
}

impl Hardware {
    /// Returns `true` if this node has a usable GPU.
    pub fn has_gpu(&self) -> bool {
        self.gpu != GpuType::None
    }

    /// Best inference runtime for this hardware.
    pub fn preferred_runtime(&self) -> Runtime {
        match self.gpu {
            GpuType::AppleSilicon => Runtime::LlamaCpp, // or MLX
            GpuType::NvidiaCuda => Runtime::Vllm,
            GpuType::AmdRdna => Runtime::LlamaCpp,
            GpuType::IntelGpu => Runtime::LlamaCpp,
            GpuType::None => Runtime::LlamaCpp,
        }
    }
}

// ─── Model ───────────────────────────────────────────────────────────────────

/// An LLM model available in the fleet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    /// Unique model identifier (e.g. "qwen3-32b-q4")
    pub id: String,
    /// Human-readable name
    pub name: String,
    /// Model tier
    pub tier: Tier,
    /// Parameter count (billions)
    pub params_b: f32,
    /// Quantization (e.g. "Q4_K_M", "FP16")
    pub quant: String,
    /// GGUF file path on the node
    pub path: String,
    /// Context window size
    pub ctx_size: u32,
    /// Runtime this model should use
    pub runtime: Runtime,
    /// Which node(s) can serve this model
    pub nodes: Vec<String>,
}

// ─── Node ────────────────────────────────────────────────────────────────────

/// A fleet node definition (from fleet.toml or discovery).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    /// Unique node ID
    pub id: Uuid,
    /// Human-readable name (e.g. "taylor", "james", "marcus")
    pub name: String,
    /// Hostname or IP address
    pub host: String,
    /// API port
    pub port: u16,
    /// Node role
    pub role: Role,
    /// Priority in leader election (lower = more preferred)
    pub election_priority: u32,
    /// Current status
    pub status: NodeStatus,
    /// Hardware profile
    pub hardware: Hardware,
    /// Models loaded on this node
    pub models: Vec<String>,
    /// Last heartbeat timestamp
    pub last_heartbeat: Option<DateTime<Utc>>,
    /// When this node was first seen
    pub registered_at: DateTime<Utc>,
}

impl Node {
    /// Check if the node's last heartbeat is within `timeout_secs`.
    pub fn is_alive(&self, timeout_secs: i64) -> bool {
        match self.last_heartbeat {
            Some(ts) => {
                let elapsed = Utc::now().signed_duration_since(ts);
                elapsed.num_seconds() < timeout_secs
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tier_ordering() {
        assert!(Tier::Tier1 < Tier::Tier2);
        assert!(Tier::Tier2 < Tier::Tier3);
        assert!(Tier::Tier3 < Tier::Tier4);
    }

    #[test]
    fn test_tier_roundtrip() {
        for n in 1..=4u8 {
            let tier = Tier::from_u8(n).unwrap();
            assert_eq!(tier.as_u8(), n);
        }
        assert!(Tier::from_u8(0).is_none());
        assert!(Tier::from_u8(5).is_none());
    }

    #[test]
    fn test_role_default() {
        assert_eq!(Role::default(), Role::Worker);
    }

    #[test]
    fn test_role_leader_like() {
        assert!(Role::Leader.is_leader_like());
        assert!(Role::Gateway.is_leader_like());
        assert!(!Role::Worker.is_leader_like());
        assert!(!Role::Builder.is_leader_like());
    }

    #[test]
    fn test_role_worker_like() {
        assert!(Role::Worker.is_worker_like());
        assert!(Role::Builder.is_worker_like());
        assert!(!Role::Leader.is_worker_like());
        assert!(!Role::Gateway.is_worker_like());
    }

    #[test]
    fn test_role_serde_gateway_builder() {
        let gw: Role = serde_json::from_str(r#""gateway""#).unwrap();
        assert_eq!(gw, Role::Gateway);
        let b: Role = serde_json::from_str(r#""builder""#).unwrap();
        assert_eq!(b, Role::Builder);
    }

    #[test]
    fn test_node_status_default() {
        assert_eq!(NodeStatus::default(), NodeStatus::Online);
    }

    #[test]
    fn test_hardware_has_gpu() {
        let hw = Hardware {
            os: OsType::MacOs,
            cpu_model: "Apple M4 Max".into(),
            cpu_cores: 16,
            gpu: GpuType::AppleSilicon,
            gpu_model: None,
            memory_gib: 128,
            memory_type: MemoryType::Unified,
            interconnect: Interconnect::Ethernet10g,
            runtimes: vec![Runtime::LlamaCpp, Runtime::Mlx],
        };
        assert!(hw.has_gpu());
        assert_eq!(hw.preferred_runtime(), Runtime::LlamaCpp);
    }

    #[test]
    fn test_serialize_os_type() {
        let json = serde_json::to_string(&OsType::MacOs).unwrap();
        assert_eq!(json, r#""mac_os""#);
        let rt: OsType = serde_json::from_str(&json).unwrap();
        assert_eq!(rt, OsType::MacOs);
    }

    #[test]
    fn test_serialize_gpu_type() {
        let json = serde_json::to_string(&GpuType::NvidiaCuda).unwrap();
        assert_eq!(json, r#""nvidia_cuda""#);
        let rt: GpuType = serde_json::from_str(&json).unwrap();
        assert_eq!(rt, GpuType::NvidiaCuda);
    }

    #[test]
    fn test_serialize_tier() {
        let json = serde_json::to_string(&Tier::Tier3).unwrap();
        assert_eq!(json, r#""tier3""#);
        let rt: Tier = serde_json::from_str(&json).unwrap();
        assert_eq!(rt, Tier::Tier3);
    }

    #[test]
    fn test_serialize_node_full() {
        let node = Node {
            id: uuid::Uuid::nil(),
            name: "taylor".into(),
            host: "192.168.5.100".into(),
            port: 51800,
            role: Role::Leader,
            election_priority: 1,
            status: NodeStatus::Online,
            hardware: Hardware {
                os: OsType::MacOs,
                cpu_model: "Apple M4 Max".into(),
                cpu_cores: 16,
                gpu: GpuType::AppleSilicon,
                gpu_model: None,
                memory_gib: 128,
                memory_type: MemoryType::Unified,
                interconnect: Interconnect::Ethernet10g,
                runtimes: vec![Runtime::LlamaCpp],
            },
            models: vec!["qwen3-32b".into()],
            last_heartbeat: None,
            registered_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string_pretty(&node).unwrap();
        let rt: Node = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.name, "taylor");
        assert_eq!(rt.role, Role::Leader);
    }

    #[test]
    fn test_node_is_alive() {
        let mut node = Node {
            id: uuid::Uuid::nil(),
            name: "test".into(),
            host: "127.0.0.1".into(),
            port: 8080,
            role: Role::Worker,
            election_priority: 99,
            status: NodeStatus::Online,
            hardware: Hardware {
                os: OsType::Linux,
                cpu_model: "AMD Ryzen 7 5700U".into(),
                cpu_cores: 16,
                gpu: GpuType::None,
                gpu_model: None,
                memory_gib: 64,
                memory_type: MemoryType::Ddr4,
                interconnect: Interconnect::Ethernet1g,
                runtimes: vec![Runtime::LlamaCpp],
            },
            models: vec![],
            last_heartbeat: Some(chrono::Utc::now()),
            registered_at: chrono::Utc::now(),
        };
        assert!(node.is_alive(30));

        // ancient heartbeat → not alive
        node.last_heartbeat = Some(chrono::Utc::now() - chrono::Duration::seconds(120));
        assert!(!node.is_alive(30));

        // no heartbeat → not alive
        node.last_heartbeat = None;
        assert!(!node.is_alive(30));
    }
}
