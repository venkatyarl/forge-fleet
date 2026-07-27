//! Convert completed work items into deterministic chat training records.
//!
//! Loading work items and persisting the resulting corpus are deliberately
//! left to callers. This module only performs the pure transformation.

use ff_db::WorkItem;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const DEFAULT_SYSTEM_PROMPT: &str =
    "You are a software engineering agent. Complete the requested work accurately and safely.";

/// Controls which terminal work items are eligible for the corpus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrainingCorpusConfig {
    pub system_prompt: String,
    pub include_failed: bool,
}

impl Default for TrainingCorpusConfig {
    fn default() -> Self {
        Self {
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
            include_failed: false,
        }
    }
}

/// A single ChatML-style training example.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrainingCorpusRecord {
    pub messages: Vec<TrainingMessage>,
    pub metadata: TrainingMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingMessage {
    pub role: TrainingRole,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrainingRole {
    System,
    User,
    Assistant,
}

/// Provenance retained alongside a training example.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrainingMetadata {
    pub work_item_id: String,
    pub project_id: String,
    pub kind: String,
    pub status: String,
    pub priority: String,
    pub labels: Value,
    pub repo_url: Option<String>,
    pub created_at: String,
    pub completed_at: Option<String>,
}

/// Convert one terminal work item into a training example.
///
/// Successful items must contain an observable result (`work`, `post_work`,
/// touched paths, or a pull-request URL). This prevents incomplete historical
/// rows from becoming examples with fabricated assistant answers.
pub fn work_item_to_training_record(
    item: &WorkItem,
    config: &TrainingCorpusConfig,
) -> Option<TrainingCorpusRecord> {
    let eligible = matches!(item.status.as_str(), "done" | "merged")
        || (config.include_failed && item.status == "failed");
    if !eligible {
        return None;
    }

    let user_content = user_content(item);
    let assistant_content = assistant_content(item)?;

    Some(TrainingCorpusRecord {
        messages: vec![
            TrainingMessage {
                role: TrainingRole::System,
                content: config.system_prompt.clone(),
            },
            TrainingMessage {
                role: TrainingRole::User,
                content: user_content,
            },
            TrainingMessage {
                role: TrainingRole::Assistant,
                content: assistant_content,
            },
        ],
        metadata: TrainingMetadata {
            work_item_id: item.id.to_string(),
            project_id: item.project_id.clone(),
            kind: item.kind.clone(),
            status: item.status.clone(),
            priority: item.priority.clone(),
            labels: item.labels.clone(),
            repo_url: item.repo_url.clone(),
            created_at: item.created_at.to_rfc3339(),
            completed_at: item.completed_at.map(|value| value.to_rfc3339()),
        },
    })
}

/// Convert all eligible work items while preserving input order.
pub fn work_items_to_training_records<'a>(
    items: impl IntoIterator<Item = &'a WorkItem>,
    config: &TrainingCorpusConfig,
) -> Vec<TrainingCorpusRecord> {
    items
        .into_iter()
        .filter_map(|item| work_item_to_training_record(item, config))
        .collect()
}

fn user_content(item: &WorkItem) -> String {
    let mut sections = vec![format!("Task: {}", item.title)];
    push_text(&mut sections, "Description", item.description.as_deref());
    push_json(&mut sections, "Context", &item.context);
    push_json(
        &mut sections,
        "Required capabilities",
        &item.required_capabilities,
    );
    push_json(&mut sections, "Predicted paths", &item.predicted_paths);
    sections.join("\n\n")
}

fn assistant_content(item: &WorkItem) -> Option<String> {
    let mut sections = Vec::new();
    push_json(&mut sections, "Work performed", &item.work);
    push_json(&mut sections, "Verification and follow-up", &item.post_work);
    push_json(&mut sections, "Touched paths", &item.touched_paths);
    push_text(&mut sections, "Pull request", item.pr_url.as_deref());
    if item.status == "failed" {
        push_text(&mut sections, "Failure", item.last_error.as_deref());
    }
    (!sections.is_empty()).then(|| sections.join("\n\n"))
}

fn push_text(sections: &mut Vec<String>, label: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        sections.push(format!("{label}:\n{value}"));
    }
}

fn push_json(sections: &mut Vec<String>, label: &str, value: &Value) {
    let empty = value.is_null()
        || value.as_array().is_some_and(Vec::is_empty)
        || value.as_object().is_some_and(serde_json::Map::is_empty);
    if !empty {
        sections.push(format!("{label}:\n{value}"));
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    fn work_item(status: &str) -> WorkItem {
        WorkItem {
            id: Uuid::nil(),
            project_id: "forge-fleet".into(),
            milestone_id: None,
            parent_id: None,
            kind: "task".into(),
            title: "Create training corpus data pipeline".into(),
            description: Some("Convert completed work items.".into()),
            labels: json!(["training"]),
            status: status.into(),
            priority: "high".into(),
            assigned_to: None,
            assigned_computer: None,
            branch_name: None,
            pr_url: Some("https://example.test/pull/1".into()),
            brain_node_ids: json!([]),
            created_at: Utc.with_ymd_and_hms(2026, 7, 27, 12, 0, 0).unwrap(),
            created_by: "test".into(),
            started_at: None,
            completed_at: Some(Utc.with_ymd_and_hms(2026, 7, 27, 13, 0, 0).unwrap()),
            due_date: None,
            estimated_hours: None,
            metadata: json!({}),
            required_capabilities: json!(["rust"]),
            complexity: "small".into(),
            predicted_paths: json!(["crates/ff-pipeline"]),
            touched_paths: json!(["crates/ff-pipeline/src/training_corpus.rs"]),
            base_branch: None,
            base_sha: None,
            integration_branch: None,
            merge_rank: None,
            risk_score: 0.0,
            reviewer_required: false,
            attempts: 1,
            last_error: None,
            repo_id: None,
            repo_url: Some("git@example.test:forge-fleet.git".into()),
            repo_path: None,
            context: json!({"constraint": "DB-free"}),
            parked: false,
            pre_work: json!([]),
            work: json!(["implemented converter"]),
            post_work: json!(["cargo test passed"]),
            cleanup_complete: true,
            original_signal: json!({}),
            signal_cleared: None,
            signal_verified_at: None,
            refiled_from: None,
            cortex_subgraph_id: None,
        }
    }

    #[test]
    fn converts_completed_item_to_chat_record() {
        let record =
            work_item_to_training_record(&work_item("done"), &TrainingCorpusConfig::default())
                .unwrap();

        assert_eq!(record.messages.len(), 3);
        assert_eq!(record.messages[1].role, TrainingRole::User);
        assert!(record.messages[1].content.contains("DB-free"));
        assert_eq!(record.messages[2].role, TrainingRole::Assistant);
        assert!(record.messages[2].content.contains("cargo test passed"));
        assert_eq!(record.metadata.work_item_id, Uuid::nil().to_string());
    }

    #[test]
    fn skips_nonterminal_and_empty_results() {
        assert!(
            work_item_to_training_record(&work_item("in_progress"), &Default::default()).is_none()
        );

        let mut item = work_item("done");
        item.work = json!([]);
        item.post_work = json!([]);
        item.touched_paths = json!([]);
        item.pr_url = None;
        assert!(work_item_to_training_record(&item, &Default::default()).is_none());
    }

    #[test]
    fn failed_items_require_opt_in() {
        let mut item = work_item("failed");
        item.work = json!([]);
        item.post_work = json!([]);
        item.touched_paths = json!([]);
        item.pr_url = None;
        item.last_error = Some("build failed".into());

        assert!(work_item_to_training_record(&item, &Default::default()).is_none());
        let config = TrainingCorpusConfig {
            include_failed: true,
            ..Default::default()
        };
        let record = work_item_to_training_record(&item, &config).unwrap();
        assert!(record.messages[2].content.contains("build failed"));
    }

    #[test]
    fn batch_conversion_preserves_order_and_serializes_chatml_shape() {
        let first = work_item("done");
        let mut skipped = work_item("ready");
        skipped.id = Uuid::from_u128(1);
        let mut second = work_item("merged");
        second.id = Uuid::from_u128(2);

        let records =
            work_items_to_training_records([&first, &skipped, &second], &Default::default());
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].metadata.work_item_id, second.id.to_string());

        let json = serde_json::to_value(&records[0]).unwrap();
        assert_eq!(json["messages"][0]["role"], "system");
        assert_eq!(json["messages"][1]["role"], "user");
        assert_eq!(json["messages"][2]["role"], "assistant");
    }
}
