//! Release domain types for deployment orchestration.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::strategy::RolloutStrategy;

/// Release promotion channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseChannel {
    /// Development channel.
    Dev,
    /// Staging channel.
    Staging,
    /// Production channel.
    Production,
}

/// Lifecycle state of a release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseState {
    /// Release record created but not started.
    Draft,
    /// Rollout in progress.
    RollingOut,
    /// Rollout completed successfully.
    Succeeded,
    /// Rollout failed.
    Failed,
    /// Rollout reverted.
    RolledBack,
}

/// Immutable release metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseManifest {
    /// Unique release id.
    pub id: Uuid,
    /// Service/application name.
    pub service: String,
    /// Version to deploy.
    pub version: String,
    /// Previous known-good version.
    pub previous_version: Option<String>,
    /// Release channel.
    pub channel: ReleaseChannel,
    /// Principal/user that requested release.
    pub requested_by: String,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
}

impl ReleaseManifest {
    /// Construct a new release manifest.
    pub fn new(
        service: impl Into<String>,
        version: impl Into<String>,
        previous_version: Option<String>,
        channel: ReleaseChannel,
        requested_by: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            service: service.into(),
            version: version.into(),
            previous_version,
            channel,
            requested_by: requested_by.into(),
            created_at: Utc::now(),
        }
    }
}

/// Mutable runtime state for a release execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseRecord {
    /// Manifest information.
    pub manifest: ReleaseManifest,
    /// Rollout strategy for this release.
    pub strategy: RolloutStrategy,
    /// Current lifecycle state.
    pub state: ReleaseState,
    /// Optional failure reason.
    pub failure_reason: Option<String>,
    /// Start timestamp.
    pub started_at: Option<DateTime<Utc>>,
    /// End timestamp.
    pub completed_at: Option<DateTime<Utc>>,
    /// Free-form execution notes.
    pub notes: Vec<String>,
}

impl ReleaseRecord {
    /// Construct a release execution record.
    pub fn new(manifest: ReleaseManifest, strategy: RolloutStrategy) -> Self {
        Self {
            manifest,
            strategy,
            state: ReleaseState::Draft,
            failure_reason: None,
            started_at: None,
            completed_at: None,
            notes: Vec::new(),
        }
    }

    /// Transition release into rolling out state.
    pub fn mark_started(&mut self) {
        self.state = ReleaseState::RollingOut;
        self.started_at = Some(Utc::now());
    }

    /// Mark release as successful.
    pub fn mark_succeeded(&mut self) {
        self.state = ReleaseState::Succeeded;
        self.completed_at = Some(Utc::now());
    }

    /// Mark release as failed with reason.
    pub fn mark_failed(&mut self, reason: impl Into<String>) {
        self.state = ReleaseState::Failed;
        self.failure_reason = Some(reason.into());
        self.completed_at = Some(Utc::now());
    }

    /// Mark release as rolled back with reason.
    pub fn mark_rolled_back(&mut self, reason: impl Into<String>) {
        self.state = ReleaseState::RolledBack;
        self.failure_reason = Some(reason.into());
        self.completed_at = Some(Utc::now());
    }

    /// Append an operator note.
    pub fn add_note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }
}
