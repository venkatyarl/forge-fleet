//! Sprint model and CRUD operations.
//!
//! Sprints are time-boxed iterations. Work items can be assigned to a sprint.
//! Velocity tracking measures how many items/points are completed per sprint.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::McDb;
use crate::error::{McError, McResult};
use crate::work_item::{WorkItem, WorkItemFilter, WorkItemStatus};

// ─── Sprint Model ────────────────────────────────────────────────────────────

/// A sprint — a time-boxed iteration of work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sprint {
    pub id: String,
    pub name: String,
    pub start_date: Option<NaiveDate>,
    pub end_date: Option<NaiveDate>,
    pub goal: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Sprint with velocity and burndown data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SprintWithStats {
    #[serde(flatten)]
    pub sprint: Sprint,
    pub total_items: usize,
    pub done_items: usize,
    pub in_progress_items: usize,
    pub blocked_items: usize,
    pub velocity: f64,
    pub work_item_ids: Vec<String>,
}

/// Burndown data point — one per day of the sprint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BurndownPoint {
    pub date: NaiveDate,
    pub ideal_remaining: f64,
    pub actual_remaining: usize,
}

/// Create parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSprint {
    pub name: String,
    #[serde(default)]
    pub start_date: Option<String>,
    #[serde(default)]
    pub end_date: Option<String>,
    #[serde(default)]
    pub goal: String,
}

/// Update parameters.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateSprint {
    pub name: Option<String>,
    pub start_date: Option<Option<String>>,
    pub end_date: Option<Option<String>>,
    pub goal: Option<String>,
}

// ─── CRUD ────────────────────────────────────────────────────────────────────

impl Sprint {
    /// Create a new sprint.
    pub fn create(db: &McDb, params: CreateSprint) -> McResult<Self> {
        let now = Utc::now();
        let start_date = params
            .start_date
            .as_deref()
            .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
        let end_date = params
            .end_date
            .as_deref()
            .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());

        let sprint = Self {
            id: Uuid::new_v4().to_string(),
            name: params.name,
            start_date,
            end_date,
            goal: params.goal,
            created_at: now,
            updated_at: now,
        };

        let conn = db.conn();
        conn.execute(
            "INSERT INTO sprints (id, name, start_date, end_date, goal, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                sprint.id,
                sprint.name,
                sprint.start_date.map(|d| d.to_string()),
                sprint.end_date.map(|d| d.to_string()),
                sprint.goal,
                sprint.created_at.to_rfc3339(),
                sprint.updated_at.to_rfc3339(),
            ],
        )?;

        Ok(sprint)
    }

    /// Get a sprint by ID.
    pub fn get(db: &McDb, id: &str) -> McResult<Self> {
        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, start_date, end_date, goal, created_at, updated_at FROM sprints WHERE id = ?1",
        )?;

        stmt.query_row(rusqlite::params![id], Self::from_row)
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    McError::SprintNotFound { id: id.to_string() }
                }
                other => McError::Database(other),
            })
    }

    /// List all sprints.
    pub fn list(db: &McDb) -> McResult<Vec<Self>> {
        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, start_date, end_date, goal, created_at, updated_at FROM sprints ORDER BY start_date DESC NULLS LAST",
        )?;
        let sprints = stmt
            .query_map([], Self::from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(sprints)
    }

    /// Update a sprint.
    pub fn update(db: &McDb, id: &str, params: UpdateSprint) -> McResult<Self> {
        let mut sprint = Self::get(db, id)?;

        if let Some(name) = params.name {
            sprint.name = name;
        }
        if let Some(start) = params.start_date {
            sprint.start_date = start.and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok());
        }
        if let Some(end) = params.end_date {
            sprint.end_date = end.and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok());
        }
        if let Some(goal) = params.goal {
            sprint.goal = goal;
        }

        sprint.updated_at = Utc::now();

        let conn = db.conn();
        conn.execute(
            "UPDATE sprints SET name=?1, start_date=?2, end_date=?3, goal=?4, updated_at=?5 WHERE id=?6",
            rusqlite::params![
                sprint.name,
                sprint.start_date.map(|d| d.to_string()),
                sprint.end_date.map(|d| d.to_string()),
                sprint.goal,
                sprint.updated_at.to_rfc3339(),
                sprint.id,
            ],
        )?;

        Ok(sprint)
    }

    /// Delete a sprint.
    pub fn delete(db: &McDb, id: &str) -> McResult<()> {
        let conn = db.conn();
        let affected = conn.execute("DELETE FROM sprints WHERE id = ?1", rusqlite::params![id])?;
        if affected == 0 {
            return Err(McError::SprintNotFound { id: id.to_string() });
        }
        Ok(())
    }

    /// Get sprint with computed stats from work items.
    pub fn with_stats(db: &McDb, id: &str) -> McResult<SprintWithStats> {
        let sprint = Self::get(db, id)?;
        let items = WorkItem::list(
            db,
            &WorkItemFilter {
                sprint_id: Some(id.to_string()),
                ..Default::default()
            },
        )?;

        let total = items.len();
        let done = items
            .iter()
            .filter(|i| i.status == WorkItemStatus::Done)
            .count();
        let in_progress = items
            .iter()
            .filter(|i| i.status == WorkItemStatus::InProgress)
            .count();
        let blocked = items
            .iter()
            .filter(|i| i.status == WorkItemStatus::Blocked)
            .count();

        // Velocity = done items / sprint duration in weeks (min 1 week)
        let weeks = match (sprint.start_date, sprint.end_date) {
            (Some(start), Some(end)) => {
                let days = (end - start).num_days().max(1);
                (days as f64 / 7.0).max(1.0)
            }
            _ => 1.0,
        };
        let velocity = done as f64 / weeks;
        let ids = items.iter().map(|i| i.id.clone()).collect();

        Ok(SprintWithStats {
            sprint,
            total_items: total,
            done_items: done,
            in_progress_items: in_progress,
            blocked_items: blocked,
            velocity,
            work_item_ids: ids,
        })
    }

    /// Generate burndown data for a sprint.
    /// This is a simplified version that computes ideal burndown only
    /// (actual burndown requires historical snapshots).
    pub fn burndown(db: &McDb, id: &str) -> McResult<Vec<BurndownPoint>> {
        let sprint = Self::get(db, id)?;
        let items = WorkItem::list(
            db,
            &WorkItemFilter {
                sprint_id: Some(id.to_string()),
                ..Default::default()
            },
        )?;

        let total = items.len();
        let done = items
            .iter()
            .filter(|i| i.status == WorkItemStatus::Done)
            .count();
        let remaining = total - done;

        let (start, end) = match (sprint.start_date, sprint.end_date) {
            (Some(s), Some(e)) => (s, e),
            _ => return Ok(Vec::new()),
        };

        let total_days = (end - start).num_days().max(1) as f64;
        let mut points = Vec::new();
        let mut current = start;

        while current <= end {
            let elapsed = (current - start).num_days() as f64;
            let ideal = total as f64 * (1.0 - elapsed / total_days);

            // Both branches produce the same value; simplified to unconditional.
            let actual = remaining;

            points.push(BurndownPoint {
                date: current,
                ideal_remaining: ideal,
                actual_remaining: actual,
            });

            current += chrono::Duration::days(1);
        }

        Ok(points)
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let start_str: Option<String> = row.get(2)?;
        let end_str: Option<String> = row.get(3)?;
        let created_str: String = row.get(5)?;
        let updated_str: String = row.get(6)?;

        Ok(Self {
            id: row.get(0)?,
            name: row.get(1)?,
            start_date: start_str.and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok()),
            end_date: end_str.and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok()),
            goal: row.get(4)?,
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
    fn test_sprint_crud() {
        let db = test_db();
        let sprint = Sprint::create(
            &db,
            CreateSprint {
                name: "Sprint 1".into(),
                start_date: Some("2026-04-01".into()),
                end_date: Some("2026-04-14".into()),
                goal: "Build core".into(),
            },
        )
        .unwrap();

        assert_eq!(sprint.name, "Sprint 1");
        assert!(sprint.start_date.is_some());

        let updated = Sprint::update(
            &db,
            &sprint.id,
            UpdateSprint {
                goal: Some("Build core + API".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(updated.goal, "Build core + API");

        let list = Sprint::list(&db).unwrap();
        assert_eq!(list.len(), 1);

        Sprint::delete(&db, &sprint.id).unwrap();
        assert!(Sprint::get(&db, &sprint.id).is_err());
    }

    #[test]
    fn test_sprint_with_stats() {
        let db = test_db();
        let sprint = Sprint::create(
            &db,
            CreateSprint {
                name: "Sprint 1".into(),
                start_date: Some("2026-04-01".into()),
                end_date: Some("2026-04-14".into()),
                goal: "Test".into(),
            },
        )
        .unwrap();

        WorkItem::create(
            &db,
            CreateWorkItem {
                title: "Done item".into(),
                status: Some("done".into()),
                sprint_id: Some(sprint.id.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        WorkItem::create(
            &db,
            CreateWorkItem {
                title: "WIP item".into(),
                status: Some("in_progress".into()),
                sprint_id: Some(sprint.id.clone()),
                ..Default::default()
            },
        )
        .unwrap();

        let stats = Sprint::with_stats(&db, &sprint.id).unwrap();
        assert_eq!(stats.total_items, 2);
        assert_eq!(stats.done_items, 1);
        assert_eq!(stats.in_progress_items, 1);
        assert!(stats.velocity > 0.0);
    }
}
