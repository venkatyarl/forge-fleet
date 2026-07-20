//! Metrics downsampler — periodically writes one row per computer from the
//! live Pulse v2 beats into `computer_metrics_history` (Schema V16).
//!
//! Runs only on the leader. Samples are bucketed at the minute boundary
//! (`date_trunc('minute', now())`) and de-duped via
//! `ON CONFLICT (computer_id, recorded_at) DO NOTHING`, so multiple
//! leaders during election churn cannot produce duplicate rows.
//!
//! Retention and rollups run on the deferred-task retention cron.

use std::time::Duration;

use sqlx::PgPool;
use thiserror::Error;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use ff_pulse::reader::{PulseError, PulseReader};

/// Errors returned by the downsampler.
#[derive(Debug, Error)]
pub enum MetricsError {
    #[error("pulse: {0}")]
    Pulse(#[from] PulseError),
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}

/// Summary of one [`MetricsDownsampler::sample_once`] call.
#[derive(Debug, Default, Clone, Copy)]
pub struct SampleReport {
    /// Rows actually inserted (excludes ON CONFLICT skips).
    pub rows_written: usize,
    /// Rows we saw beats for but skipped because no `computers` row
    /// matched the beat's `computer_name`.
    pub skipped_no_computer_row: usize,
}

/// Periodic downsampler: reads Pulse beats and INSERTs into
/// `computer_metrics_history` at a minute granularity.
pub struct MetricsDownsampler {
    pg: PgPool,
    pulse: PulseReader,
}

impl MetricsDownsampler {
    /// Build a new downsampler.
    pub fn new(pg: PgPool, pulse: PulseReader, _my_name: String) -> Self {
        Self { pg, pulse }
    }

    /// Check whether this process currently owns leadership.
    async fn is_leader(&self) -> bool {
        crate::leader_cache::is_current_leader()
    }

    /// Take one sample: read every live Pulse beat and write one row per
    /// computer. Idempotent at the minute boundary.
    pub async fn sample_once(&self) -> Result<SampleReport, MetricsError> {
        let beats = self.pulse.all_beats().await?;
        let mut report = SampleReport::default();

        for beat in beats {
            // Look up computer_id by name. Beats may report names that aren't
            // yet in `computers` (e.g. mid-enrollment) — skip them cleanly.
            let row: Option<(uuid::Uuid,)> =
                sqlx::query_as::<_, (uuid::Uuid,)>("SELECT id FROM computers WHERE name = $1")
                    .bind(&beat.computer_name)
                    .fetch_optional(&self.pg)
                    .await?;
            let Some((computer_id,)) = row else {
                report.skipped_no_computer_row += 1;
                continue;
            };

            // Aggregate LLM metrics across all active servers on this node.
            let llm_ram_allocated_gb = beat.memory.llm_ram_allocated_gb;
            let (llm_queue_depth, llm_active_requests, llm_tokens_per_sec) = beat
                .llm_servers
                .iter()
                .fold((0i32, 0i32, 0.0f64), |(q, a, t), s| {
                    (
                        q + s.queue_depth,
                        a + s.active_requests,
                        t + s.tokens_per_sec_last_min,
                    )
                });

            let result = sqlx::query(
                r#"
                INSERT INTO computer_metrics_history (
                    computer_id, recorded_at,
                    cpu_pct, ram_pct, ram_used_gb, disk_free_gb, gpu_pct,
                    llm_ram_allocated_gb, llm_queue_depth, llm_active_requests,
                    llm_tokens_per_sec
                )
                VALUES (
                    $1, date_trunc('minute', NOW()),
                    $2, $3, $4, $5, $6,
                    $7, $8, $9, $10
                )
                ON CONFLICT (computer_id, recorded_at) DO NOTHING
                "#,
            )
            .bind(computer_id)
            .bind(beat.load.cpu_pct)
            .bind(beat.load.ram_pct)
            .bind(beat.memory.ram_used_gb)
            .bind(beat.load.disk_free_gb)
            .bind(beat.load.gpu_pct)
            .bind(llm_ram_allocated_gb)
            .bind(llm_queue_depth)
            .bind(llm_active_requests)
            .bind(llm_tokens_per_sec)
            .execute(&self.pg)
            .await?;

            if result.rows_affected() > 0 {
                report.rows_written += 1;
            }
        }

        Ok(report)
    }

    /// Spawn a background task that samples every 60 seconds. Intended to be
    /// started only on the leader (callers must gate accordingly); the task
    /// itself does not check leadership.
    pub fn spawn(self, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(60));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        if !self.is_leader().await {
                            continue;
                        }
                        match self.sample_once().await {
                            Ok(report) => {
                                tracing::debug!(
                                    rows = report.rows_written,
                                    skipped = report.skipped_no_computer_row,
                                    "metrics downsample tick"
                                );
                            }
                            Err(err) => {
                                tracing::warn!(error = %err, "metrics downsample failed");
                            }
                        }
                    }
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            tracing::info!("metrics downsampler shutting down");
                            break;
                        }
                    }
                }
            }
        })
    }
}

/// Pretty-printable row used by `ff metrics history`.
#[derive(Debug, Clone)]
pub struct MetricRow {
    pub recorded_at: chrono::DateTime<chrono::Utc>,
    pub cpu_pct: Option<f64>,
    pub ram_pct: Option<f64>,
    pub ram_used_gb: Option<f64>,
    pub disk_free_gb: Option<f64>,
    pub gpu_pct: Option<f64>,
    pub llm_queue_depth: Option<i32>,
    pub llm_active_requests: Option<i32>,
    pub llm_tokens_per_sec: Option<f64>,
}

/// Fetch rows for a computer name covering the last `since_secs` seconds.
pub async fn history_for_computer(
    pg: &PgPool,
    computer_name: &str,
    since_secs: i64,
) -> Result<Vec<MetricRow>, sqlx::Error> {
    let rows = sqlx::query_as::<
        _,
        (
            chrono::DateTime<chrono::Utc>,
            Option<f64>,
            Option<f64>,
            Option<f64>,
            Option<f64>,
            Option<f64>,
            Option<f64>,
            Option<f64>,
            Option<f64>,
        ),
    >(
        r#"
        SELECT recorded_at, cpu_pct, ram_pct, ram_used_gb, disk_free_gb, gpu_pct,
               llm_queue_depth, llm_active_requests, llm_tokens_per_sec
        FROM (
            SELECT computer_id, recorded_at, cpu_pct, ram_pct, ram_used_gb,
                   disk_free_gb, gpu_pct, llm_queue_depth::double precision,
                   llm_active_requests::double precision, llm_tokens_per_sec
              FROM computer_metrics_history
             WHERE recorded_at >= NOW() - INTERVAL '7 days'
            UNION ALL
            SELECT computer_id, recorded_at, cpu_pct, ram_pct, ram_used_gb,
                   disk_free_gb, gpu_pct, llm_queue_depth,
                   llm_active_requests, llm_tokens_per_sec
              FROM computer_metrics_history_hourly
             WHERE recorded_at < NOW() - INTERVAL '7 days'
               AND recorded_at >= NOW() - INTERVAL '90 days'
            UNION ALL
            SELECT computer_id, recorded_at, cpu_pct, ram_pct, ram_used_gb,
                   disk_free_gb, gpu_pct, llm_queue_depth,
                   llm_active_requests, llm_tokens_per_sec
              FROM computer_metrics_history_daily
             WHERE recorded_at < NOW() - INTERVAL '90 days'
        ) m
        JOIN computers c ON c.id = m.computer_id
        WHERE c.name = $1
          AND recorded_at > NOW() - ($2 || ' seconds')::interval
        ORDER BY recorded_at ASC
        "#,
    )
    .bind(computer_name)
    .bind(since_secs.to_string())
    .fetch_all(pg)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(recorded_at, cpu_pct, ram_pct, ram_used_gb, disk_free_gb, gpu_pct, q, a, t)| {
                MetricRow {
                    recorded_at,
                    cpu_pct,
                    ram_pct,
                    ram_used_gb,
                    disk_free_gb,
                    gpu_pct,
                    llm_queue_depth: q.map(|v| v.round() as i32),
                    llm_active_requests: a.map(|v| v.round() as i32),
                    llm_tokens_per_sec: t,
                }
            },
        )
        .collect())
}
