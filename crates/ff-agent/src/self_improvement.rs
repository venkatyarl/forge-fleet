//! Self-Improvement Loops — P3 agentic meta-cognition.
//!
//! Monitors the agent's own performance, identifies patterns in failures,
//! and proposes or applies structural improvements to its operation.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// A single performance observation.
#[derive(Debug, Clone)]
pub struct PerformanceSample {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub task_type: String,
    pub duration_ms: u64,
    pub success: bool,
    pub error_kind: Option<String>,
    pub tokens_used: usize,
    pub model: String,
}

/// Aggregated insight from performance history.
#[derive(Debug, Clone)]
pub struct ImprovementInsight {
    pub category: String,
    pub frequency: usize,
    pub suggested_action: String,
    pub confidence: f32, // 0.0–1.0
}

/// Self-improvement engine.
pub struct SelfImprovementLoop {
    history: Arc<RwLock<Vec<PerformanceSample>>>,
    insights: Arc<RwLock<Vec<ImprovementInsight>>>,
    max_history: usize,
}

impl SelfImprovementLoop {
    pub fn new(max_history: usize) -> Self {
        Self {
            history: Arc::new(RwLock::new(Vec::new())),
            insights: Arc::new(RwLock::new(Vec::new())),
            max_history,
        }
    }

    /// Record a new performance sample.
    pub async fn record(&self, sample: PerformanceSample) {
        let mut hist = self.history.write().await;
        hist.push(sample);
        if hist.len() > self.max_history {
            hist.remove(0);
        }
    }

    /// Run the analysis loop and generate insights.
    pub async fn analyze(&self) -> Vec<ImprovementInsight> {
        let hist = self.history.read().await;
        let mut by_error: HashMap<String, Vec<&PerformanceSample>> = HashMap::new();
        let mut by_model: HashMap<String, (u64, usize)> = HashMap::new(); // (total_ms, count)

        for s in hist.iter() {
            if !s.success
                && let Some(ref err) = s.error_kind
            {
                by_error.entry(err.clone()).or_default().push(s);
            }
            let entry = by_model.entry(s.model.clone()).or_insert((0, 0));
            entry.0 += s.duration_ms;
            entry.1 += 1;
        }

        let mut insights = Vec::new();

        // Identify recurring error patterns
        for (error_kind, samples) in by_error {
            if samples.len() >= 3 {
                insights.push(ImprovementInsight {
                    category: "recurring_error".to_string(),
                    frequency: samples.len(),
                    suggested_action: format!(
                        "Consider retry logic or fallback model for error kind: {}",
                        error_kind
                    ),
                    confidence: (samples.len() as f32 / hist.len() as f32).min(1.0),
                });
            }
        }

        // Identify slow models
        for (model, (total_ms, count)) in by_model {
            let avg = total_ms / count as u64;
            if avg > 30_000 && count >= 3 {
                insights.push(ImprovementInsight {
                    category: "slow_model".to_string(),
                    frequency: count,
                    suggested_action: format!(
                        "Model {} averages {}ms per task; consider quantizing or routing to faster hardware",
                        model, avg
                    ),
                    confidence: 0.7,
                });
            }
        }

        // Token-efficiency insight
        let total_tokens: usize = hist.iter().map(|s| s.tokens_used).sum();
        let total_tasks = hist.len();
        if total_tasks > 0 {
            let avg_tokens = total_tokens / total_tasks;
            if avg_tokens > 8000 {
                insights.push(ImprovementInsight {
                    category: "token_efficiency".to_string(),
                    frequency: total_tasks,
                    suggested_action: format!(
                        "Average {} tokens/task; consider context-window pruning or smaller model for simple tasks",
                        avg_tokens
                    ),
                    confidence: 0.6,
                });
            }
        }

        let mut store = self.insights.write().await;
        *store = insights.clone();
        info!(
            "Self-improvement analysis complete: {} insights generated",
            insights.len()
        );
        insights
    }

    /// Get current insights without re-analyzing.
    pub async fn current_insights(&self) -> Vec<ImprovementInsight> {
        self.insights.read().await.clone()
    }

    /// Auto-apply low-risk improvements (e.g., add retry config).
    pub async fn apply_low_risk(&self) -> Vec<String> {
        let insights = self.insights.read().await;
        let mut applied = Vec::new();
        for insight in insights.iter() {
            if insight.confidence > 0.8 && insight.category == "recurring_error" {
                warn!(
                    "Auto-applying retry policy for: {}",
                    insight.suggested_action
                );
                applied.push(insight.suggested_action.clone());
            }
        }
        applied
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_self_improvement_analysis() {
        let engine = SelfImprovementLoop::new(100);
        for i in 0..5 {
            engine
                .record(PerformanceSample {
                    timestamp: chrono::Utc::now(),
                    task_type: "code_review".to_string(),
                    duration_ms: 45_000,
                    success: i >= 3,
                    error_kind: if i < 3 {
                        Some("timeout".to_string())
                    } else {
                        None
                    },
                    tokens_used: 12_000,
                    model: "qwen3-30b".to_string(),
                })
                .await;
        }
        let insights = engine.analyze().await;
        assert!(!insights.is_empty());
        assert!(insights.iter().any(|i| i.category == "recurring_error"));
        assert!(insights.iter().any(|i| i.category == "slow_model"));
    }
}
