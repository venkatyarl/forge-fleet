//! Task decomposition — break complex tasks into typed subtasks.
//!
//! The decomposer analyzes a user's high-level request and produces a set of
//! [`SubTask`]s, each tagged with the capability it requires (code generation,
//! research, analysis, creative writing, fast lookup, etc.).  Downstream
//! modules ([`crate::router`], [`crate::planner`]) use this decomposition to
//! decide *which model* handles each piece and *when* each piece runs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ─── SubTask Type ────────────────────────────────────────────────────────────

/// The *kind* of capability a subtask requires.
///
/// The router maps these to model tiers and specialties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubTaskType {
    /// Code generation, refactoring, bug-fixing
    Code,
    /// Web search, document retrieval, fact-checking
    Research,
    /// Data analysis, reasoning, math, logic
    Analysis,
    /// Creative writing, brainstorming, copywriting
    Creative,
    /// Simple factual lookups, translations, reformatting
    FastLookup,
    /// Code review, quality assessment, verification
    Review,
    /// Summarization and distillation
    Summarize,
    /// Planning, architecture, system design
    Planning,
    /// Tool use — shell commands, API calls
    ToolUse,
}

impl SubTaskType {
    /// Suggested minimum tier for this task type.
    ///
    /// This is a heuristic starting point — the router may override based on
    /// node availability, load, and historical performance.
    pub fn suggested_min_tier(&self) -> ff_core::Tier {
        match self {
            Self::FastLookup | Self::Summarize => ff_core::Tier::Tier1,
            Self::Code | Self::ToolUse => ff_core::Tier::Tier2,
            Self::Research | Self::Creative | Self::Review => ff_core::Tier::Tier2,
            Self::Analysis | Self::Planning => ff_core::Tier::Tier3,
        }
    }

    /// Suggested ideal tier — the sweet spot for quality vs. speed.
    pub fn suggested_ideal_tier(&self) -> ff_core::Tier {
        match self {
            Self::FastLookup => ff_core::Tier::Tier1,
            Self::Summarize | Self::ToolUse => ff_core::Tier::Tier2,
            Self::Code | Self::Creative => ff_core::Tier::Tier2,
            Self::Research | Self::Review => ff_core::Tier::Tier3,
            Self::Analysis | Self::Planning => ff_core::Tier::Tier3,
        }
    }
}

impl std::fmt::Display for SubTaskType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Code => write!(f, "Code"),
            Self::Research => write!(f, "Research"),
            Self::Analysis => write!(f, "Analysis"),
            Self::Creative => write!(f, "Creative"),
            Self::FastLookup => write!(f, "Fast Lookup"),
            Self::Review => write!(f, "Review"),
            Self::Summarize => write!(f, "Summarize"),
            Self::Planning => write!(f, "Planning"),
            Self::ToolUse => write!(f, "Tool Use"),
        }
    }
}

// ─── SubTask ─────────────────────────────────────────────────────────────────

/// A single decomposed subtask.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubTask {
    /// Unique subtask identifier.
    pub id: Uuid,
    /// Index within the decomposition (for ordering / display).
    pub index: usize,
    /// Human-readable title.
    pub title: String,
    /// Full prompt or description for the subtask.
    pub prompt: String,
    /// What capability this subtask requires.
    pub task_type: SubTaskType,
    /// IDs of subtasks that must complete before this one starts.
    pub depends_on: Vec<Uuid>,
    /// Estimated complexity (1 = trivial, 10 = very complex).
    pub complexity: u8,
    /// Maximum tokens expected in the response.
    pub max_tokens: Option<u32>,
    /// When this subtask was created.
    pub created_at: DateTime<Utc>,
}

impl SubTask {
    /// Create a new subtask with sensible defaults.
    pub fn new(
        index: usize,
        title: impl Into<String>,
        prompt: impl Into<String>,
        task_type: SubTaskType,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            index,
            title: title.into(),
            prompt: prompt.into(),
            task_type,
            depends_on: Vec::new(),
            complexity: 5,
            max_tokens: None,
            created_at: Utc::now(),
        }
    }

    /// Builder: add a dependency on another subtask.
    pub fn depends_on(mut self, dep: Uuid) -> Self {
        self.depends_on.push(dep);
        self
    }

    /// Builder: set complexity score.
    pub fn with_complexity(mut self, c: u8) -> Self {
        self.complexity = c.min(10);
        self
    }

    /// Builder: set max tokens.
    pub fn with_max_tokens(mut self, t: u32) -> Self {
        self.max_tokens = Some(t);
        self
    }

    /// Returns true if this subtask has no dependencies (can start immediately).
    pub fn is_root(&self) -> bool {
        self.depends_on.is_empty()
    }
}

// ─── Task Decomposition ─────────────────────────────────────────────────────

/// The result of decomposing a complex task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDecomposition {
    /// Unique ID for this decomposition.
    pub id: Uuid,
    /// The original user request.
    pub original_task: String,
    /// The decomposed subtasks.
    pub subtasks: Vec<SubTask>,
    /// When the decomposition was performed.
    pub created_at: DateTime<Utc>,
}

impl TaskDecomposition {
    /// Create a new empty decomposition.
    pub fn new(original_task: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            original_task: original_task.into(),
            subtasks: Vec::new(),
            created_at: Utc::now(),
        }
    }

    /// Add a subtask to this decomposition.
    pub fn add_subtask(&mut self, subtask: SubTask) {
        self.subtasks.push(subtask);
    }

    /// Return subtasks that have no dependencies (entry points).
    pub fn root_subtasks(&self) -> Vec<&SubTask> {
        self.subtasks.iter().filter(|s| s.is_root()).collect()
    }

    /// Return subtasks that depend on a given subtask ID.
    pub fn dependents_of(&self, id: Uuid) -> Vec<&SubTask> {
        self.subtasks
            .iter()
            .filter(|s| s.depends_on.contains(&id))
            .collect()
    }

    /// Total number of subtasks.
    pub fn len(&self) -> usize {
        self.subtasks.len()
    }

    /// Whether the decomposition has no subtasks.
    pub fn is_empty(&self) -> bool {
        self.subtasks.is_empty()
    }
}

// ─── Decomposer ──────────────────────────────────────────────────────────────

/// The task decomposer — analyzes a complex request and breaks it into subtasks.
///
/// This is a *rule-based* decomposer that uses keyword heuristics to classify
/// subtasks.  A future version will call an LLM to do intelligent decomposition,
/// but the rule-based approach gives us a fast, deterministic baseline.
pub struct Decomposer {
    /// Keywords that signal each subtask type.
    keyword_map: Vec<(SubTaskType, Vec<&'static str>)>,
}

impl Default for Decomposer {
    fn default() -> Self {
        Self::new()
    }
}

impl Decomposer {
    /// Create a new decomposer with default keyword mappings.
    pub fn new() -> Self {
        let keyword_map = vec![
            (
                SubTaskType::Code,
                vec![
                    "implement",
                    "code",
                    "write code",
                    "function",
                    "class",
                    "module",
                    "refactor",
                    "fix bug",
                    "compile",
                    "build",
                    "test",
                    "debug",
                    "api endpoint",
                    "database",
                    "migration",
                    "struct",
                    "enum",
                ],
            ),
            (
                SubTaskType::Research,
                vec![
                    "search",
                    "find",
                    "look up",
                    "research",
                    "investigate",
                    "what is",
                    "how does",
                    "compare",
                    "alternatives",
                    "benchmark",
                    "documentation",
                    "reference",
                ],
            ),
            (
                SubTaskType::Analysis,
                vec![
                    "analyze",
                    "analyse",
                    "evaluate",
                    "assess",
                    "calculate",
                    "reason",
                    "deduce",
                    "prove",
                    "solve",
                    "math",
                    "logic",
                    "optimize",
                    "performance",
                    "bottleneck",
                ],
            ),
            (
                SubTaskType::Creative,
                vec![
                    "write",
                    "draft",
                    "compose",
                    "create",
                    "generate text",
                    "brainstorm",
                    "ideas",
                    "story",
                    "description",
                    "marketing",
                    "copy",
                    "blog",
                    "article",
                ],
            ),
            (
                SubTaskType::FastLookup,
                vec![
                    "translate",
                    "convert",
                    "format",
                    "reformat",
                    "what time",
                    "define",
                    "synonym",
                    "abbreviation",
                    "unit",
                    "currency",
                ],
            ),
            (
                SubTaskType::Review,
                vec![
                    "review",
                    "check",
                    "verify",
                    "validate",
                    "audit",
                    "inspect",
                    "quality",
                    "correctness",
                    "lint",
                    "security review",
                ],
            ),
            (
                SubTaskType::Summarize,
                vec![
                    "summarize",
                    "summarise",
                    "tldr",
                    "brief",
                    "condense",
                    "digest",
                    "key points",
                    "overview",
                    "recap",
                ],
            ),
            (
                SubTaskType::Planning,
                vec![
                    "plan",
                    "design",
                    "architect",
                    "strategy",
                    "roadmap",
                    "breakdown",
                    "decompose",
                    "organize",
                    "prioritize",
                    "system design",
                    "architecture",
                ],
            ),
            (
                SubTaskType::ToolUse,
                vec![
                    "run", "execute", "shell", "command", "curl", "api call", "install", "deploy",
                    "start", "stop", "restart", "ssh",
                ],
            ),
        ];
        Self { keyword_map }
    }

    /// Classify a text prompt into a `SubTaskType` based on keyword matching.
    ///
    /// Returns the type with the most keyword hits.  Falls back to
    /// `SubTaskType::Analysis` if nothing matches (analysis is the safest
    /// catch-all for complex tasks).
    pub fn classify(&self, text: &str) -> SubTaskType {
        let lower = text.to_lowercase();
        let mut best_type = SubTaskType::Analysis;
        let mut best_score: usize = 0;

        for (task_type, keywords) in &self.keyword_map {
            let score = keywords.iter().filter(|kw| lower.contains(*kw)).count();
            if score > best_score {
                best_score = score;
                best_type = *task_type;
            }
        }
        best_type
    }

    /// Estimate complexity from text length and keyword density.
    ///
    /// Returns a value 1–10.
    pub fn estimate_complexity(&self, text: &str) -> u8 {
        let word_count = text.split_whitespace().count();
        let base = match word_count {
            0..=10 => 2,
            11..=30 => 4,
            31..=80 => 6,
            81..=200 => 8,
            _ => 10,
        };
        // Boost if multiple task-type keywords appear (cross-domain = complex)
        let types_detected = self
            .keyword_map
            .iter()
            .filter(|(_, kws)| {
                let lower = text.to_lowercase();
                kws.iter().any(|kw| lower.contains(kw))
            })
            .count();
        let complexity_boost = types_detected.saturating_sub(1) as u8;
        (base + complexity_boost).min(10)
    }

    /// Decompose a complex task description into subtasks.
    ///
    /// This rule-based version splits on sentence boundaries and classifies
    /// each sentence.  Adjacent sentences of the same type are merged.
    pub fn decompose(&self, task: &str) -> TaskDecomposition {
        let mut decomposition = TaskDecomposition::new(task);

        // Split on sentence boundaries (period, semicolon, newline)
        let sentences: Vec<&str> = task
            .split(['.', ';', '\n'])
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        if sentences.is_empty() {
            // Single atomic task
            let task_type = self.classify(task);
            let complexity = self.estimate_complexity(task);
            let subtask =
                SubTask::new(0, "Complete task", task, task_type).with_complexity(complexity);
            decomposition.add_subtask(subtask);
            return decomposition;
        }

        // Group consecutive sentences of the same type
        let mut groups: Vec<(SubTaskType, Vec<&str>)> = Vec::new();
        for sentence in &sentences {
            let stype = self.classify(sentence);
            if let Some(last) = groups.last_mut()
                && last.0 == stype
            {
                last.1.push(sentence);
                continue;
            }
            groups.push((stype, vec![sentence]));
        }

        // Convert groups to subtasks
        let mut prev_id: Option<Uuid> = None;
        for (idx, (task_type, sents)) in groups.into_iter().enumerate() {
            let merged_prompt = sents.join(". ");
            let title = generate_title(&merged_prompt, task_type);
            let complexity = self.estimate_complexity(&merged_prompt);

            let mut subtask =
                SubTask::new(idx, title, &merged_prompt, task_type).with_complexity(complexity);

            // Sequential dependency: each group depends on the previous
            // (conservative — the planner can relax this later)
            if let Some(dep) = prev_id {
                subtask = subtask.depends_on(dep);
            }

            prev_id = Some(subtask.id);
            decomposition.add_subtask(subtask);
        }

        decomposition
    }

    /// Decompose into independent (parallelizable) subtasks — no dependencies
    /// between them.  Useful when you *know* the sub-prompts are independent.
    pub fn decompose_parallel(&self, prompts: &[&str]) -> TaskDecomposition {
        let original = prompts.join(" | ");
        let mut decomposition = TaskDecomposition::new(&original);

        for (idx, prompt) in prompts.iter().enumerate() {
            let task_type = self.classify(prompt);
            let complexity = self.estimate_complexity(prompt);
            let title = generate_title(prompt, task_type);
            let subtask = SubTask::new(idx, title, *prompt, task_type).with_complexity(complexity);
            decomposition.add_subtask(subtask);
        }

        decomposition
    }
}

/// Generate a short title from the first few words of a prompt.
fn generate_title(prompt: &str, task_type: SubTaskType) -> String {
    let words: Vec<&str> = prompt.split_whitespace().take(6).collect();
    let snippet = words.join(" ");
    if snippet.len() > 40 {
        format!("[{}] {}…", task_type, &snippet[..40])
    } else {
        format!("[{}] {}", task_type, snippet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_code() {
        let d = Decomposer::new();
        assert_eq!(
            d.classify("implement a REST API endpoint"),
            SubTaskType::Code
        );
    }

    #[test]
    fn test_classify_research() {
        let d = Decomposer::new();
        assert_eq!(
            d.classify("research the best alternatives to Redis"),
            SubTaskType::Research
        );
    }

    #[test]
    fn test_classify_fast_lookup() {
        let d = Decomposer::new();
        assert_eq!(
            d.classify("translate this to French"),
            SubTaskType::FastLookup
        );
    }

    #[test]
    fn test_classify_fallback() {
        let d = Decomposer::new();
        // Gibberish → Analysis fallback
        assert_eq!(d.classify("xyzzy foobar qux"), SubTaskType::Analysis);
    }

    #[test]
    fn test_decompose_single() {
        let d = Decomposer::new();
        let result = d.decompose("translate hello to Spanish");
        assert_eq!(result.len(), 1);
        assert_eq!(result.subtasks[0].task_type, SubTaskType::FastLookup);
    }

    #[test]
    fn test_decompose_multi() {
        let d = Decomposer::new();
        let result = d.decompose(
            "Research Redis alternatives. Implement a caching layer. Review the code for security.",
        );
        assert!(result.len() >= 2);
        // First subtask should be a root
        assert!(result.subtasks[0].is_root());
    }

    #[test]
    fn test_decompose_parallel() {
        let d = Decomposer::new();
        let result = d.decompose_parallel(&[
            "search for Rust async patterns",
            "implement the HTTP server",
            "write unit tests",
        ]);
        assert_eq!(result.len(), 3);
        // All should be roots (no dependencies)
        assert!(result.subtasks.iter().all(|s| s.is_root()));
    }

    #[test]
    fn test_complexity_scales() {
        let d = Decomposer::new();
        let short = d.estimate_complexity("fix bug");
        let long = d.estimate_complexity(
            "analyze the performance bottleneck in the database layer, \
             optimize the query execution plan, review the indexing strategy, \
             and implement caching for the most frequently accessed endpoints \
             while ensuring security and correctness",
        );
        assert!(long > short, "long={long}, short={short}");
    }

    #[test]
    fn test_subtask_builder() {
        let dep_id = Uuid::new_v4();
        let st = SubTask::new(0, "test", "do stuff", SubTaskType::Code)
            .depends_on(dep_id)
            .with_complexity(8)
            .with_max_tokens(4096);
        assert_eq!(st.depends_on, vec![dep_id]);
        assert_eq!(st.complexity, 8);
        assert_eq!(st.max_tokens, Some(4096));
        assert!(!st.is_root());
    }

    #[test]
    fn test_root_subtasks() {
        let d = Decomposer::new();
        let result = d.decompose_parallel(&["search something", "code something"]);
        assert_eq!(result.root_subtasks().len(), 2);
    }
}
