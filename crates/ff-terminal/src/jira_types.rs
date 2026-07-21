use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JiraMonitorConfig {
    pub api_key: String,
    pub base_url: String,
    pub project_id: String,
}

impl JiraMonitorConfig {
    pub fn validate(&self) -> Result<()> {
        if self.api_key.trim().is_empty() {
            bail!("Jira API key must not be empty");
        }
        if self.project_id.trim().is_empty() {
            bail!("Jira project ID must not be empty");
        }

        let url = reqwest::Url::parse(self.base_url.trim())
            .map_err(|error| anyhow::anyhow!("invalid Jira base URL: {error}"))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            bail!("Jira base URL must be an absolute HTTP(S) URL");
        }

        Ok(())
    }
}

/// A row in `jira_issue_leases`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct JiraLease {
    pub config_id: String,
    pub issue_id: String,
    pub session_id: String,
    pub lease_token: Uuid,
    pub branch: Option<String>,
    pub repo: Option<String>,
    pub heartbeat_at: DateTime<Utc>,
    pub lease_until: DateTime<Utc>,
}

/// A row in `jira_watch_state`, representing an issue in the monitored queue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FromRow)]
pub struct JiraQueueItem {
    pub config_id: String,
    pub issue_id: String,
    pub last_seen_comment_id: Option<String>,
    pub last_seen_comment_created_at: Option<DateTime<Utc>>,
    pub last_seen_status: Option<String>,
    pub last_seen_assignee_id: Option<String>,
    pub awaiting_party: Option<String>,
    pub awaiting_since: Option<DateTime<Utc>>,
    pub last_retag_at: Option<DateTime<Utc>>,
    pub next_action_at: Option<DateTime<Utc>>,
    pub active_work_lease_id: Option<Uuid>,
    pub state_json: Value,
}

#[cfg(test)]
mod tests {
    use super::JiraMonitorConfig;

    fn valid_config() -> JiraMonitorConfig {
        JiraMonitorConfig {
            api_key: "secret".into(),
            base_url: "https://example.atlassian.net".into(),
            project_id: "HF360".into(),
        }
    }

    #[test]
    fn validates_required_jira_configuration() {
        assert!(valid_config().validate().is_ok());

        let mut config = valid_config();
        config.api_key.clear();
        assert!(config.validate().is_err());

        let mut config = valid_config();
        config.project_id = "  ".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_invalid_or_non_http_base_urls() {
        for base_url in ["", "example.atlassian.net", "ftp://example.com"] {
            let mut config = valid_config();
            config.base_url = base_url.into();
            assert!(config.validate().is_err(), "accepted {base_url}");
        }
    }
}
