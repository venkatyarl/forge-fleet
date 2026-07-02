//! Active disk management — the leader-gated disk-reconcile tick (V118).
//!
//! Turns the PASSIVE over-quota alert (`disk_sampler::maybe_alert_over_quota`,
//! which only files a `manual` deferred task) into ACTUATION: every pass it
//! reads `fleet_disk_usage`, finds over-quota nodes, builds a MOVE-vs-DELETE
//! classified plan per node (`smart_lru::plan_classified_eviction`), and — only
//! when an operator has opted in — frees space by deleting wrong-runtime/retired/
//! peer-backed copies and relocating the last copy of still-wanted models.
//!
//! ## SAFETY — three-mode gate (`fleet_secrets.disk_policy_mode`)
//! Read EVERY tick, EXACTLY like the autoscaler reads `autoscaler_mode`:
//!   - `off`     (DEFAULT, and the value when the key is missing): the tick does
//!               NOTHING. Deploying this is harmless — it cannot delete or move a
//!               single byte until an operator sets the key.
//!   - `dry-run`: compute + LOG the full classified plan, actuate NOTHING.
//!   - `active`:  actuate (delete locally/cross-node; move via transfer then
//!               source-delete-after-verify).
//!
//! Mirrors `autoscaler::spawn_autoscaler_tick` for the leader gate (the disk
//! policy is global state; only the leader plans/actuates so there are no N-way
//! delete races). On failover the new leader's forgefleetd picks the tick up.
//!
//! Conservative by construction: at most [`MAX_NODES_PER_PASS`] nodes and
//! [`MAX_ACTIONS_PER_PASS`] actions per pass, and it NEVER touches a pinned row,
//! an in-use row, a row younger than `min_cold_days`, or the only copy of a
//! still-wanted model unless a verified MOVE target exists.

use sqlx::PgPool;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::smart_lru::{ClassifiedCandidate, ClassifiedPlan, DiskAction, LruPolicy};

/// `fleet_secrets` key holding the three-mode gate. Off / missing = no-op.
const DISK_POLICY_MODE_KEY: &str = "disk_policy_mode";

/// Cap on nodes actuated per pass — one stale disk sample can't cascade deletes
/// across the whole fleet in a single tick.
const MAX_NODES_PER_PASS: usize = 2;
/// Cap on total delete+move actions actuated per pass (fleet-wide).
const MAX_ACTIONS_PER_PASS: usize = 4;

/// The gate's three modes (same shape as `autoscaler::AutoscalerMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskPolicyMode {
    Off,
    DryRun,
    Active,
}

impl DiskPolicyMode {
    fn parse(raw: Option<&str>) -> Self {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("active") => DiskPolicyMode::Active,
            Some("dry-run") | Some("dry_run") | Some("dryrun") => DiskPolicyMode::DryRun,
            // Off, missing, empty, or any unrecognised value → safe default.
            _ => DiskPolicyMode::Off,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            DiskPolicyMode::Off => "off",
            DiskPolicyMode::DryRun => "dry-run",
            DiskPolicyMode::Active => "active",
        }
    }
}

/// Read the gate from `fleet_secrets`. DEFAULTS TO OFF when missing/unparseable —
/// shipping this subsystem is harmless until an operator opts in.
async fn read_mode(pg: &PgPool) -> DiskPolicyMode {
    match ff_db::pg_read_gate_value(pg, DISK_POLICY_MODE_KEY, "off", "off").await {
        Ok(v) => DiskPolicyMode::parse(Some(v.as_str())),
        Err(e) => {
            warn!(error = %e, "disk-reconcile: failed to read mode secret; treating as off");
            DiskPolicyMode::Off
        }
    }
}

/// Per-pass summary for the info! log line + the `disk_policy_runs` row.
#[derive(Debug, Default, Clone)]
pub struct DiskReconcileSummary {
    pub mode: &'static str,
    pub nodes_over_quota: usize,
    pub planned_deletes: usize,
    pub planned_moves: usize,
    pub planned_skips: usize,
    pub actuated_deletes: usize,
    pub actuated_moves: usize,
    pub bytes_planned: u64,
    pub bytes_freed: u64,
}

/// The set of nodes currently over quota, newest-sample-first deterministic
/// ordering (by worker_name so output is stable). Uses the SAME quota math as
/// `disk_sampler`: used% of total > the node's `disk_quota_pct`.
///
/// `pub(crate)` so the backup orchestrator's over-quota backup-replica reaper
/// ([`crate::ha::backup::BackupOrchestrator::reap_over_quota_peers`]) shares the
/// exact same "is this node over quota?" definition — disk pressure has one
/// source of truth.
pub(crate) async fn over_quota_nodes(pg: &PgPool) -> Result<Vec<String>, String> {
    let usage = ff_db::pg_latest_disk_usage(pg)
        .await
        .map_err(|e| format!("pg_latest_disk_usage: {e}"))?;
    let mut over: Vec<String> = Vec::new();
    for (name, _dir, total, used, _free, _models, _ts) in &usage {
        let node = match ff_db::pg_get_node(pg, name)
            .await
            .map_err(|e| format!("pg_get_node({name}): {e}"))?
        {
            Some(n) => n,
            None => continue,
        };
        let quota = node.disk_quota_pct.max(1) as i64;
        if *total > 0 {
            let used_pct = used.saturating_mul(100) / total;
            if used_pct > quota {
                over.push(name.clone());
            }
        }
    }
    over.sort();
    Ok(over)
}

/// Compute the classified plan for every over-quota node (capped). Pure — no
/// actuation. Returns `(plans, summary-skeleton)`.
async fn plan_pass(pg: &PgPool) -> Result<(Vec<ClassifiedPlan>, DiskReconcileSummary), String> {
    let over = over_quota_nodes(pg).await?;
    let mut summary = DiskReconcileSummary {
        nodes_over_quota: over.len(),
        ..Default::default()
    };

    let policy = LruPolicy::default();
    let mut plans: Vec<ClassifiedPlan> = Vec::new();

    for node in over.iter().take(MAX_NODES_PER_PASS) {
        let plan = crate::smart_lru::plan_classified_eviction(pg, node, &policy).await?;
        for c in &plan.candidates {
            match c.action {
                DiskAction::Delete => {
                    summary.planned_deletes += 1;
                    summary.bytes_planned = summary.bytes_planned.saturating_add(c.size_bytes);
                }
                DiskAction::Move => {
                    summary.planned_moves += 1;
                    summary.bytes_planned = summary.bytes_planned.saturating_add(c.size_bytes);
                }
                DiskAction::Skip => summary.planned_skips += 1,
            }
        }
        plans.push(plan);
    }

    Ok((plans, summary))
}

/// Build the JSONB detail array recorded in `disk_policy_runs`.
fn plans_to_detail(plans: &[ClassifiedPlan]) -> serde_json::Value {
    let mut arr: Vec<serde_json::Value> = Vec::new();
    for p in plans {
        for c in &p.candidates {
            arr.push(serde_json::json!({
                "node": c.worker_name,
                "library_id": c.library_id,
                "catalog_id": c.catalog_id,
                "runtime": c.runtime,
                "size_bytes": c.size_bytes,
                "action": c.action.as_str(),
                "target_node": c.target_node,
                "reasons": c.reasons,
            }));
        }
    }
    serde_json::Value::Array(arr)
}

/// Actuate one DELETE. Local rows go through `pg_delete_library` (the DB row) +
/// a best-effort filesystem unlink when the row is on THIS node; cross-node rows
/// dispatch `ff model delete <id> --yes` via the defer queue (mirrors the
/// autoscaler's cross-node pattern). Returns bytes freed on success.
async fn actuate_delete(pg: &PgPool, c: &ClassifiedCandidate) -> Result<u64, String> {
    let this = crate::fleet_info::resolve_this_worker_name().await;
    if c.worker_name.eq_ignore_ascii_case(&this) {
        // Best-effort unlink of the file/dir, then drop the DB row.
        let path = std::path::Path::new(&c.file_path);
        if path.is_dir() {
            let _ = std::fs::remove_dir_all(path);
        } else if path.exists() {
            let _ = std::fs::remove_file(path);
        }
        ff_db::pg_delete_library(pg, &c.library_id)
            .await
            .map_err(|e| format!("pg_delete_library({}): {e}", c.library_id))?;
        Ok(c.size_bytes)
    } else {
        let command = format!(
            "~/.local/bin/ff model delete {} --yes",
            shell_quote(&c.library_id)
        );
        let payload = serde_json::json!({ "command": command });
        let title = format!(
            "disk-reconcile: delete {} on {}",
            c.catalog_id, c.worker_name
        );
        ff_db::pg_enqueue_deferred(
            pg,
            &title,
            "shell",
            &payload,
            "now",
            &serde_json::json!({}),
            Some(&c.worker_name),
            &serde_json::json!([]),
            Some("disk-reconcile"),
            Some(3),
        )
        .await
        .map_err(|e| format!("pg_enqueue_deferred(delete on {}): {e}", c.worker_name))?;
        Ok(c.size_bytes)
    }
}

/// Actuate one MOVE: transfer the model to the target, then delete the SOURCE
/// only after the transfer succeeds (transfer_model verifies size on the target).
/// Records the relocation in `disk_move_log`. Returns bytes freed on the source.
async fn actuate_move(pg: &PgPool, c: &ClassifiedCandidate) -> Result<u64, String> {
    let target = c
        .target_node
        .as_ref()
        .ok_or_else(|| "move candidate has no target_node".to_string())?;

    let move_id = ff_db::pg_open_disk_move(
        pg,
        &c.worker_name,
        target,
        &c.catalog_id,
        &c.runtime,
        &c.library_id,
        c.size_bytes as i64,
    )
    .await
    .map_err(|e| format!("pg_open_disk_move: {e}"))?;

    // Transfer (rsync-over-SSH with the V118 target free-disk pre-check).
    let res = crate::model_transfer::transfer_model(
        pg,
        crate::model_transfer::TransferOptions {
            source_node: c.worker_name.clone(),
            target_node: target.clone(),
            library_id: c.library_id.clone(),
        },
    )
    .await;

    match res {
        Ok(tr) => {
            // Transfer + size-verify succeeded.
            let _ = ff_db::pg_update_disk_move(
                pg,
                move_id,
                "verified",
                Some(&tr.target_library_id),
                None,
            )
            .await;
            // Now it's safe to delete the source copy.
            actuate_delete(pg, c).await?;
            let _ = ff_db::pg_update_disk_move(pg, move_id, "source_deleted", None, None).await;
            Ok(c.size_bytes)
        }
        Err(e) => {
            let _ = ff_db::pg_update_disk_move(pg, move_id, "failed", None, Some(&e)).await;
            Err(format!("transfer {} → {target} failed: {e}", c.worker_name))
        }
    }
}

/// One disk-reconcile pass. Reads the gate; off = no-op. Plans, logs the plan,
/// and (only in `active`) actuates. Records a `disk_policy_runs` row for every
/// non-off pass. Returns the pass summary.
pub async fn disk_reconcile_pass(pg: &PgPool) -> Result<DiskReconcileSummary, String> {
    let mode = read_mode(pg).await;
    if mode == DiskPolicyMode::Off {
        debug!("disk-reconcile: mode=off (no-op)");
        return Ok(DiskReconcileSummary {
            mode: "off",
            ..Default::default()
        });
    }

    let (plans, mut summary) = plan_pass(pg).await?;
    summary.mode = mode.as_str();

    // Always log the per-candidate plan (dry-run AND active).
    for p in &plans {
        for c in &p.candidates {
            info!(
                node = %c.worker_name,
                catalog = %c.catalog_id,
                runtime = %c.runtime,
                size_gb = format!("{:.1}", c.size_bytes as f64 / (1u64 << 30) as f64),
                action = c.action.as_str(),
                target = c.target_node.as_deref().unwrap_or("-"),
                mode = mode.as_str(),
                "disk-reconcile PLAN: {} {} on {} ({})",
                c.action.as_str(), c.catalog_id, c.worker_name,
                c.reasons.join("; ")
            );
        }
    }

    // dry-run: record the planned run, actuate nothing.
    if mode == DiskPolicyMode::DryRun {
        record_run(pg, &summary, &plans).await;
        return Ok(summary);
    }

    // active: actuate, capped.
    let mut actions_done = 0usize;
    'outer: for p in &plans {
        for c in &p.candidates {
            if actions_done >= MAX_ACTIONS_PER_PASS {
                break 'outer;
            }
            match c.action {
                DiskAction::Delete => match actuate_delete(pg, c).await {
                    Ok(freed) => {
                        summary.actuated_deletes += 1;
                        summary.bytes_freed = summary.bytes_freed.saturating_add(freed);
                        actions_done += 1;
                    }
                    Err(e) => {
                        warn!(error = %e, node = %c.worker_name, "disk-reconcile: delete failed")
                    }
                },
                DiskAction::Move => match actuate_move(pg, c).await {
                    Ok(freed) => {
                        summary.actuated_moves += 1;
                        summary.bytes_freed = summary.bytes_freed.saturating_add(freed);
                        actions_done += 1;
                    }
                    Err(e) => {
                        warn!(error = %e, node = %c.worker_name, "disk-reconcile: move failed")
                    }
                },
                DiskAction::Skip => { /* nothing eligible — surfaced in the log/run detail */ }
            }
        }
    }

    record_run(pg, &summary, &plans).await;
    Ok(summary)
}

/// Persist one `disk_policy_runs` row (best-effort — a logging failure must not
/// abort the pass).
async fn record_run(pg: &PgPool, s: &DiskReconcileSummary, plans: &[ClassifiedPlan]) {
    let detail = plans_to_detail(plans);
    if let Err(e) = ff_db::pg_insert_disk_policy_run(
        pg,
        s.mode,
        s.nodes_over_quota as i32,
        s.planned_deletes as i32,
        s.planned_moves as i32,
        s.actuated_deletes as i32,
        s.actuated_moves as i32,
        s.bytes_planned as i64,
        s.bytes_freed as i64,
        &detail,
    )
    .await
    {
        warn!(error = %e, "disk-reconcile: failed to record disk_policy_runs row");
    }
}

/// Minimal single-quote shell escaping for the defer-shell command we build.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Spawn the leader-gated disk-reconcile loop. The leader gate is read from the
/// process-local leader cache; disk policy is global state, so only the leader
/// plans/actuates (no N-way delete races).
pub fn spawn_disk_reconcile_tick(
    pg: PgPool,
    _worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        // Skip the immediate fire so pulse/election settle first.
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !crate::leader_cache::is_current_leader() {
                        continue;
                    }

                    match disk_reconcile_pass(&pg).await {
                        Ok(s) => {
                            info!(
                                mode = s.mode,
                                nodes_over_quota = s.nodes_over_quota,
                                planned_deletes = s.planned_deletes,
                                planned_moves = s.planned_moves,
                                planned_skips = s.planned_skips,
                                actuated_deletes = s.actuated_deletes,
                                actuated_moves = s.actuated_moves,
                                bytes_freed = s.bytes_freed,
                                "disk-reconcile pass"
                            );
                        }
                        Err(e) => warn!(error = %e, "disk-reconcile tick failed"),
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        info!("disk-reconcile tick loop stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parsing_defaults_off() {
        assert_eq!(DiskPolicyMode::parse(None), DiskPolicyMode::Off);
        assert_eq!(DiskPolicyMode::parse(Some("")), DiskPolicyMode::Off);
        assert_eq!(DiskPolicyMode::parse(Some("garbage")), DiskPolicyMode::Off);
        assert_eq!(DiskPolicyMode::parse(Some("off")), DiskPolicyMode::Off);
        assert_eq!(
            DiskPolicyMode::parse(Some("dry-run")),
            DiskPolicyMode::DryRun
        );
        assert_eq!(
            DiskPolicyMode::parse(Some("DRY_RUN")),
            DiskPolicyMode::DryRun
        );
        assert_eq!(
            DiskPolicyMode::parse(Some(" active ")),
            DiskPolicyMode::Active
        );
    }

    #[test]
    fn mode_as_str_roundtrip() {
        assert_eq!(DiskPolicyMode::Off.as_str(), "off");
        assert_eq!(DiskPolicyMode::DryRun.as_str(), "dry-run");
        assert_eq!(DiskPolicyMode::Active.as_str(), "active");
    }
}
