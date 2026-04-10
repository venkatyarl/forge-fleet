use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use ff_core::config::{FleetConfig, ModelConfig, NodeConfig};
use ff_core::{
    GpuType, Hardware, Interconnect, MemoryType, Model, Node, NodeStatus, OsType,
    Runtime as FleetRuntime,
};
use ff_cron::{CronEngine, Dispatcher, SchedulingPolicy};
use ff_discovery::{HealthMonitor, NodeRegistry, ScannerConfig};
use ff_orchestrator::TaskRouter;
use ff_runtime::{EngineConfig, EngineStatus};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::bootstrap::{BootstrapOptions, BootstrapPlan, StartupSubsystem, build_bootstrap_plan};
use crate::errors::{ControlError, Result};

/// Discovery subsystem handle bundle.
#[derive(Debug, Clone)]
pub struct DiscoverySubsystemHandle {
    pub registry: Arc<NodeRegistry>,
    pub scanner_config: ScannerConfig,
    pub health_monitor: HealthMonitor,
    pub last_scan_at: Option<DateTime<Utc>>,
}

/// Runtime subsystem handle bundle.
#[derive(Debug, Clone)]
pub struct RuntimeSubsystemHandle {
    pub desired_engine: EngineConfig,
    pub last_status: Option<EngineStatus>,
}

/// Orchestrator subsystem handle bundle.
#[derive(Clone)]
pub struct OrchestratorSubsystemHandle {
    pub router: Arc<RwLock<TaskRouter>>,
}

/// Scheduler subsystem handle bundle.
#[derive(Clone)]
pub struct SchedulerSubsystemHandle {
    pub engine: Arc<CronEngine>,
}

/// Deploy subsystem handle bundle.
#[derive(Debug, Clone)]
pub struct DeploySubsystemHandle {
    pub deploy_version: &'static str,
}

impl Default for DeploySubsystemHandle {
    fn default() -> Self {
        Self {
            deploy_version: ff_deploy::VERSION,
        }
    }
}

/// All control-plane subsystem handles.
#[derive(Clone)]
pub struct ControlPlaneHandles {
    pub discovery: DiscoverySubsystemHandle,
    pub runtime: RuntimeSubsystemHandle,
    pub orchestrator: OrchestratorSubsystemHandle,
    pub scheduler: SchedulerSubsystemHandle,
    pub deploy: DeploySubsystemHandle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupStepStatus {
    Ready,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartupEvent {
    pub subsystem: StartupSubsystem,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub status: StartupStepStatus,
}

/// Main control-plane facade.
#[derive(Clone)]
pub struct ControlPlane {
    pub config: Arc<FleetConfig>,
    pub handles: ControlPlaneHandles,
    pub startup_plan: BootstrapPlan,
    pub startup_events: Vec<StartupEvent>,
}

impl ControlPlane {
    /// Bootstrap a compile-safe control-plane facade from config + options.
    pub fn bootstrap(config: FleetConfig, options: BootstrapOptions) -> Result<Self> {
        let startup_plan = build_bootstrap_plan(&config, &options)?;

        let config = Arc::new(config);

        let mut discovery: Option<DiscoverySubsystemHandle> = None;
        let mut runtime: Option<RuntimeSubsystemHandle> = None;
        let mut orchestrator: Option<OrchestratorSubsystemHandle> = None;
        let mut scheduler: Option<SchedulerSubsystemHandle> = None;
        let mut deploy: Option<DeploySubsystemHandle> = None;

        let mut startup_events = Vec::with_capacity(startup_plan.order.len());

        for step in &startup_plan.order {
            let started_at = Utc::now();

            match step {
                StartupSubsystem::Config => {
                    // Validation already happened in build_bootstrap_plan.
                }
                StartupSubsystem::Discovery => {
                    discovery = Some(DiscoverySubsystemHandle {
                        registry: Arc::new(NodeRegistry::new()),
                        scanner_config: options.scanner_config.clone(),
                        health_monitor: HealthMonitor::default(),
                        last_scan_at: None,
                    });
                }
                StartupSubsystem::Runtime => {
                    runtime = Some(RuntimeSubsystemHandle {
                        desired_engine: desired_engine_from_config(&config),
                        last_status: None,
                    });
                }
                StartupSubsystem::Orchestrator => {
                    let nodes = config
                        .nodes
                        .iter()
                        .map(|(name, node)| node_from_config(name, node))
                        .collect::<Vec<_>>();
                    let models = config
                        .models
                        .iter()
                        .map(model_from_config)
                        .collect::<Vec<_>>();
                    let router = TaskRouter::new(nodes, models, HashMap::new());
                    orchestrator = Some(OrchestratorSubsystemHandle {
                        router: Arc::new(RwLock::new(router)),
                    });
                }
                StartupSubsystem::Scheduler => {
                    let dispatcher = Arc::new(Dispatcher::new());
                    let engine = CronEngine::new(dispatcher, None, SchedulingPolicy::default())
                        .with_poll_interval(options.scheduler_poll_interval);
                    scheduler = Some(SchedulerSubsystemHandle {
                        engine: Arc::new(engine),
                    });
                }
                StartupSubsystem::Deploy => {
                    deploy = Some(DeploySubsystemHandle::default());
                }
            }

            startup_events.push(StartupEvent {
                subsystem: *step,
                started_at,
                completed_at: Utc::now(),
                status: StartupStepStatus::Ready,
            });
        }

        let handles = ControlPlaneHandles {
            discovery: discovery.ok_or(ControlError::MissingSubsystem("discovery"))?,
            runtime: runtime.ok_or(ControlError::MissingSubsystem("runtime"))?,
            orchestrator: orchestrator.ok_or(ControlError::MissingSubsystem("orchestrator"))?,
            scheduler: scheduler.ok_or(ControlError::MissingSubsystem("scheduler"))?,
            deploy: deploy.ok_or(ControlError::MissingSubsystem("deploy"))?,
        };

        Ok(Self {
            config,
            handles,
            startup_plan,
            startup_events,
        })
    }

    pub fn startup_order(&self) -> &[StartupSubsystem] {
        &self.startup_plan.order
    }

    pub fn startup_events(&self) -> &[StartupEvent] {
        &self.startup_events
    }

    pub fn startup_warnings(&self) -> &[String] {
        &self.startup_plan.validation.warnings
    }
}

fn desired_engine_from_config(config: &FleetConfig) -> EngineConfig {
    let mut desired = EngineConfig::default();

    if let Some(model) = config.models.first() {
        desired.model_id = model.id.clone();
        desired.model_path = PathBuf::from(model.path.clone());
        desired.ctx_size = model.ctx_size;
    }

    if let Some((_name, node)) = config.nodes.iter().next() {
        desired.host = node.ip.clone();
        desired.port = node.port.unwrap_or(config.fleet.api_port);
    } else {
        desired.port = config.fleet.api_port;
    }

    desired
}

fn node_from_config(name: &str, node: &NodeConfig) -> Node {
    let now = Utc::now();
    let os_str = node.os.as_deref().unwrap_or("");
    let os = if os_str.to_lowercase().contains("mac") {
        OsType::MacOs
    } else if os_str.to_lowercase().contains("windows") {
        OsType::Windows
    } else {
        OsType::Linux
    };

    Node {
        id: Uuid::new_v4(),
        name: name.to_string(),
        host: node.ip.clone(),
        port: node.port.unwrap_or(55000),
        role: node.role,
        election_priority: node.priority(),
        status: NodeStatus::Online,
        hardware: Hardware {
            os,
            cpu_model: "unknown".to_string(),
            cpu_cores: node.effective_cpu_cores().unwrap_or(1),
            gpu: match os {
                OsType::MacOs => GpuType::AppleSilicon,
                _ => GpuType::None,
            },
            gpu_model: None,
            memory_gib: node.effective_ram_gb().unwrap_or(16),
            memory_type: MemoryType::Unknown,
            interconnect: Interconnect::Unknown,
            runtimes: vec![FleetRuntime::LlamaCpp],
        },
        models: node.models.keys().cloned().collect(),
        last_heartbeat: None,
        registered_at: now,
    }
}

fn model_from_config(model: &ModelConfig) -> Model {
    Model {
        id: model.id.clone(),
        name: model.name.clone(),
        tier: model.tier,
        params_b: model.params_b,
        quant: model.quant.clone(),
        path: model.path.clone(),
        ctx_size: model.ctx_size,
        runtime: model.runtime.unwrap_or(FleetRuntime::LlamaCpp),
        nodes: model.nodes.clone(),
    }
}
