//! Auto-dependency detection for work items.
//!
//! Given a work item's description, suggest related items by keyword matching.
//! This is a lightweight approach — no embeddings or ML, just smart text overlap.

use serde::{Deserialize, Serialize};

use crate::db::McDb;
use crate::error::McResult;
use crate::work_item::{WorkItem, WorkItemFilter};

/// A suggested link between two work items.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestedLink {
    pub target_item_id: String,
    pub target_item_title: String,
    pub score: f64,
    pub matching_keywords: Vec<String>,
}

/// Configuration for the auto-linker.
#[derive(Debug, Clone)]
pub struct AutoLinkConfig {
    /// Minimum score to include a suggestion (0.0 - 1.0).
    pub min_score: f64,
    /// Maximum number of suggestions to return.
    pub max_suggestions: usize,
    /// Minimum keyword length to consider.
    pub min_keyword_len: usize,
}

impl Default for AutoLinkConfig {
    fn default() -> Self {
        Self {
            min_score: 0.1,
            max_suggestions: 10,
            min_keyword_len: 4,
        }
    }
}

/// Stop words to exclude from keyword extraction.
const STOP_WORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "but", "in", "on", "at", "to", "for", "of", "with", "by",
    "from", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had", "do", "does",
    "did", "will", "would", "could", "should", "may", "might", "shall", "can", "need", "this",
    "that", "these", "those", "it", "its", "we", "they", "them", "our", "their", "not", "no",
    "nor", "so", "if", "then", "than", "when", "what", "which", "who", "whom", "how", "all",
    "each", "every", "both", "few", "more", "most", "other", "some", "such", "into", "through",
    "during", "before", "after", "above", "below", "between", "out", "off", "over", "under",
    "again", "further", "once", "here", "there", "where", "why", "about", "also", "just", "very",
    "only", "still", "already", "even", "now", "new", "make", "like", "well", "back", "much",
    "any", "same", "way", "work", "item", "task", "todo", "done", "create", "update", "delete",
    "add", "remove", "get", "set", "use", "using",
];

/// Find related work items for a given item by keyword matching.
pub fn suggest_links(
    db: &McDb,
    item_id: &str,
    description: &str,
    title: &str,
    config: &AutoLinkConfig,
) -> McResult<Vec<SuggestedLink>> {
    let source_keywords =
        extract_keywords(&format!("{title} {description}"), config.min_keyword_len);

    if source_keywords.is_empty() {
        return Ok(Vec::new());
    }

    // Get all other work items
    let all_items = WorkItem::list(db, &WorkItemFilter::default())?;

    let mut suggestions: Vec<SuggestedLink> = Vec::new();

    for target in &all_items {
        if target.id == item_id {
            continue;
        }

        let target_text = format!("{} {}", target.title, target.description);
        let target_keywords = extract_keywords(&target_text, config.min_keyword_len);

        // Find matching keywords
        let matching: Vec<String> = source_keywords
            .iter()
            .filter(|kw| target_keywords.contains(kw))
            .cloned()
            .collect();

        if matching.is_empty() {
            continue;
        }

        // Score = matching keywords / total unique keywords in both items
        let total_unique: std::collections::HashSet<&String> = source_keywords
            .iter()
            .chain(target_keywords.iter())
            .collect();
        let score = matching.len() as f64 / total_unique.len().max(1) as f64;

        if score >= config.min_score {
            suggestions.push(SuggestedLink {
                target_item_id: target.id.clone(),
                target_item_title: target.title.clone(),
                score,
                matching_keywords: matching,
            });
        }
    }

    // Sort by score descending
    suggestions.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    suggestions.truncate(config.max_suggestions);

    Ok(suggestions)
}

/// Extract meaningful keywords from text.
fn extract_keywords(text: &str, min_len: usize) -> Vec<String> {
    let stop_set: std::collections::HashSet<&str> = STOP_WORDS.iter().copied().collect();

    let mut keywords: Vec<String> = text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|word| {
            word.len() >= min_len
                && !stop_set.contains(word)
                && !word.chars().all(|c| c.is_numeric())
        })
        .map(|s| s.to_string())
        .collect();

    keywords.sort();
    keywords.dedup();
    keywords
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_item::CreateWorkItem;

    fn test_db() -> McDb {
        McDb::in_memory().unwrap()
    }

    #[test]
    fn test_keyword_extraction() {
        let keywords = extract_keywords("Build the SQLite database layer for work items", 4);
        assert!(keywords.contains(&"sqlite".to_string()));
        assert!(keywords.contains(&"database".to_string()));
        assert!(keywords.contains(&"layer".to_string()));
        assert!(keywords.contains(&"items".to_string()));
        // "the" and "for" should be filtered
        assert!(!keywords.contains(&"the".to_string()));
        assert!(!keywords.contains(&"for".to_string()));
    }

    #[test]
    fn test_suggest_links() {
        let db = test_db();

        let item1 = WorkItem::create(
            &db,
            CreateWorkItem {
                title: "Build SQLite database layer".into(),
                description: "Implement the SQLite storage backend for mission control data".into(),
                ..Default::default()
            },
        )
        .unwrap();

        let item2 = WorkItem::create(
            &db,
            CreateWorkItem {
                title: "Database migration system".into(),
                description: "Create schema migrations for SQLite database tables".into(),
                ..Default::default()
            },
        )
        .unwrap();

        // Unrelated item
        WorkItem::create(
            &db,
            CreateWorkItem {
                title: "Design the UI theme".into(),
                description: "Create color palette and typography for the dashboard".into(),
                ..Default::default()
            },
        )
        .unwrap();

        let suggestions = suggest_links(
            &db,
            &item1.id,
            &item1.description,
            &item1.title,
            &AutoLinkConfig::default(),
        )
        .unwrap();

        // Should find item2 as related (SQLite, database keywords)
        assert!(!suggestions.is_empty());
        assert_eq!(suggestions[0].target_item_id, item2.id);
        assert!(
            suggestions[0]
                .matching_keywords
                .contains(&"sqlite".to_string())
        );
    }

    #[test]
    fn test_no_self_link() {
        let db = test_db();

        let item = WorkItem::create(
            &db,
            CreateWorkItem {
                title: "Some unique thing".into(),
                description: "Very unique description".into(),
                ..Default::default()
            },
        )
        .unwrap();

        let suggestions = suggest_links(
            &db,
            &item.id,
            &item.description,
            &item.title,
            &AutoLinkConfig::default(),
        )
        .unwrap();

        // Should not suggest linking to self
        assert!(suggestions.iter().all(|s| s.target_item_id != item.id));
    }
}
