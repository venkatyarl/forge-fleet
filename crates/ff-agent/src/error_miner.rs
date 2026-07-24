//! ErrorMiner — daily aggregation pass over recent errors.
//!
//! Registered in the daemon tick registry (leader-gated, follows the pattern
//! of [`crate::ha::periodic::run_nightly_digest_tick`]). Each pass:
//!
//!   1. Pulls `work_items.last_error` / `ff_interactions.error_text` from the
//!      last 24h, normalizes + signs each one (sha256 of the error-class
//!      token when present, else of the normalized text), and upserts
//!      per-signature counts into `error_signatures`. `count_24h` is a fresh
//!      snapshot of the current window every pass — signatures that drop out
//!      of it get explicitly zeroed, never left stale. `count_total` only
//!      advances by occurrences newer than the signature's last-seen
//!      high-water mark, so the same occurrences are never recounted across
//!      overlapping 24h passes (see [`upsert_error_signatures`]).
//!   2. SSHes into every online fleet node (best-effort, bounded by a
//!      per-node timeout — see [`crate::ssh_opts`] for why daemon-spawned SSH
//!      always needs one) and classifies recent `journalctl` warning/error
//!      lines by their first six normalized words, upserting counts into
//!      `fleet_log_digest` with the same zero-if-absent treatment per node.
//!   3. Auto-files a `bug` work_item for any signature that just crossed
//!      [`AUTO_FILE_COUNT_THRESHOLD`] occurrences in 24h, is still `new`, and
//!      has no open work_item already tracking it — capped at
//!      [`AUTO_FILE_DAILY_CAP`] auto-files per day, fleet-wide. Filing goes
//!      through [`ff_db::pg_create_work_item`], the same validated path `ff
//!      pm create` uses (project-exists check, `created_by` resolution),
//!      rather than an ad hoc `INSERT INTO work_items`.
//!   4. Once per day (at/after [`DIGEST_HOUR_LOCAL`]:00 local, deduped via the
//!      `telegram_messages` row the send records — same trick as the nightly
//!      digest), sends a top-5-by-count Telegram summary.

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Timelike, Utc};
use futures::future::join_all;
use sqlx::PgPool;
use tokio::process::Command;
use tokio::time::timeout;

/// How far back to pull errors / journald lines each pass.
pub const LOOKBACK_HOURS: i64 = 24;
/// A signature must hit this many occurrences in the lookback window before
/// it's eligible for auto-filing.
pub const AUTO_FILE_COUNT_THRESHOLD: i32 = 10;
/// Hard cap on auto-filed bug work_items per day, fleet-wide.
pub const AUTO_FILE_DAILY_CAP: i64 = 3;
/// Risk score stamped on auto-filed bug work_items.
pub const AUTO_FILE_RISK_SCORE: f32 = 60.0;
/// Project auto-filed error-miner bugs land in.
pub const AUTO_FILE_PROJECT_ID: &str = "ff-error-miner";
/// `work_items.created_by` for everything this module files.
pub const CREATED_BY: &str = "error-miner";
/// Local hour (0-23) after which the daily digest becomes due.
pub const DIGEST_HOUR_LOCAL: u32 = 7;

const DIGEST_SESSION_PREFIX: &str = "error-miner-digest";
const JOURNALCTL_CMD: &str =
    "journalctl --user -u forgefleetd -p warning --since -24h --no-pager | tail -200";
const SSH_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_SAMPLES_PER_SIGNATURE: usize = 3;

const OPEN_WORK_ITEM_STATUSES: &[&str] = &[
    "idea",
    "decomposed",
    "ready",
    "claimed",
    "building",
    "in_progress",
    "in_review",
];

fn truncate(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        s.chars().take(max_len).collect::<String>() + "…"
    }
}

// ─── Pure text normalization ─────────────────────────────────────────────────

/// Replace whitespace-delimited path-like tokens (anything containing `/`)
/// with `<PATH>`, lowercasing everything else. Applied before
/// [`crate::log_analysis_worker::replace_tokens`], which already handles
/// UUIDs/IPs/numbers/sha-looking hex runs.
fn strip_paths_and_lowercase(s: &str) -> String {
    s.split(' ')
        .map(|tok| {
            if tok.len() > 1 && tok.contains('/') {
                "<PATH>".to_string()
            } else {
                tok.to_ascii_lowercase()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Normalize free-form error/log text for dedup purposes: lowercased, with
/// UUIDs, filesystem paths, numbers, and sha-looking hex runs replaced by
/// placeholder tokens.
pub fn normalize_error_text(text: &str) -> String {
    let no_paths = strip_paths_and_lowercase(text.trim());
    crate::log_analysis_worker::replace_tokens(&no_paths)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Extract a leading `ErrorClass:` token from free-form error text, e.g.
/// `"ConnectionRefused: could not connect"` -> `Some("ConnectionRefused")`.
/// Only fires on a plausible type/exception-name head (no internal spaces,
/// starts with a letter, carries at least one uppercase char so ordinary
/// lowercase prose like `"note: ..."` doesn't get mistaken for a class);
/// anything else falls back to `None` so the caller signs off the
/// normalized text instead.
pub fn extract_error_class(text: &str) -> Option<String> {
    let first_line = text.lines().next()?.trim();
    let (head, rest) = first_line.split_once(':')?;
    let head = head.trim();
    if head.is_empty() || head.len() > 64 || rest.trim().is_empty() {
        return None;
    }
    let starts_alpha = head.chars().next()?.is_ascii_alphabetic();
    let valid_chars = head
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.'));
    let looks_like_type_name = head.chars().any(|c| c.is_ascii_uppercase());
    (starts_alpha && valid_chars && looks_like_type_name).then(|| head.to_string())
}

/// Compute `(signature, normalized_text, error_class)` for one error record.
/// The signature is sha256 of the error-class token when one is
/// extractable, else sha256 of the normalized text — so differently worded
/// instances of the same exception class still collapse to one signature.
pub fn error_signature(raw_text: &str) -> (String, String, Option<String>) {
    let normalized = normalize_error_text(raw_text);
    let error_class = extract_error_class(raw_text);
    let basis = error_class.clone().unwrap_or_else(|| normalized.clone());
    (
        crate::log_analysis_worker::compute_signature(&basis),
        normalized,
        error_class,
    )
}

/// Classify a journald line by its first six normalized words.
pub fn classify_journal_line(line: &str) -> String {
    normalize_error_text(line)
        .split_whitespace()
        .take(6)
        .collect::<Vec<_>>()
        .join(" ")
}

// ─── Step 1: DB error signatures ─────────────────────────────────────────────

struct MinedError {
    raw_text: String,
    node: Option<String>,
    ts: DateTime<Utc>,
}

async fn pull_recent_errors(pg: &PgPool) -> Result<Vec<MinedError>> {
    let mut out = Vec::new();

    let work_item_rows: Vec<(String, Option<String>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT last_error, assigned_computer, COALESCE(completed_at, started_at, created_at) AS ts \
           FROM work_items \
          WHERE last_error IS NOT NULL AND last_error <> '' \
            AND COALESCE(completed_at, started_at, created_at) >= NOW() - INTERVAL '24 hours'",
    )
    .fetch_all(pg)
    .await
    .context("pull recent work_items.last_error")?;
    out.extend(
        work_item_rows
            .into_iter()
            .map(|(raw_text, node, ts)| MinedError { raw_text, node, ts }),
    );

    let interaction_rows: Vec<(String, Option<String>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT error_text, worker_name, ts \
           FROM ff_interactions \
          WHERE error_text IS NOT NULL AND error_text <> '' \
            AND ts >= NOW() - INTERVAL '24 hours'",
    )
    .fetch_all(pg)
    .await
    .context("pull recent ff_interactions.error_text")?;
    out.extend(
        interaction_rows
            .into_iter()
            .map(|(raw_text, node, ts)| MinedError { raw_text, node, ts }),
    );

    Ok(out)
}

/// Last-seen high-water mark per known signature, used to tell genuinely new
/// occurrences apart from ones already folded into `count_total` on a prior
/// (overlapping) 24h pass.
async fn fetch_last_seen_cursors(pg: &PgPool) -> Result<HashMap<String, DateTime<Utc>>> {
    let rows: Vec<(String, DateTime<Utc>)> =
        sqlx::query_as("SELECT signature, last_seen_at FROM error_signatures")
            .fetch_all(pg)
            .await
            .context("fetch error_signatures last-seen cursors")?;
    Ok(rows.into_iter().collect())
}

#[derive(Default)]
struct SignatureAgg {
    error_class: Option<String>,
    normalized_text: String,
    samples: Vec<String>,
    nodes: BTreeSet<String>,
    /// Snapshot count within the current 24h window — replaces (never adds
    /// to) the stored `count_24h` on upsert.
    count_24h: i32,
    /// Occurrences newer than this signature's stored `last_seen_at` cursor
    /// (or the window start, for a signature never seen before) — the only
    /// amount ever added to `count_total`.
    new_since_cursor: i32,
    max_ts: Option<DateTime<Utc>>,
}

fn aggregate_signatures(
    errors: &[MinedError],
    cursors: &HashMap<String, DateTime<Utc>>,
    window_start: DateTime<Utc>,
) -> HashMap<String, SignatureAgg> {
    let mut agg: HashMap<String, SignatureAgg> = HashMap::new();
    for e in errors {
        let (signature, normalized, error_class) = error_signature(&e.raw_text);
        let cursor = cursors.get(&signature).copied().unwrap_or(window_start);
        let entry = agg.entry(signature).or_default();
        entry.error_class = error_class;
        entry.normalized_text = normalized;
        entry.count_24h += 1;
        if e.ts > cursor {
            entry.new_since_cursor += 1;
        }
        entry.max_ts = Some(entry.max_ts.map_or(e.ts, |m| m.max(e.ts)));
        if entry.samples.len() < MAX_SAMPLES_PER_SIGNATURE {
            entry.samples.push(truncate(e.raw_text.trim(), 500));
        }
        if let Some(node) = &e.node {
            entry.nodes.insert(node.clone());
        }
    }
    agg
}

/// Upsert this pass's signatures. `count_24h` is set to the fresh window
/// snapshot (not added to); `count_total` advances only by
/// `new_since_cursor`, so re-scanning the same 24h-overlapping occurrences on
/// every 30-min pass never double-counts them.
async fn upsert_error_signatures(pg: &PgPool, agg: &HashMap<String, SignatureAgg>) -> Result<()> {
    for (signature, a) in agg {
        let samples = serde_json::to_value(&a.samples).context("serialize samples")?;
        let nodes = serde_json::to_value(a.nodes.iter().collect::<Vec<_>>())
            .context("serialize affected nodes")?;
        let max_ts = a.max_ts.unwrap_or_else(Utc::now);
        sqlx::query(
            "INSERT INTO error_signatures \
                (signature, error_class, normalized_text, sample_texts, affected_nodes, \
                 count_24h, count_total, first_seen_at, last_seen_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8) \
             ON CONFLICT (signature) DO UPDATE SET \
                error_class     = EXCLUDED.error_class, \
                normalized_text = EXCLUDED.normalized_text, \
                sample_texts    = EXCLUDED.sample_texts, \
                affected_nodes  = EXCLUDED.affected_nodes, \
                count_24h       = EXCLUDED.count_24h, \
                count_total     = error_signatures.count_total + $7, \
                last_seen_at    = GREATEST(error_signatures.last_seen_at, $8)",
        )
        .bind(signature)
        .bind(&a.error_class)
        .bind(&a.normalized_text)
        .bind(&samples)
        .bind(&nodes)
        .bind(a.count_24h)
        .bind(a.new_since_cursor)
        .bind(max_ts)
        .execute(pg)
        .await
        .with_context(|| format!("upsert error_signatures for {signature}"))?;
    }
    Ok(())
}

/// Zero out `count_24h` for any previously-tracked signature that didn't show
/// up in this pass's window at all — otherwise a signature that stops
/// recurring keeps whatever `count_24h` it last had, forever.
async fn reset_absent_signatures(pg: &PgPool, seen: &[String]) -> Result<()> {
    sqlx::query(
        "UPDATE error_signatures SET count_24h = 0 \
          WHERE count_24h <> 0 AND NOT (signature = ANY($1))",
    )
    .bind(seen)
    .execute(pg)
    .await
    .context("zero out count_24h for signatures absent from this pass")?;
    Ok(())
}

// ─── Step 2: journald ingest ──────────────────────────────────────────────────

/// SSH into `dest` and capture stdout, bounded by [`SSH_TIMEOUT`]. Mirrors
/// `crate::verify_computer::ssh_capture` — see [`crate::ssh_opts`] for why
/// daemon-spawned SSH always needs `IdentityAgent=none`/`BatchMode=yes` *and*
/// an outer `tokio::time::timeout` (ConnectTimeout alone doesn't cover a
/// wedged local ssh-agent hanging at the auth step).
async fn ssh_capture(dest: &str, cmd: &str) -> Result<String, String> {
    let out = timeout(
        SSH_TIMEOUT,
        Command::new("ssh")
            .args(crate::ssh_opts::ssh_bypass_args())
            .args([
                "-o",
                "ConnectTimeout=8",
                "-o",
                "StrictHostKeyChecking=accept-new",
                dest,
                cmd,
            ])
            .output(),
    )
    .await
    .map_err(|_| "ssh timeout".to_string())?
    .map_err(|e| format!("spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "exit {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr)
                .trim()
                .chars()
                .take(160)
                .collect::<String>()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[derive(Default)]
struct NodeLogAgg {
    normalized: String,
    sample: String,
    count: i32,
}

fn aggregate_journal_lines(output: &str) -> HashMap<String, NodeLogAgg> {
    let mut agg: HashMap<String, NodeLogAgg> = HashMap::new();
    for line in output.lines().filter(|l| !l.trim().is_empty()) {
        let classification = classify_journal_line(line);
        if classification.is_empty() {
            continue;
        }
        let signature = crate::log_analysis_worker::compute_signature(&classification);
        let entry = agg.entry(signature).or_default();
        entry.normalized = classification;
        if entry.sample.is_empty() {
            entry.sample = truncate(line.trim(), 500);
        }
        entry.count += 1;
    }
    agg
}

async fn upsert_fleet_log_digest(
    pg: &PgPool,
    node_name: &str,
    agg: &HashMap<String, NodeLogAgg>,
) -> Result<()> {
    for (signature, a) in agg {
        sqlx::query(
            "INSERT INTO fleet_log_digest \
                (node_name, signature, normalized, sample_line, count_24h, last_seen_at) \
             VALUES ($1, $2, $3, $4, $5, NOW()) \
             ON CONFLICT (node_name, signature) DO UPDATE SET \
                normalized  = EXCLUDED.normalized, \
                sample_line = EXCLUDED.sample_line, \
                count_24h   = EXCLUDED.count_24h, \
                last_seen_at = NOW()",
        )
        .bind(node_name)
        .bind(signature)
        .bind(&a.normalized)
        .bind(&a.sample)
        .bind(a.count)
        .execute(pg)
        .await
        .with_context(|| format!("upsert fleet_log_digest for {node_name}/{signature}"))?;
    }
    Ok(())
}

/// Zero out `count_24h` for this node's previously-tracked signatures that
/// didn't appear in this pass's journald pull — same reasoning as
/// [`reset_absent_signatures`]. Only called when the pull itself succeeded
/// (see [`ingest_node_journald`]), so a transient SSH failure never gets
/// misread as "this node has no more errors."
async fn reset_absent_fleet_log_digest(
    pg: &PgPool,
    node_name: &str,
    seen: &[String],
) -> Result<()> {
    sqlx::query(
        "UPDATE fleet_log_digest SET count_24h = 0 \
          WHERE node_name = $1 AND count_24h <> 0 AND NOT (signature = ANY($2))",
    )
    .bind(node_name)
    .bind(seen)
    .execute(pg)
    .await
    .context("zero out count_24h for absent fleet_log_digest signatures")?;
    Ok(())
}

/// Best-effort per node: an unreachable/offline node logs and moves on
/// rather than failing the whole pass.
async fn ingest_node_journald(pg: &PgPool, node_name: &str, dest: &str) -> Result<()> {
    let output = match ssh_capture(dest, JOURNALCTL_CMD).await {
        Ok(output) => output,
        Err(e) => {
            tracing::debug!(node = node_name, error = %e, "error_miner: journald pull failed");
            return Ok(());
        }
    };
    let agg = aggregate_journal_lines(&output);
    upsert_fleet_log_digest(pg, node_name, &agg).await?;
    let seen: Vec<String> = agg.keys().cloned().collect();
    reset_absent_fleet_log_digest(pg, node_name, &seen).await
}

/// SSH into every online fleet node concurrently (bounded by [`SSH_TIMEOUT`]
/// each) so total wall time doesn't scale with fleet size.
async fn ingest_journald_all_nodes(pg: &PgPool) -> Result<()> {
    let nodes = ff_db::pg_list_nodes(pg).await.context("list fleet nodes")?;
    let online: Vec<_> = nodes.into_iter().filter(|n| n.status == "online").collect();

    let passes = online.into_iter().map(|node| {
        let pg = pg.clone();
        async move {
            let dest = format!("{}@{}", node.ssh_user, node.ip);
            if let Err(e) = ingest_node_journald(&pg, &node.name, &dest).await {
                tracing::warn!(node = %node.name, error = %e, "error_miner: journald ingest failed");
            }
        }
    });
    join_all(passes).await;
    Ok(())
}

// ─── Step 3: auto-file ────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct SignatureCandidate {
    signature: String,
    error_class: Option<String>,
    normalized_text: String,
    count_24h: i32,
    sample_texts: serde_json::Value,
    affected_nodes: serde_json::Value,
}

async fn fetch_new_signature_candidates(pg: &PgPool) -> Result<Vec<SignatureCandidate>> {
    sqlx::query_as::<_, SignatureCandidate>(
        "SELECT signature, error_class, normalized_text, count_24h, sample_texts, affected_nodes \
           FROM error_signatures \
          WHERE state = 'new' AND count_24h >= $1 \
          ORDER BY count_24h DESC",
    )
    .bind(AUTO_FILE_COUNT_THRESHOLD)
    .fetch_all(pg)
    .await
    .context("fetch error_signatures auto-file candidates")
}

async fn auto_filed_today_count(pg: &PgPool) -> Result<i64> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_items \
          WHERE created_by = $1 AND created_at >= date_trunc('day', NOW())",
    )
    .bind(CREATED_BY)
    .fetch_one(pg)
    .await
    .context("count today's error-miner work_items")
}

async fn has_open_work_item_for_signature(pg: &PgPool, signature: &str) -> Result<bool> {
    let statuses: Vec<String> = OPEN_WORK_ITEM_STATUSES
        .iter()
        .map(|s| s.to_string())
        .collect();
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM work_items \
          WHERE status = ANY($1) AND metadata->>'error_signature' = $2)",
    )
    .bind(&statuses)
    .bind(signature)
    .fetch_one(pg)
    .await
    .context("check existing open work_item for signature")
}

fn samples_from_json(value: &serde_json::Value) -> Vec<String> {
    serde_json::from_value(value.clone()).unwrap_or_default()
}

/// Files the bug through [`ff_db::pg_create_work_item`] — the same validated
/// creation path `ff pm create` uses (project-exists check, `created_by`
/// resolution) — instead of an ad hoc `INSERT INTO work_items` that bypasses
/// it.
async fn file_bug_work_item(
    pg: &PgPool,
    worker_name: &str,
    candidate: &SignatureCandidate,
) -> Result<uuid::Uuid> {
    let samples = samples_from_json(&candidate.sample_texts);
    let nodes = samples_from_json(&candidate.affected_nodes);

    let title = format!(
        "Recurring error: {}",
        candidate
            .error_class
            .clone()
            .unwrap_or_else(|| truncate(&candidate.normalized_text, 100))
    );
    let description = format!(
        "ErrorMiner detected {} occurrence(s) in the last {}h.\n\n\
         Class: {}\nNormalized: {}\nAffected nodes: {}\n\nSamples:\n{}",
        candidate.count_24h,
        LOOKBACK_HOURS,
        candidate.error_class.as_deref().unwrap_or("n/a"),
        candidate.normalized_text,
        if nodes.is_empty() {
            "n/a".to_string()
        } else {
            nodes.join(", ")
        },
        samples
            .iter()
            .enumerate()
            .map(|(i, s)| format!("{}. {}", i + 1, s))
            .collect::<Vec<_>>()
            .join("\n\n"),
    );
    let metadata = serde_json::json!({
        "error_signature": candidate.signature,
        "error_class": candidate.error_class,
        "count_24h": candidate.count_24h,
        "affected_nodes": nodes,
        "samples": samples,
        "detected_by": worker_name,
    });

    ff_db::pg_create_work_item(
        pg,
        ff_db::CreateWorkItem {
            project_id: AUTO_FILE_PROJECT_ID,
            kind: "bug",
            title: &title,
            description: Some(&description),
            priority: Some("normal"),
            created_by: CREATED_BY,
            risk_score: Some(AUTO_FILE_RISK_SCORE),
            metadata: Some(metadata),
        },
    )
    .await
    .context("insert error-miner bug work_item")
}

async fn mark_signature_filed(
    pg: &PgPool,
    signature: &str,
    work_item_id: uuid::Uuid,
) -> Result<()> {
    sqlx::query(
        "UPDATE error_signatures SET state = 'filed', work_item_id = $2 WHERE signature = $1",
    )
    .bind(signature)
    .bind(work_item_id)
    .execute(pg)
    .await
    .context("mark error_signatures filed")?;
    Ok(())
}

/// Auto-file up to [`AUTO_FILE_DAILY_CAP`] bug work_items/day for
/// still-`new` signatures that just crossed [`AUTO_FILE_COUNT_THRESHOLD`]
/// and have no open work_item already tracking them.
async fn auto_file_new_signatures(pg: &PgPool, worker_name: &str) -> Result<usize> {
    let mut filed_today = auto_filed_today_count(pg).await?;
    if filed_today >= AUTO_FILE_DAILY_CAP {
        return Ok(0);
    }

    let candidates = fetch_new_signature_candidates(pg).await?;
    let mut filed = 0usize;
    for candidate in candidates {
        if filed_today >= AUTO_FILE_DAILY_CAP {
            break;
        }
        if has_open_work_item_for_signature(pg, &candidate.signature).await? {
            continue;
        }
        let work_item_id = match file_bug_work_item(pg, worker_name, &candidate).await {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(
                    signature = %candidate.signature,
                    error = %e,
                    "error_miner: failed to auto-file bug work_item"
                );
                continue;
            }
        };
        mark_signature_filed(pg, &candidate.signature, work_item_id).await?;
        tracing::info!(
            signature = %candidate.signature,
            work_item_id = %work_item_id,
            count_24h = candidate.count_24h,
            "error_miner: auto-filed bug work_item"
        );
        filed_today += 1;
        filed += 1;
    }
    Ok(filed)
}

// ─── Step 4: Telegram digest ──────────────────────────────────────────────────

/// Is the daily digest due at this local time? Due from [`DIGEST_HOUR_LOCAL`]:00
/// until midnight, so a daemon that was down when the hour hit catches up on
/// its next tick instead of skipping a day.
pub fn digest_due(now_local: chrono::NaiveTime) -> bool {
    now_local.hour() >= DIGEST_HOUR_LOCAL
}

/// Deterministic session id for one calendar day's digest — doubles as the
/// fleet-wide "already sent today" marker in `telegram_messages`.
pub fn digest_session_id(date: chrono::NaiveDate) -> String {
    format!("{DIGEST_SESSION_PREFIX}-{}", date.format("%Y-%m-%d"))
}

#[derive(sqlx::FromRow)]
struct TopSignature {
    signature: String,
    error_class: Option<String>,
    normalized_text: String,
    count_24h: i32,
    state: String,
}

async fn fetch_top_signatures(pg: &PgPool, limit: i64) -> Result<Vec<TopSignature>> {
    sqlx::query_as::<_, TopSignature>(
        "SELECT signature, error_class, normalized_text, count_24h, state \
           FROM error_signatures \
          WHERE count_24h > 0 \
          ORDER BY count_24h DESC \
          LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pg)
    .await
    .context("fetch top error_signatures for digest")
}

/// Render the digest body. Pure so it unit-tests without a database.
fn format_digest(rows: &[TopSignature]) -> String {
    if rows.is_empty() {
        return "No recurring errors in the last 24h.".to_string();
    }
    let mut lines = vec!["Top error signatures (24h):".to_string()];
    for row in rows {
        let label = row
            .error_class
            .clone()
            .unwrap_or_else(|| truncate(&row.normalized_text, 80));
        let short_sig = &row.signature[..row.signature.len().min(8)];
        lines.push(format!(
            "  • [{short_sig}] {label} — {}x ({})",
            row.count_24h, row.state
        ));
    }
    lines.join("\n")
}

async fn send_digest_if_due(pg: &PgPool) -> Result<()> {
    let now = chrono::Local::now();
    if !digest_due(now.time()) {
        return Ok(());
    }

    let session_id = digest_session_id(now.date_naive());
    let already_sent: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM telegram_messages WHERE session_id = $1)")
            .bind(&session_id)
            .fetch_one(pg)
            .await
            .context("check error-miner digest dedup")?;
    if already_sent {
        return Ok(());
    }

    let rows = fetch_top_signatures(pg, 5).await?;
    let title = format!(
        "ErrorMiner digest — {}",
        now.date_naive().format("%Y-%m-%d")
    );
    let body = format_digest(&rows);
    crate::telegram::send_telegram_recorded(pg, &title, &body, &session_id).await?;
    Ok(())
}

// ─── Entry point ──────────────────────────────────────────────────────────────

/// One scheduler pass of ErrorMiner. Registered in the daemon tick registry
/// (leader-only), so by the time this runs the caller has already
/// established that this node is the live leader.
pub async fn run_error_miner_tick(pg: &PgPool, worker_name: &str) -> Result<()> {
    let window_start = Utc::now() - chrono::Duration::hours(LOOKBACK_HOURS);
    let cursors = fetch_last_seen_cursors(pg).await?;
    let errors = pull_recent_errors(pg).await?;
    let agg = aggregate_signatures(&errors, &cursors, window_start);
    upsert_error_signatures(pg, &agg).await?;
    let seen_signatures: Vec<String> = agg.keys().cloned().collect();
    reset_absent_signatures(pg, &seen_signatures).await?;

    ingest_journald_all_nodes(pg).await?;

    auto_file_new_signatures(pg, worker_name).await?;

    send_digest_if_due(pg).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_uuids_paths_numbers_and_shas() {
        let text = "Error at /var/log/forgefleet/agent.log:42 for request \
                     550e8400-e29b-41d4-a716-446655440000 sha=deadbeefcafe";
        let normalized = normalize_error_text(text);
        assert!(!normalized.contains("/var/log"));
        assert!(!normalized.contains("550e8400"));
        assert!(!normalized.contains("deadbeefcafe"));
        assert!(normalized.contains("<PATH>"));
        assert!(normalized.contains("<UUID>"));
        assert!(normalized.contains("<HEX>"));
        assert_eq!(normalized, "error at <PATH> for request <UUID> sha=<HEX>");
    }

    #[test]
    fn same_class_different_wording_collapses_to_one_signature() {
        let a = "ConnectionRefused: could not reach 10.0.0.5:8080";
        let b = "ConnectionRefused: could not reach 10.0.0.9:9090 after 3 retries";
        let (sig_a, _, class_a) = error_signature(a);
        let (sig_b, _, class_b) = error_signature(b);
        assert_eq!(class_a.as_deref(), Some("ConnectionRefused"));
        assert_eq!(class_a, class_b);
        assert_eq!(sig_a, sig_b);
    }

    #[test]
    fn no_class_token_falls_back_to_normalized_text_signature() {
        let text = "panic occurred while draining node sophie";
        let (sig, normalized, class) = error_signature(text);
        assert!(class.is_none());
        assert_eq!(
            sig,
            crate::log_analysis_worker::compute_signature(&normalized)
        );
    }

    #[test]
    fn extract_error_class_rejects_prose_with_a_colon() {
        // Free-form sentences with a colon shouldn't be mistaken for a class
        // token: the head before ':' contains a space.
        assert_eq!(extract_error_class("note: this is not a class"), None);
        assert_eq!(extract_error_class("no colon here at all"), None);
        assert_eq!(
            extract_error_class("TimeoutError: deadline exceeded"),
            Some("TimeoutError".to_string())
        );
    }

    #[test]
    fn classify_journal_line_takes_first_six_normalized_words() {
        let line = "Jul 24 09:00:01 sophie forgefleetd[1234]: dispatch failed for work_item 7f3a";
        let classification = classify_journal_line(line);
        assert_eq!(classification.split_whitespace().count(), 6);
    }

    fn mined(raw_text: &str, node: Option<&str>, ts: DateTime<Utc>) -> MinedError {
        MinedError {
            raw_text: raw_text.to_string(),
            node: node.map(str::to_string),
            ts,
        }
    }

    #[test]
    fn aggregate_signatures_counts_and_caps_samples() {
        let now = Utc::now();
        let window_start = now - chrono::Duration::hours(LOOKBACK_HOURS);
        let errors: Vec<MinedError> = (0..5)
            .map(|i| {
                mined(
                    &format!("TimeoutError: attempt {i} failed"),
                    Some(if i % 2 == 0 { "sophie" } else { "priya" }),
                    now,
                )
            })
            .collect();
        let agg = aggregate_signatures(&errors, &HashMap::new(), window_start);
        assert_eq!(agg.len(), 1);
        let entry = agg.values().next().unwrap();
        assert_eq!(entry.count_24h, 5);
        assert_eq!(entry.new_since_cursor, 5);
        assert_eq!(entry.samples.len(), MAX_SAMPLES_PER_SIGNATURE);
        assert_eq!(entry.nodes.len(), 2);
    }

    #[test]
    fn aggregate_signatures_only_counts_new_since_cursor_as_new() {
        // Same 5 occurrences re-scanned on a later, overlapping 24h pass:
        // the signature was already fully counted (cursor == now), so
        // nothing in this pass should look "new" even though count_24h is
        // still 5.
        let now = Utc::now();
        let window_start = now - chrono::Duration::hours(LOOKBACK_HOURS);
        let errors: Vec<MinedError> = (0..5)
            .map(|i| mined(&format!("TimeoutError: attempt {i} failed"), None, now))
            .collect();
        let (signature, _, _) = error_signature(&errors[0].raw_text);
        let mut cursors = HashMap::new();
        cursors.insert(signature, now);

        let agg = aggregate_signatures(&errors, &cursors, window_start);
        let entry = agg.values().next().unwrap();
        assert_eq!(entry.count_24h, 5);
        assert_eq!(entry.new_since_cursor, 0);
    }

    #[test]
    fn aggregate_journal_lines_dedups_by_classification() {
        let output = "Jul 24 09:00:01 sophie forgefleetd[1]: dispatch failed for item a\n\
                       Jul 24 09:00:05 sophie forgefleetd[1]: dispatch failed for item b\n\
                       Jul 24 09:01:00 sophie forgefleetd[1]: mesh check timed out\n";
        let agg = aggregate_journal_lines(output);
        assert_eq!(agg.len(), 2);
        assert!(agg.values().any(|a| a.count == 2));
        assert!(agg.values().any(|a| a.count == 1));
    }

    #[test]
    fn digest_due_only_from_send_hour_onward() {
        let t = |h: u32| chrono::NaiveTime::from_hms_opt(h, 0, 0).unwrap();
        assert!(!digest_due(t(0)));
        assert!(!digest_due(t(6)));
        assert!(digest_due(t(7)));
        assert!(digest_due(t(23)));
    }

    #[test]
    fn digest_session_id_is_stable_per_date() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 7, 24).unwrap();
        assert_eq!(digest_session_id(date), "error-miner-digest-2026-07-24");
        assert_eq!(digest_session_id(date), digest_session_id(date));
    }

    #[test]
    fn format_digest_empty_and_populated() {
        assert_eq!(format_digest(&[]), "No recurring errors in the last 24h.");

        let rows = vec![TopSignature {
            signature: "abcdef1234567890".to_string(),
            error_class: Some("TimeoutError".to_string()),
            normalized_text: "timeout error attempt <num> failed".to_string(),
            count_24h: 12,
            state: "filed".to_string(),
        }];
        let body = format_digest(&rows);
        assert!(body.contains("TimeoutError"));
        assert!(body.contains("12x"));
        assert!(body.contains("filed"));
    }
}
