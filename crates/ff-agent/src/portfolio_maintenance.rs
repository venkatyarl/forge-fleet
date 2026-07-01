//! Portfolio + drift maintenance ticks for the production daemon.
//!
//! These leader-gated background loops keep the model portfolio and the
//! external-tools drift signals fresh in `forgefleetd`. They were historically
//! wired ONLY into the legacy `ff daemon` (see `ff-terminal/src/daemon_cmd.rs`
//! `[portfolio]` block), so in the production daemon they silently never ran:
//! new models were never discovered (`ff model scout` candidates stagnated),
//! HF model-revision drift was never detected (`ff model upgrade-available`
//! stayed empty), and external-tool drift was never flipped to
//! `upgrade_available`. This module restores them in `src/main.rs` with
//! per-tick *runtime* leader gating (the legacy block gated once at boot, which
//! strands the ticks after a leadership handoff).
//!
//! ## What is intentionally NOT here
//!
//! - **`software_upstream::UpstreamChecker`** — redundant in production.
//!   `AutoUpgradeTick::run_once` already refreshes
//!   `software_registry.latest_version` inline on every hourly tick
//!   (self-built / npm / pypi / github-release / git-head), a faster path than
//!   the 6h `software_upstream` loop. Wiring it would be a second writer of the
//!   same column.
//! - **`CoverageGuard::check_once` (the actuating pass)** — `check_once`
//!   enqueues fleet-wide model loads, which overlaps the demand-driven
//!   autoscaler and is unsafe to actuate unattended. We run the READ-ONLY
//!   `report_once` instead: coverage gaps are detected + logged here, while
//!   actuation stays the autoscaler's job.

use std::future::Future;
use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::coverage_guard::CoverageGuard;
use crate::deployment_catalog_reconciler::DeploymentCatalogReconciler;
use crate::external_tools_upstream::ExternalToolsUpstreamChecker;
use crate::model_scout::ModelScout;
use crate::model_upstream::ModelUpstreamChecker;
use crate::sub_agent_reaper::SubAgentReaper;

const HOUR: u64 = 3600;
const MIN: u64 = 60;

/// Spawn a background loop that runs `work` every `interval`, but only while
/// this node is the elected leader. Leadership is read from the process-local
/// leader cache each tick so the skip path does not touch Postgres. Sleeps
/// `kickoff` before the first run so the boot rush settles, and exits cleanly
/// on shutdown.
fn spawn_leader_gated<F, Fut>(
    pool: PgPool,
    _my_name: String,
    label: &'static str,
    kickoff: Duration,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
    work: F,
) -> JoinHandle<()>
where
    F: Fn(PgPool) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep(kickoff) => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { return; }
            }
        }
        loop {
            if crate::leader_cache::is_current_leader() {
                work(pool.clone()).await;
            } else {
                debug!(tick = label, "portfolio maintenance: skip (not leader)");
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

/// Spawn every portfolio + drift maintenance tick, returning their join handles
/// for the daemon's subsystem set. Each loop is leader-gated; on a follower the
/// loops idle (a cheap leader probe + sleep). Cadences mirror the legacy
/// `ff daemon` `[portfolio]` wiring.
pub fn spawn_portfolio_maintenance(
    pool: PgPool,
    worker_name: String,
    shutdown: watch::Receiver<bool>,
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();

    // Model upstream revision drift (HF) — flips per-computer model files to
    // `revision_available` and updates `upstream_latest_rev`, feeding
    // `ff model upgrade-available`. DB metadata only — no fleet actuation.
    handles.push(spawn_leader_gated(
        pool.clone(),
        worker_name.clone(),
        "model-upstream",
        Duration::from_secs(MIN),
        Duration::from_secs(24 * HOUR),
        shutdown.clone(),
        |pool| async move {
            match ModelUpstreamChecker::new(pool).check_all().await {
                Ok(r) => debug!(
                    checked = r.checked,
                    updated = r.updated,
                    errors = r.errors.len(),
                    "model upstream tick"
                ),
                Err(e) => warn!(error = %e, "model upstream tick failed"),
            }
        },
    ));

    // New-model discovery — queries HF per `fleet_task_coverage` task and
    // inserts survivors as `lifecycle_status='candidate'` for operator review.
    handles.push(spawn_leader_gated(
        pool.clone(),
        worker_name.clone(),
        "model-scout",
        Duration::from_secs(5 * MIN),
        Duration::from_secs(168 * HOUR),
        shutdown.clone(),
        |pool| async move {
            match ModelScout::new(pool).scout_once().await {
                Ok(r) => debug!(
                    discovered = r.discovered,
                    added = r.added_as_candidates,
                    "model scout tick"
                ),
                Err(e) => warn!(error = %e, "model scout tick failed"),
            }
        },
    ));

    // External-tools upstream drift — scans the `external_tools` catalog for
    // new releases and flips `computer_external_tools.status` rows to
    // `upgrade_available`. Pure detector; install dispatch is `ff ext install`.
    handles.push(spawn_leader_gated(
        pool.clone(),
        worker_name.clone(),
        "ext-tools-upstream",
        Duration::from_secs(75),
        Duration::from_secs(6 * HOUR),
        shutdown.clone(),
        |pool| async move {
            match ExternalToolsUpstreamChecker::new(pool).check_all().await {
                Ok(r) => debug!(
                    checked = r.checked,
                    updated = r.updated,
                    errors = r.errors.len(),
                    "external-tools upstream tick"
                ),
                Err(e) => warn!(error = %e, "external-tools upstream tick failed"),
            }
        },
    ));

    // Deployment → catalog reconciler — auto-declares an `active` catalog row
    // for any live deployment of a structurally-unambiguous family (embedding,
    // vision, ASR, reranker) that has no catalog row, so coverage stops
    // reporting false gaps for tasks the fleet is demonstrably serving. Runs
    // BEFORE the coverage tick (shorter kickoff) so coverage sees the reconciled
    // state. Ambiguous chat/code models are left for the operator; writes use
    // ON CONFLICT DO NOTHING so curated rows win. See the module docs.
    handles.push(spawn_leader_gated(
        pool.clone(),
        worker_name.clone(),
        "deployment-catalog-reconciler",
        Duration::from_secs(60),
        Duration::from_secs(30 * MIN),
        shutdown.clone(),
        |pool| async move {
            match DeploymentCatalogReconciler::new(pool)
                .reconcile_once(false)
                .await
            {
                Ok(r) => debug!(
                    created = r.created.len(),
                    skipped_ambiguous = r.skipped_ambiguous.len(),
                    already_cataloged = r.already_cataloged,
                    "deployment-catalog reconcile tick"
                ),
                Err(e) => warn!(error = %e, "deployment-catalog reconcile tick failed"),
            }
        },
    ));

    // Model coverage — READ-ONLY gap detection (`report_once` never enqueues a
    // load; see module docs). Surfaces portfolio gaps at INFO for visibility;
    // remediation stays the autoscaler's job.
    handles.push(spawn_leader_gated(
        pool.clone(),
        worker_name.clone(),
        "coverage-guard",
        Duration::from_secs(90),
        Duration::from_secs(15 * MIN),
        shutdown.clone(),
        |pool| async move {
            match CoverageGuard::new_dbonly(pool).report_once().await {
                Ok(r) if r.gaps.is_empty() => debug!(
                    required = r.tasks_required,
                    covered = r.tasks_covered,
                    "coverage guard tick (no gaps)"
                ),
                Ok(r) => info!(
                    required = r.tasks_required,
                    covered = r.tasks_covered,
                    gaps = r.gaps.len(),
                    "coverage guard tick: portfolio gaps detected"
                ),
                Err(e) => warn!(error = %e, "coverage guard tick failed"),
            }
        },
    ));

    // Stuck agent-slot reaper — resets `sub_agents` rows wedged in `error`/`busy`
    // so the dispatch queue can't lock up. Self-gates on leader internally, so
    // it's spawned unconditionally (like `AutoUpgradeTick`).
    handles.push(SubAgentReaper::new(pool, worker_name).spawn(shutdown));

    handles
}
