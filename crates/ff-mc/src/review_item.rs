//! Review checklist item model and workflow helpers.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::McDb;
use crate::error::{McError, McResult};
use crate::work_item::{UpdateWorkItem, WorkItem};

/// Review item status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewItemStatus {
    Pending,
    InProgress,
    Approved,
    ChangesRequested,
}

impl ReviewItemStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Approved => "approved",
            Self::ChangesRequested => "changes_requested",
        }
    }

    pub fn from_str_loose(s: &str) -> McResult<Self> {
        match s.to_lowercase().replace('-', "_").as_str() {
            "pending" => Ok(Self::Pending),
            "in_progress" | "in progress" | "inprogress" => Ok(Self::InProgress),
            "approved" | "done" => Ok(Self::Approved),
            "changes_requested" | "changes" | "needs_changes" => Ok(Self::ChangesRequested),
            other => Err(McError::InvalidStatus {
                value: other.to_string(),
            }),
        }
    }
}

impl std::fmt::Display for ReviewItemStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A checklist item tied to a work item review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewItem {
    pub id: String,
    pub work_item_id: String,
    pub title: String,
    pub status: ReviewItemStatus,
    pub reviewer: Option<String>,
    pub notes: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateReviewItem {
    pub title: String,
    #[serde(default)]
    pub reviewer: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateReviewItem {
    pub title: Option<String>,
    pub reviewer: Option<Option<String>>,
    pub notes: Option<Option<String>>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewSummary {
    pub work_item_id: String,
    pub total_items: usize,
    pub pending_items: usize,
    pub in_progress_items: usize,
    pub approved_items: usize,
    pub changes_requested_items: usize,
    pub all_approved: bool,
}

impl ReviewItem {
    pub fn create(db: &McDb, work_item_id: &str, params: CreateReviewItem) -> McResult<Self> {
        // Validate work item exists.
        let _ = WorkItem::get(db, work_item_id)?;

        let now = Utc::now();
        let status = match &params.status {
            Some(s) => ReviewItemStatus::from_str_loose(s)?,
            None => ReviewItemStatus::Pending,
        };

        let item = Self {
            id: Uuid::new_v4().to_string(),
            work_item_id: work_item_id.to_string(),
            title: params.title,
            status,
            reviewer: params.reviewer,
            notes: params.notes,
            created_at: now,
            updated_at: now,
        };

        let conn = db.conn();
        conn.execute(
            "INSERT INTO review_items (id, work_item_id, title, status, reviewer, notes, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                item.id,
                item.work_item_id,
                item.title,
                item.status.as_str(),
                item.reviewer,
                item.notes,
                item.created_at.to_rfc3339(),
                item.updated_at.to_rfc3339(),
            ],
        )?;

        Ok(item)
    }

    pub fn list_for_work_item(db: &McDb, work_item_id: &str) -> McResult<Vec<Self>> {
        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT id, work_item_id, title, status, reviewer, notes, created_at, updated_at
             FROM review_items WHERE work_item_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![work_item_id], Self::from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn get(db: &McDb, id: &str) -> McResult<Self> {
        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT id, work_item_id, title, status, reviewer, notes, created_at, updated_at
             FROM review_items WHERE id = ?1",
        )?;

        stmt.query_row(rusqlite::params![id], Self::from_row)
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    McError::ReviewItemNotFound { id: id.to_string() }
                }
                other => McError::Database(other),
            })
    }

    pub fn update(db: &McDb, id: &str, params: UpdateReviewItem) -> McResult<Self> {
        let mut item = Self::get(db, id)?;

        if let Some(title) = params.title {
            item.title = title;
        }
        if let Some(reviewer) = params.reviewer {
            item.reviewer = reviewer;
        }
        if let Some(notes) = params.notes {
            item.notes = notes;
        }
        if let Some(status) = params.status {
            item.status = ReviewItemStatus::from_str_loose(&status)?;
        }

        item.updated_at = Utc::now();

        let conn = db.conn();
        conn.execute(
            "UPDATE review_items SET title = ?1, status = ?2, reviewer = ?3, notes = ?4, updated_at = ?5 WHERE id = ?6",
            rusqlite::params![
                item.title,
                item.status.as_str(),
                item.reviewer,
                item.notes,
                item.updated_at.to_rfc3339(),
                item.id,
            ],
        )?;

        Ok(item)
    }

    pub fn delete(db: &McDb, id: &str) -> McResult<()> {
        let conn = db.conn();
        let changed = conn.execute(
            "DELETE FROM review_items WHERE id = ?1",
            rusqlite::params![id],
        )?;
        if changed == 0 {
            return Err(McError::ReviewItemNotFound { id: id.to_string() });
        }
        Ok(())
    }

    pub fn reset_for_work_item(db: &McDb, work_item_id: &str) -> McResult<usize> {
        // Validate work item exists.
        let _ = WorkItem::get(db, work_item_id)?;

        let conn = db.conn();
        let changed = conn.execute(
            "UPDATE review_items SET status = 'pending', updated_at = ?1 WHERE work_item_id = ?2",
            rusqlite::params![Utc::now().to_rfc3339(), work_item_id],
        )?;
        Ok(changed)
    }

    pub fn summary_for_work_item(db: &McDb, work_item_id: &str) -> McResult<ReviewSummary> {
        let items = Self::list_for_work_item(db, work_item_id)?;

        let total_items = items.len();
        let pending_items = items
            .iter()
            .filter(|i| i.status == ReviewItemStatus::Pending)
            .count();
        let in_progress_items = items
            .iter()
            .filter(|i| i.status == ReviewItemStatus::InProgress)
            .count();
        let approved_items = items
            .iter()
            .filter(|i| i.status == ReviewItemStatus::Approved)
            .count();
        let changes_requested_items = items
            .iter()
            .filter(|i| i.status == ReviewItemStatus::ChangesRequested)
            .count();

        Ok(ReviewSummary {
            work_item_id: work_item_id.to_string(),
            total_items,
            pending_items,
            in_progress_items,
            approved_items,
            changes_requested_items,
            all_approved: total_items > 0 && approved_items == total_items,
        })
    }

    /// Submit a work item into review state.
    pub fn submit_review(db: &McDb, work_item_id: &str) -> McResult<WorkItem> {
        WorkItem::update(
            db,
            work_item_id,
            UpdateWorkItem {
                status: Some("review".to_string()),
                ..Default::default()
            },
        )
    }

    /// Start review workflow.
    pub fn start_review(db: &McDb, work_item_id: &str) -> McResult<WorkItem> {
        WorkItem::update(
            db,
            work_item_id,
            UpdateWorkItem {
                status: Some("review".to_string()),
                ..Default::default()
            },
        )
    }

    /// Complete review workflow only when all checklist items are approved.
    pub fn complete_review(db: &McDb, work_item_id: &str) -> McResult<WorkItem> {
        let summary = Self::summary_for_work_item(db, work_item_id)?;
        if summary.total_items > 0 && !summary.all_approved {
            return Err(McError::Other(anyhow::anyhow!(
                "cannot complete review: checklist has non-approved items"
            )));
        }

        WorkItem::complete(db, work_item_id)
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let status_str: String = row.get(3)?;
        let status =
            ReviewItemStatus::from_str_loose(&status_str).unwrap_or(ReviewItemStatus::Pending);

        let created_at: String = row.get(6)?;
        let updated_at: String = row.get(7)?;

        Ok(Self {
            id: row.get(0)?,
            work_item_id: row.get(1)?,
            title: row.get(2)?,
            status,
            reviewer: row.get(4)?,
            notes: row.get(5)?,
            created_at: DateTime::parse_from_rfc3339(&created_at)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            updated_at: DateTime::parse_from_rfc3339(&updated_at)
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
    fn test_review_items_lifecycle() {
        let db = test_db();
        let wi = WorkItem::create(
            &db,
            CreateWorkItem {
                title: "Needs review".into(),
                ..Default::default()
            },
        )
        .unwrap();

        let item = ReviewItem::create(
            &db,
            &wi.id,
            CreateReviewItem {
                title: "Checklist 1".into(),
                reviewer: Some("alex".into()),
                notes: None,
                status: None,
            },
        )
        .unwrap();

        let list = ReviewItem::list_for_work_item(&db, &wi.id).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, item.id);

        let updated = ReviewItem::update(
            &db,
            &item.id,
            UpdateReviewItem {
                status: Some("approved".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(updated.status, ReviewItemStatus::Approved);

        let summary = ReviewItem::summary_for_work_item(&db, &wi.id).unwrap();
        assert_eq!(summary.approved_items, 1);
        assert!(summary.all_approved);
    }

    #[test]
    fn test_complete_review_requires_approval() {
        let db = test_db();
        let wi = WorkItem::create(
            &db,
            CreateWorkItem {
                title: "Review gate".into(),
                ..Default::default()
            },
        )
        .unwrap();

        ReviewItem::create(
            &db,
            &wi.id,
            CreateReviewItem {
                title: "Checklist 1".into(),
                reviewer: None,
                notes: None,
                status: Some("pending".into()),
            },
        )
        .unwrap();

        assert!(ReviewItem::complete_review(&db, &wi.id).is_err());

        let mut items = ReviewItem::list_for_work_item(&db, &wi.id).unwrap();
        let checklist = items.pop().unwrap();
        ReviewItem::update(
            &db,
            &checklist.id,
            UpdateReviewItem {
                status: Some("approved".into()),
                ..Default::default()
            },
        )
        .unwrap();

        let completed = ReviewItem::complete_review(&db, &wi.id).unwrap();
        assert_eq!(completed.status.as_str(), "done");
    }
}
