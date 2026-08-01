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
const RESTORE_OWNERLESS_DEPLOY_DRAINS_SQL: &str = "
    WITH restored AS (
        UPDATE computers
           SET reservation_state = 'available',
               reserved_reason = NULL,
               reservation_expires_at = NULL
         WHERE reservation_state = 'drained'
           AND reservation_owner IS NULL
           AND lower(name) <> 'vinny'
           AND (reserved_reason = 'fleet-deploy' OR reserved_reason IS NULL)
        RETURNING id
    )
    UPDATE sub_agents
       SET status = 'idle'
     WHERE status = 'disabled'
       AND computer_id IN (SELECT id FROM restored)";

async fn restore_ownerless_deploy_drains(pg: &PgPool) -> u64 {
    sqlx::query(RESTORE_OWNERLESS_DEPLOY_DRAINS_SQL)
        .execute(pg)
        .await
        .map(|result| result.rows_affected())
        .unwrap_or(0)
}

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
    // for 30+ min), independent of the deploy gate below. Never touches vinny
    // (operator-reserved) or the leader.
    let restored = sqlx::query(
        "WITH restored AS (
             UPDATE computers
                SET reservation_state='available', reserved_reason=NULL,
                    reservation_expires_at=NULL
              WHERE reservation_state='drained'
                AND reservation_owner IS NULL
                AND lower(name) <> 'vinny'
                AND (reserved_reason = 'fleet-deploy' OR reserved_reason IS NULL)
                AND coalesce(reserved_at, now() - interval '1 hour')
                    < now() - interval '30 minutes'
             RETURNING id
         )
         UPDATE sub_agents SET status='idle'
          WHERE status='disabled'
            AND computer_id IN (SELECT id FROM restored)",
    )
    .execute(pg)
    .await
    .map(|r| r.rows_affected())
    .unwrap_or(0);
    if restored > 0 {
        warn!(
            restored,
            "deploy-converge: restored stale-drained nodes (orphaned by a failed deploy) — re-enabling their slots"
        );
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

    // The child normally restores its scoped drain state, but it can be killed
    // or exit through an unforeseen error path. Recover its ownerless deploy
    // drains immediately after every child exit; do not wait for the stale
    // 30-minute crash sweep. The query cannot touch Vinny or operator-owned
    // reservations.
    if out.is_ok() {
        let restored_slots = restore_ownerless_deploy_drains(pg).await;
        if restored_slots > 0 {
            warn!(
                restored_slots,
                "deploy-converge: immediately restored ownerless deploy drains after child exit"
            );
        }
    }

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

            // AUTO-REQUEUE ON DEPLOY (operator 2026-07-26: "no human requeuing
            // should ever be needed"). A new commit shipped fleet-wide may fix
            // the exact failure class that killed items (e.g. today's
            // review-fallback fixed the "in-place review unavailable" backlog).
            // So on every successful deploy, give FIXABLE-class failures a FRESH
            // retry (attempts=0 → ready) so they build against the new code — no
            // manual requeue. Terminal classes are excluded: `review rejected`
            // (the diff is genuinely wrong) and `max-build-duration` (too big —
            // needs decomposition, not retry). Self-limiting: if the new code
            // doesn't fix it, it fails again and re-exhausts, and it only fires
            // when the SHA actually changed (once per deploy).
            let requeued = sqlx::query(
                "UPDATE work_items SET status='ready', attempts=0, \
                        last_error='auto-requeued after deploy of new code' \
                  WHERE status='failed' \
                    AND coalesce(last_error,'') NOT ILIKE '%review rejected%' \
                    AND coalesce(last_error,'') NOT ILIKE '%max-build-duration%'",
            )
            .execute(pg)
            .await
            .map(|r| r.rows_affected())
            .unwrap_or(0);
            if requeued > 0 {
                info!(requeued, head = %head, "deploy-converge: auto-requeued fixable failures for the new code");
            }
        }
        Ok(o) => {
            // `ff fleet deploy --all` exits NON-ZERO if ANY node fails — including
            // a node that is simply powered off / unreachable (shakira, vinny).
            // The old code then never advanced LAST_DEPLOYED_KEY, so every 5-min
            // tick saw `head != last`, redeployed, and RESTART-DRAINED every
            // in-flight build fleet-wide — a permanent completion outage triggered
            // by a single offline box (observed 2026-07-28: ~10 restart waves,
            // 0 completions). Fix: if the ONLY failures are offline/unreachable
            // nodes, the reachable fleet DID converge — record the SHA so we stop
            // looping. Only a genuine ONLINE-node failure retries next tick.
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            let failed_nodes: std::collections::HashSet<String> = combined
                .lines()
                .filter(|l| l.contains('✗'))
                .filter_map(|l| {
                    l.split_whitespace()
                        .find(|t| !t.contains('✗'))
                        .map(|s| s.to_string())
                })
                .collect();
            let online: std::collections::HashSet<String> = sqlx::query_scalar::<_, String>(
                "SELECT name FROM computers WHERE coalesce(status,'') = 'online'",
            )
            .fetch_all(pg)
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();
            let real_failures: Vec<&String> = failed_nodes
                .iter()
                .filter(|n| online.contains(*n))
                .collect();
            if real_failures.is_empty() {
                info!(
                    head = %head,
                    offline_skipped = ?failed_nodes,
                    "deploy-converge: reachable fleet converged (only offline nodes failed) — recording SHA to stop the redeploy/restart loop"
                );
                let _ = ff_db::pg_set_secret(
                    pg,
                    LAST_DEPLOYED_KEY,
                    &head,
                    Some("deploy-converge: converged across all ONLINE nodes (offline nodes skipped)"),
                    Some("deploy-converge"),
                )
                .await;
            } else {
                warn!(
                    head = %head, code = ?o.status.code(),
                    online_failures = ?real_failures,
                    stderr = %String::from_utf8_lossy(&o.stderr).chars().take(400).collect::<String>(),
                    "deploy-converge: ONLINE node(s) failed deploy — will retry next tick"
                );
            }
        }
        Err(e) => warn!(error = %e, "deploy-converge: failed to invoke ff fleet deploy"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::RESTORE_OWNERLESS_DEPLOY_DRAINS_SQL;

    #[test]
    fn immediate_recovery_preserves_operator_reservations_and_vinny() {
        assert!(RESTORE_OWNERLESS_DEPLOY_DRAINS_SQL.contains("reservation_owner IS NULL"));
        assert!(RESTORE_OWNERLESS_DEPLOY_DRAINS_SQL.contains("lower(name) <> 'vinny'"));
        assert!(RESTORE_OWNERLESS_DEPLOY_DRAINS_SQL.contains("reserved_reason = 'fleet-deploy'"));
        assert!(!RESTORE_OWNERLESS_DEPLOY_DRAINS_SQL.contains("reservation_owner = NULL"));
    }
}
