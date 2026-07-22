//! Typed persistence model for fleet build-capacity slots.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::FromRow;
use uuid::Uuid;

use super::WorkItem;

const RAM_TAG_PREFIX: &str = "ram:";

/// The persistent representation of a row in `sub_agents`.
///
/// `capabilities` is a JSON array of tag strings the slot can serve, e.g.
/// `["skill:rust", "model:qwen3-30b", "repo:forge-fleet", "ram:64"]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FromRow)]
pub struct Slot {
    pub id: Uuid,
    pub computer_id: Uuid,
    pub slot: i32,
    pub status: String,
    pub current_work_item_id: Option<Uuid>,
    pub started_at: Option<DateTime<Utc>>,
    pub workspace_dir: String,
    pub model_preference: Option<String>,
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    pub metadata: Value,
    pub kind: String,
    pub capabilities: Value,
}

impl Slot {
    /// Returns `true` when this slot can serve `item`: every tag in
    /// `item.required_capabilities` is satisfied by the slot. An item with no
    /// required capabilities (missing, empty, or non-array) matches any slot.
    ///
    /// Tag dispatch:
    /// - `kind:<x>` requires `self.kind == x`.
    /// - `model:<x>` requires `self.model_preference == Some(x)`.
    /// - `repo:<x>` requires the final path component of `self.workspace_dir`
    ///   to match `x` (case-insensitive) — the nested clone-per-slot layout
    ///   checks each repo out under `.../sub-agent-N/{repo-slug}/`.
    /// - `ram:<n>` requires a `ram:<m>` tag in `self.capabilities` with
    ///   `m >= n`.
    /// - Anything else (e.g. `skill:<x>`) requires an exact match in
    ///   `self.capabilities`.
    ///
    /// Any tag also matches if listed verbatim in `self.capabilities`,
    /// regardless of prefix.
    pub fn is_capable_of(&self, item: &WorkItem) -> bool {
        let required: Vec<&str> = item
            .required_capabilities
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
            .collect();
        if required.is_empty() {
            return true;
        }

        let slot_tags: Vec<&str> = self
            .capabilities
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
            .collect();

        required
            .into_iter()
            .all(|tag| self.satisfies_tag(tag, &slot_tags))
    }

    fn satisfies_tag(&self, tag: &str, slot_tags: &[&str]) -> bool {
        if slot_tags.contains(&tag) {
            return true;
        }
        if let Some(kind) = tag.strip_prefix("kind:") {
            return self.kind == kind;
        }
        if let Some(model) = tag.strip_prefix("model:") {
            return self.model_preference.as_deref() == Some(model);
        }
        if let Some(repo) = tag.strip_prefix("repo:") {
            return std::path::Path::new(&self.workspace_dir)
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case(repo));
        }
        if let Some(rest) = tag.strip_prefix(RAM_TAG_PREFIX) {
            let Ok(min_ram) = rest.parse::<f64>() else {
                return false;
            };
            return slot_tags.iter().any(|t| {
                t.strip_prefix(RAM_TAG_PREFIX)
                    .and_then(|s| s.parse::<f64>().ok())
                    .is_some_and(|have| have >= min_ram)
            });
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn slot(kind: &str, workspace_dir: &str, model_preference: Option<&str>, caps: Value) -> Slot {
        Slot {
            id: Uuid::nil(),
            computer_id: Uuid::nil(),
            slot: 0,
            status: "idle".into(),
            current_work_item_id: None,
            started_at: None,
            workspace_dir: workspace_dir.into(),
            model_preference: model_preference.map(str::to_owned),
            last_heartbeat_at: None,
            metadata: json!({}),
            kind: kind.into(),
            capabilities: caps,
        }
    }

    fn item_with_capabilities(caps: Value) -> WorkItem {
        WorkItem {
            id: Uuid::nil(),
            project_id: "forge-fleet".into(),
            milestone_id: None,
            parent_id: None,
            kind: "task".into(),
            title: "test".into(),
            description: None,
            labels: json!([]),
            status: "ready".into(),
            priority: "p2".into(),
            assigned_to: None,
            assigned_computer: None,
            branch_name: None,
            pr_url: None,
            brain_node_ids: json!([]),
            created_at: Utc::now(),
            created_by: "test".into(),
            started_at: None,
            completed_at: None,
            due_date: None,
            estimated_hours: None,
            metadata: json!({}),
            required_capabilities: caps,
            complexity: "medium".into(),
            predicted_paths: json!([]),
            touched_paths: json!([]),
            base_branch: None,
            base_sha: None,
            integration_branch: None,
            merge_rank: None,
            risk_score: 0.0,
            reviewer_required: false,
            attempts: 0,
            last_error: None,
            repo_id: None,
            repo_url: None,
            repo_path: None,
            context: json!({}),
            parked: false,
            pre_work: json!({}),
            work: json!({}),
            post_work: json!({}),
            cleanup_complete: false,
            original_signal: json!({}),
            signal_cleared: None,
            signal_verified_at: None,
            refiled_from: None,
        }
    }

    #[test]
    fn item_with_no_requirements_matches_any_slot() {
        let bare = slot("sub_agent", "/home/sub-agent-0", None, json!([]));
        let item = item_with_capabilities(json!([]));
        assert!(bare.is_capable_of(&item));

        let missing = item_with_capabilities(Value::Null);
        assert!(bare.is_capable_of(&missing));
    }

    #[test]
    fn kind_tag_requires_matching_slot_kind() {
        let item = item_with_capabilities(json!(["kind:canonical"]));
        assert!(slot("canonical", "/x", None, json!([])).is_capable_of(&item));
        assert!(!slot("sub_agent", "/x", None, json!([])).is_capable_of(&item));
    }

    #[test]
    fn model_tag_requires_matching_model_preference() {
        let item = item_with_capabilities(json!(["model:qwen3-30b"]));
        assert!(slot("sub_agent", "/x", Some("qwen3-30b"), json!([])).is_capable_of(&item));
        assert!(!slot("sub_agent", "/x", Some("llama-70b"), json!([])).is_capable_of(&item));
        assert!(!slot("sub_agent", "/x", None, json!([])).is_capable_of(&item));
    }

    #[test]
    fn repo_tag_matches_workspace_dir_basename_case_insensitively() {
        let item = item_with_capabilities(json!(["repo:forge-fleet"]));
        let good = slot(
            "sub_agent",
            "/home/sia/.forgefleet/sub-agents/sub-agent-5/Forge-Fleet",
            None,
            json!([]),
        );
        assert!(good.is_capable_of(&item));

        let wrong_repo = slot(
            "sub_agent",
            "/home/sia/.forgefleet/sub-agents/sub-agent-5/other-repo",
            None,
            json!([]),
        );
        assert!(!wrong_repo.is_capable_of(&item));
    }

    #[test]
    fn ram_tag_requires_slot_advertised_capacity_at_least_requirement() {
        let item = item_with_capabilities(json!(["ram:32"]));
        assert!(slot("sub_agent", "/x", None, json!(["ram:64"])).is_capable_of(&item));
        assert!(slot("sub_agent", "/x", None, json!(["ram:32"])).is_capable_of(&item));
        assert!(!slot("sub_agent", "/x", None, json!(["ram:16"])).is_capable_of(&item));
        assert!(!slot("sub_agent", "/x", None, json!([])).is_capable_of(&item));
    }

    #[test]
    fn skill_tag_requires_exact_capability_match() {
        let item = item_with_capabilities(json!(["skill:rust"]));
        assert!(
            slot(
                "sub_agent",
                "/x",
                None,
                json!(["skill:rust", "skill:python"])
            )
            .is_capable_of(&item)
        );
        assert!(!slot("sub_agent", "/x", None, json!(["skill:python"])).is_capable_of(&item));
    }

    #[test]
    fn multiple_required_tags_must_all_be_satisfied() {
        let item = item_with_capabilities(json!(["skill:rust", "model:qwen3-30b", "ram:32"]));
        let capable = slot(
            "sub_agent",
            "/x",
            Some("qwen3-30b"),
            json!(["skill:rust", "ram:64"]),
        );
        assert!(capable.is_capable_of(&item));

        let missing_ram = slot(
            "sub_agent",
            "/x",
            Some("qwen3-30b"),
            json!(["skill:rust", "ram:16"]),
        );
        assert!(!missing_ram.is_capable_of(&item));
    }
}
