//! Log monitor for recurring error signatures in `forgefleetd.log`.
//!
//! Tails the daemon log files configured by [`LogMonitoringConfig`] (default:
//! `~/.forgefleet/logs/forgefleetd.log` and friends), detects `error!` /
//! `warn!` output lines, and counts recurrence per signature via
//! [`LogSignatureTracker`]. When a signature crosses the configured recurrence
//! threshold inside the lookback window, it is enqueued into the existing
//! self-heal pipeline: an `INSERT ... ON CONFLICT (dedup_signature) DO NOTHING`
//! into `fleet_tasks` (task_class `self_heal`, tier `T2`), falling back to
//! [`rearm_self_heal_task`] when a previously processed signature recurs —
//! the same single-flight pattern as `leader_tick::scan_interaction_errors`.
//!
//! Unlike the leader-gated `log_analysis_worker`, this monitor is per-node:
//! every daemon watches its *own* `forgefleetd.log`. Cross-node duplicates are
//! absorbed by the unique `dedup_signature` constraint in the DB.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sqlx::PgPool;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::LogMonitoringConfig;
use crate::ha::self_heal::{
    rearm_self_heal_task, self_heal_priority_for_tier, self_heal_task_status,
};
use crate::log_signature::{LogSignature, LogSignatureTracker};

/// Severity of a matched log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Error,
    Warn,
}

/// How many leading whitespace-separated tokens are searched for a level
/// marker. Tracing-formatted lines put the level right after the timestamp,
/// so a small window avoids false positives on message text like
/// "completed with 0 errors".
const LEVEL_TOKEN_SCAN_LIMIT: usize = 8;

/// Detect an `error!`/`warn!` log line.
///
/// Returns the level and the canonical tail of the line starting at the level
/// token, which strips the variable timestamp prefix so recurring identical
/// errors hash to the same signature.
pub fn detect_log_level(line: &str) -> Option<(LogLevel, &str)> {
    let mut offset = 0usize;
    for _ in 0..LEVEL_TOKEN_SCAN_LIMIT {
        let rest = &line[offset..];
        let start = offset + (rest.len() - rest.trim_start().len());
        if start >= line.len() {
            break;
        }
        let token_end = line[start..]
            .find(char::is_whitespace)
            .map(|i| start + i)
            .unwrap_or(line.len());
        if let Some(level) = level_from_token(&line[start..token_end]) {
            return Some((level, line[start..].trim_end()));
        }
        offset = token_end;
    }
    None
}

/// Classify one token as a level marker. Accepts tracing output levels
/// (`ERROR`, `WARN`, `WARNING`, optionally bracketed) and macro-source
/// spellings (`error!`, `warn!`).
fn level_from_token(token: &str) -> Option<LogLevel> {
    let bare = token.trim_matches(|c: char| matches!(c, '[' | ']' | '(' | ')' | ':' | ','));
    if bare.eq_ignore_ascii_case("error") || bare.eq_ignore_ascii_case("error!") {
        Some(LogLevel::Error)
    } else if bare.eq_ignore_ascii_case("warn")
        || bare.eq_ignore_ascii_case("warning")
        || bare.eq_ignore_ascii_case("warn!")
    {
        Some(LogLevel::Warn)
    } else {
        None
    }
}

/// Per-file tail cursor: byte offset of the next unread byte plus any trailing
/// partial line carried over to the next poll.
#[derive(Debug, Default, Clone)]
struct TailCursor {
    offset: u64,
    partial: String,
    initialized: bool,
}

/// Read the lines appended to `path` since the cursor's last position.
///
/// Handles rotation/truncation (file shrank below the cursor → restart from
/// the top of the new file) and incomplete trailing lines (carried in the
/// cursor until the newline arrives).
fn read_new_lines(
    path: &Path,
    cursor: &mut TailCursor,
    start_at_end: bool,
) -> std::io::Result<Vec<String>> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();

    if !cursor.initialized {
        cursor.initialized = true;
        cursor.offset = if start_at_end { len } else { 0 };
        cursor.partial.clear();
    }
    if len < cursor.offset {
        cursor.offset = 0;
        cursor.partial.clear();
    }
    if len == cursor.offset {
        return Ok(Vec::new());
    }

    file.seek(SeekFrom::Start(cursor.offset))?;
    let mut buf = Vec::with_capacity((len - cursor.offset) as usize);
    file.read_to_end(&mut buf)?;
    cursor.offset += buf.len() as u64;

    let chunk = format!("{}{}", cursor.partial, String::from_utf8_lossy(&buf));
    cursor.partial.clear();

    let mut lines = Vec::new();
    let mut remainder = chunk.as_str();
    while let Some(nl) = remainder.find('\n') {
        lines.push(remainder[..nl].trim_end_matches('\r').to_string());
        remainder = &remainder[nl + 1..];
    }
    cursor.partial = remainder.to_string();
    Ok(lines)
}

/// Result of one poll over the configured log files.
#[derive(Debug, Default)]
pub struct PollOutcome {
    /// Signatures that crossed the recurrence threshold this window and have
    /// not yet been reported.
    pub recurring: Vec<LogSignature>,
    pub lines_scanned: usize,
    pub lines_matched: usize,
}

/// Summary of one `run_once` pass (poll + self-heal enqueue).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LogMonitorReport {
    pub lines_scanned: usize,
    pub lines_matched: usize,
    pub recurring: usize,
    /// Novel signatures inserted into `fleet_tasks`.
    pub enqueued: usize,
    /// Terminal signatures re-armed after the cooldown.
    pub rearmed: usize,
}

/// Outcome of enqueueing one recurring signature into the self-heal pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// A new `self_heal` task row was created.
    Inserted,
    /// An existing terminal task was re-armed back to `detected`.
    Rearmed,
    /// The signature is already tracked by an active task; nothing to do.
    AlreadyTracked,
}

/// Tails the configured daemon logs and feeds recurring error signatures into
/// the self-heal pipeline.
pub struct LogMonitor {
    config: LogMonitoringConfig,
    tracker: LogSignatureTracker,
    cursors: HashMap<PathBuf, TailCursor>,
    /// Signatures already reported this recurrence window.
    reported: HashSet<String>,
    window: Duration,
    window_started: Instant,
    start_at_end: bool,
}

impl LogMonitor {
    pub fn new(config: LogMonitoringConfig) -> Self {
        let window = Duration::from_secs(config.recurrence_window_secs.max(1));
        Self {
            config,
            tracker: LogSignatureTracker::new(),
            cursors: HashMap::new(),
            reported: HashSet::new(),
            window,
            window_started: Instant::now(),
            start_at_end: true,
        }
    }

    pub fn from_env() -> Self {
        Self::new(LogMonitoringConfig::from_env())
    }

    /// Override the recurrence window (mainly for tests).
    pub fn with_recurrence_window(mut self, window: Duration) -> Self {
        self.window = window.max(Duration::from_millis(1));
        self
    }

    /// Process files from the beginning instead of tail-only. Useful for tests
    /// and for catching up on errors logged before the monitor started.
    pub fn with_read_from_start(mut self) -> Self {
        self.start_at_end = false;
        self
    }

    /// When the lookback window elapses, forget all recurrence counts so the
    /// threshold means "N occurrences within the window", not "N ever".
    fn maybe_reset_window(&mut self) {
        if self.window_started.elapsed() >= self.window {
            self.tracker.drain();
            self.reported.clear();
            self.window_started = Instant::now();
        }
    }

    /// Read newly appended lines from every configured log file, observe
    /// matching `error!`/`warn!` lines, and return the signatures that crossed
    /// the recurrence threshold. Returned signatures are marked reported and
    /// will not be returned again within the current window.
    pub fn poll(&mut self) -> PollOutcome {
        self.maybe_reset_window();
        let mut out = PollOutcome::default();

        for path in self.config.log_paths.clone() {
            let cursor = self.cursors.entry(path.clone()).or_default();
            let lines = match read_new_lines(&path, cursor, self.start_at_end) {
                Ok(lines) => lines,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    tracing::debug!(
                        path = %path.display(),
                        error = %err,
                        "log_monitor: failed to read log file"
                    );
                    continue;
                }
            };
            for line in lines {
                out.lines_scanned += 1;
                if let Some((_level, canonical)) = detect_log_level(&line) {
                    out.lines_matched += 1;
                    self.tracker.observe(canonical);
                }
            }
        }

        let threshold = u64::from(self.config.recurrence_threshold.max(1));
        for sig in self.tracker.signatures() {
            if sig.count >= threshold && self.reported.insert(sig.signature.clone()) {
                out.recurring.push(sig);
            }
        }
        out
    }

    /// One monitor pass: poll the log files and enqueue every newly recurring
    /// signature into the self-heal pipeline.
    pub async fn run_once(&mut self, pg: &PgPool) -> Result<LogMonitorReport, sqlx::Error> {
        let outcome = self.poll();
        let mut report = LogMonitorReport {
            lines_scanned: outcome.lines_scanned,
            lines_matched: outcome.lines_matched,
            recurring: outcome.recurring.len(),
            ..Default::default()
        };

        for sig in &outcome.recurring {
            match enqueue_recurring_log_signature(pg, sig).await? {
                EnqueueOutcome::Inserted => {
                    report.enqueued += 1;
                    tracing::info!(
                        error_signature = %sig.signature,
                        count = sig.count,
                        canonical = %sig.canonical_text,
                        "log_monitor: enqueued recurring log error for self-heal"
                    );
                }
                EnqueueOutcome::Rearmed => {
                    report.rearmed += 1;
                    tracing::info!(
                        error_signature = %sig.signature,
                        count = sig.count,
                        "log_monitor: re-armed recurring log error for self-heal"
                    );
                }
                EnqueueOutcome::AlreadyTracked => {
                    tracing::debug!(
                        error_signature = %sig.signature,
                        "log_monitor: signature already tracked by self-heal; skipping"
                    );
                }
            }
        }

        Ok(report)
    }

    /// Spawn the background tail loop. No leader gating: each node monitors
    /// its own daemon log; the DB dedup constraint absorbs fleet-wide
    /// duplicates.
    pub fn spawn(mut self, pg: PgPool, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            if !self.config.enabled {
                tracing::debug!("log_monitor: disabled (FF_AGENT_LOG_MONITOR_ENABLED != true)");
                return;
            }
            let mut ticker =
                tokio::time::interval(Duration::from_secs(self.config.poll_interval_secs.max(1)));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        match self.run_once(&pg).await {
                            Ok(report) if report.enqueued + report.rearmed > 0 => {
                                tracing::info!(
                                    scanned = report.lines_scanned,
                                    matched = report.lines_matched,
                                    enqueued = report.enqueued,
                                    rearmed = report.rearmed,
                                    "log_monitor tick"
                                );
                            }
                            Ok(report) => {
                                tracing::debug!(
                                    scanned = report.lines_scanned,
                                    matched = report.lines_matched,
                                    "log_monitor tick"
                                );
                            }
                            Err(err) => {
                                tracing::warn!(error = %err, "log_monitor tick failed");
                            }
                        }
                    }
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            break;
                        }
                    }
                }
            }
        })
    }
}

/// Enqueue one recurring log signature into the self-heal pipeline.
///
/// Mirrors `leader_tick::scan_interaction_errors`: insert a `self_heal` task
/// with `ON CONFLICT (dedup_signature) DO NOTHING` so an in-flight signature is
/// never duplicated, then fall back to [`rearm_self_heal_task`] so a signature
/// already processed to a terminal state is re-armed after its cooldown.
pub async fn enqueue_recurring_log_signature(
    pg: &PgPool,
    sig: &LogSignature,
) -> Result<EnqueueOutcome, sqlx::Error> {
    let report_count = i32::try_from(sig.count).unwrap_or(i32::MAX);
    let inserted = sqlx::query(
        "INSERT INTO fleet_tasks \
            (id, task_type, summary, payload, priority, status, created_at, task_class, dedup_signature) \
         VALUES ( \
            gen_random_uuid(), \
            'self_heal_writer', \
            format('self_heal_writer: %s', $1), \
            jsonb_build_object( \
                'bug_signature', $1, \
                'tier', 'T2', \
                'status', 'detected', \
                'report_count', $2, \
                'attempts', 0, \
                'source', 'forgefleetd_log', \
                'error_text', $3 \
            ), \
            $4, \
            $5, \
            NOW(), \
            'self_heal', \
            $1 \
         ) \
         ON CONFLICT (dedup_signature) WHERE dedup_signature IS NOT NULL DO NOTHING",
    )
    .bind(&sig.signature)
    .bind(report_count)
    .bind(&sig.canonical_text)
    .bind(self_heal_priority_for_tier("T2"))
    .bind(self_heal_task_status("detected"))
    .execute(pg)
    .await?;

    if inserted.rows_affected() > 0 {
        return Ok(EnqueueOutcome::Inserted);
    }
    if rearm_self_heal_task(pg, &sig.signature, "T2", report_count, None).await? {
        Ok(EnqueueOutcome::Rearmed)
    } else {
        Ok(EnqueueOutcome::AlreadyTracked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn test_config(paths: Vec<PathBuf>, threshold: u32) -> LogMonitoringConfig {
        LogMonitoringConfig {
            enabled: true,
            log_paths: paths,
            recurrence_threshold: threshold,
            recurrence_window_secs: 300,
            poll_interval_secs: 1,
            notification_channels: Vec::new(),
        }
    }

    #[test]
    fn detects_tracing_error_and_warn_lines() {
        let line = "2026-07-19T10:00:00.123456Z ERROR ff_agent::dispatcher: pool timed out";
        let (level, canonical) = detect_log_level(line).expect("error line");
        assert_eq!(level, LogLevel::Error);
        assert_eq!(canonical, "ERROR ff_agent::dispatcher: pool timed out");

        let line = "2026-07-19T10:00:00Z  WARN ff_agent::leader: lease expired";
        let (level, canonical) = detect_log_level(line).expect("warn line");
        assert_eq!(level, LogLevel::Warn);
        assert_eq!(canonical, "WARN ff_agent::leader: lease expired");
    }

    #[test]
    fn detects_macro_and_bracketed_spellings() {
        assert_eq!(
            detect_log_level("error! failed to bind port").map(|(l, _)| l),
            Some(LogLevel::Error)
        );
        assert_eq!(
            detect_log_level("warn! disk almost full").map(|(l, _)| l),
            Some(LogLevel::Warn)
        );
        assert_eq!(
            detect_log_level("2026-07-19 [ERROR] boom").map(|(l, _)| l),
            Some(LogLevel::Error)
        );
        assert_eq!(
            detect_log_level("2026-07-19 [WARNING] careful").map(|(l, _)| l),
            Some(LogLevel::Warn)
        );
    }

    #[test]
    fn ignores_non_error_lines_and_late_tokens() {
        assert!(detect_log_level("2026-07-19T10:00:00Z INFO ff_agent: ready").is_none());
        assert!(detect_log_level("").is_none());
        // "errors" as message text is not a level token.
        assert!(detect_log_level("INFO request completed with 0 errors").is_none());
        // A level word past the token scan window is message text, not a level.
        let late = format!("{} ERROR too late", "tok ".repeat(LEVEL_TOKEN_SCAN_LIMIT));
        assert!(detect_log_level(&late).is_none());
    }

    #[test]
    fn poll_tails_only_new_lines_and_counts_recurrence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("forgefleetd.log");
        std::fs::write(
            &path,
            "2026-07-19T10:00:00Z ERROR ff: pool timed out\n\
             2026-07-19T10:00:01Z INFO ff: ready\n",
        )
        .expect("seed log");

        let mut monitor =
            LogMonitor::new(test_config(vec![path.clone()], 2)).with_read_from_start();

        let out = monitor.poll();
        assert_eq!(out.lines_scanned, 2);
        assert_eq!(out.lines_matched, 1);
        assert!(out.recurring.is_empty(), "below threshold");

        // Append a recurrence (different timestamp, same canonical error).
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open for append");
        writeln!(f, "2026-07-19T10:05:00Z ERROR ff: pool timed out").expect("append");
        drop(f);

        let out = monitor.poll();
        assert_eq!(out.lines_scanned, 1, "only the appended line is read");
        assert_eq!(out.recurring.len(), 1);
        assert_eq!(out.recurring[0].count, 2);
        assert_eq!(out.recurring[0].canonical_text, "ERROR ff: pool timed out");

        // Already reported this window: not returned again.
        let out = monitor.poll();
        assert!(out.recurring.is_empty());
    }

    #[test]
    fn poll_holds_partial_lines_until_newline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("forgefleetd.log");
        std::fs::write(&path, "ERROR ff: half a li").expect("seed log");

        let mut monitor =
            LogMonitor::new(test_config(vec![path.clone()], 1)).with_read_from_start();
        let out = monitor.poll();
        assert_eq!(out.lines_scanned, 0, "incomplete line is held back");

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open for append");
        writeln!(f, "ne").expect("complete the line");
        drop(f);

        let out = monitor.poll();
        assert_eq!(out.lines_scanned, 1);
        assert_eq!(out.recurring.len(), 1);
        assert_eq!(out.recurring[0].canonical_text, "ERROR ff: half a line");
    }

    #[test]
    fn poll_resets_cursor_on_rotation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("forgefleetd.log");
        std::fs::write(&path, "ERROR ff: before rotation and some padding\n").expect("seed log");

        let mut monitor =
            LogMonitor::new(test_config(vec![path.clone()], 1)).with_read_from_start();
        assert_eq!(monitor.poll().lines_scanned, 1);

        // Rotation: the file is replaced with shorter content.
        std::fs::write(&path, "ERROR ff: after rotation\n").expect("rotate log");
        let out = monitor.poll();
        assert_eq!(out.lines_scanned, 1);
        assert_eq!(out.lines_matched, 1);
    }

    #[test]
    fn poll_skips_missing_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.log");
        let mut monitor = LogMonitor::new(test_config(vec![path], 1)).with_read_from_start();
        let out = monitor.poll();
        assert_eq!(out.lines_scanned, 0);
        assert!(out.recurring.is_empty());
    }

    #[test]
    fn window_reset_forgets_counts_and_reports() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("forgefleetd.log");
        std::fs::write(&path, "ERROR ff: flappy failure\n").expect("seed log");

        let mut monitor = LogMonitor::new(test_config(vec![path.clone()], 1))
            .with_read_from_start()
            .with_recurrence_window(Duration::from_millis(10));

        let out = monitor.poll();
        assert_eq!(out.recurring.len(), 1);

        std::thread::sleep(Duration::from_millis(20));

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open for append");
        writeln!(f, "ERROR ff: flappy failure").expect("append");
        drop(f);

        // New window: the signature counts (and is reported) from scratch.
        let out = monitor.poll();
        assert_eq!(out.recurring.len(), 1);
        assert_eq!(out.recurring[0].count, 1);
    }

    // ── DB tests (skipped when no Postgres is configured, e.g. in CI) ───────

    fn temp_db_urls() -> (String, String, String) {
        let base_url = std::env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
            .expect("FORGEFLEET_POSTGRES_URL or FORGEFLEET_DATABASE_URL must be set for DB tests");
        let (prefix, _) = base_url
            .rsplit_once('/')
            .expect("database URL must end with /<db>");
        let db_name = format!("ff_log_monitor_{}", uuid::Uuid::new_v4().simple());
        (
            format!("{prefix}/postgres"),
            format!("{prefix}/{db_name}"),
            db_name,
        )
    }

    async fn create_temp_db() -> (sqlx::PgPool, sqlx::PgPool, String) {
        let (admin_url, db_url, db_name) = temp_db_urls();
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&admin_url)
            .await
            .expect("connect admin db");
        sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
            .execute(&admin)
            .await
            .expect("create temp db");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect(&db_url)
            .await
            .expect("connect temp db");
        sqlx::raw_sql(
            "CREATE EXTENSION IF NOT EXISTS pgcrypto;
             CREATE TABLE fleet_tasks (
                 id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                 task_type TEXT NOT NULL,
                 summary TEXT NOT NULL,
                 payload JSONB NOT NULL DEFAULT '{}'::jsonb,
                 priority INT NOT NULL DEFAULT 50,
                 status TEXT NOT NULL DEFAULT 'pending',
                 created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                 completed_at TIMESTAMPTZ,
                 task_class TEXT,
                 dedup_signature TEXT
             );
             CREATE UNIQUE INDEX idx_fleet_tasks_dedup_signature
                 ON fleet_tasks (dedup_signature)
                 WHERE dedup_signature IS NOT NULL;",
        )
        .execute(&pool)
        .await
        .expect("create minimal fleet_tasks schema");
        (admin, pool, db_name)
    }

    async fn drop_temp_db(admin: sqlx::PgPool, pool: sqlx::PgPool, db_name: &str) {
        pool.close().await;
        sqlx::query(
            "SELECT pg_terminate_backend(pid)
               FROM pg_stat_activity
              WHERE datname = $1
                AND pid <> pg_backend_pid()",
        )
        .bind(db_name)
        .execute(&admin)
        .await
        .expect("terminate temp db sessions");
        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db_name}\""))
            .execute(&admin)
            .await
            .expect("drop temp db");
        admin.close().await;
    }

    fn sample_signature(sig: &str, count: u64) -> LogSignature {
        let now = chrono::Utc::now();
        LogSignature {
            signature: sig.to_string(),
            canonical_text: "ERROR ff: pool timed out".to_string(),
            count,
            first_seen: now,
            last_seen: now,
        }
    }

    #[tokio::test]
    async fn enqueue_inserts_then_dedups_then_rearms() {
        if std::env::var("FORGEFLEET_POSTGRES_URL").is_err()
            && std::env::var("FORGEFLEET_DATABASE_URL").is_err()
        {
            eprintln!(
                "skipping enqueue_recurring_log_signature DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        }
        let (admin, pool, db_name) = create_temp_db().await;

        let sig = sample_signature("log-sig-1", 5);
        let outcome = enqueue_recurring_log_signature(&pool, &sig)
            .await
            .expect("first enqueue");
        assert_eq!(outcome, EnqueueOutcome::Inserted);

        let row = sqlx::query(
            "SELECT status, priority, task_class,
                    payload->>'tier' AS tier,
                    payload->>'source' AS source,
                    payload->>'error_text' AS error_text,
                    (payload->>'report_count')::int AS report_count
               FROM fleet_tasks
              WHERE dedup_signature = $1",
        )
        .bind(&sig.signature)
        .fetch_one(&pool)
        .await
        .expect("fetch enqueued task");
        use sqlx::Row;
        assert_eq!(row.get::<String, _>("status"), "pending");
        assert_eq!(row.get::<i32, _>("priority"), 80);
        assert_eq!(row.get::<String, _>("task_class"), "self_heal");
        assert_eq!(row.get::<String, _>("tier"), "T2");
        assert_eq!(row.get::<String, _>("source"), "forgefleetd_log");
        assert_eq!(
            row.get::<String, _>("error_text"),
            "ERROR ff: pool timed out"
        );
        assert_eq!(row.get::<i32, _>("report_count"), 5);

        // Second enqueue while the task is active: single-flight, no-op.
        let outcome = enqueue_recurring_log_signature(&pool, &sig)
            .await
            .expect("second enqueue");
        assert_eq!(outcome, EnqueueOutcome::AlreadyTracked);

        // Mark the task terminal and cooled down: the next enqueue re-arms it.
        sqlx::query(
            "UPDATE fleet_tasks
                SET status = 'completed',
                    completed_at = NOW() - INTERVAL '1 hour'
              WHERE dedup_signature = $1",
        )
        .bind(&sig.signature)
        .execute(&pool)
        .await
        .expect("mark task terminal");

        let outcome = enqueue_recurring_log_signature(&pool, &sig)
            .await
            .expect("third enqueue");
        assert_eq!(outcome, EnqueueOutcome::Rearmed);

        let status: String =
            sqlx::query_scalar("SELECT status FROM fleet_tasks WHERE dedup_signature = $1")
                .bind(&sig.signature)
                .fetch_one(&pool)
                .await
                .expect("fetch rearmed status");
        assert_eq!(status, "pending");

        drop_temp_db(admin, pool, &db_name).await;
    }
}
