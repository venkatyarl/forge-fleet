//! Legal/compliance entities and filing deadlines for Mission Control.
//!
//! This module adds a lightweight but concrete legal operations layer:
//! - legal entities (LLCs, C-Corps, etc.)
//! - compliance obligations (annual reports, franchise tax, etc.)
//! - filings with due dates and status

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{McError, McResult};

// ─── Legal Entities ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegalEntity {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub jurisdiction: String,
    pub registration_number: Option<String>,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateLegalEntity {
    pub name: String,
    pub entity_type: String,
    pub jurisdiction: String,
    #[serde(default)]
    pub registration_number: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateLegalEntity {
    pub name: Option<String>,
    pub entity_type: Option<String>,
    pub jurisdiction: Option<String>,
    pub registration_number: Option<Option<String>>,
    pub status: Option<String>,
}

// ─── Compliance Obligations ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComplianceObligation {
    pub id: String,
    pub entity_id: String,
    pub title: String,
    pub description: String,
    pub jurisdiction: String,
    pub frequency: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateComplianceObligation {
    pub entity_id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub jurisdiction: String,
    #[serde(default)]
    pub frequency: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateComplianceObligation {
    pub title: Option<String>,
    pub description: Option<String>,
    pub jurisdiction: Option<String>,
    pub frequency: Option<String>,
    pub status: Option<String>,
}

// ─── Filings ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilingStatus {
    Pending,
    Filed,
    Overdue,
    Waived,
}

impl FilingStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Filed => "filed",
            Self::Overdue => "overdue",
            Self::Waived => "waived",
        }
    }

    pub fn from_str_loose(value: &str) -> McResult<Self> {
        match value.trim().to_lowercase().replace('-', "_").as_str() {
            "pending" => Ok(Self::Pending),
            "filed" | "complete" | "completed" => Ok(Self::Filed),
            "overdue" => Ok(Self::Overdue),
            "waived" => Ok(Self::Waived),
            other => Err(McError::InvalidStatus {
                value: other.to_string(),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Filing {
    pub id: String,
    pub entity_id: String,
    pub obligation_id: Option<String>,
    pub jurisdiction: String,
    pub due_date: NaiveDate,
    pub status: FilingStatus,
    pub filed_on: Option<NaiveDate>,
    pub notes: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateFiling {
    pub entity_id: String,
    #[serde(default)]
    pub obligation_id: Option<String>,
    pub jurisdiction: String,
    pub due_date: NaiveDate,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub filed_on: Option<NaiveDate>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateFiling {
    pub obligation_id: Option<Option<String>>,
    pub jurisdiction: Option<String>,
    pub due_date: Option<NaiveDate>,
    pub status: Option<String>,
    pub filed_on: Option<Option<NaiveDate>>,
    pub notes: Option<Option<String>>,
}

#[derive(Debug, Clone, Default)]
pub struct FilingFilter {
    pub entity_id: Option<String>,
    pub obligation_id: Option<String>,
    pub status: Option<FilingStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilingDueItem {
    #[serde(flatten)]
    pub filing: Filing,
    pub entity_name: String,
    pub obligation_title: Option<String>,
    pub days_until_due: i64,
}

// ─── Tests ──────────────────────────────────────────────────────────────────
