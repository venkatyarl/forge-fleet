//! Typed persistence models for the ErrorMiner substrate: recurring
//! fleet-error signatures and the per-node daily journald digest.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

/// The stable, model-facing projection of a row in `error_signatures`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FromRow)]
pub struct ErrorSignature {
    pub signature: String,
    pub error_class: Option<String>,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub count_24h: i32,
    pub count_total: i32,
    pub sample_text: Option<String>,
    pub affected_nodes: Option<serde_json::Value>,
    pub state: String,
    pub work_item_id: Option<Uuid>,
    pub fix_commit_sha: Option<String>,
    pub resolved_at: Option<DateTime<Utc>>,
}

/// The stable, model-facing projection of a row in `fleet_log_digest`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FromRow)]
pub struct FleetLogDigest {
    pub id: Uuid,
    pub node: String,
    pub day: NaiveDate,
    pub level: String,
    pub line_class: String,
    pub count: i32,
    pub sample: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_signature_defaults_to_new_state() {
        let signature = ErrorSignature {
            signature: "deadbeef".into(),
            error_class: Some("ssh:timeout".into()),
            first_seen: Utc::now(),
            last_seen: Utc::now(),
            count_24h: 1,
            count_total: 1,
            sample_text: None,
            affected_nodes: None,
            state: "new".into(),
            work_item_id: None,
            fix_commit_sha: None,
            resolved_at: None,
        };

        assert_eq!(signature.state, "new");
        assert!(signature.work_item_id.is_none());
    }
}
