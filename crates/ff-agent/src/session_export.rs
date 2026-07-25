//! Redacted export of vendor CLI JSONL transcripts to the shared Obsidian vault.

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
    time::SystemTime,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Local, Utc};
use ff_core::schema::basic_memory::{BasicMemoryFrontmatter, extract_relations};
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};

const PART_BYTES: usize = 900 * 1024;
const MIN_EPISODE_BYTES: usize = 500;
const VAULT_REL: &str = "projects/Yarli_KnowledgeBase/ForgeFleet/sessions";

#[derive(Debug, Default, Clone, Copy)]
pub struct ExportSummary {
    pub scanned: usize,
    pub exported: usize,
    pub skipped: usize,
}

/// Export all locally present Claude Code, Codex, and Kimi JSONL sessions.
/// Non-Adele nodes stage the exact vault-relative tree and rsync it to Adele.
pub async fn export_local_sessions(
    pg: Option<&PgPool>,
    vault_override: Option<&Path>,
    force: bool,
) -> Result<ExportSummary> {
    let home = dirs::home_dir().context("home directory is unavailable")?;
    let computer = computer_name();
    let direct_vault = vault_override
        .map(Path::to_path_buf)
        .or_else(|| (computer.eq_ignore_ascii_case("adele")).then(|| home.join(VAULT_REL)));
    let root = direct_vault
        .clone()
        .unwrap_or_else(|| home.join(".forgefleet/session-exports"));

    let mut sources = Vec::new();
    collect_jsonl(&home.join(".claude/projects"), Vendor::Claude, &mut sources)?;
    collect_jsonl(&home.join(".codex/sessions"), Vendor::Codex, &mut sources)?;
    collect_jsonl(
        &home.join(".kimi-code/sessions"),
        Vendor::Kimi,
        &mut sources,
    )?;
    collect_jsonl(&home.join(".kimi/sessions"), Vendor::Kimi, &mut sources)?;
    collect_jsonl(&home.join(".kimi/user-history"), Vendor::Kimi, &mut sources)?;

    let mut summary = ExportSummary::default();
    for source in sources {
        summary.scanned += 1;
        if let Some(pg) = pg
            && let Err(error) = ingest_jsonl_episodes(pg, &source, &computer).await
        {
            tracing::warn!(
                path = %source.path.display(),
                error = %error,
                "session export: fleet episode ingestion failed"
            );
        }
        let target = target_for(&root, &source.path, source.vendor, &computer)?;
        if !force && output_is_current(&target, &source.path)? {
            summary.skipped += 1;
            continue;
        }
        let (markdown, episodes) = render_jsonl(&source.path, source.vendor, &computer)?;
        write_parts(&target, &markdown)?;
        if let Some(pg) = pg {
            for episode in episodes {
                if let Err(error) = upsert_episode(pg, &episode).await {
                    tracing::warn!(
                        session_id = %episode.session_id,
                        error = %error,
                        "session export: compaction episode persistence failed"
                    );
                }
            }
        }
        summary.exported += 1;
    }

    if direct_vault.is_none() && summary.exported > 0 {
        ship_to_adele(&root)?;
    }
    if let Some(pg) = pg {
        ingest_interaction_episodes(pg, &computer).await?;
    }
    Ok(summary)
}

#[derive(Clone, Copy)]
enum Vendor {
    Claude,
    Codex,
    Kimi,
}

impl Vendor {
    fn source_kind(self) -> &'static str {
        match self {
            Self::Claude => "claude_cli",
            Self::Codex => "codex_cli",
            Self::Kimi => "kimi_cli",
        }
    }
}

struct Source {
    path: PathBuf,
    vendor: Vendor,
}

struct EpisodeDraft {
    session_id: String,
    ts: DateTime<Utc>,
    project: String,
    title: String,
    body: String,
}

async fn ingest_jsonl_episodes(pg: &PgPool, source: &Source, node: &str) -> Result<()> {
    let session_id =
        first_session_id(&source.path).unwrap_or_else(|| source.path.display().to_string());
    let high_watermark = ff_db::queries::pg_fleet_episode_high_watermark(
        pg,
        source.vendor.source_kind(),
        node,
        &session_id,
    )
    .await?;
    let file = fs::File::open(&source.path)?;
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let seq = i32::try_from(index).unwrap_or(i32::MAX);
        if seq <= high_watermark {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line?) else {
            continue;
        };
        if value.to_string().contains("<system-reminder>") {
            continue;
        }
        let Some((role, content)) = normalized_turn(&value) else {
            continue;
        };
        let ts = json_timestamp(&value).unwrap_or_else(Utc::now);
        let episode = ff_db::queries::FleetEpisodeRecord {
            source_kind: source.vendor.source_kind().into(),
            node: node.into(),
            model: json_string(&value, &["/model", "/payload/model", "/message/model"]),
            session_id: session_id.clone(),
            work_item_id: json_string(
                &value,
                &[
                    "/work_item_id",
                    "/payload/work_item_id",
                    "/metadata/work_item_id",
                ],
            )
            .and_then(|id| uuid::Uuid::parse_str(&id).ok()),
            seq,
            ts,
            role,
            content: redact(&content),
            tokens: json_i32(
                &value,
                &[
                    "/tokens",
                    "/usage/total_tokens",
                    "/payload/usage/total_tokens",
                ],
            ),
            redacted: true,
        };
        ff_db::queries::pg_record_fleet_episode(pg, &episode).await?;
    }
    Ok(())
}

fn normalized_turn(value: &Value) -> Option<(String, String)> {
    let role = json_string(
        value,
        &[
            "/role",
            "/message/role",
            "/payload/role",
            "/type",
            "/payload/type",
        ],
    )?;
    let role = match role.to_ascii_lowercase().as_str() {
        "user" | "human" | "user_message" => "user",
        "assistant" | "assistant_message" | "model" => "assistant",
        "tool" | "tool_result" | "tool_use" => "tool",
        "system" => "system",
        _ => return None,
    }
    .to_string();
    let content = [
        "/content",
        "/message/content",
        "/payload/content",
        "/text",
        "/message/text",
    ]
    .into_iter()
    .find_map(|pointer| value.pointer(pointer))
    .map(|content| {
        let mut text = String::new();
        flatten_content(content, &mut text);
        text
    })?;
    (!content.trim().is_empty()).then_some((role, content))
}

fn json_string(value: &Value, pointers: &[&str]) -> Option<String> {
    pointers
        .iter()
        .find_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn json_i32(value: &Value, pointers: &[&str]) -> Option<i32> {
    pointers
        .iter()
        .find_map(|pointer| value.pointer(pointer).and_then(Value::as_i64))
        .and_then(|value| i32::try_from(value).ok())
}

fn json_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    json_string(value, &["/timestamp", "/created_at", "/payload/timestamp"])
        .and_then(|raw| DateTime::parse_from_rfc3339(&raw).ok())
        .map(|ts| ts.with_timezone(&Utc))
}

async fn ingest_interaction_episodes(pg: &PgPool, _local_node: &str) -> Result<()> {
    let rows = sqlx::query(
        "WITH numbered AS (
             SELECT i.*,
                    (row_number() OVER (
                        PARTITION BY COALESCE(i.session_id, i.id)
                        ORDER BY i.ts, i.id
                    ) - 1)::int AS episode_seq
               FROM ff_interactions i
         )
         SELECT n.id, n.ts, n.session_id, n.request_text, n.response_text,
                n.engine, n.worker_name, n.work_item_id, n.purpose,
                n.tokens_in, n.tokens_out, n.episode_seq
           FROM numbered n
          WHERE NOT EXISTS (
                SELECT 1 FROM fleet_episodes e
                 WHERE e.source_kind = CASE
                           WHEN n.purpose = 'research' THEN 'research'
                           WHEN n.purpose = 'council' THEN 'council'
                           ELSE 'ff_interaction'
                       END
                   AND e.node = COALESCE(n.worker_name, 'unknown')
                   AND e.session_id = COALESCE(n.session_id::text, n.id::text)
                   AND e.seq = n.episode_seq * 2
          )
          ORDER BY n.ts, n.id
          LIMIT 5000",
    )
    .fetch_all(pg)
    .await?;
    for row in rows.into_iter().rev() {
        let id: uuid::Uuid = row.get("id");
        let purpose: Option<String> = row.get("purpose");
        let source_kind = match purpose.as_deref() {
            Some("research") => "research",
            Some("council") => "council",
            _ => "ff_interaction",
        };
        let session_id = row
            .get::<Option<uuid::Uuid>, _>("session_id")
            .map(|id| id.to_string())
            .unwrap_or_else(|| format!("interaction:{id}"));
        let node = row
            .get::<Option<String>, _>("worker_name")
            .unwrap_or_else(|| "unknown".to_string());
        let episode_seq: i32 = row.get("episode_seq");
        for (offset, role, content, tokens) in [
            (
                0,
                "user",
                row.get::<String, _>("request_text"),
                row.get::<i32, _>("tokens_in"),
            ),
            (
                1,
                "assistant",
                row.get::<String, _>("response_text"),
                row.get::<i32, _>("tokens_out"),
            ),
        ] {
            if content.is_empty() {
                continue;
            }
            ff_db::queries::pg_record_fleet_episode(
                pg,
                &ff_db::queries::FleetEpisodeRecord {
                    source_kind: source_kind.into(),
                    node: node.clone(),
                    model: row.get("engine"),
                    session_id: session_id.clone(),
                    work_item_id: row.get("work_item_id"),
                    seq: episode_seq.saturating_mul(2).saturating_add(offset),
                    ts: row.get("ts"),
                    role: role.into(),
                    content: redact(&content),
                    tokens: Some(tokens),
                    redacted: true,
                },
            )
            .await?;
        }
    }
    Ok(())
}

fn collect_jsonl(dir: &Path, vendor: Vendor, out: &mut Vec<Source>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            collect_jsonl(&path, vendor, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl")
            && (!matches!(vendor, Vendor::Kimi)
                || path.file_name().and_then(|s| s.to_str()) == Some("context.jsonl")
                || path
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|s| s.to_str())
                    == Some("user-history"))
        {
            out.push(Source { path, vendor });
        }
    }
    Ok(())
}

fn target_for(root: &Path, source: &Path, vendor: Vendor, computer: &str) -> Result<PathBuf> {
    let meta = fs::metadata(source)?;
    let modified: DateTime<Local> = session_started_at(source)
        .unwrap_or_else(|| meta.modified().unwrap_or(SystemTime::now()).into());
    let fallback_session = if matches!(vendor, Vendor::Kimi)
        && source.file_name().and_then(|s| s.to_str()) == Some("context.jsonl")
    {
        source
            .parent()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
    } else {
        source
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
    };
    let session = first_session_id(source).unwrap_or_else(|| fallback_session.to_owned());
    let project = match vendor {
        Vendor::Claude => first_project_from_jsonl(source).unwrap_or_else(|| {
            source
                .parent()
                .and_then(Path::file_name)
                .and_then(|s| s.to_str())
                .map(decode_claude_project)
                .unwrap_or_else(|| "unknown".into())
        }),
        Vendor::Codex => first_project_from_jsonl(source).unwrap_or_else(|| "codex".into()),
        Vendor::Kimi => first_project_from_jsonl(source).unwrap_or_else(|| "kimi".into()),
    };
    let month = format!("{:02}-{}", modified.month(), modified.format("%B"));
    let base = format!(
        "{}-{}-{}",
        modified.format("%Y%m%d"),
        sanitize(computer),
        sanitize(&session)
    );
    Ok(root
        .join(sanitize(&project))
        .join(modified.year().to_string())
        .join(month)
        .join(base)
        .with_extension("md"))
}

fn session_started_at(path: &Path) -> Option<DateTime<Local>> {
    let file = fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines().take(40).flatten() {
        let value: Value = serde_json::from_str(&line).ok()?;
        for pointer in ["/timestamp", "/created_at", "/payload/timestamp"] {
            if let Some(timestamp) = value.pointer(pointer).and_then(Value::as_str)
                && let Ok(parsed) = DateTime::parse_from_rfc3339(timestamp)
            {
                return Some(parsed.with_timezone(&Local));
            }
        }
    }
    None
}

fn first_session_id(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines().take(40).flatten() {
        let value: Value = serde_json::from_str(&line).ok()?;
        for pointer in [
            "/sessionId",
            "/session_id",
            "/payload/session_id",
            "/payload/id",
        ] {
            if let Some(id) = value.pointer(pointer).and_then(Value::as_str)
                && !id.is_empty()
            {
                return Some(id.to_owned());
            }
        }
    }
    None
}

fn decode_claude_project(raw: &str) -> String {
    raw.trim_start_matches('-')
        .rsplit('-')
        .find(|s| !s.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

fn first_project_from_jsonl(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines().take(40).flatten() {
        let value: Value = serde_json::from_str(&line).ok()?;
        for pointer in ["/cwd", "/payload/cwd", "/session/cwd", "/workspace"] {
            if let Some(cwd) = value.pointer(pointer).and_then(Value::as_str) {
                return Path::new(cwd).file_name()?.to_str().map(str::to_owned);
            }
        }
    }
    None
}

fn render_jsonl(
    path: &Path,
    vendor: Vendor,
    computer: &str,
) -> Result<(String, Vec<EpisodeDraft>)> {
    let fallback_session = first_session_id(path).unwrap_or_else(|| path.display().to_string());
    let fallback_project = first_project_from_jsonl(path).unwrap_or_else(|| "unknown".into());
    let mut out = format!(
        "---\nsource: {}\ncomputer: {}\nredacted: true\n---\n\n# Session {}\n\n",
        vendor_name(vendor),
        computer,
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
    );
    let mut episodes = Vec::new();
    let file = fs::File::open(path)?;
    for line in BufReader::new(file).lines() {
        let line = line?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if is_system_reminder(&value) {
            continue;
        }
        if let Some(episode) =
            compaction_episode(&value, &fallback_session, &fallback_project, vendor)
        {
            episodes.push(episode);
        }
        render_value(&value, &mut out);
    }
    Ok((redact(&out), episodes))
}

fn compaction_episode(
    value: &Value,
    fallback_session: &str,
    fallback_project: &str,
    vendor: Vendor,
) -> Option<EpisodeDraft> {
    let content = [
        "/summary",
        "/payload/summary",
        "/message/summary",
        "/content",
        "/payload/content",
        "/message/content",
    ]
    .into_iter()
    .find_map(|p| value.pointer(p))
    .map(|content| {
        let mut text = String::new();
        flatten_content(content, &mut text);
        text
    })?;
    let marked_by_record = ["/type", "/kind", "/payload/type", "/payload/kind"]
        .into_iter()
        .filter_map(|p| value.pointer(p).and_then(Value::as_str))
        .any(|kind| {
            let kind = kind.to_ascii_lowercase();
            kind.contains("compact") || kind == "summary"
        });
    let marked_by_frontmatter = BasicMemoryFrontmatter::parse(&content).is_some_and(|(fm, _)| {
        matches!(
            fm.memory_type.trim().to_ascii_lowercase().as_str(),
            "summary" | "compaction"
        )
    });
    if !marked_by_record && !marked_by_frontmatter {
        return None;
    }
    if content.len() <= MIN_EPISODE_BYTES {
        return None;
    }
    let session_id = [
        "/sessionId",
        "/session_id",
        "/payload/session_id",
        "/payload/id",
    ]
    .into_iter()
    .find_map(|p| value.pointer(p).and_then(Value::as_str))
    .filter(|id| !id.is_empty())
    .unwrap_or(fallback_session)
    .to_string();
    let ts = ["/timestamp", "/created_at", "/payload/timestamp"]
        .into_iter()
        .find_map(|p| value.pointer(p).and_then(Value::as_str))
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|ts| ts.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    Some(EpisodeDraft {
        title: format!("{} compaction {}", vendor_name(vendor), ts.date_naive()),
        session_id,
        ts,
        project: fallback_project.to_string(),
        body: redact(&content),
    })
}

async fn upsert_episode(pg: &PgPool, episode: &EpisodeDraft) -> Result<()> {
    let path = format!(
        "episode://{}/{}/{}",
        sanitize(&episode.project),
        sanitize(&episode.session_id),
        episode.ts.date_naive()
    );
    let tags = vec![
        "compaction".to_string(),
        format!("session_id:{}", episode.session_id),
        format!("ts:{}", episode.ts.to_rfc3339()),
    ];
    let hash = format!("{:x}", Sha256::digest(episode.body.as_bytes()));
    let episode_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO brain_vault_nodes
            (path, title, node_type, project, tags, from_thread, confidence,
             content_hash, body, valid_from, provenance, updated_at)
         VALUES ($1, $2, 'episode', $3, $4, $5, 1.0, $6, $7, $8,
                 'session-compaction', NOW())
         ON CONFLICT (path) DO UPDATE SET
            title = EXCLUDED.title, tags = EXCLUDED.tags,
            content_hash = EXCLUDED.content_hash, body = EXCLUDED.body,
            valid_from = EXCLUDED.valid_from, valid_until = NULL,
            provenance = EXCLUDED.provenance, updated_at = NOW()
         WHERE brain_vault_nodes.valid_from <= EXCLUDED.valid_from
         RETURNING id",
    )
    .bind(&path)
    .bind(&episode.title)
    .bind(&episode.project)
    .bind(&tags)
    .bind(&episode.session_id)
    .bind(&hash)
    .bind(&episode.body)
    .bind(episode.ts)
    .fetch_optional(pg)
    .await?
    .unwrap_or_else(|| uuid::Uuid::nil());
    if episode_id.is_nil() {
        return Ok(());
    }

    for relation in extract_relations(&episode.body) {
        let (node_type, canonical_path) = match relation.relation_type.as_str() {
            "work_item" | "work-item" => (
                "pm:work_item",
                uuid::Uuid::parse_str(&relation.target)
                    .ok()
                    .map(|_| format!("pm://work_item/{}", relation.target)),
            ),
            "computer" => (
                "fleet:computer",
                Some(format!(
                    "fleet://computer/{}",
                    relation.target.to_ascii_lowercase()
                )),
            ),
            _ => continue,
        };
        let target: Option<uuid::Uuid> = sqlx::query(
            "SELECT id FROM brain_vault_nodes
              WHERE valid_until IS NULL AND node_type = $1
                AND (path = $2 OR lower(title) = lower($3))
              ORDER BY (path = $2) DESC LIMIT 1",
        )
        .bind(node_type)
        .bind(canonical_path.as_deref().unwrap_or(""))
        .bind(&relation.target)
        .fetch_optional(pg)
        .await?
        .map(|row| row.get("id"));
        if let Some(target) = target {
            ff_db::pg_upsert_brain_vault_edge(
                pg,
                episode_id,
                target,
                &relation.relation_type,
                1.0,
                "compaction-typed-link",
            )
            .await?;
        }
    }
    Ok(())
}

fn render_value(value: &Value, out: &mut String) {
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
        });
    if let Some(content) = payload.get("content").or_else(|| payload.get("text")) {
        let mut rendered = String::new();
        flatten_content(content, &mut rendered);
        if !rendered.trim().is_empty() {
            out.push_str(&format!(
                "## {}\n\n{}\n\n",
                role.unwrap_or(kind).to_ascii_uppercase(),
                rendered.trim()
            ));
        }
    } else if let Some(text) = value
        .pointer("/payload/message/content")
        .and_then(Value::as_str)
    {
        out.push_str(&format!(
            "## {}\n\n{}\n\n",
            role.unwrap_or("message").to_ascii_uppercase(),
            text
        ));
    }
}

fn flatten_content(value: &Value, out: &mut String) {
    match value {
        Value::String(s) => out.push_str(s),
        Value::Array(items) => {
            for item in items {
                flatten_content(item, out);
            }
        }
        Value::Object(map) => {
            let kind = map.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(kind, "tool_use" | "function_call") {
                let name = map.get("name").and_then(Value::as_str).unwrap_or("tool");
                out.push_str(&format!("- Tool call: `{name}`\n"));
            } else if !kind.contains("system") {
                if let Some(text) = map
                    .get("text")
                    .or_else(|| map.get("content"))
                    .or_else(|| map.get("output_text"))
                {
                    flatten_content(text, out);
                }
            }
        }
        _ => {}
    }
}

fn is_system_reminder(value: &Value) -> bool {
    value.to_string().contains("<system-reminder>")
        || value.get("type").and_then(Value::as_str) == Some("system")
}

pub fn redact(input: &str) -> String {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        [
            r"ghp_[A-Za-z0-9]{20,}",
            r"github_pat_[A-Za-z0-9_]{20,}",
            r"AGE-SECRET-KEY-[A-Z0-9-]+",
            r"sk-ant-[A-Za-z0-9_-]{16,}",
            r"ops_[A-Za-z0-9_-]{16,}",
            r"eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+",
        ]
        .into_iter()
        .map(|p| Regex::new(p).expect("valid redaction regex"))
        .collect()
    });
    patterns.iter().fold(input.to_owned(), |text, pattern| {
        pattern.replace_all(&text, "[REDACTED]").into_owned()
    })
}

fn output_is_current(target: &Path, source: &Path) -> Result<bool> {
    let source_time = fs::metadata(source)?.modified()?;
    let candidate = if target.exists() {
        target.to_owned()
    } else {
        target.with_extension("")
    };
    if candidate.is_file() {
        return Ok(fs::metadata(candidate)?.modified()? >= source_time);
    }
    if candidate.is_dir() {
        return Ok(fs::read_dir(candidate)?
            .filter_map(Result::ok)
            .filter_map(|e| e.metadata().ok())
            .filter_map(|m| m.modified().ok())
            .max()
            .is_some_and(|t| t >= source_time));
    }
    Ok(false)
}

fn write_parts(target: &Path, markdown: &str) -> Result<()> {
    let parent = target.parent().context("target has no parent")?;
    fs::create_dir_all(parent)?;
    let stem = target
        .file_stem()
        .and_then(|s| s.to_str())
        .context("target has no stem")?;
    let split_dir = parent.join(stem);
    if markdown.len() <= PART_BYTES {
        atomic_write(target, markdown.as_bytes())?;
        if split_dir.exists() {
            fs::remove_dir_all(split_dir)?;
        }
        return Ok(());
    }
    fs::create_dir_all(&split_dir)?;
    if target.exists() {
        fs::remove_file(target)?;
    }
    let mut start = 0;
    let mut part = 1;
    while start < markdown.len() {
        let mut end = (start + PART_BYTES).min(markdown.len());
        while !markdown.is_char_boundary(end) {
            end -= 1;
        }
        let part_path = split_dir.join(format!("{stem}-{part}.md"));
        atomic_write(&part_path, markdown[start..end].as_bytes())?;
        start = end;
        part += 1;
    }
    for entry in fs::read_dir(&split_dir)? {
        let path = entry?.path();
        let stale = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.rsplit('-').next())
            .and_then(|s| s.parse::<usize>().ok())
            .is_some_and(|n| n >= part);
        if stale {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("md.tmp");
    let mut file = fs::File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn ship_to_adele(root: &Path) -> Result<()> {
    let source = format!("{}/", root.display());
    let status = Command::new("rsync")
        .args([
            "-az",
            "--partial",
            &source,
            &format!("adele:~/{VAULT_REL}/"),
        ])
        .status()
        .context("run rsync to adele")?;
    if !status.success() {
        bail!("rsync to adele failed with {status}");
    }
    Ok(())
}

fn computer_name() -> String {
    std::env::var("FORGEFLEET_COMPUTER_NAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            Command::new("hostname")
                .arg("-s")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "unknown".into())
        })
}

fn sanitize(raw: &str) -> String {
    let value: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    value
        .trim_matches('-')
        .to_owned()
        .chars()
        .take(120)
        .collect::<String>()
        .pipe_nonempty()
}

trait NonEmpty {
    fn pipe_nonempty(self) -> String;
}
impl NonEmpty for String {
    fn pipe_nonempty(self) -> String {
        if self.is_empty() {
            "unknown".into()
        } else {
            self
        }
    }
}
fn vendor_name(v: Vendor) -> &'static str {
    match v {
        Vendor::Claude => "claude",
        Vendor::Codex => "codex",
        Vendor::Kimi => "kimi",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_every_required_secret_family() {
        let text = "ghp_abcdefghijklmnopqrstuvwxyz github_pat_abcdefghijklmnopqrstuvwxyz AGE-SECRET-KEY-1ABC-DEF sk-ant-abcdefghijklmnopqrstuvwxyz ops_abcdefghijklmnopqrstuvwxyz eyJabc.def.ghi";
        let redacted = redact(text);
        assert!(!redacted.contains("ghp_"));
        assert!(!redacted.contains("github_pat_"));
        assert!(!redacted.contains("AGE-SECRET-KEY"));
        assert!(!redacted.contains("sk-ant"));
        assert!(!redacted.contains("ops_"));
        assert!(!redacted.contains("eyJabc"));
    }

    #[test]
    fn tool_calls_are_markdown_bullets_and_reminders_are_skipped() {
        let mut out = String::new();
        flatten_content(
            &serde_json::json!([{"type":"tool_use","name":"Read"}]),
            &mut out,
        );
        assert_eq!(out, "- Tool call: `Read`\n");
        assert!(is_system_reminder(
            &serde_json::json!({"content":"<system-reminder>x"})
        ));
    }

    #[test]
    fn compaction_episode_requires_more_than_500_bytes() {
        let at_limit = serde_json::json!({
            "type": "compaction",
            "session_id": "session-1",
            "timestamp": "2026-07-24T01:02:03Z",
            "summary": "x".repeat(MIN_EPISODE_BYTES),
        });
        assert!(compaction_episode(&at_limit, "fallback", "forge-fleet", Vendor::Codex).is_none());

        let over_limit = serde_json::json!({
            "type": "summary",
            "session_id": "session-1",
            "timestamp": "2026-07-24T01:02:03Z",
            "summary": "x".repeat(MIN_EPISODE_BYTES + 1),
        });
        let episode =
            compaction_episode(&over_limit, "fallback", "forge-fleet", Vendor::Codex).unwrap();
        assert_eq!(episode.session_id, "session-1");
        assert_eq!(episode.ts.date_naive().to_string(), "2026-07-24");
        assert_eq!(episode.body.len(), MIN_EPISODE_BYTES + 1);
    }

    #[test]
    fn normalizes_vendor_turn_shapes() {
        let claude = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "hello"}
        });
        assert_eq!(
            normalized_turn(&claude),
            Some(("user".into(), "hello".into()))
        );

        let codex = serde_json::json!({
            "type": "response_item",
            "payload": {"role": "assistant", "content": [{"type":"text","text":"world"}]}
        });
        assert_eq!(
            normalized_turn(&codex),
            Some(("assistant".into(), "world".into()))
        );
    }
}
