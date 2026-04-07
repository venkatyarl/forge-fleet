//! Mission Control error types.

/// Errors specific to the Mission Control crate.
#[derive(Debug, thiserror::Error)]
pub enum McError {
    #[error("work item not found: {id}")]
    WorkItemNotFound { id: String },

    #[error("epic not found: {id}")]
    EpicNotFound { id: String },

    #[error("sprint not found: {id}")]
    SprintNotFound { id: String },

    #[error("review item not found: {id}")]
    ReviewItemNotFound { id: String },

    #[error("task group not found: {id}")]
    TaskGroupNotFound { id: String },

    #[error("company not found: {id}")]
    CompanyNotFound { id: String },

    #[error("project not found: {id}")]
    ProjectNotFound { id: String },

    #[error("project repo not found: {id}")]
    ProjectRepoNotFound { id: String },

    #[error("project environment not found: {id}")]
    ProjectEnvironmentNotFound { id: String },

    #[error("legal entity not found: {id}")]
    LegalEntityNotFound { id: String },

    #[error("compliance obligation not found: {id}")]
    ComplianceObligationNotFound { id: String },

    #[error("filing not found: {id}")]
    FilingNotFound { id: String },

    #[error("invalid status: {value}")]
    InvalidStatus { value: String },

    #[error("invalid priority: {value} (must be 1-5)")]
    InvalidPriority { value: i32 },

    #[error("invalid operating stage: {value}")]
    InvalidOperatingStage { value: String },

    #[error("invalid compliance sensitivity: {value}")]
    InvalidComplianceSensitivity { value: String },

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

/// Convenience result alias.
pub type McResult<T> = std::result::Result<T, McError>;
