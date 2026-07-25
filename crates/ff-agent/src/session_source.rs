//! Fallback transcript adapters for unattached and historical sessions.
//!
//! First-party clients should push [`ff_db::FleetEpisode`] rows in real time.
//! These adapters deliberately overlap that path; the canonical unique key
//! makes replay harmless.

use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{PgPool, Row};

use ff_db::FleetEpisode;

#[derive(Debug, Default, Clone, Copy)]
pub struct CollectionSummary {
    pub streams: usize,
    pub inserted: usize,
}

/// Redaction-at-source entrypoint for first-party push clients.
pub fn redact_episode_content(content: &str) -> String {
    crate::session_export::redact(content)
}

#[async_trait]
pub trait SessionSourceAdapter: Send + Sync {
    fn source_kind(&self) -> &'static str;
    fn locate(&self, home: &Path) -> Result<Vec<PathBuf>>;
    fn parse(&self, path: &Path, node: &str, after_seq: i32)
    -> Result<(String, Vec<FleetEpisode>)>;

    async fn collect(&self, pool: &PgPool, home: &Path, node: &str) -> Result<CollectionSummary> {
        let mut summary = CollectionSummary::default();
        for path in self.locate(home)? {
            let stream_id = path.to_string_lossy().into_owned();
            let after_seq: i32 = sqlx::query_scalar(
                "SELECT seq FROM fleet_episode_watermarks
                  WHERE source_kind = $1 AND node = $2 AND stream_id = $3",
            )
            .bind(self.source_kind())
            .bind(node)
            .bind(&stream_id)
            .fetch_optional(pool)
            .await?
            .unwrap_or(-1);
            let (session_id, episodes) = self.parse(&path, node, after_seq)?;
            if episodes.is_empty() {
                continue;
            }
            summary.streams += 1;
            summary.inserted += ff_db::pg_append_episode_batch(
                pool,
                self.source_kind(),
                node,
                &stream_id,
                &session_id,
                &episodes,
            )
            .await?;
        }
        Ok(summary)
    }
}

struct JsonlAdapter {
    source_kind: &'static str,
    roots: &'static [&'static str],
}

#[async_trait]
impl SessionSourceAdapter for JsonlAdapter {
    fn source_kind(&self) -> &'static str {
        self.source_kind
    }

    fn locate(&self, home: &Path) -> Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        for root in self.roots {
            collect_jsonl(&home.join(root), &mut paths)?;
        }
        paths.sort();
        Ok(paths)
    }

    fn parse(
        &self,
        path: &Path,
        node: &str,
        after_seq: i32,
    ) -> Result<(String, Vec<FleetEpisode>)> {
        let fallback_session = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("unknown")
            .to_owned();
        let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
        let mut session_id = fallback_session;
        let mut episodes = Vec::new();
        for (line_number, line) in BufReader::new(file).lines().enumerate() {
            let seq = i32::try_from(line_number).unwrap_or(i32::MAX);
            if seq <= after_seq {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(&line?) else {
                continue;
            };
            if is_private_or_system(&value) {
                continue;
            }
            if let Some(id) = session_id_from(&value) {
                session_id = id;
            }
            let Some((role, content)) = message_from(&value) else {
                continue;
            };
            let content = crate::session_export::redact(&content);
            if content.trim().is_empty() {
                continue;
            }
            let mut episode =
                FleetEpisode::new(self.source_kind, node, &session_id, seq, role, content);
            episode.ts = timestamp_from(&value).unwrap_or_else(Utc::now);
            episode.model = model_from(&value);
            episodes.push(episode);
        }
        Ok((session_id, episodes))
    }
}

fn collect_jsonl(root: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root).with_context(|| format!("read {}", root.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            collect_jsonl(&path, paths)?;
        } else if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
            paths.push(path);
        }
    }
    Ok(())
}

fn session_id_from(value: &Value) -> Option<String> {
    [
        "/sessionId",
        "/session_id",
        "/payload/session_id",
        "/payload/id",
    ]
    .into_iter()
    .find_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
    .filter(|id| !id.is_empty())
    .map(str::to_owned)
}

fn timestamp_from(value: &Value) -> Option<DateTime<Utc>> {
    ["/timestamp", "/created_at", "/payload/timestamp"]
        .into_iter()
        .find_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn model_from(value: &Value) -> Option<String> {
    ["/model", "/message/model", "/payload/model"]
        .into_iter()
        .find_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
        .map(str::to_owned)
}

fn message_from(value: &Value) -> Option<(String, String)> {
    let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
    let payload = value
        .get("message")
        .or_else(|| value.get("payload"))
        .unwrap_or(value);
    let role = payload
        .get("role")
        .and_then(Value::as_str)
        .or_else(|| match kind {
            "user" => Some("user"),
            "assistant" | "response_item" => Some("assistant"),
            _ => None,
        })?;
    let content = payload
        .get("content")
        .or_else(|| payload.get("text"))
        .or_else(|| value.pointer("/payload/message/content"))?;
    let mut text = String::new();
    flatten_text(content, &mut text);
    (!text.trim().is_empty()).then(|| (role.to_owned(), text))
}

fn flatten_text(value: &Value, text: &mut String) {
    match value {
        Value::String(value) => text.push_str(value),
        Value::Array(values) => {
            for value in values {
                flatten_text(value, text);
            }
        }
        Value::Object(map) => {
            let kind = map.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(
                kind,
                "thinking" | "reasoning" | "tool_use" | "function_call"
            ) {
                return;
            }
            if let Some(value) = map
                .get("text")
                .or_else(|| map.get("content"))
                .or_else(|| map.get("output_text"))
            {
                flatten_text(value, text);
            }
        }
        _ => {}
    }
}

fn is_private_or_system(value: &Value) -> bool {
    let serialized = value.to_string();
    value.get("type").and_then(Value::as_str) == Some("system")
        || serialized.contains("<system-reminder>")
        || serialized.contains("\"type\":\"thinking\"")
        || serialized.contains("\"type\":\"reasoning\"")
}

async fn collect_interactions(pool: &PgPool, node: &str) -> Result<CollectionSummary> {
    let after_seq: i32 = sqlx::query_scalar(
        "SELECT seq FROM fleet_episode_watermarks
          WHERE source_kind = 'ff_interaction' AND node = $1 AND stream_id = 'ff_interactions'",
    )
    .bind(node)
    .fetch_optional(pool)
    .await?
    .unwrap_or(-1);
    let rows = sqlx::query(
        "WITH ordered AS (
            SELECT *, (ROW_NUMBER() OVER (ORDER BY ts, id) - 1)::int AS source_seq
              FROM ff_interactions
             WHERE COALESCE(worker_name, $1) = $1
         )
         SELECT id, session_id, ts, request_text, response_text, engine,
                tokens_in, tokens_out, work_item_id, purpose, source_seq
           FROM ordered WHERE source_seq > $2
          ORDER BY source_seq LIMIT 200",
    )
    .bind(node)
    .bind(after_seq / 2)
    .fetch_all(pool)
    .await?;
    let mut episodes = Vec::new();
    for row in rows {
        let source_seq: i32 = row.try_get("source_seq")?;
        let session_id = row
            .try_get::<Option<uuid::Uuid>, _>("session_id")?
            .unwrap_or_else(|| row.get("id"))
            .to_string();
        let purpose: Option<String> = row.try_get("purpose")?;
        let source_kind = match purpose.as_deref() {
            Some("research") => "research",
            Some("council") => "council",
            _ => "ff_interaction",
        };
        for (offset, role, content, tokens) in [
            (
                0,
                "user",
                row.try_get::<String, _>("request_text")?,
                row.try_get::<Option<i32>, _>("tokens_in")?,
            ),
            (
                1,
                "assistant",
                row.try_get::<String, _>("response_text")?,
                row.try_get::<Option<i32>, _>("tokens_out")?,
            ),
        ] {
            if content.trim().is_empty() {
                continue;
            }
            let mut episode = FleetEpisode::new(
                source_kind,
                node,
                &session_id,
                source_seq.saturating_mul(2).saturating_add(offset),
                role,
                crate::session_export::redact(&content),
            );
            episode.ts = row.get("ts");
            episode.model = row.try_get("engine")?;
            episode.tokens = tokens;
            episode.work_item_id = row.try_get("work_item_id")?;
            episodes.push(episode);
        }
    }
    if episodes.is_empty() {
        return Ok(CollectionSummary::default());
    }
    let inserted = ff_db::pg_append_episode_batch(
        pool,
        "ff_interaction",
        node,
        "ff_interactions",
        "ff_interactions",
        &episodes,
    )
    .await?;
    Ok(CollectionSummary {
        streams: 1,
        inserted,
    })
}

pub async fn collect_fleet_episodes(pool: &PgPool, node: &str) -> Result<CollectionSummary> {
    let home = dirs::home_dir().context("home directory is unavailable")?;
    let mut adapters: Vec<Box<dyn SessionSourceAdapter>> = vec![
        Box::new(JsonlAdapter {
            source_kind: "claude_cli",
            roots: &[".claude/projects"],
        }),
        Box::new(JsonlAdapter {
            source_kind: "codex_cli",
            roots: &[".codex/sessions"],
        }),
        Box::new(JsonlAdapter {
            source_kind: "kimi_cli",
            roots: &[".kimi-code/sessions", ".kimi/sessions"],
        }),
    ];
    if std::env::var("FORGEFLEET_OPENCLAW_EPISODES")
        .is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "on"))
    {
        adapters.push(Box::new(JsonlAdapter {
            source_kind: "openclaw",
            roots: &[".openclaw"],
        }));
    }
    let mut total = CollectionSummary::default();
    for adapter in adapters {
        let summary = adapter.collect(pool, &home, node).await?;
        total.streams += summary.streams;
        total.inserted += summary.inserted;
    }
    let summary = collect_interactions(pool, node).await?;
    total.streams += summary.streams;
    total.inserted += summary.inserted;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_message_parser_excludes_private_reasoning() {
        let public = serde_json::json!({
            "type": "assistant",
            "message": {"role":"assistant","content":[{"type":"text","text":"answer"}]}
        });
        assert_eq!(
            message_from(&public),
            Some(("assistant".into(), "answer".into()))
        );
        let private = serde_json::json!({
            "type": "assistant",
            "message": {"role":"assistant","content":[{"type":"thinking","text":"secret"}]}
        });
        assert!(is_private_or_system(&private));
    }
}
