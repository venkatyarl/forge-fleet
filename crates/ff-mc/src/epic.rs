//! Epic model and CRUD operations.
//!
//! An epic groups related work items under a single theme or feature.
//! Progress is computed dynamically from the status of associated work items.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::McDb;
use crate::error::{McError, McResult};
use crate::work_item::{WorkItem, WorkItemFilter, WorkItemStatus};

// ─── Epic Status ─────────────────────────────────────────────────────────────

/// Status of an epic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpicStatus {
    Open,
    InProgress,
    Done,
    Cancelled,
}

impl EpicStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::InProgress => "in_progress",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn from_str_loose(s: &str) -> McResult<Self> {
        match s.to_lowercase().replace('-', "_").as_str() {
            "open" => Ok(Self::Open),
            "in_progress" | "inprogress" | "in progress" => Ok(Self::InProgress),
            "done" | "complete" | "completed" => Ok(Self::Done),
            "cancelled" | "canceled" => Ok(Self::Cancelled),
            other => Err(McError::InvalidStatus {
                value: other.to_string(),
            }),
        }
    }
}

impl std::fmt::Display for EpicStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ─── Epic Model ──────────────────────────────────────────────────────────────

/// An epic — a collection of related work items.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Epic {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: EpicStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Epic with computed progress data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpicWithProgress {
    #[serde(flatten)]
    pub epic: Epic,
    pub total_items: usize,
    pub done_items: usize,
    pub progress_pct: f64,
    pub work_item_ids: Vec<String>,
}

/// Create parameters for an epic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateEpic {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub status: Option<String>,
}

/// Update parameters for an epic.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateEpic {
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
}

// ─── CRUD ────────────────────────────────────────────────────────────────────

impl Epic {
    /// Create a new epic.
    pub fn create(db: &McDb, params: CreateEpic) -> McResult<Self> {
        let now = Utc::now();
        let status = match &params.status {
            Some(s) => EpicStatus::from_str_loose(s)?,
            None => EpicStatus::Open,
        };

        let epic = Self {
            id: Uuid::new_v4().to_string(),
            title: params.title,
            description: params.description,
            status,
            created_at: now,
            updated_at: now,
        };

        let conn = db.conn();
        conn.execute(
            "INSERT INTO epics (id, title, description, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                epic.id,
                epic.title,
                epic.description,
                epic.status.as_str(),
                epic.created_at.to_rfc3339(),
                epic.updated_at.to_rfc3339(),
            ],
        )?;

        Ok(epic)
    }

    /// Get an epic by ID.
    pub fn get(db: &McDb, id: &str) -> McResult<Self> {
        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT id, title, description, status, created_at, updated_at FROM epics WHERE id = ?1",
        )?;

        stmt.query_row(rusqlite::params![id], Self::from_row)
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    McError::EpicNotFound { id: id.to_string() }
                }
                other => McError::Database(other),
            })
    }

    /// List all epics.
    pub fn list(db: &McDb) -> McResult<Vec<Self>> {
        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT id, title, description, status, created_at, updated_at FROM epics ORDER BY created_at DESC",
        )?;
        let epics = stmt
            .query_map([], Self::from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(epics)
    }

    /// Update an epic.
    pub fn update(db: &McDb, id: &str, params: UpdateEpic) -> McResult<Self> {
        let mut epic = Self::get(db, id)?;

        if let Some(title) = params.title {
            epic.title = title;
        }
        if let Some(desc) = params.description {
            epic.description = desc;
        }
        if let Some(status_str) = params.status {
            epic.status = EpicStatus::from_str_loose(&status_str)?;
        }

        epic.updated_at = Utc::now();

        let conn = db.conn();
        conn.execute(
            "UPDATE epics SET title=?1, description=?2, status=?3, updated_at=?4 WHERE id=?5",
            rusqlite::params![
                epic.title,
                epic.description,
                epic.status.as_str(),
                epic.updated_at.to_rfc3339(),
                epic.id,
            ],
        )?;

        Ok(epic)
    }

    /// Delete an epic by ID.
    pub fn delete(db: &McDb, id: &str) -> McResult<()> {
        let conn = db.conn();
        let affected = conn.execute("DELETE FROM epics WHERE id = ?1", rusqlite::params![id])?;
        if affected == 0 {
            return Err(McError::EpicNotFound { id: id.to_string() });
        }
        Ok(())
    }

    /// Get an epic with computed progress from its work items.
    pub fn with_progress(db: &McDb, id: &str) -> McResult<EpicWithProgress> {
        let epic = Self::get(db, id)?;
        let items = WorkItem::list(
            db,
            &WorkItemFilter {
                epic_id: Some(id.to_string()),
                ..Default::default()
            },
        )?;

        let total = items.len();
        let done = items
            .iter()
            .filter(|i| i.status == WorkItemStatus::Done)
            .count();
        let pct = if total > 0 {
            (done as f64 / total as f64) * 100.0
        } else {
            0.0
        };
        let ids = items.iter().map(|i| i.id.clone()).collect();

        Ok(EpicWithProgress {
            epic,
            total_items: total,
            done_items: done,
            progress_pct: pct,
            work_item_ids: ids,
        })
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let status_str: String = row.get(3)?;
        let status = EpicStatus::from_str_loose(&status_str).unwrap_or(EpicStatus::Open);
        let created_str: String = row.get(4)?;
        let updated_str: String = row.get(5)?;

        Ok(Self {
            id: row.get(0)?,
            title: row.get(1)?,
            description: row.get(2)?,
            status,
            created_at: DateTime::parse_from_rfc3339(&created_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            updated_at: DateTime::parse_from_rfc3339(&updated_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
        })
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
    fn test_epic_crud() {
        let db = test_db();
        let epic = Epic::create(
            &db,
            CreateEpic {
                title: "ForgeFleet v1".into(),
                description: "Build the fleet".into(),
                status: None,
            },
        )
        .unwrap();
        assert_eq!(epic.status, EpicStatus::Open);

        let updated = Epic::update(
            &db,
            &epic.id,
            UpdateEpic {
                status: Some("in_progress".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(updated.status, EpicStatus::InProgress);

        let list = Epic::list(&db).unwrap();
        assert_eq!(list.len(), 1);

        Epic::delete(&db, &epic.id).unwrap();
        assert!(Epic::get(&db, &epic.id).is_err());
    }

    #[test]
    fn test_epic_progress() {
        let db = test_db();
        let epic = Epic::create(
            &db,
            CreateEpic {
                title: "Progress test".into(),
                description: String::new(),
                status: None,
            },
        )
        .unwrap();

        // Add 3 work items: 1 done, 1 in_progress, 1 backlog
        WorkItem::create(
            &db,
            CreateWorkItem {
                title: "Done task".into(),
                status: Some("done".into()),
                epic_id: Some(epic.id.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        WorkItem::create(
            &db,
            CreateWorkItem {
                title: "In progress task".into(),
                status: Some("in_progress".into()),
                epic_id: Some(epic.id.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        WorkItem::create(
            &db,
            CreateWorkItem {
                title: "Backlog task".into(),
                epic_id: Some(epic.id.clone()),
                ..Default::default()
            },
        )
        .unwrap();

        let prog = Epic::with_progress(&db, &epic.id).unwrap();
        assert_eq!(prog.total_items, 3);
        assert_eq!(prog.done_items, 1);
        assert!((prog.progress_pct - 33.333).abs() < 1.0);
    }
}
