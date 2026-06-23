//! Review checklist item model and workflow helpers.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{McError, McResult};

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
