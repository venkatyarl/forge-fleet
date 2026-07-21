//! Pure, in-process routing policy over the shared capacity registry.
//!
//! This crate owns ordering and escalation only. Capacity discovery remains in
//! `ff-capacity`; live load and execution remain responsibilities of callers.

use chrono::{DateTime, Utc};
use ff_capacity::{BackendCapacity, CapacitySnapshot, InferenceDeployment};
use serde::Serialize;

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
    /// Minimum remaining weekly/monthly cloud allowance. Budget rows store
    /// percent used, so 15% remaining corresponds to rejecting values > 85.
    pub cloud_budget_headroom_floor_pct: i16,
    /// Coarse per-dispatch estimate used for decision telemetry until a caller
    /// has request/model-specific pricing data.
    pub cloud_estimated_cost_usd: f64,
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
            cloud_budget_headroom_floor_pct: 15,
            cloud_estimated_cost_usd: 0.0,
            headroom_weight: 0.60,
            preference_weight: 0.30,
            health_weight: 0.10,
            preferred_cloud_backstop: None,
        }
    }
}

/// Declarative provider allowance loaded by a caller from
/// `cloud_budget_buckets`. The policy stays pure and database-independent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudBudget {
    pub provider: String,
    /// Percent used, not percent remaining.
    pub weekly_pct: Option<i16>,
    /// Percent used, not percent remaining.
    pub monthly_pct: Option<i16>,
    pub window_exhausted_until: Option<DateTime<Utc>>,
}

impl From<ff_capacity::CloudBudgetCapacity> for CloudBudget {
    fn from(row: ff_capacity::CloudBudgetCapacity) -> Self {
        Self {
            provider: row.provider,
            weekly_pct: row.weekly_pct,
            monthly_pct: row.monthly_pct,
            window_exhausted_until: row.window_exhausted_until,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CandidateDecision {
    pub backend: String,
    pub score: Option<f64>,
    pub estimated_cost_usd: f64,
    pub rejected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejection_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejection_reason: Option<String>,
}

/// Stable, serializable explanation shared by real dispatch and `ff route debug`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RouteDecision {
    pub schema_version: u8,
    pub trace_id: uuid::Uuid,
    pub mode: String,
    pub candidates: Vec<CandidateDecision>,
    pub chosen: Option<String>,
    pub estimated_cost_usd: f64,
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

/// Whether dispatch should try its local coder lane before escalating to cloud.
/// Local lane is heartbeat-safe since #62/#792 (codegen_apply moved blocking git/fs/verify
/// off the async runtime via spawn_blocking), so the old "one try then cloud" cloud-bias is
/// obsolete: keep capable local coders (Devstral) working for up to LOCAL_LANE_MAX_TRIES
/// attempts before overflowing to rate-limited cloud. Cloud stays the backstop for tasks
/// explicitly complexity-routed to it (prefers_cloud capability tag) and for the tail beyond this.
pub fn use_local_30b(requirements: &TaskRequirements, config: &PolicyConfig) -> bool {
    requirements.prior_failure_count < LOCAL_LANE_MAX_TRIES
        && tier_allowed(RoutingTier::Local30B, requirements, config)
}

/// Number of local-coder attempts before dispatch escalates to the cloud lane. Was implicitly 1
/// (`== 0`) as a workaround for the local lane starving the async heartbeat (#62) — now fixed.
pub const LOCAL_LANE_MAX_TRIES: u32 = 3;

/// Rank cloud CLI backends using the council-approved score.
pub fn rank_cloud_backends(
    rows: Vec<BackendCapacity>,
    fresh_after: DateTime<Utc>,
    requirements: &TaskRequirements,
    config: &PolicyConfig,
) -> Vec<(String, f64)> {
    evaluate_cloud_route(
        rows,
        &[],
        fresh_after,
        requirements,
        config,
        Utc::now(),
        uuid::Uuid::new_v4(),
        "legacy",
    )
    .candidates
    .into_iter()
    .filter(|candidate| !candidate.rejected)
    .map(|candidate| (candidate.backend, candidate.score.unwrap_or_default()))
    .collect()
}

/// Evaluate and explain every cloud candidate, including ineligible rows.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_cloud_route(
    rows: Vec<BackendCapacity>,
    budgets: &[CloudBudget],
    fresh_after: DateTime<Utc>,
    requirements: &TaskRequirements,
    config: &PolicyConfig,
    now: DateTime<Utc>,
    trace_id: uuid::Uuid,
    mode: impl Into<String>,
) -> RouteDecision {
    if !tier_allowed(RoutingTier::Cloud, requirements, config) {
        return RouteDecision {
            schema_version: 1,
            trace_id,
            mode: mode.into(),
            candidates: rows
                .into_iter()
                .map(|row| {
                    rejected_candidate(
                        row.backend,
                        "tier_budget",
                        "cloud tier is not allowed by task budget or capabilities",
                        config,
                    )
                })
                .collect(),
            chosen: None,
            estimated_cost_usd: 0.0,
        };
    }
    let mut candidates: Vec<_> = rows
        .into_iter()
        .map(|row| {
            let rejection = capacity_rejection(&row, fresh_after, config).or_else(|| {
                budgets
                    .iter()
                    .find(|budget| budget.provider.eq_ignore_ascii_case(&row.backend))
                    .and_then(|budget| budget_rejection(budget, now, config))
            });
            let score = backend_score_with_config(
                &row.backend,
                row.remaining_pct,
                &row.breaker_state,
                config,
            );
            CandidateDecision {
                backend: row.backend,
                score: rejection.is_none().then_some(score),
                estimated_cost_usd: config.cloud_estimated_cost_usd,
                rejected: rejection.is_some(),
                rejection_code: rejection.as_ref().map(|(code, _)| (*code).to_string()),
                rejection_reason: rejection.map(|(_, reason)| reason),
            }
        })
        .collect();
    candidates.sort_by(|a, b| {
        a.rejected.cmp(&b.rejected).then_with(|| {
            b.score
                .unwrap_or_default()
                .partial_cmp(&a.score.unwrap_or_default())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| backend_rank(&a.backend).cmp(&backend_rank(&b.backend)))
        })
    });
    if let Some(preferred) = &config.preferred_cloud_backstop
        && let Some(index) = candidates
            .iter()
            .position(|candidate| !candidate.rejected && &candidate.backend == preferred)
    {
        let preferred = candidates.remove(index);
        candidates.insert(0, preferred);
    }
    let chosen = candidates
        .iter()
        .find(|candidate| !candidate.rejected)
        .map(|candidate| candidate.backend.clone());
    RouteDecision {
        schema_version: 1,
        trace_id,
        mode: mode.into(),
        candidates,
        estimated_cost_usd: chosen
            .as_ref()
            .map_or(0.0, |_| config.cloud_estimated_cost_usd),
        chosen,
    }
}

fn rejected_candidate(
    backend: String,
    code: &str,
    reason: &str,
    config: &PolicyConfig,
) -> CandidateDecision {
    CandidateDecision {
        backend,
        score: None,
        estimated_cost_usd: config.cloud_estimated_cost_usd,
        rejected: true,
        rejection_code: Some(code.to_string()),
        rejection_reason: Some(reason.to_string()),
    }
}

fn capacity_rejection(
    row: &BackendCapacity,
    fresh_after: DateTime<Utc>,
    config: &PolicyConfig,
) -> Option<(&'static str, String)> {
    if !row.installed {
        Some(("not_installed", "backend is not installed".into()))
    } else if !row.authenticated {
        Some(("not_authenticated", "backend is not authenticated".into()))
    } else if row.last_checked_at <= fresh_after {
        Some(("stale", "backend health check is stale".into()))
    } else if row.breaker_state == "open" {
        Some(("breaker_open", "backend circuit breaker is open".into()))
    } else if row.remaining_pct.unwrap_or(100.0) < config.backend_headroom_floor_pct {
        Some((
            "provider_headroom",
            format!(
                "provider headroom is below {:.0}%",
                config.backend_headroom_floor_pct
            ),
        ))
    } else {
        None
    }
}

fn budget_rejection(
    budget: &CloudBudget,
    now: DateTime<Utc>,
    config: &PolicyConfig,
) -> Option<(&'static str, String)> {
    if budget
        .window_exhausted_until
        .is_some_and(|until| until > now)
    {
        return Some((
            "window_exhausted",
            format!(
                "quota window is exhausted until {}",
                budget.window_exhausted_until.unwrap()
            ),
        ));
    }
    let max_used = 100 - config.cloud_budget_headroom_floor_pct;
    if budget.weekly_pct.is_some_and(|used| used > max_used) {
        return Some((
            "weekly_budget",
            format!(
                "weekly budget is {0}% used, leaving less than {1}% headroom",
                budget.weekly_pct.unwrap(),
                config.cloud_budget_headroom_floor_pct
            ),
        ));
    }
    if budget.monthly_pct.is_some_and(|used| used > max_used) {
        return Some((
            "monthly_budget",
            format!(
                "monthly budget is {0}% used, leaving less than {1}% headroom",
                budget.monthly_pct.unwrap(),
                config.cloud_budget_headroom_floor_pct
            ),
        ));
    }
    None
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

    fn backend(name: &str, now: DateTime<Utc>) -> BackendCapacity {
        BackendCapacity {
            computer_id: uuid::Uuid::nil(),
            backend: name.to_string(),
            installed: true,
            authenticated: true,
            last_checked_at: now,
            remaining_pct: Some(100.0),
            breaker_state: "closed".to_string(),
        }
    }

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
        // local lane is heartbeat-safe now (#62/#792): stays local for LOCAL_LANE_MAX_TRIES...
        let one_retry = TaskRequirements {
            prior_failure_count: 1,
            ..TaskRequirements::default()
        };
        assert!(use_local_30b(&one_retry, &config));
        // ...then escalates to cloud past the local-try budget.
        let exhausted = TaskRequirements {
            prior_failure_count: LOCAL_LANE_MAX_TRIES,
            ..TaskRequirements::default()
        };
        assert!(!use_local_30b(&exhausted, &config));
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

    #[test]
    fn cloud_budget_rejects_exhausted_windows_and_low_period_headroom() {
        let now = Utc::now();
        let budgets = vec![
            CloudBudget {
                provider: "codex".into(),
                weekly_pct: Some(86),
                monthly_pct: None,
                window_exhausted_until: None,
            },
            CloudBudget {
                provider: "claude".into(),
                weekly_pct: Some(10),
                monthly_pct: Some(90),
                window_exhausted_until: None,
            },
            CloudBudget {
                provider: "kimi".into(),
                weekly_pct: None,
                monthly_pct: None,
                window_exhausted_until: Some(now + chrono::Duration::minutes(5)),
            },
        ];
        let decision = evaluate_cloud_route(
            vec![
                backend("codex", now),
                backend("claude", now),
                backend("kimi", now),
            ],
            &budgets,
            now - chrono::Duration::minutes(1),
            &TaskRequirements::default(),
            &PolicyConfig::default(),
            now,
            uuid::Uuid::nil(),
            "test",
        );

        assert!(decision.chosen.is_none());
        assert_eq!(decision.candidates.len(), 3);
        assert!(
            decision
                .candidates
                .iter()
                .any(|candidate| candidate.rejection_code.as_deref() == Some("weekly_budget"))
        );
        assert!(
            decision
                .candidates
                .iter()
                .any(|candidate| candidate.rejection_code.as_deref() == Some("monthly_budget"))
        );
        assert!(
            decision
                .candidates
                .iter()
                .any(|candidate| candidate.rejection_code.as_deref() == Some("window_exhausted"))
        );
    }

    #[test]
    fn cloud_budget_boundary_and_missing_rows_remain_eligible() {
        let now = Utc::now();
        let decision = evaluate_cloud_route(
            vec![backend("codex", now), backend("grok", now)],
            &[CloudBudget {
                provider: "codex".into(),
                weekly_pct: Some(85),
                monthly_pct: Some(85),
                window_exhausted_until: Some(now),
            }],
            now - chrono::Duration::minutes(1),
            &TaskRequirements::default(),
            &PolicyConfig::default(),
            now,
            uuid::Uuid::nil(),
            "debug",
        );

        assert_eq!(decision.chosen.as_deref(), Some("codex"));
        assert!(
            decision
                .candidates
                .iter()
                .all(|candidate| !candidate.rejected)
        );
        assert_eq!(decision.mode, "debug");
    }
}
