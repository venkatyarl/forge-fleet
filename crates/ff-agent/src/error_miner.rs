//! ErrorMiner — leader-only daily pass that normalizes and aggregates fleet
//! errors into `error_signatures` (schema V247), ingests each online node's
//! `forgefleetd` journald warnings into `fleet_log_digest`, auto-files a
//! capped number of bug work_items for signatures that cross a recurrence
//! threshold, and sends a Telegram digest of the top signatures.
//!
//! Configuration is read from environment on each tick (same convention as
//! [`crate::log_analysis_worker`]) so operators can tune it without
//! restarting the daemon:
//!   - `FF_ERROR_MINER_PROJECT_ID` target project for auto-filed bugs
//!     (default `ff-error-miner`)
//!   - `FF_ERROR_MINER_MIN_COUNT_TO_FILE` minimum `count_24h` before a `new`
//!     signature is auto-filed (default 10)
//!   - `FF_ERROR_MINER_MAX_AUTOFILE_PER_DAY` hard cap on auto-filed bugs per
//!     calendar day, fleet-wide (default 3)
//!   - `FF_ERROR_MINER_JOURNALD_TAIL_LINES` lines tailed per node per pass
//!     (default 200)

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value as JsonValue;
use sqlx::PgPool;
use tokio::process::Command;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Cadence at which the daemon tick registry invokes [`run_error_miner_tick`].
/// The pass itself is a full sweep each time (no internal clock-gating), so
/// this interval IS the "daily pass" cadence.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

const DEFAULT_PROJECT_ID: &str = "ff-error-miner";
const DEFAULT_MIN_COUNT_TO_FILE: i64 = 10;
const DEFAULT_MAX_AUTOFILE_PER_DAY: i64 = 3;
const DEFAULT_JOURNALD_TAIL_LINES: usize = 200;

/// Auto-filed bugs are stamped with this fixed risk score (0-100 scale).
const AUTO_FILE_RISK_SCORE: f32 = 60.0;
const AUTO_FILE_CREATED_BY: &str = "error-miner";

#[derive(Debug, Clone)]
struct ErrorMinerConfig {
    project_id: String,
    min_count_to_file: i64,
    max_autofile_per_day: i64,
    journald_tail_lines: usize,
}

impl ErrorMinerConfig {
    fn from_env() -> Self {
        Self {
            project_id: std::env::var("FF_ERROR_MINER_PROJECT_ID")
                .unwrap_or_else(|_| DEFAULT_PROJECT_ID.to_string()),
            min_count_to_file: std::env::var("FF_ERROR_MINER_MIN_COUNT_TO_FILE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_MIN_COUNT_TO_FILE),
            max_autofile_per_day: std::env::var("FF_ERROR_MINER_MAX_AUTOFILE_PER_DAY")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_MAX_AUTOFILE_PER_DAY),
            journald_tail_lines: std::env::var("FF_ERROR_MINER_JOURNALD_TAIL_LINES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_JOURNALD_TAIL_LINES),
        }
    }
}

/// Summary of one error-miner pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ErrorMinerReport {
    pub signatures_seen: usize,
    pub nodes_scanned: usize,
    pub auto_filed: usize,
}

struct ErrorObservation {
    text: String,
    node: Option<String>,
}

/// One scheduler pass, registered in the daemon tick registry (leader-only).
pub async fn run_error_miner_tick(pg: &PgPool, worker_name: &str) -> Result<ErrorMinerReport> {
    let config = ErrorMinerConfig::from_env();
    let mut report = ErrorMinerReport::default();

    let observations = collect_recent_errors(pg).await?;
    report.signatures_seen = upsert_error_signatures(pg, &observations).await?;

    report.nodes_scanned = ingest_journald_digest(pg, &config).await?;

    report.auto_filed = auto_file_signatures(pg, &config, worker_name).await?;

    if let Err(err) = send_digest(pg).await {
        warn!(error = %err, "error_miner: telegram digest failed");
    }

    info!(
        signatures_seen = report.signatures_seen,
        nodes_scanned = report.nodes_scanned,
        auto_filed = report.auto_filed,
        "error_miner: pass complete"
    );
    Ok(report)
}

/// Pull last-24h `work_items.last_error` + `ff_interactions.error_text`.
async fn collect_recent_errors(pg: &PgPool) -> Result<Vec<ErrorObservation>> {
    let mut out = Vec::new();

    let wi_rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT last_error, assigned_computer FROM work_items \
         WHERE last_error IS NOT NULL \
           AND COALESCE(completed_at, started_at, created_at) >= NOW() - INTERVAL '24 hours'",
    )
    .fetch_all(pg)
    .await
    .context("collect work_items.last_error")?;
    out.extend(
        wi_rows
            .into_iter()
            .map(|(text, node)| ErrorObservation { text, node }),
    );

    let ffi_rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT error_text, worker_name FROM ff_interactions \
         WHERE outcome = 'error' AND error_text IS NOT NULL AND error_text != '' \
           AND ts >= NOW() - INTERVAL '24 hours'",
    )
    .fetch_all(pg)
    .await
    .context("collect ff_interactions.error_text")?;
    out.extend(
        ffi_rows
            .into_iter()
            .map(|(text, node)| ErrorObservation { text, node }),
    );

    Ok(out)
}

/// signature = sha of (error_class token if present else normalized text:
/// lowercased, uuids/paths/numbers/shas stripped).
fn signature_for(text: &str) -> (String, Option<&'static str>) {
    let class = crate::cloud_error::classify("unknown", None, text);
    if class != crate::cloud_error::CloudErrorClass::Unknown {
        let token = class.as_str();
        (
            crate::log_analysis_worker::compute_signature(token),
            Some(token),
        )
    } else {
        let lowered = text.to_ascii_lowercase();
        let normalized = crate::log_analysis_worker::replace_tokens(&lowered);
        let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
        (
            crate::log_analysis_worker::compute_signature(&normalized),
            None,
        )
    }
}

struct GroupedSignature {
    error_class: Option<String>,
    sample_text: String,
    count: i64,
    nodes: Vec<String>,
}

/// Normalize, aggregate, and upsert `error_signatures` counts.
async fn upsert_error_signatures(pg: &PgPool, observations: &[ErrorObservation]) -> Result<usize> {
    let mut grouped: HashMap<String, GroupedSignature> = HashMap::new();

    for obs in observations {
        let (signature, class) = signature_for(&obs.text);
        let entry = grouped
            .entry(signature)
            .or_insert_with(|| GroupedSignature {
                error_class: class.map(str::to_string),
                sample_text: obs.text.clone(),
                count: 0,
                nodes: Vec::new(),
            });
        entry.count += 1;
        if let Some(node) = &obs.node {
            if !entry.nodes.iter().any(|n| n == node) {
                entry.nodes.push(node.clone());
            }
        }
    }

    for (signature, group) in &grouped {
        let affected_nodes: JsonValue = serde_json::to_value(&group.nodes)?;
        sqlx::query(
            "INSERT INTO error_signatures \
                (signature, error_class, first_seen, last_seen, count_24h, count_total, sample_text, affected_nodes) \
             VALUES ($1, $2, NOW(), NOW(), $3, $3, $4, $5) \
             ON CONFLICT (signature) DO UPDATE SET \
                 last_seen = NOW(), \
                 count_24h = $3, \
                 count_total = error_signatures.count_total + $3, \
                 sample_text = $4, \
                 affected_nodes = $5, \
                 error_class = COALESCE(error_signatures.error_class, $2)",
        )
        .bind(signature)
        .bind(&group.error_class)
        .bind(group.count as i32)
        .bind(&group.sample_text)
        .bind(&affected_nodes)
        .execute(pg)
        .await
        .with_context(|| format!("upsert error_signatures for {signature}"))?;
    }

    Ok(grouped.len())
}

fn node_online(node: &ff_db::FleetNodeRow) -> bool {
    node.status == "online"
        && !matches!(
            node.computer_status.as_deref(),
            Some("offline") | Some("reserved") | Some("decommissioned")
        )
}

/// Classify a journald line by its first 6 words, normalized (same token
/// replacement as the log-analysis worker) so timestamps/pids/hosts collapse
/// out and the same underlying message groups together.
fn classify_journal_line(line: &str) -> Option<String> {
    let normalized = crate::log_analysis_worker::replace_tokens(line);
    let words: Vec<&str> = normalized.split_whitespace().take(6).collect();
    if words.is_empty() {
        None
    } else {
        Some(words.join(" "))
    }
}

async fn run_journalctl(user: &str, ip: &str, tail_lines: usize) -> Result<String> {
    let command = format!(
        "journalctl --user -u forgefleetd -p warning --since -24h --no-pager | tail -{tail_lines}"
    );
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        Command::new("ssh")
            .args(crate::ssh_opts::ssh_bypass_args())
            .args([
                "-o",
                "ConnectTimeout=5",
                "-o",
                "StrictHostKeyChecking=accept-new",
                &format!("{user}@{ip}"),
                &command,
            ])
            .output(),
    )
    .await
    .context("ssh journalctl timed out")?
    .context("ssh journalctl spawn failed")?;

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// For each online computer, tail `forgefleetd`'s user-journal warnings and
/// classify lines by first 6 words normalized, upserting `fleet_log_digest`.
async fn ingest_journald_digest(pg: &PgPool, config: &ErrorMinerConfig) -> Result<usize> {
    let nodes = ff_db::pg_list_nodes(pg).await.context("list fleet nodes")?;
    let online: Vec<_> = nodes.into_iter().filter(node_online).collect();

    let today = chrono::Utc::now().date_naive();
    let mut scanned = 0usize;

    for node in &online {
        let output =
            match run_journalctl(&node.ssh_user, &node.ip, config.journald_tail_lines).await {
                Ok(out) => out,
                Err(err) => {
                    debug!(node = %node.name, error = %err, "error_miner: journald fetch failed");
                    continue;
                }
            };
        scanned += 1;

        let mut classes: HashMap<String, (i64, String)> = HashMap::new();
        for line in output.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Some(line_class) = classify_journal_line(line) {
                let entry = classes
                    .entry(line_class)
                    .or_insert_with(|| (0, line.to_string()));
                entry.0 += 1;
            }
        }

        for (line_class, (count, sample)) in &classes {
            sqlx::query(
                "INSERT INTO fleet_log_digest (node, day, level, line_class, count, sample) \
                 VALUES ($1, $2, 'warning', $3, $4, $5) \
                 ON CONFLICT (node, day, level, line_class) DO UPDATE SET \
                     count = fleet_log_digest.count + EXCLUDED.count, \
                     sample = EXCLUDED.sample",
            )
            .bind(&node.name)
            .bind(today)
            .bind(line_class)
            .bind(*count as i32)
            .bind(sample)
            .execute(pg)
            .await
            .with_context(|| format!("upsert fleet_log_digest for {}/{line_class}", node.name))?;
        }
    }

    Ok(scanned)
}

/// Idempotently create the auto-file target project so work_item inserts
/// never fail on the `project_id` FK.
async fn ensure_project(pg: &PgPool, project_id: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO projects (id, display_name, default_branch, status) \
         VALUES ($1, $2, 'main', 'active') \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(project_id)
    .bind(format!("Error Miner ({project_id})"))
    .execute(pg)
    .await?;
    Ok(())
}

/// Any signature with `count_24h >= min_count_to_file`, `state = 'new'`, and
/// no work_item already referencing it gets auto-filed as a bug, up to
/// `max_autofile_per_day` fleet-wide per calendar day.
async fn auto_file_signatures(
    pg: &PgPool,
    config: &ErrorMinerConfig,
    _worker_name: &str,
) -> Result<usize> {
    let filed_today: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM error_signatures es \
         JOIN work_items wi ON wi.id = es.work_item_id \
         WHERE es.state = 'filed' AND wi.created_at >= date_trunc('day', NOW())",
    )
    .fetch_one(pg)
    .await
    .context("count signatures already filed today")?;

    let remaining_cap = (config.max_autofile_per_day - filed_today).max(0);
    if remaining_cap == 0 {
        debug!("error_miner: daily auto-file cap already reached");
        return Ok(0);
    }

    let candidates: Vec<(
        String,
        Option<String>,
        i32,
        Option<String>,
        Option<JsonValue>,
    )> = sqlx::query_as(
        "SELECT signature, error_class, count_24h, sample_text, affected_nodes \
             FROM error_signatures \
             WHERE state = 'new' AND count_24h >= $1 AND work_item_id IS NULL \
             ORDER BY count_24h DESC",
    )
    .bind(config.min_count_to_file as i32)
    .fetch_all(pg)
    .await
    .context("select auto-file candidates")?;

    if candidates.is_empty() {
        return Ok(0);
    }

    ensure_project(pg, &config.project_id).await?;

    let mut filed = 0i64;
    for (signature, error_class, count_24h, sample_text, affected_nodes) in candidates {
        if filed >= remaining_cap {
            break;
        }

        let class_label = error_class.as_deref().unwrap_or("unclassified");
        let affected_nodes = affected_nodes.unwrap_or(JsonValue::Array(Vec::new()));
        let title = format!("[error-miner] {class_label} ({count_24h} occurrences/24h)");
        let description = format!(
            "Auto-filed by error-miner.\n\n\
             Signature: {signature}\n\
             Class: {class_label}\n\
             Count (24h): {count_24h}\n\
             Affected nodes: {affected_nodes}\n\n\
             Sample:\n{}",
            sample_text.as_deref().unwrap_or("(no sample captured)")
        );
        let metadata = serde_json::json!({
            "error_signature": signature,
            "error_class": error_class,
            "count_24h": count_24h,
            "affected_nodes": affected_nodes,
        });

        let work_item_id: Uuid = sqlx::query_scalar(
            "INSERT INTO work_items \
                (project_id, kind, title, description, status, priority, created_by, risk_score, metadata) \
             VALUES ($1, 'bug', $2, $3, 'idea', 'normal', $4, $5, $6) \
             RETURNING id",
        )
        .bind(&config.project_id)
        .bind(&title)
        .bind(&description)
        .bind(AUTO_FILE_CREATED_BY)
        .bind(AUTO_FILE_RISK_SCORE)
        .bind(&metadata)
        .fetch_one(pg)
        .await
        .with_context(|| format!("insert auto-filed work_item for {signature}"))?;

        sqlx::query(
            "UPDATE error_signatures SET work_item_id = $2, state = 'filed' WHERE signature = $1",
        )
        .bind(&signature)
        .bind(work_item_id)
        .execute(pg)
        .await?;

        info!(
            signature = %signature,
            %work_item_id,
            count_24h,
            "error_miner: auto-filed bug work_item"
        );
        filed += 1;
    }

    Ok(filed as usize)
}

/// Telegram digest: top-5 signatures by `count_24h` with their states.
async fn send_digest(pg: &PgPool) -> Result<()> {
    let top: Vec<(String, Option<String>, i32, String)> = sqlx::query_as(
        "SELECT signature, error_class, count_24h, state FROM error_signatures \
         WHERE count_24h > 0 ORDER BY count_24h DESC LIMIT 5",
    )
    .fetch_all(pg)
    .await
    .context("select top signatures for digest")?;

    if top.is_empty() {
        return Ok(());
    }

    let mut lines = vec!["Top error signatures (24h):".to_string()];
    for (signature, error_class, count_24h, state) in &top {
        let short_sig = &signature[..signature.len().min(12)];
        lines.push(format!(
            "  • {} — {count_24h}x [{state}] ({short_sig})",
            error_class.as_deref().unwrap_or("unclassified"),
        ));
    }
    let body = lines.join("\n");
    let session_id = format!("error-miner-{}", chrono::Utc::now().date_naive());

    crate::telegram::send_telegram_recorded(
        pg,
        "ForgeFleet error-miner digest",
        &body,
        &session_id,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_prefers_error_class_token() {
        let (sig_a, class_a) = signature_for("429 rate limit exceeded, please retry later");
        let (sig_b, class_b) = signature_for("429 Too Many Requests: rate_limit_exceeded");
        assert_eq!(class_a, Some("rate_limited"));
        assert_eq!(class_a, class_b);
        assert_eq!(sig_a, sig_b);
    }

    #[test]
    fn signature_falls_back_to_normalized_text() {
        let (_, class) = signature_for("panic: index out of bounds at src/foo.rs:42");
        assert_eq!(class, None);
    }

    #[test]
    fn signature_is_stable_across_variable_tokens() {
        let a = "connection failed to 10.0.0.5:8080 after 3 attempts";
        let b = "connection failed to 10.0.0.9:9090 after 7 attempts";
        let (sig_a, _) = signature_for(a);
        let (sig_b, _) = signature_for(b);
        assert_eq!(sig_a, sig_b);
    }

    #[test]
    fn classify_journal_line_takes_first_six_words() {
        let line = "Jul 24 08:00:01 host forgefleetd[123]: connection refused to peer 10.0.0.1";
        let class = classify_journal_line(line).unwrap();
        assert_eq!(class.split_whitespace().count(), 6);
    }

    #[test]
    fn classify_journal_line_skips_empty() {
        assert_eq!(classify_journal_line("   "), None);
    }

    #[test]
    fn config_reads_defaults_when_unset() {
        let config = ErrorMinerConfig::from_env();
        assert_eq!(config.project_id, DEFAULT_PROJECT_ID);
        assert_eq!(config.min_count_to_file, DEFAULT_MIN_COUNT_TO_FILE);
        assert_eq!(config.max_autofile_per_day, DEFAULT_MAX_AUTOFILE_PER_DAY);
        assert_eq!(config.journald_tail_lines, DEFAULT_JOURNALD_TAIL_LINES);
    }
}
