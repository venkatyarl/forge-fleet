//! Strategy semantics over the request-scoped local executor.
//!
//! The semantic classifier, validators, cascade refine prompts, and judge are
//! still owned by `ff_orchestrator::cascade_strategy`. The executor underneath
//! them supplies one shared absolute deadline and the auditable attempt ledger.

use ff_orchestrator::cascade_strategy::{
    LlmExec, RouteStrategy, ValidationOutcome, ValidatorKind, classify_task, pick_strategy,
    run_cascade, run_judge_escalate,
};
use serde::Serialize;
use serde_json::{Value, json};
use tracing::info;

use crate::llm_exec::{
    AttemptRole, ExecutionEvidence, FailureReasonCode, GatewayLlmExec, WinningRoute,
};

pub fn parse_strategy(raw: &str) -> Result<&'static str, String> {
    match raw {
        "auto" | "single" | "cascade" | "judge_escalate" => Ok(match raw {
            "auto" => "auto",
            "single" => "single",
            "cascade" => "cascade",
            "judge_escalate" => "judge_escalate",
            _ => unreachable!(),
        }),
        other => Err(format!(
            "unknown strategy '{other}' (expected one of: auto, single, cascade, judge_escalate)"
        )),
    }
}

pub fn parse_validator(raw: Option<&str>) -> ValidatorKind {
    match raw.map(|value| value.to_lowercase()) {
        Some(value) if value == "json" => ValidatorKind::Json,
        Some(value) if value == "yaml" => ValidatorKind::Yaml,
        _ => ValidatorKind::None,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StrategyDispatchSuccess {
    pub output: String,
    pub strategy: RouteStrategy,
    pub trace: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub early_exit_at_tier: Option<u8>,
    #[serde(flatten)]
    pub execution: ExecutionEvidence,
}

impl StrategyDispatchSuccess {
    pub fn route_decision(&self) -> Value {
        json!({
            "strategy": self.strategy,
            "candidate_snapshot": self.execution.candidate_snapshot,
            "winner": self.execution.winner,
            "local_authority": self.execution.local_authority,
            "cloud_fallback": self.execution.cloud_fallback,
        })
    }

    pub fn winner(&self) -> Option<&WinningRoute> {
        self.execution.winner.as_ref()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StrategyDispatchFailure {
    pub reason_code: FailureReasonCode,
    pub message: String,
    pub strategy: Option<RouteStrategy>,
    #[serde(flatten)]
    pub execution: ExecutionEvidence,
}

impl StrategyDispatchFailure {
    fn from_execution(
        exec: &GatewayLlmExec,
        strategy: Option<RouteStrategy>,
        fallback_code: FailureReasonCode,
        message: impl Into<String>,
    ) -> Self {
        let execution = exec.evidence();
        Self {
            reason_code: execution.last_failure.unwrap_or(fallback_code),
            message: message.into(),
            strategy,
            execution,
        }
    }

    fn semantic(
        exec: &GatewayLlmExec,
        strategy: Option<RouteStrategy>,
        reason_code: FailureReasonCode,
        message: impl Into<String>,
    ) -> Self {
        Self {
            reason_code,
            message: message.into(),
            strategy,
            execution: exec.evidence(),
        }
    }

    fn judge_boundary(
        exec: &GatewayLlmExec,
        strategy: Option<RouteStrategy>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            reason_code: exec
                .last_role_failure(AttemptRole::Judge)
                .unwrap_or(FailureReasonCode::InvalidResponse),
            message: message.into(),
            strategy,
            execution: exec.evidence(),
        }
    }

    pub fn error_text(&self) -> String {
        format!("{}: {}", self.reason_code.as_str(), self.message)
    }

    pub fn route_decision(&self) -> Value {
        let last_attempt = self.execution.attempts.last();
        json!({
            "strategy": self.strategy,
            "candidate_snapshot": self.execution.candidate_snapshot,
            "last_attempt": last_attempt,
            "local_authority": self.execution.local_authority,
            "cloud_fallback": self.execution.cloud_fallback,
        })
    }
}

pub type StrategyDispatchResult = Result<StrategyDispatchSuccess, StrategyDispatchFailure>;

/// Execute a semantic strategy over the same request-scoped local executor.
/// The executor's absolute deadline is created before classification, so
/// classifier, generation, validation, judge, and every retry consume the
/// same finite budget.
pub async fn dispatch_strategy(
    exec: &GatewayLlmExec,
    prompt: &str,
    strategy_str: &str,
    tier_hint: Option<u8>,
    validator_override: ValidatorKind,
) -> StrategyDispatchResult {
    let chosen_strategy: RouteStrategy = match strategy_str {
        "auto" => {
            let (complexity, shape, format) = classify_task(exec, prompt).await;
            info!(?complexity, ?shape, ?format, "strategy classifier verdict");
            let mut strategy = pick_strategy(complexity, shape, format);
            if let RouteStrategy::Cascade {
                validator: ref mut selected,
                ..
            } = strategy
                && validator_override != ValidatorKind::None
            {
                *selected = validator_override;
            }
            strategy
        }
        "single" => RouteStrategy::SingleTier {
            tier: tier_hint.unwrap_or(2),
        },
        "cascade" => RouteStrategy::Cascade {
            tiers: vec![1, 2, 3],
            validator: validator_override,
            judge_early_exit: true,
        },
        "judge_escalate" => RouteStrategy::JudgeEscalate {
            start_tier: tier_hint.unwrap_or(2),
            max_tier: 3,
            threshold: 7,
        },
        other => {
            return Err(StrategyDispatchFailure::semantic(
                exec,
                None,
                FailureReasonCode::InvalidResponse,
                format!("unknown strategy '{other}'"),
            ));
        }
    };
    info!(strategy = ?chosen_strategy, "strategy dispatch routing locally");

    let dispatched = match chosen_strategy.clone() {
        RouteStrategy::SingleTier { tier } => exec
            .complete(tier, prompt, 4096, std::time::Duration::from_secs(600))
            .await
            .map(|output| (output, json!([]), None))
            .map_err(|error| {
                StrategyDispatchFailure::from_execution(
                    exec,
                    Some(chosen_strategy.clone()),
                    FailureReasonCode::Unavailable,
                    format!("single dispatch failed: {error}"),
                )
            }),
        RouteStrategy::Cascade {
            tiers,
            validator,
            judge_early_exit,
        } => match run_cascade(exec, prompt, &tiers, validator, judge_early_exit).await {
            Ok(outcome) => {
                let judged_steps = if outcome.early_exit_at_tier.is_some() {
                    outcome.steps.len()
                } else {
                    outcome.steps.len().saturating_sub(1)
                };
                if judge_early_exit
                    && judged_steps > 0
                    && outcome
                        .steps
                        .iter()
                        .take(judged_steps)
                        .any(|step| step.judge_score.is_none())
                {
                    Err(StrategyDispatchFailure::judge_boundary(
                        exec,
                        Some(chosen_strategy.clone()),
                        "cascade did not receive a valid independent judge score for every inter-stage gate",
                    ))
                } else if let Some(ValidationOutcome::Err(error)) =
                    outcome.steps.last().map(|step| &step.validation)
                {
                    Err(StrategyDispatchFailure::semantic(
                        exec,
                        Some(chosen_strategy.clone()),
                        FailureReasonCode::InvalidResponse,
                        format!("final cascade output failed semantic validation: {error}"),
                    ))
                } else {
                    let trace = serde_json::to_value(&outcome.steps).unwrap_or_else(|_| json!([]));
                    Ok((outcome.final_output, trace, outcome.early_exit_at_tier))
                }
            }
            Err(error) => Err(StrategyDispatchFailure::from_execution(
                exec,
                Some(chosen_strategy.clone()),
                FailureReasonCode::Unavailable,
                format!("cascade failed: {error}"),
            )),
        },
        RouteStrategy::JudgeEscalate {
            start_tier,
            max_tier,
            threshold,
        } => match run_judge_escalate(exec, prompt, start_tier, max_tier, threshold).await {
            Ok(outcome) => {
                let trace = serde_json::to_value(&outcome.steps).unwrap_or_else(|_| json!([]));
                Ok((outcome.final_output, trace, None))
            }
            Err(error) => Err(StrategyDispatchFailure::judge_boundary(
                exec,
                Some(chosen_strategy.clone()),
                format!("judge_escalate failed: {error}"),
            )),
        },
    }?;

    let (output, trace, early_exit_at_tier) = dispatched;
    if output.trim().is_empty() {
        return Err(StrategyDispatchFailure::semantic(
            exec,
            Some(chosen_strategy),
            FailureReasonCode::InvalidResponse,
            "strategy returned a blank final output",
        ));
    }
    let execution = exec.evidence();
    if execution.winner.is_none() {
        return Err(StrategyDispatchFailure::semantic(
            exec,
            Some(chosen_strategy),
            FailureReasonCode::InvalidResponse,
            "strategy produced output without an attested winning local route",
        ));
    }
    Ok(StrategyDispatchSuccess {
        output,
        strategy: chosen_strategy,
        trace,
        early_exit_at_tier,
        execution,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_strategies() {
        for strategy in ["auto", "single", "cascade", "judge_escalate"] {
            assert_eq!(parse_strategy(strategy).unwrap(), strategy);
        }
    }

    #[test]
    fn rejects_unknown_strategy() {
        let error = parse_strategy("magic").unwrap_err();
        assert!(error.contains("magic"));
        assert!(error.contains("auto"));
    }

    #[test]
    fn parses_validator_variants() {
        assert_eq!(parse_validator(Some("json")), ValidatorKind::Json);
        assert_eq!(parse_validator(Some("JSON")), ValidatorKind::Json);
        assert_eq!(parse_validator(Some("yaml")), ValidatorKind::Yaml);
        assert_eq!(parse_validator(Some("none")), ValidatorKind::None);
        assert_eq!(parse_validator(None), ValidatorKind::None);
    }
}
