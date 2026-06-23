//! Epic model and data types.
//!
//! An epic groups related work items under a single theme or feature.
//! Progress is computed dynamically from the status of associated work items.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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

    pub fn from_str_loose(s: &str) -> Result<Self, String> {
        match s.to_lowercase().replace('-', "_").as_str() {
            "open" => Ok(Self::Open),
            "in_progress" | "inprogress" | "in progress" => Ok(Self::InProgress),
            "done" | "complete" | "completed" => Ok(Self::Done),
            "cancelled" | "canceled" => Ok(Self::Cancelled),
            other => Err(format!("invalid epic status: {}", other)),
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
