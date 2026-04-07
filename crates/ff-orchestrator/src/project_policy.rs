//! Project execution profiles + policy engine.
//!
//! This module lets ForgeFleet behave differently per project (HireFlow,
//! Vymatik, AuraOS, KovaBody, etc.) by mapping a project's profile to concrete
//! execution policy:
//! - routing constraints (tier/model bounds)
//! - review/test strictness
//! - rollout strategy
//! - human approval thresholds

use serde::{Deserialize, Serialize};

use ff_core::Tier;

use crate::router::RouteConstraints;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStrictness {
    Relaxed,
    #[default]
    Standard,
    Strict,
    Paranoid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DataSensitivity {
    Public,
    #[default]
    Internal,
    Confidential,
    Regulated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComplianceFlag {
    Soc2,
    Hipaa,
    PciDss,
    Gdpr,
    Pii,
    Phi,
    Finra,
    Iso27001,
    Custom(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentTarget {
    Local,
    Staging,
    Production,
    Edge,
    Mobile,
    Desktop,
    OnPrem,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestRequirements {
    pub require_unit: bool,
    pub require_integration: bool,
    pub require_e2e: bool,
    pub minimum_coverage_pct: Option<f32>,
    pub required_commands: Vec<String>,
}

impl Default for TestRequirements {
    fn default() -> Self {
        Self {
            require_unit: true,
            require_integration: false,
            require_e2e: false,
            minimum_coverage_pct: None,
            required_commands: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierAccessPolicy {
    pub min_tier: u8,
    pub max_tier: u8,
    pub allowed_models: Vec<String>,
}

impl Default for TierAccessPolicy {
    fn default() -> Self {
        Self {
            min_tier: 1,
            max_tier: 4,
            allowed_models: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectExecutionProfile {
    pub project_id: String,
    pub display_name: String,
    pub stack: Vec<String>,
    pub languages: Vec<String>,
    pub deployment_targets: Vec<DeploymentTarget>,
    pub review_strictness: ReviewStrictness,
    pub test_requirements: TestRequirements,
    pub allowed_tiers: TierAccessPolicy,
    pub data_sensitivity: DataSensitivity,
    pub compliance_flags: Vec<ComplianceFlag>,
}

impl Default for ProjectExecutionProfile {
    fn default() -> Self {
        Self {
            project_id: "default".to_string(),
            display_name: "Default Project".to_string(),
            stack: Vec::new(),
            languages: Vec::new(),
            deployment_targets: vec![DeploymentTarget::Local],
            review_strictness: ReviewStrictness::Standard,
            test_requirements: TestRequirements::default(),
            allowed_tiers: TierAccessPolicy::default(),
            data_sensitivity: DataSensitivity::Internal,
            compliance_flags: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutStrategy {
    Direct,
    Canary,
    BlueGreen,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingPolicy {
    pub min_tier: u8,
    pub max_tier: u8,
    pub allowed_models: Vec<String>,
    pub prefer_local_inference: bool,
    pub forbid_external_models: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRequirements {
    pub required: bool,
    pub strictness: ReviewStrictness,
    pub min_reviewers: u8,
    pub require_security_review: bool,
    pub require_architecture_review: bool,
    pub require_tests: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutPolicy {
    pub strategy: RolloutStrategy,
    pub require_staging: bool,
    pub progressive_steps_pct: Vec<u8>,
    pub auto_rollback: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanApprovalLevel {
    None,
    Elevated,
    Strict,
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalTrigger {
    ProductionDeploy,
    SchemaChange,
    SensitiveDataAccess,
    ExternalIntegration,
    ComplianceCritical,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HumanApprovalPolicy {
    pub level: HumanApprovalLevel,
    pub triggers: Vec<ApprovalTrigger>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionPolicy {
    pub routing: RoutingPolicy,
    pub review: ReviewRequirements,
    pub rollout: RolloutPolicy,
    pub human_approval: HumanApprovalPolicy,
}

#[derive(Debug, Clone, Default)]
pub struct ProjectPolicyEngine;

impl ProjectPolicyEngine {
    pub fn derive(profile: &ProjectExecutionProfile) -> ExecutionPolicy {
        let (mut min_tier, mut max_tier) = normalize_tier_bounds(
            profile.allowed_tiers.min_tier,
            profile.allowed_tiers.max_tier,
        );

        // Sensitive projects should never route trivially weak by default.
        match profile.data_sensitivity {
            DataSensitivity::Public | DataSensitivity::Internal => {}
            DataSensitivity::Confidential => {
                min_tier = min_tier.max(2);
            }
            DataSensitivity::Regulated => {
                min_tier = min_tier.max(2);
                max_tier = max_tier.max(3);
            }
        }

        if matches!(
            profile.review_strictness,
            ReviewStrictness::Strict | ReviewStrictness::Paranoid
        ) {
            min_tier = min_tier.max(2);
            max_tier = max_tier.max(3);
        }

        let has_prod = profile
            .deployment_targets
            .contains(&DeploymentTarget::Production);
        let has_sensitive_compliance = profile.compliance_flags.iter().any(|flag| {
            matches!(
                flag,
                ComplianceFlag::Hipaa
                    | ComplianceFlag::PciDss
                    | ComplianceFlag::Phi
                    | ComplianceFlag::Pii
                    | ComplianceFlag::Finra
            )
        });

        let review = match profile.review_strictness {
            ReviewStrictness::Relaxed => ReviewRequirements {
                required: false,
                strictness: profile.review_strictness,
                min_reviewers: 0,
                require_security_review: false,
                require_architecture_review: false,
                require_tests: profile.test_requirements.require_unit
                    || profile.test_requirements.require_integration
                    || profile.test_requirements.require_e2e,
            },
            ReviewStrictness::Standard => ReviewRequirements {
                required: true,
                strictness: profile.review_strictness,
                min_reviewers: 1,
                require_security_review: has_sensitive_compliance,
                require_architecture_review: false,
                require_tests: true,
            },
            ReviewStrictness::Strict => ReviewRequirements {
                required: true,
                strictness: profile.review_strictness,
                min_reviewers: 2,
                require_security_review: true,
                require_architecture_review: true,
                require_tests: true,
            },
            ReviewStrictness::Paranoid => ReviewRequirements {
                required: true,
                strictness: profile.review_strictness,
                min_reviewers: 2,
                require_security_review: true,
                require_architecture_review: true,
                require_tests: true,
            },
        };

        let rollout = if has_prod {
            match profile.review_strictness {
                ReviewStrictness::Relaxed => RolloutPolicy {
                    strategy: RolloutStrategy::Direct,
                    require_staging: true,
                    progressive_steps_pct: vec![100],
                    auto_rollback: true,
                },
                ReviewStrictness::Standard => RolloutPolicy {
                    strategy: RolloutStrategy::Canary,
                    require_staging: true,
                    progressive_steps_pct: vec![10, 50, 100],
                    auto_rollback: true,
                },
                ReviewStrictness::Strict => RolloutPolicy {
                    strategy: RolloutStrategy::BlueGreen,
                    require_staging: true,
                    progressive_steps_pct: vec![5, 25, 50, 100],
                    auto_rollback: true,
                },
                ReviewStrictness::Paranoid => RolloutPolicy {
                    strategy: RolloutStrategy::Manual,
                    require_staging: true,
                    progressive_steps_pct: vec![1, 5, 20, 50, 100],
                    auto_rollback: true,
                },
            }
        } else {
            RolloutPolicy {
                strategy: RolloutStrategy::Direct,
                require_staging: false,
                progressive_steps_pct: vec![100],
                auto_rollback: false,
            }
        };

        let human_approval = match (
            profile.data_sensitivity,
            profile.review_strictness,
            has_prod,
            has_sensitive_compliance,
        ) {
            (DataSensitivity::Regulated, _, _, _) | (_, ReviewStrictness::Paranoid, _, _) => {
                HumanApprovalPolicy {
                    level: HumanApprovalLevel::Always,
                    triggers: vec![
                        ApprovalTrigger::ProductionDeploy,
                        ApprovalTrigger::SchemaChange,
                        ApprovalTrigger::SensitiveDataAccess,
                        ApprovalTrigger::ExternalIntegration,
                        ApprovalTrigger::ComplianceCritical,
                    ],
                }
            }
            (DataSensitivity::Confidential, _, _, _)
            | (_, ReviewStrictness::Strict, _, _)
            | (_, _, _, true) => HumanApprovalPolicy {
                level: HumanApprovalLevel::Strict,
                triggers: vec![
                    ApprovalTrigger::ProductionDeploy,
                    ApprovalTrigger::SchemaChange,
                    ApprovalTrigger::SensitiveDataAccess,
                    ApprovalTrigger::ExternalIntegration,
                ],
            },
            (_, _, true, _) => HumanApprovalPolicy {
                level: HumanApprovalLevel::Elevated,
                triggers: vec![
                    ApprovalTrigger::ProductionDeploy,
                    ApprovalTrigger::SchemaChange,
                ],
            },
            _ => HumanApprovalPolicy {
                level: HumanApprovalLevel::None,
                triggers: Vec::new(),
            },
        };

        ExecutionPolicy {
            routing: RoutingPolicy {
                min_tier,
                max_tier,
                allowed_models: profile.allowed_tiers.allowed_models.clone(),
                prefer_local_inference: matches!(
                    profile.data_sensitivity,
                    DataSensitivity::Confidential | DataSensitivity::Regulated
                ),
                forbid_external_models: has_sensitive_compliance,
            },
            review,
            rollout,
            human_approval,
        }
    }

    pub fn clamp_tiers(policy: &RoutingPolicy, requested_start: u8, requested_max: u8) -> (u8, u8) {
        let (min_tier, max_tier) = normalize_tier_bounds(policy.min_tier, policy.max_tier);
        let start = requested_start.clamp(min_tier, max_tier);
        let max = requested_max.clamp(start, max_tier);
        (start, max)
    }

    pub fn to_route_constraints(policy: &RoutingPolicy) -> RouteConstraints {
        RouteConstraints {
            min_tier: Tier::from_u8(policy.min_tier),
            max_tier: Tier::from_u8(policy.max_tier),
            preferred_nodes: Vec::new(),
            excluded_nodes: Vec::new(),
            max_latency_ms: None,
        }
    }

    pub fn model_allowed(policy: &RoutingPolicy, model_selector: &str) -> bool {
        if policy.allowed_models.is_empty() {
            return true;
        }

        let selector = model_selector.trim().to_ascii_lowercase();
        if selector.is_empty()
            || matches!(
                selector.as_str(),
                "auto"
                    | "adaptive"
                    | "fast"
                    | "small"
                    | "medium"
                    | "large"
                    | "expert"
                    | "tier1"
                    | "tier2"
                    | "tier3"
                    | "tier4"
            )
        {
            return true;
        }

        policy
            .allowed_models
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(model_selector))
    }

    /// Returns a reason string if human approval is required.
    pub fn approval_reason(policy: &HumanApprovalPolicy, operation_text: &str) -> Option<String> {
        if policy.level == HumanApprovalLevel::None {
            return None;
        }

        let text = operation_text.to_ascii_lowercase();
        let mut matched = Vec::new();

        for trigger in &policy.triggers {
            let hit = match trigger {
                ApprovalTrigger::ProductionDeploy => contains_any(
                    &text,
                    &["deploy", "release", "prod", "production", "rollout"],
                ),
                ApprovalTrigger::SchemaChange => contains_any(
                    &text,
                    &["migration", "schema", "ddl", "drop table", "alter table"],
                ),
                ApprovalTrigger::SensitiveDataAccess => contains_any(
                    &text,
                    &[
                        "pii",
                        "phi",
                        "customer data",
                        "personal data",
                        "export data",
                        "token",
                        "secret",
                    ],
                ),
                ApprovalTrigger::ExternalIntegration => contains_any(
                    &text,
                    &[
                        "webhook",
                        "third-party",
                        "external api",
                        "post to",
                        "send email",
                        "github",
                    ],
                ),
                ApprovalTrigger::ComplianceCritical => contains_any(
                    &text,
                    &["compliance", "audit", "regulated", "hipaa", "pci", "soc2"],
                ),
            };

            if hit {
                matched.push(format!("{trigger:?}"));
            }
        }

        match policy.level {
            HumanApprovalLevel::Always => {
                if matched.is_empty() {
                    Some("policy level always requires explicit human approval".to_string())
                } else {
                    Some(format!(
                        "human approval required by policy (matched triggers: {})",
                        matched.join(", ")
                    ))
                }
            }
            HumanApprovalLevel::Elevated | HumanApprovalLevel::Strict => {
                if matched.is_empty() {
                    None
                } else {
                    Some(format!(
                        "human approval required by policy (matched triggers: {})",
                        matched.join(", ")
                    ))
                }
            }
            HumanApprovalLevel::None => None,
        }
    }
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn normalize_tier_bounds(min_tier: u8, max_tier: u8) -> (u8, u8) {
    let min_tier = min_tier.clamp(1, 4);
    let max_tier = max_tier.clamp(min_tier, 4);
    (min_tier, max_tier)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regulated_profile_derives_strict_policy() {
        let profile = ProjectExecutionProfile {
            project_id: "hireflow".to_string(),
            display_name: "HireFlow".to_string(),
            deployment_targets: vec![DeploymentTarget::Production],
            review_strictness: ReviewStrictness::Paranoid,
            data_sensitivity: DataSensitivity::Regulated,
            compliance_flags: vec![ComplianceFlag::Hipaa],
            ..ProjectExecutionProfile::default()
        };

        let policy = ProjectPolicyEngine::derive(&profile);
        assert!(policy.routing.min_tier >= 2);
        assert!(policy.review.required);
        assert!(policy.review.min_reviewers >= 2);
        assert_eq!(policy.human_approval.level, HumanApprovalLevel::Always);
        assert!(policy.routing.forbid_external_models);
    }

    #[test]
    fn clamp_tiers_respects_bounds() {
        let policy = RoutingPolicy {
            min_tier: 2,
            max_tier: 3,
            allowed_models: vec![],
            prefer_local_inference: true,
            forbid_external_models: false,
        };

        let (start, max) = ProjectPolicyEngine::clamp_tiers(&policy, 1, 4);
        assert_eq!((start, max), (2, 3));

        let (start, max) = ProjectPolicyEngine::clamp_tiers(&policy, 3, 2);
        assert_eq!((start, max), (3, 3));
    }

    #[test]
    fn approval_reason_detects_deploy_trigger() {
        let policy = HumanApprovalPolicy {
            level: HumanApprovalLevel::Strict,
            triggers: vec![ApprovalTrigger::ProductionDeploy],
        };

        let reason =
            ProjectPolicyEngine::approval_reason(&policy, "Deploy this change to production");
        assert!(reason.is_some());
    }

    #[test]
    fn model_allowlist_blocks_unknown_explicit_model() {
        let policy = RoutingPolicy {
            min_tier: 1,
            max_tier: 4,
            allowed_models: vec!["qwen-32b".to_string()],
            prefer_local_inference: false,
            forbid_external_models: false,
        };

        assert!(ProjectPolicyEngine::model_allowed(&policy, "auto"));
        assert!(ProjectPolicyEngine::model_allowed(&policy, "qwen-32b"));
        assert!(!ProjectPolicyEngine::model_allowed(&policy, "gpt-4"));
    }
}
