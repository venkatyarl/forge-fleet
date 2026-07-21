//! Typed persistence model for project-management work items.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::FromRow;
use uuid::Uuid;

/// The persistent representation of a row in `work_items`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FromRow)]
pub struct WorkItem {
    pub id: Uuid,
    pub project_id: String,
    pub milestone_id: Option<Uuid>,
    pub parent_id: Option<Uuid>,
    pub kind: String,
    pub title: String,
    pub description: Option<String>,
    pub labels: Value,
    pub status: String,
    pub priority: String,
    pub assigned_to: Option<String>,
    pub assigned_computer: Option<String>,
    pub branch_name: Option<String>,
    pub pr_url: Option<String>,
    pub brain_node_ids: Value,
    pub created_at: DateTime<Utc>,
    pub created_by: String,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub due_date: Option<NaiveDate>,
    pub estimated_hours: Option<f64>,
    pub metadata: Value,
    pub required_capabilities: Value,
    pub complexity: String,
    pub predicted_paths: Value,
    pub touched_paths: Value,
    pub base_branch: Option<String>,
    pub base_sha: Option<String>,
    pub integration_branch: Option<String>,
    pub merge_rank: Option<i32>,
    pub risk_score: f32,
    pub reviewer_required: bool,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub repo_id: Option<Uuid>,
    pub repo_url: Option<String>,
    pub repo_path: Option<String>,
    pub context: Value,
    pub parked: bool,
    pub pre_work: Value,
    pub work: Value,
    pub post_work: Value,
    pub cleanup_complete: bool,
    pub original_signal: Value,
    pub signal_cleared: Option<bool>,
    pub signal_verified_at: Option<DateTime<Utc>>,
    pub refiled_from: Option<Uuid>,
}
