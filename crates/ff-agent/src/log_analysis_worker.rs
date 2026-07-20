//! Log analysis worker — leader-gated periodic scan of configured log paths.
//!
//! Scans log files for recurring error/warning patterns and creates `ready`
//! work_items in a configurable project so the Pillar-4 scheduler can dispatch
//! remediation. Designed to integrate with the existing self-heal queue system
//! (canonical `work_items` table) rather than inventing a parallel queue.
//!
//! Configuration is read from environment on each tick so operators can tune it
//! without restarting the daemon:
//!   - `FF_LOG_ANALYSIS_INTERVAL_SECS` (default 300)
//!   - `FF_LOG_ANALYSIS_PATHS` comma-separated globs (default: common system + ff logs)
//!   - `FF_LOG_ANALYSIS_PATTERNS` comma-separated keywords (default: ERROR,FATAL,EXCEPTION,WARN)
//!   - `FF_LOG_ANALYSIS_PROJECT_ID` target project (default: `ff-log-analysis`)
//!   - `FF_LOG_ANALYSIS_MIN_RECURRENCE` minimum occurrences before creating a work_item (default 3)
//!   - `FF_LOG_ANALYSIS_TAIL_LINES` lines read from the tail of each file per tick (default 1000)

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use glob::glob;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// Default interval between log analysis scans.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(300);
const DEFAULT_PROJECT_ID: &str = "ff-log-analysis";
const DEFAULT_MIN_RECURRENCE: usize = 3;
const DEFAULT_TAIL_LINES: usize = 1000;
const DEFAULT_PATHS: &[&str] = &["/var/log/**/*.log"];
const DEFAULT_PATTERNS: &[&str] = &["ERROR", "FATAL", "EXCEPTION", "WARN"];

/// Summary of one scan pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ScanReport {
    pub files_scanned: usize,
    pub lines_scanned: usize,
    pub patterns_found: usize,
    pub work_items_created: usize,
}

/// A normalized recurring log pattern.
#[derive(Debug, Clone)]
struct RecurringPattern {
    signature: String,
    normalized: String,
    example: String,
    count: usize,
    last_path: PathBuf,
}

#[derive(Debug, Clone)]
struct LogAnalysisConfig {
    interval: Duration,
    project_id: String,
    log_paths: Vec<String>,
    patterns: Vec<String>,
    min_recurrence: usize,
    tail_lines: usize,
}

impl LogAnalysisConfig {
    fn from_env() -> Self {
        let interval = std::env::var("FF_LOG_ANALYSIS_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_INTERVAL);

        let project_id = std::env::var("FF_LOG_ANALYSIS_PROJECT_ID")
            .unwrap_or_else(|_| DEFAULT_PROJECT_ID.to_string());

        let log_paths = parse_csv_env("FF_LOG_ANALYSIS_PATHS", DEFAULT_PATHS);
        let patterns = parse_csv_env("FF_LOG_ANALYSIS_PATTERNS", DEFAULT_PATTERNS)
            .into_iter()
            .map(|p| p.to_ascii_uppercase())
            .collect();

        let min_recurrence = std::env::var("FF_LOG_ANALYSIS_MIN_RECURRENCE")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(DEFAULT_MIN_RECURRENCE);

        let tail_lines = std::env::var("FF_LOG_ANALYSIS_TAIL_LINES")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(DEFAULT_TAIL_LINES);

        Self {
            interval,
            project_id,
            log_paths,
            patterns,
            min_recurrence,
            tail_lines,
        }
    }

    fn is_enabled(&self) -> bool {
        !self.log_paths.is_empty() && !self.patterns.is_empty()
    }
}

fn parse_csv_env(name: &str, defaults: &[&str]) -> Vec<String> {
    std::env::var(name)
        .ok()
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_else(|| defaults.iter().map(|s| s.to_string()).collect())
}

/// Background worker that periodically scans logs and enqueues work_items.
pub struct LogAnalysisWorker {
    pg: PgPool,
    my_name: String,
    config: LogAnalysisConfig,
}

impl LogAnalysisWorker {
    pub fn new(pg: PgPool, my_name: String) -> Self {
        Self {
            pg,
            my_name,
            config: LogAnalysisConfig::from_env(),
        }
    }

    async fn is_leader(&self) -> bool {
        crate::leader_cache::is_current_leader()
    }

    /// Run one scan pass and create work_items for recurring patterns.
    pub async fn run_once(&self) -> Result<ScanReport> {
        if !self.config.is_enabled() {
            debug!("log_analysis_worker: disabled (no paths or patterns)");
            return Ok(ScanReport::default());
        }

        let mut report = ScanReport::default();
        let mut grouped: HashMap<String, RecurringPattern> = HashMap::new();

        for path_pattern in &self.config.log_paths {
            let expanded = expand_path_pattern(path_pattern);
            for path in expanded {
                match self.scan_file(&path).await {
                    Ok(lines) => {
                        report.files_scanned += 1;
                        report.lines_scanned += lines.len();
                        for line in lines {
                            if let Some(normalized) =
                                normalize_log_line(&line, &self.config.patterns)
                            {
                                let signature = compute_signature(&normalized);
                                grouped
                                    .entry(signature.clone())
                                    .and_modify(|p| {
                                        p.count += 1;
                                        if p.example.len() < line.len() {
                                            p.example = line.clone();
                                        }
                                    })
                                    .or_insert_with(|| RecurringPattern {
                                        signature,
                                        normalized,
                                        example: line,
                                        count: 1,
                                        last_path: path.clone(),
                                    });
                            }
                        }
                    }
                    Err(err) => {
                        debug!(path = %path.display(), error = %err, "log_analysis_worker: failed to scan file");
                    }
                }
            }
        }

        let recurring: Vec<&RecurringPattern> = grouped
            .values()
            .filter(|p| p.count >= self.config.min_recurrence)
            .collect();
        report.patterns_found = recurring.len();

        if !recurring.is_empty() {
            self.ensure_project().await?;
            let created = self.create_work_items(&recurring).await?;
            report.work_items_created = created;
        }

        if report.work_items_created > 0 {
            info!(
                files = report.files_scanned,
                lines = report.lines_scanned,
                patterns = report.patterns_found,
                created = report.work_items_created,
                "log_analysis_worker: scan complete"
            );
        } else {
            debug!(
                files = report.files_scanned,
                lines = report.lines_scanned,
                "log_analysis_worker: no recurring patterns detected"
            );
        }

        Ok(report)
    }

    /// Read up to `tail_lines` from the end of a log file.
    async fn scan_file(&self, path: &Path) -> Result<Vec<String>> {
        let content = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("read {}", path.display()))?;

        let lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
        let total = lines.len();
        if total <= self.config.tail_lines {
            Ok(lines)
        } else {
            Ok(lines
                .into_iter()
                .skip(total - self.config.tail_lines)
                .collect())
        }
    }

    /// Idempotently create the target project so work_item inserts never fail on FK.
    async fn ensure_project(&self) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO projects (id, display_name, default_branch, status) \
             VALUES ($1, $2, 'main', 'active') \
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(&self.config.project_id)
        .bind(format!("Log Analysis ({})", self.config.project_id))
        .execute(&self.pg)
        .await?;
        Ok(())
    }

    /// Create one `ready` work_item per recurring pattern, skipping patterns
    /// already tracked by an open/ready/in_progress work_item.
    async fn create_work_items(&self, patterns: &[&RecurringPattern]) -> Result<usize> {
        let mut created = 0usize;

        for pattern in patterns {
            let existing: Option<(uuid::Uuid,)> = sqlx::query_as(
                "SELECT id FROM work_items \
                 WHERE project_id = $1 \
                   AND status IN ('open', 'ready', 'in_progress') \
                   AND metadata->>'log_signature' = $2 \
                 LIMIT 1",
            )
            .bind(&self.config.project_id)
            .bind(&pattern.signature)
            .fetch_optional(&self.pg)
            .await?;

            if existing.is_some() {
                debug!(signature = %pattern.signature, "log_analysis_worker: pattern already tracked");
                continue;
            }

            let title = truncate(&pattern.normalized, 120);
            let description = format!(
                "Recurring log pattern detected {} time(s).\n\nNormalized:\n{}\n\nExample:\n{}\n\nSource: {}",
                pattern.count,
                pattern.normalized,
                pattern.example,
                pattern.last_path.display()
            );

            let metadata = serde_json::json!({
                "log_signature": pattern.signature,
                "log_pattern": pattern.normalized,
                "log_example": pattern.example,
                "log_source": pattern.last_path.display().to_string(),
                "occurrence_count": pattern.count,
                "detected_by": &self.my_name,
            });

            let id: uuid::Uuid = sqlx::query_scalar(
                "INSERT INTO work_items \
                    (project_id, kind, title, description, status, priority, created_by, metadata) \
                 VALUES ($1, 'log_pattern', $2, $3, 'ready', 'normal', $4, $5) \
                 RETURNING id",
            )
            .bind(&self.config.project_id)
            .bind(&title)
            .bind(&description)
            .bind(&self.my_name)
            .bind(&metadata)
            .fetch_one(&self.pg)
            .await?;

            info!(
                work_item_id = %id,
                signature = %pattern.signature,
                count = pattern.count,
                "log_analysis_worker: created work_item for recurring pattern"
            );
            created += 1;
        }

        Ok(created)
    }

    /// Spawn the background loop. Safe to start on every daemon; the tick is
    /// leader-gated inside `run_once`.
    pub fn spawn(self, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        let interval = self.config.interval;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        if !self.is_leader().await {
                            continue;
                        }
                        match self.run_once().await {
                            Ok(report) => {
                                debug!(
                                    files = report.files_scanned,
                                    lines = report.lines_scanned,
                                    patterns = report.patterns_found,
                                    created = report.work_items_created,
                                    "log_analysis_worker tick"
                                );
                            }
                            Err(err) => {
                                warn!(error = %err, "log_analysis_worker tick failed");
                            }
                        }
                    }
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            info!("log_analysis_worker shutting down");
                            break;
                        }
                    }
                }
            }
        })
    }
}

/// Standalone tick entry point used by the daemon tick registry.
pub async fn run_log_analysis_tick(pg: &PgPool, worker_name: &str) -> Result<usize> {
    let worker = LogAnalysisWorker::new(pg.clone(), worker_name.to_string());
    let report = worker.run_once().await?;
    Ok(report.work_items_created)
}

/// Expand a glob pattern, expanding a leading `~` to the user's home directory.
pub(crate) fn expand_path_pattern(pattern: &str) -> Vec<PathBuf> {
    let expanded = if let Some(rest) = pattern.strip_prefix("~/") {
        dirs::home_dir()
            .map(|h| h.join(rest).to_string_lossy().to_string())
            .unwrap_or_else(|| pattern.to_string())
    } else {
        pattern.to_string()
    };

    match glob(&expanded) {
        Ok(paths) => paths.filter_map(|p| p.ok()).collect(),
        Err(_) => Vec::new(),
    }
}

/// Return a normalized form of the log line if it matches any configured pattern.
///
/// Normalization collapses variable tokens (timestamps, UUIDs, hex, numbers,
/// IPs, paths) so the same underlying message groups together across many
/// occurrences.
pub(crate) fn normalize_log_line(line: &str, patterns: &[String]) -> Option<String> {
    let uppercase = line.to_ascii_uppercase();
    if !patterns.iter().any(|p| uppercase.contains(p)) {
        return None;
    }

    let normalized = replace_tokens(line);
    let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");

    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

/// Replace variable tokens (UUIDs, IPs, hex runs, numbers) with placeholders.
pub(crate) fn replace_tokens(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // Try UUID first: 8-4-4-4-12 hex with dashes.
        if is_uuid_at(bytes, i) {
            out.push_str("<UUID>");
            i += 36;
            continue;
        }

        let c = bytes[i] as char;

        // IPv4 address: digits.digits.digits.digits
        if c.is_ascii_digit() && looks_like_ipv4_at(bytes, i) {
            out.push_str("<IP>");
            i += 1;
            while i < bytes.len() {
                let nc = bytes[i] as char;
                if nc.is_ascii_digit() || nc == '.' || nc == ':' {
                    i += 1;
                } else {
                    break;
                }
            }
            continue;
        }

        // Number run (pure digits, possibly with punctuation). Checked before hex
        // so timestamps and short numeric IDs collapse to <NUM> rather than being
        // left as literal values.
        if c.is_ascii_digit() {
            out.push_str("<NUM>");
            i += 1;
            while i < bytes.len() {
                let nc = bytes[i] as char;
                if nc.is_ascii_digit() || nc == '.' || nc == ',' || nc == ':' || nc == '-' {
                    i += 1;
                } else {
                    break;
                }
            }
            continue;
        }

        // Hex run containing a-f (at least 6 chars) — excludes pure digit runs.
        if c.is_ascii_hexdigit() && c.is_ascii_alphabetic() {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i] as char).is_ascii_hexdigit() {
                i += 1;
            }
            if i - start >= 6 {
                out.push_str("<HEX>");
            } else {
                out.push_str(&s[start..i]);
            }
            continue;
        }

        out.push(c);
        i += 1;
    }

    out
}

fn is_uuid_at(bytes: &[u8], start: usize) -> bool {
    if start + 36 > bytes.len() {
        return false;
    }
    let pattern: &[usize] = &[8, 4, 4, 4, 12];
    let mut idx = start;
    for (seg, &len) in pattern.iter().enumerate() {
        for offset in 0..len {
            if !(bytes[idx + offset] as char).is_ascii_hexdigit() {
                return false;
            }
        }
        idx += len;
        if seg < pattern.len() - 1 {
            if bytes[idx] != b'-' {
                return false;
            }
            idx += 1;
        }
    }
    true
}

fn looks_like_ipv4_at(bytes: &[u8], start: usize) -> bool {
    // Very loose heuristic: digit.digit.digit.digit within next ~15 chars.
    if start + 7 > bytes.len() {
        return false;
    }
    let window = &bytes[start..(start + 16).min(bytes.len())];
    let mut dots = 0;
    let mut digits = 0;
    for &b in window {
        let c = b as char;
        if c.is_ascii_digit() {
            digits += 1;
        } else if c == '.' {
            dots += 1;
        } else if c != ':' {
            break;
        }
    }
    dots >= 3 && digits >= 4
}

pub(crate) fn compute_signature(normalized: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        s.chars().take(max_len).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_groups_similar_lines() {
        let patterns = vec!["ERROR".to_string()];
        let a = "2026-07-19 12:00:00Z host-1 app[1234]: ERROR connection failed to 10.0.0.5:8080";
        let b = "2026-07-19 12:01:00Z host-2 app[5678]: ERROR connection failed to 10.0.0.6:9090";

        let na = normalize_log_line(a, &patterns).unwrap();
        let nb = normalize_log_line(b, &patterns).unwrap();
        assert_eq!(na, nb);
        assert!(na.contains("ERROR"));
        assert!(na.contains("<IP>"));
        assert!(!na.contains("10.0.0.5"));
    }

    #[test]
    fn test_normalize_skips_unmatched_lines() {
        let patterns = vec!["ERROR".to_string()];
        assert!(normalize_log_line("INFO all good", &patterns).is_none());
    }

    #[test]
    fn test_replace_tokens() {
        assert_eq!(replace_tokens("abc 123 def"), "abc <NUM> def");
        assert_eq!(replace_tokens("addr deadbeef01"), "addr <HEX>");
        assert_eq!(replace_tokens("host 1.2.3.4"), "host <IP>");
        assert_eq!(
            replace_tokens("id a1b2c3d4-e5f6-7a8b-9c0d-1e2f3a4b5c6d"),
            "id <UUID>"
        );
    }

    #[test]
    fn test_compute_signature_is_stable() {
        let s = "ERROR connection failed";
        assert_eq!(compute_signature(s), compute_signature(s));
        assert_ne!(compute_signature(s), compute_signature("WARN timeout"));
    }

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghij", 10), "abcdefghij");
        assert_eq!(truncate("abcdefghijk", 10), "abcdefghij…");
    }

    #[test]
    fn test_parse_csv_env() {
        // Defaults.
        let got = parse_csv_env("FF_LOG_ANALYSIS_PATHS_TEST_DEFAULT", DEFAULT_PATHS);
        assert_eq!(got, vec!["/var/log/**/*.log"]);

        // Explicit.
        unsafe {
            std::env::set_var("FF_LOG_ANALYSIS_PATHS_TEST_EXPLICIT", "/a,/b,/c");
        }
        let got = parse_csv_env("FF_LOG_ANALYSIS_PATHS_TEST_EXPLICIT", DEFAULT_PATHS);
        assert_eq!(got, vec!["/a", "/b", "/c"]);
    }
}
