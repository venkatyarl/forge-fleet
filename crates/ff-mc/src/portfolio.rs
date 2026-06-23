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
