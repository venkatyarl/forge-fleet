//! MlxDegraded tool — health-check an MLX (Apple Silicon) inference endpoint
//! and derive throughput metrics.
//!
//! Wraps [`ff_pulse::mlx_adapter::MlxDegradedAdapter`] so the agent loop can
//! ask "is this MLX endpoint degraded?" directly.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{AgentTool, AgentToolContext, AgentToolResult};

pub struct MlxDegradedTool;

#[async_trait]
impl AgentTool for MlxDegradedTool {
    fn name(&self) -> &str {
        "MlxDegraded"
    }

    fn description(&self) -> &str {
        "Health-check an MLX (Apple Silicon) inference endpoint and derive throughput metrics. \
         Returns healthy/degraded/unhealthy status plus tokens/sec and queue depth."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "endpoint": {
                    "type": "string",
                    "description": "MLX server base URL (default: http://127.0.0.1:55000)"
                },
                "throughput_threshold": {
                    "type": "number",
                    "description": "Tokens/sec below which the endpoint is considered degraded (default: 5.0)"
                },
                "queue_depth_threshold": {
                    "type": "number",
                    "description": "Queue depth above which the endpoint is considered degraded (default: 8)"
                },
                "response_time_ms_threshold": {
                    "type": "number",
                    "description": "Health-ping RTT in ms above which the endpoint is degraded (default: 5000)"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let endpoint = input
            .get("endpoint")
            .and_then(Value::as_str)
            .unwrap_or("http://127.0.0.1:55000")
            .to_string();
        let throughput_threshold = input
            .get("throughput_threshold")
            .and_then(Value::as_f64)
            .unwrap_or(5.0);
        let queue_depth_threshold = input
            .get("queue_depth_threshold")
            .and_then(Value::as_i64)
            .unwrap_or(8) as i32;
        let response_time_ms_threshold = input
            .get("response_time_ms_threshold")
            .and_then(Value::as_u64)
            .unwrap_or(5_000);

        let adapter = ff_pulse::mlx_adapter::MlxDegradedAdapter::new().with_thresholds(
            throughput_threshold,
            queue_depth_threshold,
            response_time_ms_threshold,
        );

        match adapter.check_endpoint(&endpoint).await {
            Ok(state) => {
                let payload = json!({
                    "endpoint": state.endpoint,
                    "status": state.health_status.as_str(),
                    "degraded": state.degraded,
                    "tokens_per_sec": state.tokens_per_sec,
                    "queue_depth": state.queue_depth,
                    "response_time_ms": state.response_time_ms,
                });
                AgentToolResult::ok(payload.to_string())
            }
            Err(e) => AgentToolResult::err(format!("MLX degraded check failed: {e}")),
        }
    }
}
