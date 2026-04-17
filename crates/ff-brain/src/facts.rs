//! Knowledge node extraction from assistant responses.
//!
//! Detects decisions, preferences, and facts in assistant output, and
//! creates candidate knowledge nodes for human review.

use regex::Regex;
use sqlx::PgPool;
use uuid::Uuid;

/// Signal phrases that indicate a decision, preference, or fact worth capturing.
const SIGNAL_PATTERNS: &[&str] = &[
    r"\bdecided\b",
    r"\bprefer\b",
    r"\bfrom now on\b",
    r"\blet'?s always\b",
    r"\bwe should always\b",
    r"\bthe rule is\b",
    r"\bgoing forward\b",
    r"\balways use\b",
    r"\bnever use\b",
    r"\bstandard is\b",
    r"\bconvention is\b",
];

/// Check if the text contains signal phrases indicating a decision,
/// preference, or fact worth capturing.
pub fn contains_signal_phrases(text: &str) -> bool {
    let lower = text.to_lowercase();
    for pattern in SIGNAL_PATTERNS {
        let re = Regex::new(pattern).expect("valid signal regex");
        if re.is_match(&lower) {
            return true;
        }
    }
    false
}

/// Extract candidate knowledge nodes from an assistant response.
///
/// If signal phrases are detected, inserts a row into brain_knowledge_candidates
/// with the relevant text snippet. Returns the count of candidates created.
pub async fn extract_candidates(
    pool: &PgPool,
    user_id: Uuid,
    thread_id: Uuid,
    assistant_response: &str,
) -> Result<usize, String> {
    if !contains_signal_phrases(assistant_response) {
        return Ok(0);
    }

    // Extract the sentence(s) containing signal phrases
    let snippet = extract_signal_snippet(assistant_response);

    sqlx::query(
        r#"
        INSERT INTO brain_knowledge_candidates
            (id, user_id, thread_id, action, snippet, status, created_at)
        VALUES ($1, $2, $3, 'create', $4, 'pending', NOW())
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(thread_id)
    .bind(&snippet)
    .execute(pool)
    .await
    .map_err(|e| format!("DB error inserting knowledge candidate: {e}"))?;

    Ok(1)
}

/// Extract sentences containing signal phrases from the response.
fn extract_signal_snippet(text: &str) -> String {
    let sentences: Vec<&str> = text.split(['.', '!', '\n']).collect();
    let mut matching_sentences = Vec::new();

    for sentence in &sentences {
        let s_lower = sentence.to_lowercase();
        for pattern in SIGNAL_PATTERNS {
            let re = Regex::new(pattern).expect("valid signal regex");
            if re.is_match(&s_lower) {
                let trimmed = sentence.trim();
                if !trimmed.is_empty() {
                    matching_sentences.push(trimmed);
                }
                break;
            }
        }
    }

    if matching_sentences.is_empty() {
        // Fallback: return first 500 chars
        text.chars().take(500).collect()
    } else {
        matching_sentences.join(". ")
    }
}
