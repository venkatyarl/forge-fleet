//! Project configuration footprint helpers.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Locations and routing context declared in `projects.config`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectConfigFootprint {
    pub paths: Vec<String>,
    pub repos: Vec<String>,
    pub targets: Vec<String>,
    pub vault_realm: Option<String>,
}

impl ProjectConfigFootprint {
    /// Extract the project footprint from a JSONB config value.
    pub fn from_config_json(config_json: &Value) -> Self {
        Self {
            paths: json_string_list(config_json.get("paths"), &["path"]),
            repos: json_string_list(config_json.get("repos"), &["repo", "repo_url", "url"]),
            targets: json_string_list(config_json.get("targets"), &["target", "name", "id"]),
            vault_realm: config_json
                .get("vault_realm")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        }
    }

    /// Iterate every location-like value used to resolve a project's footprint.
    pub fn locations(&self) -> impl Iterator<Item = &str> {
        self.paths
            .iter()
            .chain(self.repos.iter())
            .chain(self.targets.iter())
            .map(String::as_str)
    }
}

fn json_string_list(value: Option<&Value>, object_keys: &[&str]) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };

    let mut out = Vec::new();
    match value {
        Value::Array(items) => {
            for item in items {
                push_json_string(item, object_keys, &mut out);
            }
        }
        other => push_json_string(other, object_keys, &mut out),
    }
    out.sort();
    out.dedup();
    out
}

fn push_json_string(value: &Value, object_keys: &[&str], out: &mut Vec<String>) {
    match value {
        Value::String(s) => push_trimmed(s, out),
        Value::Object(map) => {
            for key in object_keys {
                if let Some(s) = map.get(*key).and_then(Value::as_str) {
                    push_trimmed(s, out);
                    return;
                }
            }
        }
        _ => {}
    }
}

fn push_trimmed(s: &str, out: &mut Vec<String>) {
    let s = s.trim();
    if !s.is_empty() {
        out.push(s.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn project_config_footprint_extracts_locations_and_vault_realm() {
        let footprint = ProjectConfigFootprint::from_config_json(&json!({
            "paths": [" ~/projects/forge-fleet ", {"path": "/srv/forge-fleet"}, ""],
            "repos": [
                "git@github.com-venkat:venkatyarl/forge-fleet.git",
                {"repo_url": "https://github.com/venkatyarl/forge-fleet"}
            ],
            "targets": [{"name": "taylor"}, "linux-builders"],
            "vault_realm": "forgefleet"
        }));

        assert_eq!(
            footprint.locations().collect::<Vec<_>>(),
            vec![
                "/srv/forge-fleet",
                "~/projects/forge-fleet",
                "git@github.com-venkat:venkatyarl/forge-fleet.git",
                "https://github.com/venkatyarl/forge-fleet",
                "linux-builders",
                "taylor",
            ]
        );
        assert_eq!(footprint.vault_realm.as_deref(), Some("forgefleet"));
    }
}
