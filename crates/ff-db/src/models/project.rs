//! Typed representation of the `projects.config` JSONB column.

use serde::{Deserialize, Serialize};

fn default_status() -> String {
    "active".to_string()
}

/// Structured contents of `projects.config` — attached local paths, GitHub
/// repos, deploy targets, and the Vault realm this project's secrets live
/// under. Stored as JSONB alongside the existing `projects.metadata` column
/// (see [`crate::queries::ProjectGitPolicy`] for the sibling git-policy
/// columns); round-trips through `serde_json::Value` so a partial or legacy
/// payload still deserializes via field defaults instead of failing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectConfig {
    pub paths: Vec<String>,
    pub repos: Vec<String>,
    pub targets: Vec<String>,
    pub vault_realm: Option<String>,
    pub status: String,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            repos: Vec::new(),
            targets: Vec::new(),
            vault_realm: None,
            status: default_status(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_config_survives_jsonb_round_trip() {
        let original = ProjectConfig {
            paths: vec!["/srv/app".to_string()],
            repos: vec!["git@github.com:acme/app.git".to_string()],
            targets: vec!["prod".to_string(), "staging".to_string()],
            vault_realm: Some("acme-prod".to_string()),
            status: "active".to_string(),
        };

        let json = serde_json::to_value(&original).expect("serialize");
        let restored: ProjectConfig = serde_json::from_value(json).expect("deserialize");
        assert_eq!(restored, original);
    }

    #[test]
    fn project_config_defaults_from_empty_object() {
        let restored: ProjectConfig =
            serde_json::from_value(serde_json::json!({})).expect("deserialize empty config");
        assert_eq!(restored, ProjectConfig::default());
    }
}
