//! Sprint model and CRUD operations.
//!
//! Sprints are time-boxed iterations. Work items can be assigned to a sprint.
//! Velocity tracking measures how many items/points are completed per sprint.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

// ─── Sprint Model ────────────────────────────────────────────────────────────

/// A sprint — a time-boxed iteration of work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sprint {
    pub id: String,
    pub name: String,
    pub start_date: Option<NaiveDate>,
    pub end_date: Option<NaiveDate>,
    pub goal: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Sprint with velocity and burndown data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SprintWithStats {
    #[serde(flatten)]
    pub sprint: Sprint,
    pub total_items: usize,
    pub done_items: usize,
    pub in_progress_items: usize,
    pub blocked_items: usize,
    pub velocity: f64,
    pub work_item_ids: Vec<String>,
}

/// Burndown data point — one per day of the sprint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BurndownPoint {
    pub date: NaiveDate,
    pub ideal_remaining: f64,
    pub actual_remaining: usize,
}

/// Create parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSprint {
    pub name: String,
    #[serde(default)]
    pub start_date: Option<String>,
    #[serde(default)]
    pub end_date: Option<String>,
    #[serde(default)]
    pub goal: String,
}

/// Update parameters.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateSprint {
    pub name: Option<String>,
    pub start_date: Option<Option<String>>,
    pub end_date: Option<Option<String>>,
    pub goal: Option<String>,
}
