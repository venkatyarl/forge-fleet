//! Core primitives shared across ForgeFleet crates.
//!
//! This crate is the foundation of ForgeFleet, providing:
//! - **types** — Node, Model, Hardware, Tier, Runtime, Role, etc.
//! - **config** — Load/parse fleet.toml with hot-reload and env overrides
//! - **error** — Unified error type (`ForgeFleetError`)
//! - **db** — Postgres connection pool setup
//! - **hardware** — Detect OS, CPU, GPU, memory, interconnect at runtime
//! - **leader** — Leader election types and failover logic
//! - **activity** — Taylor yield modes and activity level tracking
//! - **task** — Agent task and result types

pub mod activity;
pub mod artifact_cache;
pub mod artifact_cache_dir;
pub mod artifact_fetch;
pub mod audit;
pub mod build_version;
pub mod cache;
pub mod chaos;
pub mod ci_trigger;
pub mod circuit_breaker;
pub mod computer;
pub mod config;
pub mod db;
pub mod db_health;
pub mod error;
pub mod fleet_resolver;
pub mod hardware;
pub mod leader;
pub mod maintenance;
pub mod model_id;
pub mod monitor;
pub mod notifications;
pub mod obsidian_export;
pub mod panic_hook;
pub mod quarantine;
pub mod queue;
pub mod run_limits;
pub mod schema;
pub mod synthetic;
pub mod task;
pub mod task_error;
pub mod tool_path;
pub mod types;
pub mod url;
pub mod verifier;

// Re-export the most commonly used items at crate root.
pub use activity::{ActivitySignals, ActivityState, YieldMode};
pub use artifact_cache::{
    ArtifactEvictionPolicy, evaluate_artifact_eviction, spawn_artifact_eviction_loop,
};
pub use artifact_cache_dir::{
    artifact_cache_path, default_cache_root, detect_arch, detect_os_family,
    ensure_artifact_cache_path, ensure_platform_cache_dir, platform_cache_dir,
};
pub use artifact_fetch::{ArtifactCacheManager, FetchSource, LanPeer, default_artifact_cache_root};
pub use chaos::{
    ChaosConfig, ChaosEngine, ChaosHooks, Simulation, SimulationId, SimulationState, SimulationType,
};
pub use ci_trigger::CiPipelineTrigger;
pub use circuit_breaker::{
    BackendId, CircuitBreaker, CircuitBreakerConfig, CircuitBreakerRegistry,
    CircuitBreakerSnapshot, CircuitState,
};
pub use computer::{ActivityLevel, AgentRegistrationAck, WorkerRole};
pub use error::{ForgeFleetError, Result};
pub use fleet_resolver::{
    FleetNodeInfo, FleetResolveError, FleetResolver, resolve_fleet_nodes, resolve_fleet_nodes_sync,
};
pub use maintenance::{MaintenanceEntry, MaintenanceManager, MaintenancePhase, MaintenanceWindow};
pub use monitor::{
    AlertCondition, DiskMonitor, FleetMonitor, ModelMonitor, MonitorAlert, MonitorSettings,
    NodeMonitor,
};
pub use notifications::{NotificationLevel, NotificationSender, TelegramNotifier};
pub use quarantine::{NodeQuarantine, QuarantineEntry, QuarantinePolicy};
pub use run_limits::{RunLimits, should_escalate};
pub use synthetic::{
    BackupFreshnessProbe, DbWriteReadProbe, DiskSpaceProbe, HttpHealthProbe, LlmSmokeProbe,
    ProbeCategory, ProbeRegistry, ProbeResult, ProbeStatus, ReplicationLagProbe, SyntheticProbe,
};
pub use task::{AgentTask, AgentTaskKind, TaskResult, publish_task_notification};
pub use task_error::{TaskErrorClass, classify_task_error};
pub use types::*;
pub use verifier::{
    CategoryScore, HealthScorecard, HealthVerifier, ScorecardResponse, ScorecardSnapshot,
    VerifierConfig,
};

/// Crate version from Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Returns true if `s` is empty or contains only whitespace characters.
pub fn is_blank(s: &str) -> bool {
    s.is_empty() || s.chars().all(char::is_whitespace)
}

/// Truncates `s` in the middle so the returned string is at most `max` chars.
///
/// If `s` is already `max` chars or shorter it is returned unchanged.
/// Otherwise the head and tail of `s` are preserved and joined by a single
/// Unicode ellipsis (`…`). The split is character-based, so multi-byte input
/// never panics.
pub fn truncate_middle(s: &str, max: usize) -> String {
    let len = s.chars().count();
    if len <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let keep = max - 1;
    let head = keep / 2 + keep % 2;
    let tail = keep - head;
    let left: String = s.chars().take(head).collect();
    let right: String = s
        .chars()
        .rev()
        .take(tail)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{left}…{right}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_blank_empty() {
        assert!(is_blank(""));
    }

    #[test]
    fn test_is_blank_whitespace() {
        assert!(is_blank(" \t"));
    }

    #[test]
    fn test_is_blank_non_blank() {
        assert!(!is_blank("hello"));
    }

    #[test]
    fn test_truncate_middle_short_unchanged() {
        assert_eq!(truncate_middle("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_middle_long_ascii_exact_max() {
        let s = "abcdefghijklmnopqrstuvwxyz";
        let max = 10;
        let result = truncate_middle(s, max);
        assert_eq!(result.chars().count(), max);
        assert!(result.contains('…'));
    }

    #[test]
    fn test_truncate_middle_multibyte_no_panic() {
        let s = "🙂a🙂b🙂c🙂d🙂e🙂f🙂";
        let result = truncate_middle(s, 6);
        assert!(result.chars().count() <= 6);
        assert!(result.contains('…'));
    }
}
