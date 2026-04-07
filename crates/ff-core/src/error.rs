//! ForgeFleet error types.
//!
//! Central error enum with `thiserror` for all ff-core operations.

use std::path::PathBuf;

/// The primary error type for all ForgeFleet core operations.
#[derive(Debug, thiserror::Error)]
pub enum ForgeFleetError {
    // ── Config ───────────────────────────────────────────────────────
    #[error("configuration error: {0}")]
    Config(String),

    #[error("configuration file not found: {}", path.display())]
    ConfigNotFound { path: PathBuf },

    #[error("TOML parse error: {0}")]
    TomlParse(#[from] toml::de::Error),

    #[error("TOML serialize error: {0}")]
    TomlSerialize(#[from] toml::ser::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    // ── Database ─────────────────────────────────────────────────────
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("database migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    // ── IO ───────────────────────────────────────────────────────────
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    // ── Fleet ────────────────────────────────────────────────────────
    #[error("node not found: {name}")]
    NodeNotFound { name: String },

    #[error("model not found: {name}")]
    ModelNotFound { name: String },

    #[error("node offline: {name}")]
    NodeOffline { name: String },

    #[error("tier unavailable: tier {tier}")]
    TierUnavailable { tier: u8 },

    // ── Leader Election ──────────────────────────────────────────────
    #[error("leader election failed: {reason}")]
    LeaderElectionFailed { reason: String },

    #[error("not the leader — current leader is {leader}")]
    NotLeader { leader: String },

    // ── Hardware ─────────────────────────────────────────────────────
    #[error("hardware detection failed for {component}: {reason}")]
    HardwareDetection { component: String, reason: String },

    // ── Runtime ──────────────────────────────────────────────────────
    #[error("runtime error: {0}")]
    Runtime(String),

    // ── Internal ─────────────────────────────────────────────────────
    #[error("internal error: {0}")]
    Internal(String),
}

/// Convenience alias used throughout ff-core.
pub type Result<T> = std::result::Result<T, ForgeFleetError>;
