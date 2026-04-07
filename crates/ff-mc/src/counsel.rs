//! Counsel mode — multi-model AI review for work items.
//!
//! Dispatches a work item to multiple fleet LLM models for independent review.
//! Aggregates responses with confidence scoring and dissent tracking.
//! ForgeFleet-native multi-model consensus review system.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Counsel mode configuration for a work item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CounselConfig {
    /// Whether counsel mode is enabled for this work item.
    pub enabled: bool,
    /// Models to consult (e.g., ["qwen2.5-72b", "gemma-4-31b"]).
    pub models: Vec<String>,
}

/// A response from one model in counsel mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CounselResponse {
    pub model: String,
    pub response: String,
    pub confidence: f64,
    pub dissent: Option<String>,
    pub responded_at: DateTime<Utc>,
}

/// Aggregated counsel result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CounselResult {
    pub responses: Vec<CounselResponse>,
    pub consensus: Option<String>,
    pub average_confidence: f64,
    pub has_dissent: bool,
}

impl CounselResult {
    pub fn from_responses(responses: Vec<CounselResponse>) -> Self {
        let avg_conf = if responses.is_empty() {
            0.0
        } else {
            responses.iter().map(|r| r.confidence).sum::<f64>() / responses.len() as f64
        };

        let has_dissent = responses.iter().any(|r| r.dissent.is_some());

        Self {
            consensus: None, // To be filled by aggregation logic
            average_confidence: avg_conf,
            has_dissent,
            responses,
        }
    }
}

/// Docker stack for per-ticket isolation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerStack {
    pub id: String,
    pub work_item_id: String,
    pub node_name: String,
    pub compose_path: String,
    pub db_volume_name: Option<String>,
    pub ports: serde_json::Value,
    pub status: DockerStackStatus,
    pub last_active_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DockerStackStatus {
    Running,
    Stopped,
    Stale,
    Failed,
}

/// Work item event for audit trail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItemEvent {
    pub id: String,
    pub work_item_id: String,
    pub event_type: String,
    pub actor: String,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    pub details: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

/// Node message for fleet-internal communication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeMessage {
    pub id: String,
    pub from_node: String,
    pub to_node: String,
    pub message_type: String,
    pub subject: String,
    pub body: String,
    pub work_item_id: Option<String>,
    pub read_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Model performance tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPerformance {
    pub id: String,
    pub model_name: String,
    pub task_id: String,
    pub quality_score: f64,
    pub passed: bool,
    pub duration_secs: f64,
    pub created_at: DateTime<Utc>,
}

/// Chat session with conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSession {
    pub id: String,
    pub mode: String,
    pub project_id: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    pub timestamp: DateTime<Utc>,
}

/// Extended work item fields for advanced workflows.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkItemExtensions {
    /// Counsel mode
    pub counsel_mode: bool,
    pub counsel_models: Vec<String>,
    pub counsel_responses: Vec<CounselResponse>,
    pub confidence: Option<f64>,
    pub dissent: Option<String>,

    /// Escalation
    pub escalation_level: u32,
    pub escalation_reason: Option<String>,

    /// Timer
    pub manual_timer_state: Option<String>,
    pub manual_timer_elapsed_ms: u64,

    /// Retry tracking
    pub retry_count: u32,
    pub max_retries: u32,
    pub failed_models: Vec<String>,

    /// PR/Branch
    pub branch_name: Option<String>,
    pub base_branch: Option<String>,
    pub pr_number: Option<u32>,
    pub pr_url: Option<String>,
    pub pr_status: Option<String>,

    /// Review
    pub review_bounce_count: u32,
    pub rejection_history: Vec<serde_json::Value>,

    /// Docker
    pub docker_stack_id: Option<String>,

    /// Assignment
    pub builder_node_id: Option<String>,
    pub reviewer_node_id: Option<String>,

    /// Content dedup
    pub content_hash: Option<String>,
}
