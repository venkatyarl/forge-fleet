//! Model quality tracking with exponential moving average (EMA) scoring.
//!
//! Tracks success/failure/quality per model per task type, decays old scores
//! over time, and provides rankings for adaptive routing decisions.
//!
//! Persistence is via JSON serialization — callers can store/load from
//! SQLite's `config_kv` table or any other backend.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::classifier::TaskType;

// ─── Configuration ───────────────────────────────────────────────────────────

/// Configuration for the quality tracker.
#[derive(Debug, Clone)]
pub struct QualityTrackerConfig {
    /// EMA decay factor (0.0–1.0). Higher = more weight on recent observations.
    /// Default: 0.3 (70% weight on historical, 30% on new observation).
    pub ema_alpha: f64,

    /// Minimum number of samples before quality data is considered reliable.
    pub min_samples: u32,

    /// Maximum age of the last observation before we start decaying toward the
    /// prior (default: 24 hours).
    pub staleness_threshold: Duration,

    /// How much to decay stale scores toward the prior (0.5 = mean).
    pub staleness_decay: f64,
}

impl Default for QualityTrackerConfig {
    fn default() -> Self {
        Self {
            ema_alpha: 0.3,
            min_samples: 5,
            staleness_threshold: Duration::from_secs(24 * 3600),
            staleness_decay: 0.1,
        }
    }
}

// ─── Quality Score ───────────────────────────────────────────────────────────

/// Quality score for a (model, task_type) pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityScore {
    /// Exponential moving average of outcome quality (0.0–1.0).
    pub ema_score: f64,
    /// Total number of observations.
    pub sample_count: u32,
    /// Number of successful outcomes.
    pub success_count: u32,
    /// Number of failed outcomes.
    pub failure_count: u32,
    /// Average latency in milliseconds.
    pub avg_latency_ms: f64,
    /// Unix timestamp of the last observation.
    pub last_updated: u64,
}

impl QualityScore {
    /// Create a new quality score with no observations.
    fn new() -> Self {
        Self {
            ema_score: 0.5, // neutral prior
            sample_count: 0,
            success_count: 0,
            failure_count: 0,
            avg_latency_ms: 0.0,
            last_updated: now_unix(),
        }
    }

    /// Is this score backed by enough data to be meaningful?
    pub fn is_confident(&self, min_samples: u32) -> bool {
        self.sample_count >= min_samples
    }
}

// ─── Outcome Recording ──────────────────────────────────────────────────────

/// Outcome of a model invocation.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// Quality score for this outcome (0.0 = complete failure, 1.0 = perfect).
    pub quality: f64,
    /// Whether the request succeeded (got a valid response).
    pub success: bool,
    /// Latency in milliseconds.
    pub latency_ms: f64,
}

impl Outcome {
    /// Quick constructor for a successful outcome.
    pub fn success(latency_ms: f64) -> Self {
        Self {
            quality: 1.0,
            success: true,
            latency_ms,
        }
    }

    /// Quick constructor for a failed outcome.
    pub fn failure(latency_ms: f64) -> Self {
        Self {
            quality: 0.0,
            success: false,
            latency_ms,
        }
    }

    /// Partial success with a custom quality score.
    pub fn partial(quality: f64, latency_ms: f64) -> Self {
        Self {
            quality: quality.clamp(0.0, 1.0),
            success: quality > 0.5,
            latency_ms,
        }
    }
}

// ─── Model Ranking ───────────────────────────────────────────────────────────

/// A model's ranking for a specific task type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRanking {
    pub model_id: String,
    pub tier: u8,
    pub score: f64,
    pub sample_count: u32,
    pub avg_latency_ms: f64,
    pub confident: bool,
}

// ─── Persistence Snapshot ────────────────────────────────────────────────────

/// Serializable snapshot of all quality data, for persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualitySnapshot {
    pub version: u32,
    pub scores: HashMap<String, HashMap<String, QualityScore>>,
    pub saved_at: u64,
}

// ─── Composite Key Helper ────────────────────────────────────────────────────

/// Composite key: "model_id::task_type"
fn composite_key(model_id: &str, task_type: TaskType) -> String {
    format!("{model_id}::{}", task_type.as_str())
}

fn parse_composite_key(key: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = key.splitn(2, "::").collect();
    if parts.len() == 2 {
        Some((parts[0].to_string(), parts[1].to_string()))
    } else {
        None
    }
}

// ─── Quality Tracker ─────────────────────────────────────────────────────────

/// Thread-safe quality tracker for model performance.
///
/// Tracks quality scores per (model, task_type) pair using exponential moving
/// averages. Designed for concurrent access from multiple request handlers.
#[derive(Debug)]
pub struct QualityTracker {
    config: QualityTrackerConfig,
    /// Scores indexed by composite key "model_id::task_type".
    scores: DashMap<String, QualityScore>,
}

impl QualityTracker {
    /// Create a new quality tracker with the given configuration.
    pub fn new(config: QualityTrackerConfig) -> Self {
        Self {
            config,
            scores: DashMap::new(),
        }
    }

    /// Create a quality tracker with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(QualityTrackerConfig::default())
    }

    /// Record an outcome for a model on a specific task type.
    pub fn record(&self, model_id: &str, task_type: TaskType, outcome: &Outcome) {
        let key = composite_key(model_id, task_type);
        let alpha = self.config.ema_alpha;

        let mut entry = self.scores.entry(key).or_insert_with(QualityScore::new);
        let score = entry.value_mut();

        // Update EMA score
        score.ema_score = alpha * outcome.quality + (1.0 - alpha) * score.ema_score;

        // Update counters
        score.sample_count += 1;
        if outcome.success {
            score.success_count += 1;
        } else {
            score.failure_count += 1;
        }

        // Update latency (running average)
        let n = score.sample_count as f64;
        score.avg_latency_ms =
            score.avg_latency_ms * ((n - 1.0) / n) + outcome.latency_ms * (1.0 / n);

        score.last_updated = now_unix();

        debug!(
            model = model_id,
            task = task_type.as_str(),
            quality = outcome.quality,
            ema = score.ema_score,
            samples = score.sample_count,
            "recorded quality outcome"
        );
    }

    /// Get the quality score for a specific model + task type.
    pub fn get_score(&self, model_id: &str, task_type: TaskType) -> Option<QualityScore> {
        let key = composite_key(model_id, task_type);
        self.scores.get(&key).map(|entry| {
            let mut score = entry.value().clone();
            self.apply_staleness_decay(&mut score);
            score
        })
    }

    /// Rank all tracked models for a given task type, best first.
    ///
    /// Models with enough samples are ranked by EMA score (descending).
    /// Models without enough samples are ranked after confident ones.
    pub fn rank_models(
        &self,
        task_type: TaskType,
        model_tiers: &HashMap<String, u8>,
    ) -> Vec<ModelRanking> {
        let suffix = format!("::{}", task_type.as_str());
        let mut rankings: Vec<ModelRanking> = Vec::new();

        for entry in self.scores.iter() {
            let key = entry.key();
            if !key.ends_with(&suffix) {
                continue;
            }

            if let Some((model_id, _)) = parse_composite_key(key) {
                let mut score = entry.value().clone();
                self.apply_staleness_decay(&mut score);

                let tier = model_tiers.get(&model_id).copied().unwrap_or(4);
                let confident = score.is_confident(self.config.min_samples);

                rankings.push(ModelRanking {
                    model_id,
                    tier,
                    score: score.ema_score,
                    sample_count: score.sample_count,
                    avg_latency_ms: score.avg_latency_ms,
                    confident,
                });
            }
        }

        // Sort: confident first, then by score descending
        rankings.sort_by(|a, b| {
            b.confident.cmp(&a.confident).then_with(|| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });

        rankings
    }

    /// Check if we have confident data for a task type.
    pub fn has_confident_data(&self, task_type: TaskType, min_samples: u32) -> bool {
        let suffix = format!("::{}", task_type.as_str());
        self.scores.iter().any(|entry| {
            entry.key().ends_with(&suffix) && entry.value().sample_count >= min_samples
        })
    }

    // ── Persistence ──────────────────────────────────────────────────────────

    /// Export all quality data as a JSON string for persistence.
    pub fn export_json(&self) -> Result<String, serde_json::Error> {
        let snapshot = self.to_snapshot();
        serde_json::to_string(&snapshot)
    }

    /// Import quality data from a JSON string.
    pub fn import_json(&self, json: &str) -> Result<(), String> {
        let snapshot: QualitySnapshot =
            serde_json::from_str(json).map_err(|e| format!("invalid quality snapshot: {e}"))?;

        if snapshot.version != 1 {
            warn!(
                version = snapshot.version,
                "unknown quality snapshot version, attempting import anyway"
            );
        }

        for (model_id, task_scores) in snapshot.scores {
            for (task_type_str, score) in task_scores {
                let key = format!("{model_id}::{task_type_str}");
                self.scores.insert(key, score);
            }
        }

        debug!(entries = self.scores.len(), "imported quality snapshot");
        Ok(())
    }

    /// Create a snapshot of all data.
    fn to_snapshot(&self) -> QualitySnapshot {
        let mut scores: HashMap<String, HashMap<String, QualityScore>> = HashMap::new();

        for entry in self.scores.iter() {
            if let Some((model_id, task_type)) = parse_composite_key(entry.key()) {
                scores
                    .entry(model_id)
                    .or_default()
                    .insert(task_type, entry.value().clone());
            }
        }

        QualitySnapshot {
            version: 1,
            scores,
            saved_at: now_unix(),
        }
    }

    /// Reset all quality data.
    pub fn clear(&self) {
        self.scores.clear();
    }

    // ── Staleness ────────────────────────────────────────────────────────────

    /// Apply staleness decay to a score if it's older than the threshold.
    fn apply_staleness_decay(&self, score: &mut QualityScore) {
        let now = now_unix();
        let age_secs = now.saturating_sub(score.last_updated);
        let threshold_secs = self.config.staleness_threshold.as_secs();

        if age_secs > threshold_secs {
            // How many threshold-periods have elapsed?
            let periods = age_secs as f64 / threshold_secs as f64;
            let decay = self.config.staleness_decay;
            // Decay toward 0.5 (neutral prior)
            let factor = (1.0 - decay).powf(periods);
            score.ema_score = 0.5 + (score.ema_score - 0.5) * factor;
        }
    }
}

/// Create a shared quality tracker wrapped in Arc.
pub fn shared_tracker(config: QualityTrackerConfig) -> Arc<QualityTracker> {
    Arc::new(QualityTracker::new(config))
}

// ─── Time Helper ─────────────────────────────────────────────────────────────

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ═════════════════════════════════════════════════════════════════════════════
// Tests
// ═════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker() -> QualityTracker {
        QualityTracker::with_defaults()
    }

    #[test]
    fn test_record_success_updates_ema() {
        let t = tracker();
        t.record("model-a", TaskType::Code, &Outcome::success(100.0));

        let score = t.get_score("model-a", TaskType::Code).unwrap();
        // Initial EMA = 0.5, after one success (quality=1.0):
        // new_ema = 0.3 * 1.0 + 0.7 * 0.5 = 0.65
        assert!((score.ema_score - 0.65).abs() < 0.01);
        assert_eq!(score.sample_count, 1);
        assert_eq!(score.success_count, 1);
        assert_eq!(score.failure_count, 0);
    }

    #[test]
    fn test_record_failure_decreases_ema() {
        let t = tracker();
        // Record several successes first
        for _ in 0..5 {
            t.record("model-a", TaskType::Code, &Outcome::success(100.0));
        }
        let before = t.get_score("model-a", TaskType::Code).unwrap().ema_score;

        // Record a failure
        t.record("model-a", TaskType::Code, &Outcome::failure(5000.0));
        let after = t.get_score("model-a", TaskType::Code).unwrap().ema_score;

        assert!(after < before, "failure should decrease EMA score");
    }

    #[test]
    fn test_multiple_records_converge() {
        let t = tracker();
        // Record many successes — should converge toward 1.0
        for _ in 0..50 {
            t.record("model-a", TaskType::Chat, &Outcome::success(50.0));
        }
        let score = t.get_score("model-a", TaskType::Chat).unwrap();
        assert!(
            score.ema_score > 0.95,
            "should converge near 1.0 after many successes"
        );
    }

    #[test]
    fn test_partial_quality() {
        let t = tracker();
        t.record("model-a", TaskType::Code, &Outcome::partial(0.7, 200.0));
        let score = t.get_score("model-a", TaskType::Code).unwrap();
        // 0.3 * 0.7 + 0.7 * 0.5 = 0.21 + 0.35 = 0.56
        assert!((score.ema_score - 0.56).abs() < 0.01);
    }

    #[test]
    fn test_avg_latency() {
        let t = tracker();
        t.record("model-a", TaskType::Code, &Outcome::success(100.0));
        t.record("model-a", TaskType::Code, &Outcome::success(200.0));
        t.record("model-a", TaskType::Code, &Outcome::success(300.0));

        let score = t.get_score("model-a", TaskType::Code).unwrap();
        assert!(
            (score.avg_latency_ms - 200.0).abs() < 1.0,
            "avg latency should be ~200ms, got {}",
            score.avg_latency_ms
        );
    }

    #[test]
    fn test_rank_models_by_quality() {
        let config = QualityTrackerConfig {
            min_samples: 1,
            ..Default::default()
        };
        let t = QualityTracker::new(config);

        // model-a: good quality
        for _ in 0..5 {
            t.record("model-a", TaskType::Code, &Outcome::success(100.0));
        }
        // model-b: poor quality
        for _ in 0..5 {
            t.record("model-b", TaskType::Code, &Outcome::failure(100.0));
        }

        let tiers: HashMap<String, u8> = [("model-a".to_string(), 2), ("model-b".to_string(), 2)]
            .into_iter()
            .collect();

        let rankings = t.rank_models(TaskType::Code, &tiers);
        assert_eq!(rankings.len(), 2);
        assert_eq!(
            rankings[0].model_id, "model-a",
            "higher quality model should rank first"
        );
    }

    #[test]
    fn test_confident_vs_unconfident() {
        let config = QualityTrackerConfig {
            min_samples: 10,
            ..Default::default()
        };
        let t = QualityTracker::new(config);

        // Only 3 samples — not confident
        for _ in 0..3 {
            t.record("model-a", TaskType::Code, &Outcome::success(100.0));
        }

        let score = t.get_score("model-a", TaskType::Code).unwrap();
        assert!(!score.is_confident(10));

        // Add more to reach confidence
        for _ in 0..7 {
            t.record("model-a", TaskType::Code, &Outcome::success(100.0));
        }

        let score = t.get_score("model-a", TaskType::Code).unwrap();
        assert!(score.is_confident(10));
    }

    #[test]
    fn test_has_confident_data() {
        let config = QualityTrackerConfig {
            min_samples: 5,
            ..Default::default()
        };
        let t = QualityTracker::new(config);

        assert!(!t.has_confident_data(TaskType::Code, 5));

        for _ in 0..5 {
            t.record("model-a", TaskType::Code, &Outcome::success(100.0));
        }

        assert!(t.has_confident_data(TaskType::Code, 5));
        assert!(!t.has_confident_data(TaskType::Chat, 5)); // different task type
    }

    #[test]
    fn test_separate_task_types() {
        let t = tracker();
        t.record("model-a", TaskType::Code, &Outcome::success(100.0));
        t.record("model-a", TaskType::Chat, &Outcome::failure(50.0));

        let code_score = t.get_score("model-a", TaskType::Code).unwrap();
        let chat_score = t.get_score("model-a", TaskType::Chat).unwrap();

        assert!(code_score.ema_score > chat_score.ema_score);
    }

    #[test]
    fn test_json_roundtrip() {
        let t = tracker();
        t.record("model-a", TaskType::Code, &Outcome::success(100.0));
        t.record("model-b", TaskType::Chat, &Outcome::failure(200.0));

        let json = t.export_json().unwrap();

        let t2 = tracker();
        t2.import_json(&json).unwrap();

        let score_a = t2.get_score("model-a", TaskType::Code).unwrap();
        let score_b = t2.get_score("model-b", TaskType::Chat).unwrap();

        assert_eq!(score_a.sample_count, 1);
        assert_eq!(score_b.sample_count, 1);
    }

    #[test]
    fn test_clear() {
        let t = tracker();
        t.record("model-a", TaskType::Code, &Outcome::success(100.0));
        assert!(t.get_score("model-a", TaskType::Code).is_some());

        t.clear();
        assert!(t.get_score("model-a", TaskType::Code).is_none());
    }

    #[test]
    fn test_no_score_returns_none() {
        let t = tracker();
        assert!(t.get_score("nonexistent", TaskType::Code).is_none());
    }

    #[test]
    fn test_outcome_constructors() {
        let s = Outcome::success(100.0);
        assert!(s.success);
        assert_eq!(s.quality, 1.0);

        let f = Outcome::failure(100.0);
        assert!(!f.success);
        assert_eq!(f.quality, 0.0);

        let p = Outcome::partial(0.75, 100.0);
        assert!(p.success);
        assert_eq!(p.quality, 0.75);

        // Clamp out-of-range
        let clamped = Outcome::partial(1.5, 100.0);
        assert_eq!(clamped.quality, 1.0);
    }
}
