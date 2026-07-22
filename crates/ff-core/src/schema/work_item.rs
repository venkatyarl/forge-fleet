//! Work items derived from planning and agent schema sources.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Lifecycle state of a derived work item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemStatus {
    Backlog,
    Todo,
    InProgress,
    Review,
    Done,
    Blocked,
}

/// A unit of work derived from a plan, skill, or agent definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: Uuid,
    pub title: String,
    pub description: String,
    pub status: WorkItemStatus,
    pub source_ref: String,
    pub derived_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_item_round_trips_through_json() {
        let work_item = WorkItem {
            id: Uuid::nil(),
            title: "Define shared schema".to_string(),
            description: "Add the derived work-item schema.".to_string(),
            status: WorkItemStatus::InProgress,
            source_ref: "plan://schema/work-item".to_string(),
            derived_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        };

        let json = serde_json::to_string(&work_item).unwrap();
        let decoded: WorkItem = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded.id, work_item.id);
        assert_eq!(decoded.status, WorkItemStatus::InProgress);
        assert_eq!(decoded.derived_at, work_item.derived_at);
    }
}
