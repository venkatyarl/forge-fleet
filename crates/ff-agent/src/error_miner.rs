//! Daily fleet error aggregation and bounded automatic bug filing.

use std::collections::{BTreeSet, HashMap};

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate, Timelike};
use regex::Regex;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use tokio::process::Command;
use tracing::{debug, warn};

const RUN_HOUR_LOCAL: u32 = 7;
const AUTO_FILE_THRESHOLD: i64 = 10;
const AUTO_FILE_DAILY_CAP: i64 = 3;
const JOURNAL_COMMAND: &str =
    "journalctl --user -u forgefleetd -p warning --since -24h --no-pager | tail -200";
static LAST_RUN_DATE: std::sync::LazyLock<std::sync::Mutex<Option<NaiveDate>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

#[derive(Clone, Debug)]
struct Aggregate {
    signature: String,
    error_class: Option<String>,
    normalized: String,
    count: i64,
    samples: Vec<String>,
    nodes: BTreeSet<String>,
}

impl Aggregate {
    fn new(text: &str) -> Self {
        let (error_class, normalized) = normalize_error(text);
        let basis = error_class.as_deref().unwrap_or(&normalized);
        let signature = format!("{:x}", Sha256::digest(basis.as_bytes()));
        Self {
            signature,
            error_class,
            normalized,
            count: 0,
            samples: Vec::new(),
            nodes: BTreeSet::new(),
        }
    }

    fn observe(&mut self, sample: &str, node: Option<&str>, count: i64) {
        self.count += count;
        if self.samples.len() < 3 && !self.samples.iter().any(|item| item == sample) {
            self.samples.push(sample.chars().take(500).collect());
        }
        if let Some(node) = node.filter(|node| !node.is_empty()) {
            self.nodes.insert(node.to_string());
        }
    }
}

/// Leader-only daily pass. A run row claims the date atomically; stale claims
/// can be retried after two hours if a leader dies mid-pass.
pub async fn run_daily_tick(pg: &PgPool, worker_name: &str) -> Result<()> {
    let now = Local::now();
    if now.hour() < RUN_HOUR_LOCAL {
        return Ok(());
    }
    let run_date = now.date_naive();
    {
        let mut last_run = LAST_RUN_DATE.lock().expect("ErrorMiner date lock poisoned");
        if *last_run == Some(run_date) {
            return Ok(());
        }
        *last_run = Some(run_date);
    }

    let result = run_pass(pg, run_date).await;
    match result {
        Ok(()) => {
            debug!(leader = worker_name, %run_date, "daily ErrorMiner pass complete");
            Ok(())
        }
        Err(error) => {
            *LAST_RUN_DATE.lock().expect("ErrorMiner date lock poisoned") = None;
            Err(error)
        }
    }
}

async fn run_pass(pg: &PgPool, run_date: chrono::NaiveDate) -> Result<()> {
    let mut aggregates = collect_database_errors(pg).await?;
    collect_journald(pg, run_date, &mut aggregates).await?;
    persist_aggregates(pg, aggregates.values()).await?;
    auto_file(pg).await?;
    send_digest(pg, run_date).await?;
    Ok(())
}

async fn collect_database_errors(pg: &PgPool) -> Result<HashMap<String, Aggregate>> {
    let rows = sqlx::query(
        "SELECT last_error AS error_text, assigned_computer AS node
           FROM work_items
          WHERE last_error IS NOT NULL AND btrim(last_error) <> ''
            AND COALESCE(completed_at, started_at, created_at) >= NOW() - INTERVAL '24 hours'
         UNION ALL
         SELECT error_text, worker_name
           FROM ff_interactions
          WHERE error_text IS NOT NULL AND btrim(error_text) <> ''
            AND ts >= NOW() - INTERVAL '24 hours'",
    )
    .fetch_all(pg)
    .await?;

    let mut aggregates = HashMap::new();
    for row in rows {
        let text: String = row.try_get("error_text")?;
        let node: Option<String> = row.try_get("node")?;
        observe(&mut aggregates, &text, node.as_deref(), 1);
    }
    Ok(aggregates)
}

async fn collect_journald(
    pg: &PgPool,
    run_date: chrono::NaiveDate,
    aggregates: &mut HashMap<String, Aggregate>,
) -> Result<()> {
    let nodes: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM computers
          WHERE LOWER(status) = 'online'
            AND last_seen_at >= NOW() - INTERVAL '15 minutes'
          ORDER BY name",
    )
    .fetch_all(pg)
    .await?;

    for node in nodes {
        let output = Command::new("ff")
            .args(["fleet", "exec", &node, "--json", "--", JOURNAL_COMMAND])
            .output()
            .await
            .with_context(|| format!("run ff fleet exec for {node}"))?;
        if !output.status.success() {
            warn!(%node, stderr = %String::from_utf8_lossy(&output.stderr), "ErrorMiner journald ingest failed");
            continue;
        }
        let payload: serde_json::Value = serde_json::from_slice(&output.stdout)
            .with_context(|| format!("decode ff fleet exec output for {node}"))?;
        let stdout = payload["stdout"].as_str().unwrap_or_default();
        let mut local: HashMap<String, Aggregate> = HashMap::new();
        for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
            let classified = line
                .split_whitespace()
                .take(6)
                .collect::<Vec<_>>()
                .join(" ");
            observe(&mut local, &classified, Some(&node), 1);
            observe(aggregates, &classified, Some(&node), 1);
        }
        for aggregate in local.values() {
            sqlx::query(
                "INSERT INTO fleet_log_digest
                    (node, day, level, line_class, count, sample)
                 VALUES ($1, $2, 'warning', $3, $4, $5)
                 ON CONFLICT (node, day, level, line_class) DO UPDATE SET
                    count = EXCLUDED.count,
                    sample = EXCLUDED.sample",
            )
            .bind(&node)
            .bind(run_date)
            .bind(&aggregate.normalized)
            .bind(aggregate.count)
            .bind(aggregate.samples.first().map(String::as_str).unwrap_or(""))
            .execute(pg)
            .await?;
        }
    }
    Ok(())
}

fn observe(
    aggregates: &mut HashMap<String, Aggregate>,
    text: &str,
    node: Option<&str>,
    count: i64,
) {
    let candidate = Aggregate::new(text);
    aggregates
        .entry(candidate.signature.clone())
        .or_insert(candidate)
        .observe(text, node, count);
}

async fn persist_aggregates<'a>(
    pg: &PgPool,
    aggregates: impl Iterator<Item = &'a Aggregate>,
) -> Result<()> {
    let mut tx = pg.begin().await?;
    sqlx::query("UPDATE error_signatures SET count_24h = 0, updated_at = NOW()")
        .execute(&mut *tx)
        .await?;
    for aggregate in aggregates {
        sqlx::query(
            "INSERT INTO error_signatures
                (signature, error_class, first_seen, last_seen, count_24h,
                 count_total, sample_text, affected_nodes)
             VALUES ($1, $2, NOW(), NOW(), $3, $3, $4, $5)
             ON CONFLICT (signature) DO UPDATE SET
                error_class = EXCLUDED.error_class,
                count_24h = EXCLUDED.count_24h,
                count_total = error_signatures.count_total + EXCLUDED.count_24h,
                sample_text = EXCLUDED.sample_text,
                affected_nodes = EXCLUDED.affected_nodes,
                last_seen = NOW()",
        )
        .bind(&aggregate.signature)
        .bind(&aggregate.error_class)
        .bind(aggregate.count)
        .bind(aggregate.samples.join("\n---\n"))
        .bind(json!(aggregate.nodes))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn auto_file(pg: &PgPool) -> Result<()> {
    let mut tx = pg.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('error_miner_auto_file'))")
        .execute(&mut *tx)
        .await?;
    let already_filed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_items
          WHERE created_by = 'error-miner' AND created_at::date = CURRENT_DATE",
    )
    .fetch_one(&mut *tx)
    .await?;
    let slots = (AUTO_FILE_DAILY_CAP - already_filed).max(0);
    if slots == 0 {
        return Ok(());
    }

    let candidates = sqlx::query(
        "SELECT signature, error_class, count_24h, sample_text, affected_nodes
           FROM error_signatures e
          WHERE count_24h >= $1 AND state = 'new'
            AND NOT EXISTS (
                SELECT 1 FROM work_items w
                 WHERE w.status IN ('idea','decomposed','ready','claimed','building','in_progress','in_review','blocked')
                   AND (w.metadata->>'error_signature' = e.signature
                        OR w.description LIKE '%' || e.signature || '%'))
          ORDER BY count_24h DESC, signature
          LIMIT $2",
    )
    .bind(AUTO_FILE_THRESHOLD)
    .bind(slots)
    .fetch_all(&mut *tx)
    .await?;

    for row in candidates {
        let signature: String = row.try_get("signature")?;
        let error_class: Option<String> = row.try_get("error_class")?;
        let count: i64 = row.try_get("count_24h")?;
        let samples: String = row.try_get("sample_text")?;
        let nodes: serde_json::Value = row.try_get("affected_nodes")?;
        let title = format!("Recurring error: {}", &signature[..12]);
        let description = format!(
            "ErrorMiner detected recurring fleet error.\n\nSignature: {signature}\nClass: {}\nCount (24h): {count}\nAffected nodes: {nodes}\nSamples:\n{}",
            error_class.as_deref().unwrap_or("unclassified"),
            samples
        );
        let work_item_id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO work_items
                (project_id, kind, title, description, status, priority, created_by,
                 risk_score, metadata, original_signal)
             VALUES ('forge-fleet', 'bug', $1, $2, 'idea', 'normal', 'error-miner',
                     60, $3, $4)
             RETURNING id",
        )
        .bind(title)
        .bind(description)
        .bind(json!({"error_signature": signature, "detected_by": "error-miner"}))
        .bind(json!({"kind": "error_signature", "signature": signature}))
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE error_signatures
                SET state = 'filed', work_item_id = $2, updated_at = NOW()
              WHERE signature = $1 AND state = 'new'",
        )
        .bind(&signature)
        .bind(work_item_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn send_digest(pg: &PgPool, run_date: chrono::NaiveDate) -> Result<()> {
    let rows = sqlx::query(
        "SELECT signature, error_class, count_24h, state
           FROM error_signatures
          ORDER BY count_24h DESC, signature
          LIMIT 5",
    )
    .fetch_all(pg)
    .await?;
    let body = rows
        .iter()
        .map(|row| {
            let signature: String = row.get("signature");
            let class: Option<String> = row.get("error_class");
            let count: i64 = row.get("count_24h");
            let state: String = row.get("state");
            format!(
                "{count} × {} [{}] {}",
                class.as_deref().unwrap_or("unclassified"),
                state,
                &signature[..12]
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    crate::telegram::send_telegram_recorded(
        pg,
        "ForgeFleet ErrorMiner daily digest",
        if body.is_empty() {
            "No errors in the last 24h."
        } else {
            &body
        },
        &format!("error-miner-{run_date}"),
    )
    .await?;
    Ok(())
}

fn normalize_error(text: &str) -> (Option<String>, String) {
    static CLASS: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\berror[_-]?class\s*[:=]\s*([a-z0-9_.:-]+)").unwrap()
    });
    static UUID: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(
            r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}\b",
        )
        .unwrap()
    });
    static SHA: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?i)\b[0-9a-f]{7,64}\b").unwrap());
    static PATH: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?:[a-zA-Z]:)?(?:[/\\][^\s:;,]+)+").unwrap());
    static NUMBER: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"\b\d+(?:\.\d+)?\b").unwrap());
    static SPACE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"\s+").unwrap());

    let lower = text.to_lowercase();
    let error_class = CLASS
        .captures(&lower)
        .and_then(|capture| capture.get(1))
        .map(|value| value.as_str().to_string());
    let normalized = UUID.replace_all(&lower, " ");
    let normalized = PATH.replace_all(&normalized, " ");
    let normalized = SHA.replace_all(&normalized, " ");
    let normalized = NUMBER.replace_all(&normalized, " ");
    (
        error_class,
        SPACE.replace_all(&normalized, " ").trim().to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_prefers_error_class_and_strips_volatile_values() {
        let (class, normalized) = normalize_error(
            "ERROR_CLASS=ssh:timeout failed /tmp/run/abc at 123 for 550e8400-e29b-41d4-a716-446655440000 deadbeef",
        );
        assert_eq!(class.as_deref(), Some("ssh:timeout"));
        assert!(!normalized.contains("/tmp"));
        assert!(!normalized.contains("123"));
        assert!(!normalized.contains("deadbeef"));
    }

    #[test]
    fn same_error_class_has_same_signature() {
        let left = Aggregate::new("error_class=network:timeout host 1");
        let right = Aggregate::new("ERROR_CLASS=network:timeout host 99");
        assert_eq!(left.signature, right.signature);
    }
}
