use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use ff_core::config::FleetConfig;
use ff_discovery::ScannerConfig;
use serde::{Deserialize, Serialize};

use crate::errors::{ControlError, Result};

/// Ordered control-plane subsystems for bootstrap sequencing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupSubsystem {
    Config,
    Discovery,
    Runtime,
    Orchestrator,
    Scheduler,
    Deploy,
}

impl StartupSubsystem {
    pub fn default_order() -> Vec<Self> {
        vec![
            Self::Config,
            Self::Discovery,
            Self::Runtime,
            Self::Orchestrator,
            Self::Scheduler,
            Self::Deploy,
        ]
    }
}

/// Bootstrap-time options for constructing a control plane.
#[derive(Debug, Clone)]
pub struct BootstrapOptions {
    pub startup_order: Vec<StartupSubsystem>,
    pub require_nodes: bool,
    pub require_models: bool,
    pub scheduler_poll_interval: Duration,
    pub scanner_config: ScannerConfig,
}

impl Default for BootstrapOptions {
    fn default() -> Self {
        Self {
            startup_order: StartupSubsystem::default_order(),
            require_nodes: true,
            require_models: false,
            scheduler_poll_interval: Duration::from_secs(5),
            scanner_config: ScannerConfig::default(),
        }
    }
}

/// Validation details gathered during bootstrap planning.
#[derive(Debug, Clone, Default)]
pub struct BootstrapValidation {
    pub warnings: Vec<String>,
}

/// Final bootstrap plan used by the control plane.
#[derive(Debug, Clone)]
pub struct BootstrapPlan {
    pub order: Vec<StartupSubsystem>,
    pub validation: BootstrapValidation,
    pub planned_at: DateTime<Utc>,
}

/// Validate startup sequencing constraints.
pub fn validate_startup_order(order: &[StartupSubsystem]) -> Result<()> {
    if order.is_empty() {
        return Err(ControlError::InvalidStartupOrder(
            "startup order cannot be empty".to_string(),
        ));
    }

    if order.first().copied() != Some(StartupSubsystem::Config) {
        return Err(ControlError::InvalidStartupOrder(
            "config must be first in startup order".to_string(),
        ));
    }

    let mut seen = HashSet::new();
    for step in order {
        if !seen.insert(*step) {
            return Err(ControlError::InvalidStartupOrder(format!(
                "duplicate startup step: {step:?}"
            )));
        }
    }

    for required in StartupSubsystem::default_order() {
        if !seen.contains(&required) {
            return Err(ControlError::InvalidStartupOrder(format!(
                "missing required startup step: {required:?}"
            )));
        }
    }

    Ok(())
}

/// Validate fleet config for control-plane bootstrap compatibility.
pub fn validate_fleet_config(
    config: &FleetConfig,
    require_nodes: bool,
    require_models: bool,
) -> Result<BootstrapValidation> {
    let mut validation = BootstrapValidation::default();

    if require_nodes && config.nodes.is_empty() {
        return Err(ControlError::BootstrapValidation(
            "fleet config has no nodes".to_string(),
        ));
    }

    if require_models && config.models.is_empty() {
        return Err(ControlError::BootstrapValidation(
            "fleet config has no models".to_string(),
        ));
    }

    let mut node_names = HashSet::new();
    for (name, node) in &config.nodes {
        if name.trim().is_empty() {
            return Err(ControlError::BootstrapValidation(
                "node name cannot be empty".to_string(),
            ));
        }
        if node.ip.trim().is_empty() {
            return Err(ControlError::BootstrapValidation(format!(
                "node '{}' has empty host/ip",
                name
            )));
        }
        if !node_names.insert(name.clone()) {
            return Err(ControlError::BootstrapValidation(format!(
                "duplicate node name '{}'",
                name
            )));
        }
    }

    if !config.leader.preferred.is_empty() && !node_names.contains(&config.leader.preferred) {
        validation.warnings.push(format!(
            "preferred leader '{}' is not in nodes list",
            config.leader.preferred
        ));
    }

    for model in &config.models {
        if model.id.trim().is_empty() {
            return Err(ControlError::BootstrapValidation(
                "model id cannot be empty".to_string(),
            ));
        }

        if model.nodes.is_empty() {
            validation
                .warnings
                .push(format!("model '{}' has no bound nodes", model.id));
        }

        for node_name in &model.nodes {
            if !node_names.contains(node_name) {
                return Err(ControlError::BootstrapValidation(format!(
                    "model '{}' references unknown node '{}'",
                    model.id, node_name
                )));
            }
        }
    }

    Ok(validation)
}

/// Build and validate a startup plan from options + config.
pub fn build_bootstrap_plan(
    config: &FleetConfig,
    options: &BootstrapOptions,
) -> Result<BootstrapPlan> {
    validate_startup_order(&options.startup_order)?;
    let validation = validate_fleet_config(config, options.require_nodes, options.require_models)?;

    Ok(BootstrapPlan {
        order: options.startup_order.clone(),
        validation,
        planned_at: Utc::now(),
    })
}

#[cfg(test)]
mod tests {
    use ff_core::config::{FleetSettings, LeaderConfig, ModelConfig, NodeConfig};
    use ff_core::{Role, Runtime, Tier};

    use super::*;

    fn sample_config() -> FleetConfig {
        FleetConfig {
            fleet: FleetSettings::default(),
            nodes: [(
                "taylor".to_string(),
                NodeConfig {
                    ip: "192.168.5.100".to_string(),
                    role: Role::Gateway,
                    election_priority: Some(1),
                    ram_gb: Some(128),
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
            models: vec![ModelConfig {
                id: "qwen3-32b".to_string(),
                name: "Qwen3 32B".to_string(),
                tier: Tier::Tier2,
                params_b: 32.0,
                quant: "Q4_K_M".to_string(),
                path: "/models/qwen3-32b.gguf".to_string(),
                ctx_size: 32768,
                runtime: Some(Runtime::LlamaCpp),
                nodes: vec!["taylor".to_string()],
            }],
            leader: LeaderConfig::default(),
            ..Default::default()
        }
    }

    #[test]
    fn startup_order_requires_config_first() {
        let err = validate_startup_order(&[
            StartupSubsystem::Runtime,
            StartupSubsystem::Config,
            StartupSubsystem::Discovery,
            StartupSubsystem::Orchestrator,
            StartupSubsystem::Scheduler,
            StartupSubsystem::Deploy,
        ])
        .unwrap_err();

        assert!(matches!(err, ControlError::InvalidStartupOrder(_)));
    }

    #[test]
    fn startup_order_rejects_duplicates() {
        let err = validate_startup_order(&[
            StartupSubsystem::Config,
            StartupSubsystem::Discovery,
            StartupSubsystem::Runtime,
            StartupSubsystem::Orchestrator,
            StartupSubsystem::Scheduler,
            StartupSubsystem::Scheduler,
            StartupSubsystem::Deploy,
        ])
        .unwrap_err();

        assert!(matches!(err, ControlError::InvalidStartupOrder(_)));
    }

    #[test]
    fn startup_order_rejects_missing_required_step() {
        let err = validate_startup_order(&[
            StartupSubsystem::Config,
            StartupSubsystem::Discovery,
            StartupSubsystem::Runtime,
            StartupSubsystem::Orchestrator,
            StartupSubsystem::Scheduler,
        ])
        .unwrap_err();

        assert!(matches!(err, ControlError::InvalidStartupOrder(_)));
    }

    #[test]
    fn build_plan_validates_config_and_order() {
        let cfg = sample_config();
        let options = BootstrapOptions::default();

        let plan = build_bootstrap_plan(&cfg, &options).unwrap();
        assert_eq!(plan.order, StartupSubsystem::default_order());
    }
}
