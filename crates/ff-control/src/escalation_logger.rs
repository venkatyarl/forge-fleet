//! Structured Lane-1.5 escalation and training-corpus capture.

use std::time::Duration;

use ff_db::{InteractionRecord, PgPool};
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Why Lane 1 routed a task to the local 480B Lane-1.5 model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationReason {
    Lane1Fail,
    Stall,
    Complexity,
}

impl EscalationReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lane1Fail => "lane1_fail",
            Self::Stall => "stall",
            Self::Complexity => "complexity",
        }
    }
}

/// Record a completed Lane-1.5 turn in the shared training corpus.
///
/// Capture is best-effort: an observability failure must not fail inference.
pub async fn log_escalation(
    pool: &PgPool,
    reason: EscalationReason,
    input: &str,
    output: &str,
    latency: Duration,
) {
    let record = escalation_record(reason, input, output, latency);
    if let Err(error) = ff_db::pg_record_interaction(pool, &record).await {
        warn!(%error, reason = reason.as_str(), "Lane-1.5 escalation capture failed");
    }
}

pub(crate) fn escalation_record(
    reason: EscalationReason,
    input: &str,
    output: &str,
    latency: Duration,
) -> InteractionRecord {
    InteractionRecord {
        channel: "lane-1.5-escalation".to_string(),
        request_text: input.to_string(),
        request_meta: serde_json::json!({"lane": "1.5"}),
        route_decision: serde_json::json!({
            "reason": reason,
            "target": "qwen3-coder-480b",
        }),
        engine: Some("qwen3-coder-480b".to_string()),
        response_text: output.to_string(),
        latency_ms: Some(latency.as_millis().min(i32::MAX as u128) as i32),
        outcome: "ok".to_string(),
        model_versions: serde_json::json!({"model": "qwen3-coder-480b"}),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_have_stable_training_labels() {
        assert_eq!(EscalationReason::Lane1Fail.as_str(), "lane1_fail");
        assert_eq!(EscalationReason::Stall.as_str(), "stall");
        assert_eq!(EscalationReason::Complexity.as_str(), "complexity");
    }

    #[test]
    fn record_captures_route_and_training_pair() {
        let record = escalation_record(
            EscalationReason::Stall,
            "implement the task",
            "completed output",
            Duration::from_millis(42),
        );
        assert_eq!(record.channel, "lane-1.5-escalation");
        assert_eq!(record.request_text, "implement the task");
        assert_eq!(record.response_text, "completed output");
        assert_eq!(record.route_decision["reason"], "stall");
        assert_eq!(record.route_decision["target"], "qwen3-coder-480b");
        assert_eq!(record.latency_ms, Some(42));
    }
}
