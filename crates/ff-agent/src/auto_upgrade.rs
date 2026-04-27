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
                    c.source_tree_path    AS source_tree_path,
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
                    c.source_tree_path    AS source_tree_path,
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
                    c.source_tree_path    AS source_tree_path,
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
        let source_tree_path: Option<String> = row.get("source_tree_path");
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
            Some((playbook_key, command)) => {
                // Substitute {{source_tree_path}} per target. Tilde expansion
                // does not happen inside double-quoted shell strings, so
                // convert leading `~/` → `$HOME/` here. The playbook can then
                // safely use `cd "{{source_tree_path}}"` on every platform.
                let raw_path = source_tree_path
                    .as_deref()
                    .unwrap_or("~/projects/forge-fleet");
                let expanded_path = if let Some(rest) = raw_path.strip_prefix("~/") {
                    format!("$HOME/{rest}")
                } else {
                    raw_path.to_string()
                };
                let command = command.replace("{{source_tree_path}}", &expanded_path);
                plans.push(UpgradePlan {
                    software_id: software_id.to_string(),
                    display_name: display_name.clone(),
                    computer_name: name,
                    os_family,
                    install_source,
                    installed_version,
                    latest_version: latest_version.clone(),
                    playbook_key,
                    command,
                })
            }
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
pub async fn gate_git_state(pool: &PgPool, software_id: &str, force_dirty: bool) -> GitStateGate {
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
            if force_dirty {
                GitStateGate::AllowWithWarning
            } else {
                GitStateGate::BlockDirty
            }
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
    match sqlx::query_scalar::<_, String>("SELECT member_name FROM fleet_leader_state LIMIT 1")
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
    ///
    /// Pass `force = true` to bypass the `auto_upgrade_enabled` secret gate.
    /// The leader check is never bypassed — run this on the leader.
    pub async fn run_once(&self, force: bool) -> Result<usize> {
        if !is_leader(&self.pool, &self.my_name).await {
            return Ok(0);
        }
        if !force && !is_enabled(&self.pool).await {
            tracing::debug!(
                "auto-upgrade disabled (fleet_secrets.auto_upgrade_enabled not truthy)"
            );
            return Ok(0);
        }

        // Self-built tools (ff_git, forgefleetd_git, etc.) use method=self_built
        // which means "leader's installed version IS canonical." The 6h
        // software_upstream tick eventually refreshes software_registry.latest_version,
        // but that's too slow for active dev. Do an inline refresh here on every
        // auto-upgrade tick — one SQL UPDATE per row. If leader's row just flipped,
        // the next line (drift check) will see upgrade_available immediately.
        let _ = refresh_self_built_latest_versions(&self.pool).await;
        // npm-distributed tools (openclaw, codex, context-mode, …): query
        // registry.npmjs.org/<pkg>/latest. Same-tick refresh for parity with
        // self_built — without this, npm releases sit unnoticed indefinitely.
        let _ = refresh_npm_registry_latest_versions(&self.pool).await;
        // PyPI-distributed (vllm, mlx_lm, …) and GitHub-released (gh, etc.)
        // follow the same shape, different upstream URL.
        let _ = refresh_pypi_latest_versions(&self.pool).await;
        let _ = refresh_github_release_latest_versions(&self.pool).await;
        // Then: flip computer_software.status = 'upgrade_available' for any row
        // where installed_version != latest_version and status is currently 'ok'.
        // Generic across all methods.
        let _ = flip_drift_status(&self.pool).await;

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
                            format!(
                                "fleet.events.software.upgrade_started.{}",
                                plan.computer_name
                            ),
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
                match self.run_once(false).await {
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

/// For every `software_registry` row with `version_source.method='self_built'`,
/// set `latest_version` to the canonical git ref's HEAD SHA in the leader's
/// source tree (default ref: `origin/main`). Runs inline on every
/// auto-upgrade tick so same-day drift gets caught within the tick interval,
/// not the 6h upstream interval.
///
/// Reads the ref from `version_source.git_ref` (default `origin/main`) and
/// the path from `computers.source_tree_path` for the leader. Shells out to
/// `git -C <path> rev-parse <ref>` — a single fast read.
///
/// **Why this is correct.** Treating the leader's currently-installed binary
/// as upstream truth was a chicken-and-egg: the leader could never detect
/// drift against itself, so the leader could never auto-upgrade itself, and
/// since `latest_version` flows from the leader, no other member could
/// either. Reading from git decouples the source of truth from any node's
/// current binary; drift now fires on the leader too.
async fn refresh_self_built_latest_versions(pool: &PgPool) -> Result<u64> {
    // 1. Resolve the leader's source_tree_path. If we can't (no leader
    //    elected, no source_tree set), bail with 0 affected rather than
    //    erroring the whole upgrade tick.
    let source_tree: Option<String> = sqlx::query_scalar(
        r#"
        SELECT c.source_tree_path
          FROM fleet_leader_state ls
          JOIN computers c ON c.id = ls.computer_id
         WHERE c.source_tree_path IS NOT NULL AND c.source_tree_path <> ''
         LIMIT 1
        "#,
    )
    .fetch_optional(pool)
    .await
    .context("read leader source_tree_path")?;

    let Some(source_tree) = source_tree else {
        tracing::debug!("refresh_self_built: no leader source_tree_path; skipping");
        return Ok(0);
    };

    // 2. For each self_built row, run `git -C <source_tree> rev-parse <ref>`
    //    and write the SHA back. ref defaults to origin/main.
    let rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "SELECT id, version_source FROM software_registry \
         WHERE version_source->>'method' = 'self_built'",
    )
    .fetch_all(pool)
    .await
    .context("list self_built software")?;

    let expanded_path = expand_tilde(&source_tree);
    let mut updated: u64 = 0;
    for (sw_id, vs) in rows {
        let git_ref = vs
            .get("git_ref")
            .and_then(|v| v.as_str())
            .unwrap_or("origin/main")
            .to_string();
        let path = expanded_path.clone();
        let sha = match tokio::task::spawn_blocking(move || {
            std::process::Command::new("git")
                .args(["-C", &path, "rev-parse", &git_ref])
                .output()
        })
        .await
        {
            Ok(Ok(out)) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            }
            Ok(Ok(out)) => {
                tracing::warn!(
                    sw = %sw_id,
                    stderr = %String::from_utf8_lossy(&out.stderr),
                    "refresh_self_built: git rev-parse failed"
                );
                continue;
            }
            Ok(Err(e)) => {
                tracing::warn!(sw = %sw_id, error = %e, "refresh_self_built: git spawn failed");
                continue;
            }
            Err(e) => {
                tracing::warn!(sw = %sw_id, error = %e, "refresh_self_built: join error");
                continue;
            }
        };
        if sha.is_empty() {
            continue;
        }
        let res = sqlx::query(
            r#"
            UPDATE software_registry
               SET latest_version    = $1,
                   latest_version_at = NOW()
             WHERE id = $2
               AND (latest_version IS NULL OR latest_version <> $1)
            "#,
        )
        .bind(&sha)
        .bind(&sw_id)
        .execute(pool)
        .await
        .with_context(|| format!("update latest_version for {sw_id}"))?;
        if res.rows_affected() > 0 {
            tracing::info!(sw = %sw_id, sha = %sha, "refresh_self_built: latest_version advanced");
            updated += 1;
        }
    }
    Ok(updated)
}

/// Expand a leading `~` in a path string. The DB stores paths like
/// `~/projects/forge-fleet`; child commands inherit the daemon's `$HOME`.
fn expand_tilde(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    s.to_string()
}

/// For every computer_software row where installed_version differs from
/// software_registry.latest_version AND status is currently 'ok', flip to
/// 'upgrade_available' so the drift query picks it up. Runs after the
/// per-method refresh fns so `latest_version` is current. Method-agnostic —
/// handles self_built, npm_registry, pypi, github_release, etc. uniformly.
async fn flip_drift_status(pool: &PgPool) -> Result<u64> {
    let res = sqlx::query(
        r#"
        UPDATE computer_software cs
           SET status = 'upgrade_available'
          FROM software_registry sr
         WHERE sr.id = cs.software_id
           AND sr.latest_version IS NOT NULL
           AND sr.latest_version <> ''
           AND cs.installed_version IS NOT NULL
           AND cs.installed_version <> sr.latest_version
           AND cs.status = 'ok'
        "#,
    )
    .execute(pool)
    .await
    .context("flip drift status")?;
    Ok(res.rows_affected())
}

/// For every `software_registry` row with `version_source.method='npm_registry'`,
/// query `https://registry.npmjs.org/<package>/latest` and write the returned
/// `version` field into `software_registry.latest_version`. Soft-fail per row
/// — a single registry hiccup must not poison the whole tick. The HTTP layer
/// honors a 5s timeout to keep the auto-upgrade tick bounded.
async fn refresh_npm_registry_latest_versions(pool: &PgPool) -> Result<u64> {
    refresh_via_http(
        pool,
        "npm_registry",
        |vs| {
            let pkg = vs.get("package")?.as_str()?;
            Some(format!("https://registry.npmjs.org/{pkg}/latest"))
        },
        |body| {
            let v: serde_json::Value = serde_json::from_str(body).ok()?;
            v.get("version")?.as_str().map(str::to_string)
        },
    )
    .await
}

/// PyPI version refresh. `version_source = {"method":"pypi","package":"vllm"}`.
async fn refresh_pypi_latest_versions(pool: &PgPool) -> Result<u64> {
    refresh_via_http(
        pool,
        "pypi",
        |vs| {
            let pkg = vs.get("package")?.as_str()?;
            Some(format!("https://pypi.org/pypi/{pkg}/json"))
        },
        |body| {
            let v: serde_json::Value = serde_json::from_str(body).ok()?;
            v.get("info")?.get("version")?.as_str().map(str::to_string)
        },
    )
    .await
}

/// GitHub release tag refresh.
/// `version_source = {"method":"github_release","repo":"cli/cli"}`.
/// Strips a leading 'v' from the tag (v2.91.0 → 2.91.0) so versions match
/// `--version` outputs.
async fn refresh_github_release_latest_versions(pool: &PgPool) -> Result<u64> {
    refresh_via_http(
        pool,
        "github_release",
        |vs| {
            let repo = vs.get("repo")?.as_str()?;
            Some(format!(
                "https://api.github.com/repos/{repo}/releases/latest"
            ))
        },
        |body| {
            let v: serde_json::Value = serde_json::from_str(body).ok()?;
            let tag = v.get("tag_name")?.as_str()?;
            Some(tag.strip_prefix('v').unwrap_or(tag).to_string())
        },
    )
    .await
}

/// Shared HTTP-based refresher. Walks every software_registry row whose
/// `version_source.method` matches `method`, builds a URL via `url_for`,
/// fetches it, parses the response with `extract_version`, and writes the
/// result. Per-row failures are logged at debug and skipped.
async fn refresh_via_http<UrlFn, ParseFn>(
    pool: &PgPool,
    method: &str,
    url_for: UrlFn,
    extract_version: ParseFn,
) -> Result<u64>
where
    UrlFn: Fn(&serde_json::Value) -> Option<String>,
    ParseFn: Fn(&str) -> Option<String>,
{
    let rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
        r#"
        SELECT id, version_source
          FROM software_registry
         WHERE version_source->>'method' = $1
        "#,
    )
    .bind(method)
    .fetch_all(pool)
    .await
    .with_context(|| format!("list software_registry for method={method}"))?;

    if rows.is_empty() {
        return Ok(0);
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .user_agent("forgefleetd/auto-upgrade")
        .build()
        .context("build reqwest client for upstream version refresh")?;

    let mut updated = 0u64;
    for (id, vs) in rows {
        let url = match url_for(&vs) {
            Some(u) => u,
            None => {
                tracing::debug!(software_id = %id, method, "skipping: version_source missing required field");
                continue;
            }
        };
        let body = match client.get(&url).send().await {
            Ok(r) if r.status().is_success() => match r.text().await {
                Ok(t) => t,
                Err(e) => {
                    tracing::debug!(software_id = %id, %url, error = %e, "upstream body read failed");
                    continue;
                }
            },
            Ok(r) => {
                tracing::debug!(software_id = %id, %url, status = %r.status(), "upstream non-2xx");
                continue;
            }
            Err(e) => {
                tracing::debug!(software_id = %id, %url, error = %e, "upstream fetch failed");
                continue;
            }
        };
        let version = match extract_version(&body) {
            Some(v) if !v.is_empty() => v,
            _ => {
                tracing::debug!(software_id = %id, %url, "upstream response missing version field");
                continue;
            }
        };
        // Dual-write: software_registry is the auto-upgrade catalog,
        // external_tools is the `ff ext` catalog. They overlap for tools
        // that live in both (openclaw, codex, claude-code, …). Update
        // both so `ff ext drift` and `ff software drift` agree.
        let res = sqlx::query(
            r#"
            UPDATE software_registry
               SET latest_version    = $2,
                   latest_version_at = NOW()
             WHERE id = $1
               AND (latest_version IS NULL OR latest_version <> $2)
            "#,
        )
        .bind(&id)
        .bind(&version)
        .execute(pool)
        .await;
        match res {
            Ok(r) if r.rows_affected() > 0 => {
                tracing::info!(
                    software_id = %id,
                    method,
                    version = %version,
                    "upstream version refreshed (software_registry)"
                );
                updated += 1;
            }
            Ok(_) => { /* unchanged */ }
            Err(e) => {
                tracing::warn!(software_id = %id, error = %e, "software_registry update failed")
            }
        }
        // Mirror to external_tools when an entry exists. Soft-fail.
        let _ = sqlx::query(
            r#"
            UPDATE external_tools
               SET latest_version    = $2,
                   latest_version_at = NOW()
             WHERE id = $1
               AND (latest_version IS NULL OR latest_version <> $2)
            "#,
        )
        .bind(&id)
        .bind(&version)
        .execute(pool)
        .await;
    }
    Ok(updated)
}
