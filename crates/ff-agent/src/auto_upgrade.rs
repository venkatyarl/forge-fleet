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

/// `fleet_secrets` key gating LEADER self-upgrade. Off/missing = the leader
/// stays excluded from auto-upgrade (the historical safe default — the wave
/// excludes the leader to avoid self-suicide). When truthy, the leader
/// self-upgrades its own daemon binary in a DETACHED process that survives the
/// daemon restart. Permanent default OFF so shipping this is harmless.
const LEADER_SELF_UPGRADE_KEY: &str = "leader_self_upgrade";

// The same-commit / drift predicate now lives in ONE place:
// `ff_core::build_version::{same_commit, is_same_version}`. Consolidated
// 2026-07-03 (LLM council codex+kimi) so a SHA edge case can't revive the
// phantom-drift restart loop through a forgotten duplicate.

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
    resolve_upgrade_plans_with_suffix(
        pool,
        software_id,
        only_computer,
        upgrade_available_only,
        None,
    )
    .await
}

/// Like [`resolve_upgrade_plans`] but with an optional `key_suffix` that
/// is prepended to the playbook-key candidates. Lets the wave dispatcher
/// request a build-only playbook (suffix=`build-only`) for Phase-1 of
/// the two-phase upgrade graph — `linux-ubuntu-build-only` is tried
/// before `linux-ubuntu`, falling through to the plain key if absent.
pub async fn resolve_upgrade_plans_with_suffix(
    pool: &PgPool,
    software_id: &str,
    only_computer: Option<&str>,
    upgrade_available_only: bool,
    key_suffix: Option<&str>,
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
                -- V114: never auto-upgrade a reserved/drained host (P3 / operator
                -- claimed it; the wave must not build there).
                AND COALESCE(c.reservation_state, 'available') = 'available'
                -- Drain-before-restart: never upgrade (= rebuild + restart
                -- forgefleetd on) a host with an IN-FLIGHT work_item build lease,
                -- or the restart orphans the build → stale-heartbeat reap → wasted
                -- attempt. That is exactly why long feature-task builds kept
                -- failing while short ones landed: a 20-40min build gets caught by
                -- the hourly upgrade wave. The host upgrades on a later tick once
                -- it is idle between builds (a perpetually-busy host correctly
                -- keeps its builds instead of being restarted out from under them).
                AND NOT EXISTS (
                    SELECT 1 FROM work_item_leases l
                     WHERE l.computer_id = c.id AND l.released_at IS NULL
                )
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
                AND COALESCE(c.reservation_state, 'available') = 'available'
                -- Drain-before-restart: never upgrade (= rebuild + restart
                -- forgefleetd on) a host with an IN-FLIGHT work_item build lease,
                -- or the restart orphans the build → stale-heartbeat reap → wasted
                -- attempt. That is exactly why long feature-task builds kept
                -- failing while short ones landed: a 20-40min build gets caught by
                -- the hourly upgrade wave. The host upgrades on a later tick once
                -- it is idle between builds (a perpetually-busy host correctly
                -- keeps its builds instead of being restarted out from under them).
                AND NOT EXISTS (
                    SELECT 1 FROM work_item_leases l
                     WHERE l.computer_id = c.id AND l.released_at IS NULL
                )
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

        // Skip a no-op self-upgrade: if the install is already the target
        // version (same commit, incl. a different SHA prefix length like
        // `3b644697cb` vs `3b644697cb71`), dispatching would git-reset +
        // rebuild + RESTART forgefleetd for nothing — killing any in-flight
        // build. This phantom drift was the root cause of the follower daemon
        // restart loop that prevented the fleet from ever self-building
        // (2026-07-03). Uses the ONE canonical predicate shared with
        // version_check + the wave (ff_core::build_version::is_same_version).
        if let (Some(inst), Some(latest)) =
            (installed_version.as_deref(), latest_version.as_deref())
        {
            if ff_core::build_version::is_same_version(inst, latest) {
                skipped.push((name, format!("already on target version {latest}")));
                continue;
            }
        }

        let candidates: Vec<String> = {
            let mut v = Vec::new();
            // If a key_suffix is requested, try suffixed variants FIRST so
            // a build-only / restart-only playbook key wins over the plain
            // key when present. Falls through to the plain key if the
            // suffixed variant doesn't exist for this os_family.
            if let Some(suffix) = key_suffix {
                if let Some(src) = &install_source {
                    v.push(format!("{os_family}-{src}-{suffix}"));
                }
                v.push(format!("{os_family}-{suffix}"));
                if let Some(base) = crate::upgrade_playbooks::base_family(&os_family) {
                    v.push(format!("{base}-{suffix}"));
                }
                v.push(format!("all-{suffix}"));
            }
            if let Some(src) = &install_source {
                v.push(format!("{os_family}-{src}"));
            }
            v.push(os_family.clone());
            // Base-family fallback: claude-code/codex ship a `linux` key
            // that pre-dates the linux-ubuntu / linux-dgx split. Try the
            // bare base family before falling back to `all`. Surfaced
            // 2026-04-30 — drift was stuck on every Linux host because the
            // dispatcher only tried `linux-ubuntu` and `all`, both missing
            // from the npm CLI playbooks.
            if let Some(base) = crate::upgrade_playbooks::base_family(&os_family) {
                v.push(base.to_string());
            }
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
                // Fallback ONLY when source_tree_path is NULL (unmaterialized node).
                // Auto-upgrade targets workers (the leader self-upgrades via its own
                // path), so default to the canonical per-slot worker location — never
                // regress a new worker to ~/projects (2026-07-07 layout migration).
                let raw_path = source_tree_path
                    .as_deref()
                    .unwrap_or("~/.forgefleet/sub-agents/sub-agent-0/forge-fleet");
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
pub async fn gate_git_state(
    pool: &PgPool,
    software_id: &str,
    force_dirty: bool,
) -> Result<GitStateGate> {
    if !matches!(software_id, "ff_git" | "forgefleetd_git") {
        return Ok(GitStateGate::Allow);
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
    .context("read leader git-state safety gate")?
    .flatten();

    Ok(match state.as_deref() {
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
    })
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
    // Keep the task insert and the status transition in one transaction.  The
    // previous two-step implementation could commit the task, fail the
    // best-effort status UPDATE, and enqueue a duplicate on the next tick.
    // Lock in a deterministic order so concurrent manual/automatic runs wait
    // for one another without deadlocking; the loser observes `upgrading` and
    // skips the already-dispatched target.
    let mut ordered: Vec<&UpgradePlan> = plans.iter().collect();
    ordered.sort_by(|a, b| {
        (&a.software_id, a.computer_name.to_ascii_lowercase())
            .cmp(&(&b.software_id, b.computer_name.to_ascii_lowercase()))
    });

    let mut tx = pool
        .begin()
        .await
        .context("begin atomic auto-upgrade enqueue")?;
    let mut out = Vec::with_capacity(plans.len());
    for p in ordered {
        let current_status: Option<String> = sqlx::query_scalar(
            "SELECT cs.status
               FROM computer_software cs
               JOIN computers c ON c.id = cs.computer_id
              WHERE cs.software_id = $1
                AND LOWER(c.name) = LOWER($2)
              FOR UPDATE OF cs",
        )
        .bind(&p.software_id)
        .bind(&p.computer_name)
        .fetch_optional(&mut *tx)
        .await
        .with_context(|| {
            format!(
                "lock computer_software for {} on {}",
                p.software_id, p.computer_name
            )
        })?;
        let Some(current_status) = current_status else {
            anyhow::bail!(
                "computer_software row disappeared for {} on {}",
                p.software_id,
                p.computer_name
            );
        };
        if current_status == "upgrading" {
            tracing::info!(
                software_id = %p.software_id,
                computer = %p.computer_name,
                "auto-upgrade target already upgrading; reusing the committed state fence"
            );
            continue;
        }

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
        let id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO fleet_tasks
                (task_type, summary, payload, priority, requires_capability, status,
                 created_at, task_class, not_before)
             VALUES (
                 'shell',
                 $1,
                 jsonb_strip_nulls(
                     jsonb_build_object(
                         'deferred_payload', $2,
                         'created_by', $3,
                         'kind', 'shell',
                         'trigger_type', 'node_online',
                         'trigger_spec', $4,
                         'preferred_node', $5,
                         'required_caps', '[]'::jsonb,
                         'attempts', 0,
                         'max_attempts', 3
                     )
                 ),
                 50,
                 '[]'::jsonb,
                 'pending',
                 NOW(),
                 'deferred',
                 NULL
             )
             RETURNING id",
        )
        .bind(&title)
        .bind(&payload)
        .bind(who)
        .bind(&trigger_spec)
        .bind(&p.computer_name)
        .fetch_one(&mut *tx)
        .await
        .with_context(|| {
            format!(
                "enqueue deferred upgrade for {} on {}",
                p.software_id, p.computer_name
            )
        })?;

        let updated = sqlx::query(
            "UPDATE computer_software cs
                SET status = 'upgrading',
                    metadata = COALESCE(cs.metadata, '{}'::jsonb)
                        || jsonb_build_object('upgrade_started_at', NOW())
               FROM computers c
              WHERE cs.computer_id = c.id
                AND cs.software_id = $1
                AND LOWER(c.name)  = LOWER($2)",
        )
        .bind(&p.software_id)
        .bind(&p.computer_name)
        .execute(&mut *tx)
        .await
        .with_context(|| {
            format!(
                "fence deferred upgrade for {} on {}",
                p.software_id, p.computer_name
            )
        })?
        .rows_affected();
        anyhow::ensure!(
            updated == 1,
            "expected one computer_software fence for {} on {}, updated {updated}",
            p.software_id,
            p.computer_name
        );

        out.push(EnqueuedPlan {
            computer_name: p.computer_name.clone(),
            defer_id: id.to_string(),
            software_id: p.software_id.clone(),
        });
    }
    tx.commit()
        .await
        .context("commit atomic auto-upgrade enqueue")?;
    Ok(out)
}

/// Does this `software_id` represent the daemon's own binary — i.e.
/// upgrading it requires restarting the daemon process that is
/// currently dispatching the upgrade? Used by [`AutoUpgradeTick::run_once`]
/// to gate the leader out of self-suicide.
pub(crate) fn is_daemon_self_software(software_id: &str) -> bool {
    // Single source of truth: derive membership from DAEMON_SELF_SOFTWARE so the
    // self-suicide gate and the wave-singleton serializer can never disagree on
    // which software ids restart forgefleetd (a divergence resurfaces the
    // cross-wave self-kill). See the const's doc comment.
    DAEMON_SELF_SOFTWARE.contains(&software_id)
}

/// The full daemon-self software family. Upgrading any of these restarts
/// `forgefleetd` on the target, which tears down whatever task_runner
/// subprocess that host is running — including a peer's in-flight build.
/// Used by the wave dispatcher to serialize the whole family so two
/// `*_git` waves never run concurrently against the same hosts (the
/// cross-wave self-kill documented in
/// feedback_wave_dispatcher_self_kill_race.md, resurfacing across wave
/// *generations* once V52's same-wave barrier was in place).
pub(crate) const DAEMON_SELF_SOFTWARE: &[&str] = &["ff_git", "forgefleetd_git", "forgefleet"];

/// V67 install bootstrap: for every `software_registry.auto_install=true`
/// row, insert a `computer_software` row (status='upgrade_available')
/// for any computer that doesn't already have one. Idempotent — re-runs
/// are no-ops once every member has its row. Closes the
/// new-software-never-installs gap caused by the materializer-only-from-
/// beats path.
async fn seed_auto_install_rows(pool: &PgPool) -> Result<u64> {
    let res = sqlx::query(
        r#"
        INSERT INTO computer_software (computer_id, software_id, status)
        SELECT c.id, sr.id, 'upgrade_available'
          FROM computers c
         CROSS JOIN software_registry sr
         WHERE sr.auto_install = true
           AND NOT EXISTS (
                SELECT 1 FROM computer_software cs
                 WHERE cs.computer_id = c.id
                   AND cs.software_id = sr.id
           )
        "#,
    )
    .execute(pool)
    .await
    .context("seed_auto_install_rows")?;
    let n = res.rows_affected();
    if n > 0 {
        tracing::info!(rows = n, "auto-upgrade: seeded auto_install rows");
    }
    Ok(n)
}

/// Is this process currently the elected leader?
pub(crate) async fn is_leader(_pool: &PgPool, _my_name: &str) -> bool {
    crate::leader_cache::is_current_leader()
}

/// Read leadership from the durable singleton instead of the daemon's hot-path
/// cache. Short-lived operator processes never initialize `LeaderCache`, so
/// explicit/run-once entry points must use this authority and fail closed when
/// it cannot be read.
pub async fn is_durable_leader(pool: &PgPool, my_name: &str) -> Result<bool> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM fleet_leader_state \
         WHERE singleton_key = 'current' \
           AND LOWER(member_name) = LOWER($1) \
           AND heartbeat_at > NOW() - INTERVAL '60 seconds')",
    )
    .bind(my_name)
    .fetch_one(pool)
    .await
    .context("read durable fleet leader authority")
}

/// Is the auto-upgrade feature turned on via `fleet_secrets`?
///
/// Treated as a self-expiring safety gate (V58): if an operator flipped
/// it off with `ff secrets disable-gate ... --hours N --reason "..."`
/// and the TTL has expired, this returns `true` (the safe default — fleet
/// expected posture is auto-upgrades flowing). Permanent-off rows
/// (no `expires_at`) still suppress the tick, so existing operators
/// who explicitly disable via `ff secrets set` are unaffected.
pub async fn is_enabled_durable(pool: &PgPool) -> Result<bool> {
    // default_when_missing = false   (preserve pre-V58 "off if no row")
    // restore_when_expired = true    (TTL'd disable auto-restores to ON,
    //                                 fleet's expected posture)
    ff_db::pg_read_safety_gate(pool, AUTO_UPGRADE_ENABLED_KEY, false, true)
        .await
        .context("read auto_upgrade_enabled safety gate")
}

pub(crate) async fn is_enabled(pool: &PgPool) -> bool {
    match is_enabled_durable(pool).await {
        Ok(enabled) => enabled,
        Err(error) => {
            tracing::warn!(%error, "auto-upgrade gate read failed; treating as disabled");
            false
        }
    }
}

/// Closes the leader-self-upgrade gap. The wave dispatcher excludes the leader
/// (upgrading the daemon's own binary mid-dispatch is self-suicide), so the
/// leader's `forgefleetd_git` never auto-upgrades — it drifts until a human
/// rebuilds it. When `fleet_secrets.leader_self_upgrade` is truthy AND the
/// leader is in drift on `forgefleetd_git`, this rebuilds + reinstalls the
/// leader binaries from the configured git ref and restarts the daemon via the
/// OS supervisor (`launchctl kickstart` / `systemctl --user restart` — graceful,
/// NOT pkill). The build runs in a DETACHED new session (`setsid`, mirroring
/// `model_runtime`'s spawn) so the daemon restart can't kill the in-flight
/// build. `set -e` guarantees a failed build NEVER installs/restarts. A
/// time-bounded per-target marker stops it re-firing while a build is running.
/// Gated OFF by default. Returns true if a self-upgrade was launched.
/// Live-verified 2026-06-01: leader self-heals on drift via this path.
async fn maybe_self_upgrade_leader(
    pool: &PgPool,
    my_name: &str,
    running_sha: &str,
) -> Result<bool> {
    // Gate: permanent-default OFF (no TTL auto-restore — self-restart is risky,
    // so it stays off unless explicitly enabled).
    if !ff_db::pg_read_safety_gate(pool, LEADER_SELF_UPGRADE_KEY, false, false)
        .await
        .context("read leader_self_upgrade safety gate")?
    {
        return Ok(false);
    }
    if !is_durable_leader(pool, my_name).await? {
        return Ok(false);
    }

    // Is the leader in drift on its own daemon binary? Compare the RUNNING
    // binary's compiled-in SHA (`running_sha`, from env!("FF_GIT_SHA") in the
    // caller's crate) against the registry's latest for forgefleetd_git.
    //
    // We deliberately do NOT key on `computer_software.installed_version`: that
    // column reflects the leader's SOURCE-TREE HEAD (a git pull / build bumps
    // it), which is almost always already current. A leader whose tree is built
    // but whose *process* is stale would then show zero drift and never
    // restart — the 2026-06-08 bug where vinny ran an 8h-old forgefleetd while
    // installed_version read latest, so the self-upgrade was a silent no-op and
    // a manual `launchctl kickstart` was required. The running binary's SHA is
    // the only signal that catches a stale process.
    let latest_sha: Option<String> = sqlx::query_scalar(
        "SELECT latest_version FROM software_registry \
         WHERE id = 'forgefleetd_git' AND latest_version IS NOT NULL",
    )
    .fetch_optional(pool)
    .await
    .context("read leader self-upgrade target SHA")?
    .flatten();
    let Some(latest_sha) = latest_sha else {
        return Ok(false); // no known target
    };
    // SHAs may differ in length (running is 10-char from --short=10; registry
    // may be 10 or full). Prefix-compare so the shorter is a prefix of the
    // longer. An empty running_sha (unknown build) means we can't judge drift —
    // skip rather than churn-restart.
    let running = running_sha.trim();
    let latest = latest_sha.trim();
    if running.is_empty() || running.starts_with(latest) || latest.starts_with(running) {
        return Ok(false); // running binary already at latest (or build unknown)
    }
    let target_sha = latest_sha;

    // Resolve the leader's source tree (same source as refresh_self_built).
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
    .context("read leader source_tree_path")?
    .flatten();
    let Some(source_tree) = source_tree else {
        anyhow::bail!("leader self-upgrade: no source_tree_path on leader");
    };
    let source_tree = expand_tilde(&source_tree);
    let home = std::env::var("HOME").unwrap_or_default();

    // git ref for forgefleetd_git (default origin/main, matching refresh_self_built).
    let git_ref: String = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT version_source FROM software_registry WHERE id = 'forgefleetd_git'",
    )
    .fetch_optional(pool)
    .await
    .context("read leader self-upgrade git ref")?
    .and_then(|vs| vs.get("git_ref").and_then(|v| v.as_str()).map(String::from))
    .unwrap_or_else(|| "origin/main".to_string());

    // Time-bounded marker: skip if a build for THIS target launched < 45min ago
    // (so a failed/hung build retries later instead of wedging forever).
    let marker = format!("{home}/.forgefleet/leader-self-upgrade.target");
    match std::fs::metadata(&marker) {
        Ok(meta) => {
            let recent = meta
                .modified()
                .context("read leader self-upgrade marker mtime")?
                .elapsed()
                .context("measure leader self-upgrade marker age")?
                .as_secs()
                < 2700;
            if recent
                && std::fs::read_to_string(&marker)
                    .context("read leader self-upgrade marker")?
                    .trim()
                    == target_sha
            {
                tracing::debug!(sha = %target_sha, "leader self-upgrade already in flight; skipping");
                return Ok(false);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("stat leader self-upgrade marker"),
    }

    // OS-specific graceful restart (NOT pkill). Code-signing is folded into the
    // atomic_install_cmd snippets below (macOS signs the `.new` temp before it
    // is validated + renamed into place).
    let codesign = cfg!(target_os = "macos");
    let restart = if codesign {
        "launchctl kickstart -k gui/$(id -u)/com.forgefleet.forgefleetd"
    } else {
        "systemctl --user restart forgefleetd"
    };
    // Atomic, validated installs: write to `.new`, prove `--version` runs, then
    // rename over the live binary. A disk-full / interrupted copy can never
    // leave a truncated binary in PATH (bricked ace's `ff` 2026-06-14). See
    // upgrade_playbooks::atomic_install_cmd.
    let install_ffd = crate::upgrade_playbooks::atomic_install_cmd(
        "forgefleetd",
        "$HOME/.local/bin/forgefleetd",
        codesign,
    );
    let install_ff =
        crate::upgrade_playbooks::atomic_install_cmd("ff", "$HOME/.local/bin/ff", codesign);
    let install_ff_cargo =
        crate::upgrade_playbooks::atomic_install_cmd("ff", "$HOME/.cargo/bin/ff", codesign);

    let log = format!("{home}/.forgefleet/logs/leader-self-upgrade.log");
    let script = format!(
        "#!/bin/sh\n\
         set -e\n\
         exec >>\"{log}\" 2>&1\n\
         echo \"=== leader self-upgrade target={target_sha} ref={git_ref} ===\"\n\
         date\n\
         cd \"{source_tree}\"\n\
         export GIT_SSH_COMMAND='ssh -o IdentityAgent=none -o BatchMode=yes -o ConnectTimeout=30 -o ServerAliveInterval=15 -o ServerAliveCountMax=4' GIT_HTTP_LOW_SPEED_LIMIT=1000 GIT_HTTP_LOW_SPEED_TIME=60\n\
         git fetch origin\n\
         git reset --hard \"{git_ref}\"\n\
         cargo build --release --bin forgefleetd\n\
         cargo build --release -p ff-terminal --bin ff\n\
         {install_ffd}\n\
         {install_ff}\n\
         if [ -f \"$HOME/.cargo/bin/ff\" ]; then {install_ff_cargo}; fi\n\
         echo \"build+install OK; waiting for leader handoff\"\n\
         sleep 15\n\
         {restart}\n"
    );
    let script_path = format!("{home}/.forgefleet/leader-self-upgrade.sh");
    std::fs::write(&script_path, &script).context("write leader self-upgrade helper script")?;
    std::fs::write(&marker, &target_sha).context("write leader self-upgrade target marker")?;

    // Ask the election loop to hand leadership to a follower before the
    // detached helper restarts this node. The request expires automatically.
    let handoff_until = chrono::Utc::now() + chrono::Duration::minutes(10);
    let handoff_value = format!("{my_name}|{}", handoff_until.to_rfc3339());
    if let Err(error) = ff_db::pg_set_secret(
        pool,
        "leader_yield_request",
        &handoff_value,
        Some("continuous rollout leader-last restart"),
        Some("auto-upgrade"),
    )
    .await
    {
        let _ = std::fs::remove_file(&marker);
        return Err(error).context("persist leader self-upgrade handoff request");
    }

    // Spawn DETACHED in a new session so the daemon restart (which kills
    // forgefleetd's process group) can't take the in-flight build down with it.
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg(&script_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            // SAFETY: setsid() is a single async-signal-safe syscall; detaches
            // the child into its own session so it outlives forgefleetd. Mirrors
            // model_runtime::load_model's detachment.
            let _ = libc::setsid();
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(child) => {
            tracing::warn!(
                target_sha = %target_sha,
                git_ref = %git_ref,
                source_tree = %source_tree,
                pid = child.id(),
                "LEADER SELF-UPGRADE launched (detached): rebuilding + restarting forgefleetd"
            );
            Ok(true)
        }
        Err(error) => {
            let clear_result = sqlx::query(
                "DELETE FROM fleet_secrets
                  WHERE key = 'leader_yield_request' AND value = $1",
            )
            .bind(&handoff_value)
            .execute(pool)
            .await;
            let marker_result = std::fs::remove_file(&marker);
            anyhow::bail!(
                "leader self-upgrade spawn failed: {error}; cleanup handoff={:?}, marker={:?}",
                clear_result.map(|result| result.rows_affected()),
                marker_result
            )
        }
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
    client: reqwest::Client,
    /// Compiled-in SHA of the RUNNING binary (`env!("FF_GIT_SHA")` from the
    /// caller's crate). Used by the leader self-upgrade to detect a stale
    /// running binary — the DB `installed_version` reflects the source-tree
    /// HEAD, not the running process, so it can't see this drift.
    running_sha: String,
}

/// Observable result for an operator-triggered tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoUpgradeRunOnceOutcome {
    pub refreshed_self_built: u64,
    pub enqueued: usize,
    pub rollouts_started: usize,
    pub skipped: usize,
    pub leader_self_upgrade_launched: bool,
}

fn record_operation_failure(
    failures: &mut Vec<String>,
    surface_failures: bool,
    software_id: &str,
    operation: &str,
    error: impl std::fmt::Display,
) {
    tracing::warn!(
        software_id = %software_id,
        operation = %operation,
        error = %error,
        "auto-upgrade operation failed"
    );
    if surface_failures {
        failures.push(format!("{software_id}: {operation}: {error}"));
    }
}

fn finish_run_once(
    refreshed_self_built: u64,
    enqueued: usize,
    rollouts_started: usize,
    skipped: usize,
    leader_self_upgrade_launched: bool,
    failures: Vec<String>,
) -> Result<AutoUpgradeRunOnceOutcome> {
    if !failures.is_empty() {
        anyhow::bail!(
            "auto-upgrade dispatch incomplete after refreshing {refreshed_self_built} self-built \
             version row(s), dispatching {enqueued} upgrade task(s), skipping {skipped} target(s), \
             starting {rollouts_started} rollout(s), and \
             leader_self_upgrade_launched={leader_self_upgrade_launched}: {} failure(s): {}",
            failures.len(),
            failures.join("; ")
        );
    }
    Ok(AutoUpgradeRunOnceOutcome {
        refreshed_self_built,
        enqueued,
        rollouts_started,
        skipped,
        leader_self_upgrade_launched,
    })
}

impl AutoUpgradeTick {
    pub fn new(pool: PgPool, my_name: String, running_sha: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .user_agent("forgefleetd/auto-upgrade")
            .build()
            .expect("build reqwest client");
        Self {
            pool,
            my_name,
            client,
            running_sha,
        }
    }

    /// One tick: gate, find drift, enqueue.
    ///
    /// Pass `force = true` to bypass the `auto_upgrade_enabled` secret gate.
    /// The leader check is never bypassed — run this on the leader.
    pub async fn run_once(&self, force: bool) -> Result<usize> {
        if !is_leader(&self.pool, &self.my_name).await {
            return Ok(0);
        }
        Ok(self.run_once_authorized(force, false).await?.enqueued)
    }

    /// Operator/CLI run-once path. Unlike the daemon hot path, leadership is
    /// read directly from `fleet_leader_state` and self-built refresh failures
    /// are returned to the caller instead of becoming a false zero-success.
    pub async fn run_once_durable(&self, force: bool) -> Result<AutoUpgradeRunOnceOutcome> {
        if !is_durable_leader(&self.pool, &self.my_name).await? {
            anyhow::bail!(
                "worker '{}' is not the fresh current leader in fleet_leader_state",
                self.my_name
            );
        }
        self.run_once_authorized(force, true).await
    }

    async fn run_once_authorized(
        &self,
        force: bool,
        surface_failures: bool,
    ) -> Result<AutoUpgradeRunOnceOutcome> {
        let enabled = if surface_failures {
            is_enabled_durable(&self.pool).await?
        } else {
            is_enabled(&self.pool).await
        };
        if !force && !enabled {
            tracing::debug!(
                "auto-upgrade disabled (fleet_secrets.auto_upgrade_enabled not truthy)"
            );
            return Ok(AutoUpgradeRunOnceOutcome {
                refreshed_self_built: 0,
                enqueued: 0,
                rollouts_started: 0,
                skipped: 0,
                leader_self_upgrade_launched: false,
            });
        }

        // Self-built tools (ff_git, forgefleetd_git, etc.) use method=self_built
        // which means "leader's installed version IS canonical." The 6h
        // software_upstream tick eventually refreshes software_registry.latest_version,
        // but that's too slow for active dev. Do an inline refresh here on every
        // auto-upgrade tick — one SQL UPDATE per row. If leader's row just flipped,
        // the next line (drift check) will see upgrade_available immediately.
        let mut failures = Vec::new();
        let refreshed_self_built = if surface_failures {
            refresh_self_built_latest_versions(&self.pool).await?
        } else {
            refresh_self_built_latest_versions(&self.pool)
                .await
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "auto-upgrade: self-built refresh failed");
                    0
                })
        };
        // npm-distributed tools (codex, context-mode, …): query
        // registry.npmjs.org/<pkg>/latest. Same-tick refresh for parity with
        // self_built — without this, npm releases sit unnoticed indefinitely.
        for (operation, result) in [
            (
                "refresh npm registry versions",
                refresh_npm_registry_latest_versions(&self.client, &self.pool).await,
            ),
            (
                "refresh PyPI versions",
                refresh_pypi_latest_versions(&self.client, &self.pool).await,
            ),
            (
                "refresh GitHub release versions",
                refresh_github_release_latest_versions(&self.client, &self.pool).await,
            ),
            (
                "refresh git-head versions",
                refresh_git_head_latest_versions(&self.client, &self.pool).await,
            ),
            (
                "seed auto-install rows",
                seed_auto_install_rows(&self.pool).await,
            ),
            ("flip drift status", flip_drift_status(&self.pool).await),
        ] {
            if let Err(error) = result {
                record_operation_failure(
                    &mut failures,
                    surface_failures,
                    "registry",
                    operation,
                    error,
                );
            }
        }
        // PyPI-distributed (vllm, mlx_lm, …) and GitHub-released (gh, etc.)
        // follow the same shape, different upstream URL.
        // V67 install bootstrap: for `software_registry.auto_install = true`
        // rows, seed a `computer_software` row (status='upgrade_available')
        // for every member that doesn't already have one. Without this,
        // newly-registered software with no installed members would never
        // dispatch — flip_drift_status only operates on existing rows,
        // and the materializer only creates rows when a beat reports the
        // software detected. Closes the install-bootstrap loop.
        // Then: flip computer_software.status = 'upgrade_available' for any row
        // where installed_version != latest_version and status is currently 'ok'.
        // Generic across all methods.
        if surface_failures && !failures.is_empty() {
            return finish_run_once(refreshed_self_built, 0, 0, 0, false, failures);
        }

        // Leader self-upgrade (closes the leader gap). Runs BEFORE the
        // ids.is_empty() early return below so the leader self-heals on its OWN
        // forgefleetd_git drift even when no other software is drifted — which
        // is the common case (the wave already upgraded everyone else). Gated
        // by fleet_secrets.leader_self_upgrade (OFF by default).
        // Continuous rollouts converge followers first. The leader's detached
        // self-upgrade is considered only after no automatic follower rollout
        // remains in progress (leader-last invariant).
        let auto_mode =
            match crate::upgrade_rollout::continuous_mode_is_auto_durable(&self.pool).await {
                Ok(value) => Some(value),
                Err(error) => {
                    record_operation_failure(
                        &mut failures,
                        surface_failures,
                        "rollout",
                        "read continuous rollout mode",
                        error,
                    );
                    None
                }
            };
        let auto_rollout_inflight_result: Result<bool, sqlx::Error> = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM upgrade_rollouts \
             WHERE automatic = TRUE AND status = 'in_progress')",
        )
        .fetch_one(&self.pool)
        .await;
        let auto_rollout_inflight = match auto_rollout_inflight_result {
            Ok(value) => Some(value),
            Err(error) => {
                record_operation_failure(
                    &mut failures,
                    surface_failures,
                    "rollout",
                    "read active automatic rollout",
                    error,
                );
                None
            }
        };
        let followers_converged: Option<bool> = match auto_mode {
            Some(true) => match sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM upgrade_rollouts ur \
                 JOIN software_registry sr ON sr.id = ur.software_id \
                 WHERE ur.automatic = TRUE AND ur.status = 'completed' \
                   AND ur.target_version = sr.latest_version)",
            )
            .fetch_one(&self.pool)
            .await
            {
                Ok(value) => Some(value),
                Err(error) => {
                    record_operation_failure(
                        &mut failures,
                        surface_failures,
                        "rollout",
                        "read follower convergence",
                        error,
                    );
                    None
                }
            },
            Some(false) => Some(true),
            None => None,
        };
        if surface_failures && !failures.is_empty() {
            return finish_run_once(refreshed_self_built, 0, 0, 0, false, failures);
        }
        let mut daemon_self_dispatch_allowed =
            auto_mode.is_some() && auto_rollout_inflight.is_some() && followers_converged.is_some();
        let mut leader_self_upgrade_launched = false;
        if daemon_self_dispatch_allowed
            && !auto_rollout_inflight.unwrap_or(true)
            && followers_converged.unwrap_or(false)
        {
            match maybe_self_upgrade_leader(&self.pool, &self.my_name, &self.running_sha).await {
                Ok(launched) => {
                    leader_self_upgrade_launched = launched;
                    if launched {
                        // A detached leader restart is a complete, observable
                        // side effect.  Do not dispatch anything else from an
                        // operator run that is about to lose its leader.
                        daemon_self_dispatch_allowed = false;
                        if surface_failures {
                            return finish_run_once(refreshed_self_built, 0, 0, 0, true, failures);
                        }
                    }
                }
                Err(error) => {
                    record_operation_failure(
                        &mut failures,
                        surface_failures,
                        "leader",
                        "launch self-upgrade",
                        error,
                    );
                    daemon_self_dispatch_allowed = false;
                }
            }
        }
        if surface_failures && !failures.is_empty() {
            return finish_run_once(
                refreshed_self_built,
                0,
                0,
                0,
                leader_self_upgrade_launched,
                failures,
            );
        }

        let ids = software_ids_with_drift(&self.pool).await?;
        if ids.is_empty() {
            return finish_run_once(
                refreshed_self_built,
                0,
                0,
                0,
                leader_self_upgrade_launched,
                failures,
            );
        }

        let who = format!("auto-upgrade@{}", self.my_name);
        let mut total = 0usize;
        let mut rollouts_started = 0usize;
        let mut skipped_total = 0usize;
        for software_id in &ids {
            let (plans, skipped) =
                match resolve_upgrade_plans(&self.pool, software_id, None, true).await {
                    Ok(x) => x,
                    Err(e) => {
                        record_operation_failure(
                            &mut failures,
                            surface_failures,
                            software_id,
                            "resolve upgrade plans",
                            e,
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
            skipped_total += skipped.len();
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
            let gate = match gate_git_state(&self.pool, software_id, false).await {
                Ok(gate) => gate,
                Err(error) => {
                    record_operation_failure(
                        &mut failures,
                        surface_failures,
                        software_id,
                        "read leader git-state safety gate",
                        error,
                    );
                    continue;
                }
            };
            match gate {
                GitStateGate::BlockDirty => {
                    tracing::warn!(
                        software_id = %software_id,
                        sha = %leader_sha,
                        "refusing to propagate dirty build {leader_sha} — commit or pass --force-dirty"
                    );
                    mark_targets_blocked_dirty(&self.pool, software_id).await;
                    if surface_failures {
                        failures.push(format!(
                            "{software_id}: dispatch blocked because leader build {leader_sha} is dirty"
                        ));
                    }
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

            // ── Leader-suicide safety gate ─────────────────────────
            // If this software's `version_source.method` is `self_built`
            // and this row's playbook restarts the daemon (i.e. it's
            // the daemon's own binary — `ff_git`, `forgefleetd_git`),
            // running the upgrade in the deferred-task worker on the
            // leader would mid-execution kill the worker. The launchd
            // / systemd supervisor sends SIGKILL to the whole unit
            // process group; the worker dies with the daemon, the
            // task is left `running`, the watchdog re-queues it 120s
            // later, infinite loop until MAX_HANDOFFS. We hit this
            // live on 2026-04-26.
            //
            // Fix: drop the leader from the per-target plan list for
            // these rows. The wave dispatcher
            // (`ff tasks compose-fleet-upgrade`) is the right path
            // for upgrading the leader — it's peer-driven and avoids
            // the suicide entirely. The hourly tick stays on the
            // defer queue for everyone else.
            // For daemon-self software (*_git), dispatch via the
            // two-phase wave dispatcher (fleet_tasks) instead of the
            // deferred queue. The wave dispatcher's Phase-2 restart is
            // serialized on the leader via the `wait_for_siblings`
            // barrier, eliminating the self-kill race documented in
            // feedback_wave_dispatcher_self_kill_race.md and giving
            // macOS targets a launchctl restart path. Other software
            // (apt/brew/pip-managed) keeps using the deferred queue.
            //
            // Important: this branch runs BEFORE the leader-filter
            // and plans-empty short-circuit below. compose_fleet_upgrade_wave
            // does its own resolve with `upgrade_available_only=false`
            // and handles leader exclusion internally — so even if the
            // tick's pre-filtered plans only contained the leader (and
            // would otherwise be filtered to empty), the wave still
            // sees and processes every non-leader target.
            if is_daemon_self_software(software_id) {
                if !daemon_self_dispatch_allowed {
                    skipped_total += plans.len();
                    tracing::warn!(
                        software_id = %software_id,
                        "auto-upgrade: daemon-self dispatch skipped because rollout authority is unavailable or a leader self-upgrade launched"
                    );
                    continue;
                }
                if auto_mode == Some(true) {
                    let target_sha = plans
                        .iter()
                        .find_map(|p| p.latest_version.as_deref())
                        .unwrap_or_default();
                    match crate::upgrade_rollout::maybe_start_continuous_rollout(
                        &self.pool,
                        software_id,
                        &self.my_name,
                        &self.running_sha,
                        target_sha,
                    )
                    .await
                    {
                        Ok(true) => rollouts_started += 1,
                        Ok(false) => {}
                        Err(e) => record_operation_failure(
                            &mut failures,
                            surface_failures,
                            software_id,
                            "start continuous rollout",
                            e,
                        ),
                    }
                    continue;
                }
                let leader_id: uuid::Uuid = match sqlx::query_scalar(
                    "SELECT computer_id FROM fleet_leader_state \
                     WHERE singleton_key = 'current' LIMIT 1",
                )
                .fetch_optional(&self.pool)
                .await
                {
                    Ok(Some(leader_id)) => leader_id,
                    Ok(None) => {
                        record_operation_failure(
                            &mut failures,
                            surface_failures,
                            software_id,
                            "read wave leader",
                            "no current leader row",
                        );
                        continue;
                    }
                    Err(error) => {
                        record_operation_failure(
                            &mut failures,
                            surface_failures,
                            software_id,
                            "read wave leader",
                            error,
                        );
                        continue;
                    }
                };
                match crate::task_runner::compose_fleet_upgrade_wave(
                    &self.pool,
                    software_id,
                    4,
                    leader_id,
                    false,
                )
                .await
                {
                    Ok(plan) => {
                        tracing::info!(
                            software_id = %software_id,
                            parent_task_id = ?plan.parent,
                            created_tasks = plan.created_tasks,
                            "auto-upgrade dispatched via two-phase wave"
                        );
                        total += plan.created_tasks;
                    }
                    Err(e) => record_operation_failure(
                        &mut failures,
                        surface_failures,
                        software_id,
                        "compose fleet upgrade wave",
                        e,
                    ),
                }
                continue;
            }

            // Non-*_git path: drop leader from plans (suicide protection,
            // historic — package-manager upgrades don't restart the
            // daemon, but keep the filter for parity with old behavior),
            // then enqueue via the deferred queue.
            let plans: Vec<_> = {
                let leader_name_lc = self.my_name.to_ascii_lowercase();
                plans
                    .into_iter()
                    .filter(|p| !p.computer_name.eq_ignore_ascii_case(&leader_name_lc))
                    .collect()
            };
            if plans.is_empty() {
                continue;
            }

            match enqueue_plans(&self.pool, &plans, &who).await {
                Ok(enqueued) => {
                    tracing::info!(
                        software_id = %software_id,
                        dispatched = enqueued.len(),
                        "auto-upgrade dispatched"
                    );
                    // Publish a start event — finalizer publishes completion.
                    for plan in plans.iter().filter(|plan| {
                        enqueued.iter().any(|item| {
                            item.software_id == plan.software_id
                                && item.computer_name.eq_ignore_ascii_case(&plan.computer_name)
                        })
                    }) {
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
                    skipped_total += plans.len().saturating_sub(enqueued.len());
                    total += enqueued.len();
                }
                Err(e) => {
                    record_operation_failure(
                        &mut failures,
                        surface_failures,
                        software_id,
                        "enqueue upgrade plans",
                        e,
                    );
                }
            }
        }

        finish_run_once(
            refreshed_self_built,
            total,
            rollouts_started,
            skipped_total,
            leader_self_upgrade_launched,
            failures,
        )
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelfBuiltDecision {
    Seed,
    Unchanged,
    Advance,
    Hold,
}

fn parse_full_lower_git_sha(raw: &str) -> std::result::Result<String, String> {
    let sha = raw.trim();
    if sha.len() == 40
        && sha.bytes().all(|b| b.is_ascii_hexdigit())
        && !sha.bytes().any(|b| b.is_ascii_uppercase())
    {
        Ok(sha.to_string())
    } else {
        Err(format!(
            "expected one lowercase 40-hex commit SHA, got {sha:?}"
        ))
    }
}

fn decide_self_built_update(
    stored_present: bool,
    resolved_stored: Option<&str>,
    candidate: &str,
    stored_is_ancestor: Option<bool>,
) -> SelfBuiltDecision {
    if !stored_present {
        return SelfBuiltDecision::Seed;
    }
    let Some(resolved) = resolved_stored else {
        return SelfBuiltDecision::Hold;
    };
    if resolved == candidate {
        return SelfBuiltDecision::Unchanged;
    }
    match stored_is_ancestor {
        Some(true) => SelfBuiltDecision::Advance,
        _ => SelfBuiltDecision::Hold,
    }
}

fn git_output(repo: &str, args: &[&str]) -> std::result::Result<std::process::Output, String> {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))
}

fn git_full_sha(repo: &str, spec: &str) -> std::result::Result<String, String> {
    let output = git_output(repo, &["rev-parse", "--verify", spec])?;
    if !output.status.success() {
        return Err(format!(
            "git rev-parse --verify {spec}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    parse_full_lower_git_sha(&String::from_utf8_lossy(&output.stdout))
}

fn fetch_canonical_origin_main(repo: &str) -> std::result::Result<String, String> {
    let output = git_output(
        repo,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            "origin",
            "+refs/heads/main:refs/remotes/origin/main",
        ],
    )?;
    if !output.status.success() {
        return Err(format!(
            "git fetch origin main: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    git_full_sha(repo, "refs/remotes/origin/main^{commit}")
}

fn resolve_stored_commit(repo: &str, stored: &str) -> std::result::Result<String, String> {
    let stored = stored.trim();
    if !(7..=40).contains(&stored.len()) || !stored.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("stored self-built SHA is malformed: {stored:?}"));
    }
    let spec = format!("{stored}^{{commit}}");
    git_full_sha(repo, &spec)
}

fn stored_is_ancestor(
    repo: &str,
    stored: &str,
    candidate: &str,
) -> std::result::Result<bool, String> {
    let output = git_output(repo, &["merge-base", "--is-ancestor", stored, candidate])?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        code => Err(format!(
            "git merge-base --is-ancestor exited {code:?}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelfBuiltUpdate {
    id: String,
    observed: Option<String>,
    change: bool,
}

fn plan_self_built_updates(
    repo: &str,
    candidate: &str,
    rows: &[(String, Option<String>)],
) -> std::result::Result<Vec<SelfBuiltUpdate>, String> {
    let mut plan = Vec::with_capacity(rows.len());
    for (id, observed) in rows {
        let decision = match observed.as_deref() {
            None => decide_self_built_update(false, None, candidate, None),
            Some(stored) => {
                let resolved =
                    resolve_stored_commit(repo, stored).map_err(|e| format!("{id}: {e}"))?;
                let ancestor = if resolved == candidate {
                    None
                } else {
                    Some(
                        stored_is_ancestor(repo, &resolved, candidate)
                            .map_err(|e| format!("{id}: {e}"))?,
                    )
                };
                decide_self_built_update(true, Some(&resolved), candidate, ancestor)
            }
        };
        match decision {
            SelfBuiltDecision::Seed | SelfBuiltDecision::Advance => {
                plan.push(SelfBuiltUpdate {
                    id: id.clone(),
                    observed: observed.clone(),
                    change: true,
                });
            }
            SelfBuiltDecision::Unchanged => plan.push(SelfBuiltUpdate {
                id: id.clone(),
                observed: observed.clone(),
                // A uniquely resolved legacy prefix denotes the same commit,
                // but authority itself is always persisted as the full SHA.
                change: observed.as_deref().map(str::trim) != Some(candidate),
            }),
            SelfBuiltDecision::Hold => {
                return Err(format!(
                    "{id}: candidate {candidate} is not a verified fast-forward from {observed:?}"
                ));
            }
        }
    }
    Ok(plan)
}

fn self_built_snapshot_unchanged(
    observed: &[(String, Option<String>)],
    locked: &[(String, Option<String>)],
) -> bool {
    observed == locked
}

/// Refresh the self-built registry from one canonical, fetched remote-main
/// commit. Installed versions and the checkout's current HEAD are observations,
/// never writers of upstream truth. Any malformed, missing, ambiguous,
/// unavailable, rollback, or divergent row holds the entire set unchanged.
pub async fn refresh_self_built_latest_versions(pool: &PgPool) -> Result<u64> {
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

    let repo = expand_tilde(&source_tree);
    let fetch_repo = repo.clone();
    let candidate =
        match tokio::task::spawn_blocking(move || fetch_canonical_origin_main(&fetch_repo))
            .await
            .context("join canonical origin/main fetch")?
        {
            Ok(candidate) => candidate,
            Err(reason) => anyhow::bail!(
                "refresh_self_built: canonical origin/main fetch unavailable: {reason}"
            ),
        };

    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT id, latest_version FROM software_registry \
         WHERE version_source->>'method' = 'self_built' \
            OR id IN ('ff_git', 'forgefleetd_git') \
         ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .context("list self_built software")?;

    if rows.is_empty() {
        return Ok(0);
    }

    let plan_repo = repo.clone();
    let plan_candidate = candidate.clone();
    let plan_rows = rows.clone();
    let plan = match tokio::task::spawn_blocking(move || {
        plan_self_built_updates(&plan_repo, &plan_candidate, &plan_rows)
    })
    .await
    .context("join self-built ancestry plan")?
    {
        Ok(plan) => plan,
        Err(reason) => anyhow::bail!(
            "refresh_self_built: candidate {candidate} failed monotonic proof: {reason}"
        ),
    };

    if !plan.iter().any(|row| row.change) {
        return Ok(0);
    }

    let mut tx = pool
        .begin()
        .await
        .context("begin self-built authority update")?;
    sqlx::query("LOCK TABLE software_registry IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await
        .context("lock software_registry for self-built authority update")?;
    let locked_rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT id, latest_version FROM software_registry \
         WHERE version_source->>'method' = 'self_built' \
            OR id IN ('ff_git', 'forgefleetd_git') \
         ORDER BY id",
    )
    .fetch_all(&mut *tx)
    .await
    .context("recheck self-built rows under lock")?;
    if !self_built_snapshot_unchanged(&rows, &locked_rows) {
        anyhow::bail!("self-built authority changed concurrently; refusing partial update");
    }

    let mut updated = 0u64;
    for row in plan.iter().filter(|row| row.change) {
        let result = sqlx::query(
            "UPDATE software_registry \
                SET latest_version = $1, latest_version_at = NOW() \
              WHERE id = $2 AND latest_version IS NOT DISTINCT FROM $3",
        )
        .bind(&candidate)
        .bind(&row.id)
        .bind(&row.observed)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("update self-built authority for {}", row.id))?;
        if result.rows_affected() != 1 {
            anyhow::bail!("self-built authority CAS failed for {}", row.id);
        }
        updated += 1;
    }
    tx.commit()
        .await
        .context("commit self-built authority update")?;
    tracing::info!(candidate = %candidate, updated, "refresh_self_built: authority advanced atomically");
    Ok(updated)
}

/// Expand a leading `~` in a path string. The DB stores paths like
/// `~/projects/forge-fleet`; child commands inherit the daemon's `$HOME`.
fn expand_tilde(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    s.to_string()
}

/// For every computer_software row where installed_version differs from
/// software_registry.latest_version AND status is currently 'ok', flip to
/// 'upgrade_available' so the drift query picks it up. Runs after the
/// per-method refresh fns so `latest_version` is current. Method-agnostic —
/// handles self_built, npm_registry, pypi, github_release, etc. uniformly.
async fn flip_drift_status(pool: &PgPool) -> Result<u64> {
    // Exact equality for ordinary package versions, plus guarded SHA-prefix
    // equality for Git collectors that still report a 7-39 character commit.
    // The hex/length guards keep values such as semver and release tags from
    // being treated as prefixes of one another.
    const VERSIONS_MATCH_SQL: &str = r#"
        (
          cs.installed_version = sr.latest_version
          OR (
            btrim(cs.installed_version) ~* '^[0-9a-f]{7,40}$'
            AND btrim(sr.latest_version) ~* '^[0-9a-f]{7,40}$'
            AND (
              lower(btrim(cs.installed_version)) LIKE lower(btrim(sr.latest_version)) || '%'
              OR lower(btrim(sr.latest_version)) LIKE lower(btrim(cs.installed_version)) || '%'
            )
          )
        )
    "#;

    // Rewrites status from authoritative inputs (installed_version,
    // latest_version, leader's git_state) so the field stays accurate
    // tick-to-tick instead of drifting after a transient leader-dirty
    // state set rows to `upgrade_blocked_dirty`.
    //
    // Rules:
    // - drift exists (installed != latest) AND leader git_state != dirty
    //   → `upgrade_available` (handles both `ok` and stale
    //   `upgrade_blocked_dirty` rows).
    // - drift exists AND leader git_state == dirty
    //   → `upgrade_blocked_dirty` (gate fires before any upgrade
    //   attempt, including outside run_once).
    //
    // Both clauses are scoped to status IN ('ok', 'upgrade_available',
    // 'upgrade_blocked_dirty') so we never clobber `upgrading` /
    // `failed` / other in-flight terminal states.
    let unblocked_sql = format!(
        r#"
        UPDATE computer_software cs
           SET status = 'upgrade_available'
          FROM software_registry sr
         WHERE sr.id = cs.software_id
           AND sr.latest_version IS NOT NULL
           AND sr.latest_version <> ''
           AND cs.installed_version IS NOT NULL
           AND NOT {VERSIONS_MATCH_SQL}
           AND cs.status IN ('ok', 'upgrade_blocked_dirty')
           AND (
             sr.id NOT IN ('ff_git', 'forgefleetd_git')
             OR NOT EXISTS (
               SELECT 1 FROM computer_software cs2
                 JOIN computers c2 ON c2.id = cs2.computer_id
                 JOIN fleet_leader_state fls ON LOWER(fls.member_name) = LOWER(c2.name)
                WHERE cs2.software_id = sr.id
                  AND cs2.metadata->>'git_state' = 'dirty'
             )
           )
        "#,
    );
    let unblocked = sqlx::query(&unblocked_sql)
        .execute(pool)
        .await
        .context("flip drift status — unblock")?;

    let blocked = sqlx::query(
        r#"
        UPDATE computer_software cs
           SET status = 'upgrade_blocked_dirty'
          FROM software_registry sr
         WHERE sr.id = cs.software_id
           AND sr.id IN ('ff_git', 'forgefleetd_git')
           AND cs.status IN ('ok', 'upgrade_available')
           AND EXISTS (
             SELECT 1 FROM computer_software cs2
               JOIN computers c2 ON c2.id = cs2.computer_id
               JOIN fleet_leader_state fls ON LOWER(fls.member_name) = LOWER(c2.name)
              WHERE cs2.software_id = sr.id
                AND cs2.metadata->>'git_state' = 'dirty'
           )
        "#,
    )
    .execute(pool)
    .await
    .context("flip drift status — block on dirty leader")?;

    // Third clause: once the fleet has CONVERGED (installed == latest) and
    // the leader is clean, clear any stale `upgrade_blocked_dirty` rows
    // back to `ok`. Without this, the first two clauses leave nodes stuck
    // showing `upgrade_blocked_dirty` forever — the unblock clause requires
    // `installed_version <> latest_version` (drift), and after a successful
    // wave there is no drift. Observed on 2026-05-20 after two-round
    // forgefleetd_git upgrade left 14/14 hosts at HEAD but still flagged.
    let converged_sql = format!(
        r#"
        UPDATE computer_software cs
           SET status = 'ok'
          FROM software_registry sr
         WHERE sr.id = cs.software_id
           AND sr.latest_version IS NOT NULL
           AND sr.latest_version <> ''
           AND {VERSIONS_MATCH_SQL}
           AND cs.status = 'upgrade_blocked_dirty'
           AND (
             sr.id NOT IN ('ff_git', 'forgefleetd_git')
             OR NOT EXISTS (
               SELECT 1 FROM computer_software cs2
                 JOIN computers c2 ON c2.id = cs2.computer_id
                 JOIN fleet_leader_state fls ON LOWER(fls.member_name) = LOWER(c2.name)
                WHERE cs2.software_id = sr.id
                  AND cs2.metadata->>'git_state' = 'dirty'
             )
           )
        "#,
    );
    let converged = sqlx::query(&converged_sql)
        .execute(pool)
        .await
        .context("flip drift status — clear converged stale dirty")?;

    Ok(unblocked.rows_affected() + blocked.rows_affected() + converged.rows_affected())
}

/// For every `software_registry` row with `version_source.method='npm_registry'`,
/// query `https://registry.npmjs.org/<package>/latest` and write the returned
/// `version` field into `software_registry.latest_version`. Soft-fail per row
/// — a single registry hiccup must not poison the whole tick. The HTTP layer
/// honors a 5s timeout to keep the auto-upgrade tick bounded.
async fn refresh_npm_registry_latest_versions(
    client: &reqwest::Client,
    pool: &PgPool,
) -> Result<u64> {
    refresh_via_http(
        client,
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
async fn refresh_pypi_latest_versions(client: &reqwest::Client, pool: &PgPool) -> Result<u64> {
    refresh_via_http(
        client,
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

/// GitHub release refresh, `ref_kind`-aware.
/// `version_source = {"method":"github_release","repo":"cli/cli"}`.
///
/// `ref_kind` (default `tagged`) selects WHAT "latest" means:
/// - `tagged` (or absent) → `releases/latest`; tag_name with a leading 'v'
///   stripped (v2.91.0 → 2.91.0) so it matches `--version` output.
/// - `main` / `master` / `branch:<x>` → that branch's HEAD commit
///   (`commits/{ref}`), 10-char SHA — matches what `software_collector`
///   reports for `*_git` rows.
///
/// Previously this ALWAYS hit `releases/latest`, ignoring `ref_kind`. For
/// self-built rows like `forgefleetd_git`/`ff_git` (private repo, `ref_kind=main`,
/// no releases) that 404s and `refresh_via_http` silently skipped → `latest_version`
/// froze and the drift→dispatch loop went blind to new `main` commits (#26).
async fn refresh_github_release_latest_versions(
    client: &reqwest::Client,
    pool: &PgPool,
) -> Result<u64> {
    refresh_via_http(
        client,
        pool,
        "github_release",
        |vs| {
            let repo = vs.get("repo")?.as_str()?;
            let ref_kind = vs
                .get("ref_kind")
                .and_then(|v| v.as_str())
                .unwrap_or("tagged");
            let url = match ref_kind {
                "tagged" | "release" | "latest" => {
                    format!("https://api.github.com/repos/{repo}/releases/latest")
                }
                "main" | "master" => {
                    format!("https://api.github.com/repos/{repo}/commits/{ref_kind}")
                }
                // "branch:<x>" → commits/<x>; any other literal → commits/<literal>.
                other => {
                    let branch = other.strip_prefix("branch:").unwrap_or(other);
                    format!("https://api.github.com/repos/{repo}/commits/{branch}")
                }
            };
            Some(url)
        },
        |body| {
            let v: serde_json::Value = serde_json::from_str(body).ok()?;
            // releases/latest → {"tag_name": "..."}; commits/{ref} → {"sha": "..."}.
            // Try both shapes so one parser serves every ref_kind.
            if let Some(tag) = v.get("tag_name").and_then(|t| t.as_str()) {
                return Some(tag.strip_prefix('v').unwrap_or(tag).to_string());
            }
            if let Some(sha) = v.get("sha").and_then(|s| s.as_str()) {
                return Some(sha.chars().take(10).collect());
            }
            None
        },
    )
    .await
}

/// `version_source = {"method":"git_head","repo":"https://github.com/nexu-io/open-design","ref_kind":"main"}`.
/// Used for tools we install by git-clone but that don't ship npm/pypi/github
/// releases (e.g. open-design as of 2026-04-30 — `latestRelease: null`).
/// Returns the full SHA of the named branch's HEAD. Installed-version drift
/// comparison remains prefix tolerant; authority writes never abbreviate.
async fn refresh_git_head_latest_versions(client: &reqwest::Client, pool: &PgPool) -> Result<u64> {
    refresh_via_http(
        client,
        pool,
        "git_head",
        |vs| {
            // repo can be either "owner/name" or a full https URL.
            let repo_raw = vs.get("repo")?.as_str()?;
            let repo = repo_raw
                .trim_start_matches("https://github.com/")
                .trim_end_matches(".git");
            let ref_kind = vs
                .get("ref_kind")
                .and_then(|v| v.as_str())
                .unwrap_or("main");
            Some(format!(
                "https://api.github.com/repos/{repo}/commits/{ref_kind}"
            ))
        },
        parse_git_head_response,
    )
    .await
}

fn parse_git_head_response(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let sha = value.get("sha")?.as_str()?.trim();
    if sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(sha.to_ascii_lowercase())
    } else {
        None
    }
}

/// Shared HTTP-based refresher. Walks every software_registry row whose
/// `version_source.method` matches `method`, builds a URL via `url_for`,
/// fetches it, parses the response with `extract_version`, and writes the
/// result. Per-row failures are logged at debug and skipped.
async fn refresh_via_http<UrlFn, ParseFn>(
    client: &reqwest::Client,
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
                // A persistent non-2xx (e.g. a 404 from a misrouted ref_kind, as
                // in #26) must be visible, not a silent skip that strands the row.
                tracing::warn!(software_id = %id, %url, status = %r.status(), "upstream non-2xx — latest_version not refreshed");
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
        // that live in both (codex, claude-code, …). Update
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_operation_failures_remain_soft() {
        let mut failures = Vec::new();
        record_operation_failure(
            &mut failures,
            false,
            "ff_git",
            "compose fleet upgrade wave",
            "synthetic failure",
        );
        assert!(failures.is_empty());
        assert_eq!(
            finish_run_once(2, 3, 1, 4, true, failures).unwrap(),
            AutoUpgradeRunOnceOutcome {
                refreshed_self_built: 2,
                enqueued: 3,
                rollouts_started: 1,
                skipped: 4,
                leader_self_upgrade_launched: true,
            }
        );
    }

    #[test]
    fn durable_operation_failures_report_partial_dispatch() {
        let mut failures = Vec::new();
        record_operation_failure(
            &mut failures,
            true,
            "ff_git",
            "compose fleet upgrade wave",
            "wave unavailable",
        );
        record_operation_failure(
            &mut failures,
            true,
            "codex",
            "enqueue upgrade plans",
            "queue unavailable",
        );
        let error = finish_run_once(2, 3, 1, 4, true, failures)
            .unwrap_err()
            .to_string();
        assert!(error.contains("refreshing 2 self-built"));
        assert!(error.contains("dispatching 3 upgrade task"));
        assert!(error.contains("starting 1 rollout"));
        assert!(error.contains("skipping 4 target"));
        assert!(error.contains("leader_self_upgrade_launched=true"));
        assert!(error.contains("2 failure(s)"));
        assert!(error.contains("ff_git: compose fleet upgrade wave: wave unavailable"));
        assert!(error.contains("codex: enqueue upgrade plans: queue unavailable"));
    }

    // The same-commit / prefix-length regression guard moved to
    // ff_core::build_version::tests (same_commit_is_hex_guarded_and_prefix_agnostic
    // + is_same_version_covers_every_path) when the predicate was consolidated.

    fn temp_git_repo() -> (tempfile::TempDir, String, String) {
        fn run(repo: &str, args: &[&str]) -> String {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }

        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().to_str().unwrap();
        run(repo, &["init", "-q"]);
        run(repo, &["config", "user.email", "tests@forgefleet.local"]);
        run(repo, &["config", "user.name", "ForgeFleet Tests"]);
        std::fs::write(dir.path().join("authority.txt"), "one\n").unwrap();
        run(repo, &["add", "authority.txt"]);
        run(repo, &["commit", "-qm", "first"]);
        let first = run(repo, &["rev-parse", "HEAD"]);
        std::fs::write(dir.path().join("authority.txt"), "two\n").unwrap();
        run(repo, &["commit", "-qam", "second"]);
        let second = run(repo, &["rev-parse", "HEAD"]);
        (dir, first, second)
    }

    #[test]
    fn self_built_sha_parser_requires_full_lowercase_commit_identity() {
        let valid = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(parse_full_lower_git_sha(valid).unwrap(), valid);
        assert!(parse_full_lower_git_sha(&valid.to_ascii_uppercase()).is_err());
        assert!(parse_full_lower_git_sha(&valid[..39]).is_err());
        assert!(parse_full_lower_git_sha("not-a-commit").is_err());
    }

    #[test]
    fn self_built_policy_seeds_equals_advances_and_holds_fail_closed() {
        let old = "1111111111111111111111111111111111111111";
        let candidate = "2222222222222222222222222222222222222222";
        assert_eq!(
            decide_self_built_update(false, None, candidate, None),
            SelfBuiltDecision::Seed
        );
        assert_eq!(
            decide_self_built_update(true, Some(candidate), candidate, None),
            SelfBuiltDecision::Unchanged
        );
        assert_eq!(
            decide_self_built_update(true, Some(old), candidate, Some(true)),
            SelfBuiltDecision::Advance
        );
        assert_eq!(
            decide_self_built_update(true, Some(old), candidate, Some(false)),
            SelfBuiltDecision::Hold
        );
        assert_eq!(
            decide_self_built_update(true, None, candidate, None),
            SelfBuiltDecision::Hold
        );
    }

    #[test]
    fn self_built_plan_resolves_legacy_prefix_and_rejects_rollback_or_bad_row() {
        let (dir, first, second) = temp_git_repo();
        let repo = dir.path().to_str().unwrap();
        let legacy = first[..10].to_string();
        let rows = vec![("ff_git".to_string(), Some(legacy))];
        let plan = plan_self_built_updates(repo, &second, &rows).unwrap();
        assert_eq!(plan.len(), 1);
        assert!(plan[0].change);

        let equal_legacy = vec![("ff_git".to_string(), Some(second[..10].to_string()))];
        let normalization = plan_self_built_updates(repo, &second, &equal_legacy).unwrap();
        assert!(
            normalization[0].change,
            "an equal legacy prefix must be normalized to full authority"
        );

        let equal_full = vec![("ff_git".to_string(), Some(second.clone()))];
        let no_change = plan_self_built_updates(repo, &second, &equal_full).unwrap();
        assert!(!no_change[0].change);

        let rollback_rows = vec![("ff_git".to_string(), Some(second.clone()))];
        assert!(plan_self_built_updates(repo, &first, &rollback_rows).is_err());

        let mixed = vec![
            ("ff_git".to_string(), Some(first)),
            ("forgefleetd_git".to_string(), Some("not-a-sha".into())),
        ];
        assert!(
            plan_self_built_updates(repo, &second, &mixed).is_err(),
            "one malformed row must hold the entire set"
        );
    }

    #[test]
    fn self_built_plan_rejects_a_divergent_commit() {
        let (dir, _, second) = temp_git_repo();
        let repo = dir.path().to_str().unwrap();
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["checkout", "--orphan", "divergent"])
            .output()
            .unwrap();
        assert!(output.status.success());
        std::fs::write(dir.path().join("authority.txt"), "divergent\n").unwrap();
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["commit", "-qam", "divergent"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let divergent = git_full_sha(repo, "HEAD^{commit}").unwrap();

        let rows = vec![("ff_git".to_string(), Some(second))];
        assert!(plan_self_built_updates(repo, &divergent, &rows).is_err());
    }

    #[test]
    fn self_built_snapshot_guard_prevents_atomic_two_row_partial_update() {
        let observed = vec![
            ("ff_git".to_string(), Some("1111111".to_string())),
            ("forgefleetd_git".to_string(), Some("1111111".to_string())),
        ];
        assert!(self_built_snapshot_unchanged(&observed, &observed));

        let mut concurrently_changed = observed.clone();
        concurrently_changed[1].1 = Some("2222222".to_string());
        assert!(!self_built_snapshot_unchanged(
            &observed,
            &concurrently_changed
        ));
    }

    #[test]
    fn self_built_fetch_unavailable_never_invents_a_candidate() {
        let (dir, _, _) = temp_git_repo();
        assert!(fetch_canonical_origin_main(dir.path().to_str().unwrap()).is_err());
    }

    #[test]
    fn canonical_remote_main_fetch_advances_a_stale_checkout() {
        fn git(repo: &std::path::Path, args: &[&str]) -> String {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }

        let root = tempfile::tempdir().unwrap();
        let remote = root.path().join("remote.git");
        let source = root.path().join("source");
        let checkout = root.path().join("checkout");
        std::fs::create_dir(&remote).unwrap();
        git(&remote, &["init", "--bare", "-q"]);
        std::fs::create_dir(&source).unwrap();
        git(&source, &["init", "-q", "-b", "main"]);
        git(&source, &["config", "user.email", "tests@forgefleet.local"]);
        git(&source, &["config", "user.name", "ForgeFleet Tests"]);
        std::fs::write(source.join("authority.txt"), "one\n").unwrap();
        git(&source, &["add", "authority.txt"]);
        git(&source, &["commit", "-qm", "first"]);
        git(
            &source,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&source, &["push", "-qu", "origin", "main"]);
        git(
            root.path(),
            &[
                "clone",
                "-q",
                "--branch",
                "main",
                remote.to_str().unwrap(),
                checkout.to_str().unwrap(),
            ],
        );

        std::fs::write(source.join("authority.txt"), "two\n").unwrap();
        git(&source, &["commit", "-qam", "second"]);
        let second = git(&source, &["rev-parse", "HEAD"]);
        git(&source, &["push", "-q", "origin", "main"]);

        let fetched = fetch_canonical_origin_main(checkout.to_str().unwrap()).unwrap();
        assert_eq!(
            fetched, second,
            "authority must be fetched from remote main"
        );
        assert_ne!(
            git(&checkout, &["rev-parse", "HEAD"]),
            fetched,
            "the stale working-tree HEAD must not be mistaken for authority"
        );
    }

    #[test]
    fn git_head_authority_keeps_full_sha_and_normalizes_case() {
        let sha = "ABCDEF0123456789ABCDEF0123456789ABCDEF01";
        let body = format!(r#"{{"sha":"{sha}"}}"#);
        assert_eq!(
            parse_git_head_response(&body).unwrap(),
            sha.to_ascii_lowercase()
        );
        assert!(parse_git_head_response(r#"{"sha":"abcdef0123"}"#).is_none());
        assert!(parse_git_head_response(r#"{"sha":"not-a-sha"}"#).is_none());
    }

    #[test]
    fn daemon_self_software_matches_the_family_const() {
        // is_daemon_self_software MUST agree with DAEMON_SELF_SOFTWARE for every
        // id — they are the two halves of the cross-wave self-kill guard (the
        // self-suicide gate vs. the wave singleton). If they drift, two `*_git`
        // waves can run concurrently against the same hosts and tear down each
        // other's in-flight build (feedback_wave_dispatcher_self_kill_race.md /
        // feedback_cross_family_wave_self_kill.md). This locks them together.
        for id in DAEMON_SELF_SOFTWARE {
            assert!(
                is_daemon_self_software(id),
                "{id} is in DAEMON_SELF_SOFTWARE but is_daemon_self_software says no"
            );
        }
        // The full known family is exactly these three — a new daemon-restarting
        // software id must be added to the const (and this list) deliberately.
        assert_eq!(
            DAEMON_SELF_SOFTWARE.to_vec(),
            vec!["ff_git", "forgefleetd_git", "forgefleet"]
        );
    }

    #[test]
    fn non_daemon_software_is_not_self() {
        // Tool upgrades (claude/codex/gh) do NOT restart forgefleetd, so
        // they must NOT be serialized as daemon-self or gated out of the leader.
        for id in ["claude-code", "codex_git", "gh", "ff", "forgefleetd", ""] {
            assert!(
                !is_daemon_self_software(id),
                "{id} wrongly flagged daemon-self"
            );
        }
    }

    #[test]
    fn expand_tilde_only_rewrites_leading_tilde_slash() {
        // Used to build the per-host repo path for the upgrade playbook. A bug
        // here (expanding a bare `~` or a mid-string `~`) would corrupt the build
        // command's cwd. Only a leading `~/` is a home reference.
        let home = std::env::var("HOME").unwrap_or_default();
        assert_eq!(
            expand_tilde("~/projects/forge-fleet"),
            format!("{home}/projects/forge-fleet")
        );
        // Absolute and relative paths pass through untouched.
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
        assert_eq!(expand_tilde("relative/path"), "relative/path");
        // A bare tilde or a non-leading tilde is NOT a home reference.
        assert_eq!(expand_tilde("~"), "~");
        assert_eq!(expand_tilde("/x/~/y"), "/x/~/y");
    }
}
