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

/// Score weight applied when a work item is important.
pub const IMPORTANCE_WEIGHT: f64 = 500.0;
/// Score weight applied when a work item is urgent.
pub const URGENCY_WEIGHT: f64 = 250.0;
/// Score weight shared by every quadrant.
pub const BASE_WEIGHT: f64 = 250.0;

/// Eisenhower-matrix quadrant classifying a work item by urgency and importance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EisenhowerQuadrant {
    /// Urgent and important: do first.
    UrgentImportant,
    /// Important but not urgent: schedule.
    ImportantNotUrgent,
    /// Urgent but not important: delegate if possible.
    UrgentNotImportant,
    /// Neither urgent nor important: defer.
    Neither,
}

impl EisenhowerQuadrant {
    /// Whether this quadrant is time-sensitive.
    const fn is_urgent(self) -> bool {
        matches!(self, Self::UrgentImportant | Self::UrgentNotImportant)
    }

    /// Whether this quadrant is high-impact.
    const fn is_important(self) -> bool {
        matches!(self, Self::UrgentImportant | Self::ImportantNotUrgent)
    }

    /// Weighted score combining urgency and importance; higher is scheduled sooner.
    pub const fn score(self) -> f64 {
        BASE_WEIGHT
            + if self.is_urgent() {
                URGENCY_WEIGHT
            } else {
                0.0
            }
            + if self.is_important() {
                IMPORTANCE_WEIGHT
            } else {
                0.0
            }
    }
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
    pub priority_score: Option<f64>,
    pub quadrant: EisenhowerQuadrant,
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
            priority_score: Some(1000.0),
            quadrant: EisenhowerQuadrant::UrgentImportant,
        };

        let json = serde_json::to_string(&work_item).unwrap();
        let decoded: WorkItem = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded.id, work_item.id);
        assert_eq!(decoded.status, WorkItemStatus::InProgress);
        assert_eq!(decoded.derived_at, work_item.derived_at);
        assert_eq!(decoded.priority_score, work_item.priority_score);
        assert_eq!(decoded.quadrant, work_item.quadrant);
    }

    #[test]
    fn eisenhower_quadrant_scores_are_ordered() {
        assert!(
            EisenhowerQuadrant::UrgentImportant.score()
                > EisenhowerQuadrant::ImportantNotUrgent.score()
        );
        assert!(
            EisenhowerQuadrant::ImportantNotUrgent.score()
                > EisenhowerQuadrant::UrgentNotImportant.score()
        );
        assert!(
            EisenhowerQuadrant::UrgentNotImportant.score() > EisenhowerQuadrant::Neither.score()
        );

        assert_eq!(EisenhowerQuadrant::UrgentImportant.score(), 1000.0);
        assert_eq!(EisenhowerQuadrant::ImportantNotUrgent.score(), 750.0);
        assert_eq!(EisenhowerQuadrant::UrgentNotImportant.score(), 500.0);
        assert_eq!(EisenhowerQuadrant::Neither.score(), 250.0);
    }
}
