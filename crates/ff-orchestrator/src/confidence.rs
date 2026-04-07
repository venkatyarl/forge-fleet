//! Confidence-based escalation — extract confidence from model responses
//! and decide whether to escalate to a higher tier or flag for human review.
//!
//! # How it works
//!
//! 1. A model produces a response for a subtask.
//! 2. [`ConfidenceExtractor`] analyzes the response text for hedging language,
//!    uncertainty markers, and other signals.
//! 3. If the [`ConfidenceScore`] is below the escalation threshold, the task
//!    is re-routed to a higher-tier model.
//! 4. If below the critical threshold, the task is flagged for human review.
//!
//! # Confidence Trends
//!
//! The [`ConfidenceTracker`] accumulates scores per (model, task_type) pair,
//! allowing the system to learn which models are reliable for which tasks.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use ff_core::Tier;

use crate::decomposer::SubTaskType;

// ─── Confidence Score ────────────────────────────────────────────────────────

/// A confidence score in the range [0.0, 1.0].
///
/// - 1.0 = model is fully confident in its response
/// - 0.7+ = acceptable (no escalation needed)
/// - 0.3–0.7 = uncertain (escalate to higher tier)
/// - < 0.3 = very uncertain (flag for human review)
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ConfidenceScore(f64);

impl ConfidenceScore {
    /// Create a new confidence score, clamped to [0.0, 1.0].
    pub fn new(value: f64) -> Self {
        Self(value.clamp(0.0, 1.0))
    }

    /// Get the raw f64 value.
    pub fn value(self) -> f64 {
        self.0
    }

    /// Check if the score is above a threshold.
    pub fn is_above(self, threshold: f64) -> bool {
        self.0 >= threshold
    }

    /// Check if the score is below a threshold.
    pub fn is_below(self, threshold: f64) -> bool {
        self.0 < threshold
    }

    /// A fully confident score (1.0).
    pub fn certain() -> Self {
        Self(1.0)
    }

    /// A completely uncertain score (0.0).
    pub fn unknown() -> Self {
        Self(0.0)
    }
}

impl std::fmt::Display for ConfidenceScore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.1}%", self.0 * 100.0)
    }
}

impl Default for ConfidenceScore {
    fn default() -> Self {
        Self(0.5)
    }
}

// ─── Escalation Decision ────────────────────────────────────────────────────

/// What to do based on a confidence assessment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationDecision {
    /// Confidence is high enough — accept the response.
    Accept,
    /// Confidence is below threshold — escalate to a higher-tier model.
    Escalate {
        /// The tier to escalate to.
        target_tier: Tier,
        /// Why escalation was triggered.
        reason: String,
    },
    /// Confidence is critically low — flag for human review.
    HumanReview {
        /// Why human review is needed.
        reason: String,
    },
}

impl std::fmt::Display for EscalationDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Accept => write!(f, "Accept"),
            Self::Escalate { target_tier, .. } => write!(f, "Escalate → {target_tier}"),
            Self::HumanReview { .. } => write!(f, "⚠ Human Review Required"),
        }
    }
}

// ─── Escalation Config ──────────────────────────────────────────────────────

/// Configuration thresholds for confidence-based escalation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EscalationConfig {
    /// Below this threshold, escalate to a higher tier (default: 0.7).
    pub escalation_threshold: f64,
    /// Below this threshold, flag for human review (default: 0.3).
    pub critical_threshold: f64,
    /// Maximum number of escalation attempts before giving up (default: 2).
    pub max_escalations: u32,
}

impl Default for EscalationConfig {
    fn default() -> Self {
        Self {
            escalation_threshold: 0.7,
            critical_threshold: 0.3,
            max_escalations: 2,
        }
    }
}

impl EscalationConfig {
    /// Create a stricter config (higher thresholds).
    pub fn strict() -> Self {
        Self {
            escalation_threshold: 0.85,
            critical_threshold: 0.5,
            max_escalations: 3,
        }
    }

    /// Create a more lenient config (lower thresholds).
    pub fn lenient() -> Self {
        Self {
            escalation_threshold: 0.5,
            critical_threshold: 0.2,
            max_escalations: 1,
        }
    }

    /// Evaluate a confidence score and current tier, returning a decision.
    pub fn evaluate(
        &self,
        score: ConfidenceScore,
        current_tier: Tier,
        escalation_count: u32,
    ) -> EscalationDecision {
        if score.is_above(self.escalation_threshold) {
            return EscalationDecision::Accept;
        }

        if score.is_below(self.critical_threshold) {
            return EscalationDecision::HumanReview {
                reason: format!(
                    "Confidence {score} is below critical threshold ({:.0}%)",
                    self.critical_threshold * 100.0
                ),
            };
        }

        // Below escalation threshold but above critical — try escalating
        if escalation_count >= self.max_escalations {
            return EscalationDecision::HumanReview {
                reason: format!(
                    "Confidence {score} still below threshold after {escalation_count} escalations"
                ),
            };
        }

        // Try next tier
        let target_tier = match current_tier {
            Tier::Tier1 => Tier::Tier2,
            Tier::Tier2 => Tier::Tier3,
            Tier::Tier3 => Tier::Tier4,
            Tier::Tier4 => {
                // Already at max tier — flag for human review
                return EscalationDecision::HumanReview {
                    reason: format!("Confidence {score} below threshold and already at max tier"),
                };
            }
        };

        EscalationDecision::Escalate {
            target_tier,
            reason: format!(
                "Confidence {score} below escalation threshold ({:.0}%)",
                self.escalation_threshold * 100.0
            ),
        }
    }
}

// ─── Confidence Extractor ────────────────────────────────────────────────────

/// Extracts a confidence score from a model's response text.
///
/// Uses heuristic analysis: counts hedging language, uncertainty markers,
/// and other textual signals to estimate confidence.
pub struct ConfidenceExtractor {
    /// Words/phrases that indicate uncertainty (lower confidence).
    hedging_phrases: Vec<&'static str>,
    /// Words/phrases that indicate confidence (higher confidence).
    confident_phrases: Vec<&'static str>,
}

impl Default for ConfidenceExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfidenceExtractor {
    /// Create a new extractor with default phrase lists.
    pub fn new() -> Self {
        Self {
            hedging_phrases: vec![
                "i'm not sure",
                "i am not sure",
                "i'm not certain",
                "i think",
                "i believe",
                "maybe",
                "perhaps",
                "might",
                "could be",
                "possibly",
                "it seems",
                "it appears",
                "not entirely clear",
                "unclear",
                "uncertain",
                "hard to say",
                "difficult to determine",
                "i don't know",
                "i don't have enough",
                "cannot determine",
                "without more information",
                "this is speculative",
                "take this with a grain of salt",
                "caveat",
                "disclaimer",
                "approximately",
                "roughly",
                "may or may not",
                "it depends",
                "not guaranteed",
                "i would guess",
                "my best guess",
            ],
            confident_phrases: vec![
                "definitely",
                "certainly",
                "clearly",
                "obviously",
                "without doubt",
                "absolutely",
                "i am confident",
                "i'm confident",
                "the answer is",
                "this is correct",
                "verified",
                "confirmed",
                "proven",
                "no doubt",
                "undoubtedly",
                "precisely",
                "exactly",
                "i can confirm",
            ],
        }
    }

    /// Extract a confidence score from response text.
    ///
    /// The algorithm:
    /// 1. Start at base confidence (0.7 for non-empty responses).
    /// 2. Count hedging phrases → reduce confidence.
    /// 3. Count confident phrases → increase confidence.
    /// 4. Adjust for response length (very short = lower confidence).
    /// 5. Clamp to [0.0, 1.0].
    pub fn extract(&self, response: &str) -> ConfidenceScore {
        if response.trim().is_empty() {
            return ConfidenceScore::unknown();
        }

        let lower = response.to_lowercase();
        let word_count = response.split_whitespace().count();

        // Base confidence for non-empty responses
        let mut confidence: f64 = 0.7;

        // Count hedging phrases
        let hedge_count = self
            .hedging_phrases
            .iter()
            .filter(|p| lower.contains(**p))
            .count();

        // Count confident phrases
        let confident_count = self
            .confident_phrases
            .iter()
            .filter(|p| lower.contains(**p))
            .count();

        // Each hedging phrase reduces confidence
        confidence -= hedge_count as f64 * 0.08;

        // Each confident phrase increases confidence
        confidence += confident_count as f64 * 0.05;

        // Very short responses get penalized (less than 20 words)
        if word_count < 20 {
            confidence -= 0.1;
        }

        // Very long, detailed responses get a small boost
        if word_count > 200 {
            confidence += 0.05;
        }

        // Check for explicit confidence markers (e.g. "Confidence: 85%")
        if let Some(explicit) = Self::parse_explicit_confidence(&lower) {
            // Blend: 70% explicit, 30% heuristic
            confidence = explicit * 0.7 + confidence * 0.3;
        }

        ConfidenceScore::new(confidence)
    }

    /// Try to parse an explicit confidence statement from the text.
    ///
    /// Looks for patterns like "confidence: 85%", "confidence: 0.85",
    /// "confidence level: high".
    fn parse_explicit_confidence(text: &str) -> Option<f64> {
        // Look for "confidence: XX%" or "confidence: 0.XX"
        for line in text.lines() {
            let line = line.trim().to_lowercase();
            if !line.contains("confidence") {
                continue;
            }

            // Try "confidence: XX%"
            if let Some(pos) = line.find('%') {
                // Walk backwards from % to find the number
                let before = &line[..pos];
                let num_str: String = before
                    .chars()
                    .rev()
                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
                if let Ok(pct) = num_str.parse::<f64>() {
                    if (0.0..=100.0).contains(&pct) {
                        return Some(pct / 100.0);
                    }
                }
            }

            // Try "confidence: 0.XX"
            for word in line.split_whitespace() {
                if let Ok(val) = word
                    .trim_matches(|c: char| !c.is_ascii_digit() && c != '.')
                    .parse::<f64>()
                {
                    if (0.0..=1.0).contains(&val) && word.contains('.') {
                        return Some(val);
                    }
                }
            }

            // Try qualitative: "confidence: high/medium/low"
            if line.contains("very high") || line.contains("extremely confident") {
                return Some(0.95);
            }
            if line.contains("high") {
                return Some(0.85);
            }
            if line.contains("medium") || line.contains("moderate") {
                return Some(0.6);
            }
            if line.contains("low") {
                return Some(0.3);
            }
            if line.contains("very low") {
                return Some(0.15);
            }
        }

        None
    }
}

// ─── Confidence Assessment ──────────────────────────────────────────────────

/// A complete confidence assessment of a model response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceAssessment {
    /// Unique assessment ID.
    pub id: Uuid,
    /// Which subtask this assessment is for.
    pub subtask_id: Uuid,
    /// The model that produced the response.
    pub model_id: String,
    /// The tier the model was running at.
    pub tier: Tier,
    /// The extracted confidence score.
    pub score: ConfidenceScore,
    /// The escalation decision.
    pub decision: EscalationDecision,
    /// How many times this subtask has been escalated already.
    pub escalation_count: u32,
    /// When this assessment was made.
    pub assessed_at: DateTime<Utc>,
}

impl ConfidenceAssessment {
    /// Create a new assessment.
    pub fn new(
        subtask_id: Uuid,
        model_id: impl Into<String>,
        tier: Tier,
        response: &str,
        config: &EscalationConfig,
        escalation_count: u32,
    ) -> Self {
        let extractor = ConfidenceExtractor::new();
        let score = extractor.extract(response);
        let decision = config.evaluate(score, tier, escalation_count);

        Self {
            id: Uuid::new_v4(),
            subtask_id,
            model_id: model_id.into(),
            tier,
            score,
            decision,
            escalation_count,
            assessed_at: Utc::now(),
        }
    }

    /// Whether this assessment recommends accepting the response.
    pub fn is_accepted(&self) -> bool {
        matches!(self.decision, EscalationDecision::Accept)
    }

    /// Whether this assessment recommends escalation.
    pub fn needs_escalation(&self) -> bool {
        matches!(self.decision, EscalationDecision::Escalate { .. })
    }

    /// Whether this assessment flags for human review.
    pub fn needs_human_review(&self) -> bool {
        matches!(self.decision, EscalationDecision::HumanReview { .. })
    }
}

// ─── Confidence Tracker ─────────────────────────────────────────────────────

/// Key for tracking confidence trends: (model_id, task_type).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TrackerKey {
    pub model_id: String,
    pub task_type: SubTaskType,
}

/// Accumulated confidence statistics for a (model, task_type) pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceStats {
    /// Total number of assessments.
    pub count: u64,
    /// Sum of all confidence scores (for computing mean).
    pub sum: f64,
    /// Minimum confidence score seen.
    pub min: f64,
    /// Maximum confidence score seen.
    pub max: f64,
    /// Number of times escalation was triggered.
    pub escalation_count: u64,
    /// Number of times human review was flagged.
    pub human_review_count: u64,
    /// Last updated timestamp.
    pub last_updated: DateTime<Utc>,
}

impl ConfidenceStats {
    /// Create fresh stats.
    fn new() -> Self {
        Self {
            count: 0,
            sum: 0.0,
            min: 1.0,
            max: 0.0,
            escalation_count: 0,
            human_review_count: 0,
            last_updated: Utc::now(),
        }
    }

    /// Record a new assessment.
    fn record(&mut self, assessment: &ConfidenceAssessment) {
        let val = assessment.score.value();
        self.count += 1;
        self.sum += val;
        if val < self.min {
            self.min = val;
        }
        if val > self.max {
            self.max = val;
        }
        if assessment.needs_escalation() {
            self.escalation_count += 1;
        }
        if assessment.needs_human_review() {
            self.human_review_count += 1;
        }
        self.last_updated = Utc::now();
    }

    /// Average confidence score.
    pub fn mean(&self) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        self.sum / self.count as f64
    }

    /// Escalation rate (fraction of assessments that triggered escalation).
    pub fn escalation_rate(&self) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        self.escalation_count as f64 / self.count as f64
    }

    /// Human review rate.
    pub fn human_review_rate(&self) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        self.human_review_count as f64 / self.count as f64
    }
}

/// Tracks confidence trends per (model, task_type) pair.
///
/// Used to learn over time which models are reliable for which task types,
/// informing future routing decisions.
pub struct ConfidenceTracker {
    stats: HashMap<TrackerKey, ConfidenceStats>,
}

impl Default for ConfidenceTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfidenceTracker {
    /// Create an empty tracker.
    pub fn new() -> Self {
        Self {
            stats: HashMap::new(),
        }
    }

    /// Record a confidence assessment.
    pub fn record(
        &mut self,
        model_id: &str,
        task_type: SubTaskType,
        assessment: &ConfidenceAssessment,
    ) {
        let key = TrackerKey {
            model_id: model_id.to_string(),
            task_type,
        };
        self.stats
            .entry(key)
            .or_insert_with(ConfidenceStats::new)
            .record(assessment);
    }

    /// Get stats for a specific (model, task_type) pair.
    pub fn get_stats(&self, model_id: &str, task_type: SubTaskType) -> Option<&ConfidenceStats> {
        let key = TrackerKey {
            model_id: model_id.to_string(),
            task_type,
        };
        self.stats.get(&key)
    }

    /// Get the average confidence for a model across all task types.
    pub fn model_average(&self, model_id: &str) -> Option<f64> {
        let entries: Vec<&ConfidenceStats> = self
            .stats
            .iter()
            .filter(|(k, _)| k.model_id == model_id)
            .map(|(_, v)| v)
            .collect();

        if entries.is_empty() {
            return None;
        }

        let total_sum: f64 = entries.iter().map(|s| s.sum).sum();
        let total_count: u64 = entries.iter().map(|s| s.count).sum();
        if total_count == 0 {
            return None;
        }
        Some(total_sum / total_count as f64)
    }

    /// Get the best model for a task type (highest average confidence).
    pub fn best_model_for(&self, task_type: SubTaskType) -> Option<(&str, f64)> {
        self.stats
            .iter()
            .filter(|(k, _)| k.task_type == task_type)
            .filter(|(_, v)| v.count > 0)
            .max_by(|(_, a), (_, b)| {
                a.mean()
                    .partial_cmp(&b.mean())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(k, v)| (k.model_id.as_str(), v.mean()))
    }

    /// Get all tracked (model, task_type) pairs and their stats.
    pub fn all_stats(&self) -> &HashMap<TrackerKey, ConfidenceStats> {
        &self.stats
    }

    /// Number of tracked pairs.
    pub fn len(&self) -> usize {
        self.stats.len()
    }

    /// Whether the tracker has no data.
    pub fn is_empty(&self) -> bool {
        self.stats.is_empty()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── ConfidenceScore ──────────────────────────────────────────────────

    #[test]
    fn test_score_clamping() {
        assert_eq!(ConfidenceScore::new(1.5).value(), 1.0);
        assert_eq!(ConfidenceScore::new(-0.5).value(), 0.0);
        assert_eq!(ConfidenceScore::new(0.7).value(), 0.7);
    }

    #[test]
    fn test_score_thresholds() {
        let s = ConfidenceScore::new(0.5);
        assert!(s.is_above(0.3));
        assert!(s.is_below(0.7));
        assert!(!s.is_above(0.7));
    }

    #[test]
    fn test_score_display() {
        let s = ConfidenceScore::new(0.85);
        assert_eq!(s.to_string(), "85.0%");
    }

    #[test]
    fn test_score_constants() {
        assert_eq!(ConfidenceScore::certain().value(), 1.0);
        assert_eq!(ConfidenceScore::unknown().value(), 0.0);
    }

    // ── EscalationConfig ─────────────────────────────────────────────────

    #[test]
    fn test_evaluate_accept() {
        let config = EscalationConfig::default();
        let decision = config.evaluate(ConfidenceScore::new(0.8), Tier::Tier1, 0);
        assert_eq!(decision, EscalationDecision::Accept);
    }

    #[test]
    fn test_evaluate_escalate() {
        let config = EscalationConfig::default();
        let decision = config.evaluate(ConfidenceScore::new(0.5), Tier::Tier1, 0);
        match &decision {
            EscalationDecision::Escalate { target_tier, .. } => {
                assert_eq!(*target_tier, Tier::Tier2);
            }
            other => panic!("expected Escalate, got {other:?}"),
        }
    }

    #[test]
    fn test_evaluate_human_review() {
        let config = EscalationConfig::default();
        let decision = config.evaluate(ConfidenceScore::new(0.2), Tier::Tier1, 0);
        assert!(matches!(decision, EscalationDecision::HumanReview { .. }));
    }

    #[test]
    fn test_evaluate_max_escalations() {
        let config = EscalationConfig::default();
        // Confidence 0.5, already escalated 2 times (max)
        let decision = config.evaluate(ConfidenceScore::new(0.5), Tier::Tier3, 2);
        assert!(matches!(decision, EscalationDecision::HumanReview { .. }));
    }

    #[test]
    fn test_evaluate_at_max_tier() {
        let config = EscalationConfig::default();
        let decision = config.evaluate(ConfidenceScore::new(0.5), Tier::Tier4, 0);
        assert!(matches!(decision, EscalationDecision::HumanReview { .. }));
    }

    #[test]
    fn test_strict_config() {
        let config = EscalationConfig::strict();
        // 0.8 would pass default but fail strict
        let decision = config.evaluate(ConfidenceScore::new(0.8), Tier::Tier1, 0);
        assert!(matches!(decision, EscalationDecision::Escalate { .. }));
    }

    // ── ConfidenceExtractor ──────────────────────────────────────────────

    #[test]
    fn test_extract_empty() {
        let ext = ConfidenceExtractor::new();
        let score = ext.extract("");
        assert_eq!(score.value(), 0.0);
    }

    #[test]
    fn test_extract_hedging_lowers_confidence() {
        let ext = ConfidenceExtractor::new();
        let confident_response = "The answer is 42. This is correct and verified.";
        let hedging_response =
            "I think the answer might be 42, but I'm not sure. Perhaps it could be something else.";

        let confident_score = ext.extract(confident_response);
        let hedging_score = ext.extract(hedging_response);

        assert!(
            confident_score.value() > hedging_score.value(),
            "confident ({}) should be > hedging ({})",
            confident_score.value(),
            hedging_score.value()
        );
    }

    #[test]
    fn test_extract_explicit_percentage() {
        let ext = ConfidenceExtractor::new();
        let response = "Based on my analysis, the answer is X.\n\nConfidence: 90%\n\nHere's why...";
        let score = ext.extract(response);
        // Should be heavily weighted toward 0.9
        assert!(score.value() > 0.75);
    }

    #[test]
    fn test_extract_explicit_qualitative() {
        let ext = ConfidenceExtractor::new();
        let response = "The function needs refactoring.\n\nConfidence: high";
        let score = ext.extract(response);
        assert!(score.value() > 0.7);
    }

    #[test]
    fn test_short_response_penalty() {
        let ext = ConfidenceExtractor::new();
        let short = "Yes.";
        let long = "Yes, the function should use a HashMap instead of a Vec for O(1) lookups. \
                     This will significantly improve performance for the use case described, \
                     especially when dealing with large datasets where linear search becomes \
                     a bottleneck.";

        let short_score = ext.extract(short);
        let long_score = ext.extract(long);

        assert!(
            long_score.value() > short_score.value(),
            "long ({}) should be > short ({})",
            long_score.value(),
            short_score.value()
        );
    }

    // ── ConfidenceAssessment ─────────────────────────────────────────────

    #[test]
    fn test_assessment_accept() {
        let config = EscalationConfig::default();
        let assessment = ConfidenceAssessment::new(
            Uuid::new_v4(),
            "qwen3-72b",
            Tier::Tier3,
            "The answer is definitely 42. This is correct and verified. I can confirm this is right based on multiple sources and thorough analysis of the underlying data structures.",
            &config,
            0,
        );
        assert!(assessment.is_accepted());
        assert!(!assessment.needs_escalation());
        assert!(!assessment.needs_human_review());
    }

    #[test]
    fn test_assessment_escalate() {
        let config = EscalationConfig::default();
        let assessment = ConfidenceAssessment::new(
            Uuid::new_v4(),
            "qwen3-9b",
            Tier::Tier1,
            "I think maybe the answer could be 42, but I'm not sure. Perhaps it might be something else. It depends on the context.",
            &config,
            0,
        );
        assert!(assessment.needs_escalation() || assessment.needs_human_review());
    }

    // ── ConfidenceTracker ────────────────────────────────────────────────

    #[test]
    fn test_tracker_record_and_query() {
        let mut tracker = ConfidenceTracker::new();
        let config = EscalationConfig::default();

        let a1 = ConfidenceAssessment::new(
            Uuid::new_v4(),
            "qwen3-32b",
            Tier::Tier2,
            "The answer is definitely correct. I am confident in this analysis.",
            &config,
            0,
        );
        tracker.record("qwen3-32b", SubTaskType::Code, &a1);

        let stats = tracker.get_stats("qwen3-32b", SubTaskType::Code).unwrap();
        assert_eq!(stats.count, 1);
        assert!(stats.mean() > 0.0);
    }

    #[test]
    fn test_tracker_model_average() {
        let mut tracker = ConfidenceTracker::new();
        let config = EscalationConfig::default();

        // Record across multiple task types
        for task_type in [SubTaskType::Code, SubTaskType::Research] {
            let a = ConfidenceAssessment::new(
                Uuid::new_v4(),
                "qwen3-72b",
                Tier::Tier3,
                "The answer is clear and verified in this comprehensive analysis.",
                &config,
                0,
            );
            tracker.record("qwen3-72b", task_type, &a);
        }

        let avg = tracker.model_average("qwen3-72b");
        assert!(avg.is_some());
        assert!(avg.unwrap() > 0.0);
    }

    #[test]
    fn test_tracker_best_model() {
        let mut tracker = ConfidenceTracker::new();
        let config = EscalationConfig::default();

        // Good model
        let good = ConfidenceAssessment::new(
            Uuid::new_v4(),
            "qwen3-72b",
            Tier::Tier3,
            "The answer is definitely correct. Verified and confirmed with strong evidence.",
            &config,
            0,
        );
        tracker.record("qwen3-72b", SubTaskType::Code, &good);

        // Weaker model
        let weak = ConfidenceAssessment::new(
            Uuid::new_v4(),
            "qwen3-9b",
            Tier::Tier1,
            "I think maybe this could be right, perhaps not sure, it depends.",
            &config,
            0,
        );
        tracker.record("qwen3-9b", SubTaskType::Code, &weak);

        let (best_model, _) = tracker.best_model_for(SubTaskType::Code).unwrap();
        assert_eq!(best_model, "qwen3-72b");
    }

    #[test]
    fn test_tracker_empty() {
        let tracker = ConfidenceTracker::new();
        assert!(tracker.is_empty());
        assert_eq!(tracker.len(), 0);
        assert!(tracker.model_average("nonexistent").is_none());
        assert!(tracker.best_model_for(SubTaskType::Code).is_none());
    }

    #[test]
    fn test_stats_rates() {
        let stats = ConfidenceStats {
            count: 10,
            sum: 6.0,
            min: 0.2,
            max: 0.9,
            escalation_count: 3,
            human_review_count: 1,
            last_updated: Utc::now(),
        };
        assert!((stats.mean() - 0.6).abs() < 0.01);
        assert!((stats.escalation_rate() - 0.3).abs() < 0.01);
        assert!((stats.human_review_rate() - 0.1).abs() < 0.01);
    }

    // ── Serialization ────────────────────────────────────────────────────

    #[test]
    fn test_confidence_score_serde() {
        let s = ConfidenceScore::new(0.85);
        let json = serde_json::to_string(&s).unwrap();
        let back: ConfidenceScore = serde_json::from_str(&json).unwrap();
        assert!((back.value() - 0.85).abs() < 0.001);
    }

    #[test]
    fn test_escalation_decision_serde() {
        let d = EscalationDecision::Escalate {
            target_tier: Tier::Tier3,
            reason: "too uncertain".into(),
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: EscalationDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn test_escalation_config_serde() {
        let c = EscalationConfig::strict();
        let json = serde_json::to_string(&c).unwrap();
        let back: EscalationConfig = serde_json::from_str(&json).unwrap();
        assert!((back.escalation_threshold - 0.85).abs() < 0.001);
    }
}
