//! Fleet task-coverage guard (Phase 7).
//!
//! Keeps the fleet "always-on" for the set of tasks the operator declared
//! required in `fleet_task_coverage`. For each row the guard:
//!
//!   1. Counts how many currently-active deployments serve the task
//!      (`computer_model_deployments.status = 'active'` joined to a
//!      non-retired `model_catalog` row with `tasks @> [task]`).
//!   2. If the count is below `min_models_loaded`, picks the best catalog
//!      candidate (flagship → standard, preferring smaller/Q4 quants so
//!      a 32GB box can run it) and enqueues a deferred `ff model load`
//!      shell task targeted at a computer that can host it.
//!   3. If no viable candidate exists, records a gap. Gaps are
//!      de-spammed: the same task is only surfaced once per hour.
//!
//! Runs every 15 minutes by default on the elected leader only.
//! The `PulseReader` field is reserved for future per-computer liveness
//! filtering; today we fall back to the DB snapshot (status='online')
//! when the reader isn't wired, so this module is usable even without
//! Redis.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use ff_pulse::reader::PulseReader;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use thiserror::Error;
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// How long to silence a repeat gap alert for the same task (1h).
const GAP_ALERT_COOLDOWN: Duration = Duration::from_secs(3600);

/// Errors that can occur while running the guard.
#[derive(Debug, Error)]
pub enum CoverageError {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
}

/// One row returned by [`CoverageGuard::check_once`] for a task that
/// cannot currently be satisfied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageGap {
    pub task: String,
    pub min_required: i32,
    pub currently_loaded: i32,
    /// Catalog ids that *could* serve the task (smallest/best quant first),
    /// even if no computer can host them right now.
    pub candidates: Vec<String>,
}

/// Result of one guard pass.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CoverageReport {
    pub tasks_required: usize,
    pub tasks_covered: usize,
    pub gaps: Vec<CoverageGap>,
    /// Catalog ids for which we enqueued a load-this-now deferred task.
    pub auto_loaded: Vec<String>,
}

/// Fleet task-coverage guard.
///
/// Clone-on-spawn friendly — holds an `Arc<Mutex<_>>` for the alert
/// dedup table so `spawn()` can move the guard while `check_once()`
/// continues to be callable from CLI handlers.
#[derive(Clone)]
pub struct CoverageGuard {
    pg: PgPool,
    #[allow(dead_code)]
    pulse: Option<Arc<PulseReader>>,
    last_alerted: Arc<Mutex<HashMap<String, DateTime<Utc>>>>,
}

impl CoverageGuard {
    /// Build a guard with the given Postgres pool and an optional
    /// pulse reader. `pulse` is currently only used as a future hook
    /// for liveness-aware scheduling.
    pub fn new(pg: PgPool, pulse: Option<Arc<PulseReader>>) -> Self {
        Self {
            pg,
            pulse,
            last_alerted: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Build a guard without a pulse reader (DB-only mode).
    pub fn new_dbonly(pg: PgPool) -> Self {
        Self::new(pg, None)
    }

    /// Run one coverage pass. See the module docs for the algorithm.
    pub async fn check_once(&self) -> Result<CoverageReport, CoverageError> {
        let required = sqlx::query(
            "SELECT task, min_models_loaded, preferred_model_ids, priority
             FROM fleet_task_coverage
             ORDER BY
               CASE priority
                 WHEN 'critical' THEN 0
                 WHEN 'normal' THEN 1
                 WHEN 'nice-to-have' THEN 2
                 ELSE 3
               END,
               task",
        )
        .fetch_all(&self.pg)
        .await?;

        let mut report = CoverageReport {
            tasks_required: required.len(),
            ..CoverageReport::default()
        };

        for row in required {
            let task: String = row.get("task");
            let min_required: i32 = row.get("min_models_loaded");
            let preferred_json: serde_json::Value = row.get("preferred_model_ids");
            let preferred_ids: Vec<String> = preferred_json
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            // How many active deployments cover this task right now?
            let count_row = sqlx::query(
                "SELECT COUNT(*)::INT AS n
                 FROM computer_model_deployments d
                 JOIN model_catalog mc ON mc.id = d.model_id
                 WHERE d.status = 'active'
                   AND mc.tasks @> to_jsonb(ARRAY[$1]::text[])
                   AND mc.lifecycle_status <> 'retired'",
            )
            .bind(&task)
            .fetch_one(&self.pg)
            .await?;
            let currently_loaded: i32 = count_row.get("n");

            if currently_loaded >= min_required {
                report.tasks_covered += 1;
                continue;
            }

            // Find candidate catalog rows that *could* cover this task.
            let candidates = self.rank_candidates(&task, &preferred_ids).await?;

            // Try to auto-load the best candidate on any suitable computer.
            let mut enqueued = None;
            for cand in &candidates {
                match self.pick_host_for(&cand.id, cand.min_vram_gb).await? {
                    Some(host) => {
                        let defer_id = self.enqueue_load(&cand.id, &host).await?;
                        info!(
                            task = %task,
                            model = %cand.id,
                            host = %host,
                            defer_id = %defer_id,
                            "coverage guard enqueued auto-load"
                        );
                        report.auto_loaded.push(cand.id.clone());
                        enqueued = Some(cand.id.clone());
                        break;
                    }
                    None => continue,
                }
            }

            if enqueued.is_none() {
                // Dedup: only surface the same gap once per hour.
                if self.should_alert(&task).await {
                    warn!(
                        task = %task,
                        min_required,
                        currently_loaded,
                        "coverage gap — no viable candidate host available"
                    );
                }
                report.gaps.push(CoverageGap {
                    task,
                    min_required,
                    currently_loaded,
                    candidates: candidates.into_iter().map(|c| c.id).collect(),
                });
            } else {
                // Auto-load enqueued — count as covered once it lands.
                report.tasks_covered += 1;
            }
        }

        info!(
            tasks_required = report.tasks_required,
            tasks_covered = report.tasks_covered,
            gaps = report.gaps.len(),
            auto_loaded = report.auto_loaded.len(),
            "coverage guard pass complete"
        );

        Ok(report)
    }

    /// Spawn a background tick that runs [`Self::check_once`] every
    /// `interval_mins`. Exits when `shutdown` flips to `true`.
    pub fn spawn(self, interval_mins: u64, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        let interval = Duration::from_secs(interval_mins.max(1) * 60);
        let kickoff = Duration::from_secs(90);

        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(kickoff) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return; }
                }
            }

            loop {
                match self.check_once().await {
                    Ok(report) => debug!(
                        tasks_required = report.tasks_required,
                        tasks_covered = report.tasks_covered,
                        gaps = report.gaps.len(),
                        auto_loaded = report.auto_loaded.len(),
                        "coverage guard tick"
                    ),
                    Err(err) => warn!(error = %err, "coverage guard tick failed"),
                }

                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() { break; }
                    }
                }
            }
        })
    }

    /// Return true iff we haven't alerted about this task in the last
    /// hour; updates the dedup table as a side effect when we do alert.
    async fn should_alert(&self, task: &str) -> bool {
        let now = Utc::now();
        let mut table = self.last_alerted.lock().await;
        if let Some(last) = table.get(task) {
            let elapsed = now.signed_duration_since(*last);
            if elapsed
                < chrono::Duration::from_std(GAP_ALERT_COOLDOWN).unwrap_or(chrono::Duration::zero())
            {
                return false;
            }
        }
        table.insert(task.to_string(), now);
        true
    }

    /// Rank candidate catalog rows for a task. Preferred ids from the
    /// coverage row sort first; then `quality_tier='flagship'` ahead
    /// of `'standard'`; then smallest `file_size_gb` (so low-RAM boxes
    /// get a shot); Q4 quants tie-break ahead of larger quants.
    async fn rank_candidates(
        &self,
        task: &str,
        preferred: &[String],
    ) -> Result<Vec<Candidate>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, quality_tier, quantization, file_size_gb, min_vram_gb
             FROM model_catalog
             WHERE tasks @> to_jsonb(ARRAY[$1]::text[])
               AND lifecycle_status = 'active'",
        )
        .bind(task)
        .fetch_all(&self.pg)
        .await?;

        let mut candidates: Vec<Candidate> = rows
            .iter()
            .map(|r| {
                let id: String = r.get("id");
                Candidate {
                    preferred: preferred.iter().any(|p| p == &id),
                    id,
                    quality_tier: r.get("quality_tier"),
                    quantization: r.get("quantization"),
                    file_size_gb: r.get("file_size_gb"),
                    min_vram_gb: r.get("min_vram_gb"),
                }
            })
            .collect();

        candidates.sort_by(|a, b| {
            let a_pref = if a.preferred { 0 } else { 1 };
            let b_pref = if b.preferred { 0 } else { 1 };

            let a_tier = tier_rank(a.quality_tier.as_deref());
            let b_tier = tier_rank(b.quality_tier.as_deref());

            let a_q4 = if is_q4(a.quantization.as_deref()) {
                0
            } else {
                1
            };
            let b_q4 = if is_q4(b.quantization.as_deref()) {
                0
            } else {
                1
            };

            let a_size = a.file_size_gb.unwrap_or(f64::MAX);
            let b_size = b.file_size_gb.unwrap_or(f64::MAX);

            a_pref
                .cmp(&b_pref)
                .then(a_tier.cmp(&b_tier))
                .then(a_q4.cmp(&b_q4))
                .then(
                    a_size
                        .partial_cmp(&b_size)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
        });

        Ok(candidates)
    }

    /// Pick a computer that can host `model_id`:
    /// - online (`computers.status = 'online'`),
    /// - no active deployment of this model already,
    /// - enough RAM (`total_ram_gb >= min_vram_gb`, or `has_gpu` with sufficient VRAM).
    async fn pick_host_for(
        &self,
        model_id: &str,
        min_vram_gb: Option<f64>,
    ) -> Result<Option<String>, sqlx::Error> {
        let required = min_vram_gb.unwrap_or(0.0);

        // Only consider hosts that ALREADY have the model file in their
        // library — otherwise `ff model load <id>` fails on the chosen host
        // with `no library entry with id '<id>'`. Auto-download is a
        // separate concern (handled by hf_download / model_library_scanner).
        let row = sqlx::query(
            "SELECT c.name AS name
             FROM computers c
             WHERE c.status = 'online'
               AND EXISTS (
                   SELECT 1 FROM fleet_model_library lib
                    WHERE lib.node_name = c.name
                      AND lib.catalog_id = $1
               )
               AND NOT EXISTS (
                   SELECT 1 FROM computer_model_deployments d
                    WHERE d.computer_id = c.id
                      AND d.model_id = $1
                      AND d.status IN ('active', 'loading')
               )
               AND (
                   (c.has_gpu AND COALESCE(c.gpu_total_vram_gb, 0) >= $2)
                   OR COALESCE(c.total_ram_gb, 0) >= $2
               )
             ORDER BY COALESCE(c.gpu_total_vram_gb, c.total_ram_gb::float, 0) DESC
             LIMIT 1",
        )
        .bind(model_id)
        .bind(required)
        .fetch_optional(&self.pg)
        .await?;

        Ok(row.map(|r| r.get("name")))
    }

    /// Enqueue a deferred shell task that invokes `ff model load <id>` on
    /// the chosen host. Runs on `node_online` so it re-fires if the box
    /// restarts before it executes.
    async fn enqueue_load(&self, model_id: &str, host_name: &str) -> Result<String, sqlx::Error> {
        let title = format!("coverage-guard auto-load {model_id} on {host_name}");
        let command = format!("ff model load {model_id}");
        let payload = serde_json::json!({ "command": command });
        let trigger_spec = serde_json::json!({ "node": host_name });

        ff_db::pg_enqueue_deferred(
            &self.pg,
            &title,
            "shell",
            &payload,
            "node_online",
            &trigger_spec,
            Some(host_name),
            &serde_json::json!([]),
            Some("coverage_guard"),
            Some(3),
        )
        .await
        .map_err(|e| sqlx::Error::Protocol(format!("pg_enqueue_deferred: {e}")))
    }
}

struct Candidate {
    id: String,
    preferred: bool,
    quality_tier: Option<String>,
    quantization: Option<String>,
    file_size_gb: Option<f64>,
    min_vram_gb: Option<f64>,
}

fn tier_rank(tier: Option<&str>) -> u8 {
    match tier {
        Some("flagship") => 0,
        Some("standard") => 1,
        Some("experimental") => 2,
        _ => 3,
    }
}

fn is_q4(q: Option<&str>) -> bool {
    q.map(|s| {
        let lo = s.to_ascii_lowercase();
        lo.contains("q4") || lo.contains("4bit") || lo.contains("int4")
    })
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_ranks_flagship_first() {
        assert!(tier_rank(Some("flagship")) < tier_rank(Some("standard")));
        assert!(tier_rank(Some("standard")) < tier_rank(Some("experimental")));
        assert!(tier_rank(None) > tier_rank(Some("experimental")));
    }

    #[test]
    fn q4_detected() {
        assert!(is_q4(Some("Q4_K_M")));
        assert!(is_q4(Some("q4_0")));
        assert!(is_q4(Some("4bit")));
        assert!(!is_q4(Some("Q8_0")));
        assert!(!is_q4(None));
    }
}
