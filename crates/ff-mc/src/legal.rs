//! Legal/compliance entities and filing deadlines for Mission Control.
//!
//! This module adds a lightweight but concrete legal operations layer:
//! - legal entities (LLCs, C-Corps, etc.)
//! - compliance obligations (annual reports, franchise tax, etc.)
//! - filings with due dates and status

use chrono::{DateTime, Duration, NaiveDate, Utc};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::McDb;
use crate::error::{McError, McResult};

// ─── Helpers ────────────────────────────────────────────────────────────────

fn parse_datetime_utc(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn parse_date(value: &str) -> NaiveDate {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap_or_else(|_| Utc::now().date_naive())
}

fn parse_optional_date(value: Option<String>) -> Option<NaiveDate> {
    value.and_then(|raw| NaiveDate::parse_from_str(&raw, "%Y-%m-%d").ok())
}

fn normalize_status(raw: Option<String>, default: &str) -> String {
    raw.unwrap_or_else(|| default.to_string())
        .trim()
        .to_lowercase()
        .replace(' ', "_")
}

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

impl LegalEntity {
    pub fn create(db: &McDb, params: CreateLegalEntity) -> McResult<Self> {
        let now = Utc::now();
        let entity = Self {
            id: Uuid::new_v4().to_string(),
            name: params.name,
            entity_type: params.entity_type,
            jurisdiction: params.jurisdiction,
            registration_number: params.registration_number,
            status: normalize_status(params.status, "active"),
            created_at: now,
            updated_at: now,
        };

        let conn = db.conn();
        conn.execute(
            "INSERT INTO legal_entities (id, name, entity_type, jurisdiction, registration_number, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                entity.id,
                entity.name,
                entity.entity_type,
                entity.jurisdiction,
                entity.registration_number,
                entity.status,
                entity.created_at.to_rfc3339(),
                entity.updated_at.to_rfc3339(),
            ],
        )?;

        Ok(entity)
    }

    pub fn get(db: &McDb, id: &str) -> McResult<Self> {
        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, entity_type, jurisdiction, registration_number, status, created_at, updated_at
             FROM legal_entities WHERE id = ?1",
        )?;

        stmt.query_row(params![id], Self::from_row)
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    McError::LegalEntityNotFound { id: id.into() }
                }
                other => McError::Database(other),
            })
    }

    pub fn list(db: &McDb) -> McResult<Vec<Self>> {
        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, entity_type, jurisdiction, registration_number, status, created_at, updated_at
             FROM legal_entities ORDER BY created_at DESC",
        )?;

        let entities = stmt
            .query_map([], Self::from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(entities)
    }

    pub fn update(db: &McDb, id: &str, params: UpdateLegalEntity) -> McResult<Self> {
        let mut entity = Self::get(db, id)?;

        if let Some(name) = params.name {
            entity.name = name;
        }
        if let Some(entity_type) = params.entity_type {
            entity.entity_type = entity_type;
        }
        if let Some(jurisdiction) = params.jurisdiction {
            entity.jurisdiction = jurisdiction;
        }
        if let Some(registration_number) = params.registration_number {
            entity.registration_number = registration_number;
        }
        if let Some(status) = params.status {
            entity.status = normalize_status(Some(status), "active");
        }

        entity.updated_at = Utc::now();

        let conn = db.conn();
        conn.execute(
            "UPDATE legal_entities
             SET name = ?1, entity_type = ?2, jurisdiction = ?3, registration_number = ?4, status = ?5, updated_at = ?6
             WHERE id = ?7",
            params![
                entity.name,
                entity.entity_type,
                entity.jurisdiction,
                entity.registration_number,
                entity.status,
                entity.updated_at.to_rfc3339(),
                entity.id,
            ],
        )?;

        Ok(entity)
    }

    pub fn delete(db: &McDb, id: &str) -> McResult<()> {
        let conn = db.conn();
        let affected = conn.execute("DELETE FROM legal_entities WHERE id = ?1", params![id])?;
        if affected == 0 {
            return Err(McError::LegalEntityNotFound { id: id.into() });
        }
        Ok(())
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let created_at: String = row.get(6)?;
        let updated_at: String = row.get(7)?;

        Ok(Self {
            id: row.get(0)?,
            name: row.get(1)?,
            entity_type: row.get(2)?,
            jurisdiction: row.get(3)?,
            registration_number: row.get(4)?,
            status: row.get(5)?,
            created_at: parse_datetime_utc(&created_at),
            updated_at: parse_datetime_utc(&updated_at),
        })
    }
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

impl ComplianceObligation {
    pub fn create(db: &McDb, params: CreateComplianceObligation) -> McResult<Self> {
        let now = Utc::now();
        let obligation = Self {
            id: Uuid::new_v4().to_string(),
            entity_id: params.entity_id,
            title: params.title,
            description: params.description,
            jurisdiction: params.jurisdiction,
            frequency: params.frequency.unwrap_or_else(|| "annual".to_string()),
            status: normalize_status(params.status, "active"),
            created_at: now,
            updated_at: now,
        };

        let conn = db.conn();
        conn.execute(
            "INSERT INTO compliance_obligations
             (id, entity_id, title, description, jurisdiction, frequency, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                obligation.id,
                obligation.entity_id,
                obligation.title,
                obligation.description,
                obligation.jurisdiction,
                obligation.frequency,
                obligation.status,
                obligation.created_at.to_rfc3339(),
                obligation.updated_at.to_rfc3339(),
            ],
        )?;

        Ok(obligation)
    }

    pub fn get(db: &McDb, id: &str) -> McResult<Self> {
        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, title, description, jurisdiction, frequency, status, created_at, updated_at
             FROM compliance_obligations WHERE id = ?1",
        )?;

        stmt.query_row(params![id], Self::from_row)
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    McError::ComplianceObligationNotFound { id: id.into() }
                }
                other => McError::Database(other),
            })
    }

    pub fn list(db: &McDb, entity_id: Option<&str>) -> McResult<Vec<Self>> {
        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, title, description, jurisdiction, frequency, status, created_at, updated_at
             FROM compliance_obligations
             WHERE (?1 IS NULL OR entity_id = ?1)
             ORDER BY created_at DESC",
        )?;

        let obligations = stmt
            .query_map(params![entity_id], Self::from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(obligations)
    }

    pub fn update(db: &McDb, id: &str, params: UpdateComplianceObligation) -> McResult<Self> {
        let mut obligation = Self::get(db, id)?;

        if let Some(title) = params.title {
            obligation.title = title;
        }
        if let Some(description) = params.description {
            obligation.description = description;
        }
        if let Some(jurisdiction) = params.jurisdiction {
            obligation.jurisdiction = jurisdiction;
        }
        if let Some(frequency) = params.frequency {
            obligation.frequency = frequency;
        }
        if let Some(status) = params.status {
            obligation.status = normalize_status(Some(status), "active");
        }

        obligation.updated_at = Utc::now();

        let conn = db.conn();
        conn.execute(
            "UPDATE compliance_obligations
             SET title = ?1, description = ?2, jurisdiction = ?3, frequency = ?4, status = ?5, updated_at = ?6
             WHERE id = ?7",
            params![
                obligation.title,
                obligation.description,
                obligation.jurisdiction,
                obligation.frequency,
                obligation.status,
                obligation.updated_at.to_rfc3339(),
                obligation.id,
            ],
        )?;

        Ok(obligation)
    }

    pub fn delete(db: &McDb, id: &str) -> McResult<()> {
        let conn = db.conn();
        let affected = conn.execute(
            "DELETE FROM compliance_obligations WHERE id = ?1",
            params![id],
        )?;
        if affected == 0 {
            return Err(McError::ComplianceObligationNotFound { id: id.into() });
        }
        Ok(())
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let created_at: String = row.get(7)?;
        let updated_at: String = row.get(8)?;

        Ok(Self {
            id: row.get(0)?,
            entity_id: row.get(1)?,
            title: row.get(2)?,
            description: row.get(3)?,
            jurisdiction: row.get(4)?,
            frequency: row.get(5)?,
            status: row.get(6)?,
            created_at: parse_datetime_utc(&created_at),
            updated_at: parse_datetime_utc(&updated_at),
        })
    }
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

impl Filing {
    pub fn create(db: &McDb, params: CreateFiling) -> McResult<Self> {
        let now = Utc::now();
        let status = match params.status {
            Some(raw) => FilingStatus::from_str_loose(&raw)?,
            None => FilingStatus::Pending,
        };

        let filing = Self {
            id: Uuid::new_v4().to_string(),
            entity_id: params.entity_id,
            obligation_id: params.obligation_id,
            jurisdiction: params.jurisdiction,
            due_date: params.due_date,
            status,
            filed_on: params.filed_on,
            notes: params.notes,
            created_at: now,
            updated_at: now,
        };

        let conn = db.conn();
        conn.execute(
            "INSERT INTO filings
             (id, entity_id, obligation_id, jurisdiction, due_date, status, filed_on, notes, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                filing.id,
                filing.entity_id,
                filing.obligation_id,
                filing.jurisdiction,
                filing.due_date.format("%Y-%m-%d").to_string(),
                filing.status.as_str(),
                filing.filed_on.map(|d| d.format("%Y-%m-%d").to_string()),
                filing.notes,
                filing.created_at.to_rfc3339(),
                filing.updated_at.to_rfc3339(),
            ],
        )?;

        Ok(filing)
    }

    pub fn get(db: &McDb, id: &str) -> McResult<Self> {
        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, obligation_id, jurisdiction, due_date, status, filed_on, notes, created_at, updated_at
             FROM filings WHERE id = ?1",
        )?;

        stmt.query_row(params![id], |row| Self::from_row_with_offset(row, 0))
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => McError::FilingNotFound { id: id.into() },
                other => McError::Database(other),
            })
    }

    pub fn list(db: &McDb, filter: &FilingFilter) -> McResult<Vec<Self>> {
        let conn = db.conn();
        let status: Option<String> = filter.status.map(|s| s.as_str().to_string());
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, obligation_id, jurisdiction, due_date, status, filed_on, notes, created_at, updated_at
             FROM filings
             WHERE (?1 IS NULL OR entity_id = ?1)
               AND (?2 IS NULL OR obligation_id = ?2)
               AND (?3 IS NULL OR status = ?3)
             ORDER BY due_date ASC, created_at DESC",
        )?;

        let filings = stmt
            .query_map(
                params![
                    filter.entity_id.as_deref(),
                    filter.obligation_id.as_deref(),
                    status.as_deref()
                ],
                |row| Self::from_row_with_offset(row, 0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(filings)
    }

    pub fn update(db: &McDb, id: &str, params: UpdateFiling) -> McResult<Self> {
        let mut filing = Self::get(db, id)?;

        if let Some(obligation_id) = params.obligation_id {
            filing.obligation_id = obligation_id;
        }
        if let Some(jurisdiction) = params.jurisdiction {
            filing.jurisdiction = jurisdiction;
        }
        if let Some(due_date) = params.due_date {
            filing.due_date = due_date;
        }
        if let Some(status) = params.status {
            filing.status = FilingStatus::from_str_loose(&status)?;
        }
        if let Some(filed_on) = params.filed_on {
            filing.filed_on = filed_on;
        }
        if let Some(notes) = params.notes {
            filing.notes = notes;
        }

        filing.updated_at = Utc::now();

        let conn = db.conn();
        conn.execute(
            "UPDATE filings
             SET obligation_id = ?1, jurisdiction = ?2, due_date = ?3, status = ?4, filed_on = ?5, notes = ?6, updated_at = ?7
             WHERE id = ?8",
            params![
                filing.obligation_id,
                filing.jurisdiction,
                filing.due_date.format("%Y-%m-%d").to_string(),
                filing.status.as_str(),
                filing.filed_on.map(|d| d.format("%Y-%m-%d").to_string()),
                filing.notes,
                filing.updated_at.to_rfc3339(),
                filing.id,
            ],
        )?;

        Ok(filing)
    }

    pub fn delete(db: &McDb, id: &str) -> McResult<()> {
        let conn = db.conn();
        let affected = conn.execute("DELETE FROM filings WHERE id = ?1", params![id])?;
        if affected == 0 {
            return Err(McError::FilingNotFound { id: id.into() });
        }
        Ok(())
    }

    pub fn due_within_days(db: &McDb, days: i64) -> McResult<Vec<FilingDueItem>> {
        let days = days.max(0);
        let today = Utc::now().date_naive();
        let end_date = today + Duration::days(days);

        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT
                f.id,
                f.entity_id,
                f.obligation_id,
                f.jurisdiction,
                f.due_date,
                f.status,
                f.filed_on,
                f.notes,
                f.created_at,
                f.updated_at,
                le.name,
                co.title
             FROM filings f
             INNER JOIN legal_entities le ON le.id = f.entity_id
             LEFT JOIN compliance_obligations co ON co.id = f.obligation_id
             WHERE f.due_date >= ?1
               AND f.due_date <= ?2
               AND f.status != 'filed'
             ORDER BY f.due_date ASC, f.created_at ASC",
        )?;

        let results = stmt
            .query_map(
                params![
                    today.format("%Y-%m-%d").to_string(),
                    end_date.format("%Y-%m-%d").to_string()
                ],
                |row| {
                    let filing = Self::from_row_with_offset(row, 0)?;
                    let entity_name: String = row.get(10)?;
                    let obligation_title: Option<String> = row.get(11)?;
                    let days_until_due = filing.due_date.signed_duration_since(today).num_days();

                    Ok(FilingDueItem {
                        filing,
                        entity_name,
                        obligation_title,
                        days_until_due,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(results)
    }

    fn from_row_with_offset(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<Self> {
        let due_date_raw: String = row.get(offset + 4)?;
        let status_raw: String = row.get(offset + 5)?;
        let filed_on_raw: Option<String> = row.get(offset + 6)?;
        let created_at_raw: String = row.get(offset + 8)?;
        let updated_at_raw: String = row.get(offset + 9)?;

        let status = FilingStatus::from_str_loose(&status_raw).unwrap_or(FilingStatus::Pending);

        Ok(Self {
            id: row.get(offset)?,
            entity_id: row.get(offset + 1)?,
            obligation_id: row.get(offset + 2)?,
            jurisdiction: row.get(offset + 3)?,
            due_date: parse_date(&due_date_raw),
            status,
            filed_on: parse_optional_date(filed_on_raw),
            notes: row.get(offset + 7)?,
            created_at: parse_datetime_utc(&created_at_raw),
            updated_at: parse_datetime_utc(&updated_at_raw),
        })
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> McDb {
        McDb::in_memory().expect("create in-memory mc db")
    }

    #[test]
    fn create_and_list_legal_entities() {
        let db = test_db();

        let created = LegalEntity::create(
            &db,
            CreateLegalEntity {
                name: "ForgeFleet Holdings".into(),
                entity_type: "llc".into(),
                jurisdiction: "DE".into(),
                registration_number: Some("DE-12345".into()),
                status: None,
            },
        )
        .unwrap();

        assert_eq!(created.status, "active");

        let listed = LegalEntity::list(&db).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, created.id);
        assert_eq!(listed[0].jurisdiction, "DE");
    }

    #[test]
    fn create_and_list_obligations() {
        let db = test_db();

        let entity = LegalEntity::create(
            &db,
            CreateLegalEntity {
                name: "ForgeFleet Ops".into(),
                entity_type: "c_corp".into(),
                jurisdiction: "TX".into(),
                registration_number: None,
                status: Some("active".into()),
            },
        )
        .unwrap();

        let obligation = ComplianceObligation::create(
            &db,
            CreateComplianceObligation {
                entity_id: entity.id.clone(),
                title: "Annual Franchise Tax".into(),
                description: "State franchise tax return".into(),
                jurisdiction: "TX".into(),
                frequency: Some("annual".into()),
                status: None,
            },
        )
        .unwrap();

        let listed = ComplianceObligation::list(&db, Some(&entity.id)).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, obligation.id);
        assert_eq!(listed[0].title, "Annual Franchise Tax");
    }

    #[test]
    fn create_and_list_filings() {
        let db = test_db();

        let entity = LegalEntity::create(
            &db,
            CreateLegalEntity {
                name: "ForgeFleet Labs".into(),
                entity_type: "llc".into(),
                jurisdiction: "FL".into(),
                registration_number: None,
                status: None,
            },
        )
        .unwrap();

        let obligation = ComplianceObligation::create(
            &db,
            CreateComplianceObligation {
                entity_id: entity.id.clone(),
                title: "Annual Report".into(),
                description: String::new(),
                jurisdiction: "FL".into(),
                frequency: Some("annual".into()),
                status: None,
            },
        )
        .unwrap();

        let due_date = Utc::now().date_naive() + Duration::days(12);
        let filing = Filing::create(
            &db,
            CreateFiling {
                entity_id: entity.id.clone(),
                obligation_id: Some(obligation.id.clone()),
                jurisdiction: "FL".into(),
                due_date,
                status: Some("pending".into()),
                filed_on: None,
                notes: Some("Needs board approval".into()),
            },
        )
        .unwrap();

        let listed = Filing::list(
            &db,
            &FilingFilter {
                entity_id: Some(entity.id.clone()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, filing.id);
        assert_eq!(listed[0].due_date, due_date);
        assert_eq!(listed[0].status, FilingStatus::Pending);
    }

    #[test]
    fn due_soon_query_only_returns_upcoming_unfiled_items() {
        let db = test_db();

        let entity = LegalEntity::create(
            &db,
            CreateLegalEntity {
                name: "ForgeFleet Legal".into(),
                entity_type: "llc".into(),
                jurisdiction: "DE".into(),
                registration_number: None,
                status: None,
            },
        )
        .unwrap();

        let obligation = ComplianceObligation::create(
            &db,
            CreateComplianceObligation {
                entity_id: entity.id.clone(),
                title: "Delaware Annual Report".into(),
                description: String::new(),
                jurisdiction: "DE".into(),
                frequency: Some("annual".into()),
                status: None,
            },
        )
        .unwrap();

        let today = Utc::now().date_naive();

        // Included: upcoming and pending.
        Filing::create(
            &db,
            CreateFiling {
                entity_id: entity.id.clone(),
                obligation_id: Some(obligation.id.clone()),
                jurisdiction: "DE".into(),
                due_date: today + Duration::days(9),
                status: Some("pending".into()),
                filed_on: None,
                notes: None,
            },
        )
        .unwrap();

        // Excluded: outside 30 day window.
        Filing::create(
            &db,
            CreateFiling {
                entity_id: entity.id.clone(),
                obligation_id: Some(obligation.id.clone()),
                jurisdiction: "DE".into(),
                due_date: today + Duration::days(45),
                status: Some("pending".into()),
                filed_on: None,
                notes: None,
            },
        )
        .unwrap();

        // Excluded: already filed.
        Filing::create(
            &db,
            CreateFiling {
                entity_id: entity.id,
                obligation_id: Some(obligation.id),
                jurisdiction: "DE".into(),
                due_date: today + Duration::days(5),
                status: Some("filed".into()),
                filed_on: Some(today),
                notes: None,
            },
        )
        .unwrap();

        let due = Filing::due_within_days(&db, 30).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].filing.status, FilingStatus::Pending);
        assert!(due[0].days_until_due >= 0);
        assert_eq!(
            due[0].obligation_title.as_deref(),
            Some("Delaware Annual Report")
        );
    }
}
