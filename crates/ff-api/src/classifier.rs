//! Fast prompt classifier for adaptive LLM routing.
//!
//! Classifies incoming prompts by task type, detected languages, and complexity
//! using keyword/pattern matching — no LLM calls needed.

use serde::{Deserialize, Serialize};

// ─── Task Types ──────────────────────────────────────────────────────────────

/// High-level task type inferred from the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    Code,
    Reasoning,
    Chat,
    Summary,
    Translation,
    Review,
    Debug,
}

impl TaskType {
    /// Human-readable label.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Reasoning => "reasoning",
            Self::Chat => "chat",
            Self::Summary => "summary",
            Self::Translation => "translation",
            Self::Review => "review",
            Self::Debug => "debug",
        }
    }
}

impl std::fmt::Display for TaskType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ─── Complexity ──────────────────────────────────────────────────────────────

/// Estimated task complexity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Complexity {
    Simple,
    Medium,
    Complex,
}

impl Complexity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::Medium => "medium",
            Self::Complex => "complex",
        }
    }
}

impl std::fmt::Display for Complexity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ─── Programming Language Detection ──────────────────────────────────────────

/// Programming language detected in the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgrammingLanguage {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Go,
    Java,
    Cpp,
    CSharp,
    Ruby,
    Swift,
    Kotlin,
    Php,
    Sql,
    Shell,
    Other,
}

// ─── Task Profile (classifier output) ────────────────────────────────────────

/// Full classification result for a prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskProfile {
    pub task_type: TaskType,
    pub complexity: Complexity,
    pub estimated_tokens: u32,
    pub recommended_tier: u8,
    pub detected_languages: Vec<ProgrammingLanguage>,
    /// Raw keyword hit counts used for classification (useful for debugging).
    pub keyword_scores: Vec<(TaskType, u32)>,
}

// ─── Keyword Tables ──────────────────────────────────────────────────────────

/// (keyword, weight) pairs for each task type.
/// We match against the lowercased prompt text.
const CODE_KEYWORDS: &[(&str, u32)] = &[
    ("implement", 3),
    ("function", 3),
    ("write code", 5),
    ("write a function", 5),
    ("code snippet", 4),
    ("algorithm", 3),
    ("compile", 2),
    ("syntax", 2),
    ("class ", 2),
    ("struct ", 2),
    ("enum ", 2),
    ("import ", 1),
    ("def ", 2),
    ("fn ", 2),
    ("async ", 2),
    ("await ", 2),
    ("return ", 1),
    ("variable", 2),
    ("loop", 2),
    ("array", 2),
    ("hashmap", 2),
    ("api endpoint", 3),
    ("http server", 3),
    ("database query", 3),
    ("create a", 1),
    ("build a", 1),
    ("generate code", 4),
    ("refactor", 3),
    ("```", 3),
    ("programming", 2),
    ("crate ", 2),
    ("cargo", 2),
    ("npm", 2),
    ("pip", 2),
    ("git ", 1),
];

const REASONING_KEYWORDS: &[(&str, u32)] = &[
    ("explain why", 4),
    ("reason", 3),
    ("think step by step", 5),
    ("step by step", 4),
    ("analyze", 3),
    ("compare", 3),
    ("evaluate", 3),
    ("pros and cons", 4),
    ("trade-off", 3),
    ("tradeoff", 3),
    ("what if", 2),
    ("why does", 3),
    ("how does", 2),
    ("proof", 3),
    ("theorem", 3),
    ("logic", 2),
    ("mathematical", 3),
    ("calculate", 2),
    ("derive", 2),
    ("solve", 2),
    ("equation", 3),
    ("probability", 3),
    ("hypothesis", 3),
    ("infer", 2),
    ("deduce", 3),
    ("implication", 2),
];

const CHAT_KEYWORDS: &[(&str, u32)] = &[
    ("hello", 2),
    ("hi ", 2),
    ("hey ", 2),
    ("how are you", 3),
    ("what's up", 2),
    ("thanks", 1),
    ("thank you", 1),
    ("bye", 1),
    ("good morning", 2),
    ("good night", 2),
    ("tell me about", 2),
    ("what do you think", 3),
    ("opinion", 2),
    ("chat", 2),
    ("joke", 2),
    ("story", 2),
    ("fun fact", 2),
];

const SUMMARY_KEYWORDS: &[(&str, u32)] = &[
    ("summarize", 5),
    ("summary", 5),
    ("summarise", 5),
    ("tldr", 5),
    ("tl;dr", 5),
    ("brief overview", 4),
    ("key points", 4),
    ("main points", 4),
    ("in short", 3),
    ("condense", 3),
    ("recap", 3),
    ("outline", 2),
    ("highlights", 2),
    ("digest", 2),
    ("abstract", 2),
];

const TRANSLATION_KEYWORDS: &[(&str, u32)] = &[
    ("translate", 5),
    ("translation", 5),
    ("translate to", 5),
    ("in english", 3),
    ("in spanish", 3),
    ("in french", 3),
    ("in german", 3),
    ("in japanese", 3),
    ("in chinese", 3),
    ("in korean", 3),
    ("in portuguese", 3),
    ("to english", 3),
    ("to spanish", 3),
    ("to french", 3),
    ("localize", 3),
    ("localise", 3),
    ("multilingual", 2),
    ("i18n", 3),
];

const REVIEW_KEYWORDS: &[(&str, u32)] = &[
    ("review", 4),
    ("code review", 5),
    ("review this", 4),
    ("feedback", 3),
    ("improve", 2),
    ("suggestion", 2),
    ("best practice", 3),
    ("best practices", 3),
    ("critique", 3),
    ("what's wrong", 3),
    ("could be better", 3),
    ("optimize", 2),
    ("optimise", 2),
    ("clean up", 2),
    ("idiomatic", 3),
    ("lint", 2),
    ("style guide", 3),
    ("maintainability", 3),
];

const DEBUG_KEYWORDS: &[(&str, u32)] = &[
    ("debug", 4),
    ("error", 3),
    ("bug", 3),
    ("fix", 2),
    ("crash", 3),
    ("exception", 3),
    ("stack trace", 5),
    ("stacktrace", 5),
    ("traceback", 5),
    ("segfault", 4),
    ("panic", 3),
    ("undefined", 2),
    ("null pointer", 3),
    ("not working", 3),
    ("doesn't work", 3),
    ("broken", 2),
    ("failing", 2),
    ("failed", 2),
    ("issue", 1),
    ("unexpected", 2),
    ("wrong output", 3),
    ("memory leak", 4),
    ("deadlock", 4),
    ("race condition", 4),
    ("compilation error", 4),
    ("compile error", 4),
    ("type error", 3),
    ("runtime error", 3),
];

/// Language detection patterns: (needle, language).
const LANGUAGE_PATTERNS: &[(&str, ProgrammingLanguage)] = &[
    ("rust", ProgrammingLanguage::Rust),
    ("cargo", ProgrammingLanguage::Rust),
    (".rs", ProgrammingLanguage::Rust),
    ("python", ProgrammingLanguage::Python),
    ("pip ", ProgrammingLanguage::Python),
    ("django", ProgrammingLanguage::Python),
    ("flask", ProgrammingLanguage::Python),
    (".py", ProgrammingLanguage::Python),
    ("javascript", ProgrammingLanguage::JavaScript),
    ("node.js", ProgrammingLanguage::JavaScript),
    ("nodejs", ProgrammingLanguage::JavaScript),
    ("react", ProgrammingLanguage::JavaScript),
    ("vue", ProgrammingLanguage::JavaScript),
    (".js", ProgrammingLanguage::JavaScript),
    ("typescript", ProgrammingLanguage::TypeScript),
    (".ts", ProgrammingLanguage::TypeScript),
    ("tsx", ProgrammingLanguage::TypeScript),
    ("golang", ProgrammingLanguage::Go),
    ("go ", ProgrammingLanguage::Go),
    (".go", ProgrammingLanguage::Go),
    ("java ", ProgrammingLanguage::Java),
    ("spring", ProgrammingLanguage::Java),
    (".java", ProgrammingLanguage::Java),
    ("c++", ProgrammingLanguage::Cpp),
    ("cpp", ProgrammingLanguage::Cpp),
    (".cpp", ProgrammingLanguage::Cpp),
    (".hpp", ProgrammingLanguage::Cpp),
    ("c#", ProgrammingLanguage::CSharp),
    ("csharp", ProgrammingLanguage::CSharp),
    (".cs", ProgrammingLanguage::CSharp),
    ("dotnet", ProgrammingLanguage::CSharp),
    ("ruby", ProgrammingLanguage::Ruby),
    ("rails", ProgrammingLanguage::Ruby),
    (".rb", ProgrammingLanguage::Ruby),
    ("swift", ProgrammingLanguage::Swift),
    (".swift", ProgrammingLanguage::Swift),
    ("kotlin", ProgrammingLanguage::Kotlin),
    (".kt", ProgrammingLanguage::Kotlin),
    ("php", ProgrammingLanguage::Php),
    ("laravel", ProgrammingLanguage::Php),
    (".php", ProgrammingLanguage::Php),
    ("sql", ProgrammingLanguage::Sql),
    ("select ", ProgrammingLanguage::Sql),
    ("insert into", ProgrammingLanguage::Sql),
    ("postgresql", ProgrammingLanguage::Sql),
    ("mysql", ProgrammingLanguage::Sql),
    ("sqlite", ProgrammingLanguage::Sql),
    ("bash", ProgrammingLanguage::Shell),
    ("shell", ProgrammingLanguage::Shell),
    ("zsh", ProgrammingLanguage::Shell),
    (".sh", ProgrammingLanguage::Shell),
];

// ─── Classifier ──────────────────────────────────────────────────────────────

/// Classify a prompt and return a [`TaskProfile`].
///
/// This is a pure keyword/pattern matching classifier — no LLM calls.
/// Designed to be fast (<1ms) and good enough for routing decisions.
pub fn classify(messages: &[crate::types::ChatMessage]) -> TaskProfile {
    let text = extract_text(messages);
    let lower = text.to_lowercase();

    // Score each task type
    let scores = [
        (TaskType::Code, score_keywords(&lower, CODE_KEYWORDS)),
        (TaskType::Debug, score_keywords(&lower, DEBUG_KEYWORDS)),
        (TaskType::Review, score_keywords(&lower, REVIEW_KEYWORDS)),
        (TaskType::Summary, score_keywords(&lower, SUMMARY_KEYWORDS)),
        (
            TaskType::Translation,
            score_keywords(&lower, TRANSLATION_KEYWORDS),
        ),
        (
            TaskType::Reasoning,
            score_keywords(&lower, REASONING_KEYWORDS),
        ),
        (TaskType::Chat, score_keywords(&lower, CHAT_KEYWORDS)),
    ];

    // Pick highest-scoring type, default to Chat if nothing matches
    let task_type = scores
        .iter()
        .max_by_key(|(_, s)| *s)
        .filter(|(_, s)| *s > 0)
        .map(|(t, _)| *t)
        .unwrap_or(TaskType::Chat);

    let detected_languages = detect_languages(&lower);
    let complexity = estimate_complexity(&lower, &text, task_type);
    let estimated_tokens = estimate_tokens(&text);
    let recommended_tier = recommend_tier(task_type, complexity);

    let keyword_scores: Vec<(TaskType, u32)> =
        scores.iter().filter(|(_, s)| *s > 0).copied().collect();

    TaskProfile {
        task_type,
        complexity,
        estimated_tokens,
        recommended_tier,
        detected_languages,
        keyword_scores,
    }
}

/// Classify from a raw string (convenience for non-chat contexts).
pub fn classify_text(text: &str) -> TaskProfile {
    let msg = crate::types::ChatMessage {
        role: "user".to_string(),
        content: serde_json::Value::String(text.to_string()),
        name: None,
        extra: Default::default(),
    };
    classify(&[msg])
}

// ─── Internal Helpers ────────────────────────────────────────────────────────

/// Extract all user/assistant message text into a single string.
fn extract_text(messages: &[crate::types::ChatMessage]) -> String {
    let mut parts = Vec::new();
    for msg in messages {
        match &msg.content {
            serde_json::Value::String(s) => parts.push(s.as_str().to_string()),
            serde_json::Value::Array(arr) => {
                for item in arr {
                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                        parts.push(text.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    parts.join("\n")
}

/// Score a text against a keyword table. Returns total weighted score.
fn score_keywords(text: &str, keywords: &[(&str, u32)]) -> u32 {
    keywords
        .iter()
        .filter(|(kw, _)| text.contains(kw))
        .map(|(_, weight)| weight)
        .sum()
}

/// Detect programming languages mentioned in the text.
fn detect_languages(text: &str) -> Vec<ProgrammingLanguage> {
    let mut seen = std::collections::HashSet::new();
    let mut langs = Vec::new();

    for &(pattern, lang) in LANGUAGE_PATTERNS {
        if text.contains(pattern) && seen.insert(lang) {
            langs.push(lang);
        }
    }

    langs
}

/// Estimate complexity based on message length, keyword density, and task type.
fn estimate_complexity(lower: &str, raw: &str, task_type: TaskType) -> Complexity {
    let char_count = raw.len();
    let word_count = raw.split_whitespace().count();

    // Code blocks bump complexity
    let code_block_count = lower.matches("```").count() / 2;

    // Multi-step indicators
    let multi_step = lower.contains("step 1")
        || lower.contains("first,")
        || lower.contains("then,")
        || lower.contains("finally,")
        || lower.contains("multiple")
        || lower.contains("several");

    // Base complexity from length
    let length_score = if char_count > 2000 || word_count > 400 {
        3
    } else if char_count > 500 || word_count > 100 {
        2
    } else {
        1
    };

    // Complexity modifiers
    let modifier = code_block_count.min(2) as u32
        + if multi_step { 1 } else { 0 }
        + match task_type {
            TaskType::Reasoning | TaskType::Debug => 1,
            TaskType::Code | TaskType::Review => 0,
            _ => 0,
        };

    let total = length_score + modifier;

    if total >= 4 {
        Complexity::Complex
    } else if total >= 2 {
        Complexity::Medium
    } else {
        Complexity::Simple
    }
}

/// Rough token estimate (~4 chars per token for English).
fn estimate_tokens(text: &str) -> u32 {
    (text.len() as u32 / 4).max(1)
}

/// Recommend a tier based on task type and complexity.
fn recommend_tier(task_type: TaskType, complexity: Complexity) -> u8 {
    match (task_type, complexity) {
        // Simple tasks → tier 1 (fast 9B)
        (TaskType::Chat, Complexity::Simple) => 1,
        (TaskType::Translation, Complexity::Simple) => 1,
        (TaskType::Summary, Complexity::Simple) => 1,

        // Medium tasks → tier 2 (32B code)
        (TaskType::Chat, Complexity::Medium) => 1,
        (TaskType::Chat, Complexity::Complex) => 2,
        (TaskType::Code, Complexity::Simple) => 2,
        (TaskType::Code, Complexity::Medium) => 2,
        (TaskType::Debug, Complexity::Simple) => 2,
        (TaskType::Translation, _) => 1,
        (TaskType::Summary, _) => 2,

        // Complex code / reasoning → tier 3 (72B review)
        (TaskType::Code, Complexity::Complex) => 3,
        (TaskType::Debug, Complexity::Medium) => 2,
        (TaskType::Debug, Complexity::Complex) => 3,
        (TaskType::Review, Complexity::Simple) => 2,
        (TaskType::Review, Complexity::Medium) => 3,
        (TaskType::Review, Complexity::Complex) => 3,
        (TaskType::Reasoning, Complexity::Simple) => 2,
        (TaskType::Reasoning, Complexity::Medium) => 3,

        // Expert reasoning → tier 4 (235B expert)
        (TaskType::Reasoning, Complexity::Complex) => 4,
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// Tests
// ═════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_code_prompt() {
        let profile = classify_text("Write a Rust function that sorts a vector of integers");
        assert_eq!(profile.task_type, TaskType::Code);
        assert!(
            profile
                .detected_languages
                .contains(&ProgrammingLanguage::Rust)
        );
    }

    #[test]
    fn test_classify_debug_prompt() {
        let profile = classify_text(
            "I'm getting a stack trace error when I run my program. \
             It panics at line 42 with a null pointer exception.",
        );
        assert_eq!(profile.task_type, TaskType::Debug);
    }

    #[test]
    fn test_classify_summary_prompt() {
        let profile = classify_text("Summarize the key points of this article about AI safety");
        assert_eq!(profile.task_type, TaskType::Summary);
    }

    #[test]
    fn test_classify_translation_prompt() {
        let profile = classify_text("Translate this paragraph to French");
        assert_eq!(profile.task_type, TaskType::Translation);
    }

    #[test]
    fn test_classify_review_prompt() {
        let profile =
            classify_text("Please review this code and suggest best practices for improvement");
        assert_eq!(profile.task_type, TaskType::Review);
    }

    #[test]
    fn test_classify_reasoning_prompt() {
        let profile = classify_text(
            "Explain why quicksort has O(n log n) average case. \
             Think step by step and compare with merge sort.",
        );
        assert_eq!(profile.task_type, TaskType::Reasoning);
    }

    #[test]
    fn test_classify_chat_prompt() {
        let profile = classify_text("Hello, how are you doing today?");
        assert_eq!(profile.task_type, TaskType::Chat);
    }

    #[test]
    fn test_classify_empty_defaults_to_chat() {
        let profile = classify_text("");
        assert_eq!(profile.task_type, TaskType::Chat);
    }

    #[test]
    fn test_complexity_simple() {
        let profile = classify_text("Hi there");
        assert_eq!(profile.complexity, Complexity::Simple);
    }

    #[test]
    fn test_complexity_medium() {
        let profile = classify_text(
            &"Write a function to process data. ".repeat(20), // ~600 chars
        );
        assert!(profile.complexity >= Complexity::Medium);
    }

    #[test]
    fn test_complexity_complex() {
        let long_code = format!(
            "Debug this complex issue:\n```rust\n{}\n```\nStep 1: analyze. Then, check the trace.",
            "fn foo() { bar(); }\n".repeat(100)
        );
        let profile = classify_text(&long_code);
        assert_eq!(profile.complexity, Complexity::Complex);
    }

    #[test]
    fn test_language_detection_multiple() {
        let profile = classify_text("Port this Python script to Rust and add TypeScript bindings");
        let langs = &profile.detected_languages;
        assert!(langs.contains(&ProgrammingLanguage::Python));
        assert!(langs.contains(&ProgrammingLanguage::Rust));
        assert!(langs.contains(&ProgrammingLanguage::TypeScript));
    }

    #[test]
    fn test_recommended_tier_simple_chat() {
        let profile = classify_text("Hello!");
        assert_eq!(profile.recommended_tier, 1);
    }

    #[test]
    fn test_recommended_tier_complex_reasoning() {
        let profile = classify_text(
            "Think step by step. Prove the mathematical theorem about \
             probability distributions. Derive the equation and evaluate the hypothesis. \
             Compare the implications and deduce the result. This is a very complex \
             analysis requiring multiple steps of reasoning across several domains.",
        );
        // Complex reasoning → tier 3 or 4
        assert!(profile.recommended_tier >= 3);
    }

    #[test]
    fn test_estimated_tokens() {
        let profile = classify_text("Hello world"); // 11 chars → ~2-3 tokens
        assert!(profile.estimated_tokens >= 1);
        assert!(profile.estimated_tokens < 10);
    }

    #[test]
    fn test_keyword_scores_populated() {
        let profile = classify_text("Write code to implement a function");
        assert!(!profile.keyword_scores.is_empty());
        // Code should be among the scored types
        assert!(
            profile
                .keyword_scores
                .iter()
                .any(|(t, _)| *t == TaskType::Code)
        );
    }

    #[test]
    fn test_multimodal_content_extraction() {
        let msg = crate::types::ChatMessage {
            role: "user".to_string(),
            content: serde_json::json!([
                {"type": "text", "text": "Summarize this code"},
                {"type": "image_url", "image_url": {"url": "http://example.com/img.png"}}
            ]),
            name: None,
            extra: Default::default(),
        };
        let profile = classify(&[msg]);
        assert_eq!(profile.task_type, TaskType::Summary);
    }
}
