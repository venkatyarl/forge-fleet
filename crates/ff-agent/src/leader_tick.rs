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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use chrono::Utc;
use sqlx::{PgPool, Row};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use uuid::Uuid;

use ff_db::leader_state::{
    LeaderState, pg_claim_leader_initial, pg_claim_leader_pulse_silent, pg_claim_leader_takeover,
    pg_get_current_leader, pg_refresh_leader_heartbeat, pg_yield_leader,
};
use ff_pulse::reader::{PulseError, PulseReader};

use crate::ha::pg_failover::{FailoverOutcome, PostgresFailoverManager};

/// Max revive attempts per computer per [`REVIVE_BACKOFF_WINDOW_MIN`] minutes.
/// Above this the leader skips (and should escalate via alert channels).
const REVIVE_MAX_ATTEMPTS_PER_WINDOW: i64 = 3;
/// Rolling window for revive-attempt backoff accounting.
const REVIVE_BACKOFF_WINDOW_MIN: i64 = 30;

/// If the durable leader row's `heartbeat_at` is older than this, a live
/// peer is allowed to challenge and take over.
const STALE_THRESHOLD_SECS: i64 = 45;

/// Minimum duration a leader must be ODOWN on the Pulse channel before a
/// peer is allowed to challenge via [`pg_claim_leader_pulse_silent`] —
/// even when Postgres heartbeat is fresh. Two back-to-back tick-pass
/// observations (15 s + 15 s) cheaply filter transient Redis partitions.
const MIN_PULSE_SILENT_SECS: u64 = 30;

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
/// Argument: name of the previous leader we displaced (`None` on cold
/// claim of the empty singleton). Used by OpenClaw gateway promotion
/// to rsync paired-device state from the outgoing gateway.
pub type OnBecameLeader = Arc<dyn Fn(Option<String>) + Send + Sync>;
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

    /// First wall-clock instant at which we observed the current leader
    /// as ODOWN on the Pulse channel despite a fresh Postgres heartbeat.
    /// Reset to `None` whenever the leader becomes pulse-alive again OR
    /// the leader name changes. Used to gate [`pg_claim_leader_pulse_silent`]
    /// via [`MIN_PULSE_SILENT_SECS`] — closes #91.
    leader_pulse_silent_since: tokio::sync::Mutex<Option<(String, std::time::Instant)>>,

    /// HA Phase 1 voluntary step-down. Shared with the HeartbeatV2 publisher
    /// (via [`with_yield_flag`](Self::with_yield_flag)); each tick we read the
    /// `leader_yield_request` fleet_secret and, when it names us and hasn't
    /// expired, set this flag so our beat publishes `is_yielding=true`. That
    /// makes every peer's election skip us so the next-preferred follower takes
    /// over cleanly. `None` when no publisher handle was attached (the node
    /// still functions; it just can't be told to step down).
    yield_flag: Option<Arc<AtomicBool>>,
}

/// Parse a `leader_yield_request` fleet_secret value of the form
/// `<member_name>|<rfc3339_until>`. Returns `(member, until)` on success.
/// Anything malformed yields `None` → treated as "no active request" so a
/// garbled secret can never wedge the election.
fn parse_yield_request(raw: &str) -> Option<(String, chrono::DateTime<Utc>)> {
    let (member, until) = raw.split_once('|')?;
    let member = member.trim();
    if member.is_empty() {
        return None;
    }
    let until = chrono::DateTime::parse_from_rfc3339(until.trim())
        .ok()?
        .with_timezone(&Utc);
    Some((member.to_string(), until))
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
            on_became_leader: Arc::new(|_| {}),
            on_lost_leader: Arc::new(|_| {}),
            pg_failover_manager: None,
            leader_pulse_silent_since: tokio::sync::Mutex::new(None),
            yield_flag: None,
        }
    }

    /// Attach the HeartbeatV2 publisher's `is_yielding` flag so this tick can
    /// drive voluntary step-down (HA Phase 1). Without it, `leader_yield_request`
    /// is still honoured for *this node's own* election decision, but the flag
    /// is never published to peers.
    pub fn with_yield_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.yield_flag = Some(flag);
        self
    }

    /// Read the `leader_yield_request` fleet_secret and decide whether THIS node
    /// should currently yield leadership. True only when the request names us
    /// and its deadline hasn't passed (auto fail-back on expiry). A missing /
    /// malformed / unreadable secret is "not yielding".
    async fn self_should_yield(&self) -> bool {
        match ff_db::pg_get_secret(&self.pg, "leader_yield_request").await {
            Ok(Some(raw)) => parse_yield_request(&raw)
                .map(|(member, until)| member == self.my_name && Utc::now() < until)
                .unwrap_or(false),
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(error = %e, "leader_yield_request read failed; not yielding");
                false
            }
        }
    }

    /// Track pulse-silence of the current leader across consecutive ticks.
    /// Returns `Some(duration)` once the leader has been continuously
    /// pulse-silent; `None` otherwise. Resets whenever the leader name
    /// changes (different leader → different silence window).
    async fn observe_leader_pulse_silence(
        &self,
        leader_name: &str,
        leader_alive_in_pulse: bool,
    ) -> Option<std::time::Duration> {
        let mut guard = self.leader_pulse_silent_since.lock().await;
        if leader_alive_in_pulse {
            *guard = None;
            return None;
        }
        match guard.as_ref() {
            Some((name, since)) if name == leader_name => Some(since.elapsed()),
            _ => {
                let now = std::time::Instant::now();
                *guard = Some((leader_name.to_string(), now));
                Some(std::time::Duration::ZERO)
            }
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
    pub fn spawn(self, interval_secs: u64, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
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

                                    // V43+: self-heal bug-fix pipeline.
                                    if let Err(err) = self.self_heal_scan().await {
                                        tracing::warn!(
                                            node = %self.my_name,
                                            error = %err,
                                            "self_heal_scan failed"
                                        );
                                    }

                                    // V121+: feed interaction-log errors into the
                                    // self-heal queue. ~30 min cadence (marker-file
                                    // gated) so a recurring runtime error becomes a
                                    // dispatched fix instead of dying in the log.
                                    if let Err(err) = self.scan_interaction_errors().await {
                                        tracing::warn!(
                                            node = %self.my_name,
                                            error = %err,
                                            "scan_interaction_errors failed"
                                        );
                                    }

                                    // Keep the open-design SKILL.md catalog in
                                    // step with the auto-upgrade pipeline's git
                                    // pulls. No-op unless the leader's checkout
                                    // SHA advanced since the last sync.
                                    if let Err(err) = self.sync_open_design_skills().await {
                                        tracing::warn!(
                                            node = %self.my_name,
                                            error = %err,
                                            "sync_open_design_skills failed"
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
        //    The third tuple element is each node's `is_yielding` flag (HA
        //    Phase 1 voluntary step-down): a yielding node is alive but must
        //    not be elected, so it is excluded from the candidate pool exactly
        //    like an unhealthy one.
        let health = self.pulse.computer_health_for_election().await?;
        let mut alive: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
        let mut yielding: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (name, healthy, is_yielding) in health {
            if is_yielding {
                yielding.insert(name.clone());
            }
            alive.insert(name, healthy);
        }

        // 1b) HA Phase 1: drive our own step-down from the `leader_yield_request`
        //     fleet_secret. Publish the flag for peers (via the shared handle)
        //     AND fold ourselves into the local `yielding` set immediately so we
        //     act on our own request this very tick (no beat round-trip lag).
        let self_yield = self.self_should_yield().await;
        if let Some(flag) = &self.yield_flag {
            flag.store(self_yield, Ordering::Relaxed);
        }
        if self_yield {
            yielding.insert(self.my_name.clone());
        }

        // 2) Registered candidates from Postgres. `election_priority`
        //    lower number = more preferred.
        let candidates = load_candidates(&self.pg).await?;

        // 3) Pick the best alive candidate (lowest priority, alphabetical
        //    tie-break), skipping any node that is voluntarily yielding. If no
        //    candidate is alive (or all are yielding), `best_alive` is None and
        //    we refuse to claim — a yield with no eligible successor leaves the
        //    current leader in place rather than going leaderless.
        let best_alive = pick_best_candidate(&candidates, &alive, &yielding);

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
                    (self.on_became_leader)(None);
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
                    let refreshed = pg_refresh_leader_heartbeat(&self.pg, &self.my_name).await?;
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
                let leader_alive_in_pulse = alive.get(&cur.member_name).copied().unwrap_or(false);
                let pulse_silence = self
                    .observe_leader_pulse_silence(&cur.member_name, leader_alive_in_pulse)
                    .await;
                let pulse_silent_long_enough = pulse_silence
                    .map(|d| d.as_secs() >= MIN_PULSE_SILENT_SECS)
                    .unwrap_or(false);
                let i_am_best = best.member_name == self.my_name;

                if stale && i_am_best {
                    // Classic takeover: Postgres heartbeat is stale.
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
                        (self.on_became_leader)(Some(displaced_name.clone()));
                        Ok(TickOutcome::TookOver(displaced_name))
                    } else {
                        Ok(TickOutcome::NoOp)
                    }
                } else if pulse_silent_long_enough && i_am_best && !stale {
                    // Pulse-silent challenge path (#91): Postgres heartbeat
                    // is fresh (so classic takeover is blocked) BUT the
                    // leader has been ODOWN on Pulse for ≥ MIN_PULSE_SILENT_SECS.
                    // The leader's Pulse publisher is hung / Redis partition /
                    // Redis daemon down — peers can't see it so it's
                    // effectively dead for routing + dispatch purposes.
                    let new_epoch = self.next_epoch(Some(cur.epoch));
                    let displaced_name = cur.member_name.clone();
                    tracing::warn!(
                        node = %self.my_name,
                        displaced = %displaced_name,
                        silence_secs = pulse_silence.map(|d| d.as_secs()).unwrap_or(0),
                        "leader pulse-silent but postgres fresh; issuing challenge"
                    );
                    let took = pg_claim_leader_pulse_silent(
                        &self.pg,
                        self.my_computer_id,
                        &self.my_name,
                        new_epoch,
                        &displaced_name,
                    )
                    .await?;
                    if took {
                        // Reset silence tracker: there's a new leader now.
                        let mut g = self.leader_pulse_silent_since.lock().await;
                        *g = None;
                        (self.on_became_leader)(Some(displaced_name.clone()));
                        Ok(TickOutcome::TookOver(displaced_name))
                    } else {
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
        // 1. Find every currently-offline computer. We intentionally do NOT
        //    gate on a `last_seen_at` freshness window — the earlier 10-min
        //    gate silently abandoned any member that stayed down long enough
        //    for its own beat to expire. Instead we rely on the
        //    [`REVIVE_MAX_ATTEMPTS_PER_WINDOW`] / [`REVIVE_BACKOFF_WINDOW_MIN`]
        //    backoff to prevent spam on truly-dead machines. Members that
        //    blow past the backoff get logged as "escalation-worthy" — wire
        //    an alert policy on that condition to get operator-facing
        //    notifications.
        let rows = sqlx::query(
            "SELECT id, name
               FROM computers
              WHERE status IN ('odown', 'offline', 'sdown')",
        )
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
            let script = format!("ff fleet revive {name} --internal");
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

    /// V43+: self-heal coordination scan.
    ///
    /// 1. Aggregate recent `fleet_bug_reports` by signature and upsert into
    ///    `fleet_self_heal_queue` (single-flight via UNIQUE on bug_signature).
    /// 2. Dispatch `self_heal_writer` deferred tasks for rows in `detected`.
    /// 3. Recover stale claims (`fixing` older than 5 min) — retry or escalate.
    pub async fn self_heal_scan(&self) -> Result<(), LeaderError> {
        // ── 1. Aggregate bug reports into the queue ──────────────────────────
        sqlx::query(
            "INSERT INTO fleet_self_heal_queue \
                (bug_signature, tier, status, report_count, created_at) \
             SELECT bug_signature, MAX(tier), 'detected', COUNT(*), NOW() \
             FROM fleet_bug_reports \
             WHERE reported_at > NOW() - INTERVAL '5 minutes' \
             GROUP BY bug_signature \
             ON CONFLICT (bug_signature) DO UPDATE SET \
                 report_count = fleet_self_heal_queue.report_count \
                     + EXCLUDED.report_count,
                 tier = EXCLUDED.tier",
        )
        .execute(&self.pg)
        .await?;

        // ── 2. Stale-claim recovery ─────────────────────────────────────────
        let stale_rows = sqlx::query(
            "UPDATE fleet_self_heal_queue \
             SET status = CASE \
                 WHEN attempts >= 2 THEN 'escalated' \
                 ELSE 'detected' \
             END, \
                 attempts = attempts + 1, \
                 escalated_to_operator_at = CASE \
                     WHEN attempts >= 2 THEN NOW() \
                     ELSE escalated_to_operator_at \
                 END \
             WHERE status = 'fixing' \
               AND (last_attempt_at IS NULL OR last_attempt_at < NOW() - INTERVAL '5 minutes') \
             RETURNING bug_signature, status",
        )
        .fetch_all(&self.pg)
        .await?;

        for row in &stale_rows {
            let sig: String = row.try_get("bug_signature")?;
            let status: String = row.try_get("status")?;
            if status == "escalated" {
                tracing::warn!(
                    bug_signature = %sig,
                    "self_heal: bug escalated to operator after max retries"
                );
            } else {
                tracing::info!(
                    bug_signature = %sig,
                    "self_heal: stale claim recovered, re-dispatching"
                );
            }
        }

        // ── 3. Dispatch writer tasks for detected rows ──────────────────────
        let detected = sqlx::query(
            "SELECT bug_signature, tier \
             FROM fleet_self_heal_queue \
             WHERE status = 'detected' \
             ORDER BY CASE tier \
                 WHEN 'T1' THEN 1 WHEN 'T0' THEN 2 WHEN 'T2' THEN 3 ELSE 4 \
             END, created_at",
        )
        .fetch_all(&self.pg)
        .await?;

        for row in &detected {
            let sig: String = row.try_get("bug_signature")?;

            // De-dupe: is a writer task already in-flight?
            let inflight = sqlx::query(
                "SELECT 1 FROM deferred_tasks \
                 WHERE kind = 'shell_command' \
                   AND status IN ('pending', 'dispatchable', 'running') \
                   AND title = $1",
            )
            .bind(format!("self_heal_writer: {sig}"))
            .fetch_optional(&self.pg)
            .await?;
            if inflight.is_some() {
                tracing::debug!(bug_signature = %sig, "self_heal: writer task already in-flight");
                continue;
            }

            // Enqueue writer task.
            let title = format!("self_heal_writer: {sig}");
            let payload = serde_json::json!({
                "command": format!("ff self-heal run-writer --bug-sig {sig}"),
                "summary": format!("Self-heal writer for bug {sig}")
            });
            let trigger_spec = serde_json::json!({});
            let required_caps = serde_json::json!([]);

            match ff_db::queries::pg_enqueue_deferred(
                &self.pg,
                &title,
                "shell_command",
                &payload,
                "now",
                &trigger_spec,
                Some(&self.my_name),
                &required_caps,
                Some(&format!("leader:{}", self.my_name)),
                Some(3),
            )
            .await
            {
                Ok(id) => {
                    tracing::info!(
                        bug_signature = %sig,
                        task_id = %id,
                        "self_heal: enqueued writer deferred task"
                    );
                    // Mark as fixing so we don't re-dispatch next tick.
                    sqlx::query(
                        "UPDATE fleet_self_heal_queue \
                         SET status = 'fixing', attempts = attempts + 1, \
                             last_attempt_at = NOW(), writer_computer_id = $2 \
                         WHERE bug_signature = $1",
                    )
                    .bind(&sig)
                    .bind(self.my_computer_id)
                    .execute(&self.pg)
                    .await?;
                }
                Err(err) => {
                    tracing::warn!(
                        bug_signature = %sig,
                        error = %err,
                        "self_heal: failed to enqueue writer task"
                    );
                }
            }
        }

        Ok(())
    }

    /// V121+: self-heal-on-error tick.
    ///
    /// Roughly every 30 minutes (gated by the marker file
    /// `~/.forgefleet/interaction-errors.last`), aggregate recent error rows
    /// from `ff_interactions` by `error_signature` and enqueue any *novel*
    /// signature into `fleet_self_heal_queue` (status `detected`). The next
    /// `self_heal_scan` pass then dispatches the writer task to the
    /// claude-code/kimi/codex/local self-heal writers.
    ///
    /// Single-flight is enforced two ways: the marker file caps the SELECT to
    /// ~once per 30 min per leader, and `ON CONFLICT (bug_signature) DO NOTHING`
    /// means a signature already in the queue (in any status) is never reset.
    /// We classify interaction errors as tier `T2` — runtime/interaction-layer
    /// failures, distinct from the `T0/T1` build/test bugs that
    /// `fleet_bug_reports` feeds.
    pub async fn scan_interaction_errors(&self) -> Result<(), LeaderError> {
        // ── Marker-file time gate (~30 min) ─────────────────────────────────
        let home = std::env::var("HOME").unwrap_or_default();
        let marker = format!("{home}/.forgefleet/interaction-errors.last");
        const INTERVAL_SECS: u64 = 30 * 60;
        if let Ok(meta) = std::fs::metadata(&marker) {
            if let Ok(modified) = meta.modified() {
                if let Ok(elapsed) = modified.elapsed() {
                    if elapsed.as_secs() < INTERVAL_SECS {
                        return Ok(()); // ran recently; skip this tick
                    }
                }
            }
        }

        // ── Aggregate recent interaction errors by signature ────────────────
        // 35-minute lookback slightly overlaps the 30-min cadence so we never
        // drop an error that landed between the gate and the query.
        let rows = sqlx::query(
            "SELECT error_signature, MAX(error_text) AS error_text, COUNT(*) AS n \
             FROM ff_interactions \
             WHERE outcome = 'error' \
               AND ts > NOW() - INTERVAL '35 minutes' \
               AND error_signature IS NOT NULL \
             GROUP BY error_signature",
        )
        .fetch_all(&self.pg)
        .await?;

        let mut novel = 0u32;
        for row in &rows {
            let sig: String = row.try_get("error_signature")?;
            let report_count: i64 = row.try_get("n").unwrap_or(1);
            let error_text: Option<String> = row.try_get("error_text").ok().flatten();

            // Insert only if this signature is not already tracked. DO NOTHING
            // leaves any in-flight/fixed row untouched (no status reset).
            let inserted = sqlx::query(
                "INSERT INTO fleet_self_heal_queue \
                    (bug_signature, tier, status, report_count, created_at) \
                 VALUES ($1, 'T2', 'detected', $2, NOW()) \
                 ON CONFLICT (bug_signature) DO NOTHING",
            )
            .bind(&sig)
            .bind(report_count as i32)
            .execute(&self.pg)
            .await?;

            if inserted.rows_affected() > 0 {
                novel += 1;
                tracing::info!(
                    node = %self.my_name,
                    error_signature = %sig,
                    report_count,
                    error_text = error_text.as_deref().unwrap_or(""),
                    "scan_interaction_errors: enqueued novel error signature for self-heal"
                );
            } else {
                tracing::debug!(
                    error_signature = %sig,
                    "scan_interaction_errors: signature already in self-heal queue; skipping"
                );
            }
        }

        if novel > 0 {
            tracing::info!(
                node = %self.my_name,
                novel,
                scanned = rows.len(),
                "scan_interaction_errors: queued novel interaction errors"
            );
        }

        // ── Bump the marker so we don't re-scan for ~30 min ─────────────────
        if let Some(parent) = std::path::Path::new(&marker).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&marker, Utc::now().to_rfc3339());

        Ok(())
    }

    /// Re-import the open-design SKILL.md catalog when the leader's local
    /// `open_design_git` checkout has advanced since the last sync. Leader-only
    /// (skills land in shared Postgres → once per fleet, not once per node).
    /// Cheap: one version query + a marker-file compare; the directory walk +
    /// upsert only runs on the tick where the checkout SHA actually changed, so
    /// `ff skills list` stays in step with the auto-upgrade pipeline's git pulls
    /// without re-importing 450+ files every 15s.
    async fn sync_open_design_skills(&self) -> anyhow::Result<()> {
        // This tick runs on the leader, so query *this* node's installed SHA.
        let installed: Option<String> = sqlx::query_scalar(
            "SELECT installed_version FROM computer_software \
             WHERE computer_id = $1 AND software_id = 'open_design_git'",
        )
        .bind(self.my_computer_id)
        .fetch_optional(&self.pg)
        .await?;
        let Some(version) = installed.filter(|v| !v.is_empty()) else {
            return Ok(()); // open-design not installed on the leader
        };

        let home = std::env::var("HOME").unwrap_or_default();
        let checkout = format!("{home}/.forgefleet/sub-agent-0/open-design");
        // `skills/` presence is the sentinel that the checkout is materialized.
        if !std::path::Path::new(&checkout).join("skills").exists() {
            return Ok(());
        }
        let marker = format!("{home}/.forgefleet/skills-open-design.last-version");
        if std::fs::read_to_string(&marker)
            .ok()
            .as_deref()
            .map(str::trim)
            == Some(version.as_str())
        {
            return Ok(()); // already synced at this SHA
        }

        let (imported, updated, _retired, errors) = crate::skills_db::import_repo_skills(
            &self.pg,
            std::path::Path::new(&checkout),
            "open-design",
            Some("https://github.com/nexu-io/open-design"),
            None,
        )
        .await?;
        let _ = crate::skills_db::materialize_all(&self.pg).await;
        let _ = std::fs::write(&marker, &version);
        tracing::info!(
            node = %self.my_name,
            sha = %version,
            imported,
            updated,
            errors,
            "synced open-design skills from leader checkout"
        );
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
    // Filter out computers explicitly marked `never_leader` (V49).
    // Laptops that travel off-LAN should never be promoted — if they
    // win and then drop wifi, the whole fleet's leader-gated subsystems
    // (auto-upgrade, sub-agent reaper, openclaw reconciler, task
    // watchdog) freeze until the laptop returns. Reads through
    // COALESCE so legacy rows (NULL eligibility) still count.
    // Read worker rows + their election_priority directly from
    // `fleet_workers` (canonical post-V83). Joined to `computers` for the
    // eligibility filter on the human-physical-machine side. The previous
    // implementation joined `fleet_members` instead — that table was a
    // redundant projection of fleet_workers and is now retired.
    let rows = sqlx::query(
        "SELECT c.id   AS computer_id,
                fw.name AS member_name,
                fw.election_priority AS election_priority
         FROM fleet_workers fw
         JOIN computers c ON c.name = fw.name
         WHERE COALESCE(c.election_eligibility, 'eligible') <> 'never_leader'",
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

/// Pure election rule: pick the most-preferred candidate (lowest
/// `election_priority`, alphabetical tie-break) that is **alive** and **not
/// voluntarily yielding**. `None` when every candidate is dead or yielding —
/// the caller then refuses to claim rather than going leaderless.
///
/// HA Phase 1 invariant: with an empty `yielding` set this is byte-identical to
/// the pre-Phase-1 behaviour (alive filter only), so the feature is fully
/// dormant unless an operator issues `ff fleet leader step-down`.
fn pick_best_candidate<'a>(
    candidates: &'a [Candidate],
    alive: &std::collections::HashMap<String, bool>,
    yielding: &std::collections::HashSet<String>,
) -> Option<&'a Candidate> {
    candidates
        .iter()
        .filter(|c| {
            alive.get(&c.member_name).copied().unwrap_or(false)
                && !yielding.contains(&c.member_name)
        })
        .min_by(|a, b| {
            a.election_priority
                .cmp(&b.election_priority)
                .then_with(|| a.member_name.cmp(&b.member_name))
        })
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

    #[test]
    fn parse_yield_request_extracts_member_and_deadline() {
        let until = Utc::now() + ChronoDuration::minutes(10);
        let raw = format!("taylor|{}", until.to_rfc3339());
        let (member, parsed) = parse_yield_request(&raw).expect("valid request parses");
        assert_eq!(member, "taylor");
        // Round-trips to within a second (rfc3339 sub-second precision varies).
        assert!((parsed - until).num_seconds().abs() <= 1);
    }

    #[test]
    fn parse_yield_request_trims_whitespace() {
        let until = Utc::now() + ChronoDuration::minutes(5);
        let raw = format!("  james  |  {}  ", until.to_rfc3339());
        let (member, _) = parse_yield_request(&raw).expect("trims and parses");
        assert_eq!(member, "james");
    }

    #[test]
    fn parse_yield_request_rejects_malformed() {
        // No separator, empty member, and a non-timestamp all yield None so a
        // garbled secret can never wedge the election.
        assert!(parse_yield_request("taylor").is_none());
        assert!(parse_yield_request("|2026-06-13T00:00:00Z").is_none());
        assert!(parse_yield_request("taylor|not-a-date").is_none());
        assert!(parse_yield_request("").is_none());
    }

    fn cand(name: &str, prio: i32) -> Candidate {
        Candidate {
            member_name: name.to_string(),
            computer_id: Uuid::nil(),
            election_priority: prio,
        }
    }

    fn alive_all(names: &[&str]) -> std::collections::HashMap<String, bool> {
        names.iter().map(|n| (n.to_string(), true)).collect()
    }

    #[test]
    fn pick_best_no_yield_is_pre_phase1_behaviour() {
        // Empty yielding set → lowest priority wins, exactly as before.
        let cands = vec![cand("taylor", 0), cand("james", 10), cand("sophie", 20)];
        let alive = alive_all(&["taylor", "james", "sophie"]);
        let yielding = std::collections::HashSet::new();
        let best = pick_best_candidate(&cands, &alive, &yielding).unwrap();
        assert_eq!(best.member_name, "taylor");
    }

    #[test]
    fn pick_best_skips_yielding_leader() {
        // taylor (priority 0) is yielding → next-preferred alive node wins.
        let cands = vec![cand("taylor", 0), cand("james", 10), cand("sophie", 20)];
        let alive = alive_all(&["taylor", "james", "sophie"]);
        let yielding = ["taylor".to_string()].into_iter().collect();
        let best = pick_best_candidate(&cands, &alive, &yielding).unwrap();
        assert_eq!(best.member_name, "james");
    }

    #[test]
    fn pick_best_none_when_all_yield_or_dead() {
        // A yield with no eligible successor → None → caller keeps current
        // leader rather than going leaderless.
        let cands = vec![cand("taylor", 0), cand("james", 10)];
        let mut alive = alive_all(&["taylor"]);
        alive.insert("james".to_string(), false); // james dead
        let yielding = ["taylor".to_string()].into_iter().collect();
        assert!(pick_best_candidate(&cands, &alive, &yielding).is_none());
    }

    #[test]
    fn pick_best_priority_tie_breaks_alphabetically() {
        let cands = vec![cand("zeta", 5), cand("alpha", 5)];
        let alive = alive_all(&["zeta", "alpha"]);
        let yielding = std::collections::HashSet::new();
        let best = pick_best_candidate(&cands, &alive, &yielding).unwrap();
        assert_eq!(best.member_name, "alpha");
    }

    #[test]
    fn parse_yield_request_expiry_is_caller_checked() {
        // parse_yield_request itself does NOT enforce the deadline — it only
        // decodes. The expiry comparison (`Utc::now() < until`) lives in
        // self_should_yield, so a past deadline still parses cleanly here.
        let past = Utc::now() - ChronoDuration::minutes(1);
        let raw = format!("taylor|{}", past.to_rfc3339());
        let (member, until) = parse_yield_request(&raw).expect("past deadline still parses");
        assert_eq!(member, "taylor");
        assert!(until < Utc::now());
    }
}
