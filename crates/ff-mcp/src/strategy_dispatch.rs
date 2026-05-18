//! Shared strategy-dispatch helper for `fleet_run` and `fleet_cascade`.
//!
//! Wraps the (classify → pick_strategy → run_*) flow so both MCP handlers
//! route through the same code path when the caller opts into cascade-aware
//! dispatch. Before Path 3 this logic lived inline in `fleet_cascade` and
//! `fleet_run` couldn't reach it.

use std::time::Duration;

use ff_orchestrator::cascade_strategy::{
    LlmExec, RouteStrategy, ValidatorKind, classify_task, pick_strategy, run_cascade,
    run_judge_escalate,
};
use serde_json::{Value, json};
use tracing::info;

use crate::llm_exec::GatewayLlmExec;

/// Parse `strategy` string from MCP params. Returns the canonical form or
/// errors with the operator-facing help.
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

/// Parse `validator` string from MCP params; empty/None → `ValidatorKind::None`.
pub fn parse_validator(raw: Option<&str>) -> ValidatorKind {
    match raw.map(|s| s.to_lowercase()) {
        Some(s) if s == "json" => ValidatorKind::Json,
        Some(s) if s == "yaml" => ValidatorKind::Yaml,
        _ => ValidatorKind::None,
    }
}

/// Run a strategy-dispatched LLM call. Returns the same JSON shape that
/// `fleet_cascade` historically returned so callers don't have to special-
/// case the response.
///
/// `strategy_str`:
///   - `"auto"` — classify_task picks SingleTier | Cascade | JudgeEscalate
///   - `"single"` — `SingleTier { tier: tier_hint or 2 }`
///   - `"cascade"` — `Cascade { tiers: [1,2,3], validator, judge_early_exit: true }`
///   - `"judge_escalate"` — `JudgeEscalate { start: tier_hint or 2, max: 3, threshold: 7 }`
pub async fn dispatch_strategy(
    exec: &GatewayLlmExec,
    prompt: &str,
    strategy_str: &str,
    tier_hint: Option<u8>,
    validator_override: ValidatorKind,
) -> Result<Value, String> {
    let chosen_strategy: RouteStrategy = match strategy_str {
        "auto" => {
            // Classifier returns (complexity, shape, format) so the validator
            // is auto-picked from format. Operators can still override via
            // the validator param when they know better.
            let (c, s, f) = classify_task(exec, prompt).await;
            info!(
                complexity = ?c,
                shape = ?s,
                format = ?f,
                "dispatch_strategy: classifier verdict"
            );
            let mut strat = pick_strategy(c, s, f);
            if let RouteStrategy::Cascade {
                validator: ref mut v_ref,
                ..
            } = strat
                && validator_override != ValidatorKind::None
            {
                *v_ref = validator_override;
            }
            strat
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
        other => return Err(format!("unknown strategy '{other}'")),
    };

    info!(strategy = ?chosen_strategy, "dispatch_strategy: routing");

    let result: Value = match chosen_strategy.clone() {
        RouteStrategy::SingleTier { tier } => {
            let out = exec
                .complete(tier, prompt, 4096, Duration::from_secs(600))
                .await
                .map_err(|e| format!("single dispatch failed: {e}"))?;
            json!({
                "output": out,
                "strategy": chosen_strategy,
                "trace": [],
            })
        }
        RouteStrategy::Cascade {
            tiers,
            validator,
            judge_early_exit,
        } => {
            let outcome = run_cascade(exec, prompt, &tiers, validator, judge_early_exit)
                .await
                .map_err(|e| format!("cascade failed: {e}"))?;
            json!({
                "output": outcome.final_output,
                "strategy": chosen_strategy,
                "trace": outcome.steps,
                "early_exit_at_tier": outcome.early_exit_at_tier,
            })
        }
        RouteStrategy::JudgeEscalate {
            start_tier,
            max_tier,
            threshold,
        } => {
            let outcome = run_judge_escalate(exec, prompt, start_tier, max_tier, threshold)
                .await
                .map_err(|e| format!("judge_escalate failed: {e}"))?;
            json!({
                "output": outcome.final_output,
                "strategy": chosen_strategy,
                "trace": outcome.steps,
            })
        }
    };

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_strategies() {
        assert!(parse_strategy("auto").is_ok());
        assert!(parse_strategy("single").is_ok());
        assert!(parse_strategy("cascade").is_ok());
        assert!(parse_strategy("judge_escalate").is_ok());
    }

    #[test]
    fn rejects_unknown_strategy() {
        let err = parse_strategy("magic").unwrap_err();
        assert!(err.contains("magic"));
        assert!(err.contains("auto"));
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
