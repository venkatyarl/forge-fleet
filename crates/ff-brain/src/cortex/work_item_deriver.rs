//! Derives typed work items from extracted candidate strings.

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Initial status assigned to every derived work item.
pub const STATUS_PENDING: &str = "pending";

/// A typed work item derived from a raw candidate string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItem {
    pub id: Uuid,
    pub title: String,
    pub status: String,
    /// Where the candidate came from (corpus id, file path, backlog, etc.).
    pub source: String,
    pub created_at: DateTime<Utc>,
}

/// Transform extracted candidate strings into typed [`WorkItem`]s.
///
/// Blank candidates are ignored. Non-blank candidates retain their input
/// order and are mapped without LLM involvement.
pub fn derive_work_items(candidates: &[String], source: &str) -> Vec<WorkItem> {
    candidates
        .iter()
        .filter_map(|candidate| {
            let title = candidate.trim();
            if title.is_empty() {
                return None;
            }

            Some(WorkItem {
                id: Uuid::new_v4(),
                title: title.to_string(),
                status: STATUS_PENDING.to_string(),
                source: source.to_string(),
                created_at: Utc::now(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_pending_work_items_with_source() {
        let before = Utc::now();
        let items = derive_work_items(
            &["Add retries".to_string(), "Fix scheduler".to_string()],
            "cortex",
        );
        let after = Utc::now();

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "Add retries");
        assert_eq!(items[1].title, "Fix scheduler");
        assert_ne!(items[0].id, items[1].id);
        assert!(items.iter().all(|item| item.status == STATUS_PENDING));
        assert!(items.iter().all(|item| item.source == "cortex"));
        assert!(
            items
                .iter()
                .all(|item| item.created_at >= before && item.created_at <= after)
        );
    }

    #[test]
    fn ignores_blank_candidates_and_trims_titles() {
        let items = derive_work_items(
            &["  Build context  ".to_string(), " \n\t ".to_string()],
            "extractor",
        );

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Build context");
    }
}
