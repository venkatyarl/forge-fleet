//! Typed persistence model for the software registry.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::FromRow;

/// The persistent representation of a row in `software_registry`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FromRow)]
pub struct SoftwareEntry {
    pub id: String,
    pub display_name: String,
    pub kind: String,
    pub applies_to_os_family: Option<String>,
    pub version_source: Value,
    pub upgrade_playbook: Value,
    pub rollback_playbook: Value,
    pub latest_version: Option<String>,
    pub latest_version_at: Option<DateTime<Utc>>,
    pub release_notes_url: Option<String>,
    pub requires_restart: bool,
    pub requires_reboot: bool,
    pub metadata: Value,
    pub detection: Option<Value>,
    pub auto_install: bool,
    pub agent_hint: Option<String>,
}
