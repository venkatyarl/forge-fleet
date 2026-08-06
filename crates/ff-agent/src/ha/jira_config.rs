//! Jira configuration schema for HA-orchestrated Jira monitoring.
//!
//! Loaded from the `jira_configs` table (or an equivalent config source) and
//! used by the agent-side Jira queue poll and transition tooling.

use serde::{Deserialize, Serialize};

/// Jira site configuration used when polling or transitioning issues.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JiraConfig {
    /// JQL query that defines the monitored queue.
    pub queue_jql: String,
    /// Project key prefix for issues managed under this configuration,
    /// e.g. `"HFPROD"`.
    pub project_key: String,
}

impl JiraConfig {
    /// Create a new config from its required fields.
    pub fn new(queue_jql: impl Into<String>, project_key: impl Into<String>) -> Self {
        Self {
            queue_jql: queue_jql.into(),
            project_key: project_key.into(),
        }
    }

    /// Validate that the required fields are non-empty.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.queue_jql.trim().is_empty(),
            "Jira queue JQL must not be empty"
        );
        anyhow::ensure!(
            !self.project_key.trim().is_empty(),
            "Jira project key must not be empty"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_required_fields() {
        let config = JiraConfig::new("project = HFPROD AND assignee = currentUser()", "HFPROD");
        assert!(config.validate().is_ok());

        let mut config = JiraConfig::new("project = HFPROD", "HFPROD");
        config.queue_jql = "   ".into();
        assert!(config.validate().is_err());

        let mut config = JiraConfig::new("project = HFPROD", "HFPROD");
        config.project_key = "".into();
        assert!(config.validate().is_err());
    }
}
