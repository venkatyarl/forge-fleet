//! Distributed shell-task worker for `fleet_tasks` (V44).
//!
//! Every daemon runs a [`TaskRunner`] tick. It looks for `pending`
//! rows in `fleet_tasks` whose `requires_capability` set is satisfied
//! by this computer and either has no preferred computer or names this
//! one. It atomically claims the row via `FOR UPDATE SKIP LOCKED`,
//! runs the shell payload, heartbeats every 30s while running, and
//! marks `completed` / `failed`.
//!
//! The leader runs an additional handoff watchdog that finds rows
//! whose claimed worker has gone stale (`last_heartbeat_at` older than
//! 120s) and re-puts them on the queue with `handoff_count` bumped.
//! After [`MAX_HANDOFFS`] re-tries the row is marked permanently
//! `failed` so we don't loop forever on a poison task.
//!
//! Today only `task_type = "shell"` is dispatched. The payload shape:
//!
//! ```json
//! { "command": "echo hi", "shell": "/bin/bash" }
//! ```
//!
//! `shell` is optional and defaults to `/bin/bash` (or `sh` on
//! systems where bash is absent). Stdout + stderr are captured into
//! `result.stdout` / `result.stderr`; exit code into `result.exit`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// How long a `running` row may sit without a heartbeat before the
/// leader re-queues it.
const STUCK_AFTER_SECS: i64 = 120;
/// How often the worker bumps `last_heartbeat_at` while a task runs.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);
/// Max times a row can be handed off before we give up and fail it.
const MAX_HANDOFFS: i32 = 3;

/// Read a port-shaped row from `fleet_secrets`. Panics loudly if the
/// row is missing — better than silently falling back to a hardcoded
/// default and drifting away from operational truth. Operators can
/// always re-seed via the V50 migration or `ff secrets set`.
async fn read_port_secret(pg: &PgPool, key: &str) -> Result<u16, sqlx::Error> {
    let val: Option<String> = sqlx::query_scalar("SELECT value FROM fleet_secrets WHERE key = $1")
        .bind(key)
        .fetch_optional(pg)
        .await?;
    let s = val.ok_or_else(|| {
        sqlx::Error::Configuration(
            format!(
                "fleet_secrets is missing required key '{key}' — \
                 run V50 migration or `ff secrets set {key} <port>`"
            )
            .into(),
        )
    })?;
    s.parse::<u16>().map_err(|e| {
        sqlx::Error::Configuration(
            format!("fleet_secrets['{key}']='{s}' is not a u16 port: {e}").into(),
        )
    })
}

/// Errors that can occur while running the task runner.
#[derive(Debug, thiserror::Error)]
pub enum TaskRunnerError {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("task payload missing required field: {0}")]
    BadPayload(&'static str),
    #[error("unsupported task_type: {0}")]
    UnsupportedType(String),
}

/// Per-computer worker that polls `fleet_tasks` and runs shell payloads.
#[derive(Clone)]
pub struct TaskRunner {
    pg: PgPool,
    my_computer_id: uuid::Uuid,
    my_name: String,
    /// Capabilities advertised by this computer. The runner will only
    /// claim a task whose `requires_capability` ⊆ this set.
    my_capabilities: Arc<HashSet<String>>,
    /// Per-computer environment exposed to every shell payload — the
    /// canonical "things that come from the DB, not from source code"
    /// surface. Tasks reference these as `$FF_SOURCE_TREE`, `$FF_NODE`,
    /// `$FF_LEADER_NAME`, etc. Resolved once at startup; if the leader
    /// or source_tree_path changes the daemon is restarted.
    env: Arc<Vec<(String, String)>>,
}

impl TaskRunner {
    pub fn new(
        pg: PgPool,
        my_computer_id: uuid::Uuid,
        my_name: String,
        my_capabilities: HashSet<String>,
        env: Vec<(String, String)>,
    ) -> Self {
        Self {
            pg,
            my_computer_id,
            my_name,
            my_capabilities: Arc::new(my_capabilities),
            env: Arc::new(env),
        }
    }

    /// Resolve the standard env-var bag from the DB. Reads both this
    /// computer's row (for `FF_SOURCE_TREE`, `FF_NODE`, `FF_PRIMARY_IP`)
    /// and the leader's row (for `FF_LEADER_NAME`, `FF_LEADER_IP`,
    /// `FF_GATEWAY_URL`). Falls back gracefully when columns are NULL.
    pub async fn resolve_env_from_db(
        pg: &PgPool,
        my_name: &str,
    ) -> Result<Vec<(String, String)>, sqlx::Error> {
        let mut env = vec![("FF_NODE".to_string(), my_name.to_string())];

        if let Some(row) = sqlx::query(
            "SELECT primary_ip, source_tree_path, ssh_user, os_family
               FROM computers WHERE name = $1",
        )
        .bind(my_name)
        .fetch_optional(pg)
        .await?
        {
            let primary_ip: String = row.get("primary_ip");
            let stp: Option<String> = row.try_get("source_tree_path").ok();
            let ssh_user: String = row.get("ssh_user");
            let os_family: String = row.get("os_family");
            env.push(("FF_PRIMARY_IP".to_string(), primary_ip));
            env.push(("FF_SSH_USER".to_string(), ssh_user));
            env.push(("FF_OS_FAMILY".to_string(), os_family));
            if let Some(s) = stp.filter(|s| !s.is_empty()) {
                env.push(("FF_SOURCE_TREE".to_string(), s));
            }
        }

        if let Some(row) = sqlx::query(
            "SELECT ls.member_name, c.primary_ip
               FROM fleet_leader_state ls
               JOIN computers c ON c.id = ls.computer_id
              LIMIT 1",
        )
        .fetch_optional(pg)
        .await?
        {
            let leader_name: String = row.get("member_name");
            let leader_ip: String = row.get("primary_ip");
            let gateway_port = read_port_secret(pg, "port.gateway").await?;
            let gateway_url = format!("http://{leader_ip}:{gateway_port}");
            env.push(("FF_LEADER_NAME".to_string(), leader_name));
            env.push(("FF_LEADER_IP".to_string(), leader_ip));
            env.push(("FF_GATEWAY_URL".to_string(), gateway_url));
            env.push(("FF_GATEWAY_PORT".to_string(), gateway_port.to_string()));
        }
        Ok(env)
    }

    /// One worker tick — claim at most one ready task and run it.
    pub async fn tick_once(&self) -> Result<Option<uuid::Uuid>, TaskRunnerError> {
        // 1. Atomically claim a task whose capabilities we satisfy.
        let cap_array: Vec<String> = self.my_capabilities.iter().cloned().collect();
        let row = sqlx::query(
            r#"
            UPDATE fleet_tasks
               SET status                 = 'running',
                   claimed_by_computer_id = $1,
                   claimed_at             = NOW(),
                   started_at             = COALESCE(started_at, NOW()),
                   last_heartbeat_at      = NOW()
             WHERE id = (
               SELECT id FROM fleet_tasks
                WHERE status = 'pending'
                  AND task_type = 'shell'
                  AND (preferred_computer_id IS NULL
                       OR preferred_computer_id = $1)
                  AND requires_capability <@ to_jsonb($2::text[])
                ORDER BY priority DESC, created_at ASC
                  FOR UPDATE SKIP LOCKED
                LIMIT 1
             )
            RETURNING id, payload, summary
            "#,
        )
        .bind(self.my_computer_id)
        .bind(&cap_array)
        .fetch_optional(&self.pg)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let task_id: uuid::Uuid = row.get("id");
        let payload: Value = row.get("payload");
        let summary: String = row.get("summary");

        info!(task_id = %task_id, summary = %summary, "task claimed");

        // 2. Spawn a heartbeat ticker for the duration of the run.
        let pg_hb = self.pg.clone();
        let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let hb_task: JoinHandle<()> = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(HEARTBEAT_EVERY) => {
                        let _ = sqlx::query(
                            "UPDATE fleet_tasks SET last_heartbeat_at = NOW() WHERE id = $1",
                        )
                        .bind(task_id)
                        .execute(&pg_hb)
                        .await;
                    }
                    _ = &mut cancel_rx => break,
                }
            }
        });

        // 3. Run the payload — with FF_* env vars injected so tasks
        // never have to embed IPs, paths, or names in source.
        let outcome = run_shell_payload(&payload, &self.env).await;
        let _ = cancel_tx.send(());
        let _ = hb_task.await;

        // 4. Persist result.
        match outcome {
            Ok(result) => {
                let exit = result.get("exit").and_then(Value::as_i64).unwrap_or(-1);
                if exit == 0 {
                    sqlx::query(
                        "UPDATE fleet_tasks
                            SET status        = 'completed',
                                completed_at  = NOW(),
                                progress_pct  = 100.0,
                                result        = $1
                          WHERE id = $2",
                    )
                    .bind(&result)
                    .bind(task_id)
                    .execute(&self.pg)
                    .await?;
                    info!(task_id = %task_id, "task completed");
                } else {
                    sqlx::query(
                        "UPDATE fleet_tasks
                            SET status        = 'failed',
                                completed_at  = NOW(),
                                result        = $1,
                                error         = $2
                          WHERE id = $3",
                    )
                    .bind(&result)
                    .bind(format!("non-zero exit: {exit}"))
                    .bind(task_id)
                    .execute(&self.pg)
                    .await?;
                    warn!(task_id = %task_id, exit, "task failed (non-zero exit)");
                }
            }
            Err(e) => {
                sqlx::query(
                    "UPDATE fleet_tasks
                        SET status       = 'failed',
                            completed_at = NOW(),
                            error        = $1
                      WHERE id = $2",
                )
                .bind(format!("{e}"))
                .bind(task_id)
                .execute(&self.pg)
                .await?;
                warn!(task_id = %task_id, error = %e, "task failed (runner error)");
            }
        }

        Ok(Some(task_id))
    }

    /// Spawn the worker tick loop. Tries to claim one task every
    /// `interval_secs`. Exits when `shutdown` flips to true.
    pub fn spawn(self, interval_secs: u64, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        let interval = Duration::from_secs(interval_secs.max(2));
        tokio::spawn(async move {
            loop {
                if let Err(e) = self.tick_once().await {
                    debug!(error = %e, computer = %self.my_name, "task tick error");
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

/// Leader-only watchdog: detect `running` tasks whose worker has gone
/// quiet and put them back on the queue (or fail them after
/// [`MAX_HANDOFFS`]).
pub async fn handoff_stuck_tasks(pg: &PgPool) -> Result<usize, sqlx::Error> {
    // Demote stale-claim tasks back to pending unless we've already
    // handed off too many times — those become permanent failures.
    let demoted = sqlx::query(
        r#"
        UPDATE fleet_tasks
           SET status                 = 'pending',
               claimed_by_computer_id = NULL,
               claimed_at             = NULL,
               last_heartbeat_at      = NULL,
               handoff_count          = handoff_count + 1,
               handoff_reason         = 'heartbeat_stale',
               original_computer_id   = COALESCE(original_computer_id, claimed_by_computer_id)
         WHERE status = 'running'
           AND last_heartbeat_at < NOW() - make_interval(secs => $1::int)
           AND handoff_count < $2
        RETURNING id
        "#,
    )
    .bind(STUCK_AFTER_SECS as i32)
    .bind(MAX_HANDOFFS)
    .fetch_all(pg)
    .await?;

    let _ = sqlx::query(
        r#"
        UPDATE fleet_tasks
           SET status       = 'failed',
               completed_at = NOW(),
               error        = 'exceeded MAX_HANDOFFS retries'
         WHERE status = 'running'
           AND last_heartbeat_at < NOW() - make_interval(secs => $1::int)
           AND handoff_count >= $2
        "#,
    )
    .bind(STUCK_AFTER_SECS as i32)
    .bind(MAX_HANDOFFS)
    .execute(pg)
    .await?;

    Ok(demoted.len())
}

/// Convenience: spawn the leader watchdog as a background tick.
///
/// `my_name` is compared (case-insensitive) against
/// `fleet_leader_state.member_name`; only the elected leader actually
/// performs handoffs. Every daemon spawns this; the gate keeps it idle
/// on followers.
pub fn spawn_leader_watchdog(
    pg: PgPool,
    my_name: String,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // 60s tick — half the stuck threshold so detection latency stays bounded.
        let interval = Duration::from_secs(60);
        loop {
            let leader: Option<String> =
                sqlx::query_scalar("SELECT member_name FROM fleet_leader_state LIMIT 1")
                    .fetch_optional(&pg)
                    .await
                    .ok()
                    .flatten();
            if matches!(leader, Some(ref l) if l.eq_ignore_ascii_case(&my_name)) {
                match handoff_stuck_tasks(&pg).await {
                    Ok(n) if n > 0 => {
                        info!(handed_off = n, "task watchdog re-queued stale tasks");
                    }
                    Ok(_) => {}
                    Err(e) => warn!(error = %e, "task watchdog query failed"),
                }
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

/// Enqueue a single shell task. Used by the CLI and by the
/// `compose_aura_bootstrap` helper.
#[allow(clippy::too_many_arguments)]
pub async fn pg_enqueue_shell_task(
    pg: &PgPool,
    summary: &str,
    command: &str,
    capabilities: &[String],
    preferred_computer: Option<&str>,
    parent_task_id: Option<uuid::Uuid>,
    priority: i32,
    created_by_computer_id: Option<uuid::Uuid>,
) -> Result<uuid::Uuid, sqlx::Error> {
    let preferred_id: Option<uuid::Uuid> = if let Some(name) = preferred_computer {
        sqlx::query_scalar("SELECT id FROM computers WHERE name = $1")
            .bind(name)
            .fetch_optional(pg)
            .await?
    } else {
        None
    };

    let payload = json!({ "command": command });
    let caps = serde_json::Value::Array(
        capabilities
            .iter()
            .map(|c| Value::String(c.clone()))
            .collect(),
    );

    let id: uuid::Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO fleet_tasks (
            parent_task_id, task_type, summary, payload,
            priority, requires_capability, preferred_computer_id,
            created_by_computer_id
        )
        VALUES ($1, 'shell', $2, $3, $4, $5, $6, $7)
        RETURNING id
        "#,
    )
    .bind(parent_task_id)
    .bind(summary)
    .bind(&payload)
    .bind(priority)
    .bind(&caps)
    .bind(preferred_id)
    .bind(created_by_computer_id)
    .fetch_one(pg)
    .await?;

    Ok(id)
}

/// Run a `task_type=shell` payload via `/bin/bash -lc <command>`.
/// `env` is injected on top of the inherited daemon env — these are
/// the `FF_*` values resolved from the DB at worker startup.
async fn run_shell_payload(
    payload: &Value,
    env: &[(String, String)],
) -> Result<Value, TaskRunnerError> {
    let command = payload
        .get("command")
        .and_then(Value::as_str)
        .ok_or(TaskRunnerError::BadPayload("command"))?
        .to_string();

    let shell = payload
        .get("shell")
        .and_then(Value::as_str)
        .unwrap_or("/bin/bash")
        .to_string();

    let env_owned: Vec<(String, String)> = env.to_vec();

    // Run in a blocking thread so we don't tie up the runtime.
    let out = tokio::task::spawn_blocking(move || {
        let mut cmd = std::process::Command::new(&shell);
        cmd.arg("-lc").arg(&command);
        for (k, v) in &env_owned {
            cmd.env(k, v);
        }
        cmd.output()
    })
    .await
    .map_err(|e| TaskRunnerError::BadPayload(Box::leak(format!("join: {e}").into_boxed_str())))?
    .map_err(|e| TaskRunnerError::BadPayload(Box::leak(format!("spawn: {e}").into_boxed_str())))?;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let exit = out.status.code().unwrap_or(-1) as i64;

    Ok(json!({
        "exit": exit,
        "stdout": stdout.chars().take(8192).collect::<String>(),
        "stderr": stderr.chars().take(8192).collect::<String>(),
    }))
}

/// Compose the multi-step "bring `<target>` online" task graph atomically.
///
/// Every node-specific value (IPs, ssh user, role) and every fleet
/// constant (gateway port) is read from the DB at compose time — no
/// IPs, names, usernames, or port numbers in source. Ports come from
/// `fleet_secrets` (seeded by V50: `port.gateway`, `port.openclaw`, …)
/// so an operator can change one row to change a port fleet-wide
/// without a recompile.
///
/// One parent (compound, no shell) plus N children, one per
/// cooperative step. Children are sequenced by descending priority so
/// the leader picks them in order; for a true DAG with edges we'd add
/// a `depends_on` column to V44.
pub async fn compose_node_bootstrap(
    pg: &PgPool,
    target_name: &str,
    leader_computer_id: uuid::Uuid,
) -> Result<uuid::Uuid, sqlx::Error> {
    // ── 1. Pull everything we need from the DB. ──────────────────────────
    let target_row = sqlx::query(
        "SELECT name, primary_ip, all_ips, ssh_user, ssh_port, os_family
           FROM computers WHERE name = $1",
    )
    .bind(target_name)
    .fetch_optional(pg)
    .await?
    .ok_or_else(|| {
        sqlx::Error::RowNotFound // mapped by caller
    })?;

    let target_name: String = target_row.get("name");
    let target_primary_ip: String = target_row.get("primary_ip");
    let target_all_ips_json: serde_json::Value = target_row.get("all_ips");
    let target_ssh_user: String = target_row.get("ssh_user");
    let target_ssh_port: i32 = target_row.get("ssh_port");
    let target_os_family: String = target_row.get("os_family");

    // Flatten all_ips JSONB → Vec<String>. The column may be a list of
    // strings or a list of {ip,iface,kind,…} objects depending on the
    // writer's vintage; tolerate both.
    let target_ips: Vec<String> = match &target_all_ips_json {
        serde_json::Value::Array(arr) if !arr.is_empty() => arr
            .iter()
            .filter_map(|v| match v {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Object(o) => {
                    o.get("ip").and_then(|x| x.as_str()).map(String::from)
                }
                _ => None,
            })
            .collect(),
        _ => vec![target_primary_ip.clone()],
    };

    let leader_ip: String = sqlx::query_scalar("SELECT primary_ip FROM computers WHERE id = $1")
        .bind(leader_computer_id)
        .fetch_one(pg)
        .await?;
    // Single source of truth: fleet_secrets['port.gateway'] (V50).
    let gateway_port = read_port_secret(pg, "port.gateway").await?;
    let gateway_url = format!("http://{leader_ip}:{gateway_port}");

    // ── 2. Parent task. ──────────────────────────────────────────────────
    let parent_summary = format!("{target_name}: bring online via fleet cooperation");
    let parent: uuid::Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO fleet_tasks (
            task_type, summary, payload, priority, created_by_computer_id
        )
        VALUES ('compound', $1, '{}'::jsonb, 80, $2)
        RETURNING id
        "#,
    )
    .bind(&parent_summary)
    .bind(leader_computer_id)
    .fetch_one(pg)
    .await?;

    let port_arg = if target_ssh_port == 22 {
        String::new()
    } else {
        format!(" -p {target_ssh_port}")
    };
    let ssh_target_for_iter = format!("{target_ssh_user}@$ip");
    let ssh_target_primary = format!("{target_ssh_user}@{target_primary_ip}");

    // ── Step 1: probe every known IP. ────────────────────────────────────
    let ip_list = target_ips.join(" ");
    let step1 = format!(
        "set -e; for ip in {ip_list}; do \
           echo \"== $ip ==\"; \
           timeout 5 ssh{port_arg} -o BatchMode=yes -o StrictHostKeyChecking=accept-new \
             {ssh_target_for_iter} 'echo ok && uname -srvmo' || echo 'unreachable'; \
         done"
    );
    pg_enqueue_shell_task(
        pg,
        &format!("{target_name}/1: ssh-probe all known IPs"),
        &step1,
        &["leader".to_string()],
        None,
        Some(parent),
        90,
        Some(leader_computer_id),
    )
    .await?;

    // ── Step 2: leader runs the bootstrap script. ────────────────────────
    let step2 = format!(
        "ssh{port_arg} -o BatchMode=yes {ssh_target_primary} \
         \"curl -fsSL '{gateway_url}/onboard/bootstrap.sh\
?name={target_name}&ip={target_primary_ip}&ssh_user={target_ssh_user}&role=builder&runtime=auto' \
         | sudo bash\""
    );
    pg_enqueue_shell_task(
        pg,
        &format!("{target_name}/2: install forgefleetd via gateway bootstrap"),
        &step2,
        &["leader".to_string()],
        None,
        Some(parent),
        85,
        Some(leader_computer_id),
    )
    .await?;

    // ── Step 3: verify forgefleetd active. ───────────────────────────────
    // Unit names are project-fixed deploy artifacts (see deploy/linux/
    // forgefleet.service and revive.rs fallback chain) — keeping them
    // here is consistent with treating them as constants. `grep -qx`
    // (exact line match) avoids the bug where 'inactive' matches.
    // macOS uses launchctl rather than systemctl; gate the check.
    let step3 = if target_os_family == "macos" {
        format!(
            "ssh{port_arg} -o BatchMode=yes {ssh_target_primary} \
             'launchctl list | grep -E \"com\\.forgefleet\\.(forgefleetd|daemon)\" >/dev/null'"
        )
    } else {
        format!(
            "ssh{port_arg} -o BatchMode=yes {ssh_target_primary} \
             'systemctl --user is-active forgefleetd.service \
                || systemctl --user is-active forgefleet-node.service \
                || systemctl --user is-active forgefleet-daemon.service \
                || systemctl --user is-active forgefleet-agent.service' \
             | grep -qx active"
        )
    };
    pg_enqueue_shell_task(
        pg,
        &format!("{target_name}/3: verify forgefleetd running on {target_name}"),
        &step3,
        &["leader".to_string()],
        None,
        Some(parent),
        80,
        Some(leader_computer_id),
    )
    .await?;

    // ── Step 4: confirm online via ff (any peer with ff). ────────────────
    let step4 = format!(
        "for i in $(seq 1 30); do \
           if ff fleet health 2>/dev/null | awk '$1 == \"{target_name}\" {{print $3}}' \
              | grep -qx online; then \
             echo '{target_name} online in fleet health'; exit 0; \
           fi; sleep 2; \
         done; echo 'timeout: {target_name} still not online after 60s'; exit 1"
    );
    pg_enqueue_shell_task(
        pg,
        &format!("{target_name}/4: confirm {target_name} shows online in ff fleet health"),
        &step4,
        &["ff".to_string()],
        None,
        Some(parent),
        75,
        Some(leader_computer_id),
    )
    .await?;

    Ok(parent)
}

/// Compose a wave-based fleet-upgrade graph for `software_id`.
///
/// **Why waves.** A node cannot reliably restart its own daemon from a
/// task running inside that daemon — the supervisor (`launchd` on
/// macOS, `systemd --user` on Linux) sends `SIGKILL` to the whole
/// process group when it kills the unit, so the restart command takes
/// the task itself down with it; the task is left in `running` state,
/// the watchdog re-queues it 120s later, and the loop repeats until
/// `MAX_HANDOFFS` retries are exhausted.
///
/// The fix is structural: a peer SSHs into the target and runs the
/// upgrade remotely. The executor's daemon stays alive throughout (it
/// only spawns an `ssh` subshell); the target's daemon dies after its
/// session ends, but by then the executor has captured stdout / stderr
/// / exit and marked the row `completed`.
///
/// **Wave layout.** Every non-leader online member with a row in
/// `computer_software` for this `software_id` is a target. Targets are
/// chunked into waves of `fanout`. Wave 0 has the highest priority so
/// the workers pick them first; subsequent waves descend by 3 in
/// priority. Tasks have an empty `requires_capability` set, so any
/// online worker can claim them — the fleet self-parallelizes once
/// Wave 0 has expanded the worker pool from 1 (just the leader) to
/// `1 + fanout`. The leader is intentionally excluded from this graph
/// (cycles back into the suicide-restart problem); restart it
/// manually after the fleet is upgraded, e.g. by SSHing from any
/// upgraded peer.
///
/// **No hardcoded fleet data.** Every per-target value (ssh_user,
/// primary_ip, ssh_port, the playbook command itself) is read from
/// the DB at compose time via
/// [`crate::auto_upgrade::resolve_upgrade_plans`].
pub async fn compose_fleet_upgrade_wave(
    pg: &PgPool,
    software_id: &str,
    fanout: usize,
    leader_computer_id: uuid::Uuid,
) -> Result<uuid::Uuid, sqlx::Error> {
    use crate::auto_upgrade::resolve_upgrade_plans;

    let fanout = fanout.max(1);

    // 1. Resolve playbook for every member that has this software.
    //    `upgrade_available_only=false` means we get ALL targets, not
    //    just drift candidates — the dispatcher itself is the trigger.
    let (plans, skipped) = resolve_upgrade_plans(pg, software_id, None, false)
        .await
        .map_err(|e| sqlx::Error::Configuration(format!("resolve_upgrade_plans: {e}").into()))?;

    if !skipped.is_empty() {
        tracing::info!(
            count = skipped.len(),
            "fleet-upgrade-wave: members skipped (no playbook key matched)"
        );
    }

    // 2. Pull leader name; non-leader plans become wave targets.
    let leader_name: Option<String> =
        sqlx::query_scalar("SELECT name FROM computers WHERE id = $1")
            .bind(leader_computer_id)
            .fetch_optional(pg)
            .await?;
    let leader_lower = leader_name
        .as_deref()
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();

    let mut wave_targets: Vec<WaveTarget> = Vec::with_capacity(plans.len());
    for plan in plans {
        if plan.computer_name.eq_ignore_ascii_case(&leader_lower) {
            continue;
        }
        let row: Option<(String, String, i32)> =
            sqlx::query_as("SELECT ssh_user, primary_ip, ssh_port FROM computers WHERE name = $1")
                .bind(&plan.computer_name)
                .fetch_optional(pg)
                .await?;
        let Some((ssh_user, primary_ip, ssh_port)) = row else {
            tracing::warn!(
                computer = %plan.computer_name,
                "fleet-upgrade-wave: no computers row, skipping"
            );
            continue;
        };
        wave_targets.push(WaveTarget {
            target_name: plan.computer_name.clone(),
            ssh_user,
            primary_ip,
            ssh_port,
            playbook_command: plan.command,
        });
    }

    if wave_targets.is_empty() {
        return Err(sqlx::Error::Configuration(
            format!("no non-leader targets for software_id='{software_id}'").into(),
        ));
    }

    // 3. Parent task for the whole graph.
    let wave_count = wave_targets.len().div_ceil(fanout);
    let parent_summary = format!(
        "fleet-upgrade-wave: {software_id} ({n} target(s), fanout {fanout}, {wave_count} wave(s))",
        n = wave_targets.len()
    );
    let parent: uuid::Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO fleet_tasks (
            task_type, summary, payload, priority, created_by_computer_id
        )
        VALUES ('compound', $1, $2, 90, $3)
        RETURNING id
        "#,
    )
    .bind(&parent_summary)
    .bind(serde_json::json!({
        "software_id": software_id,
        "fanout": fanout,
        "wave_count": wave_count,
        "target_count": wave_targets.len(),
    }))
    .bind(leader_computer_id)
    .fetch_one(pg)
    .await?;

    // 4. Chunk targets into waves; one shell task per (wave, target).
    //    Empty capability set = any worker may claim, so the fleet
    //    self-parallelizes as Wave 0 expands the worker pool.
    for (wave_idx, chunk) in wave_targets.chunks(fanout).enumerate() {
        let priority = 95i32.saturating_sub((wave_idx as i32).saturating_mul(3));
        for t in chunk {
            let port_arg = if t.ssh_port == 22 {
                String::new()
            } else {
                format!(" -p {}", t.ssh_port)
            };
            // The playbook is sent over ssh stdin to a remote login
            // bash. Quoting `'FF_PLAYBOOK_EOF'` prevents the local
            // shell from expanding `$` / backticks in the playbook —
            // remote bash sees the script verbatim. `ssh -T`
            // suppresses pseudo-tty allocation so the heredoc flows
            // cleanly.
            let command = format!(
                "set -e\n\
                 echo \"== upgrading {target} via ssh from $(hostname) ==\"\n\
                 ssh -T{port_arg} -o BatchMode=yes -o StrictHostKeyChecking=accept-new \
                     {ssh_user}@{primary_ip} bash -l <<'FF_PLAYBOOK_EOF'\n\
                 {playbook}\n\
                 FF_PLAYBOOK_EOF\n",
                target = t.target_name,
                port_arg = port_arg,
                ssh_user = t.ssh_user,
                primary_ip = t.primary_ip,
                playbook = t.playbook_command,
            );

            pg_enqueue_shell_task(
                pg,
                &format!(
                    "fleet-upgrade-wave/wave{wave_idx}: {software_id} on {}",
                    t.target_name
                ),
                &command,
                &[],
                None,
                Some(parent),
                priority,
                Some(leader_computer_id),
            )
            .await?;
        }
    }

    Ok(parent)
}

/// One row in [`compose_fleet_upgrade_wave`]'s working set.
struct WaveTarget {
    target_name: String,
    ssh_user: String,
    primary_ip: String,
    ssh_port: i32,
    playbook_command: String,
}
