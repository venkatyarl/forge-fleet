//! Typed persistence model for project-management work items.

use chrono::{DateTime, NaiveDate, Utc};
use ff_core::schema::work_items::Quadrant;
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
    #[serde(default)]
    pub eisenhower_quadrant: Option<String>,
    #[serde(default)]
    pub numeric_priority: Option<i32>,
    #[serde(default)]
    pub pick_score: Option<f64>,
    #[serde(default)]
    pub blocked_by_count: i64,
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

impl WorkItem {
    /// Compute the scheduler pick score from urgency, priority, age, and blockers.
    pub fn compute_pick_score(&self) -> f64 {
        let quadrant = self
            .eisenhower_quadrant
            .as_deref()
            .and_then(parse_quadrant)
            .unwrap_or(Quadrant::Q4);
        let priority = self
            .numeric_priority
            .filter(|value| (1..=5).contains(value))
            .unwrap_or(3);
        let age_hours = Utc::now()
            .signed_duration_since(self.created_at)
            .num_seconds() as f64
            / 3600.0;

        quadrant.base_score() + ((6 - priority) * 100) as f64 + age_hours * 10.0
            - self.blocked_by_count as f64 * 50.0
    }

    /// Compute the optional WSJF variant using required capabilities as job size.
    pub fn compute_wsjf_pick_score(&self) -> f64 {
        let job_size = self
            .required_capabilities
            .as_array()
            .map_or(1.0, |capabilities| capabilities.len().max(1) as f64);
        self.compute_pick_score() / job_size
    }
}

fn parse_quadrant(value: &str) -> Option<Quadrant> {
    match value.trim().to_ascii_lowercase().as_str() {
        "q1" | "urgent_important" | "urgent-important" => Some(Quadrant::Q1),
        "q2" | "important_not_urgent" | "important-not-urgent" => Some(Quadrant::Q2),
        "q3" | "urgent_not_important" | "urgent-not-important" => Some(Quadrant::Q3),
        "q4" | "not_urgent_not_important" | "not-urgent-not-important" => Some(Quadrant::Q4),
        _ => None,
    }
}
