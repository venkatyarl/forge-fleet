//! Kanban board view.
//!
//! The board groups work items by status into columns. Board state is computed
//! on-the-fly from the database — nothing is stored separately.

use serde::{Deserialize, Serialize};

use crate::work_item::{WorkItem, WorkItemStatus};

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
