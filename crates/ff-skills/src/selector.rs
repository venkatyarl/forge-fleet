//! Skill selector — relevance scoring and skill selection for user queries.
//!
//! Given a user query or task description, the selector scores all registered
//! skills and returns the most relevant ones.  This is used by the gateway to
//! decide which skills to inject into the LLM context.

use tracing::debug;

use crate::registry::SkillRegistry;
use crate::types::SkillMetadata;

// ─── Scoring Config ──────────────────────────────────────────────────────────

/// Configuration for the skill selector.
#[derive(Debug, Clone)]
pub struct SelectorConfig {
    /// Maximum number of skills to return.
    pub max_results: usize,
    /// Minimum relevance score (0.0–1.0) to include a skill.
    pub min_score: f64,
    /// Weight for name matches.
    pub name_weight: f64,
    /// Weight for description matches.
    pub description_weight: f64,
    /// Weight for tag matches.
    pub tag_weight: f64,
    /// Weight for tool-name matches.
    pub tool_weight: f64,
    /// Weight for keyword index matches.
    pub keyword_weight: f64,
}

impl Default for SelectorConfig {
    fn default() -> Self {
        Self {
            max_results: 5,
            min_score: 0.1,
            name_weight: 10.0,
            description_weight: 3.0,
            tag_weight: 5.0,
            tool_weight: 6.0,
            keyword_weight: 2.0,
        }
    }
}

// ─── Scored Skill ────────────────────────────────────────────────────────────

/// A skill with its computed relevance score.
#[derive(Debug, Clone)]
pub struct ScoredSkill {
    /// The skill metadata.
    pub skill: SkillMetadata,
    /// Relevance score (0.0 = irrelevant, 1.0 = perfect match).
    pub score: f64,
    /// Which tokens matched (for debugging / explanation).
    pub matched_tokens: Vec<String>,
}

// ─── Selector ────────────────────────────────────────────────────────────────

/// Selects the most relevant skills for a given query.
#[derive(Debug, Clone)]
pub struct SkillSelector {
    config: SelectorConfig,
}

impl SkillSelector {
    /// Create a selector with the given config.
    pub fn new(config: SelectorConfig) -> Self {
        Self { config }
    }

    /// Create a selector with default config.
    pub fn default_selector() -> Self {
        Self::new(SelectorConfig::default())
    }

    /// Select the most relevant skills for a query from the registry.
    pub fn select(&self, registry: &SkillRegistry, query: &str) -> Vec<ScoredSkill> {
        let query_tokens = tokenize(query);
        if query_tokens.is_empty() {
            return Vec::new();
        }

        let all_skills = registry.list_all();
        let mut scored: Vec<ScoredSkill> = all_skills
            .into_iter()
            .filter_map(|skill| {
                let (raw_score, matched) = self.compute_score(&skill, &query_tokens);
                if raw_score > 0.0 {
                    Some(ScoredSkill {
                        skill,
                        score: raw_score,
                        matched_tokens: matched,
                    })
                } else {
                    None
                }
            })
            .collect();

        // Normalize scores to 0.0–1.0 range.
        if let Some(max_score) = scored.iter().map(|s| s.score).reduce(f64::max)
            && max_score > 0.0
        {
            for s in &mut scored {
                s.score /= max_score;
            }
        }

        // Filter by min score and sort descending.
        scored.retain(|s| s.score >= self.config.min_score);
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(self.config.max_results);

        debug!(
            query,
            results = scored.len(),
            top_score = scored.first().map(|s| s.score).unwrap_or(0.0),
            "skill selection complete"
        );

        scored
    }

    /// Select skills and return just the metadata (without scores).
    pub fn select_skills(&self, registry: &SkillRegistry, query: &str) -> Vec<SkillMetadata> {
        self.select(registry, query)
            .into_iter()
            .map(|s| s.skill)
            .collect()
    }

    /// Compute the raw relevance score for a skill against query tokens.
    fn compute_score(&self, skill: &SkillMetadata, query_tokens: &[String]) -> (f64, Vec<String>) {
        let mut score = 0.0f64;
        let mut matched = Vec::new();

        let name_lower = skill.name.to_lowercase();
        let id_lower = skill.id.to_lowercase();
        let desc_lower = skill.description.to_lowercase();

        for token in query_tokens {
            // Name / ID match (highest weight).
            if name_lower.contains(token) || id_lower.contains(token) {
                score += self.config.name_weight;
                matched.push(format!("name:{token}"));
            }

            // Description match.
            if desc_lower.contains(token) {
                score += self.config.description_weight;
                matched.push(format!("desc:{token}"));
            }

            // Tag match.
            for tag in &skill.tags {
                if tag.to_lowercase().contains(token) {
                    score += self.config.tag_weight;
                    matched.push(format!("tag:{token}"));
                    break; // Only count once per token.
                }
            }

            // Tool name match.
            for tool in &skill.tools {
                if tool.name.to_lowercase().contains(token) {
                    score += self.config.tool_weight;
                    matched.push(format!("tool:{token}"));
                    break;
                }
                // Also check tool description.
                if tool.description.to_lowercase().contains(token) {
                    score += self.config.description_weight * 0.5;
                    matched.push(format!("tool_desc:{token}"));
                    break;
                }
            }

            // Keyword index match.
            for kw in &skill.search_keywords {
                if kw.contains(token) {
                    score += self.config.keyword_weight;
                    matched.push(format!("kw:{token}"));
                    break;
                }
            }
        }

        // Bonus: exact full-query match in name.
        let full_query = query_tokens.join(" ");
        if name_lower == full_query || id_lower == full_query {
            score *= 2.0;
            matched.push("exact_match".into());
        }

        (score, matched)
    }
}

// ─── Tokenization ────────────────────────────────────────────────────────────

/// Tokenize a query string into lowercase search tokens.
///
/// Strips punctuation, splits on whitespace, and removes stop words.
fn tokenize(query: &str) -> Vec<String> {
    let stop_words: &[&str] = &[
        "a", "an", "the", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had",
        "do", "does", "did", "will", "would", "could", "should", "may", "might", "can", "shall",
        "to", "of", "in", "for", "on", "with", "at", "by", "from", "as", "into", "through",
        "during", "before", "after", "above", "below", "between", "and", "but", "or", "not", "no",
        "nor", "so", "yet", "both", "either", "neither", "this", "that", "these", "those", "it",
        "its", "i", "me", "my", "we", "our", "you", "your", "he", "she", "they", "them",
    ];

    query
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .map(|s| s.to_lowercase())
        .filter(|s| s.len() >= 2 && !stop_words.contains(&s.as_str()))
        .collect()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{SkillOrigin, ToolDefinition, ToolInvocation};
    use chrono::Utc;
    use uuid::Uuid;

    fn make_skill(id: &str, tags: &[&str], tools: &[&str]) -> SkillMetadata {
        let mut skill = SkillMetadata {
            id: id.to_string(),
            name: id.to_string(),
            description: format!("Skill for {id}"),
            origin: SkillOrigin::OpenClaw,
            location: None,
            version: None,
            author: None,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            tools: tools
                .iter()
                .map(|t| ToolDefinition {
                    name: t.to_string(),
                    description: format!("Tool {t}"),
                    parameters: Vec::new(),
                    invocation: ToolInvocation::Builtin {
                        handler: "noop".into(),
                    },
                    permissions: Vec::new(),
                    timeout_secs: 30,
                })
                .collect(),
            permissions: Vec::new(),
            registered_at: Utc::now(),
            uuid: Uuid::new_v4(),
            search_keywords: Vec::new(),
        };
        skill.rebuild_keywords();
        skill
    }

    #[test]
    fn test_tokenize() {
        let tokens = tokenize("What is the weather in Austin?");
        assert!(tokens.contains(&"weather".to_string()));
        assert!(tokens.contains(&"austin".to_string()));
        // Stop words should be removed.
        assert!(!tokens.contains(&"the".to_string()));
        assert!(!tokens.contains(&"is".to_string()));
    }

    #[test]
    fn test_select_basic() {
        let reg = SkillRegistry::empty();
        reg.upsert(make_skill(
            "weather",
            &["forecast", "temperature"],
            &["get_weather"],
        ));
        reg.upsert(make_skill(
            "calendar",
            &["schedule", "events"],
            &["list_events"],
        ));
        reg.upsert(make_skill("email", &["inbox", "send"], &["send_email"]));

        let selector = SkillSelector::default_selector();
        let results = selector.select(&reg, "weather forecast");
        assert!(!results.is_empty());
        assert_eq!(results[0].skill.id, "weather");
        assert!(results[0].score > 0.5);
    }

    #[test]
    fn test_select_by_tool() {
        let reg = SkillRegistry::empty();
        reg.upsert(make_skill("comm", &[], &["send_email", "send_sms"]));
        reg.upsert(make_skill("file", &[], &["read_file", "write_file"]));

        let selector = SkillSelector::default_selector();
        let results = selector.select(&reg, "send email");
        assert!(!results.is_empty());
        assert_eq!(results[0].skill.id, "comm");
    }

    #[test]
    fn test_select_empty_query() {
        let reg = SkillRegistry::empty();
        reg.upsert(make_skill("test", &[], &[]));
        let selector = SkillSelector::default_selector();
        let results = selector.select(&reg, "");
        assert!(results.is_empty());
    }

    #[test]
    fn test_select_max_results() {
        let reg = SkillRegistry::empty();
        for i in 0..20 {
            reg.upsert(make_skill(
                &format!("test-{i}"),
                &["common"],
                &[&format!("common_tool_{i}")],
            ));
        }
        let config = SelectorConfig {
            max_results: 3,
            ..Default::default()
        };
        let selector = SkillSelector::new(config);
        let results = selector.select(&reg, "common tool");
        assert!(results.len() <= 3);
    }
}
