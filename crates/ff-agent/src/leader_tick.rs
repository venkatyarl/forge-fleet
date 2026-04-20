//! Leader-election tick.
//!
//! Every `ff daemon` runs this tick every 15s. The first node to
//! successfully claim the `fleet_leader_state` singleton wins; subsequent
//! ticks refresh the heartbeat or yield/takeover as needed.
//!
//! ## Data sources
//! - **Pulse** (Redis) — `computer_health_for_election()` reports which
//!   peers currently have a live beat and whether they are `going_offline`.
//!   A missing beat ⇒ that computer does not appear in the health vector
//!   (TTL has already removed it), i.e. it is treated as not alive.
//! - **Postgres** — `fleet_members ⋈ computers` provides the candidate
//!   pool and each candidate's `election_priority` (lower = preferred).
//!   `fleet_leader_state` is the durable singleton used for the race.
//!
//! ## Election rule (inlined)
//! Among candidates that are **both** registered in `fleet_members` and
//! alive in Pulse (not `going_offline`), the node with the **lowest**
//! `election_priority` wins. Ties broken by alphabetical `member_name`
//! for determinism.
//!
//! Note: `ff_core::leader::elect_leader` operates over a `FleetConfig`
//! struct; here we work straight from Postgres rows, so we inline an
//! equivalent lowest-priority-alive rule rather than round-tripping
//! through `FleetConfig`. See the task notes in this module's PR for
//! the rationale.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::Utc;
use sqlx::{PgPool, Row};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use uuid::Uuid;

use ff_db::leader_state::{
    LeaderState, pg_claim_leader_initial, pg_claim_leader_takeover, pg_get_current_leader,
    pg_refresh_leader_heartbeat, pg_yield_leader,
};
use ff_pulse::reader::{PulseError, PulseReader};

use crate::ha::pg_failover::{FailoverOutcome, PostgresFailoverManager};

/// Max revive attempts per computer per [`REVIVE_BACKOFF_WINDOW_MIN`] minutes.
/// Above this the leader skips (and should escalate via alert channels).
const REVIVE_MAX_ATTEMPTS_PER_WINDOW: i64 = 3;
/// Rolling window for revive-attempt backoff accounting.
const REVIVE_BACKOFF_WINDOW_MIN: i64 = 30;
/// Maximum age of `last_seen_at` that still qualifies a computer as a revive
/// candidate (i.e. "was alive recently"). Matches the task spec.
const REVIVE_RECENT_SEEN_MIN: i64 = 10;

/// If the durable leader row's `heartbeat_at` is older than this, a live
/// peer is allowed to challenge and take over.
const STALE_THRESHOLD_SECS: i64 = 45;

/// Errors returned by [`LeaderTick::tick`].
#[derive(Debug, thiserror::Error)]
pub enum LeaderError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("pulse: {0}")]
    Pulse(#[from] PulseError),
}

/// Outcome of a single [`LeaderTick::tick`] pass — useful for tests and
/// operational logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickOutcome {
    /// No action taken — I'm a member and the current leader is fresh.
    NoOp,
    /// I just claimed the empty singleton row.
    BecameLeader,
    /// I refreshed my own `heartbeat_at`.
    StillLeader,
    /// I relinquished leadership (preferred peer is back online).
    /// Carries the name of the expected new leader (or "" if unknown).
    Yielded(String),
    /// I took leadership from a stale/dead old leader.
    /// Carries the name of the previous leader that was displaced.
    TookOver(String),
}

/// Callback fired once, on the tick that transitions us to leader.
pub type OnBecameLeader = Arc<dyn Fn() + Send + Sync>;
/// Callback fired once, on the tick that we stop being leader.
/// Argument: name of the new/expected leader (may be empty if unknown).
pub type OnLostLeader = Arc<dyn Fn(String) + Send + Sync>;

/// Periodic leader-election state machine for a single daemon.
///
/// Construct with [`LeaderTick::new`], attach hooks with
/// [`with_on_became_leader`](Self::with_on_became_leader) /
/// [`with_on_lost_leader`](Self::with_on_lost_leader), then start with
/// [`spawn`](Self::spawn).
pub struct LeaderTick {
    pg: PgPool,
    pulse: PulseReader,
    my_computer_id: Uuid,
    my_name: String,
    #[allow(dead_code)] // priority is read from DB, but retained for diagnostics
    my_priority: i32,
    epoch: AtomicU64,

    on_became_leader: OnBecameLeader,
    on_lost_leader: OnLostLeader,

    /// Optional Postgres auto-failover manager. When set, each tick that
    /// leaves us as the current leader will also call
    /// [`PostgresFailoverManager::check_and_failover`] to detect a dead
    /// primary and promote the local replica if we host one.
    pg_failover_manager: Option<Arc<PostgresFailoverManager>>,
}

impl LeaderTick {
    /// Build a new tick with no-op hooks.
    pub fn new(
        pg: PgPool,
        pulse: PulseReader,
        my_computer_id: Uuid,
        my_name: String,
        my_priority: i32,
    ) -> Self {
        Self {
            pg,
            pulse,
            my_computer_id,
            my_name,
            my_priority,
            epoch: AtomicU64::new(0),
            on_became_leader: Arc::new(|| {}),
            on_lost_leader: Arc::new(|_| {}),
            pg_failover_manager: None,
        }
    }

    /// Attach a callback fired when we become leader.
    pub fn with_on_became_leader(mut self, cb: OnBecameLeader) -> Self {
        self.on_became_leader = cb;
        self
    }

    /// Attach a callback fired when we lose leadership.
    pub fn with_on_lost_leader(mut self, cb: OnLostLeader) -> Self {
        self.on_lost_leader = cb;
        self
    }

    /// Attach a [`PostgresFailoverManager`]. Without this, the leader
    /// never attempts to promote a local Postgres replica, even if the
    /// primary goes ODOWN.
    pub fn with_pg_failover(mut self, manager: Arc<PostgresFailoverManager>) -> Self {
        self.pg_failover_manager = Some(manager);
        self
    }

    /// Spawn the periodic loop. Runs `tick()` every `interval_secs` until
    /// `shutdown` flips to `true`.
    pub fn spawn(
        self,
        interval_secs: u64,
        mut shutdown: watch::Receiver<bool>,
    ) -> JoinHandle<()> {
        let period = Duration::from_secs(interval_secs.max(1));
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(period);
            // Prevent a burst when several ticks are missed.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        match self.tick().await {
                            Ok(outcome) => {
                                tracing::debug!(
                                    node = %self.my_name,
                                    ?outcome,
                                    "leader tick"
                                );
                                // Only scan for revivable members when we
                                // are currently the leader (just elected or
                                // continuing).
                                if matches!(
                                    outcome,
                                    TickOutcome::StillLeader
                                        | TickOutcome::BecameLeader
                                        | TickOutcome::TookOver(_)
                                ) {
                                    if let Err(err) = self.revive_scan().await {
                                        tracing::warn!(
                                            node = %self.my_name,
                                            error = %err,
                                            "revive_scan failed"
                                        );
                                    }

                                    // Phase 6 HA: auto-failover check. Only
                                    // runs on the currently-elected ForgeFleet
                                    // leader. Disabled via env var
                                    // FORGEFLEET_DISABLE_AUTO_PG_FAILOVER.
                                    if let Some(manager) = &self.pg_failover_manager {
                                        match manager.check_and_failover(&self.pulse).await {
                                            Ok(FailoverOutcome::Promoted) => {
                                                tracing::info!(
                                                    node = %self.my_name,
                                                    "pg_failover: promoted local replica to primary"
                                                );
                                            }
                                            Ok(FailoverOutcome::PrimaryOdownPromotingMyReplica) => {
                                                tracing::info!(
                                                    node = %self.my_name,
                                                    "pg_failover: promoting local replica"
                                                );
                                            }
                                            Ok(FailoverOutcome::PrimaryOdownCantPromote) => {
                                                tracing::warn!(
                                                    node = %self.my_name,
                                                    "pg_failover: primary odown but no local replica to promote"
                                                );
                                            }
                                            Ok(FailoverOutcome::Blocked(why)) => {
                                                tracing::warn!(
                                                    node = %self.my_name,
                                                    reason = %why,
                                                    "pg_failover: blocked"
                                                );
                                            }
                                            Ok(FailoverOutcome::NoOp) => {
                                                tracing::debug!(
                                                    node = %self.my_name,
                                                    "pg_failover: no-op"
                                                );
                                            }
                                            Err(e) => {
                                                tracing::error!(
                                                    node = %self.my_name,
                                                    error = %e,
                                                    "pg_failover: check_and_failover failed"
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                            Err(err) => {
                                tracing::warn!(
                                    node = %self.my_name,
                                    error = %err,
                                    "leader tick failed"
                                );
                            }
                        }
                    }
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            tracing::info!(node = %self.my_name, "leader tick shutting down");
                            break;
                        }
                    }
                }
            }
        })
    }

    /// Run one election pass. Public so tests can drive the state
    /// machine deterministically.
    pub async fn tick(&self) -> Result<TickOutcome, LeaderError> {
        // 1) Live health from Pulse. Absent peers are implicitly "not alive".
        let health = self.pulse.computer_health_for_election().await?;
        let alive: std::collections::HashMap<String, bool> = health
            .into_iter()
            .map(|(name, healthy, _going_offline)| (name, healthy))
            .collect();

        // 2) Registered candidates from Postgres. `election_priority`
        //    lower number = more preferred.
        let candidates = load_candidates(&self.pg).await?;

        // 3) Pick the best alive candidate (lowest priority, alphabetical
        //    tie-break). If no candidate is alive, `best_alive` is None
        //    and we refuse to claim.
        let best_alive: Option<&Candidate> = candidates
            .iter()
            .filter(|c| alive.get(&c.member_name).copied().unwrap_or(false))
            .min_by(|a, b| {
                a.election_priority
                    .cmp(&b.election_priority)
                    .then_with(|| a.member_name.cmp(&b.member_name))
            });

        let current = pg_get_current_leader(&self.pg).await?;

        match (current, best_alive) {
            // No durable leader yet → try to claim.
            (None, Some(best)) if best.member_name == self.my_name => {
                let new_epoch = self.next_epoch(None);
                let claimed = pg_claim_leader_initial(
                    &self.pg,
                    self.my_computer_id,
                    &self.my_name,
                    new_epoch,
                    "initial",
                )
                .await?;
                if claimed {
                    (self.on_became_leader)();
                    Ok(TickOutcome::BecameLeader)
                } else {
                    // Someone else won the race — we'll see them next tick.
                    Ok(TickOutcome::NoOp)
                }
            }

            // No durable leader, but we're not the best candidate.
            (None, _) => Ok(TickOutcome::NoOp),

            // Leader row exists and it's us.
            (Some(cur), Some(best)) if cur.member_name == self.my_name => {
                if best.member_name != self.my_name {
                    // A more-preferred peer is alive → yield.
                    let yielded = pg_yield_leader(&self.pg, &self.my_name).await?;
                    if yielded {
                        (self.on_lost_leader)(best.member_name.clone());
                        Ok(TickOutcome::Yielded(best.member_name.clone()))
                    } else {
                        Ok(TickOutcome::NoOp)
                    }
                } else {
                    // We remain best → just refresh our heartbeat.
                    let refreshed =
                        pg_refresh_leader_heartbeat(&self.pg, &self.my_name).await?;
                    if refreshed {
                        // Keep our in-memory epoch aligned with the row.
                        self.observe_epoch(cur.epoch);
                        Ok(TickOutcome::StillLeader)
                    } else {
                        // Row was deleted/taken under us. Treat as lost.
                        (self.on_lost_leader)(String::new());
                        Ok(TickOutcome::Yielded(String::new()))
                    }
                }
            }

            // Leader row exists and it's us, but no one is alive — still
            // refresh (we're the only node left).
            (Some(cur), None) if cur.member_name == self.my_name => {
                let refreshed = pg_refresh_leader_heartbeat(&self.pg, &self.my_name).await?;
                if refreshed {
                    self.observe_epoch(cur.epoch);
                    Ok(TickOutcome::StillLeader)
                } else {
                    (self.on_lost_leader)(String::new());
                    Ok(TickOutcome::Yielded(String::new()))
                }
            }

            // Someone else is leader.
            (Some(cur), Some(best)) => {
                let stale = leader_is_stale(&cur);
                if stale && best.member_name == self.my_name {
                    let new_epoch = self.next_epoch(Some(cur.epoch));
                    let displaced_name = cur.member_name.clone();
                    let took = pg_claim_leader_takeover(
                        &self.pg,
                        self.my_computer_id,
                        &self.my_name,
                        new_epoch,
                        &displaced_name,
                        STALE_THRESHOLD_SECS,
                    )
                    .await?;
                    if took {
                        (self.on_became_leader)();
                        Ok(TickOutcome::TookOver(displaced_name))
                    } else {
                        // Another peer raced us; just wait for the next tick.
                        Ok(TickOutcome::NoOp)
                    }
                } else {
                    Ok(TickOutcome::NoOp)
                }
            }

            // Someone else is leader and no peer is alive in Pulse —
            // without evidence that we are the right taker, do nothing.
            (Some(_), None) => Ok(TickOutcome::NoOp),
        }
    }

    /// Scan for computers stuck in an objectively-down state that were alive
    /// recently, and enqueue a `revive_member` deferred task per eligible
    /// target. Called only when we are the current leader.
    pub async fn revive_scan(&self) -> Result<(), LeaderError> {
        // 1. Find all currently-offline computers that were seen in the last
        //    REVIVE_RECENT_SEEN_MIN minutes.
        let rows = sqlx::query(
            "SELECT id, name
               FROM computers
              WHERE status IN ('odown', 'offline', 'sdown')
                AND last_seen_at IS NOT NULL
                AND last_seen_at > NOW() - ($1 || ' minutes')::INTERVAL",
        )
        .bind(REVIVE_RECENT_SEEN_MIN.to_string())
        .fetch_all(&self.pg)
        .await?;

        for row in rows {
            let computer_id: Uuid = row.get("id");
            let name: String = row.get("name");

            // Never attempt to revive ourselves.
            if name == self.my_name {
                continue;
            }

            // 2. Backoff guard — skip if we've already tried too often lately.
            let recent_attempts = match crate::revive::ReviveManager::recent_attempt_count(
                &self.pg,
                computer_id,
                REVIVE_BACKOFF_WINDOW_MIN,
            )
            .await
            {
                Ok(n) => n,
                Err(err) => {
                    tracing::warn!(
                        node = %name,
                        error = %err,
                        "revive backoff lookup failed"
                    );
                    continue;
                }
            };

            if recent_attempts >= REVIVE_MAX_ATTEMPTS_PER_WINDOW {
                tracing::warn!(
                    node = %name,
                    recent_attempts,
                    window_min = REVIVE_BACKOFF_WINDOW_MIN,
                    "revive backoff reached — skipping (escalation-worthy)"
                );
                continue;
            }

            // 3. De-dupe: is a revive_member task already in-flight?
            let inflight = sqlx::query(
                "SELECT 1 FROM deferred_tasks
                   WHERE kind = 'shell'
                     AND status IN ('pending', 'dispatchable', 'running')
                     AND title = $1",
            )
            .bind(format!("revive_member: {name}"))
            .fetch_optional(&self.pg)
            .await?;
            if inflight.is_some() {
                tracing::debug!(node = %name, "revive task already in-flight; skipping");
                continue;
            }

            // 4. Enqueue. Use trigger_type='now' so the scheduler promotes
            //    immediately. The revive call runs on the leader itself
            //    (preferred_node = self) because the target is offline.
            let title = format!("revive_member: {name}");
            let script = format!(
                "ff fleet revive {name} --internal"
            );
            let payload = serde_json::json!({ "command": script });
            let trigger_spec = serde_json::json!({});
            let required_caps = serde_json::json!([]);

            match ff_db::queries::pg_enqueue_deferred(
                &self.pg,
                &title,
                "shell",
                &payload,
                "now",
                &trigger_spec,
                Some(&self.my_name),
                &required_caps,
                Some(&format!("leader:{}", self.my_name)),
                Some(2),
            )
            .await
            {
                Ok(id) => tracing::info!(
                    node = %name,
                    task_id = %id,
                    "enqueued revive_member deferred task"
                ),
                Err(err) => tracing::warn!(
                    node = %name,
                    error = %err,
                    "failed to enqueue revive_member task"
                ),
            }
        }

        Ok(())
    }

    /// Compute the next epoch to propose: `max(seen_epoch + 1, local + 1, 1)`.
    fn next_epoch(&self, seen_current: Option<i64>) -> i64 {
        let local = self.epoch.load(Ordering::Relaxed) as i64;
        let from_row = seen_current.unwrap_or(0);
        let next = local.max(from_row).saturating_add(1).max(1);
        self.epoch.store(next as u64, Ordering::Relaxed);
        next
    }

    /// Record an epoch we observed from the DB so future bumps are monotonic.
    fn observe_epoch(&self, seen: i64) {
        let seen_u = seen.max(0) as u64;
        // CAS-free but monotonic: only raise the stored value.
        let mut current = self.epoch.load(Ordering::Relaxed);
        while seen_u > current {
            match self.epoch.compare_exchange_weak(
                current,
                seen_u,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────

/// One row from the candidate-pool query.
#[derive(Debug, Clone)]
struct Candidate {
    member_name: String,
    #[allow(dead_code)]
    computer_id: Uuid,
    election_priority: i32,
}

/// Load the full candidate pool from Postgres. Any `fleet_members` row
/// whose `computers.name` is present is a candidate — Pulse decides
/// which of them is currently alive.
async fn load_candidates(pool: &PgPool) -> Result<Vec<Candidate>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT c.id          AS computer_id,
                c.name        AS member_name,
                fm.election_priority AS election_priority
         FROM fleet_members fm
         JOIN computers c ON c.id = fm.computer_id",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| Candidate {
            computer_id: r.get("computer_id"),
            member_name: r.get("member_name"),
            election_priority: r.get("election_priority"),
        })
        .collect())
}

/// Leader considered stale iff its `heartbeat_at` is older than
/// [`STALE_THRESHOLD_SECS`] seconds relative to wall-clock `now()`.
fn leader_is_stale(cur: &LeaderState) -> bool {
    let now = Utc::now();
    let age = now.signed_duration_since(cur.heartbeat_at);
    age.num_seconds() > STALE_THRESHOLD_SECS
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    fn fake_leader(name: &str, heartbeat_age_secs: i64, epoch: i64) -> LeaderState {
        LeaderState {
            computer_id: Uuid::nil(),
            member_name: name.to_string(),
            epoch,
            elected_at: Utc::now() - ChronoDuration::seconds(heartbeat_age_secs),
            reason: None,
            heartbeat_at: Utc::now() - ChronoDuration::seconds(heartbeat_age_secs),
        }
    }

    #[test]
    fn stale_detection_matches_threshold() {
        let fresh = fake_leader("taylor", 10, 1);
        let stale = fake_leader("taylor", STALE_THRESHOLD_SECS + 1, 1);
        assert!(!leader_is_stale(&fresh));
        assert!(leader_is_stale(&stale));
    }
}
