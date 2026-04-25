//! Semantic code search — structural search beyond text matching.

use crate::graph::CodeGraph;
use crate::parser::{CodeEntity, EntityKind};

/// Search result with relevance scoring.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub entity: CodeEntity,
    pub score: f64,
    pub match_reason: String,
}

/// Search the code graph with a natural-language-like query.
pub fn semantic_search(graph: &CodeGraph, query: &str, max_results: usize) -> Vec<SearchResult> {
    let lower = query.to_ascii_lowercase();
    let mut results = Vec::new();

    for entity in graph.entities.values() {
        let mut score = 0.0;
        let mut reasons = Vec::new();

        // Exact name match
        if entity.name.to_ascii_lowercase() == lower {
            score += 10.0;
            reasons.push("exact name match");
        }

        // Name contains query
        if entity.name.to_ascii_lowercase().contains(&lower) {
            score += 5.0;
            reasons.push("name contains query");
        }

        // Signature contains query
        if entity.signature.to_ascii_lowercase().contains(&lower) {
            score += 3.0;
            reasons.push("signature match");
        }

        // Source contains query
        if entity.source.to_ascii_lowercase().contains(&lower) {
            score += 1.0;
            reasons.push("source contains query");
        }

        // Boost by entity kind importance
        score *= match entity.kind {
            EntityKind::Function | EntityKind::Method => 1.5,
            EntityKind::Struct | EntityKind::Class | EntityKind::Trait | EntityKind::Interface => {
                2.0
            }
            EntityKind::Enum | EntityKind::Type => 1.3,
            _ => 1.0,
        };

        if score > 0.0 {
            results.push(SearchResult {
                entity: entity.clone(),
                score,
                match_reason: reasons.join(", "),
            });
        }
    }

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results.truncate(max_results);
    results
}

/// Find all definitions of a symbol across the graph.
pub fn find_definitions<'a>(graph: &'a CodeGraph, name: &str) -> Vec<&'a CodeEntity> {
    let lower = name.to_ascii_lowercase();
    graph
        .entities
        .values()
        .filter(|e| e.name.to_ascii_lowercase() == lower)
        .filter(|e| !matches!(e.kind, EntityKind::Import))
        .collect()
}

/// Find all references/usages of a symbol (imports + calls).
pub fn find_references<'a>(graph: &'a CodeGraph, name: &str) -> Vec<&'a CodeEntity> {
    let lower = name.to_ascii_lowercase();
    graph
        .entities
        .values()
        .filter(|e| {
            e.source.to_ascii_lowercase().contains(&lower) && e.name.to_ascii_lowercase() != lower
        })
        .collect()
}
