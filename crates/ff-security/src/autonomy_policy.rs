use serde::{Deserialize, Serialize};

/// High-level action class used by autonomy policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionType {
    OperationalRead,
    MutatingOperation,
    DestructiveOperation,
    Administrative,
}

impl ActionType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OperationalRead => "operational_read",
            Self::MutatingOperation => "mutating_operation",
            Self::DestructiveOperation => "destructive_operation",
            Self::Administrative => "administrative",
        }
    }
}

/// Compliance sensitivity of the requested action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComplianceLevel {
    Standard,
    Elevated,
    ComplianceCritical,
}

impl ComplianceLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Elevated => "elevated",
            Self::ComplianceCritical => "compliance_critical",
        }
    }
}

/// Runtime risk estimation for the requested action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

impl RiskLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// Autonomy policy decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    AutoAllow,
    RequireHumanApproval,
    Deny,
}

impl Decision {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AutoAllow => "auto_allow",
            Self::RequireHumanApproval => "require_human_approval",
            Self::Deny => "deny",
        }
    }
}

/// Decide whether an action can execute autonomously.
///
/// Matrix defaults:
/// - low-risk operational reads => `AutoAllow`
/// - medium-risk mutating operations => `RequireHumanApproval`
/// - high-risk destructive operations => `Deny`
/// - compliance-critical contexts => `RequireHumanApproval` unless denied by higher-risk destructive rule
pub fn decide(action: ActionType, compliance: ComplianceLevel, risk: RiskLevel) -> Decision {
    match action {
        ActionType::OperationalRead => match (compliance, risk) {
            (ComplianceLevel::Standard, RiskLevel::Low) => Decision::AutoAllow,
            (ComplianceLevel::Standard, RiskLevel::Medium) => Decision::RequireHumanApproval,
            (ComplianceLevel::Standard, RiskLevel::High) => Decision::RequireHumanApproval,
            (ComplianceLevel::Elevated, RiskLevel::Low) => Decision::AutoAllow,
            (ComplianceLevel::Elevated, RiskLevel::Medium) => Decision::RequireHumanApproval,
            (ComplianceLevel::Elevated, RiskLevel::High) => Decision::RequireHumanApproval,
            (ComplianceLevel::ComplianceCritical, RiskLevel::Low) => Decision::RequireHumanApproval,
            (ComplianceLevel::ComplianceCritical, RiskLevel::Medium) => {
                Decision::RequireHumanApproval
            }
            (ComplianceLevel::ComplianceCritical, RiskLevel::High) => {
                Decision::RequireHumanApproval
            }
        },
        ActionType::MutatingOperation => match (compliance, risk) {
            (ComplianceLevel::Standard, RiskLevel::Low) => Decision::AutoAllow,
            (ComplianceLevel::Standard, RiskLevel::Medium) => Decision::RequireHumanApproval,
            (ComplianceLevel::Standard, RiskLevel::High) => Decision::RequireHumanApproval,
            (ComplianceLevel::Elevated, RiskLevel::Low) => Decision::AutoAllow,
            (ComplianceLevel::Elevated, RiskLevel::Medium) => Decision::RequireHumanApproval,
            (ComplianceLevel::Elevated, RiskLevel::High) => Decision::RequireHumanApproval,
            (ComplianceLevel::ComplianceCritical, RiskLevel::Low) => Decision::RequireHumanApproval,
            (ComplianceLevel::ComplianceCritical, RiskLevel::Medium) => {
                Decision::RequireHumanApproval
            }
            (ComplianceLevel::ComplianceCritical, RiskLevel::High) => {
                Decision::RequireHumanApproval
            }
        },
        ActionType::DestructiveOperation => match (compliance, risk) {
            (ComplianceLevel::Standard, RiskLevel::Low) => Decision::RequireHumanApproval,
            (ComplianceLevel::Standard, RiskLevel::Medium) => Decision::RequireHumanApproval,
            (ComplianceLevel::Standard, RiskLevel::High) => Decision::Deny,
            (ComplianceLevel::Elevated, RiskLevel::Low) => Decision::RequireHumanApproval,
            (ComplianceLevel::Elevated, RiskLevel::Medium) => Decision::RequireHumanApproval,
            (ComplianceLevel::Elevated, RiskLevel::High) => Decision::Deny,
            (ComplianceLevel::ComplianceCritical, RiskLevel::Low) => Decision::RequireHumanApproval,
            (ComplianceLevel::ComplianceCritical, RiskLevel::Medium) => {
                Decision::RequireHumanApproval
            }
            (ComplianceLevel::ComplianceCritical, RiskLevel::High) => Decision::Deny,
        },
        ActionType::Administrative => match (compliance, risk) {
            (ComplianceLevel::Standard, RiskLevel::Low) => Decision::RequireHumanApproval,
            (ComplianceLevel::Standard, RiskLevel::Medium) => Decision::RequireHumanApproval,
            (ComplianceLevel::Standard, RiskLevel::High) => Decision::RequireHumanApproval,
            (ComplianceLevel::Elevated, RiskLevel::Low) => Decision::RequireHumanApproval,
            (ComplianceLevel::Elevated, RiskLevel::Medium) => Decision::RequireHumanApproval,
            (ComplianceLevel::Elevated, RiskLevel::High) => Decision::RequireHumanApproval,
            (ComplianceLevel::ComplianceCritical, RiskLevel::Low) => Decision::RequireHumanApproval,
            (ComplianceLevel::ComplianceCritical, RiskLevel::Medium) => {
                Decision::RequireHumanApproval
            }
            (ComplianceLevel::ComplianceCritical, RiskLevel::High) => {
                Decision::RequireHumanApproval
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_risk_operational_read_auto_allows() {
        let decision = decide(
            ActionType::OperationalRead,
            ComplianceLevel::Standard,
            RiskLevel::Low,
        );
        assert_eq!(decision, Decision::AutoAllow);
    }

    #[test]
    fn medium_risk_mutation_requires_human_approval() {
        let decision = decide(
            ActionType::MutatingOperation,
            ComplianceLevel::Standard,
            RiskLevel::Medium,
        );
        assert_eq!(decision, Decision::RequireHumanApproval);
    }

    #[test]
    fn high_risk_destructive_is_denied() {
        let decision = decide(
            ActionType::DestructiveOperation,
            ComplianceLevel::Standard,
            RiskLevel::High,
        );
        assert_eq!(decision, Decision::Deny);
    }

    #[test]
    fn compliance_critical_requires_human_approval() {
        let decision = decide(
            ActionType::OperationalRead,
            ComplianceLevel::ComplianceCritical,
            RiskLevel::Medium,
        );
        assert_eq!(decision, Decision::RequireHumanApproval);
    }

    #[test]
    fn administrative_actions_never_auto_allow() {
        for compliance in [
            ComplianceLevel::Standard,
            ComplianceLevel::Elevated,
            ComplianceLevel::ComplianceCritical,
        ] {
            for risk in [RiskLevel::Low, RiskLevel::Medium, RiskLevel::High] {
                assert_eq!(
                    decide(ActionType::Administrative, compliance, risk),
                    Decision::RequireHumanApproval
                );
            }
        }
    }

    #[test]
    fn destructive_high_risk_always_denied() {
        for compliance in [
            ComplianceLevel::Standard,
            ComplianceLevel::Elevated,
            ComplianceLevel::ComplianceCritical,
        ] {
            assert_eq!(
                decide(
                    ActionType::DestructiveOperation,
                    compliance,
                    RiskLevel::High
                ),
                Decision::Deny
            );
        }
    }
}
