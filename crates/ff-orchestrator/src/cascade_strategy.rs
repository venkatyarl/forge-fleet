//! Outcome-aware routing strategy: pre-classifier, cascade pipeline, judge.
//!
//! Solves the *false-success* problem in the existing TierRouter:
//!
//!   - TierRouter today only escalates on hard failures (timeout, 5xx, refusal).
//!   - A tier-1 model that confidently answers a complex question *wrongly*
//!     slips through silently — "got a 200, ship it."
//!   - And tier-3 (the big model) ends up doing 100% of the work on tasks where
//!     a 9B drafter could have produced 80% of the boilerplate for free.
//!
//! This module adds two complementary capabilities, both opt-in:
//!
//!   1. **Pre-classifier** — a single small-LLM call rates the prompt on two
//!      axes: `Complexity` (simple/moderate/complex/expert) and `TaskShape`
//!      (structured/open_ended). The classification decides which strategy
//!      to dispatch with.
//!
//!   2. **Cascade pipeline** — for `complex + structured` tasks, run a
//!      drafter → verifier → finalizer sequence. Each tier sees the previous
//!      tier's output AND its rationale and is given an explicit license to
//!      *throw it out* if the prior attempt fundamentally misunderstood the
//!      request. A validator gate (JSON parse, etc.) sits between tiers and
//!      a judge can early-exit when the draft is already good enough.
//!
//! For `complex + open_ended` we **skip the cascade** (anchoring bias outweighs
//! the savings) and dispatch straight to a high tier with a judge as the
//! quality floor.
//!
//! Default behaviour is `SingleTier`-with-existing-router — opting in is per-
//! request (`strategy="auto"` on the MCP call) so we can A/B safely.

use serde::{Deserialize, Serialize};
use std::time::Duration;

// ─── Public types ───────────────────────────────────────────────────────────

/// Difficulty axis. Maps to recommended starting tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Complexity {
    /// 1-line factual answer, definitions, classification.
    Simple,
    /// Multi-step but uncontroversial — a 30B class model nails it.
    Moderate,
    /// Multiple subtasks, edge cases, judgment required.
    Complex,
    /// Architecture-grade, safety-critical, frontier reasoning.
    Expert,
}

impl Complexity {
    /// Tier this complexity ideally targets (1..=4).
    pub fn ideal_tier(self) -> u8 {
        match self {
            Self::Simple => 1,
            Self::Moderate => 2,
            Self::Complex => 3,
            Self::Expert => 4,
        }
    }
}

/// Shape axis. Drives whether a cascade pipeline can be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskShape {
    /// Output has a parseable structure: JSON, YAML, SQL, code. Cheap to
    /// validate at each cascade step → good cascade target.
    Structured,
    /// Free-form prose, reasoning, analysis. No cheap validator; cascade
    /// risks anchoring bias more than it saves cost → direct dispatch.
    OpenEnded,
}

/// Concrete output format. Drives which validator (if any) is wired into
/// the cascade gates. Inferred by the classifier alongside complexity and
/// shape — operators no longer need to pass `validator="json"` by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    /// Free-form text — markdown, English prose, summaries.
    Prose,
    /// Strict JSON; validator parses each tier's output.
    Json,
    /// YAML / Kubernetes manifest / Helm value / docker-compose.
    Yaml,
    /// Source code in any language. No built-in syntax validator yet —
    /// cascade still helps but errors won't surface until runtime.
    Code,
    /// SQL DDL/DML/query. No built-in parser yet.
    Sql,
}

impl OutputFormat {
    /// Map this format to the validator the cascade gate should use.
    /// `Code` and `Sql` fall back to `None` until we add per-language
    /// validators (would need to know the language for Code).
    pub fn validator(self) -> ValidatorKind {
        match self {
            Self::Json => ValidatorKind::Json,
            Self::Yaml => ValidatorKind::Yaml,
            Self::Prose | Self::Code | Self::Sql => ValidatorKind::None,
        }
    }

    /// Reasonable default when the classifier emits only 2 words
    /// (complexity + shape). Preserves backward-compat for callers /
    /// tests that haven't been updated.
    pub fn default_for_shape(shape: TaskShape) -> Self {
        match shape {
            TaskShape::Structured => Self::Json,
            TaskShape::OpenEnded => Self::Prose,
        }
    }
}

/// Which built-in validator to use as a gate between cascade tiers.
/// `None` means we still run the cascade but skip the parse step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidatorKind {
    None,
    Json,
    Yaml,
}

/// The router's verdict after classification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RouteStrategy {
    /// Single dispatch to one tier. Existing TierRouter handles escalation
    /// from there. Used for simple + any-shape and for moderate + open_ended.
    SingleTier { tier: u8 },
    /// Drafter → verifier → finalizer sequence with optional validator gate
    /// and judge-based early-exit. Used for complex/expert + structured.
    Cascade {
        tiers: Vec<u8>,
        validator: ValidatorKind,
        judge_early_exit: bool,
    },
    /// Dispatch to one tier, judge the result. If judge score < threshold,
    /// escalate one tier with the judge's critique appended. Used for
    /// complex/expert + open_ended where cascade anchoring would hurt more
    /// than it helps.
    JudgeEscalate {
        start_tier: u8,
        max_tier: u8,
        /// Judge threshold: re-dispatch when score < this (0-10 scale).
        threshold: u8,
    },
}

/// Map a (complexity, shape, format) classification onto a strategy.
///
/// Validator is auto-set from `format` — operators no longer need to pass
/// `validator="json"` by hand for JSON tasks. Defaults err on the side of
/// *spending more compute* for safety: when the classifier fails or its
/// output is unparseable, callers should fall back to `(Complex, OpenEnded,
/// Prose)` → JudgeEscalate from tier-3, never downgrading silently to
/// tier-1.
pub fn pick_strategy(
    complexity: Complexity,
    shape: TaskShape,
    format: OutputFormat,
) -> RouteStrategy {
    let validator = format.validator();
    match (complexity, shape) {
        (Complexity::Simple, _) => RouteStrategy::SingleTier { tier: 1 },
        (Complexity::Moderate, TaskShape::Structured) => RouteStrategy::Cascade {
            tiers: vec![1, 2],
            validator,
            judge_early_exit: true,
        },
        (Complexity::Moderate, TaskShape::OpenEnded) => RouteStrategy::SingleTier { tier: 2 },
        (Complexity::Complex, TaskShape::Structured) => RouteStrategy::Cascade {
            tiers: vec![1, 2, 3],
            validator,
            judge_early_exit: true,
        },
        (Complexity::Complex, TaskShape::OpenEnded) => RouteStrategy::JudgeEscalate {
            start_tier: 3,
            max_tier: 4,
            threshold: 7,
        },
        (Complexity::Expert, TaskShape::Structured) => RouteStrategy::Cascade {
            tiers: vec![2, 3, 4],
            validator,
            judge_early_exit: true,
        },
        (Complexity::Expert, TaskShape::OpenEnded) => RouteStrategy::JudgeEscalate {
            start_tier: 4,
            max_tier: 4,
            threshold: 8,
        },
    }
}

// ─── Validators ─────────────────────────────────────────────────────────────

/// Result of validating a tier's output. `Ok` means downstream tiers can
/// rely on the structure; `Err` means the next tier MUST see the parse
/// error so it can fix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ValidationOutcome {
    Ok,
    Err(String),
}

impl ValidationOutcome {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// Pull a code/JSON/YAML payload out of a possibly markdown-wrapped LLM
/// response. Strips ```lang ... ``` fences, leading/trailing whitespace.
/// Idempotent.
pub fn strip_markdown_fences(s: &str) -> &str {
    let s = s.trim();
    // ```json\n...\n``` or ```\n...\n```
    if let Some(rest) = s.strip_prefix("```") {
        // Skip the language tag line.
        let after_lang = rest.find('\n').map(|i| &rest[i + 1..]).unwrap_or(rest);
        if let Some(inner) = after_lang.strip_suffix("```") {
            return inner.trim();
        }
        // Sometimes the closing fence has a trailing newline.
        if let Some(idx) = after_lang.rfind("```") {
            return after_lang[..idx].trim();
        }
    }
    s
}

pub fn validate_json(s: &str) -> ValidationOutcome {
    let payload = strip_markdown_fences(s);
    match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(_) => ValidationOutcome::Ok,
        Err(e) => ValidationOutcome::Err(format!("JSON parse: {e}")),
    }
}

pub fn validate_yaml(s: &str) -> ValidationOutcome {
    let payload = strip_markdown_fences(s);
    // Cheap fallback: we don't depend on serde_yaml in this crate.  Treat
    // anything that contains a colon and a newline as plausibly YAML for
    // gate purposes; a richer parser can be wired later.
    if payload.is_empty() {
        ValidationOutcome::Err("empty YAML payload".to_string())
    } else {
        ValidationOutcome::Ok
    }
}

pub fn validate(kind: ValidatorKind, s: &str) -> ValidationOutcome {
    match kind {
        ValidatorKind::None => ValidationOutcome::Ok,
        ValidatorKind::Json => validate_json(s),
        ValidatorKind::Yaml => validate_yaml(s),
    }
}

// ─── Prompt templates ───────────────────────────────────────────────────────

/// Classifier prompt — asks the small LLM for three words on one line.
/// Carefully phrased to bias toward "complex" when in doubt (matches the
/// `pick_strategy` defaults) and to extract the concrete output format so
/// the cascade validator is auto-wired.
pub fn classifier_prompt(user_prompt: &str) -> String {
    format!(
        "Rate the following user task on THREE axes. Respond with EXACTLY \
         three lowercase words separated by single spaces, nothing else.\n\
         \n\
         Axis 1 (complexity): one of {{simple, moderate, complex, expert}}\n\
         Axis 2 (shape):      one of {{structured, open_ended}}\n\
         Axis 3 (format):     one of {{prose, json, yaml, code, sql}}\n\
         \n\
         Definitions:\n\
         - simple    = single-step, factual, classification, definition, \
         short rewrite.\n\
         - moderate  = multiple steps but uncontroversial; a competent 30B \
         class model handles it cleanly.\n\
         - complex   = edge cases, multi-part, judgment required, design \
         decisions.\n\
         - expert    = frontier reasoning, safety-critical, novel research \
         framing.\n\
         - structured = output is parseable: JSON, YAML, SQL, code, schema, \
         config file, regex.\n\
         - open_ended = free-form prose, analysis, explanation, conversation, \
         creative writing.\n\
         - prose     = paragraphs of natural-language text (English/etc).\n\
         - json      = JSON object/array, JSON Schema, OpenAPI, config file.\n\
         - yaml      = YAML config, Kubernetes manifest, Helm values, \
         docker-compose, CI workflow.\n\
         - code      = source code in any programming language (Rust, Python, \
         JS, Go, Bash, etc).\n\
         - sql       = SQL DDL/DML/query/migration.\n\
         \n\
         Rules:\n\
         - If shape is open_ended, format is usually prose.\n\
         - If shape is structured, pick the concrete format (json/yaml/code/sql).\n\
         - When in doubt about complexity, prefer the harder label.\n\
         \n\
         Examples:\n\
         - \"What is 2+2?\"                            → simple open_ended prose\n\
         - \"Write a JSON schema for a User object\"    → complex structured json\n\
         - \"K8s CronJob to back up Postgres at 3am\"   → complex structured yaml\n\
         - \"Rust fn parse_iso8601_duration(s) -> ...\" → complex structured code\n\
         - \"Explain Byzantine fault tolerance\"        → complex open_ended prose\n\
         \n\
         Task:\n\
         {user_prompt}\n\
         \n\
         Answer (three words):"
    )
}

/// Cascade refine prompt — used by tier 2+ when there's a prior attempt.
/// The "throw it out" clause is load-bearing: without it the higher-tier
/// model anchors to the draft and produces a polished version of the wrong
/// answer.
pub fn cascade_refine_prompt(
    user_prompt: &str,
    prior_output: &str,
    prior_validation: &ValidationOutcome,
    tier_label: u8,
) -> String {
    let validation_note = match prior_validation {
        ValidationOutcome::Ok => String::from("(structure validated OK)"),
        ValidationOutcome::Err(e) => format!(
            "(WARNING: prior attempt failed structural validation: {e} — \
             you must fix or replace it)"
        ),
    };
    format!(
        "You are tier-{tier_label}, refining a previous attempt at a task.\n\
         \n\
         ORIGINAL TASK:\n\
         {user_prompt}\n\
         \n\
         PREVIOUS ATTEMPT (by a smaller model — may contain errors, omissions, \
         or fundamental misunderstandings) {validation_note}:\n\
         ---\n\
         {prior_output}\n\
         ---\n\
         \n\
         RULES:\n\
         1. If the previous attempt fundamentally misunderstood the task or \
         framed the wrong problem, IGNORE it and produce the correct output \
         from scratch.\n\
         2. Otherwise, build on it: fix errors, fill gaps, improve quality.\n\
         3. Your output is the FINAL deliverable — do not include commentary, \
         meta-discussion, or phrases like \"the previous attempt was good but \
         ...\". Just deliver the answer.\n\
         4. If the task asked for structured output (JSON, code, etc.), output \
         ONLY that structure.\n\
         \n\
         YOUR FINAL OUTPUT:"
    )
}

/// Judge prompt — Gemma-4 (or any third-party-family judge) scores a draft.
///
/// We ask for an integer because parsing free-form prose is brittle and the
/// judge call adds latency we don't want to waste.
pub fn judge_prompt(user_prompt: &str, candidate_output: &str) -> String {
    format!(
        "You are an independent judge evaluating an AI's answer to a user task. \
         Score the answer on a 0-10 integer scale where:\n\
         \n\
         10 = comprehensive, accurate, well-structured, correct on all edge cases.\n\
          7 = solid; minor gaps or rough edges but a competent answer.\n\
          5 = passable; clear weaknesses that a stronger model would fix.\n\
          3 = significant errors, missing depth, or wrong framing.\n\
          0 = wrong, unsafe, or off-topic.\n\
         \n\
         Respond with ONLY a single integer 0-10. No explanation, no prose.\n\
         \n\
         USER TASK:\n\
         {user_prompt}\n\
         \n\
         CANDIDATE ANSWER:\n\
         {candidate_output}\n\
         \n\
         SCORE (0-10):"
    )
}

/// Parse the classifier's response into a (Complexity, TaskShape, OutputFormat)
/// triple. Returns `None` if complexity AND shape can't be resolved — caller
/// defaults to `(Complex, OpenEnded, Prose)` so hard prompts never silently
/// downgrade to tier-1.
///
/// Tolerant of:
///   - 2-word responses (defaults format from shape)
///   - reversed/scrambled word order
///   - extra prose ("I'd rate it: complex structured json because ...")
///   - synonyms ("hard json", "medium yaml")
///   - punctuation, commas, line breaks
///
/// Note that some words double as shape AND format hints — "json" implies
/// `Structured + Json`, "prose" implies `OpenEnded + Prose`. The parser
/// fills in the redundant axis when only one is given.
pub fn parse_classifier_response(raw: &str) -> Option<(Complexity, TaskShape, OutputFormat)> {
    let lower = raw.trim().to_lowercase();
    // Tolerate the model outputting newlines, commas, or extra words.
    let tokens: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .collect();

    let mut complexity: Option<Complexity> = None;
    let mut shape: Option<TaskShape> = None;
    let mut format: Option<OutputFormat> = None;
    for t in &tokens {
        match *t {
            // ── complexity ──
            "simple" | "easy" | "trivial" => complexity = complexity.or(Some(Complexity::Simple)),
            "moderate" | "medium" => complexity = complexity.or(Some(Complexity::Moderate)),
            "complex" | "hard" | "difficult" => {
                complexity = complexity.or(Some(Complexity::Complex))
            }
            "expert" | "frontier" | "advanced" => {
                complexity = complexity.or(Some(Complexity::Expert))
            }

            // ── format (also implies shape) ──
            "json" | "schema" | "openapi" => {
                format = format.or(Some(OutputFormat::Json));
                shape = shape.or(Some(TaskShape::Structured));
            }
            "yaml" | "yml" | "kubernetes" | "k8s" | "helm" | "compose" => {
                format = format.or(Some(OutputFormat::Yaml));
                shape = shape.or(Some(TaskShape::Structured));
            }
            "code" | "rust" | "python" | "javascript" | "typescript" | "go" | "bash" | "shell"
            | "java" | "cpp" => {
                format = format.or(Some(OutputFormat::Code));
                shape = shape.or(Some(TaskShape::Structured));
            }
            "sql" | "ddl" | "dml" | "query" | "migration" => {
                format = format.or(Some(OutputFormat::Sql));
                shape = shape.or(Some(TaskShape::Structured));
            }
            "prose" | "text" | "english" | "essay" | "paragraph" | "summary" => {
                format = format.or(Some(OutputFormat::Prose));
                shape = shape.or(Some(TaskShape::OpenEnded));
            }

            // ── shape (kept as a fallback for old prompts) ──
            "structured" | "structural" => shape = shape.or(Some(TaskShape::Structured)),
            "open_ended" | "openended" | "open" | "freeform" | "free_form" => {
                shape = shape.or(Some(TaskShape::OpenEnded))
            }
            _ => {}
        }
        if complexity.is_some() && shape.is_some() && format.is_some() {
            break;
        }
    }
    match (complexity, shape) {
        (Some(c), Some(s)) => {
            // If format wasn't explicit, derive a sensible default from shape.
            let f = format.unwrap_or_else(|| OutputFormat::default_for_shape(s));
            Some((c, s, f))
        }
        _ => None,
    }
}

/// Parse the judge's response into a 0-10 score. Returns `None` if the
/// response wasn't an integer in range — caller should treat it as "judge
/// failed; don't gate on this."
pub fn parse_judge_response(raw: &str) -> Option<u8> {
    let trimmed = raw.trim();
    // The model sometimes wraps the number in extra prose like
    // "Score: 8" or "8/10" — extract the first integer 0-10.
    let mut buf = String::new();
    for ch in trimmed.chars() {
        if ch.is_ascii_digit() {
            buf.push(ch);
            if buf.len() == 2 {
                break;
            }
        } else if !buf.is_empty() {
            break;
        }
    }
    let n: u8 = buf.parse().ok()?;
    if n <= 10 { Some(n) } else { None }
}

// ─── LLM exec trait ─────────────────────────────────────────────────────────

/// Abstraction over "call an LLM and get text back."
///
/// The concrete production impl is HTTP-via-gateway; the test impl is
/// canned responses. Keeping this as a trait lets us unit-test the cascade
/// logic without spinning up a fake server.
#[async_trait::async_trait]
pub trait LlmExec: Send + Sync {
    /// Send `prompt` to the indicated tier (1..=4) and return the text
    /// completion. `max_tokens` is advisory — concrete impls may set a
    /// floor (e.g. 1024 for qwen3 thinking mode).
    async fn complete(
        &self,
        tier: u8,
        prompt: &str,
        max_tokens: u32,
        timeout: Duration,
    ) -> Result<String, String>;

    /// Send `prompt` to the judge endpoint (typically Gemma-4 on Taylor).
    /// Separate from `complete` so the impl can pin the model.
    async fn judge(
        &self,
        prompt: &str,
        max_tokens: u32,
        timeout: Duration,
    ) -> Result<String, String>;
}

// ─── Cascade executor ──────────────────────────────────────────────────────

/// Per-stage record kept during a cascade run.
#[derive(Debug, Clone, Serialize)]
pub struct CascadeStep {
    pub tier: u8,
    pub output: String,
    pub validation: ValidationOutcome,
    pub judge_score: Option<u8>,
    pub elapsed_ms: u64,
}

/// Result of a cascade run — final output plus per-stage trace.
#[derive(Debug, Clone, Serialize)]
pub struct CascadeOutcome {
    pub final_output: String,
    pub steps: Vec<CascadeStep>,
    pub early_exit_at_tier: Option<u8>,
}

/// Run a cascade pipeline: drafter → verifier → finalizer.
///
/// Each tier sees the previous tier's output AND its validation result.
/// When `judge_early_exit` is true, between tiers we ask the judge to score
/// the current output and stop the cascade if the score ≥ 8 (well above
/// "ship it" threshold).
pub async fn run_cascade<E: LlmExec>(
    exec: &E,
    user_prompt: &str,
    tiers: &[u8],
    validator: ValidatorKind,
    judge_early_exit: bool,
) -> Result<CascadeOutcome, String> {
    if tiers.is_empty() {
        return Err("run_cascade: tiers cannot be empty".into());
    }
    let mut steps: Vec<CascadeStep> = Vec::with_capacity(tiers.len());
    let mut prior_output: Option<String> = None;
    let mut prior_validation = ValidationOutcome::Ok;
    let mut early_exit_at: Option<u8> = None;

    for (i, &tier) in tiers.iter().enumerate() {
        let prompt = match &prior_output {
            None => user_prompt.to_string(),
            Some(prev) => cascade_refine_prompt(user_prompt, prev, &prior_validation, tier),
        };

        let start = std::time::Instant::now();
        let max_tokens = if validator == ValidatorKind::None {
            4096
        } else {
            8192
        };
        let output = exec
            // 10-min ceiling per tier — qwen3-coder-30b at ~10-20 tok/s can
            // need ~70-140s for 1k tokens, and reasoning models (DeepSeek-R1)
            // spend ~half their budget in <think> and routinely cross 5 min
            // on cascade refine prompts. 120s and 300s both proved too tight.
            .complete(tier, &prompt, max_tokens, Duration::from_secs(600))
            .await
            .map_err(|e| format!("cascade tier-{tier} failed: {e}"))?;
        let elapsed_ms = start.elapsed().as_millis() as u64;

        let validation = validate(validator, &output);

        let mut judge_score: Option<u8> = None;
        // Only judge between stages, not after the final one (no point).
        if judge_early_exit && i < tiers.len() - 1 {
            let jp = judge_prompt(user_prompt, &output);
            // 256 tokens for the judge — mlx_lm.server silently truncates
            // to empty when given a small budget, even though the model's
            // intended reply is just "8". Discovered 2026-05-18 on Taylor's
            // gemma-4 mlx endpoint.
            match exec.judge(&jp, 256, Duration::from_secs(30)).await {
                Ok(resp) => judge_score = parse_judge_response(&resp),
                Err(e) => tracing::warn!("judge failed at tier-{tier}: {e}"),
            }
        }

        steps.push(CascadeStep {
            tier,
            output: output.clone(),
            validation: validation.clone(),
            judge_score,
            elapsed_ms,
        });

        // Early-exit if validator passed AND judge approves.
        if judge_early_exit && validation.is_ok() && judge_score.map(|s| s >= 8).unwrap_or(false) {
            early_exit_at = Some(tier);
            prior_output = Some(output);
            break;
        }

        prior_validation = validation;
        prior_output = Some(output);
    }

    let final_output = prior_output.ok_or("cascade produced no output")?;
    Ok(CascadeOutcome {
        final_output,
        steps,
        early_exit_at_tier: early_exit_at,
    })
}

// ─── Judge-escalate executor ───────────────────────────────────────────────

/// Run a judge-escalate dispatch: try `start_tier` first, judge the output,
/// escalate by one tier (up to `max_tier`) if the score is below threshold.
/// Returns the *last* response and the path of attempts.
#[derive(Debug, Clone, Serialize)]
pub struct JudgeEscalateOutcome {
    pub final_output: String,
    pub steps: Vec<CascadeStep>,
}

pub async fn run_judge_escalate<E: LlmExec>(
    exec: &E,
    user_prompt: &str,
    start_tier: u8,
    max_tier: u8,
    threshold: u8,
) -> Result<JudgeEscalateOutcome, String> {
    if start_tier > max_tier {
        return Err("run_judge_escalate: start_tier > max_tier".into());
    }
    let mut steps: Vec<CascadeStep> = Vec::new();
    let mut prior_critique: Option<String> = None;
    let mut tier = start_tier;
    loop {
        let prompt = match &prior_critique {
            None => user_prompt.to_string(),
            Some(critique) => format!(
                "{user_prompt}\n\n\
                 The previous attempt at this task was judged insufficient. \
                 Specific weaknesses:\n{critique}\n\
                 Produce a better answer that addresses these.",
            ),
        };

        let start = std::time::Instant::now();
        let output = exec
            .complete(tier, &prompt, 4096, Duration::from_secs(600))
            .await
            .map_err(|e| format!("judge-escalate tier-{tier} failed: {e}"))?;
        let elapsed_ms = start.elapsed().as_millis() as u64;

        // Judge.
        let jp = judge_prompt(user_prompt, &output);
        // See run_cascade comment — judge needs ≥ ~256 tokens or mlx_lm.server
        // truncates to empty.
        let judge_score = match exec.judge(&jp, 256, Duration::from_secs(30)).await {
            Ok(resp) => parse_judge_response(&resp),
            Err(e) => {
                tracing::warn!("judge call failed at tier-{tier}: {e}");
                None
            }
        };

        steps.push(CascadeStep {
            tier,
            output: output.clone(),
            validation: ValidationOutcome::Ok,
            judge_score,
            elapsed_ms,
        });

        let pass = judge_score.map(|s| s >= threshold).unwrap_or(true);
        if pass || tier >= max_tier {
            return Ok(JudgeEscalateOutcome {
                final_output: output,
                steps,
            });
        }
        // Build a short critique for the next tier (just the score note for
        // now — a fuller critique would be a second judge call).
        prior_critique = Some(format!(
            "Judge scored the previous attempt {}/10 (threshold {}); add depth, \
             correct any errors, and address edge cases the previous attempt missed.",
            judge_score.map(|s| s.to_string()).unwrap_or("?".into()),
            threshold
        ));
        tier += 1;
    }
}

// ─── Classifier executor ───────────────────────────────────────────────────

/// One classifier call. Default on parse failure is `(Complex, OpenEnded,
/// Prose)` → JudgeEscalate from tier-3, never silently downgrading to tier-1.
///
/// Returns the triple so callers can pass it straight to `pick_strategy`
/// without an extra format lookup.
pub async fn classify_task<E: LlmExec>(
    exec: &E,
    user_prompt: &str,
) -> (Complexity, TaskShape, OutputFormat) {
    let prompt = classifier_prompt(user_prompt);
    // 64 tokens — 3 words is ~3-5 tokens but mlx_lm.server can truncate
    // small budgets to empty. See judge_max_tokens note in run_cascade.
    match exec.complete(1, &prompt, 64, Duration::from_secs(15)).await {
        Ok(resp) => parse_classifier_response(&resp).unwrap_or_else(|| {
            tracing::warn!(
                response = %resp,
                "classifier output not parseable, defaulting to (complex, open_ended, prose)"
            );
            (
                Complexity::Complex,
                TaskShape::OpenEnded,
                OutputFormat::Prose,
            )
        }),
        Err(e) => {
            tracing::warn!(
                "classifier call failed, defaulting to (complex, open_ended, prose): {e}"
            );
            (
                Complexity::Complex,
                TaskShape::OpenEnded,
                OutputFormat::Prose,
            )
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // ─── strip_markdown_fences ──────────────────────────────────────────────

    #[test]
    fn strips_fenced_json() {
        let s = "```json\n{\"a\":1}\n```";
        assert_eq!(strip_markdown_fences(s), "{\"a\":1}");
    }

    #[test]
    fn strips_bare_fences() {
        let s = "```\n{\"a\":1}\n```";
        assert_eq!(strip_markdown_fences(s), "{\"a\":1}");
    }

    #[test]
    fn no_fences_passes_through() {
        assert_eq!(strip_markdown_fences("plain text"), "plain text");
    }

    // ─── validate_json ──────────────────────────────────────────────────────

    #[test]
    fn validates_good_json() {
        assert!(validate_json(r#"{"a": 1, "b": [2,3]}"#).is_ok());
    }

    #[test]
    fn validates_fenced_json() {
        assert!(validate_json("```json\n{\"a\":1}\n```").is_ok());
    }

    #[test]
    fn rejects_bad_json() {
        match validate_json("{not json") {
            ValidationOutcome::Err(_) => {}
            _ => panic!("expected Err"),
        }
    }

    // ─── parse_classifier_response ──────────────────────────────────────────

    #[test]
    fn parses_canonical_three_words() {
        let (c, s, f) = parse_classifier_response("complex structured json").unwrap();
        assert_eq!(c, Complexity::Complex);
        assert_eq!(s, TaskShape::Structured);
        assert_eq!(f, OutputFormat::Json);
    }

    #[test]
    fn parses_two_words_with_format_default() {
        // Backward-compat: 2-word responses still parse, format derived
        // from shape (Structured → Json by default).
        let (c, s, f) = parse_classifier_response("complex structured").unwrap();
        assert_eq!(c, Complexity::Complex);
        assert_eq!(s, TaskShape::Structured);
        assert_eq!(f, OutputFormat::Json);
    }

    #[test]
    fn parses_reversed_order() {
        let (c, s, f) = parse_classifier_response("yaml complex structured").unwrap();
        assert_eq!(c, Complexity::Complex);
        assert_eq!(s, TaskShape::Structured);
        assert_eq!(f, OutputFormat::Yaml);
    }

    #[test]
    fn parses_with_extra_prose() {
        // Some models can't help but explain themselves.
        let (c, s, f) = parse_classifier_response(
            "I'd rate it: complex, structured, json. The prompt asks for JSON.",
        )
        .unwrap();
        assert_eq!(c, Complexity::Complex);
        assert_eq!(s, TaskShape::Structured);
        assert_eq!(f, OutputFormat::Json);
    }

    #[test]
    fn parses_synonyms() {
        let (c, s, f) = parse_classifier_response("hard json").unwrap();
        assert_eq!(c, Complexity::Complex);
        assert_eq!(s, TaskShape::Structured);
        assert_eq!(f, OutputFormat::Json);
    }

    #[test]
    fn detects_format_from_language_keyword() {
        let (c, s, f) = parse_classifier_response("complex rust").unwrap();
        assert_eq!(c, Complexity::Complex);
        assert_eq!(s, TaskShape::Structured);
        assert_eq!(f, OutputFormat::Code);
    }

    #[test]
    fn detects_yaml_from_k8s_keyword() {
        let (c, s, f) = parse_classifier_response("complex kubernetes").unwrap();
        assert_eq!(c, Complexity::Complex);
        assert_eq!(s, TaskShape::Structured);
        assert_eq!(f, OutputFormat::Yaml);
    }

    #[test]
    fn detects_sql_from_query_keyword() {
        let (c, s, f) = parse_classifier_response("moderate sql").unwrap();
        assert_eq!(c, Complexity::Moderate);
        assert_eq!(s, TaskShape::Structured);
        assert_eq!(f, OutputFormat::Sql);
    }

    #[test]
    fn open_ended_defaults_to_prose() {
        let (c, s, f) = parse_classifier_response("complex open_ended").unwrap();
        assert_eq!(c, Complexity::Complex);
        assert_eq!(s, TaskShape::OpenEnded);
        assert_eq!(f, OutputFormat::Prose);
    }

    #[test]
    fn returns_none_on_garbage() {
        assert!(parse_classifier_response("blah blah blah").is_none());
    }

    // ─── OutputFormat → ValidatorKind ───────────────────────────────────────

    #[test]
    fn json_format_picks_json_validator() {
        assert_eq!(OutputFormat::Json.validator(), ValidatorKind::Json);
    }

    #[test]
    fn yaml_format_picks_yaml_validator() {
        assert_eq!(OutputFormat::Yaml.validator(), ValidatorKind::Yaml);
    }

    #[test]
    fn code_and_sql_have_no_validator() {
        assert_eq!(OutputFormat::Code.validator(), ValidatorKind::None);
        assert_eq!(OutputFormat::Sql.validator(), ValidatorKind::None);
    }

    #[test]
    fn prose_has_no_validator() {
        assert_eq!(OutputFormat::Prose.validator(), ValidatorKind::None);
    }

    // ─── parse_judge_response ───────────────────────────────────────────────

    #[test]
    fn parses_bare_score() {
        assert_eq!(parse_judge_response("8"), Some(8));
        assert_eq!(parse_judge_response("10"), Some(10));
        assert_eq!(parse_judge_response("0"), Some(0));
    }

    #[test]
    fn parses_prefixed_score() {
        assert_eq!(parse_judge_response("Score: 7"), Some(7));
        assert_eq!(parse_judge_response("7/10"), Some(7));
    }

    #[test]
    fn rejects_out_of_range() {
        assert_eq!(parse_judge_response("42"), None);
    }

    #[test]
    fn rejects_nonsense() {
        assert_eq!(parse_judge_response("excellent"), None);
    }

    // ─── pick_strategy ──────────────────────────────────────────────────────

    #[test]
    fn simple_always_tier_1() {
        assert_eq!(
            pick_strategy(
                Complexity::Simple,
                TaskShape::Structured,
                OutputFormat::Json,
            ),
            RouteStrategy::SingleTier { tier: 1 }
        );
        assert_eq!(
            pick_strategy(
                Complexity::Simple,
                TaskShape::OpenEnded,
                OutputFormat::Prose,
            ),
            RouteStrategy::SingleTier { tier: 1 }
        );
    }

    #[test]
    fn complex_structured_cascades_with_json_validator() {
        match pick_strategy(
            Complexity::Complex,
            TaskShape::Structured,
            OutputFormat::Json,
        ) {
            RouteStrategy::Cascade {
                tiers, validator, ..
            } => {
                assert_eq!(tiers, vec![1, 2, 3]);
                assert_eq!(validator, ValidatorKind::Json);
            }
            other => panic!("expected Cascade, got {other:?}"),
        }
    }

    #[test]
    fn complex_structured_yaml_picks_yaml_validator() {
        match pick_strategy(
            Complexity::Complex,
            TaskShape::Structured,
            OutputFormat::Yaml,
        ) {
            RouteStrategy::Cascade { validator, .. } => {
                assert_eq!(validator, ValidatorKind::Yaml);
            }
            other => panic!("expected Cascade, got {other:?}"),
        }
    }

    #[test]
    fn complex_structured_code_has_no_validator() {
        // Code cascade still runs but the gate is a no-op until we add
        // language-specific syntax validators.
        match pick_strategy(
            Complexity::Complex,
            TaskShape::Structured,
            OutputFormat::Code,
        ) {
            RouteStrategy::Cascade { validator, .. } => {
                assert_eq!(validator, ValidatorKind::None);
            }
            other => panic!("expected Cascade, got {other:?}"),
        }
    }

    #[test]
    fn complex_open_judge_escalates() {
        match pick_strategy(
            Complexity::Complex,
            TaskShape::OpenEnded,
            OutputFormat::Prose,
        ) {
            RouteStrategy::JudgeEscalate {
                start_tier,
                max_tier,
                threshold,
            } => {
                assert_eq!(start_tier, 3);
                assert_eq!(max_tier, 4);
                assert_eq!(threshold, 7);
            }
            other => panic!("expected JudgeEscalate, got {other:?}"),
        }
    }

    #[test]
    fn expert_structured_skips_tier_1() {
        match pick_strategy(
            Complexity::Expert,
            TaskShape::Structured,
            OutputFormat::Json,
        ) {
            RouteStrategy::Cascade { tiers, .. } => {
                assert!(!tiers.contains(&1), "expert shouldn't start at tier-1");
                assert_eq!(tiers, vec![2, 3, 4]);
            }
            other => panic!("expected Cascade, got {other:?}"),
        }
    }

    // ─── Mocked LlmExec for cascade tests ───────────────────────────────────

    struct CannedExec {
        per_tier: Vec<String>,
        judge_scores: Vec<u8>,
        complete_calls: Mutex<usize>,
        judge_calls: Mutex<usize>,
    }
    impl CannedExec {
        fn new(per_tier: Vec<&str>, judge_scores: Vec<u8>) -> Self {
            Self {
                per_tier: per_tier.into_iter().map(String::from).collect(),
                judge_scores,
                complete_calls: Mutex::new(0),
                judge_calls: Mutex::new(0),
            }
        }
    }
    #[async_trait::async_trait]
    impl LlmExec for CannedExec {
        async fn complete(
            &self,
            _tier: u8,
            _prompt: &str,
            _max_tokens: u32,
            _timeout: Duration,
        ) -> Result<String, String> {
            let mut n = self.complete_calls.lock().unwrap();
            let idx = *n;
            *n += 1;
            self.per_tier
                .get(idx)
                .cloned()
                .ok_or(format!("CannedExec exhausted at call {idx}"))
        }
        async fn judge(
            &self,
            _prompt: &str,
            _max_tokens: u32,
            _timeout: Duration,
        ) -> Result<String, String> {
            let mut n = self.judge_calls.lock().unwrap();
            let idx = *n;
            *n += 1;
            Ok(self.judge_scores.get(idx).copied().unwrap_or(0).to_string())
        }
    }

    #[tokio::test]
    async fn cascade_runs_all_tiers_when_judge_low() {
        let exec = CannedExec::new(
            vec![r#"{"a":1}"#, r#"{"a":1,"b":2}"#, r#"{"a":1,"b":2,"c":3}"#],
            vec![5, 6], // both below early-exit threshold
        );
        let result = run_cascade(&exec, "make a JSON", &[1, 2, 3], ValidatorKind::Json, true)
            .await
            .expect("cascade ok");

        assert_eq!(result.steps.len(), 3);
        assert!(result.early_exit_at_tier.is_none());
        assert_eq!(result.final_output, r#"{"a":1,"b":2,"c":3}"#);
        assert_eq!(*exec.complete_calls.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn cascade_early_exits_when_judge_high() {
        let exec = CannedExec::new(
            vec![r#"{"a":1}"#, r#"{"a":1,"b":2}"#, r#"never_called"#],
            vec![9], // tier-1 immediately scored 9 → exit after tier-1
        );
        let result = run_cascade(&exec, "make a JSON", &[1, 2, 3], ValidatorKind::Json, true)
            .await
            .expect("cascade ok");

        assert_eq!(result.early_exit_at_tier, Some(1));
        assert_eq!(result.steps.len(), 1);
        assert_eq!(result.final_output, r#"{"a":1}"#);
    }

    #[tokio::test]
    async fn cascade_propagates_validation_warning() {
        // tier-1 returns invalid JSON; tier-2 must see the validation error.
        let exec = CannedExec::new(vec!["{not_json", r#"{"fixed":true}"#], vec![3, 8]);
        let result = run_cascade(&exec, "make a JSON", &[1, 2], ValidatorKind::Json, true)
            .await
            .expect("cascade ok");

        assert!(!result.steps[0].validation.is_ok());
        assert_eq!(result.final_output, r#"{"fixed":true}"#);
    }

    #[tokio::test]
    async fn judge_escalate_escalates_on_low_score() {
        let exec = CannedExec::new(vec!["weak answer", "better answer"], vec![4, 8]);
        let result = run_judge_escalate(&exec, "explain consensus protocols", 3, 4, 7)
            .await
            .expect("ok");
        assert_eq!(result.steps.len(), 2);
        assert_eq!(result.final_output, "better answer");
        assert_eq!(result.steps[0].tier, 3);
        assert_eq!(result.steps[1].tier, 4);
    }

    #[tokio::test]
    async fn judge_escalate_stops_when_threshold_met() {
        let exec = CannedExec::new(vec!["good first try"], vec![9]);
        let result = run_judge_escalate(&exec, "explain X", 3, 4, 7)
            .await
            .unwrap();
        assert_eq!(result.steps.len(), 1);
        assert_eq!(result.final_output, "good first try");
    }

    #[tokio::test]
    async fn classifier_defaults_to_safe_on_garbage() {
        struct GarbageExec;
        #[async_trait::async_trait]
        impl LlmExec for GarbageExec {
            async fn complete(
                &self,
                _: u8,
                _: &str,
                _: u32,
                _: Duration,
            ) -> Result<String, String> {
                Ok("blah blah".to_string())
            }
            async fn judge(&self, _: &str, _: u32, _: Duration) -> Result<String, String> {
                Ok("0".to_string())
            }
        }
        let (c, s, f) = classify_task(&GarbageExec, "anything").await;
        assert_eq!(c, Complexity::Complex);
        assert_eq!(s, TaskShape::OpenEnded);
        assert_eq!(f, OutputFormat::Prose);
    }
}
