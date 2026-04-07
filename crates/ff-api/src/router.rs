//! Smart LLM router with tier escalation, health tracking, and metrics.
//!
//! Two routers are provided:
//! - [`ModelRouter`] — Simple model-based routing with round-robin (backward compat).
//! - [`TierRouter`] — Full tier-escalation router with health tracking, metrics,
//!   configurable timeouts, and retry-aware routing.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use serde::Serialize;
use tracing::warn;

use crate::registry::{BackendEndpoint, BackendRegistry};

// ─── Tier Timeout Configuration ──────────────────────────────────────────────

/// Per-tier timeout configuration.
#[derive(Debug, Clone)]
pub struct TierTimeouts {
    pub tier1: Duration,
    pub tier2: Duration,
    pub tier3: Duration,
    pub tier4: Duration,
}

impl Default for TierTimeouts {
    fn default() -> Self {
        Self {
            tier1: Duration::from_secs(30),
            tier2: Duration::from_secs(60),
            tier3: Duration::from_secs(120),
            tier4: Duration::from_secs(300),
        }
    }
}

impl TierTimeouts {
    /// Get the timeout for a given tier number (1–4).
    pub fn for_tier(&self, tier: u8) -> Duration {
        match tier {
            1 => self.tier1,
            2 => self.tier2,
            3 => self.tier3,
            4 => self.tier4,
            _ => self.tier4,
        }
    }
}

// ─── TierRouter Configuration ────────────────────────────────────────────────

/// Configuration for the [`TierRouter`].
#[derive(Debug, Clone)]
pub struct TierRouterConfig {
    /// Per-tier request timeouts.
    pub timeouts: TierTimeouts,
    /// Consecutive failures before marking a backend unhealthy.
    pub unhealthy_threshold: u32,
    /// Duration before an unhealthy backend becomes eligible for retry.
    pub health_cooldown: Duration,
    /// Default starting tier for escalation (1–4).
    pub start_tier: u8,
    /// Maximum tier to escalate to (1–4).
    pub max_tier: u8,
}

impl Default for TierRouterConfig {
    fn default() -> Self {
        Self {
            timeouts: TierTimeouts::default(),
            unhealthy_threshold: 3,
            health_cooldown: Duration::from_secs(60),
            start_tier: 1,
            max_tier: 4,
        }
    }
}

// ─── Health Tracking ─────────────────────────────────────────────────────────

/// Internal health state for a single backend.
#[derive(Debug)]
struct BackendHealthState {
    consecutive_failures: u32,
    last_failure: Option<Instant>,
    healthy: bool,
}

impl Default for BackendHealthState {
    fn default() -> Self {
        Self {
            consecutive_failures: 0,
            last_failure: None,
            healthy: true,
        }
    }
}

/// Internal per-backend metrics accumulator.
#[derive(Debug, Default)]
struct MetricsAccumulator {
    request_count: u64,
    success_count: u64,
    error_count: u64,
    total_latency_ms: u64,
}

// ─── Public Metric / Health Snapshots ────────────────────────────────────────

/// Snapshot of per-backend metrics (read-only).
#[derive(Debug, Clone, Serialize)]
pub struct BackendMetrics {
    pub backend_id: String,
    pub request_count: u64,
    pub success_count: u64,
    pub error_count: u64,
    pub avg_latency_ms: f64,
    pub error_rate: f64,
}

/// Snapshot of per-backend health (read-only).
#[derive(Debug, Clone, Serialize)]
pub struct BackendHealthInfo {
    pub backend_id: String,
    pub healthy: bool,
    pub consecutive_failures: u32,
    pub seconds_since_last_failure: Option<f64>,
}

// ═══════════════════════════════════════════════════════════════════════════════
// TierRouter — Smart LLM router with tier escalation
// ═══════════════════════════════════════════════════════════════════════════════

/// Smart LLM router with tier escalation, health-aware routing, and metrics.
///
/// # Routing Strategy
///
/// 1. **Specific model requested** → only backends serving that model, grouped by tier.
/// 2. **Tier selector** (e.g. `"tier1"`, `"fast"`) → escalate from that tier upward.
/// 3. **Unknown model / no match** → escalate from `start_tier` to `max_tier`.
///
/// Within each tier, backends are round-robin ordered for load distribution.
/// Unhealthy backends (based on consecutive failure tracking) are excluded,
/// but become eligible again after a configurable cooldown period.
pub struct TierRouter {
    registry: Arc<BackendRegistry>,
    config: TierRouterConfig,
    health: DashMap<String, BackendHealthState>,
    metrics: DashMap<String, MetricsAccumulator>,
    rr_index: DashMap<String, usize>,
}

impl TierRouter {
    /// Create a new TierRouter with the given backend registry and configuration.
    pub fn new(registry: Arc<BackendRegistry>, config: TierRouterConfig) -> Self {
        Self {
            registry,
            config,
            health: DashMap::new(),
            metrics: DashMap::new(),
            rr_index: DashMap::new(),
        }
    }

    /// Create a TierRouter with default configuration.
    pub fn with_defaults(registry: Arc<BackendRegistry>) -> Self {
        Self::new(registry, TierRouterConfig::default())
    }

    // ── Health queries ───────────────────────────────────────────────────────

    /// Check if a backend is currently considered healthy.
    ///
    /// A backend is healthy if:
    /// - It has never failed, OR
    /// - Its consecutive failures are below the threshold, OR
    /// - It was marked unhealthy but the cooldown period has elapsed.
    pub fn is_backend_healthy(&self, backend_id: &str) -> bool {
        let Some(health) = self.health.get(backend_id) else {
            return true; // Unknown backends are assumed healthy
        };

        if health.healthy {
            return true;
        }

        // Cooldown recovery: if enough time has passed, consider it eligible
        if let Some(last_failure) = health.last_failure {
            if last_failure.elapsed() >= self.config.health_cooldown {
                return true;
            }
        }

        false
    }

    /// Record a successful request for a backend. Resets failure count.
    pub fn record_success(&self, backend_id: &str, latency: Duration) {
        // Reset health
        let mut health = self.health.entry(backend_id.to_string()).or_default();
        health.consecutive_failures = 0;
        health.healthy = true;

        // Update metrics
        let mut m = self.metrics.entry(backend_id.to_string()).or_default();
        m.request_count += 1;
        m.success_count += 1;
        m.total_latency_ms += latency.as_millis() as u64;
    }

    /// Record a failed request for a backend. Increments failure count.
    pub fn record_failure(&self, backend_id: &str, latency: Duration) {
        let mut health = self.health.entry(backend_id.to_string()).or_default();
        health.consecutive_failures += 1;
        health.last_failure = Some(Instant::now());

        if health.consecutive_failures >= self.config.unhealthy_threshold && health.healthy {
            warn!(
                backend = %backend_id,
                failures = health.consecutive_failures,
                "marking backend unhealthy after {} consecutive failures",
                health.consecutive_failures,
            );
            health.healthy = false;
        }

        let mut m = self.metrics.entry(backend_id.to_string()).or_default();
        m.request_count += 1;
        m.error_count += 1;
        m.total_latency_ms += latency.as_millis() as u64;
    }

    // ── Metric queries ───────────────────────────────────────────────────────

    /// Get a snapshot of metrics for a specific backend.
    pub fn get_metrics(&self, backend_id: &str) -> Option<BackendMetrics> {
        self.metrics.get(backend_id).map(|m| {
            let error_rate = if m.request_count > 0 {
                m.error_count as f64 / m.request_count as f64
            } else {
                0.0
            };
            let avg_latency = if m.request_count > 0 {
                m.total_latency_ms as f64 / m.request_count as f64
            } else {
                0.0
            };
            BackendMetrics {
                backend_id: backend_id.to_string(),
                request_count: m.request_count,
                success_count: m.success_count,
                error_count: m.error_count,
                avg_latency_ms: avg_latency,
                error_rate,
            }
        })
    }

    /// Get metrics for all tracked backends.
    pub fn all_metrics(&self) -> Vec<BackendMetrics> {
        self.metrics
            .iter()
            .map(|entry| {
                let m = entry.value();
                let error_rate = if m.request_count > 0 {
                    m.error_count as f64 / m.request_count as f64
                } else {
                    0.0
                };
                let avg_latency = if m.request_count > 0 {
                    m.total_latency_ms as f64 / m.request_count as f64
                } else {
                    0.0
                };
                BackendMetrics {
                    backend_id: entry.key().clone(),
                    request_count: m.request_count,
                    success_count: m.success_count,
                    error_count: m.error_count,
                    avg_latency_ms: avg_latency,
                    error_rate,
                }
            })
            .collect()
    }

    /// Get health info for a specific backend.
    pub fn get_health(&self, backend_id: &str) -> Option<BackendHealthInfo> {
        self.health.get(backend_id).map(|h| {
            let effectively_healthy = h.healthy
                || h.last_failure
                    .map_or(false, |t| t.elapsed() >= self.config.health_cooldown);
            BackendHealthInfo {
                backend_id: backend_id.to_string(),
                healthy: effectively_healthy,
                consecutive_failures: h.consecutive_failures,
                seconds_since_last_failure: h.last_failure.map(|t| t.elapsed().as_secs_f64()),
            }
        })
    }

    /// Get the configured timeout for a given tier.
    pub fn timeout_for_tier(&self, tier: u8) -> Duration {
        self.config.timeouts.for_tier(tier)
    }

    // ── Routing ──────────────────────────────────────────────────────────────

    /// Build a tier-escalation route chain.
    ///
    /// Returns a `Vec<(tier, backends)>` ordered by tier for the caller to
    /// iterate through, trying each tier's backends before escalating.
    ///
    /// # Model selection logic
    ///
    /// - If `model` matches specific backend model names → only those backends,
    ///   grouped by tier (no cross-tier escalation to unrelated models).
    /// - If `model` is a tier selector (e.g. `"fast"`, `"tier2"`) → escalate
    ///   from that tier upward through `max_tier`.
    /// - Otherwise → escalate from `start_tier` through `max_tier` across all
    ///   healthy backends.
    pub async fn route_with_escalation(
        &self,
        model: &str,
        start_tier: Option<u8>,
        max_tier: Option<u8>,
    ) -> Vec<(u8, Vec<BackendEndpoint>)> {
        let start = start_tier.unwrap_or(self.config.start_tier);
        let max = max_tier.unwrap_or(self.config.max_tier);

        // ── Specific model match ─────────────────────────────────────────
        let exact_backends = self.registry.healthy_by_model(model).await;
        if !exact_backends.is_empty() {
            let healthy: Vec<_> = exact_backends
                .into_iter()
                .filter(|b| self.is_backend_healthy(&b.id))
                .collect();

            if !healthy.is_empty() {
                let mut tier_groups: BTreeMap<u8, Vec<BackendEndpoint>> = BTreeMap::new();
                for b in healthy {
                    tier_groups.entry(b.tier).or_default().push(b);
                }
                return tier_groups
                    .into_iter()
                    .map(|(tier, mut backends)| {
                        let key = format!("model:{model}:tier:{tier}");
                        let offset = self.next_rr_offset(&key, backends.len());
                        rotate(&mut backends, offset);
                        (tier, backends)
                    })
                    .collect();
            }
        }

        // ── Tier selector or generic escalation ──────────────────────────
        let selector_tier = parse_tier_selector(model);
        let effective_start = selector_tier.unwrap_or(start);

        let mut result = Vec::new();
        for tier in effective_start..=max {
            let tier_backends = self.registry.healthy_by_tier(tier).await;
            let healthy: Vec<_> = tier_backends
                .into_iter()
                .filter(|b| self.is_backend_healthy(&b.id))
                .collect();

            if !healthy.is_empty() {
                let mut backends = healthy;
                let key = format!("tier:{tier}");
                let offset = self.next_rr_offset(&key, backends.len());
                rotate(&mut backends, offset);
                result.push((tier, backends));
            }
        }

        result
    }

    /// Round-robin offset helper.
    fn next_rr_offset(&self, key: &str, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        let mut index = self.rr_index.entry(key.to_string()).or_insert(0);
        let offset = *index % len;
        *index = index.wrapping_add(1);
        offset
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// ModelRouter — Simple model-based routing (backward compat)
// ═══════════════════════════════════════════════════════════════════════════════

/// Simple model-based router with round-robin and tier fallback.
///
/// Kept for backward compatibility with `ff-api`'s own HTTP server.
/// For the gateway's full tier-escalation routing, use [`TierRouter`] instead.
#[derive(Debug)]
pub struct ModelRouter {
    registry: Arc<BackendRegistry>,
    rr_index: DashMap<String, usize>,
}

impl ModelRouter {
    pub fn new(registry: Arc<BackendRegistry>) -> Self {
        Self {
            registry,
            rr_index: DashMap::new(),
        }
    }

    pub async fn route(&self, model: &str) -> Option<BackendEndpoint> {
        self.route_chain(model).await.into_iter().next()
    }

    /// Build a fallback chain for a requested model.
    ///
    /// Priority:
    /// 1) Exact model healthy endpoints (non-busy first, then busy)
    /// 2) Tier-matched healthy endpoints (parsed selector or model's known tier)
    /// 3) Any remaining healthy endpoints
    pub async fn route_chain(&self, model: &str) -> Vec<BackendEndpoint> {
        let mut chain = Vec::new();
        let mut seen = HashSet::new();

        let exact = self.registry.healthy_by_model(model).await;
        self.extend_chain(&mut chain, &mut seen, &format!("model:{model}"), exact);

        let tier = match parse_tier_selector(model) {
            Some(tier) => Some(tier),
            None => self.registry.model_tier(model).await,
        };

        if let Some(tier) = tier {
            let tier_matches = self.registry.healthy_by_tier(tier).await;
            self.extend_chain(&mut chain, &mut seen, &format!("tier:{tier}"), tier_matches);
        }

        let global = self.registry.healthy_endpoints().await;
        self.extend_chain(&mut chain, &mut seen, "global", global);

        chain
    }

    fn extend_chain(
        &self,
        chain: &mut Vec<BackendEndpoint>,
        seen: &mut HashSet<String>,
        key: &str,
        mut candidates: Vec<BackendEndpoint>,
    ) {
        candidates.retain(|b| !seen.contains(&b.id));
        if candidates.is_empty() {
            return;
        }

        // Sort: non-busy first, then busy
        candidates.sort_by_key(|b| b.busy as u8);

        let offset = self.next_offset(key, candidates.len());
        rotate(&mut candidates, offset);

        for b in &candidates {
            seen.insert(b.id.clone());
        }
        chain.extend(candidates);
    }

    fn next_offset(&self, key: &str, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        let mut index = self.rr_index.entry(key.to_string()).or_insert(0);
        let offset = *index % len;
        *index = index.wrapping_add(1);
        offset
    }
}

// ─── Shared Helpers ──────────────────────────────────────────────────────────

/// Parse a human-friendly tier selector string into a tier number.
///
/// Accepts: `"fast"`, `"tier1"`, `"tier-2"`, `"tier:3"`, `"t4"`, `"expert"`, etc.
pub fn parse_tier_selector(model: &str) -> Option<u8> {
    let normalized = model.trim().to_ascii_lowercase();

    match normalized.as_str() {
        "fast" | "small" | "tier1" | "tier-1" | "tier:1" | "t1" => return Some(1),
        "medium" | "tier2" | "tier-2" | "tier:2" | "t2" => return Some(2),
        "large" | "tier3" | "tier-3" | "tier:3" | "t3" => return Some(3),
        "expert" | "tier4" | "tier-4" | "tier:4" | "t4" => return Some(4),
        _ => {}
    }

    if let Some(stripped) = normalized
        .strip_prefix("tier")
        .map(|value| value.trim_start_matches(['-', ':']))
        && let Ok(value) = stripped.parse::<u8>()
    {
        return Some(value);
    }

    None
}

fn rotate<T>(items: &mut [T], offset: usize) {
    if items.is_empty() || offset == 0 {
        return;
    }
    items.rotate_left(offset % items.len());
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn make_backend(id: &str, tier: u8, model: &str) -> BackendEndpoint {
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
        }
    }

    fn make_tier_router(backends: Vec<BackendEndpoint>) -> TierRouter {
        let registry = Arc::new(BackendRegistry::new(backends));
        TierRouter::new(registry, TierRouterConfig::default())
    }

    fn make_tier_router_with_config(
        backends: Vec<BackendEndpoint>,
        config: TierRouterConfig,
    ) -> TierRouter {
        let registry = Arc::new(BackendRegistry::new(backends));
        TierRouter::new(registry, config)
    }

    // ── Tier ordering tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_tier_escalation_order() {
        let router = make_tier_router(vec![
            make_backend("t4-a", 4, "model-200b"),
            make_backend("t2-a", 2, "model-32b"),
            make_backend("t1-a", 1, "model-9b"),
            make_backend("t3-a", 3, "model-72b"),
        ]);

        // Using tier selector "tier1" → should escalate 1→2→3→4
        let chain = router.route_with_escalation("tier1", None, None).await;
        assert_eq!(chain.len(), 4, "should have all 4 tiers");
        assert_eq!(chain[0].0, 1);
        assert_eq!(chain[1].0, 2);
        assert_eq!(chain[2].0, 3);
        assert_eq!(chain[3].0, 4);
    }

    #[tokio::test]
    async fn test_tier_escalation_starts_at_selector() {
        let router = make_tier_router(vec![
            make_backend("t1-a", 1, "model-9b"),
            make_backend("t2-a", 2, "model-32b"),
            make_backend("t3-a", 3, "model-72b"),
            make_backend("t4-a", 4, "model-200b"),
        ]);

        // Selector "tier2" → should start at tier 2
        let chain = router.route_with_escalation("tier2", None, None).await;
        assert_eq!(chain.len(), 3, "tiers 2, 3, 4");
        assert_eq!(chain[0].0, 2);
        assert_eq!(chain[1].0, 3);
        assert_eq!(chain[2].0, 4);
    }

    #[tokio::test]
    async fn test_model_specific_routing_no_escalation() {
        let router = make_tier_router(vec![
            make_backend("t1-a", 1, "model-9b"),
            make_backend("t2-a", 2, "model-32b"),
            make_backend("t2-b", 2, "model-32b"),
            make_backend("t3-a", 3, "model-72b"),
        ]);

        // Requesting "model-32b" → only tier 2 backends for that model
        let chain = router.route_with_escalation("model-32b", None, None).await;
        assert_eq!(chain.len(), 1, "only one tier group for model-32b");
        assert_eq!(chain[0].0, 2);
        assert_eq!(chain[0].1.len(), 2, "two backends for model-32b");
    }

    #[tokio::test]
    async fn test_unknown_model_escalates_from_tier1() {
        let router = make_tier_router(vec![
            make_backend("t1-a", 1, "model-9b"),
            make_backend("t2-a", 2, "model-32b"),
        ]);

        // Unknown model → escalate from tier 1
        let chain = router
            .route_with_escalation("nonexistent-model", None, None)
            .await;
        assert_eq!(chain.len(), 2, "tiers 1 and 2");
        assert_eq!(chain[0].0, 1);
        assert_eq!(chain[1].0, 2);
    }

    #[tokio::test]
    async fn test_max_tier_limits_escalation() {
        let router = make_tier_router(vec![
            make_backend("t1-a", 1, "model-9b"),
            make_backend("t2-a", 2, "model-32b"),
            make_backend("t3-a", 3, "model-72b"),
            make_backend("t4-a", 4, "model-200b"),
        ]);

        // Limit escalation to tier 2
        let chain = router.route_with_escalation("fast", Some(1), Some(2)).await;
        assert_eq!(chain.len(), 2, "only tiers 1 and 2");
        assert_eq!(chain[0].0, 1);
        assert_eq!(chain[1].0, 2);
    }

    // ── Health tracking tests ────────────────────────────────────────────

    #[tokio::test]
    async fn test_health_tracking_marks_unhealthy() {
        let router = make_tier_router(vec![make_backend("t1-a", 1, "model-9b")]);

        // 3 consecutive failures (default threshold) → unhealthy
        for _ in 0..3 {
            router.record_failure("t1-a", Duration::from_millis(100));
        }

        let health = router.get_health("t1-a").unwrap();
        assert!(!health.healthy, "should be unhealthy after 3 failures");
        assert_eq!(health.consecutive_failures, 3);
    }

    #[tokio::test]
    async fn test_health_below_threshold_stays_healthy() {
        let router = make_tier_router(vec![make_backend("t1-a", 1, "model-9b")]);

        // 2 failures (below threshold of 3)
        router.record_failure("t1-a", Duration::from_millis(50));
        router.record_failure("t1-a", Duration::from_millis(50));

        assert!(
            router.is_backend_healthy("t1-a"),
            "should still be healthy with only 2 failures"
        );
    }

    #[tokio::test]
    async fn test_success_resets_failure_count() {
        let router = make_tier_router(vec![make_backend("t1-a", 1, "model-9b")]);

        router.record_failure("t1-a", Duration::from_millis(50));
        router.record_failure("t1-a", Duration::from_millis(50));
        // Success resets
        router.record_success("t1-a", Duration::from_millis(10));

        let health = router.get_health("t1-a").unwrap();
        assert!(health.healthy);
        assert_eq!(health.consecutive_failures, 0);
    }

    #[tokio::test]
    async fn test_health_recovery_after_cooldown() {
        let config = TierRouterConfig {
            unhealthy_threshold: 1,
            health_cooldown: Duration::from_millis(50),
            ..Default::default()
        };
        let router =
            make_tier_router_with_config(vec![make_backend("t1-a", 1, "model-9b")], config);

        // Single failure → unhealthy (threshold = 1)
        router.record_failure("t1-a", Duration::from_millis(10));
        assert!(
            !router.is_backend_healthy("t1-a"),
            "should be unhealthy immediately"
        );

        // Wait for cooldown
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            router.is_backend_healthy("t1-a"),
            "should recover after cooldown"
        );
    }

    #[tokio::test]
    async fn test_unhealthy_backend_excluded_from_routing() {
        let config = TierRouterConfig {
            unhealthy_threshold: 1,
            ..Default::default()
        };
        let router = make_tier_router_with_config(
            vec![
                make_backend("t1-a", 1, "model-a"),
                make_backend("t1-b", 1, "model-b"),
            ],
            config,
        );

        // Mark t1-a unhealthy
        router.record_failure("t1-a", Duration::from_millis(10));

        let chain = router.route_with_escalation("tier1", None, None).await;
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].1.len(), 1, "only one backend should be healthy");
        assert_eq!(chain[0].1[0].id, "t1-b");
    }

    // ── Round-robin tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_round_robin_rotation() {
        let router = make_tier_router(vec![
            make_backend("t1-a", 1, "model-9b"),
            make_backend("t1-b", 1, "model-9b-v2"),
            make_backend("t1-c", 1, "model-9b-v3"),
        ]);

        let chain1 = router.route_with_escalation("tier1", None, None).await;
        let first1 = chain1[0].1[0].id.clone();

        let chain2 = router.route_with_escalation("tier1", None, None).await;
        let first2 = chain2[0].1[0].id.clone();

        let chain3 = router.route_with_escalation("tier1", None, None).await;
        let first3 = chain3[0].1[0].id.clone();

        // All three should be different (round-robin across 3 backends)
        assert_ne!(first1, first2, "round-robin should rotate");
        assert_ne!(first2, first3, "round-robin should keep rotating");
        assert_ne!(first1, first3, "all three should differ");
    }

    #[tokio::test]
    async fn test_round_robin_wraps_around() {
        let router = make_tier_router(vec![
            make_backend("t1-a", 1, "model-a"),
            make_backend("t1-b", 1, "model-b"),
        ]);

        let c1 = router.route_with_escalation("tier1", None, None).await;
        let c2 = router.route_with_escalation("tier1", None, None).await;
        let c3 = router.route_with_escalation("tier1", None, None).await;

        // After 2 calls, should wrap back to the first
        assert_eq!(c1[0].1[0].id, c3[0].1[0].id, "should wrap around");
        assert_ne!(c1[0].1[0].id, c2[0].1[0].id, "adjacent should differ");
    }

    // ── Metrics tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_metrics_tracking() {
        let router = make_tier_router(vec![make_backend("t1-a", 1, "model-9b")]);

        router.record_success("t1-a", Duration::from_millis(100));
        router.record_success("t1-a", Duration::from_millis(200));
        router.record_failure("t1-a", Duration::from_millis(50));

        let metrics = router.get_metrics("t1-a").unwrap();
        assert_eq!(metrics.request_count, 3);
        assert_eq!(metrics.success_count, 2);
        assert_eq!(metrics.error_count, 1);
        assert!(
            (metrics.error_rate - 1.0 / 3.0).abs() < 0.01,
            "error rate should be ~33%"
        );
        // avg latency = (100 + 200 + 50) / 3 ≈ 116.67
        assert!(
            (metrics.avg_latency_ms - 116.67).abs() < 1.0,
            "avg latency should be ~116.67ms"
        );
    }

    #[tokio::test]
    async fn test_all_metrics() {
        let router = make_tier_router(vec![
            make_backend("t1-a", 1, "model-a"),
            make_backend("t1-b", 1, "model-b"),
        ]);

        router.record_success("t1-a", Duration::from_millis(100));
        router.record_success("t1-b", Duration::from_millis(200));

        let all = router.all_metrics();
        assert_eq!(all.len(), 2);
    }

    // ── Timeout configuration tests ──────────────────────────────────────

    #[test]
    fn test_tier_timeouts() {
        let timeouts = TierTimeouts::default();
        assert_eq!(timeouts.for_tier(1), Duration::from_secs(30));
        assert_eq!(timeouts.for_tier(2), Duration::from_secs(60));
        assert_eq!(timeouts.for_tier(3), Duration::from_secs(120));
        assert_eq!(timeouts.for_tier(4), Duration::from_secs(300));
        // Unknown tier falls back to tier 4 timeout
        assert_eq!(timeouts.for_tier(5), Duration::from_secs(300));
    }

    // ── Tier selector parsing tests ──────────────────────────────────────

    #[test]
    fn test_parse_tier_selectors() {
        assert_eq!(parse_tier_selector("fast"), Some(1));
        assert_eq!(parse_tier_selector("small"), Some(1));
        assert_eq!(parse_tier_selector("tier1"), Some(1));
        assert_eq!(parse_tier_selector("t1"), Some(1));

        assert_eq!(parse_tier_selector("medium"), Some(2));
        assert_eq!(parse_tier_selector("tier-2"), Some(2));
        assert_eq!(parse_tier_selector("t2"), Some(2));

        assert_eq!(parse_tier_selector("large"), Some(3));
        assert_eq!(parse_tier_selector("tier:3"), Some(3));

        assert_eq!(parse_tier_selector("expert"), Some(4));
        assert_eq!(parse_tier_selector("tier4"), Some(4));

        assert_eq!(parse_tier_selector("gpt-4"), None);
        assert_eq!(parse_tier_selector("qwen-32b"), None);
    }

    // ── Empty / edge-case tests ──────────────────────────────────────────

    #[tokio::test]
    async fn test_empty_registry() {
        let router = make_tier_router(vec![]);
        let chain = router.route_with_escalation("tier1", None, None).await;
        assert!(chain.is_empty(), "empty registry → empty chain");
    }

    #[tokio::test]
    async fn test_all_backends_unhealthy_returns_empty() {
        let config = TierRouterConfig {
            unhealthy_threshold: 1,
            health_cooldown: Duration::from_secs(3600), // Long cooldown
            ..Default::default()
        };
        let router =
            make_tier_router_with_config(vec![make_backend("t1-a", 1, "model-9b")], config);

        router.record_failure("t1-a", Duration::from_millis(10));

        let chain = router.route_with_escalation("tier1", None, None).await;
        assert!(chain.is_empty(), "all unhealthy → empty chain");
    }
}
