//! Chaos testing helpers for ForgeFleet.
//!
//! Provides controlled failure injection for testing fleet resilience:
//!
//! - [`SimulateNodeFailure`] — mark a node as offline temporarily
//! - [`SimulateModelCrash`] — stop responding to health checks
//! - [`SimulateLeaderFailover`] — force the leader to yield
//! - [`SimulateNetworkPartition`] — block communication between two nodes
//!
//! # Safety
//!
//! All simulations:
//! - Are time-limited and auto-recover after the duration expires
//! - Require an explicit `--enable-chaos` flag (controlled by [`ChaosConfig`])
//! - Should **never** run in production by default
//! - Track active simulations for cleanup on shutdown

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::error::{ForgeFleetError, Result};

// ─── Configuration ───────────────────────────────────────────────────────────

/// Chaos testing configuration. Must be explicitly enabled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChaosConfig {
    /// Master switch — chaos features are disabled unless this is `true`.
    /// Map to `--enable-chaos` CLI flag.
    #[serde(default)]
    pub enabled: bool,

    /// Maximum duration for any single simulation (safety cap).
    #[serde(default = "default_max_duration_secs")]
    pub max_duration_secs: u64,

    /// Maximum concurrent simulations allowed.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,

    /// Disallow chaos in these environments.
    #[serde(default = "default_blocked_envs")]
    pub blocked_environments: Vec<String>,
}

fn default_max_duration_secs() -> u64 {
    300 // 5 minutes
}

fn default_max_concurrent() -> usize {
    3
}

fn default_blocked_envs() -> Vec<String> {
    vec!["production".into(), "prod".into()]
}

impl Default for ChaosConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_duration_secs: default_max_duration_secs(),
            max_concurrent: default_max_concurrent(),
            blocked_environments: default_blocked_envs(),
        }
    }
}

impl ChaosConfig {
    /// Validate that chaos is allowed given the current environment.
    pub fn validate(&self, environment: &str) -> Result<()> {
        if !self.enabled {
            return Err(ForgeFleetError::Runtime(
                "chaos testing is disabled — pass --enable-chaos to enable".into(),
            ));
        }

        let env_lower = environment.to_lowercase();
        if self
            .blocked_environments
            .iter()
            .any(|e| e.to_lowercase() == env_lower)
        {
            return Err(ForgeFleetError::Runtime(format!(
                "chaos testing is blocked in environment '{environment}'"
            )));
        }

        Ok(())
    }

    /// Clamp a requested duration to the safety maximum.
    pub fn clamp_duration(&self, requested: Duration) -> Duration {
        let max = Duration::from_secs(self.max_duration_secs);
        if requested > max {
            warn!(
                requested_secs = requested.as_secs(),
                max_secs = max.as_secs(),
                "Clamping chaos duration to maximum"
            );
            max
        } else {
            requested
        }
    }
}

// ─── Simulation Types ────────────────────────────────────────────────────────

/// Unique ID for a running simulation.
pub type SimulationId = String;

/// The type of chaos simulation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SimulationType {
    /// Temporarily mark a node as offline.
    NodeFailure { node: String },
    /// Stop responding to health checks for a model.
    ModelCrash { node: String, model: String },
    /// Force the current leader to yield.
    LeaderFailover,
    /// Block communication between two nodes.
    NetworkPartition { node_a: String, node_b: String },
}

impl std::fmt::Display for SimulationType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NodeFailure { node } => write!(f, "node_failure({node})"),
            Self::ModelCrash { node, model } => write!(f, "model_crash({node}/{model})"),
            Self::LeaderFailover => write!(f, "leader_failover"),
            Self::NetworkPartition { node_a, node_b } => {
                write!(f, "network_partition({node_a} <-> {node_b})")
            }
        }
    }
}

/// A running or completed chaos simulation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Simulation {
    /// Unique simulation ID.
    pub id: SimulationId,
    /// What type of chaos.
    pub simulation_type: SimulationType,
    /// When the simulation started.
    pub started_at: DateTime<Utc>,
    /// How long it should run.
    pub duration: Duration,
    /// When it should auto-recover.
    pub expires_at: DateTime<Utc>,
    /// Current state.
    pub state: SimulationState,
    /// Optional description.
    pub description: Option<String>,
}

/// State of a simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimulationState {
    /// Simulation is actively injecting failure.
    Active,
    /// Simulation has auto-recovered.
    Recovered,
    /// Simulation was manually cancelled.
    Cancelled,
    /// Simulation failed to inject or recover properly.
    Error { reason: String },
}

// ─── Chaos Engine ────────────────────────────────────────────────────────────

/// The chaos testing engine. Manages active simulations and auto-recovery.
pub struct ChaosEngine {
    config: ChaosConfig,
    environment: String,
    simulations: Arc<Mutex<HashMap<SimulationId, Simulation>>>,
    /// Callback registry for applying/reverting simulations.
    /// In practice, these hooks are provided by the fleet runtime.
    hooks: Arc<Mutex<ChaosHooks>>,
}

/// Callbacks for applying and reverting chaos simulations.
/// The fleet runtime wires these up to actual node/model/leader state.
pub struct ChaosHooks {
    /// Called to mark a node offline. Returns Ok(()) if applied.
    pub on_node_failure: Option<Box<dyn Fn(&str) -> Result<()> + Send + Sync>>,
    /// Called to restore a node. Returns Ok(()) if reverted.
    pub on_node_recover: Option<Box<dyn Fn(&str) -> Result<()> + Send + Sync>>,
    /// Called to crash a model endpoint. Returns Ok(()) if applied.
    pub on_model_crash: Option<Box<dyn Fn(&str, &str) -> Result<()> + Send + Sync>>,
    /// Called to restore a model endpoint. Returns Ok(()) if reverted.
    pub on_model_recover: Option<Box<dyn Fn(&str, &str) -> Result<()> + Send + Sync>>,
    /// Called to force leader to yield. Returns Ok(()) if applied.
    pub on_leader_yield: Option<Box<dyn Fn() -> Result<()> + Send + Sync>>,
    /// Called to resume normal leader election. Returns Ok(()) if reverted.
    pub on_leader_resume: Option<Box<dyn Fn() -> Result<()> + Send + Sync>>,
    /// Called to partition two nodes. Returns Ok(()) if applied.
    pub on_partition: Option<Box<dyn Fn(&str, &str) -> Result<()> + Send + Sync>>,
    /// Called to heal a partition. Returns Ok(()) if reverted.
    pub on_heal_partition: Option<Box<dyn Fn(&str, &str) -> Result<()> + Send + Sync>>,
}

impl Default for ChaosHooks {
    fn default() -> Self {
        Self {
            on_node_failure: None,
            on_node_recover: None,
            on_model_crash: None,
            on_model_recover: None,
            on_leader_yield: None,
            on_leader_resume: None,
            on_partition: None,
            on_heal_partition: None,
        }
    }
}

impl ChaosEngine {
    /// Create a new chaos engine.
    pub fn new(config: ChaosConfig, environment: impl Into<String>) -> Self {
        Self {
            config,
            environment: environment.into(),
            simulations: Arc::new(Mutex::new(HashMap::new())),
            hooks: Arc::new(Mutex::new(ChaosHooks::default())),
        }
    }

    /// Set the chaos hooks (wired by the fleet runtime).
    pub async fn set_hooks(&self, hooks: ChaosHooks) {
        *self.hooks.lock().await = hooks;
    }

    /// Start a chaos simulation.
    pub async fn start_simulation(
        &self,
        sim_type: SimulationType,
        duration: Duration,
        description: Option<String>,
    ) -> Result<Simulation> {
        // Validate chaos is allowed
        self.config.validate(&self.environment)?;

        // Clamp duration
        let duration = self.config.clamp_duration(duration);

        // Check concurrent limit
        let active_count = {
            let sims = self.simulations.lock().await;
            sims.values()
                .filter(|s| s.state == SimulationState::Active)
                .count()
        };

        if active_count >= self.config.max_concurrent {
            return Err(ForgeFleetError::Runtime(format!(
                "max concurrent simulations ({}) reached",
                self.config.max_concurrent
            )));
        }

        // Check for conflicting simulations
        {
            let sims = self.simulations.lock().await;
            for existing in sims.values() {
                if existing.state == SimulationState::Active && existing.simulation_type == sim_type
                {
                    return Err(ForgeFleetError::Runtime(format!(
                        "simulation '{}' is already active (id: {})",
                        sim_type, existing.id
                    )));
                }
            }
        }

        let id = format!(
            "chaos-{}-{}",
            sim_type
                .to_string()
                .replace(['(', ')', '/', ' ', '<', '>', '-'], "_"),
            uuid::Uuid::new_v4().as_simple(),
        );

        let now = Utc::now();
        let expires_at =
            now + chrono::Duration::from_std(duration).unwrap_or(chrono::Duration::minutes(5));

        // Apply the simulation via hooks
        let state = match self.apply_simulation(&sim_type).await {
            Ok(()) => SimulationState::Active,
            Err(e) => SimulationState::Error {
                reason: e.to_string(),
            },
        };

        let simulation = Simulation {
            id: id.clone(),
            simulation_type: sim_type.clone(),
            started_at: now,
            duration,
            expires_at,
            state: state.clone(),
            description,
        };

        self.simulations
            .lock()
            .await
            .insert(id.clone(), simulation.clone());

        if state == SimulationState::Active {
            info!(
                id = %id,
                sim_type = %sim_type,
                duration_secs = duration.as_secs(),
                "Chaos simulation started"
            );

            // Spawn auto-recovery task
            let engine_sims = self.simulations.clone();
            let engine_hooks = self.hooks.clone();
            let sim_id = id.clone();
            let sim_type_clone = sim_type;
            tokio::spawn(async move {
                tokio::time::sleep(duration).await;
                Self::auto_recover(engine_sims, engine_hooks, &sim_id, &sim_type_clone).await;
            });
        }

        Ok(simulation)
    }

    /// Cancel an active simulation early.
    pub async fn cancel_simulation(&self, id: &str) -> Result<Simulation> {
        let mut sims = self.simulations.lock().await;
        let sim = sims
            .get_mut(id)
            .ok_or_else(|| ForgeFleetError::Runtime(format!("simulation '{id}' not found")))?;

        if sim.state != SimulationState::Active {
            return Err(ForgeFleetError::Runtime(format!(
                "simulation '{id}' is not active (state: {:?})",
                sim.state
            )));
        }

        // Revert the simulation
        if let Err(e) = self.revert_simulation(&sim.simulation_type).await {
            warn!(id = %id, error = %e, "Failed to revert simulation on cancel");
        }

        sim.state = SimulationState::Cancelled;
        info!(id = %id, "Chaos simulation cancelled");

        Ok(sim.clone())
    }

    /// Cancel all active simulations (e.g. on shutdown).
    pub async fn cancel_all(&self) -> Vec<SimulationId> {
        let mut cancelled = Vec::new();
        let active_ids: Vec<String> = {
            let sims = self.simulations.lock().await;
            sims.iter()
                .filter(|(_, s)| s.state == SimulationState::Active)
                .map(|(id, _)| id.clone())
                .collect()
        };

        for id in active_ids {
            if self.cancel_simulation(&id).await.is_ok() {
                cancelled.push(id);
            }
        }

        if !cancelled.is_empty() {
            info!(count = cancelled.len(), "Cancelled all chaos simulations");
        }

        cancelled
    }

    /// List all simulations (active and completed).
    pub async fn list_simulations(&self) -> Vec<Simulation> {
        self.simulations.lock().await.values().cloned().collect()
    }

    /// List only active simulations.
    pub async fn active_simulations(&self) -> Vec<Simulation> {
        self.simulations
            .lock()
            .await
            .values()
            .filter(|s| s.state == SimulationState::Active)
            .cloned()
            .collect()
    }

    /// Get a specific simulation by ID.
    pub async fn get_simulation(&self, id: &str) -> Option<Simulation> {
        self.simulations.lock().await.get(id).cloned()
    }

    /// Apply a simulation using the registered hooks.
    async fn apply_simulation(&self, sim_type: &SimulationType) -> Result<()> {
        let hooks = self.hooks.lock().await;
        match sim_type {
            SimulationType::NodeFailure { node } => {
                if let Some(hook) = &hooks.on_node_failure {
                    hook(node)?;
                }
            }
            SimulationType::ModelCrash { node, model } => {
                if let Some(hook) = &hooks.on_model_crash {
                    hook(node, model)?;
                }
            }
            SimulationType::LeaderFailover => {
                if let Some(hook) = &hooks.on_leader_yield {
                    hook()?;
                }
            }
            SimulationType::NetworkPartition { node_a, node_b } => {
                if let Some(hook) = &hooks.on_partition {
                    hook(node_a, node_b)?;
                }
            }
        }
        Ok(())
    }

    /// Revert a simulation using the registered hooks.
    async fn revert_simulation(&self, sim_type: &SimulationType) -> Result<()> {
        let hooks = self.hooks.lock().await;
        match sim_type {
            SimulationType::NodeFailure { node } => {
                if let Some(hook) = &hooks.on_node_recover {
                    hook(node)?;
                }
            }
            SimulationType::ModelCrash { node, model } => {
                if let Some(hook) = &hooks.on_model_recover {
                    hook(node, model)?;
                }
            }
            SimulationType::LeaderFailover => {
                if let Some(hook) = &hooks.on_leader_resume {
                    hook()?;
                }
            }
            SimulationType::NetworkPartition { node_a, node_b } => {
                if let Some(hook) = &hooks.on_heal_partition {
                    hook(node_a, node_b)?;
                }
            }
        }
        Ok(())
    }

    /// Auto-recovery after duration expires.
    async fn auto_recover(
        simulations: Arc<Mutex<HashMap<SimulationId, Simulation>>>,
        hooks: Arc<Mutex<ChaosHooks>>,
        sim_id: &str,
        sim_type: &SimulationType,
    ) {
        // Revert via hooks
        let hooks_guard = hooks.lock().await;
        let revert_result = match sim_type {
            SimulationType::NodeFailure { node } => hooks_guard
                .on_node_recover
                .as_ref()
                .map(|h| h(node))
                .unwrap_or(Ok(())),
            SimulationType::ModelCrash { node, model } => hooks_guard
                .on_model_recover
                .as_ref()
                .map(|h| h(node, model))
                .unwrap_or(Ok(())),
            SimulationType::LeaderFailover => hooks_guard
                .on_leader_resume
                .as_ref()
                .map(|h| h())
                .unwrap_or(Ok(())),
            SimulationType::NetworkPartition { node_a, node_b } => hooks_guard
                .on_heal_partition
                .as_ref()
                .map(|h| h(node_a, node_b))
                .unwrap_or(Ok(())),
        };
        drop(hooks_guard);

        // Update simulation state
        let mut sims = simulations.lock().await;
        if let Some(sim) = sims.get_mut(sim_id) {
            if sim.state == SimulationState::Active {
                match revert_result {
                    Ok(()) => {
                        sim.state = SimulationState::Recovered;
                        info!(id = %sim_id, "Chaos simulation auto-recovered");
                    }
                    Err(e) => {
                        sim.state = SimulationState::Error {
                            reason: format!("recovery failed: {e}"),
                        };
                        warn!(id = %sim_id, error = %e, "Chaos simulation recovery failed");
                    }
                }
            }
        }
    }
}

// ─── Convenience constructors for simulations ────────────────────────────────

/// Create a node failure simulation.
pub fn simulate_node_failure(
    node: impl Into<String>,
    duration: Duration,
) -> (SimulationType, Duration) {
    (SimulationType::NodeFailure { node: node.into() }, duration)
}

/// Create a model crash simulation.
pub fn simulate_model_crash(
    node: impl Into<String>,
    model: impl Into<String>,
    duration: Duration,
) -> (SimulationType, Duration) {
    (
        SimulationType::ModelCrash {
            node: node.into(),
            model: model.into(),
        },
        duration,
    )
}

/// Create a leader failover simulation.
pub fn simulate_leader_failover(duration: Duration) -> (SimulationType, Duration) {
    (SimulationType::LeaderFailover, duration)
}

/// Create a network partition simulation.
pub fn simulate_network_partition(
    node_a: impl Into<String>,
    node_b: impl Into<String>,
    duration: Duration,
) -> (SimulationType, Duration) {
    (
        SimulationType::NetworkPartition {
            node_a: node_a.into(),
            node_b: node_b.into(),
        },
        duration,
    )
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chaos_config_defaults() {
        let config = ChaosConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.max_duration_secs, 300);
        assert_eq!(config.max_concurrent, 3);
        assert!(config.blocked_environments.contains(&"production".into()));
        assert!(config.blocked_environments.contains(&"prod".into()));
    }

    #[test]
    fn test_chaos_config_validate_disabled() {
        let config = ChaosConfig::default();
        let result = config.validate("dev");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("--enable-chaos"));
    }

    #[test]
    fn test_chaos_config_validate_blocked_env() {
        let config = ChaosConfig {
            enabled: true,
            ..Default::default()
        };
        let result = config.validate("production");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("blocked"));
    }

    #[test]
    fn test_chaos_config_validate_ok() {
        let config = ChaosConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(config.validate("dev").is_ok());
        assert!(config.validate("staging").is_ok());
        assert!(config.validate("test").is_ok());
    }

    #[test]
    fn test_clamp_duration() {
        let config = ChaosConfig {
            enabled: true,
            max_duration_secs: 60,
            ..Default::default()
        };
        // Under limit — no clamping
        assert_eq!(
            config.clamp_duration(Duration::from_secs(30)),
            Duration::from_secs(30)
        );
        // Over limit — clamped
        assert_eq!(
            config.clamp_duration(Duration::from_secs(120)),
            Duration::from_secs(60)
        );
        // Exact limit — no clamping
        assert_eq!(
            config.clamp_duration(Duration::from_secs(60)),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn test_simulation_type_display() {
        assert_eq!(
            format!(
                "{}",
                SimulationType::NodeFailure {
                    node: "james".into()
                }
            ),
            "node_failure(james)"
        );
        assert_eq!(
            format!(
                "{}",
                SimulationType::ModelCrash {
                    node: "james".into(),
                    model: "qwen-9b".into()
                }
            ),
            "model_crash(james/qwen-9b)"
        );
        assert_eq!(
            format!("{}", SimulationType::LeaderFailover),
            "leader_failover"
        );
        assert_eq!(
            format!(
                "{}",
                SimulationType::NetworkPartition {
                    node_a: "taylor".into(),
                    node_b: "james".into()
                }
            ),
            "network_partition(taylor <-> james)"
        );
    }

    #[tokio::test]
    async fn test_engine_disabled_rejects() {
        let engine = ChaosEngine::new(ChaosConfig::default(), "dev");
        let result = engine
            .start_simulation(
                SimulationType::NodeFailure {
                    node: "james".into(),
                },
                Duration::from_secs(10),
                None,
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_engine_production_blocked() {
        let config = ChaosConfig {
            enabled: true,
            ..Default::default()
        };
        let engine = ChaosEngine::new(config, "production");
        let result = engine
            .start_simulation(
                SimulationType::NodeFailure {
                    node: "james".into(),
                },
                Duration::from_secs(10),
                None,
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_engine_start_and_list() {
        let config = ChaosConfig {
            enabled: true,
            blocked_environments: vec![],
            ..Default::default()
        };
        let engine = ChaosEngine::new(config, "test");

        let sim = engine
            .start_simulation(
                SimulationType::NodeFailure {
                    node: "james".into(),
                },
                Duration::from_secs(60),
                Some("test simulation".into()),
            )
            .await
            .unwrap();

        assert_eq!(sim.state, SimulationState::Active);
        assert!(sim.id.starts_with("chaos-"));

        let active = engine.active_simulations().await;
        assert_eq!(active.len(), 1);

        let all = engine.list_simulations().await;
        assert_eq!(all.len(), 1);
    }

    #[tokio::test]
    async fn test_engine_cancel_simulation() {
        let config = ChaosConfig {
            enabled: true,
            blocked_environments: vec![],
            ..Default::default()
        };
        let engine = ChaosEngine::new(config, "test");

        let sim = engine
            .start_simulation(
                SimulationType::LeaderFailover,
                Duration::from_secs(60),
                None,
            )
            .await
            .unwrap();

        let cancelled = engine.cancel_simulation(&sim.id).await.unwrap();
        assert_eq!(cancelled.state, SimulationState::Cancelled);

        let active = engine.active_simulations().await;
        assert!(active.is_empty());
    }

    #[tokio::test]
    async fn test_engine_duplicate_rejection() {
        let config = ChaosConfig {
            enabled: true,
            blocked_environments: vec![],
            ..Default::default()
        };
        let engine = ChaosEngine::new(config, "test");

        // First should succeed
        engine
            .start_simulation(
                SimulationType::NodeFailure {
                    node: "james".into(),
                },
                Duration::from_secs(60),
                None,
            )
            .await
            .unwrap();

        // Duplicate should fail
        let result = engine
            .start_simulation(
                SimulationType::NodeFailure {
                    node: "james".into(),
                },
                Duration::from_secs(30),
                None,
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already active"));
    }

    #[tokio::test]
    async fn test_engine_cancel_all() {
        let config = ChaosConfig {
            enabled: true,
            blocked_environments: vec![],
            max_concurrent: 5,
            ..Default::default()
        };
        let engine = ChaosEngine::new(config, "test");

        engine
            .start_simulation(
                SimulationType::NodeFailure {
                    node: "james".into(),
                },
                Duration::from_secs(60),
                None,
            )
            .await
            .unwrap();
        engine
            .start_simulation(
                SimulationType::NodeFailure {
                    node: "marcus".into(),
                },
                Duration::from_secs(60),
                None,
            )
            .await
            .unwrap();

        let cancelled = engine.cancel_all().await;
        assert_eq!(cancelled.len(), 2);
        assert!(engine.active_simulations().await.is_empty());
    }

    #[tokio::test]
    async fn test_engine_max_concurrent() {
        let config = ChaosConfig {
            enabled: true,
            blocked_environments: vec![],
            max_concurrent: 1,
            ..Default::default()
        };
        let engine = ChaosEngine::new(config, "test");

        engine
            .start_simulation(
                SimulationType::NodeFailure {
                    node: "james".into(),
                },
                Duration::from_secs(60),
                None,
            )
            .await
            .unwrap();

        let result = engine
            .start_simulation(
                SimulationType::LeaderFailover,
                Duration::from_secs(30),
                None,
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("max concurrent"));
    }

    #[test]
    fn test_convenience_constructors() {
        let (sim, dur) = simulate_node_failure("james", Duration::from_secs(30));
        assert!(matches!(sim, SimulationType::NodeFailure { node } if node == "james"));
        assert_eq!(dur, Duration::from_secs(30));

        let (sim, _) = simulate_model_crash("taylor", "qwen-32b", Duration::from_secs(60));
        assert!(matches!(
            sim,
            SimulationType::ModelCrash { node, model }
            if node == "taylor" && model == "qwen-32b"
        ));

        let (sim, _) = simulate_leader_failover(Duration::from_secs(120));
        assert!(matches!(sim, SimulationType::LeaderFailover));

        let (sim, _) = simulate_network_partition("taylor", "james", Duration::from_secs(45));
        assert!(matches!(
            sim,
            SimulationType::NetworkPartition { node_a, node_b }
            if node_a == "taylor" && node_b == "james"
        ));
    }

    #[test]
    fn test_simulation_state_equality() {
        assert_eq!(SimulationState::Active, SimulationState::Active);
        assert_eq!(SimulationState::Recovered, SimulationState::Recovered);
        assert_eq!(SimulationState::Cancelled, SimulationState::Cancelled);
        assert_ne!(SimulationState::Active, SimulationState::Recovered);
    }

    #[test]
    fn test_chaos_config_serialization() {
        let config = ChaosConfig {
            enabled: true,
            max_duration_secs: 120,
            max_concurrent: 2,
            blocked_environments: vec!["prod".into()],
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: ChaosConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.max_duration_secs, 120);
        assert_eq!(parsed.max_concurrent, 2);
    }
}
