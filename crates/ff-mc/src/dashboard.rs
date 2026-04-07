//! Dashboard stats for Mission Control.
//!
//! Computes summary statistics from work items, sprints, and epics.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::db::McDb;
use crate::error::McResult;
use crate::sprint::Sprint;
use crate::work_item::{WorkItem, WorkItemFilter, WorkItemStatus};

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

impl DashboardStats {
    /// Compute full dashboard stats from the database.
    pub fn compute(db: &McDb) -> McResult<Self> {
        let items_by_status = Self::compute_items_by_status(db)?;
        let total_items = items_by_status.values().sum();
        let items_per_assignee = Self::compute_items_per_assignee(db)?;
        let blocked_items = Self::compute_blocked_items(db)?;
        let velocity_trend = Self::compute_velocity_trend(db)?;
        let overdue_items = Self::compute_overdue_items(db)?;

        Ok(Self {
            items_by_status,
            total_items,
            items_per_assignee,
            blocked_items,
            velocity_trend,
            overdue_items,
        })
    }

    fn compute_items_by_status(db: &McDb) -> McResult<HashMap<String, i64>> {
        let counts = WorkItem::count_by_status(db)?;
        let mut map = HashMap::new();
        for (status, count) in counts {
            map.insert(status.as_str().to_string(), count);
        }
        Ok(map)
    }

    fn compute_items_per_assignee(db: &McDb) -> McResult<HashMap<String, i64>> {
        let conn = db.conn();
        let mut stmt =
            conn.prepare("SELECT assignee, COUNT(*) FROM work_items GROUP BY assignee")?;
        let rows = stmt
            .query_map([], |row| {
                let assignee: String = row.get(0)?;
                let count: i64 = row.get(1)?;
                Ok((assignee, count))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().collect())
    }

    fn compute_blocked_items(db: &McDb) -> McResult<Vec<BlockedItemSummary>> {
        let blocked = WorkItem::list(
            db,
            &WorkItemFilter {
                status: Some(WorkItemStatus::Blocked),
                ..Default::default()
            },
        )?;
        Ok(blocked
            .into_iter()
            .map(|i| BlockedItemSummary {
                id: i.id,
                title: i.title,
                assignee: i.assignee,
                epic_id: i.epic_id,
            })
            .collect())
    }

    fn compute_velocity_trend(db: &McDb) -> McResult<Vec<VelocityPoint>> {
        let sprints = Sprint::list(db)?;
        let mut trend = Vec::new();

        for sprint in sprints.into_iter().take(10) {
            let items = WorkItem::list(
                db,
                &WorkItemFilter {
                    sprint_id: Some(sprint.id.clone()),
                    ..Default::default()
                },
            )?;
            let total = items.len();
            let done = items
                .iter()
                .filter(|i| i.status == WorkItemStatus::Done)
                .count();

            trend.push(VelocityPoint {
                sprint_id: sprint.id,
                sprint_name: sprint.name,
                done_items: done,
                total_items: total,
            });
        }

        Ok(trend)
    }

    fn compute_overdue_items(db: &McDb) -> McResult<Vec<OverdueItemSummary>> {
        let today = chrono::Utc::now().date_naive();
        let sprints = Sprint::list(db)?;
        let mut overdue = Vec::new();

        for sprint in sprints {
            // Only check sprints with an end date in the past
            let end = match sprint.end_date {
                Some(d) if d < today => d,
                _ => continue,
            };

            let items = WorkItem::list(
                db,
                &WorkItemFilter {
                    sprint_id: Some(sprint.id.clone()),
                    ..Default::default()
                },
            )?;

            for item in items {
                if item.status != WorkItemStatus::Done {
                    overdue.push(OverdueItemSummary {
                        id: item.id,
                        title: item.title,
                        sprint_id: sprint.id.clone(),
                        sprint_name: sprint.name.clone(),
                        sprint_end_date: end.to_string(),
                        status: item.status.as_str().to_string(),
                    });
                }
            }
        }

        Ok(overdue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sprint::CreateSprint;
    use crate::work_item::CreateWorkItem;

    fn test_db() -> McDb {
        McDb::in_memory().unwrap()
    }

    #[test]
    fn test_dashboard_stats() {
        let db = test_db();

        // Create some items
        WorkItem::create(
            &db,
            CreateWorkItem {
                title: "A".into(),
                status: Some("done".into()),
                assignee: Some("taylor".into()),
                ..Default::default()
            },
        )
        .unwrap();
        WorkItem::create(
            &db,
            CreateWorkItem {
                title: "B".into(),
                status: Some("blocked".into()),
                assignee: Some("james".into()),
                ..Default::default()
            },
        )
        .unwrap();
        WorkItem::create(
            &db,
            CreateWorkItem {
                title: "C".into(),
                status: Some("in_progress".into()),
                assignee: Some("taylor".into()),
                ..Default::default()
            },
        )
        .unwrap();

        let stats = DashboardStats::compute(&db).unwrap();

        assert_eq!(stats.total_items, 3);
        assert_eq!(*stats.items_by_status.get("done").unwrap_or(&0), 1);
        assert_eq!(*stats.items_by_status.get("blocked").unwrap_or(&0), 1);
        assert_eq!(stats.blocked_items.len(), 1);
        assert_eq!(stats.blocked_items[0].title, "B");
        assert_eq!(*stats.items_per_assignee.get("taylor").unwrap_or(&0), 2);
    }

    #[test]
    fn test_velocity_trend() {
        let db = test_db();

        let sprint = Sprint::create(
            &db,
            CreateSprint {
                name: "Sprint 1".into(),
                start_date: Some("2026-03-01".into()),
                end_date: Some("2026-03-14".into()),
                goal: "Test velocity".into(),
            },
        )
        .unwrap();

        WorkItem::create(
            &db,
            CreateWorkItem {
                title: "Done".into(),
                status: Some("done".into()),
                sprint_id: Some(sprint.id.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        WorkItem::create(
            &db,
            CreateWorkItem {
                title: "WIP".into(),
                status: Some("in_progress".into()),
                sprint_id: Some(sprint.id.clone()),
                ..Default::default()
            },
        )
        .unwrap();

        let stats = DashboardStats::compute(&db).unwrap();
        assert_eq!(stats.velocity_trend.len(), 1);
        assert_eq!(stats.velocity_trend[0].done_items, 1);
        assert_eq!(stats.velocity_trend[0].total_items, 2);
    }
}
