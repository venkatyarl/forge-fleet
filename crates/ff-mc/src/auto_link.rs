//! Auto-dependency detection for work items.
//!
//! Given a work item's description, suggest related items by keyword matching.
//! This is a lightweight approach — no embeddings or ML, just smart text overlap.

use serde::{Deserialize, Serialize};

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
