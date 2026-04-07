//! Work item model and CRUD operations.
//!
//! A work item is the fundamental unit of work in Mission Control — like a ticket
//! or issue. It has a status, priority, assignee, and can belong to an epic and sprint.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::McDb;
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
}

// ─── CRUD ────────────────────────────────────────────────────────────────────

impl WorkItem {
    /// Create a new work item and insert it into the database.
    pub fn create(db: &McDb, params: CreateWorkItem) -> McResult<Self> {
        let now = Utc::now();
        let status = match &params.status {
            Some(s) => WorkItemStatus::from_str_loose(s)?,
            None => WorkItemStatus::Backlog,
        };
        let priority = match params.priority {
            Some(p) => Priority::new(p)?,
            None => Priority::default(),
        };

        let item = Self {
            id: Uuid::new_v4().to_string(),
            title: params.title,
            description: params.description,
            status,
            priority,
            assignee: params.assignee.unwrap_or_else(|| "unassigned".into()),
            epic_id: params.epic_id,
            sprint_id: params.sprint_id,
            task_group_id: params.task_group_id,
            sequence_order: params.sequence_order,
            labels: params.labels,
            created_at: now,
            updated_at: now,
        };

        let labels_json =
            serde_json::to_string(&item.labels).map_err(|e| McError::Other(e.into()))?;

        let conn = db.conn();
        conn.execute(
            "INSERT INTO work_items (id, title, description, status, priority, assignee, epic_id, sprint_id, task_group_id, sequence_order, labels, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            rusqlite::params![
                item.id,
                item.title,
                item.description,
                item.status.as_str(),
                item.priority.0,
                item.assignee,
                item.epic_id,
                item.sprint_id,
                item.task_group_id,
                item.sequence_order,
                labels_json,
                item.created_at.to_rfc3339(),
                item.updated_at.to_rfc3339(),
            ],
        )?;

        Ok(item)
    }

    /// Get a work item by ID.
    pub fn get(db: &McDb, id: &str) -> McResult<Self> {
        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT id, title, description, status, priority, assignee, epic_id, sprint_id, task_group_id, sequence_order, labels, created_at, updated_at
             FROM work_items WHERE id = ?1",
        )?;

        stmt.query_row(rusqlite::params![id], Self::from_row)
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    McError::WorkItemNotFound { id: id.to_string() }
                }
                other => McError::Database(other),
            })
    }

    /// List all work items, optionally filtered.
    pub fn list(db: &McDb, filter: &WorkItemFilter) -> McResult<Vec<Self>> {
        let conn = db.conn();
        let mut sql = String::from(
            "SELECT id, title, description, status, priority, assignee, epic_id, sprint_id, task_group_id, sequence_order, labels, created_at, updated_at FROM work_items WHERE 1=1",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut param_idx = 1;

        if let Some(status) = &filter.status {
            sql.push_str(&format!(" AND status = ?{param_idx}"));
            params_vec.push(Box::new(status.as_str().to_string()));
            param_idx += 1;
        }
        if let Some(assignee) = &filter.assignee {
            sql.push_str(&format!(" AND assignee = ?{param_idx}"));
            params_vec.push(Box::new(assignee.clone()));
            param_idx += 1;
        }
        if let Some(epic_id) = &filter.epic_id {
            sql.push_str(&format!(" AND epic_id = ?{param_idx}"));
            params_vec.push(Box::new(epic_id.clone()));
            param_idx += 1;
        }
        if let Some(sprint_id) = &filter.sprint_id {
            sql.push_str(&format!(" AND sprint_id = ?{param_idx}"));
            params_vec.push(Box::new(sprint_id.clone()));
            param_idx += 1;
        }
        if let Some(task_group_id) = &filter.task_group_id {
            sql.push_str(&format!(" AND task_group_id = ?{param_idx}"));
            params_vec.push(Box::new(task_group_id.clone()));
            param_idx += 1;
        }
        if let Some(label) = &filter.label {
            sql.push_str(&format!(" AND labels LIKE ?{param_idx}"));
            params_vec.push(Box::new(format!("%\"{label}\"%")));
            let _ = param_idx; // suppress unused warning on last increment
        }

        sql.push_str(" ORDER BY priority ASC, updated_at DESC");

        let mut stmt = conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();
        let items = stmt
            .query_map(params_refs.as_slice(), Self::from_row)?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(items)
    }

    /// Update a work item by ID.
    pub fn update(db: &McDb, id: &str, params: UpdateWorkItem) -> McResult<Self> {
        // Verify it exists
        let mut item = Self::get(db, id)?;

        if let Some(title) = params.title {
            item.title = title;
        }
        if let Some(desc) = params.description {
            item.description = desc;
        }
        if let Some(status_str) = params.status {
            item.status = WorkItemStatus::from_str_loose(&status_str)?;
        }
        if let Some(p) = params.priority {
            item.priority = Priority::new(p)?;
        }
        if let Some(assignee) = params.assignee {
            item.assignee = assignee;
        }
        if let Some(epic_id) = params.epic_id {
            item.epic_id = epic_id;
        }
        if let Some(sprint_id) = params.sprint_id {
            item.sprint_id = sprint_id;
        }
        if let Some(task_group_id) = params.task_group_id {
            item.task_group_id = task_group_id;
        }
        if let Some(sequence_order) = params.sequence_order {
            item.sequence_order = sequence_order;
        }
        if let Some(labels) = params.labels {
            item.labels = labels;
        }

        item.updated_at = Utc::now();

        let labels_json =
            serde_json::to_string(&item.labels).map_err(|e| McError::Other(e.into()))?;

        let conn = db.conn();
        conn.execute(
            "UPDATE work_items SET title=?1, description=?2, status=?3, priority=?4, assignee=?5, epic_id=?6, sprint_id=?7, task_group_id=?8, sequence_order=?9, labels=?10, updated_at=?11 WHERE id=?12",
            rusqlite::params![
                item.title,
                item.description,
                item.status.as_str(),
                item.priority.0,
                item.assignee,
                item.epic_id,
                item.sprint_id,
                item.task_group_id,
                item.sequence_order,
                labels_json,
                item.updated_at.to_rfc3339(),
                item.id,
            ],
        )?;

        Ok(item)
    }

    /// Delete a work item by ID.
    pub fn delete(db: &McDb, id: &str) -> McResult<()> {
        let conn = db.conn();
        let affected = conn.execute(
            "DELETE FROM work_items WHERE id = ?1",
            rusqlite::params![id],
        )?;
        if affected == 0 {
            return Err(McError::WorkItemNotFound { id: id.to_string() });
        }
        Ok(())
    }

    /// Claim a work item for an assignee.
    ///
    /// If the item is still in backlog, it is moved to `todo` as part of claim.
    pub fn claim(db: &McDb, id: &str, assignee: Option<String>) -> McResult<Self> {
        let item = Self::get(db, id)?;
        let mut update = UpdateWorkItem {
            assignee: Some(assignee.unwrap_or_else(|| "unassigned".to_string())),
            ..Default::default()
        };

        if item.status == WorkItemStatus::Backlog {
            update.status = Some("todo".to_string());
        }

        Self::update(db, id, update)
    }

    /// Mark a work item as complete.
    pub fn complete(db: &McDb, id: &str) -> McResult<Self> {
        Self::update(
            db,
            id,
            UpdateWorkItem {
                status: Some("done".to_string()),
                ..Default::default()
            },
        )
    }

    /// Mark a work item as failed/blocked.
    pub fn fail(db: &McDb, id: &str) -> McResult<Self> {
        Self::update(
            db,
            id,
            UpdateWorkItem {
                status: Some("blocked".to_string()),
                ..Default::default()
            },
        )
    }

    /// Escalate a work item by raising priority and moving to blocked if needed.
    pub fn escalate(db: &McDb, id: &str) -> McResult<Self> {
        let item = Self::get(db, id)?;
        let new_priority = (item.priority.0 - 1).clamp(1, 5);
        let status = if item.status == WorkItemStatus::Done {
            None
        } else {
            Some("blocked".to_string())
        };

        Self::update(
            db,
            id,
            UpdateWorkItem {
                priority: Some(new_priority),
                status,
                ..Default::default()
            },
        )
    }

    /// Count work items grouped by status.
    pub fn count_by_status(db: &McDb) -> McResult<Vec<(WorkItemStatus, i64)>> {
        let conn = db.conn();
        let mut stmt = conn.prepare("SELECT status, COUNT(*) FROM work_items GROUP BY status")?;
        let rows = stmt
            .query_map([], |row| {
                let status_str: String = row.get(0)?;
                let count: i64 = row.get(1)?;
                Ok((status_str, count))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut result = Vec::new();
        for (s, c) in rows {
            if let Ok(status) = WorkItemStatus::from_str_loose(&s) {
                result.push((status, c));
            }
        }
        Ok(result)
    }

    /// Helper to construct a WorkItem from a database row.
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let status_str: String = row.get(3)?;
        let status = WorkItemStatus::from_str_loose(&status_str).unwrap_or(WorkItemStatus::Backlog);
        let priority_val: i32 = row.get(4)?;
        let labels_json: String = row.get(10)?;
        let labels: Vec<String> = serde_json::from_str(&labels_json).unwrap_or_default();
        let created_str: String = row.get(11)?;
        let updated_str: String = row.get(12)?;

        Ok(Self {
            id: row.get(0)?,
            title: row.get(1)?,
            description: row.get(2)?,
            status,
            priority: Priority(priority_val),
            assignee: row.get(5)?,
            epic_id: row.get(6)?,
            sprint_id: row.get(7)?,
            task_group_id: row.get(8)?,
            sequence_order: row.get(9)?,
            labels,
            created_at: DateTime::parse_from_rfc3339(&created_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            updated_at: DateTime::parse_from_rfc3339(&updated_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
        })
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> McDb {
        McDb::in_memory().unwrap()
    }

    #[test]
    fn test_create_and_get() {
        let db = test_db();
        let item = WorkItem::create(
            &db,
            CreateWorkItem {
                title: "Test item".into(),
                description: "A test".into(),
                status: None,
                priority: Some(2),
                assignee: Some("taylor".into()),
                epic_id: None,
                sprint_id: None,
                task_group_id: None,
                sequence_order: None,
                labels: vec!["rust".into()],
            },
        )
        .unwrap();

        assert_eq!(item.status, WorkItemStatus::Backlog);
        assert_eq!(item.priority.0, 2);
        assert_eq!(item.assignee, "taylor");

        let fetched = WorkItem::get(&db, &item.id).unwrap();
        assert_eq!(fetched.title, "Test item");
        assert_eq!(fetched.labels, vec!["rust".to_string()]);
    }

    #[test]
    fn test_update() {
        let db = test_db();
        let item = WorkItem::create(
            &db,
            CreateWorkItem {
                title: "Before".into(),
                ..Default::default()
            },
        )
        .unwrap();

        let updated = WorkItem::update(
            &db,
            &item.id,
            UpdateWorkItem {
                title: Some("After".into()),
                status: Some("in_progress".into()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(updated.title, "After");
        assert_eq!(updated.status, WorkItemStatus::InProgress);
    }

    #[test]
    fn test_delete() {
        let db = test_db();
        let item = WorkItem::create(
            &db,
            CreateWorkItem {
                title: "Delete me".into(),
                ..Default::default()
            },
        )
        .unwrap();

        WorkItem::delete(&db, &item.id).unwrap();
        assert!(WorkItem::get(&db, &item.id).is_err());
    }

    #[test]
    fn test_list_with_filter() {
        let db = test_db();
        WorkItem::create(
            &db,
            CreateWorkItem {
                title: "A".into(),
                status: Some("todo".into()),
                assignee: Some("taylor".into()),
                ..Default::default()
            },
        )
        .unwrap();
        WorkItem::create(
            &db,
            CreateWorkItem {
                title: "B".into(),
                status: Some("done".into()),
                assignee: Some("james".into()),
                ..Default::default()
            },
        )
        .unwrap();

        let all = WorkItem::list(&db, &WorkItemFilter::default()).unwrap();
        assert_eq!(all.len(), 2);

        let taylor_items = WorkItem::list(
            &db,
            &WorkItemFilter {
                assignee: Some("taylor".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(taylor_items.len(), 1);
        assert_eq!(taylor_items[0].title, "A");
    }

    #[test]
    fn test_claim_complete_fail_escalate_workflows() {
        let db = test_db();
        let item = WorkItem::create(
            &db,
            CreateWorkItem {
                title: "Workflow".into(),
                ..Default::default()
            },
        )
        .unwrap();

        let claimed = WorkItem::claim(&db, &item.id, Some("reviewer-1".into())).unwrap();
        assert_eq!(claimed.assignee, "reviewer-1");
        assert_eq!(claimed.status, WorkItemStatus::Todo);

        let failed = WorkItem::fail(&db, &item.id).unwrap();
        assert_eq!(failed.status, WorkItemStatus::Blocked);

        let escalated = WorkItem::escalate(&db, &item.id).unwrap();
        assert!(escalated.priority.0 <= failed.priority.0);

        let completed = WorkItem::complete(&db, &item.id).unwrap();
        assert_eq!(completed.status, WorkItemStatus::Done);
    }
}
