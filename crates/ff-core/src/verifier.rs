//! Self-verification scorecard for ForgeFleet.
//!
//! Runs all synthetic probes and produces a weighted health score (0–100).
//!
//! # Categories & Weights
//!
//! | Category | Weight | Probes |
//! |----------|--------|--------|
//! | API | 30 | HttpHealthProbe |
//! | Models | 25 | LlmSmokeProbe |
//! | Storage | 20 | DbWriteReadProbe |
//! | Fleet | 15 | ReplicationLagProbe |
//! | Infra | 10 | DiskSpaceProbe, BackupFreshnessProbe |
//!
//! # Endpoint
//!
//! `GET /api/fleet/scorecard` returns the full scorecard as JSON.
//! Historical scorecards (last 24h) are tracked in memory.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::config::FleetConfig;
use crate::synthetic::{ProbeCategory, ProbeRegistry, ProbeResult, ProbeStatus};

// ─── Scorecard Types ─────────────────────────────────────────────────────────

/// Score for a single health category.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryScore {
    /// Category name.
    pub category: ProbeCategory,
    /// Weight (out of 100).
    pub weight: u32,
    /// Category score (0.0–1.0).
    pub score: f64,
    /// Weighted contribution to overall score.
    pub weighted_score: f64,
    /// Number of probes that passed.
    pub passed: usize,
    /// Number of probes that were degraded.
    pub degraded: usize,
    /// Number of probes that failed.
    pub failed: usize,
    /// Total probes in this category.
    pub total: usize,
    /// Individual probe results.
    pub probes: Vec<ProbeResult>,
}

/// Full health scorecard — the top-level response from `/api/fleet/scorecard`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthScorecard {
    /// Overall fleet health score (0–100).
    pub score: f64,
    /// Health status string: "healthy", "degraded", or "unhealthy".
    pub status: String,
    /// When this scorecard was generated.
    pub timestamp: DateTime<Utc>,
    /// How long all probes took to run.
    pub total_duration: Duration,
    /// Per-category breakdown.
    pub categories: Vec<CategoryScore>,
    /// Total probe count.
    pub total_probes: usize,
    /// Passed probe count.
    pub passed_probes: usize,
    /// Degraded probe count.
    pub degraded_probes: usize,
    /// Failed probe count.
    pub failed_probes: usize,
    /// Alert threshold — score below this triggers notifications.
    pub alert_threshold: f64,
    /// Whether this scorecard triggered an alert.
    pub alerted: bool,
}

impl HealthScorecard {
    /// Determine the status string from the overall score.
    pub fn status_from_score(score: f64) -> String {
        if score >= 90.0 {
            "healthy".into()
        } else if score >= 70.0 {
            "degraded".into()
        } else {
            "unhealthy".into()
        }
    }
}

/// Historical scorecard entry — trimmed for storage efficiency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScorecardSnapshot {
    pub score: f64,
    pub status: String,
    pub timestamp: DateTime<Utc>,
    pub category_scores: HashMap<String, f64>,
    pub total_probes: usize,
    pub failed_probes: usize,
}

impl From<&HealthScorecard> for ScorecardSnapshot {
    fn from(sc: &HealthScorecard) -> Self {
        let mut category_scores = HashMap::new();
        for cat in &sc.categories {
            category_scores.insert(format!("{}", cat.category), cat.score * 100.0);
        }
        Self {
            score: sc.score,
            status: sc.status.clone(),
            timestamp: sc.timestamp,
            category_scores,
            total_probes: sc.total_probes,
            failed_probes: sc.failed_probes,
        }
    }
}

// ─── Scorecard Engine ────────────────────────────────────────────────────────

/// Configuration for the health verifier.
#[derive(Debug, Clone)]
pub struct VerifierConfig {
    /// Score threshold below which an alert is triggered.
    pub alert_threshold: f64,
    /// How long to keep historical scorecards.
    pub history_retention: Duration,
    /// Maximum number of history entries to keep.
    pub max_history_entries: usize,
}

impl Default for VerifierConfig {
    fn default() -> Self {
        Self {
            alert_threshold: 70.0,
            history_retention: Duration::from_secs(24 * 3600),
            max_history_entries: 288, // one every 5 min for 24h
        }
    }
}

/// The self-verification engine that runs probes and produces scorecards.
pub struct HealthVerifier {
    registry: ProbeRegistry,
    config: VerifierConfig,
    history: Arc<Mutex<Vec<ScorecardSnapshot>>>,
}

impl HealthVerifier {
    /// Create a new verifier with the given probe registry and config.
    pub fn new(registry: ProbeRegistry, config: VerifierConfig) -> Self {
        Self {
            registry,
            config,
            history: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Create with default settings and all built-in probes.
    pub fn with_defaults(db_path: impl Into<String>) -> Self {
        Self::new(
            ProbeRegistry::with_defaults(db_path),
            VerifierConfig::default(),
        )
    }

    /// Run all probes and produce a complete scorecard.
    pub async fn run_scorecard(&self, fleet_config: &FleetConfig) -> HealthScorecard {
        let start = std::time::Instant::now();
        let all_results = self.registry.run_all(fleet_config).await;
        let total_duration = start.elapsed();

        // Group results by category
        let mut by_category: HashMap<ProbeCategory, Vec<ProbeResult>> = HashMap::new();
        for result in &all_results {
            // Determine category from probe name
            let category = self.probe_name_to_category(&result.probe_name);
            by_category
                .entry(category)
                .or_default()
                .push(result.clone());
        }

        // Build category scores
        let mut categories = Vec::new();
        let mut overall_score = 0.0;

        for category in ProbeCategory::all() {
            let probes = by_category.remove(category).unwrap_or_default();
            let total = probes.len();

            let (passed, degraded, failed) = count_statuses(&probes);

            let cat_score = if total > 0 {
                probes.iter().map(|p| p.status.score()).sum::<f64>() / total as f64
            } else {
                // No probes for this category — assume healthy (don't penalize)
                1.0
            };

            let weighted = cat_score * category.weight() as f64;
            overall_score += weighted;

            categories.push(CategoryScore {
                category: *category,
                weight: category.weight(),
                score: cat_score,
                weighted_score: weighted,
                passed,
                degraded,
                failed,
                total,
                probes,
            });
        }

        let total_probes = all_results.len();
        let (passed_probes, degraded_probes, failed_probes) = count_statuses(&all_results);

        let status = HealthScorecard::status_from_score(overall_score);
        let alerted = overall_score < self.config.alert_threshold;

        if alerted {
            warn!(
                score = overall_score,
                threshold = self.config.alert_threshold,
                "Health scorecard below alert threshold"
            );
        } else {
            info!(score = overall_score, status = %status, "Health scorecard generated");
        }

        let scorecard = HealthScorecard {
            score: overall_score,
            status,
            timestamp: Utc::now(),
            total_duration,
            categories,
            total_probes,
            passed_probes,
            degraded_probes,
            failed_probes,
            alert_threshold: self.config.alert_threshold,
            alerted,
        };

        // Store snapshot in history
        self.record_snapshot(&scorecard).await;

        scorecard
    }

    /// Record a scorecard snapshot and prune old entries.
    async fn record_snapshot(&self, scorecard: &HealthScorecard) {
        let snapshot = ScorecardSnapshot::from(scorecard);
        let mut history = self.history.lock().await;

        history.push(snapshot);

        // Prune by count
        if history.len() > self.config.max_history_entries {
            let excess = history.len() - self.config.max_history_entries;
            history.drain(..excess);
        }

        // Prune by age
        let cutoff = Utc::now()
            - chrono::Duration::from_std(self.config.history_retention)
                .unwrap_or(chrono::Duration::hours(24));
        history.retain(|s| s.timestamp > cutoff);
    }

    /// Get historical scorecards within the retention window.
    pub async fn history(&self) -> Vec<ScorecardSnapshot> {
        self.history.lock().await.clone()
    }

    /// Get the most recent scorecard snapshot.
    pub async fn last_snapshot(&self) -> Option<ScorecardSnapshot> {
        self.history.lock().await.last().cloned()
    }

    /// Get the trend — score delta between latest and N entries ago.
    pub async fn trend(&self, lookback: usize) -> Option<f64> {
        let history = self.history.lock().await;
        if history.len() < 2 {
            return None;
        }
        let latest = history.last()?;
        let earlier_idx = history.len().saturating_sub(lookback + 1);
        let earlier = history.get(earlier_idx)?;
        Some(latest.score - earlier.score)
    }

    /// Map probe names to categories. This is the source of truth
    /// for which probe belongs to which category.
    fn probe_name_to_category(&self, probe_name: &str) -> ProbeCategory {
        match probe_name {
            "http_health" => ProbeCategory::Api,
            "llm_smoke" => ProbeCategory::Models,
            "db_write_read" => ProbeCategory::Storage,
            "replication_lag" => ProbeCategory::Fleet,
            "disk_space" | "backup_freshness" => ProbeCategory::Infra,
            _ => ProbeCategory::Infra, // default bucket
        }
    }

    /// Access the underlying probe registry.
    pub fn registry(&self) -> &ProbeRegistry {
        &self.registry
    }

    /// Access the verifier config.
    pub fn verifier_config(&self) -> &VerifierConfig {
        &self.config
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn count_statuses(results: &[ProbeResult]) -> (usize, usize, usize) {
    let mut passed = 0;
    let mut degraded = 0;
    let mut failed = 0;
    for r in results {
        match &r.status {
            ProbeStatus::Pass => passed += 1,
            ProbeStatus::Degraded { .. } => degraded += 1,
            ProbeStatus::Fail { .. } => failed += 1,
        }
    }
    (passed, degraded, failed)
}

// ─── JSON Response Types (for API) ──────────────────────────────────────────

/// Compact scorecard response for the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScorecardResponse {
    pub score: f64,
    pub status: String,
    pub timestamp: DateTime<Utc>,
    pub categories: Vec<CategorySummary>,
    pub probes_total: usize,
    pub probes_passed: usize,
    pub probes_failed: usize,
    pub alert_threshold: f64,
    pub alerted: bool,
}

/// Compact category summary (without full probe results).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategorySummary {
    pub name: String,
    pub weight: u32,
    pub score: f64,
    pub passed: usize,
    pub failed: usize,
    pub total: usize,
}

impl From<&HealthScorecard> for ScorecardResponse {
    fn from(sc: &HealthScorecard) -> Self {
        Self {
            score: sc.score,
            status: sc.status.clone(),
            timestamp: sc.timestamp,
            categories: sc
                .categories
                .iter()
                .map(|c| CategorySummary {
                    name: format!("{}", c.category),
                    weight: c.weight,
                    score: c.score * 100.0,
                    passed: c.passed,
                    failed: c.failed,
                    total: c.total,
                })
                .collect(),
            probes_total: sc.total_probes,
            probes_passed: sc.passed_probes,
            probes_failed: sc.failed_probes,
            alert_threshold: sc.alert_threshold,
            alerted: sc.alerted,
        }
    }
}

/// Historical trend response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryResponse {
    pub entries: Vec<ScorecardSnapshot>,
    pub trend: Option<f64>,
    pub retention_hours: f64,
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthetic::{ProbeResult, ProbeStatus};

    #[test]
    fn test_status_from_score() {
        assert_eq!(HealthScorecard::status_from_score(95.0), "healthy");
        assert_eq!(HealthScorecard::status_from_score(90.0), "healthy");
        assert_eq!(HealthScorecard::status_from_score(85.0), "degraded");
        assert_eq!(HealthScorecard::status_from_score(70.0), "degraded");
        assert_eq!(HealthScorecard::status_from_score(69.9), "unhealthy");
        assert_eq!(HealthScorecard::status_from_score(0.0), "unhealthy");
    }

    #[test]
    fn test_count_statuses() {
        let results = vec![
            ProbeResult::new("a", ProbeStatus::Pass, Duration::from_millis(1), None),
            ProbeResult::new("b", ProbeStatus::Pass, Duration::from_millis(2), None),
            ProbeResult::new(
                "c",
                ProbeStatus::Degraded {
                    reason: "slow".into(),
                },
                Duration::from_millis(3),
                None,
            ),
            ProbeResult::new(
                "d",
                ProbeStatus::Fail {
                    reason: "down".into(),
                },
                Duration::from_millis(4),
                None,
            ),
        ];
        let (p, d, f) = count_statuses(&results);
        assert_eq!(p, 2);
        assert_eq!(d, 1);
        assert_eq!(f, 1);
    }

    #[test]
    fn test_scorecard_snapshot_from() {
        let scorecard = HealthScorecard {
            score: 85.0,
            status: "degraded".into(),
            timestamp: Utc::now(),
            total_duration: Duration::from_secs(5),
            categories: vec![CategoryScore {
                category: ProbeCategory::Api,
                weight: 30,
                score: 1.0,
                weighted_score: 30.0,
                passed: 3,
                degraded: 0,
                failed: 0,
                total: 3,
                probes: vec![],
            }],
            total_probes: 3,
            passed_probes: 3,
            degraded_probes: 0,
            failed_probes: 0,
            alert_threshold: 70.0,
            alerted: false,
        };

        let snapshot = ScorecardSnapshot::from(&scorecard);
        assert_eq!(snapshot.score, 85.0);
        assert_eq!(snapshot.status, "degraded");
        assert_eq!(snapshot.total_probes, 3);
        assert_eq!(snapshot.failed_probes, 0);
        assert!(snapshot.category_scores.contains_key("API"));
        assert_eq!(*snapshot.category_scores.get("API").unwrap(), 100.0);
    }

    #[test]
    fn test_scorecard_response_from() {
        let scorecard = HealthScorecard {
            score: 92.5,
            status: "healthy".into(),
            timestamp: Utc::now(),
            total_duration: Duration::from_secs(2),
            categories: vec![
                CategoryScore {
                    category: ProbeCategory::Api,
                    weight: 30,
                    score: 1.0,
                    weighted_score: 30.0,
                    passed: 5,
                    degraded: 0,
                    failed: 0,
                    total: 5,
                    probes: vec![],
                },
                CategoryScore {
                    category: ProbeCategory::Models,
                    weight: 25,
                    score: 0.8,
                    weighted_score: 20.0,
                    passed: 3,
                    degraded: 1,
                    failed: 0,
                    total: 4,
                    probes: vec![],
                },
            ],
            total_probes: 9,
            passed_probes: 8,
            degraded_probes: 1,
            failed_probes: 0,
            alert_threshold: 70.0,
            alerted: false,
        };

        let resp = ScorecardResponse::from(&scorecard);
        assert_eq!(resp.score, 92.5);
        assert_eq!(resp.categories.len(), 2);
        assert_eq!(resp.categories[0].name, "API");
        assert_eq!(resp.categories[0].score, 100.0); // 1.0 * 100
        assert_eq!(resp.categories[1].name, "Models");
        assert_eq!(resp.categories[1].score, 80.0); // 0.8 * 100
    }

    #[test]
    fn test_verifier_config_defaults() {
        let config = VerifierConfig::default();
        assert_eq!(config.alert_threshold, 70.0);
        assert_eq!(config.history_retention, Duration::from_secs(24 * 3600));
        assert_eq!(config.max_history_entries, 288);
    }

    #[tokio::test]
    async fn test_verifier_empty_history() {
        let verifier = HealthVerifier::new(ProbeRegistry::new(), VerifierConfig::default());
        assert!(verifier.history().await.is_empty());
        assert!(verifier.last_snapshot().await.is_none());
        assert!(verifier.trend(5).await.is_none());
    }

    #[tokio::test]
    async fn test_verifier_record_and_prune() {
        let config = VerifierConfig {
            alert_threshold: 70.0,
            history_retention: Duration::from_secs(24 * 3600),
            max_history_entries: 3,
        };
        let verifier = HealthVerifier::new(ProbeRegistry::new(), config);

        // Record 5 snapshots — should prune to 3
        for i in 0..5 {
            let scorecard = HealthScorecard {
                score: 80.0 + i as f64,
                status: "degraded".into(),
                timestamp: Utc::now(),
                total_duration: Duration::from_secs(1),
                categories: vec![],
                total_probes: 0,
                passed_probes: 0,
                degraded_probes: 0,
                failed_probes: 0,
                alert_threshold: 70.0,
                alerted: false,
            };
            verifier.record_snapshot(&scorecard).await;
        }

        let history = verifier.history().await;
        assert_eq!(history.len(), 3);
        // Most recent entries should be kept
        assert_eq!(history.last().unwrap().score, 84.0);
    }
}
