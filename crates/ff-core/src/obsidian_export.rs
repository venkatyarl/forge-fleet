//! Obsidian vault export utilities.
//!
//! Provides the LLM-distillation step that turns raw session transcripts into
//! concise, linkable Obsidian notes by calling the fleet `fleet_run` tool
//! through the local ForgeFleet MCP server.

use std::error::Error;
use std::process::Command;
use std::time::Duration;

use serde_json::{Value, json};
use tracing::{error, info, warn};

use crate::config::{FleetConfig, ObsidianExportConfig};

/// A raw session transcript ready to be distilled into an Obsidian note.
#[derive(Debug, Clone)]
pub struct SessionTranscript {
    /// Stable session identifier (used in the note frontmatter and for
    /// idempotency).
    pub session_id: String,
    /// Raw transcript / interaction log for the session.
    pub content: String,
    /// Optional machine-readable metadata (channel, participants, tags, …).
    pub metadata: Option<Value>,
}

/// Commit any pending Obsidian vault changes and push to the configured remote.
///
/// This is a synchronous, best-effort helper; callers should decide whether
/// to treat a failure here as fatal.
pub fn commit_and_push() -> Result<(), Box<dyn Error>> {
    let commit_status = Command::new("git")
        .args([
            "commit",
            "-m",
            "Auto-update by ff",
            "--author",
            "ff <ff@forgefleet>",
        ])
        .output()?;

    if !commit_status.status.success() {
        let stderr = String::from_utf8_lossy(&commit_status.stderr);
        return Err(format!("Git commit failed: {stderr}").into());
    }

    let push_status = Command::new("git").args(["push"]).output()?;

    if !push_status.status.success() {
        let stderr = String::from_utf8_lossy(&push_status.stderr);
        return Err(format!("Git push failed: {stderr}").into());
    }

    Ok(())
}

/// Distil a single session transcript into an Obsidian markdown note.
///
/// The implementation calls `fleet_run` on the local ForgeFleet MCP server. It
/// honours `[obsidian_export]` configuration:
///
/// * `enabled` — must be `true` or the call short-circuits.
/// * `model` — passed through as the `fleet_run` model selector when set.
///
/// The MCP endpoint is resolved from `[mcp.forgefleet]`; if none is configured
/// the call falls back to `http://127.0.0.1:50001/mcp`.
///
/// # Errors
///
/// Returns an error if obsidian export is disabled, the MCP endpoint is
/// unreachable, `fleet_run` returns a JSON-RPC error, or the response cannot
/// be parsed into a note string.
pub async fn distill_session_to_note(
    config: &FleetConfig,
    session: &SessionTranscript,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    if !config.obsidian_export.enabled {
        return Err("obsidian export is disabled in fleet.toml".into());
    }

    info!(
        session_id = %session.session_id,
        "distilling session transcript into Obsidian note via fleet_run"
    );

    let prompt = build_distillation_prompt(session);
    let endpoint = resolve_mcp_endpoint(config);
    let arguments = build_fleet_run_arguments(&config.obsidian_export, &prompt);

    info!(endpoint = %endpoint, "calling fleet_run for obsidian distillation");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let request = json!({
        "jsonrpc": "2.0",
        "id": format!("obsidian-export-{}", session.session_id),
        "method": "fleet_run",
        "params": arguments,
    });

    let response = client
        .post(&endpoint)
        .json(&request)
        .send()
        .await
        .map_err(|e| {
            error!(
                session_id = %session.session_id,
                endpoint = %endpoint,
                error = %e,
                "fleet_run HTTP request failed"
            );
            format!("fleet_run request to {endpoint} failed: {e}")
        })?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        error!(
            session_id = %session.session_id,
            endpoint = %endpoint,
            status = %status,
            body = %body,
            "fleet_run returned non-success HTTP status"
        );
        return Err(format!("fleet_run returned HTTP {status}: {body}").into());
    }

    let payload: Value = response.json().await.map_err(|e| {
        error!(
            session_id = %session.session_id,
            endpoint = %endpoint,
            error = %e,
            "failed to parse fleet_run JSON-RPC response"
        );
        format!("invalid JSON-RPC response from fleet_run: {e}")
    })?;

    if let Some(err) = payload.get("error") {
        error!(
            session_id = %session.session_id,
            endpoint = %endpoint,
            error = %err,
            "fleet_run returned JSON-RPC error"
        );
        return Err(format!("fleet_run returned error: {err}").into());
    }

    let result = payload.get("result").cloned().unwrap_or(Value::Null);
    let note_text = extract_text_from_fleet_run_result(&result).ok_or_else(|| {
        error!(
            session_id = %session.session_id,
            result = %result,
            "fleet_run response did not contain distillable text"
        );
        "fleet_run response did not contain distillable text".to_string()
    })?;

    if note_text.trim().is_empty() {
        warn!(
            session_id = %session.session_id,
            "fleet_run returned an empty distilled note"
        );
        return Err("fleet_run returned an empty distilled note".into());
    }

    info!(
        session_id = %session.session_id,
        note_len = note_text.len(),
        "session distillation complete"
    );

    Ok(note_text)
}

/// Build the `fleet_run` argument object from obsidian export settings.
fn build_fleet_run_arguments(obsidian: &ObsidianExportConfig, prompt: &str) -> Value {
    let mut args = json!({
        "prompt": prompt,
        "strategy": "auto",
    });

    if let Some(model) = &obsidian.model {
        args["model"] = json!(model);
    }

    args
}

/// Build the distillation prompt for `fleet_run`.
fn build_distillation_prompt(session: &SessionTranscript) -> String {
    let metadata = session
        .metadata
        .as_ref()
        .map(Value::to_string)
        .unwrap_or_else(|| "{}".to_string());

    format!(
        "Distil the following session transcript into a concise, well-structured \
         Obsidian markdown note.\n\n\
         Capture the key decisions, actions taken, important findings, open \
         questions, and any blockers. Use Obsidian-style [[wikilinks]] for \
         concepts that connect to other notes. Return ONLY the markdown note \
         content, including YAML frontmatter with session_id and tags. Do not \
         include commentary outside the note.\n\n\
         Session ID: {}\n\
         Metadata: {}\n\n\
         Transcript:\n---\n{}\n---",
        session.session_id, metadata, session.content
    )
}

/// Resolve the local ForgeFleet MCP endpoint.
///
/// Prefers `[mcp.forgefleet].endpoint`, then `[mcp.forgefleet].port`, then the
/// conventional default.
fn resolve_mcp_endpoint(config: &FleetConfig) -> String {
    if let Some(cfg) = config.mcp.get("forgefleet") {
        if let Some(endpoint) = cfg.endpoint.as_ref().filter(|s| !s.trim().is_empty()) {
            return normalize_mcp_endpoint(endpoint);
        }
        if let Some(port) = cfg.port {
            return format!("http://127.0.0.1:{port}/mcp");
        }
    }
    "http://127.0.0.1:50001/mcp".to_string()
}

/// Normalize a raw MCP endpoint string, ensuring it has a scheme and `/mcp` path.
fn normalize_mcp_endpoint(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    };

    // Light-weight path normalization without pulling in the `url` crate.
    if with_scheme.ends_with("/mcp") {
        with_scheme
    } else {
        let base = with_scheme.trim_end_matches('/');
        format!("{base}/mcp")
    }
}

/// Extract readable text from a `fleet_run` JSON-RPC result.
///
/// Handles both direct method results and the `tools/call` wrapper shape.
fn extract_text_from_fleet_run_result(result: &Value) -> Option<String> {
    // Direct string result.
    if let Some(text) = result.as_str() {
        return Some(text.to_string());
    }

    // tools/call wrapper: { content: [{ type: "text", text: "..." }] }
    if let Some(text) = result.pointer("/content/0/text").and_then(Value::as_str) {
        // The wrapper text may itself be a JSON-serialized Value.
        if let Ok(parsed) = serde_json::from_str::<Value>(text) {
            if let Some(inner) = extract_text_from_fleet_run_result(&parsed) {
                return Some(inner);
            }
        }
        return Some(text.to_string());
    }

    // Common object shapes.
    if let Some(text) = result.get("text").and_then(Value::as_str) {
        return Some(text.to_string());
    }
    if let Some(text) = result.get("stdout").and_then(Value::as_str) {
        return Some(text.to_string());
    }
    if let Some(text) = result
        .get("output")
        .or_else(|| result.get("response"))
        .and_then(Value::as_str)
    {
        return Some(text.to_string());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_mcp_endpoint_adds_scheme_and_path() {
        assert_eq!(
            normalize_mcp_endpoint("127.0.0.1:50001"),
            "http://127.0.0.1:50001/mcp"
        );
        assert_eq!(
            normalize_mcp_endpoint("http://127.0.0.1:50001/mcp"),
            "http://127.0.0.1:50001/mcp"
        );
        assert_eq!(
            normalize_mcp_endpoint("https://mcp.internal/mcp"),
            "https://mcp.internal/mcp"
        );
    }

    #[test]
    fn extract_text_from_direct_string_result() {
        let result = Value::String("# Note\nBody".to_string());
        assert_eq!(
            extract_text_from_fleet_run_result(&result),
            Some("# Note\nBody".to_string())
        );
    }

    #[test]
    fn extract_text_from_tools_call_wrapper() {
        let result = json!({
            "content": [{ "type": "text", "text": "# Distilled\n- a\n- b" }]
        });
        assert_eq!(
            extract_text_from_fleet_run_result(&result),
            Some("# Distilled\n- a\n- b".to_string())
        );
    }

    #[test]
    fn extract_text_from_output_object() {
        let result = json!({ "output": " concise note " });
        assert_eq!(
            extract_text_from_fleet_run_result(&result).unwrap(),
            " concise note "
        );
    }

    #[test]
    fn resolve_mcp_endpoint_prefers_config_endpoint() {
        let mut config = FleetConfig::default();
        assert_eq!(resolve_mcp_endpoint(&config), "http://127.0.0.1:50001/mcp");

        config.mcp.insert(
            "forgefleet".to_string(),
            crate::config::McpConfig {
                endpoint: Some("http://mcp.example.com/custom".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(
            resolve_mcp_endpoint(&config),
            "http://mcp.example.com/custom/mcp"
        );
    }

    #[test]
    fn distill_session_disabled_returns_error() {
        let config = FleetConfig::default();
        let session = SessionTranscript {
            session_id: "s-1".to_string(),
            content: "hello".to_string(),
            metadata: None,
        };

        // Runtime block is not needed: the function checks `enabled` before any
        // async work, but the signature is async so we must await.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let err = rt
            .block_on(distill_session_to_note(&config, &session))
            .unwrap_err();
        assert!(err.to_string().contains("disabled"));
    }
}
