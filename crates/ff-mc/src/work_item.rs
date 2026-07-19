//! Work item model and CRUD operations.
//!
//! A work item is the fundamental unit of work in Mission Control — like a ticket
//! or issue. It has a status, priority, assignee, and can belong to an epic and sprint.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{McError, McResult};

// ─── Status ──────────────────────────────────────────────────────────────────

/// Work item status — tracks where it is in the workflow.
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

impl WorkItemStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Backlog => "backlog",
            Self::Todo => "todo",
            Self::InProgress => "in_progress",
            Self::Review => "review",
            Self::Done => "done",
            Self::Blocked => "blocked",
        }
    }

    pub fn from_str_loose(s: &str) -> McResult<Self> {
        match s.to_lowercase().replace('-', "_").as_str() {
            "backlog" => Ok(Self::Backlog),
            "todo" => Ok(Self::Todo),
            "in_progress" | "inprogress" | "in progress" => Ok(Self::InProgress),
            "review" => Ok(Self::Review),
            "done" | "complete" | "completed" => Ok(Self::Done),
            "blocked" => Ok(Self::Blocked),
            other => Err(McError::InvalidStatus {
                value: other.to_string(),
            }),
        }
    }

    /// All possible statuses in board column order.
    pub fn all_columns() -> &'static [Self] {
        &[
            Self::Backlog,
            Self::Todo,
            Self::InProgress,
            Self::Review,
            Self::Done,
            Self::Blocked,
        ]
    }
}

impl std::fmt::Display for WorkItemStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ─── Priority ────────────────────────────────────────────────────────────────

/// Priority level 1 (critical) through 5 (low).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Priority(pub i32);

impl Priority {
    pub fn new(v: i32) -> McResult<Self> {
        if (1..=5).contains(&v) {
            Ok(Self(v))
        } else {
            Err(McError::InvalidPriority { value: v })
        }
    }

    pub fn label(&self) -> &'static str {
        match self.0 {
            1 => "critical",
            2 => "high",
            3 => "medium",
            4 => "low",
            5 => "minimal",
            _ => "unknown",
        }
    }
}

impl Default for Priority {
    fn default() -> Self {
        Self(3)
    }
}

// ─── Work Item ───────────────────────────────────────────────────────────────

/// A single work item (ticket / task / issue).
///
/// The work-queue priority fields (`eisenhower_quadrant`, `numeric_priority`,
/// `pick_score`, `capability_tags`) all carry `#[serde(default)]` — records
/// persisted before these fields existed must still deserialize.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: WorkItemStatus,
    pub priority: Priority,
    pub assignee: String,
    pub epic_id: Option<String>,
    pub sprint_id: Option<String>,
    pub task_group_id: Option<String>,
    pub sequence_order: Option<i32>,
    pub labels: Vec<String>,
    /// Eisenhower matrix quadrant: `urgent_important`, `not_urgent_important`,
    /// `urgent_not_important`, or `not_urgent_not_important`.
    #[serde(default)]
    pub eisenhower_quadrant: Option<String>,
    /// Fine-grained numeric priority for work-queue ordering (higher = sooner);
    /// complements the coarse 1–5 [`Priority`] band.
    #[serde(default)]
    pub numeric_priority: Option<i32>,
    /// Computed scheduler pick score; recalculated by the work queue, not set
    /// by operators.
    #[serde(default)]
    pub pick_score: Option<f64>,
    /// Capabilities a slot must have to pick this item up (e.g. `gpu`, `macos`).
    #[serde(default)]
    pub capability_tags: Vec<String>,
    /// Free-form structured context attached to the work item (e.g. task
    /// metadata, source-system identifiers, or dispatch hints).
    #[serde(default)]
    pub context: Option<Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Parameters for creating a new work item.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CreateWorkItem {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub priority: Option<i32>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub epic_id: Option<String>,
    #[serde(default)]
    pub sprint_id: Option<String>,
    #[serde(default)]
    pub task_group_id: Option<String>,
    #[serde(default)]
    pub sequence_order: Option<i32>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub context: Option<Value>,
}

/// Parameters for updating a work item. All fields optional.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateWorkItem {
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    pub priority: Option<i32>,
    pub assignee: Option<String>,
    pub epic_id: Option<Option<String>>,
    pub sprint_id: Option<Option<String>>,
    pub task_group_id: Option<Option<String>>,
    pub sequence_order: Option<Option<i32>>,
    pub labels: Option<Vec<String>>,
    pub context: Option<Option<Value>>,
}

// ─── CRUD ────────────────────────────────────────────────────────────────────

/// Filter criteria for listing work items.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkItemFilter {
    pub status: Option<WorkItemStatus>,
    pub assignee: Option<String>,
    pub epic_id: Option<String>,
    pub sprint_id: Option<String>,
    pub task_group_id: Option<String>,
    pub label: Option<String>,
}
