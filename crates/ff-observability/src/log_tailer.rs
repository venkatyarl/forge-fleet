//! Log tailer — follow `forgefleetd.log` on disk and stream parsed entries.
//!
//! Where [`crate::log_ingest`] receives log entries pushed over channels, this
//! module handles the *pull* side: it tails a log file on the local node
//! (typically `~/.forgefleet/logs/forgefleetd*.log`), turning freshly-appended
//! lines into structured [`LogEntry`] values with a normalized timestamp and a
//! parsed [`LogLevel`].
//!
//! It is deliberately dependency-light — plain [`std::fs::File`] + [`Seek`], no
//! external filesystem-watch crate — so it works identically on every fleet
//! node (Linux + macOS). Two things make it robust for long-running daemons:
//!
//! * **Ring buffer.** Ingested entries are kept in a bounded [`LogRingBuffer`]
//!   so a caller can pull "recent" context without unbounded memory growth.
//! * **Rotation-safe.** [`tracing_appender`]-style daily rotation renames the
//!   active file out from under an open handle. The tailer detects this (inode
//!   change on Unix, or a file that shrank below the read cursor) and reopens
//!   the path, resuming from the start of the fresh file so no lines are lost.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::log_ingest::LogLevel;

// ─── Log Entry ───────────────────────────────────────────────────────────────

/// A single log line parsed off a tailed file.
///
/// The field set is intentionally minimal — `level`, `message`, `source`,
/// `timestamp` — for the self-heal / log-follow use case. For the richer,
/// channel-ingested representation (node, component, trace ids, structured
/// fields) see [`crate::log_ingest::LogEntry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// Parsed severity level.
    pub level: LogLevel,
    /// The log message body.
    pub message: String,
    /// Where the line came from (log target/module, or the file's basename).
    pub source: String,
    /// Emission time, normalized to UTC (falls back to receive time).
    pub timestamp: DateTime<Utc>,
}

impl LogEntry {
    /// Construct an entry, defaulting the source/timestamp when unknown.
    fn build(
        level: LogLevel,
        message: impl Into<String>,
        source: Option<String>,
        timestamp: Option<DateTime<Utc>>,
        default_source: &str,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            level,
            message: message.into(),
            source: source.unwrap_or_else(|| default_source.to_string()),
            timestamp: timestamp.unwrap_or(now),
        }
    }
}

// ─── Ring Buffer ─────────────────────────────────────────────────────────────

/// Bounded FIFO ring buffer of [`LogEntry`] values.
///
/// Unlike [`crate::log_ingest::LogBuffer`] this is a plain (non-shared,
/// non-async) [`VecDeque`] owned by a single [`LogTailer`]; the tailer runs on
/// one polling task so no interior locking is required.
#[derive(Debug, Clone)]
pub struct LogRingBuffer {
    entries: VecDeque<LogEntry>,
    capacity: usize,
}

impl LogRingBuffer {
    /// Create a ring buffer holding at most `capacity` entries (min 1).
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Push an entry, evicting the oldest once at capacity.
    pub fn push(&mut self, entry: LogEntry) {
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    /// Number of buffered entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the buffer holds no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Snapshot of the most recent `n` entries (oldest-first).
    pub fn recent(&self, n: usize) -> Vec<LogEntry> {
        let start = self.entries.len().saturating_sub(n);
        self.entries.iter().skip(start).cloned().collect()
    }

    /// Snapshot of every buffered entry (oldest-first).
    pub fn snapshot(&self) -> Vec<LogEntry> {
        self.entries.iter().cloned().collect()
    }
}

// ─── Tailer ──────────────────────────────────────────────────────────────────

/// Tails a single log file, emitting parsed [`LogEntry`] values as lines are
/// appended, and mirroring them into an internal [`LogRingBuffer`].
///
/// ```no_run
/// use ff_observability::log_tailer::LogTailer;
///
/// let mut tailer = LogTailer::new("/home/me/.forgefleet/logs/forgefleetd.log");
/// // Poll periodically (e.g. from a daemon loop):
/// let fresh = tailer.poll().unwrap();
/// for entry in fresh {
///     println!("[{}] {}", entry.level, entry.message);
/// }
/// ```
#[derive(Debug)]
pub struct LogTailer {
    path: PathBuf,
    /// Fallback source label for lines that don't carry their own target.
    source: String,
    /// Currently-open handle (None while the file is missing mid-rotation).
    file: Option<File>,
    /// Byte offset of the next unread line in the current file.
    pos: u64,
    /// Inode of the open file, used to detect rotation (Unix only).
    inode: Option<u64>,
    /// Whether we've opened the file at least once (distinguishes first-open
    /// positioning from a rotation reopen).
    opened_once: bool,
    /// If true, the first open reads from the start of the file rather than
    /// seeking to the end (tail-from-beginning vs `tail -f` semantics).
    follow_from_start: bool,
    buffer: LogRingBuffer,
}

/// Default ring-buffer capacity when unspecified.
const DEFAULT_CAPACITY: usize = 4096;

impl LogTailer {
    /// Tail `path`, starting from the current end of the file (`tail -f`).
    ///
    /// The default source label is the file's basename.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self::with_capacity(path, DEFAULT_CAPACITY)
    }

    /// Tail `path` with an explicit ring-buffer capacity.
    pub fn with_capacity(path: impl AsRef<Path>, capacity: usize) -> Self {
        let path = path.as_ref().to_path_buf();
        let source = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "forgefleetd".to_string());
        Self {
            path,
            source,
            file: None,
            pos: 0,
            inode: None,
            opened_once: false,
            follow_from_start: false,
            buffer: LogRingBuffer::new(capacity),
        }
    }

    /// Read the whole file from the beginning on first open, instead of
    /// seeking to the end. Useful for one-shot ingestion of an existing log.
    pub fn follow_from_start(mut self) -> Self {
        self.follow_from_start = true;
        self
    }

    /// Override the fallback source label used for lines without a target.
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }

    /// Access the internal ring buffer.
    pub fn buffer(&self) -> &LogRingBuffer {
        &self.buffer
    }

    /// Most recent `n` entries seen so far.
    pub fn recent(&self, n: usize) -> Vec<LogEntry> {
        self.buffer.recent(n)
    }

    /// Poll once: reopen if rotated, read every newly-appended complete line,
    /// push each into the ring buffer, and return the fresh entries.
    ///
    /// A partial trailing line (no newline yet) is left unconsumed and picked
    /// up on a later poll once it's complete.
    pub fn poll(&mut self) -> std::io::Result<Vec<LogEntry>> {
        self.ensure_open()?;
        let Some(file) = self.file.as_mut() else {
            // File is absent (mid-rotation); nothing to read this tick.
            return Ok(Vec::new());
        };

        file.seek(SeekFrom::Start(self.pos))?;
        let mut reader = BufReader::new(file);
        let now = Utc::now();
        let mut out = Vec::new();

        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                break; // EOF
            }
            if !line.ends_with('\n') {
                // Incomplete trailing line — don't consume it; re-read next poll.
                break;
            }
            self.pos += n as u64;
            let entry = parse_line(line.trim_end_matches(['\n', '\r']), &self.source, now);
            self.buffer.push(entry.clone());
            out.push(entry);
        }

        Ok(out)
    }

    /// Open the file if needed and detect rotation/truncation.
    ///
    /// Reopens (resetting the read cursor to the start of the new file) when:
    /// the inode changed (rename-and-recreate rotation), or the file shrank
    /// below our cursor (truncation), or we had no open handle.
    fn ensure_open(&mut self) -> std::io::Result<()> {
        let meta = match fs::metadata(&self.path) {
            Ok(m) => m,
            Err(_) => {
                // Missing right now (rotation window); drop the handle and retry.
                self.file = None;
                return Ok(());
            }
        };

        let cur_inode = inode_of_meta(&meta);
        let rotated = self.inode.is_some() && cur_inode.is_some() && cur_inode != self.inode;
        let truncated = meta.len() < self.pos;
        let need_reopen = self.file.is_none() || rotated || truncated;

        if need_reopen {
            let file = File::open(&self.path)?;
            let end = file.metadata()?.len();
            if !self.opened_once {
                // First ever open: tail from end unless asked to read all.
                self.pos = if self.follow_from_start { 0 } else { end };
                self.opened_once = true;
            } else {
                // Reopen after rotation/truncation/gap: read the new file whole.
                self.pos = 0;
            }
            self.inode = inode_of_file(&file);
            self.file = Some(file);
        }

        Ok(())
    }

    /// Consume the tailer and spawn a background task that polls every
    /// `interval` and forwards each parsed entry over an mpsc channel — the
    /// "stream of log entries" surface.
    ///
    /// The task exits when the receiver is dropped. I/O errors are logged and
    /// the loop continues (a transient error, e.g. mid-rotation, shouldn't kill
    /// the tailer).
    pub fn spawn(mut self, interval: Duration) -> tokio::sync::mpsc::Receiver<LogEntry> {
        let (tx, rx) = tokio::sync::mpsc::channel(1024);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                match self.poll() {
                    Ok(entries) => {
                        for entry in entries {
                            if tx.send(entry).await.is_err() {
                                return; // receiver dropped
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(path = %self.path.display(), error = %e, "log tailer poll failed");
                    }
                }
            }
        });
        rx
    }
}

// ─── Parsing ─────────────────────────────────────────────────────────────────

/// Parse a single (newline-stripped) log line into a [`LogEntry`].
///
/// Handles two shapes: structured JSON (as emitted by the tracing JSON layer)
/// and plain text (`<timestamp> <LEVEL> <target>: <message>`), degrading
/// gracefully to `INFO` + whole-line message when nothing parses.
fn parse_line(line: &str, default_source: &str, now: DateTime<Utc>) -> LogEntry {
    let trimmed = line.trim();
    if trimmed.starts_with('{') {
        if let Ok(serde_json::Value::Object(map)) =
            serde_json::from_str::<serde_json::Value>(trimmed)
        {
            return parse_json(&map, default_source, now);
        }
    }
    parse_plain(trimmed, default_source, now)
}

/// Extract fields from a structured JSON log object.
fn parse_json(
    map: &serde_json::Map<String, serde_json::Value>,
    default_source: &str,
    now: DateTime<Utc>,
) -> LogEntry {
    let str_field = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .find_map(|k| map.get(*k))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    };

    let level = str_field(&["level", "lvl", "severity"])
        .map(|s| LogLevel::from_str_loose(&s))
        .unwrap_or(LogLevel::Info);

    // tracing's JSON layer nests the human message under `fields.message`.
    let message = str_field(&["message", "msg", "body"])
        .or_else(|| {
            map.get("fields")
                .and_then(|f| f.get("message"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default();

    let source = str_field(&["target", "source", "module", "logger", "name"]);
    let timestamp = str_field(&["timestamp", "ts", "time", "@timestamp"])
        .as_deref()
        .and_then(normalize_timestamp);

    LogEntry::build(level, message, source, timestamp, default_source, now)
}

/// Best-effort parse of a plain-text log line.
fn parse_plain(line: &str, default_source: &str, now: DateTime<Utc>) -> LogEntry {
    if line.is_empty() {
        return LogEntry::build(LogLevel::Info, "", None, None, default_source, now);
    }

    let mut rest = line;
    let mut timestamp = None;

    // Optional leading timestamp token (RFC3339 / ISO-8601, no internal space).
    if let Some((first, tail)) = split_first_token(rest) {
        if let Some(ts) = normalize_timestamp(first) {
            timestamp = Some(ts);
            rest = tail;
        }
    }

    // Optional level token.
    let mut level = LogLevel::Info;
    if let Some((tok, tail)) = split_first_token(rest) {
        let cleaned = tok.trim_matches(|c| c == '[' || c == ']');
        if is_level_token(cleaned) {
            level = LogLevel::from_str_loose(cleaned);
            rest = tail;
        }
    }

    // Optional `target:` prefix.
    let mut source = None;
    if let Some((tok, tail)) = split_first_token(rest) {
        if let Some(target) = tok.strip_suffix(':') {
            if !target.is_empty() && !target.contains(char::is_whitespace) {
                source = Some(target.to_string());
                rest = tail;
            }
        }
    }

    LogEntry::build(level, rest.trim(), source, timestamp, default_source, now)
}

/// Split off the first whitespace-delimited token, returning `(token, rest)`.
fn split_first_token(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    match s.find(char::is_whitespace) {
        Some(idx) => Some((&s[..idx], s[idx..].trim_start())),
        None => Some((s, "")),
    }
}

/// Whether a token names a known severity level.
fn is_level_token(tok: &str) -> bool {
    matches!(
        tok.to_ascii_lowercase().as_str(),
        "trace" | "debug" | "info" | "warn" | "warning" | "error" | "err" | "fatal"
    )
}

/// Normalize a timestamp string into UTC, trying several common encodings.
fn normalize_timestamp(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // RFC3339 / ISO-8601 with offset (the tracing default).
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }

    // Naive forms assumed to already be UTC.
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
    ] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(Utc.from_utc_datetime(&naive));
        }
    }

    // Epoch seconds / milliseconds.
    if let Ok(num) = s.parse::<i64>() {
        let (secs, nanos) = if s.len() >= 13 {
            (num / 1000, ((num % 1000) * 1_000_000) as u32)
        } else {
            (num, 0)
        };
        if let chrono::LocalResult::Single(dt) = Utc.timestamp_opt(secs, nanos) {
            return Some(dt);
        }
    }

    None
}

// ─── Inode helpers (rotation detection) ──────────────────────────────────────

#[cfg(unix)]
fn inode_of_meta(meta: &fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.ino())
}

#[cfg(not(unix))]
fn inode_of_meta(_meta: &fs::Metadata) -> Option<u64> {
    // Non-Unix: rely on size-shrink detection only.
    None
}

fn inode_of_file(file: &File) -> Option<u64> {
    file.metadata().ok().as_ref().and_then(inode_of_meta)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use uuid::Uuid;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ff-logtailer-{tag}-{}.log", Uuid::new_v4()))
    }

    #[test]
    fn ring_buffer_evicts_oldest() {
        let mut buf = LogRingBuffer::new(2);
        let now = Utc::now();
        for i in 0..3 {
            buf.push(LogEntry::build(
                LogLevel::Info,
                format!("msg{i}"),
                None,
                None,
                "src",
                now,
            ));
        }
        assert_eq!(buf.len(), 2);
        let recent = buf.recent(10);
        assert_eq!(recent[0].message, "msg1");
        assert_eq!(recent[1].message, "msg2");
    }

    #[test]
    fn parses_json_line() {
        let now = Utc::now();
        let line = r#"{"timestamp":"2026-07-21T10:00:00Z","level":"WARN","target":"ff_agent::runner","fields":{"message":"disk low"}}"#;
        let e = parse_line(line, "forgefleetd", now);
        assert_eq!(e.level, LogLevel::Warn);
        assert_eq!(e.message, "disk low");
        assert_eq!(e.source, "ff_agent::runner");
        assert_eq!(
            e.timestamp,
            DateTime::parse_from_rfc3339("2026-07-21T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );
    }

    #[test]
    fn parses_plain_line() {
        let now = Utc::now();
        let line = "2026-07-21T10:00:00Z ERROR ff_core::db: connection refused";
        let e = parse_line(line, "forgefleetd", now);
        assert_eq!(e.level, LogLevel::Error);
        assert_eq!(e.source, "ff_core::db");
        assert_eq!(e.message, "connection refused");
    }

    #[test]
    fn plain_line_without_metadata_defaults() {
        let now = Utc::now();
        let e = parse_line("just a bare message", "forgefleetd", now);
        assert_eq!(e.level, LogLevel::Info);
        assert_eq!(e.source, "forgefleetd");
        assert_eq!(e.message, "just a bare message");
        assert_eq!(e.timestamp, now);
    }

    #[test]
    fn normalize_timestamp_forms() {
        assert!(normalize_timestamp("2026-07-21T10:00:00Z").is_some());
        assert!(normalize_timestamp("2026-07-21 10:00:00.123").is_some());
        assert!(normalize_timestamp("1753092000").is_some());
        assert!(normalize_timestamp("1753092000000").is_some());
        assert!(normalize_timestamp("not-a-time").is_none());
    }

    #[test]
    fn tails_appended_lines() {
        let path = temp_path("append");
        {
            let mut f = File::create(&path).unwrap();
            writeln!(f, "seed line before tail starts").unwrap();
        }
        // Start from end: the seed line must NOT appear.
        let mut tailer = LogTailer::new(&path);
        assert!(tailer.poll().unwrap().is_empty());

        {
            let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(f, "2026-07-21T10:00:00Z INFO ff::a: first").unwrap();
            writeln!(f, "2026-07-21T10:00:01Z ERROR ff::b: second").unwrap();
        }
        let entries = tailer.poll().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].message, "first");
        assert_eq!(entries[1].level, LogLevel::Error);
        assert_eq!(tailer.buffer().len(), 2);

        fs::remove_file(&path).ok();
    }

    #[test]
    fn does_not_emit_partial_line() {
        let path = temp_path("partial");
        File::create(&path).unwrap();
        let mut tailer = LogTailer::new(&path).follow_from_start();

        {
            let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
            write!(f, "no newline yet").unwrap();
        }
        assert!(tailer.poll().unwrap().is_empty());

        {
            let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(f, " now complete").unwrap();
        }
        let entries = tailer.poll().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].message, "no newline yet now complete");

        fs::remove_file(&path).ok();
    }

    #[test]
    fn handles_rotation_by_reopening() {
        let path = temp_path("rotate");
        {
            let mut f = File::create(&path).unwrap();
            writeln!(f, "old file line").unwrap();
        }
        let mut tailer = LogTailer::new(&path);
        assert!(tailer.poll().unwrap().is_empty());

        // Simulate rotation: move the active file aside and create a fresh one.
        let rotated = temp_path("rotate-old");
        fs::rename(&path, &rotated).unwrap();
        {
            let mut f = File::create(&path).unwrap();
            writeln!(f, "2026-07-21T11:00:00Z WARN ff::c: post-rotation").unwrap();
        }

        let entries = tailer.poll().unwrap();
        assert_eq!(entries.len(), 1, "must read the fresh post-rotation file");
        assert_eq!(entries[0].message, "post-rotation");
        assert_eq!(entries[0].level, LogLevel::Warn);

        fs::remove_file(&path).ok();
        fs::remove_file(&rotated).ok();
    }
}
