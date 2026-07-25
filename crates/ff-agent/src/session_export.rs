//! Redacted export of vendor CLI JSONL transcripts to the shared Obsidian
//! vault, and to `fleet_episodes` in shared Postgres.
//!
//! Session state used to live only on `adele` — every non-adele node staged
//! its rendered markdown locally and rsync'd it to adele's vault, so an
//! unreachable adele meant those nodes' exported sessions were unavailable
//! fleet-wide. Every node now also inserts its parsed turns directly into
//! `fleet_episodes` (shared Postgres, no single node in the path); the local
//! markdown/vault staging is unchanged and still adele-only, since it's a
//! human-readable Obsidian export rather than the fleet's session state.

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
    time::SystemTime,
};

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Local, Utc};
use ff_db::FleetEpisodeInsert;
use regex::Regex;
use serde_json::Value;
use sqlx::PgPool;

const PART_BYTES: usize = 900 * 1024;
const VAULT_REL: &str = "projects/Yarli_KnowledgeBase/ForgeFleet/sessions";

#[derive(Debug, Default, Clone, Copy)]
pub struct ExportSummary {
    pub scanned: usize,
    pub exported: usize,
    pub skipped: usize,
}

/// Export all locally present Claude Code, Codex, and Kimi JSONL sessions:
/// render+redact each to markdown (adele writes straight into the vault;
/// other nodes stage the same tree locally for inspection), and insert its
/// turns into `fleet_episodes` in shared Postgres — the fleet-wide session
/// state, not tied to any single node.
pub async fn export_local_sessions(
    pool: &PgPool,
    vault_override: Option<&Path>,
    force: bool,
) -> Result<ExportSummary> {
    let vault_override = vault_override.map(Path::to_path_buf);
    let (summary, pending) =
        tokio::task::spawn_blocking(move || scan_and_render(vault_override.as_deref(), force))
            .await
            .context("session export scan panicked")??;

    for episode in pending {
        pg_insert_episode(pool, &episode).await?;
    }
    Ok(summary)
}

/// One parsed transcript turn, staged for insertion into `fleet_episodes`.
struct PendingEpisode {
    source_kind: &'static str,
    node: String,
    session_id: String,
    seq: i32,
    ts: DateTime<Utc>,
    role: String,
    content: String,
}

async fn pg_insert_episode(pool: &PgPool, episode: &PendingEpisode) -> Result<()> {
    ff_db::pg_insert_fleet_episode(
        pool,
        &FleetEpisodeInsert {
            source_kind: episode.source_kind.to_owned(),
            node: episode.node.clone(),
            session_id: episode.session_id.clone(),
            seq: episode.seq,
            ts: episode.ts,
            role: episode.role.clone(),
            content: episode.content.clone(),
            redacted: true,
        },
    )
    .await
    .context("insert fleet_episodes row")?;
    Ok(())
}

/// Blocking half of [`export_local_sessions`]: scans local JSONL transcript
/// stores, writes redacted markdown, and returns the parsed turns still
/// needing a `fleet_episodes` insert.
fn scan_and_render(
    vault_override: Option<&Path>,
    force: bool,
) -> Result<(ExportSummary, Vec<PendingEpisode>)> {
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
    collect_jsonl(&home.join(".kimi/sessions"), Vendor::Kimi, &mut sources)?;
    collect_jsonl(&home.join(".kimi/user-history"), Vendor::Kimi, &mut sources)?;

    let mut summary = ExportSummary::default();
    let mut pending = Vec::new();
    for source in sources {
        summary.scanned += 1;
        let target = target_for(&root, &source.path, source.vendor, &computer)?;
        if !force && output_is_current(&target, &source.path)? {
            summary.skipped += 1;
            continue;
        }
        let turns = parse_turns(&source.path)?;
        let markdown = render_markdown(&turns, source.vendor, &computer, &source.path);
        write_parts(&target, &markdown)?;

        let session_id =
            first_session_id(&source.path).unwrap_or_else(|| fallback_session_id(&source.path));
        let ts = session_started_at(&source.path)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(Utc::now);
        for (seq, turn) in turns.iter().enumerate() {
            pending.push(PendingEpisode {
                source_kind: source_kind_for(source.vendor),
                node: computer.clone(),
                session_id: session_id.clone(),
                seq: seq as i32,
                ts,
                role: turn.role.clone(),
                content: redact(&turn.content),
            });
        }
        summary.exported += 1;
    }

    Ok((summary, pending))
}

fn source_kind_for(vendor: Vendor) -> &'static str {
    match vendor {
        Vendor::Claude => "claude_cli",
        Vendor::Codex => "codex_cli",
        Vendor::Kimi => "kimi_cli",
    }
}

fn fallback_session_id(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_owned()
}

#[derive(Clone, Copy)]
enum Vendor {
    Claude,
    Codex,
    Kimi,
}

struct Source {
    path: PathBuf,
    vendor: Vendor,
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

/// One rendered (role, content) turn parsed out of a vendor JSONL transcript
/// — shared by the markdown renderer and the `fleet_episodes` insert path.
struct Turn {
    role: String,
    content: String,
}

fn render_markdown(turns: &[Turn], vendor: Vendor, computer: &str, path: &Path) -> String {
    let mut out = format!(
        "---\nsource: {}\ncomputer: {}\nredacted: true\n---\n\n# Session {}\n\n",
        vendor_name(vendor),
        computer,
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
    );
    for turn in turns {
        out.push_str(&format!(
            "## {}\n\n{}\n\n",
            turn.role.to_ascii_uppercase(),
            turn.content
        ));
    }
    redact(&out)
}

fn parse_turns(path: &Path) -> Result<Vec<Turn>> {
    let file = fs::File::open(path)?;
    let mut turns = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if is_system_reminder(&value) {
            continue;
        }
        if let Some(turn) = turn_from_value(&value) {
            turns.push(turn);
        }
    }
    Ok(turns)
}

fn turn_from_value(value: &Value) -> Option<Turn> {
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
        let rendered = rendered.trim();
        if rendered.is_empty() {
            return None;
        }
        return Some(Turn {
            role: role.unwrap_or(kind).to_owned(),
            content: rendered.to_owned(),
        });
    }
    if let Some(text) = value
        .pointer("/payload/message/content")
        .and_then(Value::as_str)
    {
        return Some(Turn {
            role: role.unwrap_or("message").to_owned(),
            content: text.to_owned(),
        });
    }
    None
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

fn redact(input: &str) -> String {
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
    fn turn_from_value_extracts_role_and_flattened_content() {
        let turn = turn_from_value(&serde_json::json!({
            "message": {"role": "user", "content": [{"type": "text", "text": "hi there"}]}
        }))
        .expect("turn present");
        assert_eq!(turn.role, "user");
        assert_eq!(turn.content, "hi there");
    }

    #[test]
    fn turn_from_value_skips_empty_content() {
        assert!(
            turn_from_value(&serde_json::json!({"message": {"role": "user", "content": []}}))
                .is_none()
        );
    }

    #[test]
    fn source_kind_maps_every_vendor() {
        assert_eq!(source_kind_for(Vendor::Claude), "claude_cli");
        assert_eq!(source_kind_for(Vendor::Codex), "codex_cli");
        assert_eq!(source_kind_for(Vendor::Kimi), "kimi_cli");
    }
}
