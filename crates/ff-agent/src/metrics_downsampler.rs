//! Metrics downsampler — periodically writes one row per computer from the
//! live Pulse v2 beats into `computer_metrics_history` (Schema V16).
//!
//! Runs only on the leader. Samples are bucketed at the minute boundary
//! (`date_trunc('minute', now())`) and de-duped via
//! `ON CONFLICT (computer_id, recorded_at) DO NOTHING`, so multiple
//! leaders during election churn cannot produce duplicate rows.
//!
//! Retention is applied by [`delete_older_than_days`]; the daemon calls it
//! once per day after the first successful sample.

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
    /// Name of this node — used to gate sampling on leadership.
    my_name: String,
}

impl MetricsDownsampler {
    /// Build a new downsampler.
    pub fn new(pg: PgPool, pulse: PulseReader, my_name: String) -> Self {
        Self { pg, pulse, my_name }
    }

    /// Check whether this node currently owns the leader singleton.
    async fn is_leader(&self) -> bool {
        match sqlx::query_scalar::<_, String>("SELECT member_name FROM fleet_leader_state LIMIT 1")
            .fetch_optional(&self.pg)
            .await
        {
            Ok(Some(leader)) => leader == self.my_name,
            Ok(None) => false,
            Err(_) => false,
        }
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
            // Track last retention run to apply it at most once per day.
            let mut last_retention = std::time::Instant::now() - Duration::from_secs(86_400);

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

                        if last_retention.elapsed() > Duration::from_secs(86_400) {
                            match delete_older_than_days(&self.pg, 90).await {
                                Ok(n) => {
                                    tracing::info!(rows = n, "metrics retention sweep deleted old rows");
                                    last_retention = std::time::Instant::now();
                                }
                                Err(e) => tracing::warn!(error = %e, "metrics retention sweep failed"),
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

/// Delete metrics history older than `days` days. Returns the row count.
pub async fn delete_older_than_days(pg: &PgPool, days: i32) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM computer_metrics_history WHERE recorded_at < NOW() - ($1 || ' days')::interval",
    )
    .bind(days.to_string())
    .execute(pg)
    .await?;
    Ok(result.rows_affected())
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
            Option<i32>,
            Option<i32>,
            Option<f64>,
        ),
    >(
        r#"
        SELECT recorded_at, cpu_pct, ram_pct, ram_used_gb, disk_free_gb, gpu_pct,
               llm_queue_depth, llm_active_requests, llm_tokens_per_sec
        FROM computer_metrics_history m
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
                    llm_queue_depth: q,
                    llm_active_requests: a,
                    llm_tokens_per_sec: t,
                }
            },
        )
        .collect())
}
