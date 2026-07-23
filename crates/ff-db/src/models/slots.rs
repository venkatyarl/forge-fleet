//! Typed persistence model for a row in `sub_agents` — a concurrency slot
//! that the agent coordinator claims before dispatching a work item to a
//! computer's local LLM.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::FromRow;
use uuid::Uuid;

use super::work_item::WorkItem;

/// Prefix on a `WorkItem.required_capabilities` tag that expresses a
/// minimum-RAM requirement, e.g. `"ram:32"` means "needs a slot backed by
/// a computer with at least 32 GB RAM". Checked numerically against
/// [`Slot::ram_gb`] rather than by exact tag match, since a slot with more
/// RAM than required still satisfies the requirement.
const RAM_TAG_PREFIX: &str = "ram:";

/// The persistent representation of a row in `sub_agents`.
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
    /// Flat explicit capability tags this slot can satisfy (e.g. `"gpu"`,
    /// a repo name override), on top of the tags implied by `kind`,
    /// `skill`, `model_preference`, and `workspace_dir`. Added V244.
    pub capabilities: Value,
    /// Flat skill tags for what this slot's assigned agent/model is good
    /// at (e.g. `"rust"`, `"frontend"`, `"code-review"`). Added V244.
    pub skill: Value,
    /// RAM available to this slot's computer, in GB. `None` for rows that
    /// predate V244 and haven't been backfilled. Added V244.
    pub ram_gb: Option<i32>,
}

impl Slot {
    /// Returns true if this slot satisfies every capability tag `item`
    /// requires.
    ///
    /// Most tags are checked against the union of this slot's `kind`,
    /// `skill`, `model_preference`, the repo name derived from
    /// `workspace_dir`, and its explicit `capabilities`. RAM tags
    /// (`"ram:<n>"`) are the exception: they're checked numerically
    /// against `ram_gb` (slot RAM must be >= the requested minimum)
    /// rather than by exact tag match. An item with no required
    /// capabilities is satisfiable by any slot.
    pub fn is_capable_of(&self, item: &WorkItem) -> bool {
        let required = tags_from_json(&item.required_capabilities);
        if required.is_empty() {
            return true;
        }
        let available = self.capability_tags();
        required.iter().all(|tag| {
            match tag
                .strip_prefix(RAM_TAG_PREFIX)
                .and_then(|n| n.parse::<i32>().ok())
            {
                Some(min_ram_gb) => self.ram_gb.is_some_and(|ram| ram >= min_ram_gb),
                None => available.contains(tag),
            }
        })
    }

    fn capability_tags(&self) -> HashSet<String> {
        let mut tags = tags_from_json(&self.capabilities);
        tags.extend(tags_from_json(&self.skill));
        tags.insert(self.kind.clone());
        if let Some(model) = self.model_preference.as_deref() {
            if !model.is_empty() {
                tags.insert(model.to_string());
            }
        }
        if let Some(repo) = repo_name(&self.workspace_dir) {
            tags.insert(repo);
        }
        tags
    }
}

/// Parses a JSON array of strings into a tag set, skipping anything else.
fn tags_from_json(value: &Value) -> HashSet<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// Derives a repo name from a workspace directory path (its last
/// non-empty path component), e.g. `/home/x/repo` -> `repo`.
fn repo_name(workspace_dir: &str) -> Option<String> {
    workspace_dir
        .trim_end_matches('/')
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(
        kind: &str,
        model: Option<&str>,
        workspace_dir: &str,
        capabilities: Value,
        skill: Value,
        ram_gb: Option<i32>,
    ) -> Slot {
        Slot {
            id: Uuid::nil(),
            computer_id: Uuid::nil(),
            slot: 0,
            status: "idle".to_string(),
            current_work_item_id: None,
            started_at: None,
            workspace_dir: workspace_dir.to_string(),
            model_preference: model.map(str::to_string),
            last_heartbeat_at: None,
            metadata: serde_json::json!({}),
            kind: kind.to_string(),
            capabilities,
            skill,
            ram_gb,
        }
    }

    fn item_requiring(tags: Value) -> WorkItem {
        WorkItem {
            id: Uuid::nil(),
            project_id: "p".to_string(),
            milestone_id: None,
            parent_id: None,
            kind: "task".to_string(),
            title: "t".to_string(),
            description: None,
            labels: serde_json::json!([]),
            status: "pending".to_string(),
            priority: "normal".to_string(),
            eisenhower_quadrant: None,
            numeric_priority: None,
            pick_score: None,
            blocked_by_count: 0,
            assigned_to: None,
            assigned_computer: None,
            branch_name: None,
            pr_url: None,
            brain_node_ids: serde_json::json!([]),
            created_at: Utc::now(),
            created_by: "test".to_string(),
            started_at: None,
            completed_at: None,
            due_date: None,
            estimated_hours: None,
            metadata: serde_json::json!({}),
            required_capabilities: tags,
            complexity: "low".to_string(),
            predicted_paths: serde_json::json!([]),
            touched_paths: serde_json::json!([]),
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
            context: serde_json::json!({}),
            parked: false,
            pre_work: serde_json::json!({}),
            work: serde_json::json!({}),
            post_work: serde_json::json!({}),
            cleanup_complete: false,
            original_signal: serde_json::json!({}),
            signal_cleared: None,
            signal_verified_at: None,
            refiled_from: None,
        }
    }

    #[test]
    fn no_required_capabilities_is_always_satisfiable() {
        let s = slot(
            "sub_agent",
            None,
            "/tmp/slot",
            serde_json::json!([]),
            serde_json::json!([]),
            None,
        );
        assert!(s.is_capable_of(&item_requiring(serde_json::json!([]))));
    }

    #[test]
    fn matches_via_explicit_capabilities_tag() {
        let s = slot(
            "sub_agent",
            None,
            "/tmp/slot",
            serde_json::json!(["gpu", "rust"]),
            serde_json::json!([]),
            None,
        );
        assert!(s.is_capable_of(&item_requiring(serde_json::json!(["gpu"]))));
        assert!(!s.is_capable_of(&item_requiring(serde_json::json!(["macos"]))));
    }

    #[test]
    fn matches_via_kind() {
        let s = slot(
            "reviewer",
            None,
            "/tmp/slot",
            serde_json::json!([]),
            serde_json::json!([]),
            None,
        );
        assert!(s.is_capable_of(&item_requiring(serde_json::json!(["reviewer"]))));
    }

    #[test]
    fn matches_via_skill_tag() {
        let s = slot(
            "sub_agent",
            None,
            "/tmp/slot",
            serde_json::json!([]),
            serde_json::json!(["frontend", "code-review"]),
            None,
        );
        assert!(s.is_capable_of(&item_requiring(serde_json::json!(["frontend"]))));
        assert!(!s.is_capable_of(&item_requiring(serde_json::json!(["backend"]))));
    }

    #[test]
    fn matches_via_model_preference() {
        let s = slot(
            "sub_agent",
            Some("qwen3-coder-30b"),
            "/tmp/slot",
            serde_json::json!([]),
            serde_json::json!([]),
            None,
        );
        assert!(s.is_capable_of(&item_requiring(serde_json::json!(["qwen3-coder-30b"]))));
        assert!(!s.is_capable_of(&item_requiring(serde_json::json!(["llama-70b"]))));
    }

    #[test]
    fn matches_via_repo_derived_from_workspace_dir() {
        let s = slot(
            "sub_agent",
            None,
            "/home/lily/.forgefleet/sub-agents/sub-agent-0/forge-fleet",
            serde_json::json!([]),
            serde_json::json!([]),
            None,
        );
        assert!(s.is_capable_of(&item_requiring(serde_json::json!(["forge-fleet"]))));
        assert!(!s.is_capable_of(&item_requiring(serde_json::json!(["other-repo"]))));
    }

    #[test]
    fn ram_requirement_is_satisfied_when_slot_meets_or_exceeds_minimum() {
        let s = slot(
            "sub_agent",
            None,
            "/tmp/slot",
            serde_json::json!([]),
            serde_json::json!([]),
            Some(64),
        );
        assert!(s.is_capable_of(&item_requiring(serde_json::json!(["ram:32"]))));
        assert!(s.is_capable_of(&item_requiring(serde_json::json!(["ram:64"]))));
    }

    #[test]
    fn ram_requirement_fails_when_slot_ram_is_insufficient() {
        let s = slot(
            "sub_agent",
            None,
            "/tmp/slot",
            serde_json::json!([]),
            serde_json::json!([]),
            Some(16),
        );
        assert!(!s.is_capable_of(&item_requiring(serde_json::json!(["ram:32"]))));
    }

    #[test]
    fn ram_requirement_fails_when_slot_ram_is_unknown() {
        let s = slot(
            "sub_agent",
            None,
            "/tmp/slot",
            serde_json::json!([]),
            serde_json::json!([]),
            None,
        );
        assert!(!s.is_capable_of(&item_requiring(serde_json::json!(["ram:32"]))));
    }

    #[test]
    fn requires_all_tags_to_match_across_every_dimension() {
        let s = slot(
            "sub_agent",
            Some("qwen3-30b"),
            "/tmp/forge-fleet",
            serde_json::json!(["gpu"]),
            serde_json::json!(["rust"]),
            Some(64),
        );
        assert!(s.is_capable_of(&item_requiring(serde_json::json!([
            "gpu",
            "rust",
            "qwen3-30b",
            "forge-fleet",
            "ram:32"
        ]))));
        assert!(!s.is_capable_of(&item_requiring(serde_json::json!(["gpu", "macos"]))));
        assert!(!s.is_capable_of(&item_requiring(serde_json::json!(["gpu", "ram:128"]))));
    }
}
