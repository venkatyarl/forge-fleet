//! Pure, in-process routing policy over the shared capacity registry.
//!
//! This crate owns ordering and escalation only. Capacity discovery remains in
//! `ff-capacity`; live load and execution remain responsibilities of callers.

use chrono::{DateTime, Utc};
use ff_capacity::{BackendCapacity, CapacitySnapshot, InferenceDeployment};

/// A routing tier, ordered from cheapest to most expensive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RoutingTier {
    Local30B,
    Local480B,
    Cloud,
}

/// Declarative allowance for one tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierBudget {
    pub tier: RoutingTier,
    pub cost_units: u32,
    pub min_context_tokens: u32,
    pub capability_tags: Vec<String>,
}

/// Policy configuration. Values mirror the pre-extraction router behavior.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyConfig {
    pub budgets: Vec<TierBudget>,
    pub backend_headroom_floor_pct: f64,
    pub headroom_weight: f64,
    pub preference_weight: f64,
    pub health_weight: f64,
    /// The cloud build backstop is promoted after score ordering for parity.
    pub preferred_cloud_backstop: Option<String>,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            budgets: vec![
                TierBudget {
                    tier: RoutingTier::Local30B,
                    cost_units: 0,
                    min_context_tokens: 32_768,
                    capability_tags: vec!["code".into(), "tool_calling".into()],
                },
                TierBudget {
                    tier: RoutingTier::Local480B,
                    cost_units: 0,
                    min_context_tokens: 32_768,
                    capability_tags: vec!["code".into(), "tool_calling".into()],
                },
                TierBudget {
                    tier: RoutingTier::Cloud,
                    cost_units: 1,
                    min_context_tokens: 32_768,
                    capability_tags: vec!["code".into(), "tool_calling".into(), "cloud".into()],
                },
            ],
            backend_headroom_floor_pct: 15.0,
            headroom_weight: 0.60,
            preference_weight: 0.30,
            health_weight: 0.10,
            preferred_cloud_backstop: None,
        }
    }
}

/// Requirements supplied by the task/request being routed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskRequirements {
    pub capability_tags: Vec<String>,
    pub min_context_tokens: u32,
    pub budget_units: Option<u32>,
    pub prior_failure_count: u32,
}

/// A capacity-registry candidate after policy ranking.
#[derive(Debug, Clone, PartialEq)]
pub enum RankedBackend {
    Inference {
        tier: RoutingTier,
        deployment: InferenceDeployment,
    },
    Cloud {
        backend: String,
        score: f64,
    },
}

/// Rank every eligible backend in a snapshot, cheapest-capable-first.
///
/// Attempt zero preserves today's ladder: 30B, then 480B, then cloud. A prior
/// failed dispatch skips both local lanes and starts at cloud.
pub fn rank_backends(
    snapshot: &CapacitySnapshot,
    requirements: &TaskRequirements,
    config: &PolicyConfig,
    computer_id: uuid::Uuid,
    fresh_after: DateTime<Utc>,
) -> Vec<RankedBackend> {
    let mut ranked = rank_inference_deployments(snapshot, requirements, config);
    ranked.extend(
        rank_cloud_backends(
            snapshot.backend_capacity(computer_id),
            fresh_after,
            requirements,
            config,
        )
        .into_iter()
        .map(|(backend, score)| RankedBackend::Cloud { backend, score }),
    );
    ranked
}

/// Rank registry deployments by escalation tier. Callers may retain live-load
/// ordering within a tier.
pub fn rank_inference_deployments(
    snapshot: &CapacitySnapshot,
    requirements: &TaskRequirements,
    config: &PolicyConfig,
) -> Vec<RankedBackend> {
    if requirements.prior_failure_count > 0 {
        return Vec::new();
    }
    let mut deployments: Vec<_> = snapshot
        .inference_deployments()
        .into_iter()
        .filter_map(|deployment| {
            let tier = deployment_tier(&deployment);
            tier_allowed(tier, requirements, config)
                .then_some(RankedBackend::Inference { tier, deployment })
        })
        .collect();
    deployments.sort_by_key(|candidate| match candidate {
        RankedBackend::Inference { tier, .. } => *tier,
        RankedBackend::Cloud { .. } => RoutingTier::Cloud,
    });
    deployments
}

/// Whether dispatch should try its 30B local lane before escalating.
pub fn use_local_30b(requirements: &TaskRequirements, config: &PolicyConfig) -> bool {
    requirements.prior_failure_count == 0
        && tier_allowed(RoutingTier::Local30B, requirements, config)
}

/// Rank cloud CLI backends using the council-approved score.
pub fn rank_cloud_backends(
    rows: Vec<BackendCapacity>,
    fresh_after: DateTime<Utc>,
    requirements: &TaskRequirements,
    config: &PolicyConfig,
) -> Vec<(String, f64)> {
    if !tier_allowed(RoutingTier::Cloud, requirements, config) {
        return Vec::new();
    }
    let mut scored: Vec<_> = rows
        .into_iter()
        .filter(|row| {
            row.installed
                && row.authenticated
                && row.last_checked_at > fresh_after
                && row.breaker_state != "open"
                && row.remaining_pct.unwrap_or(100.0) >= config.backend_headroom_floor_pct
        })
        .map(|row| {
            let score = backend_score_with_config(
                &row.backend,
                row.remaining_pct,
                &row.breaker_state,
                config,
            );
            (row.backend, score)
        })
        .collect();
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| backend_rank(&a.0).cmp(&backend_rank(&b.0)))
    });
    if let Some(preferred) = &config.preferred_cloud_backstop
        && let Some(index) = scored.iter().position(|(backend, _)| backend == preferred)
    {
        let preferred = scored.remove(index);
        scored.insert(0, preferred);
    }
    scored
}

/// Promote a configured build backstop after ordinary score ordering.
pub fn promote_cloud_backstop(backends: &mut Vec<String>, config: &PolicyConfig) {
    if let Some(preferred) = &config.preferred_cloud_backstop
        && let Some(index) = backends.iter().position(|backend| backend == preferred)
    {
        let preferred = backends.remove(index);
        backends.insert(0, preferred);
    }
}

fn tier_allowed(tier: RoutingTier, req: &TaskRequirements, config: &PolicyConfig) -> bool {
    let Some(budget) = config.budgets.iter().find(|budget| budget.tier == tier) else {
        return false;
    };
    req.budget_units
        .is_none_or(|limit| budget.cost_units <= limit)
        && budget.min_context_tokens >= req.min_context_tokens
        && req.capability_tags.iter().all(|tag| {
            budget
                .capability_tags
                .iter()
                .any(|available| available == tag)
        })
}

fn deployment_tier(deployment: &InferenceDeployment) -> RoutingTier {
    let identity = format!(
        "{} {}",
        deployment.catalog_id,
        deployment.catalog_family.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase();
    if identity.contains("480b") {
        RoutingTier::Local480B
    } else {
        RoutingTier::Local30B
    }
}

/// Dispatch-preference rank for a cloud CLI backend. Lower is preferred.
pub fn backend_rank(backend: &str) -> u8 {
    match backend {
        "codex" => 0,
        "claude" => 1,
        "kimi" => 2,
        "gemini" => 3,
        "grok" => 4,
        _ => 9,
    }
}

/// Council-tuned cloud score using the parity/default policy weights.
pub fn backend_score(backend: &str, remaining_pct: Option<f64>, breaker_state: &str) -> f64 {
    backend_score_with_config(
        backend,
        remaining_pct,
        breaker_state,
        &PolicyConfig::default(),
    )
}

fn backend_score_with_config(
    backend: &str,
    remaining_pct: Option<f64>,
    breaker_state: &str,
    config: &PolicyConfig,
) -> f64 {
    let headroom = (remaining_pct.unwrap_or(100.0) / 100.0).clamp(0.0, 1.0);
    let preference = 1.0 - (backend_rank(backend) as f64 / 9.0);
    let health = match breaker_state {
        "half_open" => 0.5,
        "open" => 0.0,
        _ => 1.0,
    };
    config.headroom_weight * headroom
        + config.preference_weight * preference
        + config.health_weight * health
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_rank_preserves_existing_order() {
        let mut backends = vec!["grok", "claude", "codex", "kimi", "gemini"];
        backends.sort_by_key(|backend| backend_rank(backend));
        assert_eq!(backends, ["codex", "claude", "kimi", "gemini", "grok"]);
    }

    #[test]
    fn score_preserves_headroom_and_health_behavior() {
        assert!(backend_score("codex", None, "closed") > backend_score("claude", None, "closed"));
        assert!(
            backend_score("kimi", Some(99.0), "closed")
                > backend_score("codex", Some(5.0), "closed")
        );
        assert!(
            backend_score("codex", Some(50.0), "closed")
                > backend_score("codex", Some(50.0), "half_open")
        );
    }

    #[test]
    fn prior_failure_escalates_past_local_tiers() {
        let config = PolicyConfig::default();
        let first = TaskRequirements::default();
        assert!(use_local_30b(&first, &config));
        let retry = TaskRequirements {
            prior_failure_count: 1,
            ..first
        };
        assert!(!use_local_30b(&retry, &config));
    }

    #[test]
    fn budget_and_capabilities_are_declarative_filters() {
        let config = PolicyConfig::default();
        let local_only = TaskRequirements {
            budget_units: Some(0),
            ..Default::default()
        };
        assert!(!tier_allowed(RoutingTier::Cloud, &local_only, &config));
        let cloud = TaskRequirements {
            capability_tags: vec!["cloud".into()],
            ..Default::default()
        };
        assert!(!tier_allowed(RoutingTier::Local30B, &cloud, &config));
        assert!(tier_allowed(RoutingTier::Cloud, &cloud, &config));
    }
}
