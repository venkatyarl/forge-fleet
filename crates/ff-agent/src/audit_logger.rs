//! Structured Tool Audit Logger (Phase 5)
//!
//! Logs every tool call to `tool_audit_log` with:
//! - agent_id, session_id, step_id
//! - tool_name, params_json, prompt_hash
//! - outcome (success | failure | denied | timeout)
//! - duration_ms, node_name
//!
//! Designed for compliance tracing and security review.

use sqlx::PgPool;
use tracing::{debug, warn};
use uuid::Uuid;

/// Outcome of a tool execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOutcome {
    Success,
    Failure,
    Denied,
    Timeout,
}

impl ToolOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            ToolOutcome::Success => "success",
            ToolOutcome::Failure => "failure",
            ToolOutcome::Denied => "denied",
            ToolOutcome::Timeout => "timeout",
        }
    }
}

/// Log a single tool call to `tool_audit_log`.
///
/// Fire-and-forget: errors are logged but not propagated.
#[allow(clippy::too_many_arguments)]
pub async fn log_tool_call(
    pg: &PgPool,
    session_id: Option<Uuid>,
    step_id: Option<Uuid>,
    agent_id: &str,
    tool_name: &str,
    params_json: &serde_json::Value,
    prompt_hash: Option<&str>,
    outcome: ToolOutcome,
    error: Option<&str>,
    duration_ms: Option<u64>,
    node_name: &str,
) {
    let result = sqlx::query(
        r#"
        INSERT INTO tool_audit_log (
            session_id, step_id, agent_id, tool_name, params_json,
            prompt_hash, outcome, error, duration_ms, node_name
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        "#,
    )
    .bind(session_id)
    .bind(step_id)
    .bind(agent_id)
    .bind(tool_name)
    .bind(params_json)
    .bind(prompt_hash)
    .bind(outcome.as_str())
    .bind(error)
    .bind(duration_ms.map(|d| d as i64))
    .bind(node_name)
    .execute(pg)
    .await;

    match result {
        Ok(_) => debug!(tool = %tool_name, outcome = %outcome.as_str(), "tool call audited"),
        Err(e) => warn!(tool = %tool_name, error = %e, "failed to write tool audit log"),
    }
}

/// Check whether a tool is in the step's allow-list.
///
/// - `allowed_tools` is empty (`[]`) → all tools permitted.
/// - `allowed_tools` contains entries → only listed tools permitted.
pub fn tool_is_allowed(tool_name: &str, allowed_tools: &serde_json::Value) -> bool {
    let allowed = match allowed_tools.as_array() {
        Some(arr) if !arr.is_empty() => arr,
        _ => return true, // empty array = allow all
    };

    allowed
        .iter()
        .filter_map(|v| v.as_str())
        .any(|allowed_name| allowed_name.eq_ignore_ascii_case(tool_name))
}

/// Hash a prompt string for audit trail integrity.
/// Uses SHA-256 truncated to 16 hex chars (sufficient for collision resistance
/// in an audit context and keeps row size small).
pub fn hash_prompt(prompt: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(prompt.as_bytes());
    hash.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_allow_list_empty() {
        let allowed = serde_json::json!([]);
        assert!(tool_is_allowed("bash", &allowed));
        assert!(tool_is_allowed("anything", &allowed));
    }

    #[test]
    fn test_tool_allow_list_specific() {
        let allowed = serde_json::json!(["bash", "read", "grep"]);
        assert!(tool_is_allowed("bash", &allowed));
        assert!(tool_is_allowed("Bash", &allowed)); // case-insensitive
        assert!(!tool_is_allowed("rm", &allowed));
    }

    #[test]
    fn test_hash_prompt_stable() {
        let h1 = hash_prompt("hello world");
        let h2 = hash_prompt("hello world");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 16);
    }
}
