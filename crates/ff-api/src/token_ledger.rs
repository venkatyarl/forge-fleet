//! Token usage ledger and cost tracking for ForgeFleet LLM routing.
//!
//! Provides per-model, per-request token counting with budget enforcement,
//! cost calculation from a built-in pricing database, and alerting hooks.
//!
//! Inspired by best practices from LiteLLM, llm-tokencost, and RouteLLM.

use std::collections::HashMap;
// use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{info, warn};

// ─── Model Pricing ───────────────────────────────────────────────────────────

/// Cost per 1K tokens for a given model.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ModelPricing {
    /// Cost per 1K input/prompt tokens in USD.
    pub input_cost_per_1k: f64,
    /// Cost per 1K output/completion tokens in USD.
    pub output_cost_per_1k: f64,
    /// Whether this is a local (free) model.
    pub is_local: bool,
    /// Model capability tier (1-4).
    pub tier: u8,
}

impl ModelPricing {
    /// Calculate cost for a given token usage.
    pub fn calculate_cost(&self, prompt_tokens: u32, completion_tokens: u32) -> f64 {
        let input_cost = (prompt_tokens as f64 / 1000.0) * self.input_cost_per_1k;
        let output_cost = (completion_tokens as f64 / 1000.0) * self.output_cost_per_1k;
        input_cost + output_cost
    }
}

/// Built-in pricing database for common models.
/// Prices are in USD per 1K tokens.
/// Local models have zero cost.
pub fn default_pricing_db() -> HashMap<String, ModelPricing> {
    let mut db = HashMap::new();

    // OpenAI models (cloud)
    db.insert(
        "gpt-4o".to_string(),
        ModelPricing {
            input_cost_per_1k: 0.00250,
            output_cost_per_1k: 0.01000,
            is_local: false,
            tier: 4,
        },
    );
    db.insert(
        "gpt-4o-mini".to_string(),
        ModelPricing {
            input_cost_per_1k: 0.00015,
            output_cost_per_1k: 0.00060,
            is_local: false,
            tier: 2,
        },
    );
    db.insert(
        "gpt-4-turbo".to_string(),
        ModelPricing {
            input_cost_per_1k: 0.01000,
            output_cost_per_1k: 0.03000,
            is_local: false,
            tier: 4,
        },
    );
    db.insert(
        "gpt-3.5-turbo".to_string(),
        ModelPricing {
            input_cost_per_1k: 0.00050,
            output_cost_per_1k: 0.00150,
            is_local: false,
            tier: 1,
        },
    );

    // Anthropic models (cloud)
    db.insert(
        "claude-3-5-sonnet".to_string(),
        ModelPricing {
            input_cost_per_1k: 0.00300,
            output_cost_per_1k: 0.01500,
            is_local: false,
            tier: 3,
        },
    );
    db.insert(
        "claude-3-opus".to_string(),
        ModelPricing {
            input_cost_per_1k: 0.01500,
            output_cost_per_1k: 0.07500,
            is_local: false,
            tier: 4,
        },
    );
    db.insert(
        "claude-3-haiku".to_string(),
        ModelPricing {
            input_cost_per_1k: 0.00025,
            output_cost_per_1k: 0.00125,
            is_local: false,
            tier: 1,
        },
    );

    // Google models (cloud)
    db.insert(
        "gemini-2.0-flash".to_string(),
        ModelPricing {
            input_cost_per_1k: 0.00010,
            output_cost_per_1k: 0.00040,
            is_local: false,
            tier: 2,
        },
    );
    db.insert(
        "gemini-1.5-pro".to_string(),
        ModelPricing {
            input_cost_per_1k: 0.00125,
            output_cost_per_1k: 0.00500,
            is_local: false,
            tier: 3,
        },
    );

    // Local models (free / self-hosted)
    // Common patterns for local model IDs
    for local_model in [
        "qwen-9b",
        "qwen-32b",
        "qwen-72b",
        "qwen-235b",
        "qwen3-0.6b",
        "qwen3-1.7b",
        "qwen3-4b",
        "qwen3-8b",
        "qwen3-14b",
        "qwen3-32b",
        "llama-3.1-8b",
        "llama-3.1-70b",
        "llama-3.1-405b",
        "llama-3.2-1b",
        "llama-3.2-3b",
        "llama-4-scout",
        "llama-4-maverick",
        "mistral-7b",
        "mixtral-8x7b",
        "mixtral-8x22b",
        "codellama-7b",
        "codellama-13b",
        "codellama-34b",
        "deepseek-coder-6.7b",
        "deepseek-coder-33b",
        "phi-3-mini",
        "phi-3-medium",
        "gemma-2b",
        "gemma-4b",
        "gemma-7b",
        "gemma-27b",
    ] {
        db.insert(
            local_model.to_string(),
            ModelPricing {
                input_cost_per_1k: 0.0,
                output_cost_per_1k: 0.0,
                is_local: true,
                tier: 2,
            },
        );
    }

    db
}

// ─── Token Usage Record ──────────────────────────────────────────────────────

/// A single LLM request with token usage and cost.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsageRecord {
    /// Unique request ID.
    pub request_id: String,
    /// When the request was made.
    pub timestamp: DateTime<Utc>,
    /// Model that served the request.
    pub model: String,
    /// Backend/node that served the request.
    pub backend_id: String,
    /// Task type (from classifier).
    pub task_type: String,
    /// Routing strategy used.
    pub routing_strategy: String,
    /// Number of prompt/input tokens.
    pub prompt_tokens: u32,
    /// Number of completion/output tokens.
    pub completion_tokens: u32,
    /// Total tokens (prompt + completion).
    pub total_tokens: u32,
    /// Cost in USD.
    pub cost_usd: f64,
    /// Whether the model is local (free).
    pub is_local: bool,
    /// Latency in milliseconds.
    pub latency_ms: u64,
    /// Whether the request succeeded.
    pub success: bool,
    /// Optional error message.
    pub error: Option<String>,
}

impl TokenUsageRecord {
    pub fn new(
        request_id: impl Into<String>,
        model: impl Into<String>,
        backend_id: impl Into<String>,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            timestamp: Utc::now(),
            model: model.into(),
            backend_id: backend_id.into(),
            task_type: "unknown".to_string(),
            routing_strategy: "unknown".to_string(),
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            cost_usd: 0.0,
            is_local: false,
            latency_ms: 0,
            success: true,
            error: None,
        }
    }

    pub fn with_tokens(mut self, prompt: u32, completion: u32) -> Self {
        self.prompt_tokens = prompt;
        self.completion_tokens = completion;
        self.total_tokens = prompt + completion;
        self
    }

    pub fn with_cost(mut self, cost: f64, is_local: bool) -> Self {
        self.cost_usd = cost;
        self.is_local = is_local;
        self
    }

    pub fn with_task(mut self, task_type: impl Into<String>, strategy: impl Into<String>) -> Self {
        self.task_type = task_type.into();
        self.routing_strategy = strategy.into();
        self
    }

    pub fn with_latency(mut self, ms: u64) -> Self {
        self.latency_ms = ms;
        self
    }

    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.success = false;
        self.error = Some(error.into());
        self
    }
}

// ─── Budget Configuration ────────────────────────────────────────────────────

/// Budget configuration for cost tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetConfig {
    /// Total daily budget in USD.
    pub daily_budget_usd: f64,
    /// Cloud-only daily budget in USD.
    pub cloud_daily_budget_usd: f64,
    /// Whether to block requests when budget is exceeded.
    pub enforce_budget: bool,
    /// Callback URL or hook when budget threshold is reached (0.8 = 80%).
    pub alert_threshold: f64,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            daily_budget_usd: 100.0,
            cloud_daily_budget_usd: 50.0,
            enforce_budget: false,
            alert_threshold: 0.8,
        }
    }
}

// ─── Cost Tracker ────────────────────────────────────────────────────────────

/// Thread-safe token usage and cost tracker.
///
/// Tracks per-model, per-day usage with budget enforcement.
/// All operations are non-blocking.
pub struct CostTracker {
    pricing_db: DashMap<String, ModelPricing>,
    /// Per-day usage records: "YYYY-MM-DD" -> Vec<records>
    daily_records: DashMap<String, Vec<TokenUsageRecord>>,
    /// Per-model cumulative stats.
    model_stats: DashMap<String, ModelCostStats>,
    budget: RwLock<BudgetConfig>,
    /// Alert state to prevent duplicate alerts.
    alert_fired: DashMap<String, bool>,
}

/// Cumulative statistics for a model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelCostStats {
    pub model: String,
    pub request_count: u64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_tokens: u64,
    pub total_cost_usd: f64,
    pub local_request_count: u64,
    pub cloud_request_count: u64,
    pub cloud_cost_usd: f64,
    pub avg_latency_ms: f64,
}

/// Summary of fleet-wide token usage and costs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FleetCostSummary {
    pub total_requests: u64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_tokens: u64,
    pub total_cost_usd: f64,
    pub local_requests: u64,
    pub cloud_requests: u64,
    pub cloud_cost_usd: f64,
    pub savings_vs_cloud_only_usd: f64,
    pub models: Vec<ModelCostStats>,
    pub daily_cost_usd: f64,
    pub daily_budget_usd: f64,
    pub budget_remaining_usd: f64,
    pub budget_percent_used: f64,
}

impl CostTracker {
    pub fn new() -> Self {
        Self {
            pricing_db: DashMap::from_iter(default_pricing_db()),
            daily_records: DashMap::new(),
            model_stats: DashMap::new(),
            budget: RwLock::new(BudgetConfig::default()),
            alert_fired: DashMap::new(),
        }
    }

    pub fn with_budget(budget: BudgetConfig) -> Self {
        let tracker = Self::new();
        // Use blocking lock here since this is typically called at startup
        if let Ok(mut b) = tracker.budget.try_write() {
            *b = budget;
        }
        tracker
    }

    /// Look up pricing for a model. Falls back to local/free if unknown.
    pub fn get_pricing(&self, model: &str) -> ModelPricing {
        // Try exact match first
        if let Some(p) = self.pricing_db.get(model) {
            return *p;
        }
        // Try prefix match for local models (e.g., "qwen-32b-instruct" -> "qwen-32b")
        for entry in self.pricing_db.iter() {
            if !entry.is_local {
                continue;
            }
            if model.starts_with(entry.key().as_str()) || entry.key().as_str().starts_with(model) {
                return *entry;
            }
        }
        // Default: assume local/free
        ModelPricing {
            input_cost_per_1k: 0.0,
            output_cost_per_1k: 0.0,
            is_local: true,
            tier: 2,
        }
    }

    /// Add or update pricing for a model.
    pub fn set_pricing(&self, model: impl Into<String>, pricing: ModelPricing) {
        self.pricing_db.insert(model.into(), pricing);
    }

    /// Record a token usage event and update stats.
    pub async fn record_usage(&self, record: TokenUsageRecord) {
        let day = record.timestamp.format("%Y-%m-%d").to_string();

        // Store in daily records
        self.daily_records
            .entry(day.clone())
            .or_default()
            .push(record.clone());

        // Update model stats
        let mut stats = self.model_stats.entry(record.model.clone()).or_default();
        stats.model = record.model.clone();
        stats.request_count += 1;
        stats.total_prompt_tokens += record.prompt_tokens as u64;
        stats.total_completion_tokens += record.completion_tokens as u64;
        stats.total_tokens += record.total_tokens as u64;
        stats.total_cost_usd += record.cost_usd;
        if record.is_local {
            stats.local_request_count += 1;
        } else {
            stats.cloud_request_count += 1;
            stats.cloud_cost_usd += record.cost_usd;
        }
        // Rolling average latency
        let n = stats.request_count as f64;
        stats.avg_latency_ms = (stats.avg_latency_ms * (n - 1.0) + record.latency_ms as f64) / n;

        // Check budget
        self.check_budget(&day).await;

        info!(
            request_id = %record.request_id,
            model = %record.model,
            tokens = record.total_tokens,
            cost = %format!("{:.6}", record.cost_usd),
            is_local = record.is_local,
            "token usage recorded"
        );
    }

    /// Calculate cost for a hypothetical request.
    pub fn estimate_cost(&self, model: &str, prompt_tokens: u32, completion_tokens: u32) -> f64 {
        self.get_pricing(model)
            .calculate_cost(prompt_tokens, completion_tokens)
    }

    /// Check if a request would exceed the cloud budget.
    pub async fn would_exceed_budget(
        &self,
        model: &str,
        prompt_tokens: u32,
        completion_tokens: u32,
    ) -> bool {
        let budget = self.budget.read().await;
        if !budget.enforce_budget {
            return false;
        }

        let pricing = self.get_pricing(model);
        if pricing.is_local {
            return false; // Local models don't affect cloud budget
        }

        let day = Utc::now().format("%Y-%m-%d").to_string();
        let daily_cloud_cost = self.daily_cloud_cost(&day).await;
        let estimated_cost = pricing.calculate_cost(prompt_tokens, completion_tokens);

        daily_cloud_cost + estimated_cost > budget.cloud_daily_budget_usd
    }

    /// Get fleet-wide cost summary.
    pub async fn summary(&self) -> FleetCostSummary {
        let day = Utc::now().format("%Y-%m-%d").to_string();
        let daily_cost = self.daily_total_cost(&day).await;
        let daily_cloud_cost = self.daily_cloud_cost(&day).await;
        let budget = self.budget.read().await.clone();

        let mut total_requests = 0u64;
        let mut total_prompt = 0u64;
        let mut total_completion = 0u64;
        let mut total_tokens = 0u64;
        let mut total_cost = 0.0f64;
        let mut local_req = 0u64;
        let mut cloud_req = 0u64;
        let mut cloud_cost = 0.0f64;

        let models: Vec<ModelCostStats> = self
            .model_stats
            .iter()
            .map(|entry| {
                let s = entry.value().clone();
                total_requests += s.request_count;
                total_prompt += s.total_prompt_tokens;
                total_completion += s.total_completion_tokens;
                total_tokens += s.total_tokens;
                total_cost += s.total_cost_usd;
                local_req += s.local_request_count;
                cloud_req += s.cloud_request_count;
                cloud_cost += s.cloud_cost_usd;
                s
            })
            .collect();

        // Estimate savings: if all local requests had used cheapest cloud model
        let cheapest_cloud_cost_per_1k = 0.00010; // gemini flash-ish
        let local_tokens: u64 = self
            .model_stats
            .iter()
            .map(|e| {
                if e.value().local_request_count > 0 {
                    e.value().total_tokens
                } else {
                    0
                }
            })
            .sum();
        let estimated_cloud_cost = (local_tokens as f64 / 1000.0) * cheapest_cloud_cost_per_1k;

        FleetCostSummary {
            total_requests,
            total_prompt_tokens: total_prompt,
            total_completion_tokens: total_completion,
            total_tokens,
            total_cost_usd: total_cost,
            local_requests: local_req,
            cloud_requests: cloud_req,
            cloud_cost_usd: cloud_cost,
            savings_vs_cloud_only_usd: estimated_cloud_cost,
            models,
            daily_cost_usd: daily_cost,
            daily_budget_usd: budget.cloud_daily_budget_usd,
            budget_remaining_usd: (budget.cloud_daily_budget_usd - daily_cloud_cost).max(0.0),
            budget_percent_used: if budget.cloud_daily_budget_usd > 0.0 {
                (daily_cloud_cost / budget.cloud_daily_budget_usd * 100.0).min(100.0)
            } else {
                0.0
            },
        }
    }

    /// Get per-model stats.
    pub fn model_stats(&self) -> Vec<ModelCostStats> {
        self.model_stats.iter().map(|e| e.value().clone()).collect()
    }

    /// Get records for a specific day.
    pub async fn daily_records(&self, day: &str) -> Vec<TokenUsageRecord> {
        self.daily_records
            .get(day)
            .map(|e| e.clone())
            .unwrap_or_default()
    }

    /// Update budget configuration.
    pub async fn set_budget(&self, config: BudgetConfig) {
        let mut budget = self.budget.write().await;
        *budget = config;
    }

    /// Get current budget config.
    pub async fn budget_config(&self) -> BudgetConfig {
        self.budget.read().await.clone()
    }

    /// Flush all accumulated records to a Postgres pool.
    /// Creates the `token_ledger` table if it doesn't exist.
    pub async fn flush_to_db(&self, pool: &sqlx::PgPool) -> anyhow::Result<u64> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS token_ledger (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                request_id TEXT NOT NULL,
                timestamp TIMESTAMPTZ NOT NULL,
                model TEXT NOT NULL,
                backend_id TEXT NOT NULL,
                task_type TEXT NOT NULL,
                routing_strategy TEXT NOT NULL,
                prompt_tokens BIGINT NOT NULL,
                completion_tokens BIGINT NOT NULL,
                total_tokens BIGINT NOT NULL,
                cost_usd DOUBLE PRECISION NOT NULL,
                is_local BOOLEAN NOT NULL,
                latency_ms BIGINT NOT NULL,
                success BOOLEAN NOT NULL,
                error TEXT
            )
            "#,
        )
        .execute(pool)
        .await?;

        let mut total_inserted = 0u64;
        for entry in self.daily_records.iter() {
            for record in entry.value() {
                sqlx::query(
                    r#"
                    INSERT INTO token_ledger
                        (request_id, timestamp, model, backend_id, task_type, routing_strategy,
                         prompt_tokens, completion_tokens, total_tokens, cost_usd, is_local,
                         latency_ms, success, error)
                    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
                    ON CONFLICT (request_id) DO NOTHING
                    "#,
                )
                .bind(&record.request_id)
                .bind(record.timestamp)
                .bind(&record.model)
                .bind(&record.backend_id)
                .bind(&record.task_type)
                .bind(&record.routing_strategy)
                .bind(record.prompt_tokens as i64)
                .bind(record.completion_tokens as i64)
                .bind(record.total_tokens as i64)
                .bind(record.cost_usd)
                .bind(record.is_local)
                .bind(record.latency_ms as i64)
                .bind(record.success)
                .bind(&record.error)
                .execute(pool)
                .await?;
                total_inserted += 1;
            }
        }

        Ok(total_inserted)
    }

    // ─── Internal Helpers ───────────────────────────────────────────────────

    async fn daily_total_cost(&self, day: &str) -> f64 {
        self.daily_records(day)
            .await
            .iter()
            .map(|r| r.cost_usd)
            .sum()
    }

    async fn daily_cloud_cost(&self, day: &str) -> f64 {
        self.daily_records(day)
            .await
            .iter()
            .filter(|r| !r.is_local)
            .map(|r| r.cost_usd)
            .sum()
    }

    async fn check_budget(&self, day: &str) {
        let budget = self.budget.read().await.clone();
        if !budget.enforce_budget {
            return;
        }

        let daily_cloud = self.daily_cloud_cost(day).await;
        let pct = daily_cloud / budget.cloud_daily_budget_usd;

        if pct >= budget.alert_threshold
            && !self
                .alert_fired
                .get("threshold")
                .map(|v| *v)
                .unwrap_or(false)
        {
            warn!(
                daily_cloud_cost = %format!("{:.4}", daily_cloud),
                budget = %format!("{:.4}", budget.cloud_daily_budget_usd),
                percent = %format!("{:.1}", pct * 100.0),
                "CLOUD BUDGET ALERT: approaching daily cloud budget limit"
            );
            self.alert_fired.insert("threshold".to_string(), true);
        }

        if daily_cloud >= budget.cloud_daily_budget_usd
            && !self
                .alert_fired
                .get("exceeded")
                .map(|v| *v)
                .unwrap_or(false)
        {
            warn!(
                daily_cloud_cost = %format!("{:.4}", daily_cloud),
                "CLOUD BUDGET EXCEEDED: daily cloud budget has been exceeded"
            );
            self.alert_fired.insert("exceeded".to_string(), true);
        }
    }
}

impl Default for CostTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// Tests
// ═════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pricing_db_has_common_models() {
        let db = default_pricing_db();
        assert!(db.contains_key("gpt-4o"));
        assert!(db.contains_key("qwen-32b"));
        assert!(db.get("qwen-32b").unwrap().is_local);
    }

    #[test]
    fn test_cost_calculation() {
        let pricing = ModelPricing {
            input_cost_per_1k: 0.00250,
            output_cost_per_1k: 0.01000,
            is_local: false,
            tier: 4,
        };
        let cost = pricing.calculate_cost(1000, 500);
        assert!(
            (cost - 0.00750).abs() < 0.00001,
            "cost should be ~0.00750, got {cost}"
        );
    }

    #[test]
    fn test_local_model_zero_cost() {
        let db = default_pricing_db();
        let pricing = db.get("qwen-9b").unwrap();
        assert_eq!(pricing.calculate_cost(10000, 5000), 0.0);
    }

    #[tokio::test]
    async fn test_record_usage_updates_stats() {
        let tracker = CostTracker::new();
        let record = TokenUsageRecord::new("req-1", "gpt-4o", "backend-1")
            .with_tokens(1000, 500)
            .with_cost(0.00750, false);

        tracker.record_usage(record).await;

        let stats = tracker.model_stats();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].request_count, 1);
        assert_eq!(stats[0].total_tokens, 1500);
        assert!((stats[0].total_cost_usd - 0.00750).abs() < 0.00001);
    }

    #[tokio::test]
    async fn test_budget_enforcement() {
        let budget = BudgetConfig {
            daily_budget_usd: 100.0,
            cloud_daily_budget_usd: 0.01, // Very low for testing
            enforce_budget: true,
            alert_threshold: 0.5,
        };
        let tracker = CostTracker::with_budget(budget);

        // First request should be fine
        let record1 = TokenUsageRecord::new("req-1", "gpt-4o", "backend-1")
            .with_tokens(1000, 500)
            .with_cost(0.00750, false);
        tracker.record_usage(record1).await;

        // This should exceed budget
        let would_exceed = tracker.would_exceed_budget("gpt-4o", 1000, 500).await;
        assert!(would_exceed, "should exceed very low budget");
    }

    #[tokio::test]
    async fn test_fleet_summary() {
        let tracker = CostTracker::new();

        tracker
            .record_usage(
                TokenUsageRecord::new("req-1", "qwen-32b", "local-1")
                    .with_tokens(2000, 1000)
                    .with_cost(0.0, true),
            )
            .await;
        tracker
            .record_usage(
                TokenUsageRecord::new("req-2", "gpt-4o", "cloud-1")
                    .with_tokens(1000, 500)
                    .with_cost(0.00750, false),
            )
            .await;

        let summary = tracker.summary().await;
        assert_eq!(summary.total_requests, 2);
        assert_eq!(summary.local_requests, 1);
        assert_eq!(summary.cloud_requests, 1);
        assert!((summary.total_cost_usd - 0.00750).abs() < 0.00001);
    }
}
