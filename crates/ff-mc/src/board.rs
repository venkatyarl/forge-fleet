//! Kanban board view.
//!
//! The board groups work items by status into columns. Board state is computed
//! on-the-fly from the database — nothing is stored separately.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::db::McDb;
use crate::error::McResult;
use crate::work_item::{WorkItem, WorkItemFilter, WorkItemStatus};

/// A single column on the Kanban board.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardColumn {
    pub status: WorkItemStatus,
    pub label: String,
    pub items: Vec<WorkItem>,
    pub count: usize,
}

/// The full board view — all columns with items.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardView {
    pub columns: Vec<BoardColumn>,
    pub total_items: usize,
}

/// Filters for the board view.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BoardFilter {
    pub assignee: Option<String>,
    pub epic_id: Option<String>,
    pub sprint_id: Option<String>,
    pub task_group_id: Option<String>,
    pub label: Option<String>,
}

impl BoardView {
    /// Build a board view from current work items, optionally filtered.
    pub fn build(db: &McDb, filter: &BoardFilter) -> McResult<Self> {
        // Fetch all matching items (no status filter — we want all columns).
        let items = WorkItem::list(
            db,
            &WorkItemFilter {
                status: None,
                assignee: filter.assignee.clone(),
                epic_id: filter.epic_id.clone(),
                sprint_id: filter.sprint_id.clone(),
                task_group_id: filter.task_group_id.clone(),
                label: filter.label.clone(),
            },
        )?;

        let total = items.len();

        // Group by status using a BTreeMap for stable ordering.
        let mut grouped: BTreeMap<u8, Vec<WorkItem>> = BTreeMap::new();
        for item in items {
            let key = status_sort_key(item.status);
            grouped.entry(key).or_default().push(item);
        }

        // Build columns in board order, including empty columns.
        let columns: Vec<BoardColumn> = WorkItemStatus::all_columns()
            .iter()
            .map(|status| {
                let key = status_sort_key(*status);
                let items = grouped.remove(&key).unwrap_or_default();
                let count = items.len();
                BoardColumn {
                    status: *status,
                    label: status_label(*status),
                    items,
                    count,
                }
            })
            .collect();

        Ok(Self {
            columns,
            total_items: total,
        })
    }
}

/// Sort key for board columns (left to right).
fn status_sort_key(s: WorkItemStatus) -> u8 {
    match s {
        WorkItemStatus::Backlog => 0,
        WorkItemStatus::Todo => 1,
        WorkItemStatus::InProgress => 2,
        WorkItemStatus::Review => 3,
        WorkItemStatus::Done => 4,
        WorkItemStatus::Blocked => 5,
    }
}

/// Human-friendly column label.
fn status_label(s: WorkItemStatus) -> String {
    match s {
        WorkItemStatus::Backlog => "📋 Backlog".into(),
        WorkItemStatus::Todo => "📝 To Do".into(),
        WorkItemStatus::InProgress => "🔨 In Progress".into(),
        WorkItemStatus::Review => "🔍 Review".into(),
        WorkItemStatus::Done => "✅ Done".into(),
        WorkItemStatus::Blocked => "🚫 Blocked".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_item::CreateWorkItem;

    fn test_db() -> McDb {
        McDb::in_memory().unwrap()
    }

    #[test]
    fn test_board_view() {
        let db = test_db();

        WorkItem::create(
            &db,
            CreateWorkItem {
                title: "Backlog item".into(),
                status: Some("backlog".into()),
                ..Default::default()
            },
        )
        .unwrap();
        WorkItem::create(
            &db,
            CreateWorkItem {
                title: "In progress item".into(),
                status: Some("in_progress".into()),
                assignee: Some("taylor".into()),
                ..Default::default()
            },
        )
        .unwrap();
        WorkItem::create(
            &db,
            CreateWorkItem {
                title: "Done item".into(),
                status: Some("done".into()),
                assignee: Some("taylor".into()),
                ..Default::default()
            },
        )
        .unwrap();

        // Full board
        let board = BoardView::build(&db, &BoardFilter::default()).unwrap();
        assert_eq!(board.total_items, 3);
        assert_eq!(board.columns.len(), 6); // all 6 columns always present

        let backlog = board
            .columns
            .iter()
            .find(|c| c.status == WorkItemStatus::Backlog)
            .unwrap();
        assert_eq!(backlog.count, 1);

        let todo = board
            .columns
            .iter()
            .find(|c| c.status == WorkItemStatus::Todo)
            .unwrap();
        assert_eq!(todo.count, 0);

        // Filtered by assignee
        let filtered = BoardView::build(
            &db,
            &BoardFilter {
                assignee: Some("taylor".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(filtered.total_items, 2);
    }
}
