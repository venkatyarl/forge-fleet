//! Portfolio domain models for Mission Control.
//!
//! This module provides a concrete portfolio layer for operating model tracking:
//! - Companies
//! - Projects
//! - Project repositories
//! - Project environments
//! - Portfolio summary aggregation for dashboard consumption

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::McDb;
use crate::error::{McError, McResult};

// ─── Portfolio Status ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortfolioStatus {
    Proposed,
    Active,
    Paused,
    AtRisk,
    Completed,
    Archived,
}

impl PortfolioStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::AtRisk => "at_risk",
            Self::Completed => "completed",
            Self::Archived => "archived",
        }
    }

    pub fn from_str_loose(s: &str) -> McResult<Self> {
        match s.to_lowercase().replace('-', "_").as_str() {
            "proposed" => Ok(Self::Proposed),
            "active" => Ok(Self::Active),
            "paused" => Ok(Self::Paused),
            "at_risk" | "atrisk" | "at risk" => Ok(Self::AtRisk),
            "completed" | "done" => Ok(Self::Completed),
            "archived" => Ok(Self::Archived),
            other => Err(McError::InvalidStatus {
                value: other.to_string(),
            }),
        }
    }
}

impl std::fmt::Display for PortfolioStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ─── Portfolio Priority ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PortfolioPriority(pub i32);

impl PortfolioPriority {
    pub fn new(v: i32) -> McResult<Self> {
        if (1..=5).contains(&v) {
            Ok(Self(v))
        } else {
            Err(McError::InvalidPriority { value: v })
        }
    }
}

impl Default for PortfolioPriority {
    fn default() -> Self {
        Self(3)
    }
}

// ─── Operating Stage ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingStage {
    Discovery,
    Validation,
    Build,
    Launch,
    Growth,
    Sustain,
    Sunset,
}

impl OperatingStage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Discovery => "discovery",
            Self::Validation => "validation",
            Self::Build => "build",
            Self::Launch => "launch",
            Self::Growth => "growth",
            Self::Sustain => "sustain",
            Self::Sunset => "sunset",
        }
    }

    pub fn from_str_loose(s: &str) -> McResult<Self> {
        match s.to_lowercase().replace('-', "_").as_str() {
            "discovery" => Ok(Self::Discovery),
            "validation" => Ok(Self::Validation),
            "build" => Ok(Self::Build),
            "launch" => Ok(Self::Launch),
            "growth" | "scale" => Ok(Self::Growth),
            "sustain" | "steady" => Ok(Self::Sustain),
            "sunset" | "retire" => Ok(Self::Sunset),
            other => Err(McError::InvalidOperatingStage {
                value: other.to_string(),
            }),
        }
    }
}

impl std::fmt::Display for OperatingStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ─── Compliance Sensitivity ─────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComplianceSensitivity {
    Low,
    Moderate,
    High,
    Regulated,
}

impl ComplianceSensitivity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Moderate => "moderate",
            Self::High => "high",
            Self::Regulated => "regulated",
        }
    }

    pub fn from_str_loose(s: &str) -> McResult<Self> {
        match s.to_lowercase().replace('-', "_").as_str() {
            "low" => Ok(Self::Low),
            "moderate" | "medium" => Ok(Self::Moderate),
            "high" => Ok(Self::High),
            "regulated" | "critical" => Ok(Self::Regulated),
            other => Err(McError::InvalidComplianceSensitivity {
                value: other.to_string(),
            }),
        }
    }
}

impl std::fmt::Display for ComplianceSensitivity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ─── Company ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Company {
    pub id: String,
    pub name: String,
    pub business_unit: Option<String>,
    pub status: PortfolioStatus,
    pub priority: PortfolioPriority,
    pub owner: String,
    pub operating_stage: OperatingStage,
    pub compliance_sensitivity: ComplianceSensitivity,
    pub revenue_model_tags: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CreateCompany {
    pub name: String,
    #[serde(default)]
    pub business_unit: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub priority: Option<i32>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub operating_stage: Option<String>,
    #[serde(default)]
    pub compliance_sensitivity: Option<String>,
    #[serde(default)]
    pub revenue_model_tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateCompany {
    pub name: Option<String>,
    pub business_unit: Option<Option<String>>,
    pub status: Option<String>,
    pub priority: Option<i32>,
    pub owner: Option<String>,
    pub operating_stage: Option<String>,
    pub compliance_sensitivity: Option<String>,
    pub revenue_model_tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CompanyFilter {
    pub status: Option<PortfolioStatus>,
    pub owner: Option<String>,
    pub operating_stage: Option<OperatingStage>,
    pub compliance_sensitivity: Option<ComplianceSensitivity>,
    pub business_unit: Option<String>,
}

impl Company {
    pub fn create(db: &McDb, params: CreateCompany) -> McResult<Self> {
        let now = Utc::now();
        let status = params
            .status
            .as_deref()
            .map(PortfolioStatus::from_str_loose)
            .transpose()?
            .unwrap_or(PortfolioStatus::Active);
        let priority = params
            .priority
            .map(PortfolioPriority::new)
            .transpose()?
            .unwrap_or_default();
        let operating_stage = params
            .operating_stage
            .as_deref()
            .map(OperatingStage::from_str_loose)
            .transpose()?
            .unwrap_or(OperatingStage::Build);
        let compliance_sensitivity = params
            .compliance_sensitivity
            .as_deref()
            .map(ComplianceSensitivity::from_str_loose)
            .transpose()?
            .unwrap_or(ComplianceSensitivity::Moderate);

        let company = Self {
            id: Uuid::new_v4().to_string(),
            name: params.name,
            business_unit: params.business_unit,
            status,
            priority,
            owner: params.owner.unwrap_or_else(|| "unassigned".into()),
            operating_stage,
            compliance_sensitivity,
            revenue_model_tags: params.revenue_model_tags,
            created_at: now,
            updated_at: now,
        };

        let tags_json = serde_json::to_string(&company.revenue_model_tags)
            .map_err(|e| McError::Other(e.into()))?;

        let conn = db.conn();
        conn.execute(
            "INSERT INTO companies (id, name, business_unit, status, priority, owner, operating_stage, compliance_sensitivity, revenue_model_tags, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                company.id,
                company.name,
                company.business_unit,
                company.status.as_str(),
                company.priority.0,
                company.owner,
                company.operating_stage.as_str(),
                company.compliance_sensitivity.as_str(),
                tags_json,
                company.created_at.to_rfc3339(),
                company.updated_at.to_rfc3339(),
            ],
        )?;

        Ok(company)
    }

    pub fn get(db: &McDb, id: &str) -> McResult<Self> {
        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, business_unit, status, priority, owner, operating_stage, compliance_sensitivity, revenue_model_tags, created_at, updated_at
             FROM companies WHERE id = ?1",
        )?;

        stmt.query_row(rusqlite::params![id], Self::from_row)
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    McError::CompanyNotFound { id: id.to_string() }
                }
                other => McError::Database(other),
            })
    }

    pub fn list(db: &McDb, filter: &CompanyFilter) -> McResult<Vec<Self>> {
        let conn = db.conn();
        let mut sql = String::from(
            "SELECT id, name, business_unit, status, priority, owner, operating_stage, compliance_sensitivity, revenue_model_tags, created_at, updated_at FROM companies WHERE 1=1",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut param_idx = 1;

        if let Some(status) = &filter.status {
            sql.push_str(&format!(" AND status = ?{param_idx}"));
            params_vec.push(Box::new(status.as_str().to_string()));
            param_idx += 1;
        }
        if let Some(owner) = &filter.owner {
            sql.push_str(&format!(" AND owner = ?{param_idx}"));
            params_vec.push(Box::new(owner.clone()));
            param_idx += 1;
        }
        if let Some(stage) = &filter.operating_stage {
            sql.push_str(&format!(" AND operating_stage = ?{param_idx}"));
            params_vec.push(Box::new(stage.as_str().to_string()));
            param_idx += 1;
        }
        if let Some(comp) = &filter.compliance_sensitivity {
            sql.push_str(&format!(" AND compliance_sensitivity = ?{param_idx}"));
            params_vec.push(Box::new(comp.as_str().to_string()));
            param_idx += 1;
        }
        if let Some(unit) = &filter.business_unit {
            sql.push_str(&format!(" AND business_unit = ?{param_idx}"));
            params_vec.push(Box::new(unit.clone()));
            let _ = param_idx;
        }

        sql.push_str(" ORDER BY priority ASC, updated_at DESC");

        let mut stmt = conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();
        let rows = stmt
            .query_map(params_refs.as_slice(), Self::from_row)?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    pub fn update(db: &McDb, id: &str, params: UpdateCompany) -> McResult<Self> {
        let mut company = Self::get(db, id)?;

        if let Some(name) = params.name {
            company.name = name;
        }
        if let Some(business_unit) = params.business_unit {
            company.business_unit = business_unit;
        }
        if let Some(status) = params.status {
            company.status = PortfolioStatus::from_str_loose(&status)?;
        }
        if let Some(priority) = params.priority {
            company.priority = PortfolioPriority::new(priority)?;
        }
        if let Some(owner) = params.owner {
            company.owner = owner;
        }
        if let Some(stage) = params.operating_stage {
            company.operating_stage = OperatingStage::from_str_loose(&stage)?;
        }
        if let Some(comp) = params.compliance_sensitivity {
            company.compliance_sensitivity = ComplianceSensitivity::from_str_loose(&comp)?;
        }
        if let Some(tags) = params.revenue_model_tags {
            company.revenue_model_tags = tags;
        }

        company.updated_at = Utc::now();

        let tags_json = serde_json::to_string(&company.revenue_model_tags)
            .map_err(|e| McError::Other(e.into()))?;

        let conn = db.conn();
        conn.execute(
            "UPDATE companies
             SET name = ?1,
                 business_unit = ?2,
                 status = ?3,
                 priority = ?4,
                 owner = ?5,
                 operating_stage = ?6,
                 compliance_sensitivity = ?7,
                 revenue_model_tags = ?8,
                 updated_at = ?9
             WHERE id = ?10",
            rusqlite::params![
                company.name,
                company.business_unit,
                company.status.as_str(),
                company.priority.0,
                company.owner,
                company.operating_stage.as_str(),
                company.compliance_sensitivity.as_str(),
                tags_json,
                company.updated_at.to_rfc3339(),
                company.id,
            ],
        )?;

        Ok(company)
    }

    pub fn delete(db: &McDb, id: &str) -> McResult<()> {
        let conn = db.conn();
        let affected =
            conn.execute("DELETE FROM companies WHERE id = ?1", rusqlite::params![id])?;
        if affected == 0 {
            return Err(McError::CompanyNotFound { id: id.to_string() });
        }
        Ok(())
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let status_str: String = row.get(3)?;
        let stage_str: String = row.get(6)?;
        let compliance_str: String = row.get(7)?;
        let tags_json: String = row.get(8)?;
        let created_str: String = row.get(9)?;
        let updated_str: String = row.get(10)?;

        Ok(Self {
            id: row.get(0)?,
            name: row.get(1)?,
            business_unit: row.get(2)?,
            status: PortfolioStatus::from_str_loose(&status_str).unwrap_or(PortfolioStatus::Active),
            priority: PortfolioPriority(row.get(4)?),
            owner: row.get(5)?,
            operating_stage: OperatingStage::from_str_loose(&stage_str)
                .unwrap_or(OperatingStage::Build),
            compliance_sensitivity: ComplianceSensitivity::from_str_loose(&compliance_str)
                .unwrap_or(ComplianceSensitivity::Moderate),
            revenue_model_tags: serde_json::from_str(&tags_json).unwrap_or_default(),
            created_at: parse_ts(&created_str),
            updated_at: parse_ts(&updated_str),
        })
    }
}

// ─── Project ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub company_id: String,
    pub name: String,
    pub description: String,
    pub status: PortfolioStatus,
    pub priority: PortfolioPriority,
    pub owner: String,
    pub operating_stage: OperatingStage,
    pub compliance_sensitivity: ComplianceSensitivity,
    pub revenue_model_tags: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CreateProject {
    pub company_id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub priority: Option<i32>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub operating_stage: Option<String>,
    #[serde(default)]
    pub compliance_sensitivity: Option<String>,
    #[serde(default)]
    pub revenue_model_tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateProject {
    pub name: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    pub priority: Option<i32>,
    pub owner: Option<String>,
    pub operating_stage: Option<String>,
    pub compliance_sensitivity: Option<String>,
    pub revenue_model_tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectFilter {
    pub company_id: Option<String>,
    pub status: Option<PortfolioStatus>,
    pub owner: Option<String>,
    pub operating_stage: Option<OperatingStage>,
    pub compliance_sensitivity: Option<ComplianceSensitivity>,
}

impl Project {
    pub fn create(db: &McDb, params: CreateProject) -> McResult<Self> {
        Company::get(db, &params.company_id)?;

        let now = Utc::now();
        let status = params
            .status
            .as_deref()
            .map(PortfolioStatus::from_str_loose)
            .transpose()?
            .unwrap_or(PortfolioStatus::Active);
        let priority = params
            .priority
            .map(PortfolioPriority::new)
            .transpose()?
            .unwrap_or_default();
        let operating_stage = params
            .operating_stage
            .as_deref()
            .map(OperatingStage::from_str_loose)
            .transpose()?
            .unwrap_or(OperatingStage::Build);
        let compliance_sensitivity = params
            .compliance_sensitivity
            .as_deref()
            .map(ComplianceSensitivity::from_str_loose)
            .transpose()?
            .unwrap_or(ComplianceSensitivity::Moderate);

        let project = Self {
            id: Uuid::new_v4().to_string(),
            company_id: params.company_id,
            name: params.name,
            description: params.description,
            status,
            priority,
            owner: params.owner.unwrap_or_else(|| "unassigned".into()),
            operating_stage,
            compliance_sensitivity,
            revenue_model_tags: params.revenue_model_tags,
            created_at: now,
            updated_at: now,
        };

        let tags_json = serde_json::to_string(&project.revenue_model_tags)
            .map_err(|e| McError::Other(e.into()))?;

        let conn = db.conn();
        conn.execute(
            "INSERT INTO projects (id, company_id, name, description, status, priority, owner, operating_stage, compliance_sensitivity, revenue_model_tags, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                project.id,
                project.company_id,
                project.name,
                project.description,
                project.status.as_str(),
                project.priority.0,
                project.owner,
                project.operating_stage.as_str(),
                project.compliance_sensitivity.as_str(),
                tags_json,
                project.created_at.to_rfc3339(),
                project.updated_at.to_rfc3339(),
            ],
        )?;

        Ok(project)
    }

    pub fn get(db: &McDb, id: &str) -> McResult<Self> {
        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT id, company_id, name, description, status, priority, owner, operating_stage, compliance_sensitivity, revenue_model_tags, created_at, updated_at
             FROM projects WHERE id = ?1",
        )?;

        stmt.query_row(rusqlite::params![id], Self::from_row)
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    McError::ProjectNotFound { id: id.to_string() }
                }
                other => McError::Database(other),
            })
    }

    pub fn list(db: &McDb, filter: &ProjectFilter) -> McResult<Vec<Self>> {
        let conn = db.conn();
        let mut sql = String::from(
            "SELECT id, company_id, name, description, status, priority, owner, operating_stage, compliance_sensitivity, revenue_model_tags, created_at, updated_at FROM projects WHERE 1=1",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut param_idx = 1;

        if let Some(company_id) = &filter.company_id {
            sql.push_str(&format!(" AND company_id = ?{param_idx}"));
            params_vec.push(Box::new(company_id.clone()));
            param_idx += 1;
        }
        if let Some(status) = &filter.status {
            sql.push_str(&format!(" AND status = ?{param_idx}"));
            params_vec.push(Box::new(status.as_str().to_string()));
            param_idx += 1;
        }
        if let Some(owner) = &filter.owner {
            sql.push_str(&format!(" AND owner = ?{param_idx}"));
            params_vec.push(Box::new(owner.clone()));
            param_idx += 1;
        }
        if let Some(stage) = &filter.operating_stage {
            sql.push_str(&format!(" AND operating_stage = ?{param_idx}"));
            params_vec.push(Box::new(stage.as_str().to_string()));
            param_idx += 1;
        }
        if let Some(comp) = &filter.compliance_sensitivity {
            sql.push_str(&format!(" AND compliance_sensitivity = ?{param_idx}"));
            params_vec.push(Box::new(comp.as_str().to_string()));
            let _ = param_idx;
        }

        sql.push_str(" ORDER BY priority ASC, updated_at DESC");

        let mut stmt = conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();
        let rows = stmt
            .query_map(params_refs.as_slice(), Self::from_row)?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    pub fn update(db: &McDb, id: &str, params: UpdateProject) -> McResult<Self> {
        let mut project = Self::get(db, id)?;

        if let Some(name) = params.name {
            project.name = name;
        }
        if let Some(description) = params.description {
            project.description = description;
        }
        if let Some(status) = params.status {
            project.status = PortfolioStatus::from_str_loose(&status)?;
        }
        if let Some(priority) = params.priority {
            project.priority = PortfolioPriority::new(priority)?;
        }
        if let Some(owner) = params.owner {
            project.owner = owner;
        }
        if let Some(stage) = params.operating_stage {
            project.operating_stage = OperatingStage::from_str_loose(&stage)?;
        }
        if let Some(comp) = params.compliance_sensitivity {
            project.compliance_sensitivity = ComplianceSensitivity::from_str_loose(&comp)?;
        }
        if let Some(tags) = params.revenue_model_tags {
            project.revenue_model_tags = tags;
        }

        project.updated_at = Utc::now();

        let tags_json = serde_json::to_string(&project.revenue_model_tags)
            .map_err(|e| McError::Other(e.into()))?;

        let conn = db.conn();
        conn.execute(
            "UPDATE projects
             SET name = ?1,
                 description = ?2,
                 status = ?3,
                 priority = ?4,
                 owner = ?5,
                 operating_stage = ?6,
                 compliance_sensitivity = ?7,
                 revenue_model_tags = ?8,
                 updated_at = ?9
             WHERE id = ?10",
            rusqlite::params![
                project.name,
                project.description,
                project.status.as_str(),
                project.priority.0,
                project.owner,
                project.operating_stage.as_str(),
                project.compliance_sensitivity.as_str(),
                tags_json,
                project.updated_at.to_rfc3339(),
                project.id,
            ],
        )?;

        Ok(project)
    }

    pub fn delete(db: &McDb, id: &str) -> McResult<()> {
        let conn = db.conn();
        let affected = conn.execute("DELETE FROM projects WHERE id = ?1", rusqlite::params![id])?;
        if affected == 0 {
            return Err(McError::ProjectNotFound { id: id.to_string() });
        }
        Ok(())
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let status_str: String = row.get(4)?;
        let stage_str: String = row.get(7)?;
        let compliance_str: String = row.get(8)?;
        let tags_json: String = row.get(9)?;
        let created_str: String = row.get(10)?;
        let updated_str: String = row.get(11)?;

        Ok(Self {
            id: row.get(0)?,
            company_id: row.get(1)?,
            name: row.get(2)?,
            description: row.get(3)?,
            status: PortfolioStatus::from_str_loose(&status_str).unwrap_or(PortfolioStatus::Active),
            priority: PortfolioPriority(row.get(5)?),
            owner: row.get(6)?,
            operating_stage: OperatingStage::from_str_loose(&stage_str)
                .unwrap_or(OperatingStage::Build),
            compliance_sensitivity: ComplianceSensitivity::from_str_loose(&compliance_str)
                .unwrap_or(ComplianceSensitivity::Moderate),
            revenue_model_tags: serde_json::from_str(&tags_json).unwrap_or_default(),
            created_at: parse_ts(&created_str),
            updated_at: parse_ts(&updated_str),
        })
    }
}

// ─── Project Repo ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRepo {
    pub id: String,
    pub project_id: String,
    pub repository_url: String,
    pub provider: String,
    pub default_branch: String,
    pub status: PortfolioStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CreateProjectRepo {
    pub repository_url: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateProjectRepo {
    pub repository_url: Option<String>,
    pub provider: Option<String>,
    pub default_branch: Option<String>,
    pub status: Option<String>,
}

impl ProjectRepo {
    pub fn create(db: &McDb, project_id: &str, params: CreateProjectRepo) -> McResult<Self> {
        Project::get(db, project_id)?;

        let now = Utc::now();
        let status = params
            .status
            .as_deref()
            .map(PortfolioStatus::from_str_loose)
            .transpose()?
            .unwrap_or(PortfolioStatus::Active);

        let repo = Self {
            id: Uuid::new_v4().to_string(),
            project_id: project_id.to_string(),
            repository_url: params.repository_url,
            provider: params.provider.unwrap_or_else(|| "github".into()),
            default_branch: params.default_branch.unwrap_or_else(|| "main".into()),
            status,
            created_at: now,
            updated_at: now,
        };

        let conn = db.conn();
        conn.execute(
            "INSERT INTO project_repos (id, project_id, repository_url, provider, default_branch, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                repo.id,
                repo.project_id,
                repo.repository_url,
                repo.provider,
                repo.default_branch,
                repo.status.as_str(),
                repo.created_at.to_rfc3339(),
                repo.updated_at.to_rfc3339(),
            ],
        )?;

        Ok(repo)
    }

    pub fn get(db: &McDb, id: &str) -> McResult<Self> {
        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT id, project_id, repository_url, provider, default_branch, status, created_at, updated_at
             FROM project_repos WHERE id = ?1",
        )?;

        stmt.query_row(rusqlite::params![id], Self::from_row)
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    McError::ProjectRepoNotFound { id: id.to_string() }
                }
                other => McError::Database(other),
            })
    }

    pub fn list_for_project(db: &McDb, project_id: &str) -> McResult<Vec<Self>> {
        Project::get(db, project_id)?;

        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT id, project_id, repository_url, provider, default_branch, status, created_at, updated_at
             FROM project_repos WHERE project_id = ?1 ORDER BY updated_at DESC",
        )?;

        let rows = stmt
            .query_map(rusqlite::params![project_id], Self::from_row)?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    pub fn update(db: &McDb, id: &str, params: UpdateProjectRepo) -> McResult<Self> {
        let mut repo = Self::get(db, id)?;

        if let Some(repository_url) = params.repository_url {
            repo.repository_url = repository_url;
        }
        if let Some(provider) = params.provider {
            repo.provider = provider;
        }
        if let Some(default_branch) = params.default_branch {
            repo.default_branch = default_branch;
        }
        if let Some(status) = params.status {
            repo.status = PortfolioStatus::from_str_loose(&status)?;
        }

        repo.updated_at = Utc::now();

        let conn = db.conn();
        conn.execute(
            "UPDATE project_repos
             SET repository_url = ?1,
                 provider = ?2,
                 default_branch = ?3,
                 status = ?4,
                 updated_at = ?5
             WHERE id = ?6",
            rusqlite::params![
                repo.repository_url,
                repo.provider,
                repo.default_branch,
                repo.status.as_str(),
                repo.updated_at.to_rfc3339(),
                repo.id,
            ],
        )?;

        Ok(repo)
    }

    pub fn delete(db: &McDb, id: &str) -> McResult<()> {
        let conn = db.conn();
        let affected = conn.execute(
            "DELETE FROM project_repos WHERE id = ?1",
            rusqlite::params![id],
        )?;
        if affected == 0 {
            return Err(McError::ProjectRepoNotFound { id: id.to_string() });
        }
        Ok(())
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let status_str: String = row.get(5)?;
        let created_str: String = row.get(6)?;
        let updated_str: String = row.get(7)?;

        Ok(Self {
            id: row.get(0)?,
            project_id: row.get(1)?,
            repository_url: row.get(2)?,
            provider: row.get(3)?,
            default_branch: row.get(4)?,
            status: PortfolioStatus::from_str_loose(&status_str).unwrap_or(PortfolioStatus::Active),
            created_at: parse_ts(&created_str),
            updated_at: parse_ts(&updated_str),
        })
    }
}

// ─── Project Environment ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEnvironment {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub environment_type: String,
    pub status: PortfolioStatus,
    pub owner: String,
    pub endpoint_url: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CreateProjectEnvironment {
    pub name: String,
    #[serde(default)]
    pub environment_type: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub endpoint_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateProjectEnvironment {
    pub name: Option<String>,
    pub environment_type: Option<String>,
    pub status: Option<String>,
    pub owner: Option<String>,
    pub endpoint_url: Option<Option<String>>,
}

impl ProjectEnvironment {
    pub fn create(db: &McDb, project_id: &str, params: CreateProjectEnvironment) -> McResult<Self> {
        Project::get(db, project_id)?;

        let now = Utc::now();
        let status = params
            .status
            .as_deref()
            .map(PortfolioStatus::from_str_loose)
            .transpose()?
            .unwrap_or(PortfolioStatus::Active);

        let env = Self {
            id: Uuid::new_v4().to_string(),
            project_id: project_id.to_string(),
            name: params.name,
            environment_type: params.environment_type.unwrap_or_else(|| "runtime".into()),
            status,
            owner: params.owner.unwrap_or_else(|| "unassigned".into()),
            endpoint_url: params.endpoint_url,
            created_at: now,
            updated_at: now,
        };

        let conn = db.conn();
        conn.execute(
            "INSERT INTO project_environments (id, project_id, name, environment_type, status, owner, endpoint_url, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                env.id,
                env.project_id,
                env.name,
                env.environment_type,
                env.status.as_str(),
                env.owner,
                env.endpoint_url,
                env.created_at.to_rfc3339(),
                env.updated_at.to_rfc3339(),
            ],
        )?;

        Ok(env)
    }

    pub fn get(db: &McDb, id: &str) -> McResult<Self> {
        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT id, project_id, name, environment_type, status, owner, endpoint_url, created_at, updated_at
             FROM project_environments WHERE id = ?1",
        )?;

        stmt.query_row(rusqlite::params![id], Self::from_row)
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    McError::ProjectEnvironmentNotFound { id: id.to_string() }
                }
                other => McError::Database(other),
            })
    }

    pub fn list_for_project(db: &McDb, project_id: &str) -> McResult<Vec<Self>> {
        Project::get(db, project_id)?;

        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT id, project_id, name, environment_type, status, owner, endpoint_url, created_at, updated_at
             FROM project_environments WHERE project_id = ?1 ORDER BY updated_at DESC",
        )?;

        let rows = stmt
            .query_map(rusqlite::params![project_id], Self::from_row)?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    pub fn update(db: &McDb, id: &str, params: UpdateProjectEnvironment) -> McResult<Self> {
        let mut env = Self::get(db, id)?;

        if let Some(name) = params.name {
            env.name = name;
        }
        if let Some(environment_type) = params.environment_type {
            env.environment_type = environment_type;
        }
        if let Some(status) = params.status {
            env.status = PortfolioStatus::from_str_loose(&status)?;
        }
        if let Some(owner) = params.owner {
            env.owner = owner;
        }
        if let Some(endpoint_url) = params.endpoint_url {
            env.endpoint_url = endpoint_url;
        }

        env.updated_at = Utc::now();

        let conn = db.conn();
        conn.execute(
            "UPDATE project_environments
             SET name = ?1,
                 environment_type = ?2,
                 status = ?3,
                 owner = ?4,
                 endpoint_url = ?5,
                 updated_at = ?6
             WHERE id = ?7",
            rusqlite::params![
                env.name,
                env.environment_type,
                env.status.as_str(),
                env.owner,
                env.endpoint_url,
                env.updated_at.to_rfc3339(),
                env.id,
            ],
        )?;

        Ok(env)
    }

    pub fn delete(db: &McDb, id: &str) -> McResult<()> {
        let conn = db.conn();
        let affected = conn.execute(
            "DELETE FROM project_environments WHERE id = ?1",
            rusqlite::params![id],
        )?;
        if affected == 0 {
            return Err(McError::ProjectEnvironmentNotFound { id: id.to_string() });
        }
        Ok(())
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let status_str: String = row.get(4)?;
        let created_str: String = row.get(7)?;
        let updated_str: String = row.get(8)?;

        Ok(Self {
            id: row.get(0)?,
            project_id: row.get(1)?,
            name: row.get(2)?,
            environment_type: row.get(3)?,
            status: PortfolioStatus::from_str_loose(&status_str).unwrap_or(PortfolioStatus::Active),
            owner: row.get(5)?,
            endpoint_url: row.get(6)?,
            created_at: parse_ts(&created_str),
            updated_at: parse_ts(&updated_str),
        })
    }
}

// ─── Portfolio Summary ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortfolioSummary {
    pub total_companies: i64,
    pub total_projects: i64,
    pub active_projects: i64,
    pub projects_by_status: HashMap<String, i64>,
    pub projects_by_operating_stage: HashMap<String, i64>,
    pub projects_by_compliance_sensitivity: HashMap<String, i64>,
    pub projects_by_owner: HashMap<String, i64>,
    pub revenue_model_tag_counts: HashMap<String, i64>,
}

impl PortfolioSummary {
    pub fn compute(db: &McDb) -> McResult<Self> {
        let conn = db.conn();

        let total_companies: i64 =
            conn.query_row("SELECT COUNT(*) FROM companies", [], |row| row.get(0))?;
        let total_projects: i64 =
            conn.query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))?;
        let active_projects: i64 = conn.query_row(
            "SELECT COUNT(*) FROM projects WHERE status = 'active'",
            [],
            |row| row.get(0),
        )?;

        let projects_by_status = count_map(
            &conn,
            "SELECT status, COUNT(*) FROM projects GROUP BY status",
        )?;
        let projects_by_operating_stage = count_map(
            &conn,
            "SELECT operating_stage, COUNT(*) FROM projects GROUP BY operating_stage",
        )?;
        let projects_by_compliance_sensitivity = count_map(
            &conn,
            "SELECT compliance_sensitivity, COUNT(*) FROM projects GROUP BY compliance_sensitivity",
        )?;
        let projects_by_owner =
            count_map(&conn, "SELECT owner, COUNT(*) FROM projects GROUP BY owner")?;

        let mut revenue_model_tag_counts: HashMap<String, i64> = HashMap::new();
        let mut stmt = conn.prepare("SELECT revenue_model_tags FROM projects")?;
        let tags_rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for tags_json in tags_rows {
            let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
            for tag in tags {
                *revenue_model_tag_counts.entry(tag).or_insert(0) += 1;
            }
        }

        Ok(Self {
            total_companies,
            total_projects,
            active_projects,
            projects_by_status,
            projects_by_operating_stage,
            projects_by_compliance_sensitivity,
            projects_by_owner,
            revenue_model_tag_counts,
        })
    }
}

fn count_map(conn: &rusqlite::Connection, sql: &str) -> McResult<HashMap<String, i64>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map([], |row| {
            let key: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((key, count))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows.into_iter().collect())
}

fn parse_ts(ts: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> McDb {
        McDb::in_memory().unwrap()
    }

    #[test]
    fn company_create_list_update_flow() {
        let db = test_db();

        let company = Company::create(
            &db,
            CreateCompany {
                name: "Forge Holdings".into(),
                business_unit: Some("platform".into()),
                owner: Some("taylor".into()),
                operating_stage: Some("growth".into()),
                compliance_sensitivity: Some("high".into()),
                revenue_model_tags: vec!["subscription".into(), "usage".into()],
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(company.status, PortfolioStatus::Active);
        assert_eq!(company.priority.0, 3);
        assert_eq!(company.owner, "taylor");

        let listed = Company::list(
            &db,
            &CompanyFilter {
                owner: Some("taylor".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(listed.len(), 1);

        let updated = Company::update(
            &db,
            &company.id,
            UpdateCompany {
                status: Some("paused".into()),
                priority: Some(2),
                business_unit: Some(None),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(updated.status, PortfolioStatus::Paused);
        assert_eq!(updated.priority.0, 2);
        assert_eq!(updated.business_unit, None);
    }

    #[test]
    fn project_create_list_update_flow() {
        let db = test_db();
        let company = Company::create(
            &db,
            CreateCompany {
                name: "FleetCo".into(),
                ..Default::default()
            },
        )
        .unwrap();

        let project = Project::create(
            &db,
            CreateProject {
                company_id: company.id.clone(),
                name: "ForgeFleet Core".into(),
                description: "Core control plane".into(),
                owner: Some("venkat".into()),
                status: Some("active".into()),
                priority: Some(1),
                operating_stage: Some("launch".into()),
                compliance_sensitivity: Some("regulated".into()),
                revenue_model_tags: vec!["enterprise".into()],
            },
        )
        .unwrap();

        let listed = Project::list(
            &db,
            &ProjectFilter {
                company_id: Some(company.id.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, project.id);

        let updated = Project::update(
            &db,
            &project.id,
            UpdateProject {
                status: Some("at_risk".into()),
                owner: Some("ops".into()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(updated.status, PortfolioStatus::AtRisk);
        assert_eq!(updated.owner, "ops");
    }

    #[test]
    fn repo_and_environment_create_list_update_flow() {
        let db = test_db();
        let company = Company::create(
            &db,
            CreateCompany {
                name: "FleetOps".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let project = Project::create(
            &db,
            CreateProject {
                company_id: company.id,
                name: "Gateway".into(),
                ..Default::default()
            },
        )
        .unwrap();

        let repo = ProjectRepo::create(
            &db,
            &project.id,
            CreateProjectRepo {
                repository_url: "https://github.com/taylor-oclaw/forge-fleet".into(),
                provider: Some("github".into()),
                default_branch: Some("main".into()),
                status: Some("active".into()),
            },
        )
        .unwrap();
        let repos = ProjectRepo::list_for_project(&db, &project.id).unwrap();
        assert_eq!(repos.len(), 1);

        let repo_updated = ProjectRepo::update(
            &db,
            &repo.id,
            UpdateProjectRepo {
                default_branch: Some("stable".into()),
                status: Some("paused".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(repo_updated.default_branch, "stable");
        assert_eq!(repo_updated.status, PortfolioStatus::Paused);

        let env = ProjectEnvironment::create(
            &db,
            &project.id,
            CreateProjectEnvironment {
                name: "production".into(),
                environment_type: Some("kubernetes".into()),
                owner: Some("platform".into()),
                endpoint_url: Some("https://fleet.example.com".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let envs = ProjectEnvironment::list_for_project(&db, &project.id).unwrap();
        assert_eq!(envs.len(), 1);

        let env_updated = ProjectEnvironment::update(
            &db,
            &env.id,
            UpdateProjectEnvironment {
                status: Some("at_risk".into()),
                owner: Some("sre".into()),
                endpoint_url: Some(None),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(env_updated.status, PortfolioStatus::AtRisk);
        assert_eq!(env_updated.owner, "sre");
        assert_eq!(env_updated.endpoint_url, None);
    }

    #[test]
    fn portfolio_summary_counts() {
        let db = test_db();
        let a = Company::create(
            &db,
            CreateCompany {
                name: "A".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let b = Company::create(
            &db,
            CreateCompany {
                name: "B".into(),
                ..Default::default()
            },
        )
        .unwrap();

        Project::create(
            &db,
            CreateProject {
                company_id: a.id,
                name: "P1".into(),
                status: Some("active".into()),
                operating_stage: Some("launch".into()),
                compliance_sensitivity: Some("high".into()),
                owner: Some("alice".into()),
                revenue_model_tags: vec!["subscription".into()],
                ..Default::default()
            },
        )
        .unwrap();
        Project::create(
            &db,
            CreateProject {
                company_id: b.id,
                name: "P2".into(),
                status: Some("paused".into()),
                operating_stage: Some("build".into()),
                compliance_sensitivity: Some("regulated".into()),
                owner: Some("bob".into()),
                revenue_model_tags: vec!["usage".into(), "subscription".into()],
                ..Default::default()
            },
        )
        .unwrap();

        let summary = PortfolioSummary::compute(&db).unwrap();
        assert_eq!(summary.total_companies, 2);
        assert_eq!(summary.total_projects, 2);
        assert_eq!(summary.active_projects, 1);
        assert_eq!(summary.projects_by_status.get("active"), Some(&1));
        assert_eq!(summary.projects_by_status.get("paused"), Some(&1));
        assert_eq!(
            summary.revenue_model_tag_counts.get("subscription"),
            Some(&2)
        );
    }
}
