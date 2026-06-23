//! Work-item dependency graph persistence and checks.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItemDependency {
    pub work_item_id: String,
    pub depends_on_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyCheck {
    pub work_item_id: String,
    pub blocked_by_ids: Vec<String>,
    pub blocked_count: usize,
    pub can_start: bool,
}
