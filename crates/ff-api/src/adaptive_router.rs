//! Adaptive LLM router that combines prompt classification with model quality
//! tracking to make intelligent routing decisions.
//!
//! Uses the [`classifier`] to understand what the prompt needs, the
//! [`quality_tracker`] to find the best model for that task type, and falls
//! back to [`TierRouter`] when adaptive data is insufficient.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use tracing::{debug, info};

use crate::classifier::{self, TaskProfile};
use crate::quality_tracker::QualityTracker;
use crate::registry::{BackendEndpoint, BackendRegistry};
use crate::router::TierRouter;
use crate::types::ChatMessage;

// ─── Configuration ───────────────────────────────────────────────────────────

/// Routing policy for local vs cloud model selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingPolicy {
    /// Prefer local models, only escalate to cloud for complex tasks or when local fails.
    LocalFirst,
    /// Optimize for lowest cost (prefer local, then cheapest cloud).
    CostOptimized,
    /// Optimize for highest quality (use adaptive quality tracking).
    QualityFirst,
    /// Balanced: consider quality, cost, and locality together.
    Balanced,
}

impl Default for RoutingPolicy {
    fn default() -> Self {
        RoutingPolicy::LocalFirst
    }
}

/// Configuration for the adaptive router.
#[derive(Debug, Clone)]
pub struct AdaptiveRouterConfig {
    /// Minimum samples per (model, task_type) before we trust adaptive routing.
    pub confidence_threshold: u32,

    /// Quality score difference threshold — if two models' scores differ by
    /// less than this, prefer the cheaper/faster one.
    pub quality_epsilon: f64,

    /// Maximum acceptable latency ratio. If a cheaper model's avg latency is
    /// more than this factor above the fastest, skip it even if quality is okay.
    pub max_latency_ratio: f64,

    /// Whether to allow adaptive routing at all (feature flag).
    pub enabled: bool,

    /// Whether to auto-load models on demand when the router selects a model
    /// that isn't currently in the registry. When true, callers can invoke
    /// [`AdaptiveRouter::autoload_if_missing`] to spawn the inference server
    /// for a catalog model before routing falls over.
    pub autoload_enabled: bool,

    /// Routing policy for local vs cloud selection.
    pub policy: RoutingPolicy,

    /// Complexity threshold above which cloud models are allowed even with LocalFirst policy.
    /// Complex tasks (tier >= this) may use cloud if no local model can handle them.
    pub cloud_complexity_threshold: u8,
}

impl Default for AdaptiveRouterConfig {
    fn default() -> Self {
        Self {
            confidence_threshold: 10,
            quality_epsilon: 0.05,
            max_latency_ratio: 3.0,
            enabled: true,
            autoload_enabled: true,
            policy: RoutingPolicy::LocalFirst,
            cloud_complexity_threshold: 3,
        }
    }
}

// ─── Routing Decision ────────────────────────────────────────────────────────

/// Explains why the router picked a particular backend chain.
#[derive(Debug, Clone, Serialize)]
pub struct RoutingDecision {
    /// Which routing strategy was used.
    pub strategy: RoutingStrategy,
    /// The prompt classification result.
    pub profile: TaskProfile,
    /// The recommended model (if adaptive routing succeeded).
    pub recommended_model: Option<String>,
    /// The tier escalation chain that will be tried.
    pub escalation_tiers: Vec<u8>,
    /// Reason for the decision (human-readable).
    pub reason: String,
}

/// Which routing strategy was selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingStrategy {
    /// Adaptive routing based on quality data.
    Adaptive,
    /// Fell back to tier-based routing (not enough quality data).
    TierFallback,
    /// User explicitly requested a specific model/tier.
    Explicit,
    /// Adaptive routing is disabled.
    Disabled,
}

// ─── Adaptive Router ─────────────────────────────────────────────────────────

/// Smart router that combines prompt classification, quality tracking, and
/// tier-based fallback.
pub struct AdaptiveRouter {
    config: AdaptiveRouterConfig,
    registry: Arc<BackendRegistry>,
    tier_router: Arc<TierRouter>,
    quality_tracker: Arc<QualityTracker>,
}

impl AdaptiveRouter {
    /// Create a new adaptive router.
    pub fn new(
        config: AdaptiveRouterConfig,
        registry: Arc<BackendRegistry>,
        tier_router: Arc<TierRouter>,
        quality_tracker: Arc<QualityTracker>,
    ) -> Self {
        Self {
            config,
            registry,
            tier_router,
            quality_tracker,
        }
    }

    /// Create with default configuration.
    pub fn with_defaults(
        registry: Arc<BackendRegistry>,
        tier_router: Arc<TierRouter>,
        quality_tracker: Arc<QualityTracker>,
    ) -> Self {
        Self::new(
            AdaptiveRouterConfig::default(),
            registry,
            tier_router,
            quality_tracker,
        )
    }

    /// Get a reference to the quality tracker.
    pub fn quality_tracker(&self) -> &Arc<QualityTracker> {
        &self.quality_tracker
    }

    /// Get a reference to the tier router.
    pub fn tier_router(&self) -> &Arc<TierRouter> {
        &self.tier_router
    }

    /// If no healthy backend exists for `catalog_id`, attempt to auto-load it
    /// on the local node via [`crate::autoload::ensure_deployed`] and register
    /// the resulting endpoint in the shared registry.
    ///
    /// Returns `Ok(Some(url))` if a model was newly loaded, `Ok(None)` if the
    /// model was already present (or autoload is disabled), or `Err(_)` if the
    /// autoload attempt failed.
    pub async fn autoload_if_missing(
        &self,
        pool: &sqlx::PgPool,
        catalog_id: &str,
    ) -> Result<Option<String>, String> {
        if !self.config.autoload_enabled {
            return Ok(None);
        }
        if !self.registry.healthy_by_model(catalog_id).await.is_empty() {
            return Ok(None);
        }
        let url = crate::autoload::ensure_deployed(pool, catalog_id).await?;
        // Register as an ad-hoc tier-2 backend so future requests route to it.
        let (host, port) = parse_host_port(&url).ok_or_else(|| format!("bad url: {url}"))?;
        let endpoint = BackendEndpoint {
            id: format!("autoload-{}-{}", catalog_id, port),
            node: "local".to_string(),
            host,
            port,
            model: catalog_id.to_string(),
            tier: 2,
            healthy: true,
            busy: false,
            scheme: "http".to_string(),
            is_local: true,
            cost_per_1k_input: 0.0,
            cost_per_1k_output: 0.0,
        };
        self.registry.add_endpoint(endpoint).await;
        Ok(Some(url))
    }

    // ── Main Routing Entry Point ─────────────────────────────────────────

    /// Route a chat completion request adaptively.
    ///
    /// Returns a `(decision, escalation_chain)` pair. The escalation chain
    /// is the ordered list of `(tier, backends)` to try.
    pub async fn route(
        &self,
        model: &str,
        messages: &[ChatMessage],
        start_tier: Option<u8>,
        max_tier: Option<u8>,
    ) -> (RoutingDecision, Vec<(u8, Vec<BackendEndpoint>)>) {
        let profile = classifier::classify(messages);

        // If the user explicitly requested a model or tier, respect that
        if is_explicit_request(model) {
            let chain = self
                .tier_router
                .route_with_escalation(model, start_tier, max_tier)
                .await;

            let decision = RoutingDecision {
                strategy: RoutingStrategy::Explicit,
                profile,
                recommended_model: Some(model.to_string()),
                escalation_tiers: chain.iter().map(|(t, _)| *t).collect(),
                reason: format!("user explicitly requested model/tier '{model}'"),
            };

            return (decision, chain);
        }

        // If adaptive routing is disabled, fall back to tier router
        if !self.config.enabled {
            return self.tier_fallback(profile, start_tier, max_tier).await;
        }

        // Try adaptive routing
        self.route_adaptive(profile, start_tier, max_tier).await
    }

    // ── Adaptive Strategy ────────────────────────────────────────────────

    async fn route_adaptive(
        &self,
        profile: TaskProfile,
        start_tier: Option<u8>,
        max_tier: Option<u8>,
    ) -> (RoutingDecision, Vec<(u8, Vec<BackendEndpoint>)>) {
        let task_type = profile.task_type;

        // Check if we have enough quality data
        if !self
            .quality_tracker
            .has_confident_data(task_type, self.config.confidence_threshold)
        {
            debug!(
                task_type = task_type.as_str(),
                threshold = self.config.confidence_threshold,
                "insufficient quality data, falling back to tier router"
            );
            return self.tier_fallback(profile, start_tier, max_tier).await;
        }

        // Get model → tier mapping from registry
        let model_tiers = self.get_model_tiers().await;

        // Rank models by quality for this task type
        let rankings = self.quality_tracker.rank_models(task_type, &model_tiers);

        if rankings.is_empty() {
            return self.tier_fallback(profile, start_tier, max_tier).await;
        }

        // Find the best model considering quality, cost (tier), and latency
        let best = self.select_best_model(&rankings, &profile);

        match best {
            Some((model_id, reason)) => {
                info!(
                    model = %model_id,
                    task_type = task_type.as_str(),
                    complexity = profile.complexity.as_str(),
                    reason = %reason,
                    "adaptive routing selected model"
                );

                // Build escalation chain starting from the selected model's tier
                let model_tier = model_tiers.get(&model_id).copied().unwrap_or(2);
                let effective_max = max_tier.unwrap_or(4);

                // Put the selected model's tier first, then escalate upward
                let chain = self
                    .tier_router
                    .route_with_escalation(
                        &format!("tier{model_tier}"),
                        Some(model_tier),
                        Some(effective_max),
                    )
                    .await;

                let decision = RoutingDecision {
                    strategy: RoutingStrategy::Adaptive,
                    profile,
                    recommended_model: Some(model_id),
                    escalation_tiers: chain.iter().map(|(t, _)| *t).collect(),
                    reason,
                };

                (decision, chain)
            }
            None => self.tier_fallback(profile, start_tier, max_tier).await,
        }
    }

    /// Select the best model from rankings considering quality, cost, locality, and latency.
    fn select_best_model(
        &self,
        rankings: &[crate::quality_tracker::ModelRanking],
        profile: &TaskProfile,
    ) -> Option<(String, String)> {
        // Filter to confident models only
        let confident: Vec<_> = rankings.iter().filter(|r| r.confident).collect();

        if confident.is_empty() {
            return None;
        }

        let best_quality = confident[0]; // Already sorted by quality descending
        let policy = self.config.policy;
        let is_complex = profile.complexity == crate::classifier::Complexity::Complex
            || profile.recommended_tier >= self.config.cloud_complexity_threshold;

        // Apply policy-based filtering
        let filtered: Vec<_> = match policy {
            RoutingPolicy::LocalFirst => {
                // Try local models first for non-complex tasks
                if !is_complex {
                    let local_models: Vec<_> = confident.iter().filter(|r| self.is_local_model(&r.model_id)).copied().collect();
                    if !local_models.is_empty() {
                        local_models
                    } else {
                        confident.clone()
                    }
                } else {
                    confident.clone()
                }
            }
            RoutingPolicy::CostOptimized => {
                // Prefer local (free), then cheapest tier
                let mut sorted = confident.clone();
                sorted.sort_by(|a, b| {
                    let a_local = self.is_local_model(&a.model_id);
                    let b_local = self.is_local_model(&b.model_id);
                    match (a_local, b_local) {
                        (true, false) => std::cmp::Ordering::Less,
                        (false, true) => std::cmp::Ordering::Greater,
                        _ => a.tier.cmp(&b.tier).then(a.avg_latency_ms.partial_cmp(&b.avg_latency_ms).unwrap_or(std::cmp::Ordering::Equal)),
                    }
                });
                sorted
            }
            RoutingPolicy::QualityFirst => {
                // Use quality rankings as-is
                confident.clone()
            }
            RoutingPolicy::Balanced => {
                // Score each model: quality * 0.4 + locality_bonus * 0.3 + cost_bonus * 0.3
                let mut scored: Vec<_> = confident.iter().map(|r| {
                    let locality_bonus = if self.is_local_model(&r.model_id) { 0.3 } else { 0.0 };
                    let cost_bonus = if r.tier <= 2 { 0.2 } else { 0.0 };
                    let score = r.score * 0.5 + locality_bonus + cost_bonus;
                    (*r, score)
                }).collect();
                scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                scored.into_iter().map(|(r, _)| r).collect()
            }
        };

        if filtered.is_empty() {
            return None;
        }

        // Find the best model within filtered set
        let mut candidate = filtered[0];
        let mut reason = match policy {
            RoutingPolicy::LocalFirst => {
                if self.is_local_model(&candidate.model_id) {
                    format!("local-first: best local model ({:.2}) for {}", candidate.score, profile.task_type)
                } else {
                    format!("local-first fallback: no local model available, using cloud ({:.2}) for {}", candidate.score, profile.task_type)
                }
            }
            RoutingPolicy::CostOptimized => {
                if self.is_local_model(&candidate.model_id) {
                    format!("cost-optimized: free local model ({:.2}) for {}", candidate.score, profile.task_type)
                } else {
                    format!("cost-optimized: cheapest cloud model ({:.2}, tier {}) for {}", candidate.score, candidate.tier, profile.task_type)
                }
            }
            RoutingPolicy::QualityFirst => {
                format!("quality-first: best quality ({:.2}) for {}", candidate.score, profile.task_type)
            }
            RoutingPolicy::Balanced => {
                format!("balanced: best overall score ({:.2}) for {}", candidate.score, profile.task_type)
            }
        };

        // Within the filtered set, apply epsilon-based optimization
        for r in &filtered {
            // Skip models with much worse quality than best in filtered set
            if filtered[0].score - r.score > self.config.quality_epsilon {
                continue;
            }

            // Prefer local models when quality is similar
            let r_local = self.is_local_model(&r.model_id);
            let cand_local = self.is_local_model(&candidate.model_id);
            if r_local && !cand_local {
                candidate = r;
                reason = format!("similar quality ({:.2}) but local (free) vs cloud", r.score);
                continue;
            }

            // Prefer cheaper tier when quality is similar
            if r.tier < candidate.tier {
                candidate = r;
                reason = format!(
                    "similar quality ({:.2} vs {:.2}) but cheaper tier {} vs {}",
                    r.score, filtered[0].score, r.tier, candidate.tier
                );
            }

            // If same tier, prefer lower latency
            if r.tier == candidate.tier && r.avg_latency_ms < candidate.avg_latency_ms {
                if filtered[0].avg_latency_ms > 0.0
                    && r.avg_latency_ms / filtered[0].avg_latency_ms
                        < self.config.max_latency_ratio
                {
                    candidate = r;
                    reason = format!(
                        "similar quality ({:.2}) with lower latency ({:.0}ms vs {:.0}ms)",
                        r.score, r.avg_latency_ms, candidate.avg_latency_ms
                    );
                }
            }
        }

        Some((candidate.model_id.clone(), reason))
    }

    /// Check if a model_id corresponds to a local (free) model.
    /// Uses a simple heuristic based on model naming conventions.
    fn is_local_model(&self, model_id: &str) -> bool {
        let local_prefixes = [
            "qwen", "llama", "mistral", "mixtral", "codellama",
            "deepseek-coder", "phi-", "gemma", "yi-",
        ];
        let lower = model_id.to_lowercase();
        local_prefixes.iter().any(|prefix| lower.starts_with(prefix))
    }

    // ── Tier Fallback ────────────────────────────────────────────────────

    async fn tier_fallback(
        &self,
        profile: TaskProfile,
        start_tier: Option<u8>,
        max_tier: Option<u8>,
    ) -> (RoutingDecision, Vec<(u8, Vec<BackendEndpoint>)>) {
        let recommended = profile.recommended_tier;
        let effective_start = start_tier.unwrap_or(recommended);
        let effective_max = max_tier.unwrap_or(4);

        let selector = format!("tier{effective_start}");
        let chain = self
            .tier_router
            .route_with_escalation(&selector, Some(effective_start), Some(effective_max))
            .await;

        let reason = format!(
            "tier fallback: {} {} → start tier {}",
            profile.task_type, profile.complexity, effective_start
        );

        let decision = RoutingDecision {
            strategy: RoutingStrategy::TierFallback,
            profile,
            recommended_model: None,
            escalation_tiers: chain.iter().map(|(t, _)| *t).collect(),
            reason,
        };

        (decision, chain)
    }

    // ── Helpers ──────────────────────────────────────────────────────────

    /// Build a model_id → tier mapping from the registry.
    async fn get_model_tiers(&self) -> HashMap<String, u8> {
        self.registry.available_models().await.into_iter().collect()
    }
}

/// Parse a `http://host:port` URL into `(host, port)`. Returns `None` if the
/// URL doesn't have the expected shape.
fn parse_host_port(url: &str) -> Option<(String, u16)> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let rest = rest.split('/').next().unwrap_or(rest);
    let (host, port_str) = rest.rsplit_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    Some((host.to_string(), port))
}

/// Check if a model string is an explicit model/tier request (not "auto" or empty).
fn is_explicit_request(model: &str) -> bool {
    let m = model.trim().to_lowercase();
    // If it's empty, "auto", or "adaptive" → not explicit
    if m.is_empty() || m == "auto" || m == "adaptive" {
        return false;
    }
    // If it's a generic tier selector, also not explicit (let classifier decide)
    if matches!(m.as_str(), "fast" | "small" | "medium" | "large" | "expert") {
        return false;
    }
    // Anything else is an explicit request
    true
}

// ═════════════════════════════════════════════════════════════════════════════
// Tests
// ═════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classifier::TaskType;
    use crate::quality_tracker::{Outcome, QualityTrackerConfig};
    use crate::registry::BackendEndpoint;
    use crate::router::TierRouter;

    fn make_backend(id: &str, tier: u8, model: &str) -> BackendEndpoint {
        let is_local = !model.starts_with("gpt") && !model.starts_with("claude") && !model.starts_with("gemini");
        BackendEndpoint {
            id: id.to_string(),
            node: "test-node".to_string(),
            host: "127.0.0.1".to_string(),
            port: 8000 + tier as u16,
            model: model.to_string(),
            tier,
            healthy: true,
            busy: false,
            scheme: "http".to_string(),
            is_local,
            cost_per_1k_input: if is_local { 0.0 } else { 0.001 },
            cost_per_1k_output: if is_local { 0.0 } else { 0.003 },
        }
    }

    fn make_test_router(backends: Vec<BackendEndpoint>) -> (AdaptiveRouter, Arc<QualityTracker>) {
        let registry = Arc::new(BackendRegistry::new(backends));
        let tier_router = Arc::new(TierRouter::with_defaults(registry.clone()));
        let tracker = Arc::new(QualityTracker::new(QualityTrackerConfig {
            min_samples: 3,
            ..Default::default()
        }));
        let config = AdaptiveRouterConfig {
            confidence_threshold: 3,
            ..Default::default()
        };
        let router = AdaptiveRouter::new(config, registry, tier_router, tracker.clone());
        (router, tracker)
    }

    fn user_msg(text: &str) -> ChatMessage {
        ChatMessage {
            role: "user".to_string(),
            content: serde_json::Value::String(text.to_string()),
            name: None,
            extra: Default::default(),
        }
    }

    // ── Explicit requests bypass adaptive routing ────────────────────────

    #[tokio::test]
    async fn test_explicit_model_bypasses_adaptive() {
        let (router, _tracker) = make_test_router(vec![
            make_backend("t1-a", 1, "qwen-9b"),
            make_backend("t2-a", 2, "qwen-32b"),
        ]);

        let messages = [user_msg("Write some code")];
        let (decision, _chain) = router.route("qwen-32b", &messages, None, None).await;

        assert_eq!(decision.strategy, RoutingStrategy::Explicit);
        assert_eq!(decision.recommended_model.as_deref(), Some("qwen-32b"));
    }

    // ── Falls back to tier router with no quality data ───────────────────

    #[tokio::test]
    async fn test_falls_back_without_quality_data() {
        let (router, _tracker) = make_test_router(vec![
            make_backend("t1-a", 1, "qwen-9b"),
            make_backend("t2-a", 2, "qwen-32b"),
        ]);

        let messages = [user_msg("Hello!")];
        let (decision, chain) = router.route("auto", &messages, None, None).await;

        assert_eq!(decision.strategy, RoutingStrategy::TierFallback);
        assert!(!chain.is_empty());
    }

    // ── Uses adaptive routing when quality data exists ───────────────────

    #[tokio::test]
    async fn test_adaptive_routing_with_quality_data() {
        let (router, tracker) = make_test_router(vec![
            make_backend("t1-a", 1, "qwen-9b"),
            make_backend("t2-a", 2, "qwen-32b"),
            make_backend("t3-a", 3, "qwen-72b"),
        ]);

        // Record enough quality data for code tasks
        for _ in 0..5 {
            tracker.record("qwen-32b", TaskType::Code, &Outcome::success(200.0));
        }
        // Lower quality for tier 1
        for _ in 0..5 {
            tracker.record("qwen-9b", TaskType::Code, &Outcome::partial(0.4, 100.0));
        }

        let messages = [user_msg("Write a Rust function to sort integers")];
        let (decision, chain) = router.route("auto", &messages, None, None).await;

        assert_eq!(decision.strategy, RoutingStrategy::Adaptive);
        assert!(!chain.is_empty());
    }

    // ── Prefers cheaper model when quality is similar ────────────────────

    #[tokio::test]
    async fn test_prefers_cheaper_model_similar_quality() {
        let (router, tracker) = make_test_router(vec![
            make_backend("t1-a", 1, "qwen-9b"),
            make_backend("t2-a", 2, "qwen-32b"),
            make_backend("t3-a", 3, "qwen-72b"),
        ]);

        // All models have similar quality for chat
        for _ in 0..5 {
            tracker.record("qwen-9b", TaskType::Chat, &Outcome::success(50.0));
            tracker.record("qwen-32b", TaskType::Chat, &Outcome::success(100.0));
            tracker.record("qwen-72b", TaskType::Chat, &Outcome::success(200.0));
        }

        let messages = [user_msg("Hello, how are you?")];
        let (decision, _chain) = router.route("auto", &messages, None, None).await;

        assert_eq!(decision.strategy, RoutingStrategy::Adaptive);
        // Should prefer the cheapest (tier 1) since quality is similar
        if let Some(ref model) = decision.recommended_model {
            assert_eq!(
                model, "qwen-9b",
                "should prefer cheapest model with similar quality"
            );
        }
    }

    // ── Disabled routing ─────────────────────────────────────────────────

    #[tokio::test]
    async fn test_disabled_adaptive_routing() {
        let registry = Arc::new(BackendRegistry::new(vec![make_backend(
            "t1-a", 1, "qwen-9b",
        )]));
        let tier_router = Arc::new(TierRouter::with_defaults(registry.clone()));
        let tracker = Arc::new(QualityTracker::with_defaults());
        let config = AdaptiveRouterConfig {
            enabled: false,
            ..Default::default()
        };
        let router = AdaptiveRouter::new(config, registry, tier_router, tracker);

        let messages = [user_msg("Write code")];
        let (decision, _chain) = router.route("auto", &messages, None, None).await;

        assert_eq!(decision.strategy, RoutingStrategy::TierFallback);
    }

    // ── is_explicit_request tests ────────────────────────────────────────

    #[test]
    fn test_is_explicit_request() {
        assert!(!is_explicit_request(""));
        assert!(!is_explicit_request("auto"));
        assert!(!is_explicit_request("adaptive"));
        assert!(!is_explicit_request("fast"));
        assert!(!is_explicit_request("medium"));
        assert!(!is_explicit_request("large"));
        assert!(!is_explicit_request("expert"));

        assert!(is_explicit_request("qwen-32b"));
        assert!(is_explicit_request("gpt-4"));
        assert!(is_explicit_request("tier1"));
        assert!(is_explicit_request("model-9b"));
    }

    // ── Routing decision contains classification info ────────────────────

    #[tokio::test]
    async fn test_decision_contains_profile() {
        let (router, _tracker) = make_test_router(vec![make_backend("t1-a", 1, "qwen-9b")]);

        let messages = [user_msg("Summarize this article about AI")];
        let (decision, _chain) = router.route("auto", &messages, None, None).await;

        assert_eq!(decision.profile.task_type, TaskType::Summary);
        assert!(!decision.reason.is_empty());
    }

    // ── Tier selector keywords are not explicit ──────────────────────────

    #[tokio::test]
    async fn test_tier_selectors_use_classification() {
        let (router, _tracker) = make_test_router(vec![
            make_backend("t1-a", 1, "qwen-9b"),
            make_backend("t2-a", 2, "qwen-32b"),
        ]);

        // "fast", "medium", etc. should NOT be treated as explicit
        let messages = [user_msg("Debug this error: stack trace follows")];
        let (decision, _chain) = router.route("fast", &messages, None, None).await;

        // Should be tier fallback (no quality data), not explicit
        assert_eq!(decision.strategy, RoutingStrategy::TierFallback);
        assert_eq!(decision.profile.task_type, TaskType::Debug);
    }
}
