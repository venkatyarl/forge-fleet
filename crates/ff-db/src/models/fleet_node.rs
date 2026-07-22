//! Typed persistence model for a fleet node.
//!
//! A `FleetNode` is the single resource type produced by LEFT-JOINing the
//! `fleet_workers` table (a node's registered role/config: IP, election
//! priority, capabilities, preferences) with the `computers` table (a node's
//! physical hardware identity: GPU, true RAM/CPU, lifecycle status). The two
//! tables coexist in Postgres — see schema V14 — but callers should not need
//! to know that; `FleetNode` merges both into one entity per node.
//!
//! Fields sourced from `fleet_workers` use their bare column name. Fields
//! sourced from `computers` are prefixed `computer_` (or, for GPU attributes
//! that only exist on `computers`, left bare but `Option`-typed) so the two
//! origins stay distinguishable without needing two structs.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

fn default_runtime() -> String {
    "unknown".to_string()
}
fn default_models_dir() -> String {
    "~/models".to_string()
}
fn default_disk_quota_pct() -> i32 {
    80
}
fn default_sub_agent_count() -> i32 {
    1
}
fn default_tooling() -> JsonValue {
    serde_json::json!({})
}

/// The persistent representation of a fleet node, merging `fleet_workers`
/// (role/config) and `computers` (physical hardware) attributes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetNode {
    // ─── fleet_workers: registered role/config ─────────────────────────────
    pub name: String,
    pub ip: String,
    pub ssh_user: String,
    pub ram_gb: i32,
    pub cpu_cores: i32,
    pub os: String,
    pub role: String,
    pub election_priority: i32,
    pub hardware: String,
    pub alt_ips: JsonValue,
    pub capabilities: JsonValue,
    pub preferences: JsonValue,
    pub resources: JsonValue,
    pub status: String,
    /// Inference runtime: 'llama.cpp' | 'mlx' | 'vllm' | 'unknown'.
    /// Added in schema V11; defaults to 'unknown' for pre-existing rows.
    #[serde(default = "default_runtime")]
    pub runtime: String,
    /// Models directory on the node (default '~/models').
    #[serde(default = "default_models_dir")]
    pub models_dir: String,
    /// Disk quota for the models dir as a percentage of total disk (default 80).
    #[serde(default = "default_disk_quota_pct")]
    pub disk_quota_pct: i32,
    /// Concurrent defer-worker slots on this node (default 1). Scales agent-
    /// heavy workloads. Added in schema V12.
    #[serde(default = "default_sub_agent_count")]
    pub sub_agent_count: i32,
    /// GitHub owner/account this node is authenticated against (e.g.
    /// "venkatyarl"). NULL for existing nodes still on Taylor's PAT. V12.
    #[serde(default)]
    pub gh_account: Option<String>,
    /// Map of installed-tool versions:
    ///   {"os":{"current":"Ubuntu 24.04.4","latest":"Ubuntu 24.04.5","checked_at":"..."}}
    /// Populated every 6h by the daemon's version_check tick. V12.
    #[serde(default = "default_tooling")]
    pub tooling: JsonValue,

    // ─── computers: physical hardware (discriminator fields) ───────────────
    // fleet_workers carries the worker *role*; physical hardware (GPU vendor,
    // VRAM, true RAM) lives on `computers`. These are LEFT-JOINed in so a
    // single `ff nodes` / `fleet_nodes_db` call can answer "which boxes are
    // AMD/NVIDIA/Apple and how much VRAM" without SSH-probing. None when the
    // worker has no matching computers row.
    #[serde(default)]
    pub gpu_kind: Option<String>,
    #[serde(default)]
    pub gpu_model: Option<String>,
    #[serde(default)]
    pub gpu_vram_gb: Option<f64>,
    /// Total GPU VRAM (GB). For unified-memory boxes (Apple Silicon, GB10
    /// Grace+Blackwell) per-GPU `gpu_vram_gb` is NULL by design, so this is
    /// the correct source for "how much VRAM"; prefer it when present.
    #[serde(default)]
    pub gpu_total_vram_gb: Option<f64>,
    #[serde(default)]
    pub has_gpu: Option<bool>,
    /// True RAM (GB) from the `computers` hardware row. `ram_gb` above is the
    /// often-stale worker-registry value; prefer this when present.
    #[serde(default)]
    pub computer_ram_gb: Option<i32>,
    /// True CPU cores from the `computers` hardware row; prefer over the
    /// often-stale `cpu_cores` worker-registry value when present.
    #[serde(default)]
    pub computer_cpu_cores: Option<i32>,
    /// Lifecycle status from the physical `computers` registry. This is kept
    /// separate from `fleet_workers.status`, whose heartbeat can be stale.
    #[serde(default)]
    pub computer_status: Option<String>,
}
