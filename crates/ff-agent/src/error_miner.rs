//! Daily fleet error aggregation, journald digesting, and bounded bug filing.

use anyhow::{Context, Result};
use chrono::{NaiveDate, Utc};
use regex::Regex;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;
use tokio::process::Command;

const PROJECT_ID: &str = "forge-fleet";
const AUTO_FILE_THRESHOLD: i32 = 10;
const DAILY_FILE_CAP: i64 = 3;
const DIGEST_SESSION_PREFIX: &str = "error-miner";
const OPEN_STATUSES: &[&str] = &[
    "idea",
    "decomposed",
    "ready",
    "claimed",
    "building",
    "in_progress",
    "in_review",
];

static ERROR_CLASS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\berror[_ -]?class\s*[:=]\s*([a-z0-9_.:/-]+)").expect("valid regex")
});
static UUID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}\b")
        .expect("valid regex")
});
static SHA_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b[0-9a-f]{7,64}\b").expect("valid regex"));
static WINDOWS_PATH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b[a-z]:\\(?:[^\\\s]+\\)*[^\\\s]*").expect("valid regex"));
static UNIX_PATH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|\s)/(?:[^\s/]+/)*[^\s]*").expect("valid regex"));
static NUMBER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d+(?:\.\d+)?\b").expect("valid regex"));

#[derive(Debug, Clone)]
struct Observation {
    text: String,
    node: Option<String>,
}

#[derive(Debug, Clone)]
struct Aggregate {
    signature: String,
    error_class: Option<String>,
    count: i32,
    samples: Vec<String>,
    nodes: BTreeSet<String>,
}

#[derive(Debug, Deserialize)]
struct FleetExecOutput {
    stdout: String,
}

#[derive(Debug, sqlx::FromRow)]
struct FilingCandidate {
    signature: String,
    error_class: Option<String>,
    count_24h: i32,
    sample_text: Option<String>,
    affected_nodes: Option<serde_json::Value>,
}

/// Extract a canonical error-class token when the producer supplied one.
pub fn extract_error_class(text: &str) -> Option<String> {
    ERROR_CLASS_RE
        .captures(text)
        .and_then(|captures| captures.get(1))
        .map(|token| {
            token
                .as_str()
                .trim_matches(['.', ',', ';'])
                .to_ascii_lowercase()
        })
}

/// Remove volatile identifiers from error text so equivalent failures group.
pub fn normalize_error_text(text: &str) -> String {
    let lowered = text.to_ascii_lowercase();
    let without_uuid = UUID_RE.replace_all(&lowered, " ");
    let without_sha = SHA_RE.replace_all(&without_uuid, " ");
    let without_windows_path = WINDOWS_PATH_RE.replace_all(&without_sha, " ");
    let without_unix_path = UNIX_PATH_RE.replace_all(&without_windows_path, " ");
    let without_numbers = NUMBER_RE.replace_all(&without_unix_path, " ");
    without_numbers
        .split_whitespace()
        .filter(|token| token.chars().any(char::is_alphanumeric))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Stable SHA-256 signature of the class token, or normalized text as fallback.
pub fn error_signature(text: &str) -> (String, Option<String>) {
    let error_class = extract_error_class(text);
    let basis = error_class
        .as_deref()
        .map(str::to_owned)
        .unwrap_or_else(|| normalize_error_text(text));
    let signature = format!("{:x}", Sha256::digest(basis.as_bytes()));
    (signature, error_class)
}

/// The normalized first six words used to classify journald lines.
pub fn journal_line_class(line: &str) -> String {
    normalize_error_text(line)
        .split_whitespace()
        .take(6)
        .collect::<Vec<_>>()
        .join(" ")
}

fn aggregate_observations(observations: Vec<Observation>) -> Vec<Aggregate> {
    let mut grouped: BTreeMap<String, Aggregate> = BTreeMap::new();
    for observation in observations {
        let (signature, error_class) = error_signature(&observation.text);
        let entry = grouped
            .entry(signature.clone())
            .or_insert_with(|| Aggregate {
                signature,
                error_class,
                count: 0,
                samples: Vec::new(),
                nodes: BTreeSet::new(),
            });
        entry.count += 1;
        if entry.samples.len() < 3 && !entry.samples.contains(&observation.text) {
            entry.samples.push(observation.text);
        }
        if let Some(node) = observation.node {
            entry.nodes.insert(node);
        }
    }
    grouped.into_values().collect()
}

async fn collect_database_errors(pg: &PgPool) -> Result<Vec<Observation>> {
    let rows = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT last_error, assigned_computer
           FROM work_items
          WHERE last_error IS NOT NULL
            AND COALESCE(completed_at, started_at, created_at) >= NOW() - INTERVAL '24 hours'
         UNION ALL
         SELECT error_text, worker_name
           FROM ff_interactions
          WHERE error_text IS NOT NULL
            AND ts >= NOW() - INTERVAL '24 hours'",
    )
    .fetch_all(pg)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(text, node)| Observation { text, node })
        .collect())
}

async fn persist_error_aggregates(pg: &PgPool, aggregates: &[Aggregate]) -> Result<()> {
    // A signature absent from this pass no longer has occurrences in the rolling window.
    sqlx::query("UPDATE error_signatures SET count_24h = 0")
        .execute(pg)
        .await?;
    for aggregate in aggregates {
        sqlx::query(
            "INSERT INTO error_signatures
                 (signature, error_class, first_seen, last_seen, count_24h, count_total,
                  sample_text, affected_nodes)
             VALUES ($1, $2, NOW(), NOW(), $3, $3, $4, $5)
             ON CONFLICT (signature) DO UPDATE SET
                 error_class = COALESCE(error_signatures.error_class, EXCLUDED.error_class),
                 last_seen = NOW(),
                 count_24h = EXCLUDED.count_24h,
                 count_total = GREATEST(error_signatures.count_total, EXCLUDED.count_24h),
                 sample_text = EXCLUDED.sample_text,
                 affected_nodes = EXCLUDED.affected_nodes",
        )
        .bind(&aggregate.signature)
        .bind(&aggregate.error_class)
        .bind(aggregate.count)
        .bind(serde_json::to_string(&aggregate.samples)?)
        .bind(serde_json::json!(aggregate.nodes))
        .execute(pg)
        .await?;
    }
    Ok(())
}

async fn online_computers(pg: &PgPool) -> Result<Vec<String>> {
    sqlx::query_scalar("SELECT name FROM computers WHERE status = 'online' ORDER BY name")
        .fetch_all(pg)
        .await
        .context("query online computers")
}

async fn collect_journald(pg: &PgPool, day: NaiveDate) -> Result<()> {
    for node in online_computers(pg).await? {
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            Command::new("ff")
                .args([
                    "fleet",
                    "exec",
                    "--json",
                    &node,
                    "--",
                    "journalctl",
                    "--user",
                    "-u",
                    "forgefleetd",
                    "-p",
                    "warning",
                    "--since",
                    "-24h",
                    "--no-pager",
                    "|",
                    "tail",
                    "-200",
                ])
                .output(),
        )
        .await;
        let output = match output {
            Ok(Ok(output)) if output.status.success() => output,
            Ok(Ok(output)) => {
                tracing::warn!(
                    node,
                    stderr = %String::from_utf8_lossy(&output.stderr),
                    "error miner journald collection failed"
                );
                continue;
            }
            Ok(Err(error)) => {
                tracing::warn!(node, %error, "error miner could not start ff fleet exec");
                continue;
            }
            Err(_) => {
                tracing::warn!(node, "error miner fleet exec timed out");
                continue;
            }
        };
        let parsed: FleetExecOutput = match serde_json::from_slice(&output.stdout) {
            Ok(parsed) => parsed,
            Err(error) => {
                tracing::warn!(node, %error, "error miner received invalid fleet exec JSON");
                continue;
            }
        };
        let mut classes: BTreeMap<String, (i32, String)> = BTreeMap::new();
        for line in parsed.stdout.lines().filter(|line| !line.trim().is_empty()) {
            let class = journal_line_class(line);
            if class.is_empty() {
                continue;
            }
            let entry = classes.entry(class).or_insert((0, line.to_owned()));
            entry.0 += 1;
        }
        for (class, (count, sample)) in classes {
            sqlx::query(
                "INSERT INTO fleet_log_digest (node, day, level, line_class, count, sample)
                 VALUES ($1, $2, 'warning', $3, $4, $5)
                 ON CONFLICT (node, day, level, line_class) DO UPDATE SET
                     count = EXCLUDED.count, sample = EXCLUDED.sample",
            )
            .bind(&node)
            .bind(day)
            .bind(class)
            .bind(count)
            .bind(sample)
            .execute(pg)
            .await?;
        }
    }
    Ok(())
}

async fn create_work_item(
    tx: &mut Transaction<'_, Postgres>,
    candidate: &FilingCandidate,
) -> Result<uuid::Uuid> {
    sqlx::query(
        "INSERT INTO projects (id, display_name, default_branch, status)
         VALUES ($1, 'ForgeFleet', 'main', 'active')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(PROJECT_ID)
    .execute(&mut **tx)
    .await?;
    let samples: Vec<String> = candidate
        .sample_text
        .as_deref()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_default();
    let description = format!(
        "ErrorMiner detected a recurring fleet error.\n\nSignature: {}\nClass: {}\nCount (24h): {}\nAffected nodes: {}\n\nSamples:\n{}",
        candidate.signature,
        candidate.error_class.as_deref().unwrap_or("unclassified"),
        candidate.count_24h,
        candidate
            .affected_nodes
            .as_ref()
            .map(serde_json::Value::to_string)
            .unwrap_or_else(|| "[]".into()),
        samples
            .iter()
            .take(3)
            .map(|sample| format!("- {sample}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    sqlx::query_scalar(
        "INSERT INTO work_items
             (project_id, kind, title, description, status, priority, created_by,
              risk_score, metadata, original_signal)
         VALUES ($1, 'bug', $2, $3, 'idea', 'normal', 'error-miner', 60, $4, $5)
         RETURNING id",
    )
    .bind(PROJECT_ID)
    .bind(format!(
        "Recurring error: {}",
        candidate
            .error_class
            .as_deref()
            .unwrap_or(&candidate.signature[..12])
    ))
    .bind(description)
    .bind(serde_json::json!({
        "error_signature": candidate.signature,
        "error_class": candidate.error_class,
        "count_24h": candidate.count_24h,
        "samples": samples,
        "affected_nodes": candidate.affected_nodes,
    }))
    .bind(serde_json::json!({
        "kind": "error_signature",
        "signature": candidate.signature,
    }))
    .fetch_one(&mut **tx)
    .await
    .context("ff-pm-create error-miner bug")
}

async fn auto_file_bugs(pg: &PgPool) -> Result<usize> {
    let mut tx = pg.begin().await?;
    // Serialize the cap and candidate state transition across leader failover.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('error-miner-auto-file'))")
        .execute(&mut *tx)
        .await?;
    let filed_today: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_items
          WHERE created_by = 'error-miner' AND created_at::date = CURRENT_DATE",
    )
    .fetch_one(&mut *tx)
    .await?;
    let remaining = (DAILY_FILE_CAP - filed_today).max(0);
    if remaining == 0 {
        tx.commit().await?;
        return Ok(0);
    }
    let candidates: Vec<FilingCandidate> = sqlx::query_as(
        "SELECT es.signature, es.error_class, es.count_24h, es.sample_text, es.affected_nodes
           FROM error_signatures es
          WHERE es.count_24h >= $1 AND es.state = 'new'
            AND NOT EXISTS (
                SELECT 1 FROM work_items wi
                 WHERE wi.status = ANY($2)
                   AND (wi.metadata->>'error_signature' = es.signature
                        OR wi.original_signal->>'signature' = es.signature))
          ORDER BY es.count_24h DESC, es.signature
          LIMIT $3
          FOR UPDATE SKIP LOCKED",
    )
    .bind(AUTO_FILE_THRESHOLD)
    .bind(OPEN_STATUSES)
    .bind(remaining)
    .fetch_all(&mut *tx)
    .await?;
    for candidate in &candidates {
        let work_item_id = create_work_item(&mut tx, candidate).await?;
        sqlx::query(
            "UPDATE error_signatures
                SET work_item_id = $2, state = 'filed'
              WHERE signature = $1 AND state = 'new'",
        )
        .bind(&candidate.signature)
        .bind(work_item_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(candidates.len())
}

async fn send_digest(pg: &PgPool, day: NaiveDate) -> Result<()> {
    let session_id = format!("{DIGEST_SESSION_PREFIX}-{}", day.format("%Y-%m-%d"));
    let already_sent: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM telegram_messages WHERE session_id = $1)")
            .bind(&session_id)
            .fetch_one(pg)
            .await?;
    if already_sent {
        return Ok(());
    }
    let rows: Vec<(String, Option<String>, i32, String)> = sqlx::query_as(
        "SELECT signature, error_class, count_24h, state
           FROM error_signatures
          WHERE count_24h > 0
          ORDER BY count_24h DESC, signature
          LIMIT 5",
    )
    .fetch_all(pg)
    .await?;
    let body = if rows.is_empty() {
        "No errors observed in the last 24 hours.".to_owned()
    } else {
        rows.into_iter()
            .map(|(signature, class, count, state)| {
                format!(
                    "• {} — {} occurrences — {} [{}]",
                    class.unwrap_or_else(|| signature[..12].to_owned()),
                    count,
                    state,
                    &signature[..12]
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    crate::telegram::send_telegram_recorded(
        pg,
        &format!("ForgeFleet error digest — {}", day.format("%Y-%m-%d")),
        &body,
        &session_id,
    )
    .await?;
    Ok(())
}

/// Run one idempotent daily ErrorMiner pass. The daemon registry leader-gates it.
pub async fn run_error_miner_tick(pg: &PgPool, worker_name: &str) -> Result<()> {
    let day = Utc::now().date_naive();
    let session_id = format!("{DIGEST_SESSION_PREFIX}-{}", day.format("%Y-%m-%d"));
    let already_sent: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM telegram_messages WHERE session_id = $1)")
            .bind(&session_id)
            .fetch_one(pg)
            .await?;
    if already_sent {
        return Ok(());
    }

    let observations = collect_database_errors(pg).await?;
    let aggregates = aggregate_observations(observations);
    persist_error_aggregates(pg, &aggregates).await?;
    collect_journald(pg, day).await?;
    let filed = auto_file_bugs(pg).await?;
    send_digest(pg, day).await?;
    tracing::info!(
        leader = worker_name,
        signatures = aggregates.len(),
        bugs_filed = filed,
        "daily error miner pass complete"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_token_controls_signature() {
        let a = error_signature("error_class=ssh.timeout host 10 failed");
        let b = error_signature("ERROR_CLASS:SSH.TIMEOUT host 99 failed");
        assert_eq!(a.0, b.0);
        assert_eq!(a.1.as_deref(), Some("ssh.timeout"));
    }

    #[test]
    fn normalization_strips_volatile_values() {
        let a = normalize_error_text(
            "Failed /tmp/build-22/src.rs at 123 for 550e8400-e29b-41d4-a716-446655440000 abcdef1234",
        );
        let b = normalize_error_text("FAILED /var/lib/other.rs at 987 for 123");
        assert_eq!(a, b);
        assert_eq!(a, "failed at for");
    }

    #[test]
    fn journal_class_is_first_six_normalized_words() {
        assert_eq!(
            journal_line_class("Jul 24 12:31:02 host service failed to connect endpoint"),
            "jul host service failed to connect"
        );
    }

    #[test]
    fn aggregation_keeps_three_samples_and_nodes() {
        let observations = (0..5)
            .map(|number| Observation {
                text: format!("error_class=db.timeout attempt {number}"),
                node: Some(format!("node-{}", number % 2)),
            })
            .collect();
        let aggregates = aggregate_observations(observations);
        assert_eq!(aggregates.len(), 1);
        assert_eq!(aggregates[0].count, 5);
        assert_eq!(aggregates[0].samples.len(), 3);
        assert_eq!(aggregates[0].nodes.len(), 2);
    }
}
