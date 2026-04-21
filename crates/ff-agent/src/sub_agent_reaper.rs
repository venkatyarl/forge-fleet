//! Stuck-slot reaper for `sub_agents`.
//!
//! ## Role
//!
//! Runs on the leader every 10 minutes. Resets any `sub_agents` row whose
//! `status` is `'error'` or `'busy'` AND whose `started_at` is either NULL
//! or older than 10 minutes. The dispatch queue depends on slots cycling
//! back to `'idle'` — when a worker crashes mid-task or flips to `'error'`
//! without a later cleanup, the slot is effectively dead. A NULL
//! `started_at` on an `'error'`/`'busy'` row means the slot was never
//! meaningfully running, so it should be reset too. This tick automates
//! the manual `UPDATE sub_agents SET status='idle' ...` the operator used
//! to run by hand.
//!
//! Schema note: the V23 `sub_agents` table has no `claimed_at` or
//! `last_error` columns. We use `started_at` as the staleness clock and
//! surface the reap reason via `tracing::info!` + the audit trail the
//! caller controls, not a per-row text column.

use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::{PgPool, Row};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Is this pool's leader the computer whose name matches `my_name`?
async fn is_leader(pool: &PgPool, my_name: &str) -> bool {
    match sqlx::query_scalar::<_, String>(
        "SELECT member_name FROM fleet_leader_state LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    {
        Ok(Some(leader)) => leader.eq_ignore_ascii_case(my_name),
        _ => false,
    }
}

/// Background stuck-slot reaper.
pub struct SubAgentReaper {
    pool: PgPool,
    my_name: String,
}

impl SubAgentReaper {
    pub fn new(pool: PgPool, my_name: String) -> Self {
        Self { pool, my_name }
    }

    /// One tick: gate on leader, reset stuck rows, log each. Returns count.
    pub async fn run_once(&self) -> Result<usize> {
        if !is_leader(&self.pool, &self.my_name).await {
            return Ok(0);
        }

        let rows = sqlx::query(
            "UPDATE sub_agents AS s
                SET status               = 'idle',
                    current_work_item_id = NULL
               FROM computers c
              WHERE s.computer_id = c.id
                AND s.status IN ('error','busy')
                AND (s.started_at IS NULL OR s.started_at < NOW() - INTERVAL '10 minutes')
              RETURNING c.name AS computer_name, s.slot AS slot, s.status AS status",
        )
        .fetch_all(&self.pool)
        .await
        .context("reap stuck sub_agents")?;

        for row in &rows {
            let computer: String = row.get("computer_name");
            let slot: i32 = row.get("slot");
            let prior: String = row.get("status");
            tracing::info!(
                computer = %computer,
                slot = slot,
                prior_status = %prior,
                "reaped stuck sub_agent slot after 10min timeout"
            );
        }
        Ok(rows.len())
    }

    /// Spawn the 10-minute tick. First tick fires ~120s after spawn so the
    /// daemon's other subsystems come up first.
    pub fn spawn(self, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let kickoff = Duration::from_secs(120);
            let interval = Duration::from_secs(600);

            tokio::select! {
                _ = tokio::time::sleep(kickoff) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return; }
                }
            }

            loop {
                match self.run_once().await {
                    Ok(n) if n > 0 => tracing::info!(reaped = n, "sub-agent reaper tick"),
                    Ok(_) => tracing::debug!("sub-agent reaper tick: nothing to do"),
                    Err(e) => tracing::warn!(error = %e, "sub-agent reaper tick failed"),
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
}
