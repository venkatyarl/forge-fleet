//! Work-item dependency graph persistence and checks.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::db::McDb;
use crate::error::{McError, McResult};
use crate::work_item::{WorkItem, WorkItemStatus};

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

impl WorkItemDependency {
    pub fn add(db: &McDb, work_item_id: &str, depends_on_id: &str) -> McResult<Self> {
        if work_item_id == depends_on_id {
            return Err(McError::Other(anyhow::anyhow!(
                "work item cannot depend on itself"
            )));
        }

        // Validate both work items exist.
        let _ = WorkItem::get(db, work_item_id)?;
        let _ = WorkItem::get(db, depends_on_id)?;

        let created_at = Utc::now();
        let conn = db.conn();
        conn.execute(
            "INSERT OR IGNORE INTO work_item_dependencies (work_item_id, depends_on_id, created_at)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![work_item_id, depends_on_id, created_at.to_rfc3339()],
        )?;

        Ok(Self {
            work_item_id: work_item_id.to_string(),
            depends_on_id: depends_on_id.to_string(),
            created_at,
        })
    }

    pub fn remove(db: &McDb, work_item_id: &str, depends_on_id: &str) -> McResult<bool> {
        let conn = db.conn();
        let changed = conn.execute(
            "DELETE FROM work_item_dependencies WHERE work_item_id = ?1 AND depends_on_id = ?2",
            rusqlite::params![work_item_id, depends_on_id],
        )?;
        Ok(changed > 0)
    }

    pub fn list_for_work_item(db: &McDb, work_item_id: &str) -> McResult<Vec<Self>> {
        // Validate primary work item exists.
        let _ = WorkItem::get(db, work_item_id)?;

        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT work_item_id, depends_on_id, created_at
             FROM work_item_dependencies
             WHERE work_item_id = ?1
             ORDER BY created_at ASC",
        )?;

        let deps = stmt
            .query_map(rusqlite::params![work_item_id], |row| {
                let created_at: String = row.get(2)?;
                Ok(WorkItemDependency {
                    work_item_id: row.get(0)?,
                    depends_on_id: row.get(1)?,
                    created_at: DateTime::parse_from_rfc3339(&created_at)
                        .map(|dt| dt.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(deps)
    }

    /// Return blockers for a work item where dependency status != done.
    pub fn check(db: &McDb, work_item_id: &str) -> McResult<DependencyCheck> {
        // Validate work item exists.
        let _ = WorkItem::get(db, work_item_id)?;

        let deps = Self::list_for_work_item(db, work_item_id)?;
        let mut blocked_by_ids = Vec::new();

        for dep in deps {
            let dep_item = WorkItem::get(db, &dep.depends_on_id)?;
            if dep_item.status != WorkItemStatus::Done {
                blocked_by_ids.push(dep_item.id);
            }
        }

        let blocked_count = blocked_by_ids.len();
        Ok(DependencyCheck {
            work_item_id: work_item_id.to_string(),
            blocked_by_ids,
            blocked_count,
            can_start: blocked_count == 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_item::{CreateWorkItem, UpdateWorkItem};

    fn test_db() -> McDb {
        McDb::in_memory().unwrap()
    }

    #[test]
    fn test_dependency_check_blocks_until_done() {
        let db = test_db();

        let upstream = WorkItem::create(
            &db,
            CreateWorkItem {
                title: "Upstream".into(),
                ..Default::default()
            },
        )
        .unwrap();

        let downstream = WorkItem::create(
            &db,
            CreateWorkItem {
                title: "Downstream".into(),
                ..Default::default()
            },
        )
        .unwrap();

        WorkItemDependency::add(&db, &downstream.id, &upstream.id).unwrap();

        let blocked = WorkItemDependency::check(&db, &downstream.id).unwrap();
        assert!(!blocked.can_start);
        assert_eq!(blocked.blocked_count, 1);

        WorkItem::update(
            &db,
            &upstream.id,
            UpdateWorkItem {
                status: Some("done".into()),
                ..Default::default()
            },
        )
        .unwrap();

        let clear = WorkItemDependency::check(&db, &downstream.id).unwrap();
        assert!(clear.can_start);
        assert_eq!(clear.blocked_count, 0);
    }

    #[test]
    fn test_remove_dependency() {
        let db = test_db();

        let a = WorkItem::create(
            &db,
            CreateWorkItem {
                title: "A".into(),
                ..Default::default()
            },
        )
        .unwrap();

        let b = WorkItem::create(
            &db,
            CreateWorkItem {
                title: "B".into(),
                ..Default::default()
            },
        )
        .unwrap();

        WorkItemDependency::add(&db, &b.id, &a.id).unwrap();
        assert!(WorkItemDependency::remove(&db, &b.id, &a.id).unwrap());
        assert!(!WorkItemDependency::remove(&db, &b.id, &a.id).unwrap());
    }
}
