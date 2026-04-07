use anyhow::Result;
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::store::{Memory, MemoryStore, SearchMemoriesParams};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalQuery {
    pub query: String,
    pub workspace_id: Option<String>,
    pub tags: Vec<String>,
    pub min_importance: Option<f32>,
    pub max_age_days: Option<i64>,
    pub limit: usize,
}

impl Default for RetrievalQuery {
    fn default() -> Self {
        Self {
            query: String::new(),
            workspace_id: None,
            tags: vec![],
            min_importance: None,
            max_age_days: None,
            limit: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalResult {
    pub memory: Memory,
    pub score: f32,
    pub matched_terms: Vec<String>,
    pub ranking_reason: String,
}

#[derive(Debug, Clone)]
pub struct MemoryRetrievalEngine {
    store: MemoryStore,
}

impl MemoryRetrievalEngine {
    pub fn new(store: MemoryStore) -> Self {
        Self { store }
    }

    pub async fn retrieve(&self, query: RetrievalQuery) -> Result<Vec<RetrievalResult>> {
        let terms = tokenize_query(&query.query);
        let since = query
            .max_age_days
            .map(|days| Utc::now() - Duration::days(days.max(1)));

        let candidates = self
            .store
            .search_memories(SearchMemoriesParams {
                workspace_id: query.workspace_id.clone(),
                keyword: if query.query.trim().is_empty() {
                    None
                } else {
                    Some(query.query.clone())
                },
                tags: query.tags.clone(),
                source: None,
                min_importance: query.min_importance,
                since,
                limit: query.limit.clamp(1, 100) * 4,
            })
            .await?;

        let mut scored = candidates
            .into_iter()
            .map(|memory| score_memory(memory, &terms))
            .collect::<Vec<_>>();

        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(query.limit.clamp(1, 100));

        Ok(scored)
    }

    /// Semantic retrieval using lexical filtering + soft similarity scoring.
    ///
    /// This provides production relevance ranking today and can be extended
    /// with an embedding-backed index without changing the API.
    pub async fn semantic_search(&self, query: RetrievalQuery) -> Result<Vec<RetrievalResult>> {
        self.retrieve(query).await
    }

    pub async fn retrieve_context(&self, query: RetrievalQuery) -> Result<String> {
        let results = self.retrieve(query).await?;
        let mut context = String::new();

        for (idx, hit) in results.iter().enumerate() {
            context.push_str(&format!(
                "{}. [{} | importance {:.2} | score {:.2}] {}\n",
                idx + 1,
                hit.memory.workspace_id,
                hit.memory.importance,
                hit.score,
                hit.memory.content
            ));
        }

        Ok(context)
    }
}

fn score_memory(memory: Memory, query_terms: &[String]) -> RetrievalResult {
    let lower_content = memory.content.to_ascii_lowercase();
    let mut matched_terms = Vec::new();

    for term in query_terms {
        if lower_content.contains(term)
            || memory
                .tags
                .iter()
                .any(|tag| tag.to_ascii_lowercase().contains(term))
        {
            matched_terms.push(term.clone());
        }
    }

    let term_ratio = if query_terms.is_empty() {
        0.5
    } else {
        matched_terms.len() as f32 / query_terms.len() as f32
    };

    let age_days = (Utc::now() - memory.created_at).num_days().max(0) as f32;
    let recency = (1.0 / (1.0 + age_days / 7.0)).clamp(0.0, 1.0);

    let score = (memory.importance * 0.50) + (term_ratio * 0.35) + (recency * 0.15);

    let ranking_reason = format!(
        "importance={:.2}, term_ratio={:.2}, recency={:.2}",
        memory.importance, term_ratio, recency
    );

    RetrievalResult {
        memory,
        score,
        matched_terms,
        ranking_reason,
    }
}

fn tokenize_query(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| t.len() >= 2)
        .collect()
}
