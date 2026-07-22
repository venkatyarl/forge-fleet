//! Pillar 4 — distributed concurrent development scheduler tick.
//!
//! Leader-only, serial. Each tick: reap stale leases (freeing slots + returning
//! their work_items to the ready pool), then assign `status='ready'` work_items
//! to free fleet slots via [`ff_db::pg_assign_work_item`] (one active lease per
//! item, enforced by a partial-unique index). Single-leader serial execution
//! means no cross-process race — no `FOR UPDATE SKIP LOCKED` needed.
//!
//! v1 = assignment (lease + slot reservation). The slot's agent loop picks up
//! its `current_work_item_id` to execute; the merge-queue drain + dispatch are
//! follow-ups. Only touches work_items explicitly flagged `status='ready'`, so
//! operator PM items (status 'idea' etc.) are never disturbed.
//!
//! Design: `.forgefleet/plans/DECISION-pillar4-canonical-home.md`.

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use std::collections::{HashMap, HashSet, VecDeque};
use tracing::{info, warn};

/// Lease heartbeat deadline: a slot must heartbeat within this window or its
/// lease is reaped and the work_item re-queued. `pub(crate)` so the dispatch
/// path can keep the Lane-1 local-codegen timeout STRICTLY BELOW it (see
/// `work_item_dispatch::LANE1_TIMEOUT_SECS`) — a slow local lane must fail over
/// to the cloud backstop before this reaper can reclaim the lease.
///
/// 480 (was 180): with a 45s heartbeat cadence, 180s tolerated only ~3 missed
/// beats — under wave-burst load daemons routinely missed that window and
/// healthy builds were reaped as "stalled" (2026-07-19: 100+ takeovers in 2h
/// fleet-wide, most of the night's stall-class failures). 480s tolerates ~10
/// missed beats while MAX_LEASE_DURATION_SECS still bounds true wedges.
pub(crate) const LEASE_STALE_SECS: i64 = 480;
/// Hard ceiling on lease HOLD time regardless of heartbeat — reclaims a wedged
/// dispatch that keeps its heartbeat fresh but makes no progress (the
/// "building forever, live heartbeat" wedge). Above a real build's Lane-2 cap
/// (~18.5 min).
pub(crate) const MAX_LEASE_DURATION_SECS: i64 = 45 * 60;
/// Lease lifetime granted at assignment (refreshed by heartbeats).
pub(crate) const LEASE_GRANT_SECS: i64 = 600;
/// Max assignments per tick (back-pressure; the rest wait for the next tick).
const MAX_ASSIGN_PER_TICK: i64 = 64;
/// Minimum age before an `in_progress` work_item with NO active lease is
/// considered orphaned and cancelled. Far above the lease/heartbeat windows so
/// a legitimately-leased item is never swept mid-assignment.
const ORPHAN_MIN_AGE_SECS: i64 = 3600;
/// Failure-convergence ceiling: after this many stalled/reaped attempts a
/// work_item is marked `failed` (with context) instead of re-queued forever.
/// A task the swarm genuinely can't build must STOP thrashing and surface for a
/// human or a retry-with-error-context.
///
/// MUST stay STRICTLY ABOVE `ff_routing_policy::LOCAL_LANE_MAX_TRIES` (=3): the
/// escalation ladder keeps a build on the local Devstral lane for the first
/// LOCAL_LANE_MAX_TRIES attempts, then escalates to cloud (claude/codex). If this
/// cap equals LOCAL_LANE_MAX_TRIES the item dies on the LAST local attempt and
/// cloud NEVER gets a try (root cause of the 2026-07-22 "53 items failed after 3
/// stalled attempts, zero cloud escalation" freeze). 5 = 3 local + 2 cloud tries.
const MAX_BUILD_ATTEMPTS: i32 = 5;
/// Four missed 15-second dispatch passes makes a host ineligible. General
/// Pulse beats may still be fresh when this subsystem clock is stale.
const DISPATCH_TICK_STALE_SECS: i64 = 60;

/// One scheduler pass. Returns the number of work_items assigned this tick.
pub async fn evaluate_work_items(pg: &PgPool) -> Result<usize> {
    let reaped = ff_db::pg_reap_stale_work_item_leases(
        pg,
        LEASE_STALE_SECS,
        MAX_LEASE_DURATION_SECS,
        MAX_BUILD_ATTEMPTS,
    )
    .await?;
    if reaped > 0 {
        warn!(
            reaped,
            "work_item_scheduler: reaped stale leases (slots freed, items re-queued)"
        );
    }

    // Companion sweep: `in_progress` work_items with no active lease can't be
    // reaped by the lease sweep above (they have no lease row). Cancel the ones
    // older than ORPHAN_MIN_AGE_SECS so they stop polluting `in_progress`.
    let orphans = ff_db::pg_reap_orphaned_work_items(pg, ORPHAN_MIN_AGE_SECS).await?;
    if orphans > 0 {
        warn!(
            orphans,
            "work_item_scheduler: cancelled orphaned in_progress work_items (no active lease)"
        );
    }

    // Self-heal: terminally-`failed` items whose last_error was TRANSIENT
    // infrastructure (backend spawn, provider/network, pool, heartbeat) are
    // buildable once the condition clears — return a batch to the ready pool
    // with full redispatch eligibility restored (leases released, assignment
    // cleared). Best-effort: a sweep failure must never stall assignment.
    match crate::self_heal::requeue_transient_failures(pg).await {
        Ok(healed) if healed > 0 => info!(
            healed,
            "work_item_scheduler: self-heal requeued transiently-failed work_items"
        ),
        Ok(_) => {}
        Err(e) => warn!(error = %e, "work_item_scheduler: self-heal requeue sweep failed"),
    }

    // Auto-complete decomposed parents (bug/feature) once all of their task
    // children are terminal. This stops parent rows from lingering in `ready`
    // and cluttering the board after their leaves finish.
    let completed_parents = ff_db::pg_complete_parent_work_items(pg).await?;
    if completed_parents > 0 {
        info!(
            completed_parents,
            "work_item_scheduler: auto-completed parent work_items"
        );
    }

    let ready = ff_db::pg_ready_work_items(pg, MAX_ASSIGN_PER_TICK).await?;
    if ready.is_empty() {
        return Ok(0);
    }

    // Slots that are free fleet-wide (a pinned item filters to its host).
    let mut active_by_computer: HashMap<uuid::Uuid, usize> = sqlx::query(
        "SELECT computer_id, COUNT(*)::bigint AS active \
           FROM work_item_leases \
          WHERE released_at IS NULL \
          GROUP BY computer_id",
    )
    .fetch_all(pg)
    .await?
    .into_iter()
    .map(|row| {
        (
            row.get("computer_id"),
            row.get::<i64, _>("active").max(0) as usize,
        )
    })
    .collect();
    // Active projects fleet-wide (by currently-leased work_items), used below
    // to cap each project's fair share of this tick's slot capacity. Distinct
    // from `interleave_by_project`, which only reorders THIS tick's ready set —
    // a project already holding a disproportionate share of ACTIVE leases must
    // be deprioritized even if its ready backlog looks the same size as a
    // less-active project's. Read straight off `work_item_leases.project_id`
    // (denormalized at lease-assignment time) instead of joining `work_items`.
    let active_by_project: HashMap<Option<String>, usize> =
        ff_db::pg_active_lease_counts_by_project(pg)
            .await?
            .into_iter()
            .map(|(project_id, active)| (project_id, active.max(0) as usize))
            .collect();
    let mut global_free = ff_db::pg_free_slots(pg, None, MAX_ASSIGN_PER_TICK).await?;
    let now = Utc::now();
    let dispatch_live: HashSet<uuid::Uuid> =
        sqlx::query("SELECT id, dispatch_tick_at FROM computers")
            .fetch_all(pg)
            .await?
            .into_iter()
            .filter_map(|row| {
                let id = row.get("id");
                let tick = row.get::<Option<DateTime<Utc>>, _>("dispatch_tick_at");
                dispatch_tick_is_fresh(tick, now).then_some(id)
            })
            .collect();
    global_free.retain(|slot| dispatch_live.contains(&slot.computer_id));
    global_free.retain(|slot| dispatch_capacity_left(&active_by_computer, slot.computer_id));
    if global_free.is_empty() {
        info!(
            ready = ready.len(),
            "work_item_scheduler: items ready but no free slots"
        );
        return Ok(0);
    }

    // Prefer slots on computers with a LIVE agent-capable LLM endpoint so we
    // don't hand a build to a node whose model is already dead at tick time
    // (the E3 finding: a stale 'healthy' row wasted a ~6min lease cycle on
    // priya). This is a PREFERENCE, not a gate — `pop_slot` falls back to any
    // free slot if no viable one remains, so assignment never starves when the
    // deployment rows are momentarily stale (e.g. right after a deploy).
    let viable: HashSet<uuid::Uuid> = match ff_db::pg_agent_viable_computer_ids(pg).await {
        Ok(ids) => ids.into_iter().collect(),
        Err(e) => {
            warn!(error = %e, "work_item_scheduler: agent-viability lookup failed; assigning without preference");
            std::collections::HashSet::new()
        }
    };

    let mut pool: Vec<ff_db::FreeSlot> = global_free;
    let mut assigned = 0usize;
    let mut fallback_assigns = 0usize;

    let interleaved = interleave_by_project(ready);
    let distinct_projects: HashSet<&Option<String>> = active_by_project
        .keys()
        .chain(interleaved.iter().map(|i| &i.project_id))
        .collect();
    let total_capacity = active_by_project.values().sum::<usize>() + pool.len();
    let fair_share = project_fair_share(distinct_projects.len(), total_capacity);

    // Two passes so fair-share stays work-conserving: first give every project
    // first refusal up to `fair_share`; anything a project couldn't take because
    // it was already at/over share (`deferred`) gets a second shot once every
    // project has been through pass one, so free slots never sit idle just
    // because the projects that could use them were momentarily capped.
    let mut assigned_this_tick: HashMap<Option<String>, usize> = HashMap::new();
    let mut deferred: Vec<ff_db::ReadyWorkItem> = Vec::new();
    for item in interleaved {
        if project_at_fair_share(
            &item.project_id,
            &active_by_project,
            &assigned_this_tick,
            fair_share,
        ) {
            deferred.push(item);
            continue;
        }
        if try_assign_item(
            pg,
            &item,
            &mut pool,
            &mut active_by_computer,
            &dispatch_live,
            &viable,
            &mut fallback_assigns,
        )
        .await
        {
            assigned += 1;
            *assigned_this_tick
                .entry(item.project_id.clone())
                .or_default() += 1;
        }
    }
    for item in deferred {
        if try_assign_item(
            pg,
            &item,
            &mut pool,
            &mut active_by_computer,
            &dispatch_live,
            &viable,
            &mut fallback_assigns,
        )
        .await
        {
            assigned += 1;
            *assigned_this_tick
                .entry(item.project_id.clone())
                .or_default() += 1;
        }
    }

    if assigned > 0 {
        info!(
            assigned,
            fallback_assigns, "work_item_scheduler: assigned work_items to fleet slots"
        );
    }
    if fallback_assigns > 0 {
        // Not silent: surface that we leased build work to nodes with no live
        // agent endpoint in the DB. Expected transiently after a deploy; if it
        // persists, agent-capability detection or the reconciler is lagging.
        warn!(
            fallback_assigns,
            "work_item_scheduler: assigned to non-agent-viable slots (no live agent endpoint); \
             self-heal will reclaim if the build stalls"
        );
    }
    Ok(assigned)
}

fn dispatch_tick_is_fresh(tick_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    tick_at.is_some_and(|tick| tick >= now - chrono::Duration::seconds(DISPATCH_TICK_STALE_SECS))
}

/// Attempt to assign one ready work_item to a free slot, updating the shared
/// pool / dispatch-capacity bookkeeping on success. Extracted so the fair-share
/// two-pass loop in `evaluate_work_items` (first pass: projects under their
/// share, second pass: work-conserving overflow) shares one assignment path
/// instead of forking it.
#[allow(clippy::too_many_arguments)]
async fn try_assign_item(
    pg: &PgPool,
    item: &ff_db::ReadyWorkItem,
    pool: &mut Vec<ff_db::FreeSlot>,
    active_by_computer: &mut HashMap<uuid::Uuid, usize>,
    dispatch_live: &HashSet<uuid::Uuid>,
    viable: &HashSet<uuid::Uuid>,
    fallback_assigns: &mut usize,
) -> bool {
    // Honor a host pin by re-querying that host's free slots; else take from
    // the shared pool, preferring an agent-viable computer.
    let slot = if let Some(host) = item.assigned_computer.as_deref() {
        match ff_db::pg_free_slots(pg, Some(host), 1).await {
            Ok(mut v) => v
                .pop()
                .filter(|slot| dispatch_live.contains(&slot.computer_id))
                .filter(|slot| dispatch_capacity_left(active_by_computer, slot.computer_id)),
            Err(e) => {
                warn!(host, error = %e, "work_item_scheduler: pinned-slot lookup failed");
                None
            }
        }
    } else {
        pop_slot(pool, viable, fallback_assigns)
    };
    let Some(slot) = slot else { return false };

    match ff_db::pg_assign_work_item(
        pg,
        item.id,
        slot.sub_agent_id,
        slot.computer_id,
        LEASE_GRANT_SECS,
    )
    .await
    {
        Ok(true) => {
            *active_by_computer.entry(slot.computer_id).or_default() += 1;
            spawn_claim_heartbeat(pg.clone(), item.id);
            // Keep the shared pool consistent if a pinned assignment consumed
            // a slot that also sat in `pool`, and remove the rest of this
            // computer's slots as soon as its dispatch capacity is full.
            pool.retain(|s| {
                s.sub_agent_id != slot.sub_agent_id
                    && dispatch_capacity_left(active_by_computer, s.computer_id)
            });
            true
        }
        Ok(false) => false, // lost the race / already leased
        Err(e) => {
            warn!(item = %item.id, error = %e, "work_item_scheduler: assign failed");
            false
        }
    }
}

/// Each project's fair share of this tick's total slot capacity (pre-existing
/// active leases + still-free slots), split evenly across every project that
/// is either currently active or has ready work. Ceil-divided so a remainder
/// favors filling slots over under-assigning. Pure so fair-share sizing is
/// testable without a database.
fn project_fair_share(distinct_projects: usize, total_capacity: usize) -> usize {
    if distinct_projects == 0 {
        return total_capacity;
    }
    total_capacity.div_ceil(distinct_projects)
}

/// True once `project_id` has reached (or exceeded) its fair share of slot
/// capacity, counting both its pre-existing active leases and whatever this
/// tick has already assigned it. The scheduler defers items past this point to
/// a work-conserving second pass instead of dropping them, so a capped project
/// still gets surplus capacity once every project has had first refusal. Pure
/// so the skip rule is testable without a database.
fn project_at_fair_share(
    project_id: &Option<String>,
    active_by_project: &HashMap<Option<String>, usize>,
    assigned_this_tick: &HashMap<Option<String>, usize>,
    fair_share: usize,
) -> bool {
    let active = active_by_project.get(project_id).copied().unwrap_or(0);
    let assigned_now = assigned_this_tick.get(project_id).copied().unwrap_or(0);
    active + assigned_now >= fair_share
}

/// Keep a newly-created lease alive while it waits for the owning host's
/// dispatch loop. Once dispatch changes the item from `claimed` to `building`,
/// `dispatch_one`'s own guard takes over for the rest of the lease lifecycle.
fn spawn_claim_heartbeat(pg: PgPool, work_item_id: uuid::Uuid) {
    tokio::spawn(async move {
        let _guard = crate::work_item_dispatch::HeartbeatGuard::spawn(work_item_id);
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
            crate::work_item_dispatch::HEARTBEAT_SECS,
        ));
        loop {
            ticker.tick().await;
            let still_queued = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM work_items WHERE id = $1 AND status = 'claimed')",
            )
            .bind(work_item_id)
            .fetch_one(&pg)
            .await
            .unwrap_or(true);
            if !still_queued {
                break;
            }
        }
    });
}

fn dispatch_capacity_left(
    active_by_computer: &HashMap<uuid::Uuid, usize>,
    computer_id: uuid::Uuid,
) -> bool {
    active_by_computer.get(&computer_id).copied().unwrap_or(0)
        < crate::work_item_dispatch::MAX_DISPATCH_PER_TICK as usize
}

/// Take one free slot from `pool`, preferring a slot whose computer currently
/// has a live agent-capable LLM endpoint (`viable`). Falls back to any free slot
/// (bumping `fallback_assigns`) so assignment never starves when the deployment
/// rows are momentarily stale. Pure so the prefer-with-fallback rule is testable.
fn pop_slot(
    pool: &mut Vec<ff_db::FreeSlot>,
    viable: &std::collections::HashSet<uuid::Uuid>,
    fallback_assigns: &mut usize,
) -> Option<ff_db::FreeSlot> {
    if let Some(idx) = pool.iter().position(|s| viable.contains(&s.computer_id)) {
        return Some(pool.remove(idx));
    }
    // No viable slot left — fall back to any free slot (preserves prior pop()).
    let slot = pool.pop();
    if slot.is_some() {
        *fallback_assigns += 1;
    }
    slot
}

/// Round-robin the ready list across projects, preserving each project's
/// internal (risk/age) order, so assignment order gives every project a fair
/// share of this tick's free slots. `pg_ready_work_items` already ranks
/// per-project BEFORE its LIMIT so no project can monopolize the fetched set;
/// this pass enforces the same guarantee on selection order locally, so the
/// scheduler stays fair even if the fetch ordering regresses. Work-conserving:
/// no item is dropped — once smaller projects drain, surplus slots go to
/// whatever remains. A NULL project_id is its own bucket. Pure so fair-share
/// enforcement is testable without a database.
fn interleave_by_project(items: Vec<ff_db::ReadyWorkItem>) -> Vec<ff_db::ReadyWorkItem> {
    // Vec-of-buckets (not HashMap) keeps project order = first appearance,
    // which the fetch query already sorted by top-item priority.
    let mut buckets: Vec<(Option<String>, VecDeque<ff_db::ReadyWorkItem>)> = Vec::new();
    for item in items {
        match buckets.iter_mut().find(|(p, _)| *p == item.project_id) {
            Some((_, q)) => q.push_back(item),
            None => buckets.push((item.project_id.clone(), VecDeque::from([item]))),
        }
    }
    let total: usize = buckets.iter().map(|(_, q)| q.len()).sum();
    let mut out = Vec::with_capacity(total);
    while out.len() < total {
        for (_, q) in buckets.iter_mut() {
            if let Some(item) = q.pop_front() {
                out.push(item);
            }
        }
    }
    out
}

/// Spawn the leader-gated scheduler loop. The skip path reads the process-local
/// leader cache instead of probing Postgres.
pub fn spawn_work_item_scheduler(
    pg: PgPool,
    _worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !crate::leader_cache::is_current_leader() {
                        continue;
                    }
                    if let Err(e) = evaluate_work_items(&pg).await {
                        warn!(error = %e, "work_item_scheduler tick failed");
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        info!("work_item_scheduler loop stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(computer: uuid::Uuid) -> ff_db::FreeSlot {
        ff_db::FreeSlot {
            sub_agent_id: uuid::Uuid::new_v4(),
            computer_id: computer,
        }
    }

    fn ready(project: Option<&str>) -> ff_db::ReadyWorkItem {
        ff_db::ReadyWorkItem {
            id: uuid::Uuid::new_v4(),
            assigned_computer: None,
            project_id: project.map(str::to_owned),
        }
    }

    /// Fair-share enforcement: even a worst-case fetched set where one project's
    /// items arrive as a contiguous block ahead of everyone else's (the attempt-1
    /// monopoly failure) must be reordered so every ready project appears within
    /// the first `distinct_projects` picks. While every project still has items
    /// queued, no project may hold more than `k` of the first
    /// `k * distinct_projects` picks.
    #[test]
    fn fair_share_stops_one_project_monopolizing_selection() {
        let mut items: Vec<_> = (0..6).map(|_| ready(Some("alpha"))).collect();
        items.extend([
            ready(Some("beta")),
            ready(Some("beta")),
            ready(Some("gamma")),
        ]);
        let out = interleave_by_project(items);

        let projects_in_prefix: HashSet<_> =
            out[..3].iter().map(|i| i.project_id.clone()).collect();
        assert_eq!(
            projects_in_prefix.len(),
            3,
            "first 3 picks must cover all 3 ready projects"
        );

        // Equal backlogs (3 projects x 3 items, alpha's block first): the k-cap
        // invariant holds for every round because no bucket drains early.
        let mut even: Vec<_> = (0..3).map(|_| ready(Some("alpha"))).collect();
        even.extend((0..3).map(|_| ready(Some("beta"))));
        even.extend((0..3).map(|_| ready(Some("gamma"))));
        let out = interleave_by_project(even);
        for k in 1..=3 {
            for project in ["alpha", "beta", "gamma"] {
                let share = out[..k * 3]
                    .iter()
                    .filter(|i| i.project_id.as_deref() == Some(project))
                    .count();
                assert_eq!(
                    share,
                    k,
                    "{project} took {share} of the first {} picks (fair share is {k})",
                    k * 3
                );
            }
        }
    }

    /// Work-conserving: interleaving reorders but never drops items — once the
    /// smaller projects drain, the surplus project fills the remaining picks,
    /// and each project's internal (risk/age) order is preserved.
    #[test]
    fn fair_share_is_work_conserving_and_order_stable() {
        let alpha: Vec<_> = (0..4).map(|_| ready(Some("alpha"))).collect();
        let beta = vec![ready(Some("beta"))];
        let alpha_ids: Vec<_> = alpha.iter().map(|i| i.id).collect();
        let mut items = alpha;
        items.extend(beta);
        let out = interleave_by_project(items);

        assert_eq!(out.len(), 5, "no item may be dropped");
        assert!(
            out[2..]
                .iter()
                .all(|i| i.project_id.as_deref() == Some("alpha")),
            "surplus picks must fall to the remaining project, not go unused"
        );
        let alpha_out: Vec<_> = out
            .iter()
            .filter(|i| i.project_id.as_deref() == Some("alpha"))
            .map(|i| i.id)
            .collect();
        assert_eq!(
            alpha_out, alpha_ids,
            "within-project order must be preserved"
        );
    }

    /// Items with no project_id form their own fair-share bucket rather than
    /// being merged into another project or starved.
    #[test]
    fn fair_share_treats_null_project_as_own_bucket() {
        let items = vec![
            ready(Some("alpha")),
            ready(Some("alpha")),
            ready(None),
            ready(None),
        ];
        let out = interleave_by_project(items);
        assert!(
            out[..2].iter().any(|i| i.project_id.is_none()),
            "project-less items must get a fair-share pick too"
        );
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn dispatch_capacity_counts_active_leases() {
        let computer = uuid::Uuid::new_v4();
        let mut active = HashMap::new();
        assert!(dispatch_capacity_left(&active, computer));
        active.insert(
            computer,
            crate::work_item_dispatch::MAX_DISPATCH_PER_TICK as usize,
        );
        assert!(!dispatch_capacity_left(&active, computer));
    }

    /// E3 finding: prefer an agent-viable computer's slot when one exists, so a
    /// build isn't handed to a node with no live LLM endpoint.
    #[test]
    fn pop_slot_prefers_a_viable_computer() {
        let dead = uuid::Uuid::new_v4();
        let live = uuid::Uuid::new_v4();
        // dead node's slot is "fresher" (would win a plain pop()) but has no LLM.
        let mut pool = vec![slot(live), slot(dead)];
        let viable: std::collections::HashSet<_> = [live].into_iter().collect();
        let mut fb = 0;
        let picked = pop_slot(&mut pool, &viable, &mut fb).unwrap();
        assert_eq!(picked.computer_id, live, "must pick the live-endpoint node");
        assert_eq!(fb, 0, "a viable pick is not a fallback");
        assert_eq!(pool.len(), 1, "exactly one slot consumed");
    }

    /// Safety: when NO slot is agent-viable (e.g. rows stale right after a
    /// deploy), assignment must still proceed via fallback rather than starve.
    #[test]
    fn pop_slot_falls_back_when_none_viable() {
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        let mut pool = vec![slot(a), slot(b)];
        let viable = std::collections::HashSet::new();
        let mut fb = 0;
        assert!(pop_slot(&mut pool, &viable, &mut fb).is_some());
        assert_eq!(fb, 1, "fallback assignment must be counted, not silent");
        // Empty pool yields None without bumping the fallback counter.
        let mut empty: Vec<ff_db::FreeSlot> = vec![];
        assert!(pop_slot(&mut empty, &viable, &mut fb).is_none());
        assert_eq!(fb, 1);
    }

    /// REGRESSION GUARD (reaper bug class #589/#590): same coupling as
    /// `lease_takeover` — the scheduler's own lease-reap window must clear at
    /// least two dispatch heartbeats so a live build's lease is never reclaimed.
    #[test]
    fn lease_stale_window_clears_two_heartbeats() {
        let cadence = crate::work_item_dispatch::HEARTBEAT_SECS as i64;
        assert!(
            LEASE_STALE_SECS >= 2 * cadence,
            "LEASE_STALE_SECS ({LEASE_STALE_SECS}) must be >= 2x the dispatch heartbeat ({cadence})"
        );
    }

    /// Capacity splits evenly (ceil-divided) across every distinct project so a
    /// remainder favors filling slots over under-assigning.
    #[test]
    fn project_fair_share_splits_capacity_evenly() {
        assert_eq!(project_fair_share(3, 9), 3);
        assert_eq!(project_fair_share(3, 10), 4, "remainder rounds up");
        assert_eq!(
            project_fair_share(0, 5),
            5,
            "no projects: share is the whole pool"
        );
    }

    /// A project with no pre-existing active leases and nothing assigned yet
    /// this tick is under its share; once its active-plus-this-tick count
    /// reaches the share it must be skipped (deferred), not assigned further.
    #[test]
    fn project_at_fair_share_counts_active_and_this_tick_assignments() {
        let alpha = Some("alpha".to_string());
        let mut active = HashMap::new();
        active.insert(alpha.clone(), 2usize);
        let mut assigned_this_tick = HashMap::new();

        assert!(
            !project_at_fair_share(&alpha, &active, &assigned_this_tick, 3),
            "2 active < share of 3"
        );

        assigned_this_tick.insert(alpha.clone(), 1);
        assert!(
            project_at_fair_share(&alpha, &active, &assigned_this_tick, 3),
            "2 active + 1 this tick reaches the share of 3"
        );

        let beta = Some("beta".to_string());
        assert!(
            !project_at_fair_share(&beta, &active, &assigned_this_tick, 3),
            "an untracked project has 0 active and 0 assigned so it is under share"
        );
    }

    #[test]
    fn stale_dispatch_tick_is_not_assignment_eligible() {
        let now = Utc::now();
        assert!(!dispatch_tick_is_fresh(None, now));
        assert!(dispatch_tick_is_fresh(
            Some(now - chrono::Duration::seconds(DISPATCH_TICK_STALE_SECS)),
            now
        ));
        assert!(!dispatch_tick_is_fresh(
            Some(now - chrono::Duration::seconds(DISPATCH_TICK_STALE_SECS + 1)),
            now
        ));
    }
}
