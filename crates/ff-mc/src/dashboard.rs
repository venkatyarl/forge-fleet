//! Dashboard stats for Mission Control.
//!
//! Computes summary statistics from work items, sprints, and epics.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Dashboard summary statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardStats {
    /// Total count of work items per status.
    pub items_by_status: HashMap<String, i64>,
    /// Total work items.
    pub total_items: i64,
    /// Count of work items per assignee.
    pub items_per_assignee: HashMap<String, i64>,
    /// Blocked items (details).
    pub blocked_items: Vec<BlockedItemSummary>,
    /// Sprint velocity trend (last N sprints).
    pub velocity_trend: Vec<VelocityPoint>,
    /// Overdue items (in sprints past their end date but not done).
    pub overdue_items: Vec<OverdueItemSummary>,
}

/// A blocked item's summary for the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockedItemSummary {
    pub id: String,
    pub title: String,
    pub assignee: String,
    pub epic_id: Option<String>,
}

/// Velocity data point — one per sprint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VelocityPoint {
    pub sprint_id: String,
    pub sprint_name: String,
    pub done_items: usize,
    pub total_items: usize,
}

/// An overdue work item summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverdueItemSummary {
    pub id: String,
    pub title: String,
    pub sprint_id: String,
    pub sprint_name: String,
    pub sprint_end_date: String,
    pub status: String,
}
