//! Periodic DB integrity guard — runs PostgreSQL `amcheck` over every valid
//! btree UNIQUE index and raises a fleet alert on corruption.
//!
//! Motivation: on 2026-05-30 a glibc/ICU collation upgrade silently corrupted
//! several btree UNIQUE indexes (their on-disk ordering no longer matched the
//! new collation). It was only caught by hand. This tick catches it
//! automatically next time.
//!
//! Design notes:
//!   - **Leader-gated, checked every fire** (NOT at spawn). Uses
//!     [`ff_db::leader_state::pg_get_current_leader`] and requires the leader
//!     row's `member_name` to match this node AND its `heartbeat_at` to be
//!     fresher than [`LEADER_FRESH_SECS`]. We deliberately do NOT copy
//!     `scheduler_tick`'s gate — that one queries nonexistent columns
//!     (`leader_name`/`last_heartbeat`) and is broken.
//!   - **Alert-only.** On detecting >=1 corrupt index we INSERT an
//!     `alert_event` AND call [`crate::alert_evaluator::dispatch_alert`]
//!     directly. We never write `channel_result='pending'` and rely on a
//!     pickup loop — nothing dispatches pending rows (the `secrets_rotation`
//!     bug). We also never auto-REINDEX (per "updates never auto-applied").
//!   - **Cadence:** [`CHECK_INTERVAL`] (6h) with `MissedTickBehavior::Skip`
//!     so a slow check on a huge DB can't pile up backlogged ticks.

use std::time::Duration;

use sqlx::{PgPool, Row};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// How often the integrity check runs. amcheck's `heapallindexed` scan reads
/// every index AND its heap, so we keep this infrequent.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// The leader's Postgres `heartbeat_at` must be fresher than this for us to
/// consider ourselves the live leader. Matches the 60s window used by the
/// `leader_heartbeat_stale` alert policy.
const LEADER_FRESH_SECS: i64 = 60;

/// The alert policy name seeded by migration V110.
const POLICY_NAME: &str = "db_index_corruption";

/// One corrupt index found during a check pass.
#[derive(Debug, Clone)]
pub struct CorruptIndex {
    pub index_name: String,
    /// SQLSTATE code if the failure carried one (e.g. `XX002` =
    /// `index_corrupted`), else `None`.
    pub sqlstate: Option<String>,
    pub message: String,
}

/// Summary of one integrity check pass.
#[derive(Debug, Default, Clone)]
pub struct IntegrityReport {
    pub checked: usize,
    pub ok: usize,
    pub corrupt: Vec<CorruptIndex>,
}

/// The integrity guard tick. Spawned on every daemon; no-ops on followers via
/// the per-fire leader gate.
pub struct AmcheckTick {
    pg: PgPool,
    my_name: String,
}

impl AmcheckTick {
    pub fn new(pg: PgPool, my_name: String) -> Self {
        Self { pg, my_name }
    }

    /// Are we the live leader right now? True iff the `fleet_leader_state`
    /// singleton names us AND its heartbeat is fresh.
    async fn is_live_leader(&self) -> bool {
        match ff_db::leader_state::pg_get_current_leader(&self.pg).await {
            Ok(Some(leader)) => {
                let fresh = chrono::Utc::now()
                    .signed_duration_since(leader.heartbeat_at)
                    .num_seconds()
                    < LEADER_FRESH_SECS;
                leader.member_name == self.my_name && fresh
            }
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(error = %e, "amcheck: failed to read leader state");
                false
            }
        }
    }

    /// List every valid btree UNIQUE index in the `public` schema, as
    /// `(oid, relname)`. The oid is returned as `int8` so it binds cleanly as
    /// an `i64` and is cast back to `oid`/`regclass` in the check query.
    async fn list_unique_btree_indexes(&self) -> Result<Vec<(i64, String)>, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            SELECT c.oid::int8 AS oid, c.relname AS relname
            FROM pg_index i
            JOIN pg_class c  ON c.oid = i.indexrelid
            JOIN pg_am    am ON am.oid = c.relam
            WHERE i.indisunique
              AND i.indisvalid
              AND am.amname = 'btree'
              AND c.relnamespace = 'public'::regnamespace
            ORDER BY c.relname
            "#,
        )
        .fetch_all(&self.pg)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| (r.get::<i64, _>("oid"), r.get::<String, _>("relname")))
            .collect())
    }

    /// Run one full integrity pass over all unique btree indexes.
    ///
    /// Each index is checked in its own statement; a failure is caught here
    /// (the sqlx error == amcheck reporting corruption) and recorded rather
    /// than aborting the whole pass.
    pub async fn run_check_once(&self) -> Result<IntegrityReport, sqlx::Error> {
        let indexes = self.list_unique_btree_indexes().await?;
        let mut report = IntegrityReport {
            checked: indexes.len(),
            ..Default::default()
        };

        for (oid, name) in indexes {
            // `index => $1::oid` binds the index by oid (cast to regclass by
            // amcheck). `heapallindexed => true` also verifies every heap
            // tuple has a matching index entry — this is what catches
            // collation drift.
            match sqlx::query("SELECT bt_index_check(index => $1::oid, heapallindexed => true)")
                .bind(oid)
                .execute(&self.pg)
                .await
            {
                Ok(_) => report.ok += 1,
                Err(e) => {
                    let sqlstate = match &e {
                        sqlx::Error::Database(db) => db.code().map(|c| c.into_owned()),
                        _ => None,
                    };
                    tracing::warn!(
                        index = %name,
                        sqlstate = ?sqlstate,
                        error = %e,
                        "amcheck: bt_index_check reported a problem"
                    );
                    report.corrupt.push(CorruptIndex {
                        index_name: name,
                        sqlstate,
                        message: e.to_string(),
                    });
                }
            }
        }

        Ok(report)
    }

    /// Fire the `db_index_corruption` alert: INSERT an `alert_event` with the
    /// dispatch result and dispatch through the policy's channel directly.
    async fn fire_corruption_alert(&self, report: &IntegrityReport) {
        // Resolve the seeded policy (id, severity, channel).
        let policy: Option<(uuid::Uuid, String, String)> = match sqlx::query_as(
            "SELECT id, severity, channel FROM alert_policies WHERE name = $1 AND enabled = true",
        )
        .bind(POLICY_NAME)
        .fetch_optional(&self.pg)
        .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, "amcheck: failed to load db_index_corruption policy");
                None
            }
        };

        let Some((policy_id, severity, channel)) = policy else {
            tracing::error!(
                "amcheck: {} corrupt index(es) detected but alert policy '{}' is missing/disabled — NOT alerting",
                report.corrupt.len(),
                POLICY_NAME
            );
            return;
        };

        let names: Vec<String> = report
            .corrupt
            .iter()
            .map(|c| match &c.sqlstate {
                Some(code) => format!("{} ({})", c.index_name, code),
                None => c.index_name.clone(),
            })
            .collect();

        let message = format!(
            "DB integrity: {} of {} btree unique index(es) failed amcheck on leader '{}'. \
             Likely glibc/ICU collation drift — REINDEX the affected indexes after verifying. \
             Affected: {}",
            report.corrupt.len(),
            report.checked,
            self.my_name,
            names.join(", ")
        );

        // Dispatch FIRST so the recorded channel_result reflects reality
        // (never 'pending').
        let channel_result =
            crate::alert_evaluator::dispatch_alert(&self.pg, &channel, &severity, &message).await;

        if let Err(e) = sqlx::query(
            r#"
            INSERT INTO alert_events
                (policy_id, computer_id, value, value_text, message, channel_result)
            VALUES ($1, NULL, $2, NULL, $3, $4)
            "#,
        )
        .bind(policy_id)
        .bind(report.corrupt.len() as f64)
        .bind(&message)
        .bind(&channel_result)
        .execute(&self.pg)
        .await
        {
            tracing::error!(error = %e, "amcheck: failed to record alert_event");
        }

        tracing::error!(
            corrupt = report.corrupt.len(),
            checked = report.checked,
            channel = %channel,
            channel_result = %channel_result,
            "amcheck: DB index corruption alert fired"
        );
    }

    /// Spawn the 6h check loop. Leadership is gated inside the loop on every
    /// fire (NOT at spawn), so this is safe to start on every daemon.
    pub fn spawn(self, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(CHECK_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        if !self.is_live_leader().await {
                            continue;
                        }
                        match self.run_check_once().await {
                            Ok(report) => {
                                if report.corrupt.is_empty() {
                                    tracing::info!(
                                        checked = report.checked,
                                        ok = report.ok,
                                        "amcheck: all btree unique indexes OK"
                                    );
                                } else {
                                    self.fire_corruption_alert(&report).await;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "amcheck: integrity check pass failed");
                            }
                        }
                    }
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            tracing::info!("amcheck integrity guard shutting down");
                            break;
                        }
                    }
                }
            }
        })
    }
}
