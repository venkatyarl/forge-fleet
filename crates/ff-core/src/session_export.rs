//! Export local CLI session transcripts (Claude Code / Codex / Kimi) into the
//! Obsidian vault.
//!
//! One-shot (`ff session export`) and periodic daemon sweep are both backed by
//! `export_cli_sessions`.  Progress is tracked per source JSONL via a byte
//! offset + mtime cursor stored in the vault root, so repeated runs append only
//! new lines.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};

use crate::config::{FleetConfig, SessionExportConfig};

/// Default split threshold for a single markdown file (bytes).
const PART_SIZE_BYTES: usize = 900 * 1024;

/// Redaction patterns applied to every exported line before it is written.
const REDACTION_PATTERNS: &[&str] = &[
    // GitHub personal access tokens.
    r"ghp_[a-zA-Z0-9]{36}",
    // GitHub fine-grained PATs.
    r"github_pat_[a-zA-Z0-9_]{22,}",
    // age secret keys.
    r"AGE-SECRET-KEY-[a-zA-Z0-9]{59}",
    // Anthropic API keys.
    r"sk-ant-[a-zA-Z0-9_-]{32,}",
    // 1Password service-account tokens.
    r"ops_[a-zA-Z0-9]{32,}",
    // JWTs (header.payload.signature).
    r"eyJ[a-zA-Z0-9_-]*\.eyJ[a-zA-Z0-9_-]*\.[a-zA-Z0-9_-]*",
];

/// Lazily-initialized compiled redaction regex set.
fn redaction_regexes() -> Vec<Regex> {
    REDACTION_PATTERNS
        .iter()
        .map(|p| Regex::new(p).expect("static redaction regex is valid"))
        .collect()
}

/// Replace known secret patterns with `[REDACTED-<kind>]`.
pub fn redact(text: &str) -> String {
    let mut out = text.to_string();
    for (i, re) in redaction_regexes().iter().enumerate() {
        let label = match i {
            0 => "github-token",
            1 => "github-pat",
            2 => "age-secret-key",
            3 => "anthropic-key",
            4 => "1password-token",
            5 => "jwt",
            _ => "secret",
        };
        out = re
            .replace_all(&out, format!("[REDACTED-{label}]"))
            .to_string();
    }
    out
}

/// Result of one export pass.
#[derive(Debug, Clone, Default)]
pub struct ExportResult {
    /// Number of source sessions exported.
    pub sessions_exported: usize,
    /// Number of source files processed.
    pub files_processed: usize,
    /// Total redactions performed.
    pub redactions: usize,
}

impl ExportResult {
    fn add(&mut self, other: &ExportResult) {
        self.sessions_exported += other.sessions_exported;
        self.files_processed += other.files_processed;
        self.redactions += other.redactions;
    }
}

/// Per-source-file cursor so the next tick resumes from the last byte.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FileCursor {
    mtime_secs: i64,
    mtime_nanos: u32,
    bytes_read: u64,
}

/// Persisted cursor for the whole export subsystem.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ExportCursor {
    files: HashMap<PathBuf, FileCursor>,
}

impl ExportCursor {
    fn load(vault_dir: &Path) -> Self {
        let path = cursor_path(vault_dir);
        fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self, vault_dir: &Path) -> Result<()> {
        let path = cursor_path(vault_dir);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(self)?;
        fs::write(&path, data)?;
        Ok(())
    }
}

fn cursor_path(vault_dir: &Path) -> PathBuf {
    vault_dir.join(".ff_session_export_cursor.json")
}

/// Synchronous entry point used by the CLI and the daemon tick.
///
/// `computer_name` is used in the vault filename; when `None` it is resolved
/// from `hostname`.
pub fn export_cli_sessions(
    config: &FleetConfig,
    computer_name: Option<&str>,
) -> Result<ExportResult> {
    let cfg = &config.session_export;
    if !cfg.enabled {
        return Ok(ExportResult::default());
    }

    let vault_dir = resolve_vault_dir(cfg)?;
    fs::create_dir_all(&vault_dir)?;

    let computer = match computer_name {
        Some(c) => c.to_string(),
        None => hostname(),
    };

    let mut cursor = ExportCursor::load(&vault_dir);
    let mut total = ExportResult::default();

    for source_dir in &cfg.source_dirs {
        let expanded = expand_tilde(source_dir);
        match process_source_dir(&expanded, &vault_dir, &computer, cfg, &mut cursor) {
            Ok(result) => total.add(&result),
            Err(e) => {
                warn!(dir = %expanded.display(), error = %e, "session export source dir failed")
            }
        }
    }

    cursor.save(&vault_dir)?;
    Ok(total)
}

/// Async wrapper suitable for daemon ticks.
pub async fn run_export_tick(config: &FleetConfig, computer_name: Option<&str>) -> Result<()> {
    let config = config.clone();
    let computer = computer_name.map(|s| s.to_string());
    tokio::task::spawn_blocking(move || export_cli_sessions(&config, computer.as_deref()))
        .await
        .context("session export tick panicked")?
        .map(|_| ())
}

fn process_source_dir(
    source_dir: &Path,
    vault_dir: &Path,
    computer_name: &str,
    cfg: &SessionExportConfig,
    cursor: &mut ExportCursor,
) -> Result<ExportResult> {
    if !source_dir.is_dir() {
        return Ok(ExportResult::default());
    }

    let mut result = ExportResult::default();

    for path in find_jsonl_files(source_dir) {
        let project_folder = project_folder_for_jsonl(source_dir, &path);
        let session_id = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        match process_one_jsonl(
            &path,
            vault_dir,
            &project_folder,
            computer_name,
            &session_id,
            cursor,
        ) {
            Ok((exported, redactions)) => {
                result.files_processed += 1;
                if exported {
                    result.sessions_exported += 1;
                }
                result.redactions += redactions;
            }
            Err(e) => warn!(file = %path.display(), error = %e, "failed to export session jsonl"),
        }
    }

    // Prune cursor entries for files that no longer exist.
    cursor.files.retain(|p, _| p.exists());

    // Optionally ship to a remote vault.
    if let Some(target) = cfg.rsync_target.as_deref() {
        if let Err(e) = rsync_to_remote(vault_dir, target) {
            warn!(target, error = %e, "session export rsync failed");
        }
    }

    Ok(result)
}

fn process_one_jsonl(
    path: &Path,
    vault_dir: &Path,
    project_folder: &str,
    computer_name: &str,
    session_id: &str,
    cursor: &mut ExportCursor,
) -> Result<(bool, usize)> {
    let meta = fs::metadata(path)?;
    let mtime = meta.modified()?;
    let file_size = meta.len();

    let prev = cursor.files.get(path).cloned().unwrap_or_default();
    let (mtime_secs, mtime_nanos) = system_time_to_secs_nanos(mtime);

    // Nothing changed since last export.
    if prev.bytes_read >= file_size
        && prev.mtime_secs == mtime_secs
        && prev.mtime_nanos == mtime_nanos
    {
        return Ok((false, 0));
    }

    // If the file shrank or mtime jumped backward, re-export from start.
    let start_offset = if prev.mtime_secs > mtime_secs
        || (prev.mtime_secs == mtime_secs && prev.mtime_nanos > mtime_nanos)
    {
        0
    } else {
        prev.bytes_read.min(file_size)
    };

    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let start_offset = align_offset_to_next_line(&mut reader, start_offset)?;
    if start_offset > 0 {
        reader.seek(SeekFrom::Start(start_offset))?;
    }

    let mut rendered = String::new();
    let mut redactions = 0usize;
    let mut session_ts: Option<DateTime<Utc>> = None;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                warn!(file = %path.display(), error = %e, "skipping malformed jsonl line");
                continue;
            }
        };

        if let Some(ts) = extract_timestamp(&value) {
            if session_ts.is_none() || ts < session_ts.unwrap() {
                session_ts = Some(ts);
            }
        }

        if let Some(entry) = format_entry(&value) {
            let redacted = redact(&entry);
            redactions += count_redactions(&redacted);
            rendered.push_str(&redacted);
            rendered.push('\n');
        }
    }

    if rendered.trim().is_empty() {
        cursor.files.insert(
            path.to_path_buf(),
            FileCursor {
                mtime_secs,
                mtime_nanos,
                bytes_read: file_size,
            },
        );
        return Ok((false, redactions));
    }

    let ts = session_ts.unwrap_or_else(|| DateTime::<Utc>::from(std::time::SystemTime::now()));

    write_session_export(
        vault_dir,
        project_folder,
        ts,
        computer_name,
        &session_id,
        &rendered,
    )?;

    cursor.files.insert(
        path.to_path_buf(),
        FileCursor {
            mtime_secs,
            mtime_nanos,
            bytes_read: file_size,
        },
    );

    info!(
        file = %path.display(),
        session = %session_id,
        project = %project_folder,
        bytes = rendered.len(),
        redactions,
        "exported CLI session transcript"
    );

    Ok((true, redactions))
}

fn extract_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    value
        .get("timestamp")
        .or_else(|| value.get("ts"))
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

/// Render one JSONL object as a short markdown entry.
///
/// Returns `None` for entries that should be skipped entirely
/// (system-reminders, queue bookkeeping, etc.).
fn format_entry(value: &Value) -> Option<String> {
    let typ = value.get("type").and_then(Value::as_str).unwrap_or("");

    match typ {
        "system" | "system-reminder" => None,
        "queue-operation" => None,
        "last-prompt" => None,
        "user" => {
            let content = value
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
                .or_else(|| value.get("content").and_then(Value::as_str))
                .unwrap_or("");
            if content.trim().is_empty() {
                None
            } else {
                Some(format!("**User:** {}\n", content.trim()))
            }
        }
        "assistant" => {
            let msg = value.get("message").unwrap_or(value);
            let mut out = String::new();

            if let Some(content) = msg.get("content").and_then(Value::as_str) {
                if !content.trim().is_empty() {
                    out.push_str(&format!("**Assistant:** {}\n", content.trim()));
                }
            } else if let Some(parts) = msg.get("content").and_then(Value::as_array) {
                for part in parts {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        out.push_str(&format!("**Assistant:** {}\n", text.trim()));
                    }
                }
            }

            if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
                for tc in tool_calls {
                    let name = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .or_else(|| tc.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("tool");
                    let args = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .or_else(|| tc.get("arguments"))
                        .map(Value::to_string)
                        .unwrap_or_default();
                    out.push_str(&format!("- **Tool call:** `{}` {}\n", name, args));
                }
            }

            if out.is_empty() { None } else { Some(out) }
        }
        "attachment" => {
            let attachment = value.get("attachment").unwrap_or(value);
            let att_type = attachment.get("type").and_then(Value::as_str).unwrap_or("");
            match att_type {
                "tool_result" => {
                    let name = attachment
                        .get("tool_name")
                        .or_else(|| attachment.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("tool");
                    let result = attachment
                        .get("result")
                        .or_else(|| attachment.get("content"))
                        .map(|v| {
                            v.as_str()
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| v.to_string())
                        })
                        .unwrap_or_default();
                    Some(format!("- **Tool result:** `{}` {}\n", name, result))
                }
                "tool_use" => {
                    let name = attachment
                        .get("tool_name")
                        .or_else(|| attachment.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("tool");
                    let input = attachment
                        .get("input")
                        .or_else(|| attachment.get("arguments"))
                        .map(Value::to_string)
                        .unwrap_or_default();
                    Some(format!("- **Tool use:** `{}` {}\n", name, input))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn write_session_export(
    vault_dir: &Path,
    project_folder: &str,
    ts: DateTime<Utc>,
    computer_name: &str,
    session_id: &str,
    new_content: &str,
) -> Result<()> {
    let year = ts.year().to_string();
    let month_dir = format!("{:02}-{}", ts.month(), month_name(ts.month()));
    let base_name = format!(
        "{}-{}-{}",
        ts.format("%Y%m%d"),
        sanitize_filename(computer_name),
        sanitize_filename(session_id)
    );

    let sessions_root = vault_dir
        .join("ForgeFleet")
        .join("sessions")
        .join(sanitize_filename(project_folder))
        .join(&year)
        .join(&month_dir);

    let single_file = sessions_root.join(format!("{base_name}.md"));
    let parts_dir = sessions_root.join(&base_name);

    // Determine whether we are already using parts.
    let using_parts = parts_dir.is_dir();

    if !using_parts {
        let existing_size = if single_file.exists() {
            fs::metadata(&single_file)?.len() as usize
        } else {
            0
        };
        if existing_size + new_content.len() <= PART_SIZE_BYTES {
            // Append to the single file.
            fs::create_dir_all(&sessions_root)?;
            let mut f = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&single_file)?;
            f.write_all(new_content.as_bytes())?;
            return Ok(());
        }

        // Convert single file to parts.
        fs::create_dir_all(&parts_dir)?;
        let existing = if single_file.exists() {
            fs::read_to_string(&single_file)?
        } else {
            String::new()
        };
        if single_file.exists() {
            fs::remove_file(&single_file)?;
        }
        write_parts(&parts_dir, &base_name, &existing, new_content)?;
        return Ok(());
    }

    // Already using parts: append to the last part until it would overflow.
    fs::create_dir_all(&parts_dir)?;
    let mut part_files: Vec<PathBuf> = fs::read_dir(&parts_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
        .collect();
    part_files.sort();

    let mut existing_last = String::new();
    let last_index = if let Some(last) = part_files.last() {
        existing_last = fs::read_to_string(last)?;
        extract_part_index(last).unwrap_or(1)
    } else {
        1
    };

    if existing_last.len() + new_content.len() <= PART_SIZE_BYTES {
        let last_path = part_files
            .last()
            .cloned()
            .unwrap_or_else(|| parts_dir.join(format!("{base_name}-{last_index}.md")));
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&last_path)?;
        f.write_all(new_content.as_bytes())?;
    } else {
        // Start a new part.
        write_parts(&parts_dir, &base_name, "", new_content)?;
    }

    Ok(())
}

fn write_parts(parts_dir: &Path, base_name: &str, existing: &str, new_content: &str) -> Result<()> {
    let mut current = existing.to_string();
    current.push_str(new_content);

    let mut part_index = 1usize;
    let mut remaining = current.as_str();

    while !remaining.is_empty() {
        // Find existing part files to avoid overwriting.
        let path = parts_dir.join(format!("{base_name}-{part_index}.md"));
        if path.exists() {
            let existing_part = fs::read_to_string(&path)?;
            if existing_part.len() < PART_SIZE_BYTES {
                // Top up existing part.
                let capacity = PART_SIZE_BYTES - existing_part.len();
                let (chunk, rest) = split_at_char_boundary(remaining, capacity);
                let mut f = fs::OpenOptions::new().append(true).open(&path)?;
                f.write_all(chunk.as_bytes())?;
                remaining = rest;
                part_index += 1;
                continue;
            }
        }

        let (chunk, rest) = split_at_char_boundary(remaining, PART_SIZE_BYTES);
        fs::write(&path, chunk.as_bytes())?;
        remaining = rest;
        part_index += 1;
    }

    Ok(())
}

fn split_at_char_boundary(s: &str, max_bytes: usize) -> (String, &str) {
    if s.len() <= max_bytes {
        return (s.to_string(), "");
    }
    let mut pos = max_bytes;
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    (s[..pos].to_string(), &s[pos..])
}

fn extract_part_index(path: &Path) -> Option<usize> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.rsplitn(2, '-').next())
        .and_then(|n| n.parse().ok())
}

fn count_redactions(redacted: &str) -> usize {
    redacted.matches("[REDACTED-").count()
}

fn find_jsonl_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return out;
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = match fs::read_dir(&current) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn project_folder_for_jsonl(source_dir: &Path, jsonl: &Path) -> String {
    jsonl
        .parent()
        .filter(|p| p != &source_dir)
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| project_folder_for_source_dir(source_dir))
}

fn project_folder_for_source_dir(source_dir: &Path) -> String {
    source_dir
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn resolve_vault_dir(cfg: &SessionExportConfig) -> Result<PathBuf> {
    let raw = cfg
        .vault_dir
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("~/projects/Yarli_KnowledgeBase");
    Ok(expand_tilde(raw))
}

fn expand_tilde(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if let Ok(home) = std::env::var("HOME") {
        if path.starts_with("~") {
            let stripped = path.strip_prefix("~").unwrap_or(path);
            return PathBuf::from(home).join(stripped);
        }
    }
    path.to_path_buf()
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn sanitize_filename(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

fn month_name(month: u32) -> &'static str {
    match month {
        1 => "January",
        2 => "February",
        3 => "March",
        4 => "April",
        5 => "May",
        6 => "June",
        7 => "July",
        8 => "August",
        9 => "September",
        10 => "October",
        11 => "November",
        12 => "December",
        _ => "Unknown",
    }
}

fn align_offset_to_next_line<R: Read + Seek>(reader: &mut R, offset: u64) -> Result<u64> {
    if offset == 0 {
        return Ok(0);
    }
    // If the byte immediately before the offset is a newline, we're aligned.
    reader.seek(SeekFrom::Start(offset.saturating_sub(1)))?;
    let mut buf = [0u8; 1];
    reader.read_exact(&mut buf)?;
    if buf[0] == b'\n' {
        return Ok(offset);
    }
    // Otherwise scan forward for the next newline.
    reader.seek(SeekFrom::Start(offset))?;
    let mut scan = std::io::BufReader::new(reader);
    let mut line = Vec::new();
    match scan.read_until(b'\n', &mut line) {
        Ok(0) | Err(_) => Ok(offset),
        Ok(_) => Ok(offset + line.len() as u64),
    }
}

fn system_time_to_secs_nanos(st: SystemTime) -> (i64, u32) {
    let dur = st
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0));
    (dur.as_secs() as i64, dur.subsec_nanos())
}

fn rsync_to_remote(vault_dir: &Path, target: &str) -> Result<()> {
    let status = std::process::Command::new("rsync")
        .args([
            "-a",
            "--delete-delay",
            "--checksum",
            &format!("{}/", vault_dir.display()),
            target,
        ])
        .status()
        .context("failed to spawn rsync")?;
    if !status.success() {
        anyhow::bail!("rsync exited with status {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_masks_github_token() {
        let text = "token = ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
        let out = redact(text);
        assert!(!out.contains("ghp_"));
        assert!(out.contains("[REDACTED-github-token]"));
    }

    #[test]
    fn redaction_masks_jwt() {
        let text = "auth: eyJhbGciOiJIUzI1NiIs.eyJzdWIiOiIxMjM0.abc123";
        let out = redact(text);
        assert!(!out.contains("eyJhbGci"));
        assert!(out.contains("[REDACTED-jwt]"));
    }

    #[test]
    fn format_user_entry_renders_content() {
        let v = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": "hello" },
        });
        assert_eq!(format_entry(&v).unwrap(), "**User:** hello\n");
    }

    #[test]
    fn format_system_entry_skipped() {
        let v = serde_json::json!({ "type": "system-reminder", "content": "x" });
        assert!(format_entry(&v).is_none());
    }

    #[test]
    fn format_assistant_tool_call_bulleted() {
        let v = serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": "ok",
                "tool_calls": [{ "function": { "name": "Bash", "arguments": "{}" } }]
            },
        });
        let out = format_entry(&v).unwrap();
        assert!(out.contains("**Assistant:** ok"));
        assert!(out.contains("- **Tool call:** `Bash`"));
    }

    #[test]
    fn split_respects_char_boundaries() {
        let (a, b) = split_at_char_boundary("abc🙂def", 5);
        assert_eq!(a, "abc");
        assert_eq!(b, "🙂def");
    }

    #[test]
    fn sanitize_filename_cleans_special_chars() {
        assert_eq!(sanitize_filename("a/b:c"), "a-b-c");
    }

    #[test]
    fn export_cli_sessions_writes_redacted_markdown() {
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();
        let source_dir = tmp.path().join("projects");
        let vault_dir = tmp.path().join("vault");
        fs::create_dir_all(&source_dir).unwrap();

        let session_file = source_dir.join("sess-123.jsonl");
        let mut f = fs::File::create(&session_file).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","timestamp":"2026-07-19T12:00:00Z","message":{{"role":"user","content":"token = ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","timestamp":"2026-07-19T12:01:00Z","message":{{"role":"assistant","content":"ok"}}}}"#
        )
        .unwrap();

        let mut config = crate::config::FleetConfig::default();
        config.session_export.enabled = true;
        config.session_export.vault_dir = Some(vault_dir.to_string_lossy().to_string());
        config.session_export.source_dirs = vec![source_dir.to_string_lossy().to_string()];
        config.session_export.computer_name = Some("testbox".to_string());

        let result = export_cli_sessions(&config, Some("testbox")).unwrap();
        assert_eq!(result.files_processed, 1);
        assert_eq!(result.sessions_exported, 1);
        assert!(result.redactions > 0);

        let expected = vault_dir
            .join("ForgeFleet")
            .join("sessions")
            .join("projects")
            .join("2026")
            .join("07-July")
            .join("20260719-testbox-sess-123.md");
        assert!(expected.exists(), "expected {expected:?} to exist");

        let content = fs::read_to_string(&expected).unwrap();
        assert!(content.contains("**User:**"));
        assert!(content.contains("[REDACTED-github-token]"));
        assert!(!content.contains("ghp_"));
        assert!(content.contains("**Assistant:** ok"));
    }
}
