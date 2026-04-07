use chrono::{DateTime, Utc};
use ff_core::{AgentRegistrationAck, AgentTask, AgentTaskKind, NodeRole};
use ff_cron::{JobDefinition, JobPriority, JobTask};
use ff_discovery::DiscoveredNode;
use uuid::Uuid;

use crate::control_plane::ControlPlane;
use crate::errors::{ControlError, Result};

#[derive(Debug, Clone)]
pub enum DiscoverMode {
    /// Upsert already discovered nodes (no network side effects).
    Upsert { nodes: Vec<DiscoveredNode> },
    /// Return the active scanner configuration without scanning.
    PlanOnly,
}

#[derive(Debug, Clone)]
pub struct DiscoverRequest {
    pub mode: DiscoverMode,
}

#[derive(Debug, Clone)]
pub struct DiscoverResult {
    pub upserted_ids: Vec<Uuid>,
    pub total_registry_nodes: usize,
}

#[derive(Debug, Clone)]
pub struct StartAgentRequest {
    pub node_name: String,
    pub role: NodeRole,
}

#[derive(Debug, Clone)]
pub struct StartAgentResult {
    pub node_name: String,
    pub accepted_at: DateTime<Utc>,
    pub registration: AgentRegistrationAck,
}

#[derive(Debug, Clone)]
pub struct RunTaskRequest {
    pub kind: AgentTaskKind,
    pub target_node: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RunTaskResult {
    pub task: AgentTask,
    pub target_node: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ScheduleRequest {
    pub name: String,
    pub cron_expression: String,
    pub task: JobTask,
    pub priority: JobPriority,
    /// If true, validates and builds the job but does not persist into scheduler engine.
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct ScheduleResult {
    pub job: JobDefinition,
    pub persisted: bool,
}

#[derive(Debug, Clone)]
pub enum DeployStrategy {
    Rolling,
    BlueGreen,
    Canary { percent: u8 },
}

#[derive(Debug, Clone)]
pub struct DeployRequest {
    pub release: String,
    pub targets: Vec<String>,
    pub strategy: DeployStrategy,
}

#[derive(Debug, Clone)]
pub struct DeployResult {
    pub accepted: bool,
    pub deployment_id: Uuid,
    pub release: String,
    pub targets: Vec<String>,
    pub strategy: DeployStrategy,
    pub deploy_version: &'static str,
}

#[derive(Debug, Clone)]
pub enum ControlCommand {
    Discover(DiscoverRequest),
    StartAgent(StartAgentRequest),
    RunTask(RunTaskRequest),
    Schedule(ScheduleRequest),
    Deploy(DeployRequest),
}

#[derive(Debug, Clone)]
pub enum ControlCommandResult {
    Discover(DiscoverResult),
    StartAgent(StartAgentResult),
    RunTask(RunTaskResult),
    Schedule(Box<ScheduleResult>),
    Deploy(DeployResult),
}

impl ControlPlane {
    pub async fn execute(&self, cmd: ControlCommand) -> Result<ControlCommandResult> {
        match cmd {
            ControlCommand::Discover(req) => self.discover(req).map(ControlCommandResult::Discover),
            ControlCommand::StartAgent(req) => {
                self.start_agent(req).map(ControlCommandResult::StartAgent)
            }
            ControlCommand::RunTask(req) => self.run_task(req).map(ControlCommandResult::RunTask),
            ControlCommand::Schedule(req) => self
                .schedule(req)
                .await
                .map(|result| ControlCommandResult::Schedule(Box::new(result))),
            ControlCommand::Deploy(req) => self.deploy(req).map(ControlCommandResult::Deploy),
        }
    }

    pub fn discover(&self, req: DiscoverRequest) -> Result<DiscoverResult> {
        let upserted_ids = match req.mode {
            DiscoverMode::Upsert { nodes } => self
                .handles
                .discovery
                .registry
                .upsert_many_discovered(nodes),
            DiscoverMode::PlanOnly => Vec::new(),
        };

        Ok(DiscoverResult {
            upserted_ids,
            total_registry_nodes: self.handles.discovery.registry.len(),
        })
    }

    pub fn start_agent(&self, req: StartAgentRequest) -> Result<StartAgentResult> {
        let exists = self.config.nodes.contains_key(&req.node_name);
        if !exists {
            return Err(ControlError::UnknownNode(req.node_name));
        }

        Ok(StartAgentResult {
            node_name: req.node_name,
            accepted_at: Utc::now(),
            registration: AgentRegistrationAck {
                accepted: true,
                role: req.role,
                heartbeat_interval_secs: self.config.fleet.heartbeat_interval_secs,
                reason: None,
            },
        })
    }

    pub fn run_task(&self, req: RunTaskRequest) -> Result<RunTaskResult> {
        if let Some(target_node) = &req.target_node
            && !self.config.nodes.contains_key(target_node)
        {
            return Err(ControlError::UnknownNode(target_node.clone()));
        }

        Ok(RunTaskResult {
            task: AgentTask {
                id: Uuid::new_v4(),
                created_at: Utc::now(),
                kind: req.kind,
            },
            target_node: req.target_node,
        })
    }

    pub async fn schedule(&self, req: ScheduleRequest) -> Result<ScheduleResult> {
        let job = JobDefinition::new(req.name, req.cron_expression, req.task, req.priority)?;

        if req.dry_run {
            return Ok(ScheduleResult {
                job,
                persisted: false,
            });
        }

        self.handles.scheduler.engine.add_job(job.clone()).await?;

        Ok(ScheduleResult {
            job,
            persisted: true,
        })
    }

    pub fn deploy(&self, req: DeployRequest) -> Result<DeployResult> {
        for target in &req.targets {
            if !self.config.nodes.contains_key(target) {
                return Err(ControlError::UnknownNode(target.clone()));
            }
        }

        if let DeployStrategy::Canary { percent } = &req.strategy
            && (*percent == 0 || *percent > 100)
        {
            return Err(ControlError::BootstrapValidation(
                "canary percentage must be between 1 and 100".to_string(),
            ));
        }

        Ok(DeployResult {
            accepted: true,
            deployment_id: Uuid::new_v4(),
            release: req.release,
            targets: req.targets,
            strategy: req.strategy,
            deploy_version: self.handles.deploy.deploy_version,
        })
    }
}
