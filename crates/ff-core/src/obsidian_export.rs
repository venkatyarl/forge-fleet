//! Obsidian vault export utilities.
//!
//! Provides the LLM-distillation step that turns raw session transcripts into
//! concise, linkable Obsidian notes by calling the fleet `fleet_run` tool
//! through the local ForgeFleet MCP server, plus a small daemon tick that
//! polls `ff_interactions` for new rows, groups them by session, and writes
//! one basic-memory markdown note per session.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use tracing::{error, info, warn};

use crate::config::{FleetConfig, ObsidianExportConfig};
use crate::schema::basic_memory::BasicMemoryFrontmatter;

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

/// One `ff_interactions` row used by the export daemon.
#[derive(Debug, Clone)]
struct InteractionRow {
    id: String,
    ts: DateTime<Utc>,
    session_id: Option<String>,
    request_text: String,
    response_text: String,
    engine: Option<String>,
    tokens_in: i32,
    tokens_out: i32,
    outcome: String,
    steps: Value,
}

/// Rows for a single session, in chronological order.
#[derive(Debug, Clone)]
struct Session {
    key: String,
    rows: Vec<InteractionRow>,
}

impl Session {
    /// Build the transcript object passed to the distillation LLM.
    fn to_transcript(&self) -> SessionTranscript {
        let mut content = format!("# Session {}\n\n", self.key);
        let mut engines = Vec::new();
        let mut total_tokens = 0u64;

        for (i, row) in self.rows.iter().enumerate() {
            content.push_str(&format!("## Turn {}\n\n", i + 1));
            if let Some(engine) = &row.engine {
                content.push_str(&format!("**Engine:** {engine}\n\n"));
                engines.push(engine.clone());
            }
            content.push_str(&format!("**Outcome:** {}\n\n", row.outcome));
            if !row.request_text.trim().is_empty() {
                content.push_str("**Request:**\n");
                content.push_str(row.request_text.trim());
                content.push_str("\n\n");
            }
            if !row.response_text.trim().is_empty() {
                content.push_str("**Response:**\n");
                content.push_str(row.response_text.trim());
                content.push_str("\n\n");
            }
            total_tokens +=
                (row.tokens_in.max(0) as u64).saturating_add(row.tokens_out.max(0) as u64);
        }

        let metadata = json!({
            "session_id": self.key,
            "turns": self.rows.len(),
            "engines": engines,
            "total_tokens": total_tokens,
        });

        SessionTranscript {
            session_id: self.key.clone(),
            content,
            metadata: Some(metadata),
        }
    }

    /// Aggregate row metadata into a basic-memory frontmatter block.
    fn to_frontmatter(&self, project_id: &str, title: &str) -> BasicMemoryFrontmatter {
        let first_ts = self.rows.first().map(|r| r.ts).unwrap_or_else(Utc::now);
        let last_ts = self.rows.last().map(|r| r.ts).unwrap_or_else(Utc::now);
        let total_tokens: u64 = self
            .rows
            .iter()
            .map(|r| (r.tokens_in.max(0) as u64).saturating_add(r.tokens_out.max(0) as u64))
            .sum();

        let model = most_common(self.rows.iter().filter_map(|r| r.engine.as_deref()))
            .unwrap_or("unknown")
            .to_string();

        let mut tools = HashSet::new();
        for row in &self.rows {
            if let Some(arr) = row.steps.as_array() {
                for step in arr {
                    if step.get("type").and_then(Value::as_str) == Some("tool") {
                        if let Some(name) = step.get("name").and_then(Value::as_str) {
                            tools.insert(name.to_string());
                        }
                    }
                }
            }
        }
        let mut tools: Vec<String> = tools.into_iter().collect();
        tools.sort();

        BasicMemoryFrontmatter {
            title: title.to_string(),
            date: first_ts.to_rfc3339_opts(SecondsFormat::Secs, true),
            project: project_id.to_string(),
            model,
            tokens: total_tokens,
            tools,
            files_touched: Vec::new(),
            memory_type: "session".to_string(),
            realm: "session".to_string(),
            last_updated: last_ts.to_rfc3339_opts(SecondsFormat::Secs, true),
        }
    }
}

/// Cursor persisted in the target vault so the daemon only processes new
/// `ff_interactions` rows across restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExportCursor {
    last_ts: DateTime<Utc>,
    last_id: String,
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

/// Run one daemon tick: query new `ff_interactions` rows for `project_id`,
/// distill each affected session through `fleet_run`, and write a basic-memory
/// markdown note to `config.obsidian_export.target_dir`.
///
/// Progress is persisted in a small JSON cursor file inside the target
/// directory (`.ff_obsidian_export_cursor.json`) so ticks are idempotent.
///
/// Returns the number of session notes written.
pub async fn process_new_sessions(
    config: &FleetConfig,
    pg: &PgPool,
    project_id: &str,
) -> Result<usize, Box<dyn Error + Send + Sync>> {
    if !config.obsidian_export.enabled {
        return Ok(0);
    }

    let target_dir = config
        .obsidian_export
        .target_dir
        .as_deref()
        .ok_or("obsidian_export.target_dir is not configured")?;
    let target_path = Path::new(target_dir);
    std::fs::create_dir_all(target_path)?;

    let cursor = read_cursor(target_path).unwrap_or(ExportCursor {
        last_ts: DateTime::<Utc>::UNIX_EPOCH,
        last_id: String::new(),
    });

    let limit = 1000i64;
    let rows = fetch_unexported_rows(pg, project_id, &cursor, limit)
        .await
        .map_err(|e| e.to_string())?;
    if rows.is_empty() {
        info!(project_id = %project_id, "Obsidian export: no new sessions");
        return Ok(0);
    }

    let sessions = group_rows_into_sessions(rows);
    info!(
        project_id = %project_id,
        sessions = sessions.len(),
        "Obsidian export: distilling new sessions"
    );

    let mut exported = 0usize;
    let mut next_cursor = cursor.clone();

    for session in &sessions {
        let transcript = session.to_transcript();

        let (title, body) = match distill_session_to_note(config, &transcript).await {
            Ok(raw) => normalize_distilled_output(&raw, &session.key),
            Err(e) => {
                warn!(
                    session_id = %session.key,
                    error = %e,
                    "session distillation failed; skipping session"
                );
                continue;
            }
        };

        let frontmatter = session.to_frontmatter(project_id, &title);
        let note = frontmatter.to_note(&body);
        let path = write_session_note(target_path, &session.key, &note)?;
        exported += 1;

        info!(
            session_id = %session.key,
            path = %path.display(),
            "Obsidian export: wrote session note"
        );
    }

    // Advance the cursor to the highest row processed in this batch, even if
    // individual sessions failed.  This keeps a single stuck session from
    // blocking the whole queue.
    for session in &sessions {
        for row in &session.rows {
            if row.ts > next_cursor.last_ts
                || (row.ts == next_cursor.last_ts && row.id > next_cursor.last_id)
            {
                next_cursor.last_ts = row.ts;
                next_cursor.last_id = row.id.clone();
            }
        }
    }
    write_cursor(target_path, &next_cursor)?;

    Ok(exported)
}

/// Long-running daemon wrapper.  Ticks every 60 seconds until the shutdown
/// watch receiver becomes `true`.
pub async fn run_export_daemon(
    config: FleetConfig,
    pg: PgPool,
    project_id: String,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                if let Err(e) = process_new_sessions(&config, &pg, &project_id).await {
                    error!(error = %e, "Obsidian export daemon tick failed");
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("Obsidian export daemon shutting down");
                    break;
                }
            }
        }
    }

    Ok(())
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

/// True when the live `ff_interactions` table has a dedicated `project_id`
/// column; otherwise project filtering falls back to `request_meta->>'project_id'`.
async fn ff_interactions_has_project_id(pg: &PgPool) -> anyhow::Result<bool> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
              FROM information_schema.columns
             WHERE table_schema = current_schema()
               AND table_name = 'ff_interactions'
               AND column_name = 'project_id'
        )",
    )
    .fetch_one(pg)
    .await
    .context("check ff_interactions.project_id column")
}

/// Fetch rows for sessions that have at least one interaction newer than the
/// persisted cursor, including older rows in those sessions so the distilled
/// note is complete.
async fn fetch_unexported_rows(
    pg: &PgPool,
    project_id: &str,
    cursor: &ExportCursor,
    limit: i64,
) -> anyhow::Result<Vec<InteractionRow>> {
    let has_project_col = ff_interactions_has_project_id(pg).await?;

    let sql = if has_project_col {
        r#"
        WITH new_sessions AS (
            SELECT DISTINCT COALESCE(session_id, id) AS sid
              FROM ff_interactions
             WHERE project_id = $1
               AND (ts > $2 OR (ts = $2 AND id::text > $3))
        )
        SELECT id::text AS id,
               ts,
               session_id::text AS session_id,
               request_text,
               response_text,
               engine,
               tokens_in,
               tokens_out,
               outcome,
               steps
          FROM ff_interactions
         WHERE COALESCE(session_id, id) IN (SELECT sid FROM new_sessions)
         ORDER BY COALESCE(session_id, id), ts ASC
         LIMIT $4
        "#
    } else {
        r#"
        WITH new_sessions AS (
            SELECT DISTINCT COALESCE(session_id, id) AS sid
              FROM ff_interactions
             WHERE request_meta->>'project_id' = $1
               AND (ts > $2 OR (ts = $2 AND id::text > $3))
        )
        SELECT id::text AS id,
               ts,
               session_id::text AS session_id,
               request_text,
               response_text,
               engine,
               tokens_in,
               tokens_out,
               outcome,
               steps
          FROM ff_interactions
         WHERE COALESCE(session_id, id) IN (SELECT sid FROM new_sessions)
         ORDER BY COALESCE(session_id, id), ts ASC
         LIMIT $4
        "#
    };

    let rows = sqlx::query(sql)
        .bind(project_id)
        .bind(cursor.last_ts)
        .bind(&cursor.last_id)
        .bind(limit)
        .fetch_all(pg)
        .await
        .context("fetch unexported ff_interactions rows")?;

    Ok(rows
        .into_iter()
        .map(|r| InteractionRow {
            id: r.get("id"),
            ts: r.get("ts"),
            session_id: r.get("session_id"),
            request_text: r.get("request_text"),
            response_text: r.get("response_text"),
            engine: r.get("engine"),
            tokens_in: r.get("tokens_in"),
            tokens_out: r.get("tokens_out"),
            outcome: r.get("outcome"),
            steps: r.get("steps"),
        })
        .collect())
}

/// Group a sorted stream of interaction rows into sessions.
fn group_rows_into_sessions(rows: Vec<InteractionRow>) -> Vec<Session> {
    let mut sessions: Vec<Session> = Vec::new();
    let mut current_key: Option<String> = None;

    for row in rows {
        let key = row.session_id.clone().unwrap_or_else(|| row.id.clone());
        if current_key.as_ref() == Some(&key) {
            sessions.last_mut().expect("current_key set").rows.push(row);
        } else {
            sessions.push(Session {
                key: key.clone(),
                rows: vec![row],
            });
            current_key = Some(key);
        }
    }

    sessions
}

/// Path to the persisted cursor inside the target vault.
fn cursor_path(target_dir: &Path) -> PathBuf {
    target_dir.join(".ff_obsidian_export_cursor.json")
}

/// Read the persisted cursor, if any.
fn read_cursor(target_dir: &Path) -> Option<ExportCursor> {
    let data = std::fs::read_to_string(cursor_path(target_dir)).ok()?;
    serde_json::from_str(&data).ok()
}

/// Persist the cursor so the next tick resumes from the correct row.
fn write_cursor(
    target_dir: &Path,
    cursor: &ExportCursor,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    std::fs::create_dir_all(target_dir)?;
    let data = serde_json::to_string_pretty(cursor)?;
    std::fs::write(cursor_path(target_dir), data)?;
    Ok(())
}

/// Write a markdown note for `session_id` into the target directory.
fn write_session_note(
    target_dir: &Path,
    session_id: &str,
    note: &str,
) -> Result<PathBuf, Box<dyn Error + Send + Sync>> {
    let filename = format!("{}.md", slugify(session_id));
    let path = target_dir.join(filename);
    std::fs::create_dir_all(target_dir)?;
    std::fs::write(&path, note)?;
    Ok(path)
}

/// Convert a candidate note into a clean body, then pick a title.
///
/// If the LLM already emitted a basic-memory frontmatter block, it is stripped
/// so the daemon can emit its own canonical frontmatter.  The title prefers
/// the first level-1 heading in the body, then falls back to the session key.
fn normalize_distilled_output(raw: &str, session_key: &str) -> (String, String) {
    let body = if let Some((_, body)) = BasicMemoryFrontmatter::parse(raw) {
        body
    } else {
        raw.to_string()
    };

    let title = extract_title(&body).unwrap_or_else(|| format!("Session {session_key}"));

    (title, body)
}

/// Extract a title from the first `# Heading` line, if present.
fn extract_title(body: &str) -> Option<String> {
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("# ") {
            let title = rest.trim().to_string();
            if !title.is_empty() {
                return Some(title);
            }
        }
    }
    None
}

/// Replace non-filename characters with `-` and lowercase the result.
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Return the most common string in the iterator, breaking ties by first
/// appearance.
fn most_common<'a>(items: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut first_seen: Vec<&str> = Vec::new();
    for item in items {
        if counts.insert(item, 1).is_none() {
            first_seen.push(item);
        } else {
            *counts.get_mut(item).expect("present") += 1;
        }
    }
    let mut best: Option<&str> = None;
    let mut best_count = 0usize;
    for item in first_seen {
        let count = counts[item];
        if count > best_count {
            best = Some(item);
            best_count = count;
        }
    }
    best
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

    #[test]
    fn group_rows_into_sessions_groups_by_session_or_id() {
        let base_time = DateTime::<Utc>::UNIX_EPOCH;
        let rows = vec![
            InteractionRow {
                id: "a1".to_string(),
                ts: base_time,
                session_id: Some("s1".to_string()),
                request_text: "hi".to_string(),
                response_text: "hello".to_string(),
                engine: Some("claude".to_string()),
                tokens_in: 10,
                tokens_out: 5,
                outcome: "ok".to_string(),
                steps: json!([]),
            },
            InteractionRow {
                id: "a2".to_string(),
                ts: base_time,
                session_id: Some("s1".to_string()),
                request_text: "again".to_string(),
                response_text: "yes".to_string(),
                engine: Some("claude".to_string()),
                tokens_in: 3,
                tokens_out: 2,
                outcome: "ok".to_string(),
                steps: json!([]),
            },
            InteractionRow {
                id: "b1".to_string(),
                ts: base_time,
                session_id: None,
                request_text: "solo".to_string(),
                response_text: "ok".to_string(),
                engine: None,
                tokens_in: 1,
                tokens_out: 1,
                outcome: "ok".to_string(),
                steps: json!([]),
            },
        ];

        let sessions = group_rows_into_sessions(rows);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].key, "s1");
        assert_eq!(sessions[0].rows.len(), 2);
        assert_eq!(sessions[1].key, "b1");
        assert_eq!(sessions[1].rows.len(), 1);
    }

    #[test]
    fn session_frontmatter_aggregates_rows() {
        let base_time = DateTime::<Utc>::UNIX_EPOCH;
        let session = Session {
            key: "s1".to_string(),
            rows: vec![
                InteractionRow {
                    id: "a1".to_string(),
                    ts: base_time,
                    session_id: Some("s1".to_string()),
                    request_text: "a".to_string(),
                    response_text: "b".to_string(),
                    engine: Some("claude".to_string()),
                    tokens_in: 10,
                    tokens_out: 5,
                    outcome: "ok".to_string(),
                    steps: json!([
                        {"type": "tool", "name": "Bash"},
                        {"type": "tool", "name": "Edit"},
                    ]),
                },
                InteractionRow {
                    id: "a2".to_string(),
                    ts: base_time + chrono::Duration::seconds(1),
                    session_id: Some("s1".to_string()),
                    request_text: "c".to_string(),
                    response_text: "d".to_string(),
                    engine: Some("claude".to_string()),
                    tokens_in: 2,
                    tokens_out: 8,
                    outcome: "ok".to_string(),
                    steps: json!([
                        {"type": "tool", "name": "Edit"},
                    ]),
                },
            ],
        };

        let fm = session.to_frontmatter("forge-fleet", "Test Session");
        assert_eq!(fm.title, "Test Session");
        assert_eq!(fm.project, "forge-fleet");
        assert_eq!(fm.memory_type, "session");
        assert_eq!(fm.realm, "session");
        assert_eq!(fm.tokens, 25);
        assert_eq!(fm.model, "claude");
        assert_eq!(fm.tools, vec!["Bash", "Edit"]);
        assert!(fm.date < fm.last_updated);
    }

    #[test]
    fn normalize_distilled_output_strips_frontmatter_and_extracts_title() {
        let raw = "---\ntitle: Old\n---\n# Real Title\nBody";
        let (title, body) = normalize_distilled_output(raw, "fallback");
        assert_eq!(title, "Real Title");
        assert!(body.contains("Body"));
        assert!(!body.contains("---"));
    }

    #[test]
    fn normalize_distilled_output_falls_back_to_session_key() {
        let raw = "No heading here.";
        let (title, body) = normalize_distilled_output(raw, "abc");
        assert_eq!(title, "Session abc");
        assert_eq!(body, "No heading here.");
    }

    #[test]
    fn slugify_cleans_filenames() {
        assert_eq!(slugify("Hello World!"), "hello-world");
        assert_eq!(slugify("a::b__c"), "a-b__c");
    }

    #[test]
    fn most_common_breaks_ties_by_first_seen() {
        assert_eq!(most_common(["a", "b", "a", "b"].into_iter()), Some("a"));
        assert_eq!(most_common(["x"].into_iter()), Some("x"));
        assert_eq!(most_common(std::iter::empty()), None);
    }
}
