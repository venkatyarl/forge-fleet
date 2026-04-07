//! Task group model and sequence-order workflow helpers.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::McDb;
use crate::error::{McError, McResult};
use crate::work_item::{UpdateWorkItem, WorkItem, WorkItemFilter};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskGroup {
    pub id: String,
    pub name: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTaskGroup {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateTaskGroup {
    pub name: Option<String>,
    pub description: Option<String>,
}

impl TaskGroup {
    pub fn create(db: &McDb, params: CreateTaskGroup) -> McResult<Self> {
        let now = Utc::now();
        let group = Self {
            id: Uuid::new_v4().to_string(),
            name: params.name,
            description: params.description,
            created_at: now,
            updated_at: now,
        };

        let conn = db.conn();
        conn.execute(
            "INSERT INTO task_groups (id, name, description, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                group.id,
                group.name,
                group.description,
                group.created_at.to_rfc3339(),
                group.updated_at.to_rfc3339(),
            ],
        )?;

        Ok(group)
    }

    pub fn get(db: &McDb, id: &str) -> McResult<Self> {
        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, description, created_at, updated_at FROM task_groups WHERE id = ?1",
        )?;

        stmt.query_row(rusqlite::params![id], |row| {
            let created_at: String = row.get(3)?;
            let updated_at: String = row.get(4)?;
            Ok(TaskGroup {
                id: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
                created_at: DateTime::parse_from_rfc3339(&created_at)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                updated_at: DateTime::parse_from_rfc3339(&updated_at)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
            })
        })
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => {
                McError::TaskGroupNotFound { id: id.to_string() }
            }
            other => McError::Database(other),
        })
    }

    pub fn list(db: &McDb) -> McResult<Vec<Self>> {
        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, description, created_at, updated_at FROM task_groups ORDER BY created_at DESC",
        )?;

        let rows = stmt
            .query_map([], |row| {
                let created_at: String = row.get(3)?;
                let updated_at: String = row.get(4)?;
                Ok(TaskGroup {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    description: row.get(2)?,
                    created_at: DateTime::parse_from_rfc3339(&created_at)
                        .map(|dt| dt.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                    updated_at: DateTime::parse_from_rfc3339(&updated_at)
                        .map(|dt| dt.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    pub fn update(db: &McDb, id: &str, params: UpdateTaskGroup) -> McResult<Self> {
        let mut group = Self::get(db, id)?;

        if let Some(name) = params.name {
            group.name = name;
        }
        if let Some(description) = params.description {
            group.description = description;
        }
        group.updated_at = Utc::now();

        let conn = db.conn();
        conn.execute(
            "UPDATE task_groups SET name = ?1, description = ?2, updated_at = ?3 WHERE id = ?4",
            rusqlite::params![
                group.name,
                group.description,
                group.updated_at.to_rfc3339(),
                group.id,
            ],
        )?;

        Ok(group)
    }

    pub fn delete(db: &McDb, id: &str) -> McResult<()> {
        // Ensure it exists first.
        let _ = Self::get(db, id)?;

        let conn = db.conn();
        conn.execute(
            "UPDATE work_items SET task_group_id = NULL, sequence_order = NULL WHERE task_group_id = ?1",
            rusqlite::params![id],
        )?;

        conn.execute(
            "DELETE FROM task_groups WHERE id = ?1",
            rusqlite::params![id],
        )?;
        Ok(())
    }

    pub fn assign_work_item(
        db: &McDb,
        task_group_id: &str,
        work_item_id: &str,
        sequence_order: Option<i32>,
    ) -> McResult<WorkItem> {
        let _ = Self::get(db, task_group_id)?;
        let _ = WorkItem::get(db, work_item_id)?;

        WorkItem::update(
            db,
            work_item_id,
            UpdateWorkItem {
                task_group_id: Some(Some(task_group_id.to_string())),
                sequence_order: Some(sequence_order),
                ..Default::default()
            },
        )
    }

    pub fn unassign_work_item(
        db: &McDb,
        task_group_id: &str,
        work_item_id: &str,
    ) -> McResult<WorkItem> {
        let _ = Self::get(db, task_group_id)?;
        let item = WorkItem::get(db, work_item_id)?;
        if item.task_group_id.as_deref() != Some(task_group_id) {
            return Err(McError::Other(anyhow::anyhow!(
                "work item is not assigned to task group"
            )));
        }

        WorkItem::update(
            db,
            work_item_id,
            UpdateWorkItem {
                task_group_id: Some(None),
                sequence_order: Some(None),
                ..Default::default()
            },
        )
    }

    pub fn list_items(db: &McDb, task_group_id: &str) -> McResult<Vec<WorkItem>> {
        let _ = Self::get(db, task_group_id)?;
        let mut items = WorkItem::list(
            db,
            &WorkItemFilter {
                task_group_id: Some(task_group_id.to_string()),
                ..Default::default()
            },
        )?;

        items.sort_by(|a, b| {
            let asq = a.sequence_order.unwrap_or(i32::MAX);
            let bsq = b.sequence_order.unwrap_or(i32::MAX);
            asq.cmp(&bsq)
                .then_with(|| a.priority.0.cmp(&b.priority.0))
                .then_with(|| a.updated_at.cmp(&b.updated_at))
        });

        Ok(items)
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
    fn test_task_group_assignments() {
        let db = test_db();

        let group = TaskGroup::create(
            &db,
            CreateTaskGroup {
                name: "Release sequencing".into(),
                description: "release runbook tasks".into(),
            },
        )
        .unwrap();

        let first = WorkItem::create(
            &db,
            CreateWorkItem {
                title: "Build".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let second = WorkItem::create(
            &db,
            CreateWorkItem {
                title: "Deploy".into(),
                ..Default::default()
            },
        )
        .unwrap();

        TaskGroup::assign_work_item(&db, &group.id, &second.id, Some(2)).unwrap();
        TaskGroup::assign_work_item(&db, &group.id, &first.id, Some(1)).unwrap();

        let items = TaskGroup::list_items(&db, &group.id).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "Build");
        assert_eq!(items[1].title, "Deploy");

        let unassigned = TaskGroup::unassign_work_item(&db, &group.id, &first.id).unwrap();
        assert!(unassigned.task_group_id.is_none());
    }
}
