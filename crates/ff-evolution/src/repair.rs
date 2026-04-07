//! Repair proposal and action tracking.
//!
//! Maps analyzed root causes to concrete repair strategies with confidence + risk.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::analyzer::{AnalysisReport, RootCause, RootCauseCategory};

/// Canonical repair strategy types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairStrategy {
    FixCompilation,
    PinDependency,
    PatchConfiguration,
    RetryWithBackoff,
    IncreaseResources,
    ContractAlignment,
    StabilizeTest,
    ToolchainReset,
    ManualInvestigation,
}

/// Risk band for an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairRisk {
    Low,
    Medium,
    High,
    Critical,
}

/// Lifecycle status of a repair action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairStatus {
    Proposed,
    Approved,
    Applying,
    Applied,
    Verified,
    Failed,
    RolledBack,
    Suppressed,
}

/// Concrete candidate repair generated from a root cause.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairAction {
    pub id: Uuid,
    pub cause_fingerprint: String,
    pub strategy: RepairStrategy,
    pub description: String,
    pub commands: Vec<String>,
    pub confidence: f32,
    pub risk: RepairRisk,
    pub status: RepairStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl RepairAction {
    fn score(&self) -> f32 {
        let risk_penalty = match self.risk {
            RepairRisk::Low => 0.0,
            RepairRisk::Medium => 0.08,
            RepairRisk::High => 0.2,
            RepairRisk::Critical => 0.35,
        };
        (self.confidence - risk_penalty).clamp(0.0, 1.0)
    }
}

/// Planner + tracker for repair actions.
#[derive(Clone)]
pub struct RepairPlanner {
    tracked: Arc<DashMap<Uuid, RepairAction>>,
    http_client: reqwest::Client,
    webhook: Option<reqwest::Url>,
    audit_pool: Option<sqlx::PgPool>,
}

impl std::fmt::Debug for RepairPlanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepairPlanner")
            .field("tracked_len", &self.tracked.len())
            .field("webhook", &self.webhook)
            .field(
                "audit_pool",
                &self.audit_pool.as_ref().map(|_| "configured"),
            )
            .finish()
    }
}

impl Default for RepairPlanner {
    fn default() -> Self {
        Self::new()
    }
}

impl RepairPlanner {
    pub fn new() -> Self {
        Self {
            tracked: Arc::new(DashMap::new()),
            http_client: reqwest::Client::new(),
            webhook: None,
            audit_pool: None,
        }
    }

    pub fn with_webhook(mut self, webhook: reqwest::Url) -> Self {
        self.webhook = Some(webhook);
        self
    }

    pub fn with_audit_pool(mut self, pool: sqlx::PgPool) -> Self {
        self.audit_pool = Some(pool);
        self
    }

    /// Generate candidate actions for all detected causes.
    pub fn propose_actions(&self, report: &AnalysisReport) -> Vec<RepairAction> {
        let mut actions: Vec<RepairAction> = report
            .causes
            .iter()
            .flat_map(|cause| self.propose_for_cause(cause))
            .collect();

        actions.sort_by(|a, b| b.score().total_cmp(&a.score()));
        actions
    }

    /// Generate 1+ candidate repair actions for a single root cause.
    pub fn propose_for_cause(&self, cause: &RootCause) -> Vec<RepairAction> {
        let now = Utc::now();

        let make = |strategy: RepairStrategy,
                    description: &str,
                    commands: Vec<&str>,
                    confidence: f32,
                    risk: RepairRisk| {
            RepairAction {
                id: Uuid::new_v4(),
                cause_fingerprint: cause.fingerprint.clone(),
                strategy,
                description: description.to_string(),
                commands: commands.into_iter().map(|c| c.to_string()).collect(),
                confidence: confidence.clamp(0.0, 1.0),
                risk,
                status: RepairStatus::Proposed,
                created_at: now,
                updated_at: now,
            }
        };

        match cause.category {
            RootCauseCategory::CompilationError => vec![make(
                RepairStrategy::FixCompilation,
                "Apply source-level fix for compile errors and re-run checks.",
                vec!["cargo fmt", "cargo check -p ff-evolution"],
                (cause.confidence + 0.06).min(0.98),
                RepairRisk::Low,
            )],
            RootCauseCategory::DependencyResolution => vec![make(
                RepairStrategy::PinDependency,
                "Pin or update conflicting dependencies and refresh lockfile.",
                vec!["cargo update -p <crate>", "cargo check -p ff-evolution"],
                (cause.confidence + 0.04).min(0.95),
                RepairRisk::Medium,
            )],
            RootCauseCategory::MissingConfiguration => vec![make(
                RepairStrategy::PatchConfiguration,
                "Fix missing config/env settings and retry.",
                vec!["printenv | sort", "cargo test -p ff-evolution"],
                cause.confidence,
                RepairRisk::Medium,
            )],
            RootCauseCategory::ApiContractMismatch => vec![make(
                RepairStrategy::ContractAlignment,
                "Update serialization/deserialization or endpoint assumptions.",
                vec!["cargo test -p ff-evolution -- --nocapture"],
                cause.confidence,
                RepairRisk::Medium,
            )],
            RootCauseCategory::NetworkInstability => vec![make(
                RepairStrategy::RetryWithBackoff,
                "Retry failed operation with jittered backoff and circuit guards.",
                vec!["retry with exponential backoff"],
                (cause.confidence - 0.05).max(0.45),
                RepairRisk::Low,
            )],
            RootCauseCategory::ResourceExhaustion => vec![make(
                RepairStrategy::IncreaseResources,
                "Reduce parallelism or provision additional resources.",
                vec![
                    "export RUSTFLAGS='-C codegen-units=1'",
                    "cargo check -p ff-evolution",
                ],
                cause.confidence,
                RepairRisk::High,
            )],
            RootCauseCategory::TestRegression => vec![make(
                RepairStrategy::StabilizeTest,
                "Adjust implementation or assertions to restore expected behavior.",
                vec!["cargo test -p ff-evolution"],
                (cause.confidence + 0.03).min(0.95),
                RepairRisk::Low,
            )],
            RootCauseCategory::FlakyBehavior => vec![
                make(
                    RepairStrategy::StabilizeTest,
                    "Harden tests against race/timing flakiness.",
                    vec!["cargo test -p ff-evolution -- --test-threads=1"],
                    cause.confidence,
                    RepairRisk::Low,
                ),
                make(
                    RepairStrategy::RetryWithBackoff,
                    "Add retry policy while deeper root cause is investigated.",
                    vec!["rerun flaky test suite"],
                    (cause.confidence - 0.1).max(0.4),
                    RepairRisk::Medium,
                ),
            ],
            RootCauseCategory::ToolingFailure => vec![make(
                RepairStrategy::ToolchainReset,
                "Reset local toolchain caches and re-run build.",
                vec!["rustup show", "cargo clean", "cargo check -p ff-evolution"],
                cause.confidence,
                RepairRisk::Medium,
            )],
            RootCauseCategory::Unknown => vec![make(
                RepairStrategy::ManualInvestigation,
                "Escalate to manual triage with richer telemetry capture.",
                vec!["collect logs", "open ticket"],
                0.35,
                RepairRisk::Low,
            )],
        }
    }

    pub fn best_candidate(&self, actions: &[RepairAction]) -> Option<RepairAction> {
        actions
            .iter()
            .cloned()
            .max_by(|a, b| a.score().total_cmp(&b.score()))
    }

    pub fn track(&self, action: RepairAction) {
        self.tracked.insert(action.id, action);
    }

    pub fn get(&self, id: Uuid) -> Option<RepairAction> {
        self.tracked.get(&id).map(|entry| entry.clone())
    }

    pub fn tracked_len(&self) -> usize {
        self.tracked.len()
    }

    pub fn update_status(&self, id: Uuid, status: RepairStatus) -> Option<RepairAction> {
        self.tracked.get_mut(&id).map(|mut action| {
            action.status = status;
            action.updated_at = Utc::now();
            action.clone()
        })
    }

    /// Emit an audit event to an optional webhook (best effort).
    pub async fn emit_audit_event(&self, action: &RepairAction) -> anyhow::Result<()> {
        if let Some(webhook) = &self.webhook {
            let payload = serde_json::json!({
                "action_id": action.id,
                "strategy": action.strategy,
                "risk": action.risk,
                "status": action.status,
                "confidence": action.confidence,
                "timestamp": Utc::now(),
            });
            self.http_client
                .post(webhook.clone())
                .json(&payload)
                .send()
                .await?
                .error_for_status()?;
        }

        // SQL audit sink wiring point (currently webhook-first by design).
        if self.audit_pool.is_some() {
            tracing::debug!(action_id = %action.id, "audit pool configured; SQL persistence hook is disabled");
        }

        Ok(())
    }
}
