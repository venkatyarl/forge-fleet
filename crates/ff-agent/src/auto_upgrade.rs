//! Fully automatic upgrade loop.
//!
//! ## Role
//!
//! Runs on the leader every hour. Finds every `computer_software` row with
//! `status = 'upgrade_available'`, resolves the per-OS-family/per-install-source
//! playbook for each, and enqueues one `deferred_tasks` row per target so the
//! remote worker pulls the upgrade. Flips the `computer_software.status` to
//! `'upgrading'` as soon as a task is enqueued so we don't double-dispatch.
//!
//! Payload carries a `meta.auto_upgrade` block so the worker's finalizer
//! can publish a `fleet.events.software.upgrade_completed` NATS event and
//! fire a Telegram message without the operator ever running a CLI command.
//!
//! Gated by `fleet_secrets.auto_upgrade_enabled = 'true'` — off by default.
//!
//! ## Shared with manual dispatch
//!
//! Both `ff fleet upgrade` and this tick call [`resolve_upgrade_plans`] and
//! [`enqueue_plans`]. Keeping one source of truth for playbook resolution
//! avoids drift between paths.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use sqlx::{PgPool, Row};
use tokio::sync::watch;
use tokio::task::JoinHandle;

const AUTO_UPGRADE_ENABLED_KEY: &str = "auto_upgrade_enabled";

/// One target computer + the resolved playbook command for it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradePlan {
    pub software_id: String,
    pub display_name: String,
    pub computer_name: String,
    pub os_family: String,
    pub install_source: Option<String>,
    pub installed_version: Option<String>,
    pub latest_version: Option<String>,
    pub playbook_key: String,
    pub command: String,
}

/// Result of enqueuing one plan.
#[derive(Debug, Clone)]
pub struct EnqueuedPlan {
    pub computer_name: String,
    pub defer_id: String,
    pub software_id: String,
}

/// Resolve upgrade plans for `software_id`. When `only_computer` is set
/// we filter to that single name (case-insensitive). Computers for which
/// no playbook key resolves are skipped with a warning — returned in the
/// second element of the tuple: `(plans, skipped_with_reason)`.
pub async fn resolve_upgrade_plans(
    pool: &PgPool,
    software_id: &str,
    only_computer: Option<&str>,
    upgrade_available_only: bool,
) -> Result<(Vec<UpgradePlan>, Vec<(String, String)>)> {
    // Pull the registry metadata first so we can carry display_name +
    // upgrade_playbook into each plan.
    let sw_row = sqlx::query(
        "SELECT id, display_name, upgrade_playbook, latest_version
           FROM software_registry
          WHERE id = $1",
    )
    .bind(software_id)
    .fetch_optional(pool)
    .await
    .context("select software_registry")?;

    let Some(sw_row) = sw_row else {
        anyhow::bail!("no software_registry entry for id='{software_id}'");
    };
    let display_name: String = sw_row.get("display_name");
    let playbook: JsonValue = sw_row.get("upgrade_playbook");
    let latest_version: Option<String> = sw_row.get("latest_version");

    // Pull target rows.
    let rows = if let Some(name) = only_computer {
        sqlx::query(
            "SELECT c.name                AS name,
                    c.os_family           AS os_family,
                    cs.install_source     AS install_source,
                    cs.installed_version  AS installed_version,
                    cs.status             AS status
               FROM computer_software cs
               JOIN computers c ON c.id = cs.computer_id
              WHERE cs.software_id = $1
                AND LOWER(c.name)  = LOWER($2)
              ORDER BY c.name",
        )
        .bind(software_id)
        .bind(name)
        .fetch_all(pool)
        .await
    } else if upgrade_available_only {
        sqlx::query(
            "SELECT c.name                AS name,
                    c.os_family           AS os_family,
                    cs.install_source     AS install_source,
                    cs.installed_version  AS installed_version,
                    cs.status             AS status
               FROM computer_software cs
               JOIN computers c ON c.id = cs.computer_id
              WHERE cs.software_id = $1
                AND cs.status = 'upgrade_available'
              ORDER BY c.name",
        )
        .bind(software_id)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query(
            "SELECT c.name                AS name,
                    c.os_family           AS os_family,
                    cs.install_source     AS install_source,
                    cs.installed_version  AS installed_version,
                    cs.status             AS status
               FROM computer_software cs
               JOIN computers c ON c.id = cs.computer_id
              WHERE cs.software_id = $1
              ORDER BY c.name",
        )
        .bind(software_id)
        .fetch_all(pool)
        .await
    }
    .context("select computer_software")?;

    let mut plans = Vec::with_capacity(rows.len());
    let mut skipped = Vec::new();

    for row in &rows {
        let name: String = row.get("name");
        let os_family: String = row.get("os_family");
        let install_source: Option<String> = row.get("install_source");
        let installed_version: Option<String> = row.get("installed_version");

        let candidates: Vec<String> = {
            let mut v = Vec::new();
            if let Some(src) = &install_source {
                v.push(format!("{os_family}-{src}"));
            }
            v.push(os_family.clone());
            v.push("all".to_string());
            v
        };

        let mut matched: Option<(String, String)> = None;
        for key in &candidates {
            if let Some(val) = playbook.get(key).and_then(|v| v.as_str()) {
                matched = Some((key.clone(), val.to_string()));
                break;
            }
        }

        match matched {
            Some((playbook_key, command)) => plans.push(UpgradePlan {
                software_id: software_id.to_string(),
                display_name: display_name.clone(),
                computer_name: name,
                os_family,
                install_source,
                installed_version,
                latest_version: latest_version.clone(),
                playbook_key,
                command,
            }),
            None => skipped.push((
                name,
                format!(
                    "no playbook key for os='{os_family}' source='{}' (tried {:?})",
                    install_source.as_deref().unwrap_or("-"),
                    candidates
                ),
            )),
        }
    }

    Ok((plans, skipped))
}

/// Outcome of [`gate_git_state`] — what the dirty/unpushed/pushed check
/// decided about a batch of plans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitStateGate {
    /// Proceed as normal. Either `pushed` or `unknown` (dev environment).
    Allow,
    /// Proceed but warn + emit a NATS `unpushed_propagation` event.
    AllowWithWarning,
    /// Refuse — leader's build is dirty. Caller should mark targets
    /// `upgrade_blocked_dirty` and abort.
    BlockDirty,
}

/// Look up the leader's `git_state` for a `*_git` software_id and decide
/// whether propagation is safe. Returns [`GitStateGate::Allow`] for any
/// non-`ff_git` / `forgefleetd_git` software (the gate is a no-op for
/// package-manager-managed upgrades). `force_dirty` converts `BlockDirty`
/// to `AllowWithWarning` so the operator can override after inspection.
pub async fn gate_git_state(
    pool: &PgPool,
    software_id: &str,
    force_dirty: bool,
) -> GitStateGate {
    if !matches!(software_id, "ff_git" | "forgefleetd_git") {
        return GitStateGate::Allow;
    }
    // Leader = the computer currently named in `fleet_leader_state`.
    let state = sqlx::query_scalar::<_, Option<String>>(
        "SELECT cs.metadata->>'git_state'
           FROM computer_software cs
           JOIN computers c ON c.id = cs.computer_id
           JOIN fleet_leader_state fls ON LOWER(fls.member_name) = LOWER(c.name)
          WHERE cs.software_id = $1
          LIMIT 1",
    )
    .bind(software_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .flatten();

    match state.as_deref() {
        Some("pushed") => GitStateGate::Allow,
        Some("unpushed") => GitStateGate::AllowWithWarning,
        Some("dirty") => {
            if force_dirty { GitStateGate::AllowWithWarning } else { GitStateGate::BlockDirty }
        }
        _ => GitStateGate::Allow, // unknown / missing — dev fleet, proceed with weaker guarantees
    }
}

/// Mark every target row for `software_id` as `upgrade_blocked_dirty` so
/// operators can see why propagation refused. Best-effort; errors swallowed.
pub async fn mark_targets_blocked_dirty(pool: &PgPool, software_id: &str) {
    let _ = sqlx::query(
        "UPDATE computer_software SET status = 'upgrade_blocked_dirty' WHERE software_id = $1",
    )
    .bind(software_id)
    .execute(pool)
    .await;
}

/// Enqueue the given plans as `kind='shell'` deferred tasks.
///
/// Each payload carries:
///   - `command`       → the playbook command the worker runs
///   - `meta.auto_upgrade` → `{software_id, display_name, computer, old_version, latest_version, playbook_key}`
///
/// After enqueuing, the matching `computer_software` row is flipped to
/// `status='upgrading'` so subsequent auto-upgrade ticks don't double-fire.
pub async fn enqueue_plans(
    pool: &PgPool,
    plans: &[UpgradePlan],
    who: &str,
) -> Result<Vec<EnqueuedPlan>> {
    let mut out = Vec::with_capacity(plans.len());
    for p in plans {
        let payload = json!({
            "command": p.command,
            "meta": {
                "auto_upgrade": {
                    "software_id":    p.software_id,
                    "display_name":   p.display_name,
                    "computer":       p.computer_name,
                    "old_version":    p.installed_version,
                    "latest_version": p.latest_version,
                    "playbook_key":   p.playbook_key,
                    "source":         who,
                }
            }
        });
        let trigger_spec = json!({ "node": p.computer_name });
        let title = format!("Upgrade {} on {}", p.software_id, p.computer_name);
        let id = ff_db::pg_enqueue_deferred(
            pool,
            &title,
            "shell",
            &payload,
            "node_online",
            &trigger_spec,
            Some(&p.computer_name),
            &json!([]),
            Some(who),
            Some(3),
        )
        .await
        .context("enqueue deferred task")?;

        // Flip status so repeat ticks don't double-dispatch.
        let _ = sqlx::query(
            "UPDATE computer_software cs
                SET status = 'upgrading'
               FROM computers c
              WHERE cs.computer_id = c.id
                AND cs.software_id = $1
                AND LOWER(c.name)  = LOWER($2)",
        )
        .bind(&p.software_id)
        .bind(&p.computer_name)
        .execute(pool)
        .await;

        out.push(EnqueuedPlan {
            computer_name: p.computer_name.clone(),
            defer_id: id,
            software_id: p.software_id.clone(),
        });
    }
    Ok(out)
}

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

/// Is the auto-upgrade feature turned on via `fleet_secrets`?
async fn is_enabled(pool: &PgPool) -> bool {
    match ff_db::pg_get_secret(pool, AUTO_UPGRADE_ENABLED_KEY).await {
        Ok(Some(v)) => matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "on"),
        _ => false,
    }
}

/// Pick every software_id that has at least one computer with
/// `status='upgrade_available'`.
async fn software_ids_with_drift(pool: &PgPool) -> Result<Vec<String>> {
    let rows = sqlx::query(
        "SELECT DISTINCT software_id
           FROM computer_software
          WHERE status = 'upgrade_available'
          ORDER BY software_id",
    )
    .fetch_all(pool)
    .await
    .context("select drifted software")?;
    Ok(rows.into_iter().map(|r| r.get("software_id")).collect())
}

/// Background auto-upgrade tick.
///
/// Runs on every daemon — but skips the work unless it's the current
/// leader AND the `auto_upgrade_enabled` secret is truthy. Safe to run
/// everywhere; only the leader actually enqueues.
pub struct AutoUpgradeTick {
    pool: PgPool,
    my_name: String,
}

impl AutoUpgradeTick {
    pub fn new(pool: PgPool, my_name: String) -> Self {
        Self { pool, my_name }
    }

    /// One tick: gate, find drift, enqueue.
    pub async fn run_once(&self) -> Result<usize> {
        if !is_leader(&self.pool, &self.my_name).await {
            return Ok(0);
        }
        if !is_enabled(&self.pool).await {
            tracing::debug!("auto-upgrade disabled (fleet_secrets.auto_upgrade_enabled not truthy)");
            return Ok(0);
        }

        let ids = software_ids_with_drift(&self.pool).await?;
        if ids.is_empty() {
            return Ok(0);
        }

        let who = format!("auto-upgrade@{}", self.my_name);
        let mut total = 0usize;
        for software_id in &ids {
            let (plans, skipped) =
                match resolve_upgrade_plans(&self.pool, software_id, None, true).await {
                    Ok(x) => x,
                    Err(e) => {
                        tracing::warn!(
                            software_id = %software_id,
                            error = %e,
                            "resolve_upgrade_plans failed"
                        );
                        continue;
                    }
                };
            for (name, reason) in &skipped {
                tracing::warn!(
                    software_id = %software_id,
                    computer = %name,
                    reason = %reason,
                    "auto-upgrade skipped computer"
                );
            }
            if plans.is_empty() {
                continue;
            }
            // ── Dirty-build safety gate ────────────────────────────
            // Never force-dirty from the automatic path — the operator
            // must explicitly opt in via `ff fleet upgrade --force-dirty`.
            let leader_sha = plans
                .first()
                .and_then(|p| p.installed_version.clone())
                .unwrap_or_else(|| "(unknown)".into());
            let gate = gate_git_state(&self.pool, software_id, false).await;
            match gate {
                GitStateGate::BlockDirty => {
                    tracing::warn!(
                        software_id = %software_id,
                        sha = %leader_sha,
                        "refusing to propagate dirty build {leader_sha} — commit or pass --force-dirty"
                    );
                    mark_targets_blocked_dirty(&self.pool, software_id).await;
                    continue;
                }
                GitStateGate::AllowWithWarning => {
                    tracing::warn!(
                        software_id = %software_id,
                        sha = %leader_sha,
                        computers = plans.len(),
                        "propagating unpushed commit {leader_sha} from leader to fleet — push to origin/main when ready"
                    );
                    let payload = json!({
                        "software_id": software_id,
                        "sha": leader_sha,
                        "computer_count": plans.len(),
                        "source": who,
                        "ts": chrono::Utc::now().to_rfc3339(),
                    });
                    crate::nats_client::publish_json(
                        "fleet.events.software.unpushed_propagation".to_string(),
                        &payload,
                    )
                    .await;
                }
                GitStateGate::Allow => {}
            }

            match enqueue_plans(&self.pool, &plans, &who).await {
                Ok(enqueued) => {
                    tracing::info!(
                        software_id = %software_id,
                        dispatched = enqueued.len(),
                        "auto-upgrade dispatched"
                    );
                    // Publish a start event — finalizer publishes completion.
                    for plan in &plans {
                        let payload = json!({
                            "software_id": plan.software_id,
                            "display_name": plan.display_name,
                            "computer": plan.computer_name,
                            "old_version": plan.installed_version,
                            "latest_version": plan.latest_version,
                            "playbook_key": plan.playbook_key,
                            "ts": chrono::Utc::now().to_rfc3339(),
                        });
                        crate::nats_client::publish_json(
                            format!("fleet.events.software.upgrade_started.{}", plan.computer_name),
                            &payload,
                        )
                        .await;
                    }
                    total += enqueued.len();
                }
                Err(e) => {
                    tracing::warn!(
                        software_id = %software_id,
                        error = %e,
                        "enqueue_plans failed"
                    );
                }
            }
        }
        Ok(total)
    }

    /// Spawn the hourly tick. First tick fires ~90s after spawn so the
    /// daemon's other subsystems come up first.
    pub fn spawn(self, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let kickoff = Duration::from_secs(90);
            let interval = Duration::from_secs(3600);

            tokio::select! {
                _ = tokio::time::sleep(kickoff) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return; }
                }
            }

            loop {
                match self.run_once().await {
                    Ok(n) if n > 0 => tracing::info!(dispatched = n, "auto-upgrade tick"),
                    Ok(_) => tracing::debug!("auto-upgrade tick: nothing to do"),
                    Err(e) => tracing::warn!(error = %e, "auto-upgrade tick failed"),
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
