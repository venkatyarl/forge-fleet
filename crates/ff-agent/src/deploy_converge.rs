//! Native rolling-deployment convergence tick (operator 2026-07-25).
//!
//! Makes fleet convergence AUTOMATIC + NATIVE instead of an operator running
//! `ff fleet deploy --all` by hand (the shell-band-aid `version_reconciler`).
//! When a new commit lands on `origin/main`, this leader-gated tick invokes the
//! proven `ff fleet deploy` path — which already:
//!   * groups targets by `(os_family, arch)` = the deployment PROFILES
//!     (linux-x86_64, linux-aarch64 DGX, macos-aarch64 ace, and windows if
//!     present), so the 4 profiles fall out of the grouping for free;
//!   * BUILDS ONCE per profile on a roomy builder, then scp's the prebuilt
//!     `forgefleetd`+`ff` to same-profile peers (no rebuild-per-node);
//!   * DRAINS in-flight work + hands off leadership before restart, and restores
//!     drained targets even on error.
//!
//! This tick is the durable replacement: it doesn't re-implement that logic (400
//! lines of battle-tested drain/handoff/ship), it SCHEDULES it. Invoking the
//! `ff` binary is dogfooding native ff, not a shell band-aid — `ff fleet deploy`
//! IS the native deploy.
//!
//! SAFETY: gated on the `rolling_deploy_mode` fleet-secret, default **off**. A
//! fresh fleet-wide daemon start can't trigger a surprise convergence storm;
//! flip to "on" deliberately. Only ONE convergence runs at a time (the leader
//! gate + the last-deployed-SHA check prevent overlap).

use std::time::Duration;

use anyhow::Result;
use sqlx::PgPool;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// fleet-secret key holding the SHA of the last commit this tick deployed, so a
/// new push is detected as `origin/main HEAD != last_deployed_sha`.
const LAST_DEPLOYED_KEY: &str = "rolling_deploy_last_sha";

/// Spawn the convergence tick. Runs on every daemon, leader-gates itself.
pub fn spawn_deploy_converge_tick(
    pg: PgPool,
    check_secs: u64,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(check_secs.max(60)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !crate::leader_cache::is_current_leader() {
                        continue;
                    }
                    if let Err(err) = run_once(&pg).await {
                        warn!(error = %err, "deploy-converge tick failed");
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        info!("deploy-converge tick shutting down");
                        break;
                    }
                }
            }
        }
    })
}

/// Read the `rolling_deploy_mode` gate. Default OFF.
async fn mode_enabled(pg: &PgPool) -> bool {
    matches!(
        ff_db::pg_read_gate_value(pg, "rolling_deploy_mode", "off", "off")
            .await
            .as_deref(),
        Ok("on") | Ok("true") | Ok("1")
    )
}

/// One pass: if a new commit is on `origin/main` and we're not already at it,
/// run the native `ff fleet deploy --all` (build-once-per-profile + ship).
async fn run_once(pg: &PgPool) -> Result<()> {
    // SAFETY NET (operator 2026-07-25): a deploy that fails/crashes mid-flight
    // leaves its targets stuck `reservation_state='drained'` — and drained nodes
    // have disabled sub-agents, so the WHOLE FLEET silently stops building (0
    // merges) AND every later deploy sees "no eligible targets" and false-
    // succeeds. This happened live: 16 nodes stranded drained. Restore any node
    // drained longer than a deploy could possibly take (no deploy drains a node
    // for 30+ min), independent of the deploy gate below. Never touches taylor
    // (operator-reserved) or the leader.
    let restored = sqlx::query(
        "UPDATE computers SET reservation_state='available', reserved_reason=NULL, \
                reservation_owner=NULL, reservation_expires_at=NULL \
          WHERE reservation_state='drained' \
            AND coalesce(reserved_at, now() - interval '1 hour') < now() - interval '30 minutes' \
            AND lower(name) <> 'taylor'",
    )
    .execute(pg)
    .await
    .map(|r| r.rows_affected())
    .unwrap_or(0);
    if restored > 0 {
        warn!(restored, "deploy-converge: restored stale-drained nodes (orphaned by a failed deploy) — re-enabling their slots");
        let _ = sqlx::query(
            "UPDATE sub_agents SET status='idle' WHERE status='disabled' \
              AND computer_id IN (SELECT id FROM computers WHERE reservation_state='available')",
        )
        .execute(pg)
        .await;
    }

    if !mode_enabled(pg).await {
        return Ok(());
    }

    // HEAD of origin/main from the leader's forge-fleet checkout. `git fetch`
    // first so a just-pushed commit is seen.
    let repo = format!(
        "{}/projects/forge-fleet",
        std::env::var("HOME").unwrap_or_else(|_| "/root".into())
    );
    let _ = tokio::process::Command::new("git")
        .args(["-C", &repo, "fetch", "origin", "--quiet"])
        .output()
        .await;
    let head = match tokio::process::Command::new("git")
        .args(["-C", &repo, "rev-parse", "--short=10", "origin/main"])
        .output()
        .await
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => {
            warn!("deploy-converge: could not read origin/main HEAD");
            return Ok(());
        }
    };
    if head.is_empty() {
        return Ok(());
    }

    let last = ff_db::pg_get_secret(pg, LAST_DEPLOYED_KEY)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    if head == last {
        // Fleet already converged to this commit — nothing to do.
        return Ok(());
    }

    info!(
        head = %head, last = %last,
        "deploy-converge: new commit on origin/main — running native ff fleet deploy --all"
    );

    // Invoke the proven native deploy (build-once-per-profile + ship + drain).
    // ff resolves its own DB + node list; we just trigger it on the leader.
    let ff_bin = format!(
        "{}/.local/bin/ff",
        std::env::var("HOME").unwrap_or_else(|_| "/root".into())
    );
    let out = tokio::process::Command::new(&ff_bin)
        .args(["fleet", "deploy", "--all"])
        .current_dir(&repo)
        .output()
        .await;

    match out {
        Ok(o) if o.status.success() => {
            info!(head = %head, "deploy-converge: fleet deploy succeeded");
            // Record the deployed SHA so we don't redeploy the same commit, and
            // log a fleet_deploy_events row for the operator digest.
            let _ = ff_db::pg_set_secret(
                pg,
                LAST_DEPLOYED_KEY,
                &head,
                Some("last commit the native deploy-converge tick shipped fleet-wide"),
                Some("deploy-converge"),
            )
            .await;
            let _ = sqlx::query(
                "INSERT INTO fleet_deploy_events (commit_sha, nodes_updated, nodes_total, deployed_at) \
                 VALUES ($1, \
                   (SELECT COUNT(*) FROM computers WHERE coalesce(status,'')='online'), \
                   (SELECT COUNT(*) FROM computers), now())",
            )
            .bind(&head)
            .execute(pg)
            .await;
        }
        Ok(o) => warn!(
            head = %head, code = ?o.status.code(),
            stderr = %String::from_utf8_lossy(&o.stderr).chars().take(400).collect::<String>(),
            "deploy-converge: fleet deploy returned non-zero — will retry next tick"
        ),
        Err(e) => warn!(error = %e, "deploy-converge: failed to invoke ff fleet deploy"),
    }
    Ok(())
}
