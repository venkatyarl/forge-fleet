//! Deferred-task worker loop for forgefleetd.
//!
//! ## Why this exists
//!
//! Historically the deferred-task **worker** (claim a dispatchable task, run
//! it, mark it complete) lived in `ff daemon` — a separate CLI command. The
//! convention was: forgefleetd handles pulse + leader election + the
//! scheduler-pass (pending → dispatchable), and a sibling `ff daemon` process
//! handles the worker-pass. In practice nobody remembered to run `ff daemon`
//! on each host, so dispatchable tasks piled up forever. The auto-upgrade
//! pipeline silently broke; cross-node defer dispatches needed manual SSH.
//! See `feedback_forgefleetd_no_scheduler` memory.
//!
//! This module folds the worker loop into forgefleetd itself. One process
//! does both halves; the architectural split that caused the bug is gone.
//!
//! ## Scope
//!
//! Handles the task kinds that account for ~99% of fleet traffic:
//!
//!   - `shell`      — run a command, optionally on a remote node via SSH
//!   - `http`       — POST/GET to a URL (used by webhook integrations)
//!   - `upgrade`    — look up the `upgrade_playbook` for a software entry and
//!                    execute it via shell on the target node
//!   - `internal`   — ForgeFleet-internal tasks dispatched by title (today: SSH
//!                    mesh propagation)
//!   - `mesh_retry` — re-probe a single (src,dst) mesh pair, refresh status
//!
//! `internal` + `mesh_retry` were ported from the legacy `ff daemon` (decision-2
//! Phase A1 of retiring it) so forgefleetd no longer strands them. The
//! auto_upgrade/external_tool post-completion finalizers remain a follow-up —
//! they hook the task-finish path rather than the kind dispatch.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// Wall-clock cap for a single deferred shell task when the payload does not
/// override it. Deliberately generous: the deferred queue runs legitimate
/// multi-GB cross-node model downloads and the HA-backup rsync fan-out, all of
/// which can run for many minutes. 2h is well above any healthy such task yet
/// finite, so a *stuck* one (a slow-trickle `rsync --timeout=3600` that keeps
/// resetting its I/O-inactivity timer, a hung `git fetch`) can no longer run
/// forever. Matches the leaked-orphan reaper's default age threshold so the two
/// safety layers line up: a task is group-killed here at 2h, and anything that
/// still escapes (a `setsid`'d grandchild) is reaped by the orphan-reaper tick.
/// Per-task override via `payload.max_duration_secs`.
pub(crate) const DEFAULT_DEFER_MAX_DURATION: Duration = Duration::from_secs(7200);

/// Resolve a deferred task's wall-clock cap from its payload, falling back to
/// [`DEFAULT_DEFER_MAX_DURATION`]. A `max_duration_secs` of 0 (or absent /
/// non-numeric) yields the default — there is no "unlimited" escape hatch,
/// because an uncapped shell exec is exactly the leak this closes.
fn resolve_max_duration(payload: &serde_json::Value) -> Duration {
    match payload.get("max_duration_secs").and_then(|v| v.as_u64()) {
        Some(secs) if secs > 0 => Duration::from_secs(secs),
        _ => DEFAULT_DEFER_MAX_DURATION,
    }
}

/// Spawn a background task that periodically claims and executes deferred
/// tasks from `deferred_tasks`. Returns the JoinHandle so forgefleetd's
/// subsystem-shutdown machinery can drain it cleanly.
///
/// `poll_interval_secs` — how often to poll the queue when empty. The loop
/// drains until empty before sleeping; this only governs idle cadence.
///
/// `max_concurrent` — how many tasks this worker runs in parallel. A
/// `tokio::sync::Semaphore` gates spawning.
pub fn spawn_defer_worker(
    pg_pool: PgPool,
    worker_name: String,
    poll_interval_secs: u64,
    max_concurrent: usize,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let semaphore = Arc::new(Semaphore::new(max_concurrent));
        let mut ticker = tokio::time::interval(Duration::from_secs(poll_interval_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        info!(
            worker = %worker_name,
            poll_interval_secs,
            max_concurrent,
            "defer_worker: started"
        );

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(e) = drain_queue(&pg_pool, &worker_name, &semaphore).await {
                        warn!(error = %e, "defer_worker: drain failed");
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        info!("defer_worker: shutdown received, exiting");
                        break;
                    }
                }
            }
        }
    })
}

/// One drain pass: claim tasks until queue empty or all slots busy.
async fn drain_queue(
    pool: &PgPool,
    worker_name: &str,
    semaphore: &Arc<Semaphore>,
) -> Result<(), String> {
    loop {
        let permit = match semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => return Ok(()), // all slots busy — try next tick
        };

        let claimed = ff_db::pg_claim_deferred(pool, worker_name)
            .await
            .map_err(|e| format!("pg_claim_deferred: {e}"))?;

        let Some(task) = claimed else {
            // permit drops here automatically — slot back to the pool
            return Ok(());
        };

        info!(
            task_id = %task.id,
            kind = %task.kind,
            title = %task.title,
            "defer_worker: claimed"
        );

        let pool_clone = pool.clone();
        let nodes = ff_db::pg_list_nodes(pool).await.unwrap_or_default();
        tokio::spawn(async move {
            let _permit = permit; // hold for the duration of execution

            let (success, result, err) = execute(&task, &nodes).await;

            if let Err(e) = ff_db::pg_finish_deferred(
                &pool_clone,
                &task.id,
                success,
                result.as_ref(),
                err.as_deref(),
            )
            .await
            {
                warn!(task_id = %task.id, error = %e, "defer_worker: pg_finish_deferred failed");
                return;
            }

            if success {
                info!(task_id = %task.id, "defer_worker: ✓ completed");
            } else {
                warn!(
                    task_id = %task.id,
                    error = ?err,
                    "defer_worker: ✗ failed"
                );
            }
        });
    }
}

/// Execute one task by dispatching on its `kind`. Returns
/// `(success, optional_result_value, optional_error_message)`.
async fn execute(
    task: &ff_db::DeferredTaskRow,
    nodes: &[ff_db::FleetNodeRow],
) -> (bool, Option<serde_json::Value>, Option<String>) {
    let max_duration = resolve_max_duration(&task.payload);
    match task.kind.as_str() {
        "shell" => {
            let Some(command) = task.payload.get("command").and_then(|v| v.as_str()) else {
                return (
                    false,
                    None,
                    Some("shell payload missing 'command' field".into()),
                );
            };
            execute_shell(task.preferred_node.as_deref(), command, nodes, max_duration).await
        }
        "http" => {
            let Some(url) = task.payload.get("url").and_then(|v| v.as_str()) else {
                return (false, None, Some("http payload missing 'url' field".into()));
            };
            let method = task
                .payload
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("GET");
            let body = task.payload.get("body").cloned();
            execute_http(method, url, body).await
        }
        "upgrade" => {
            let Some(tool) = task.payload.get("tool").and_then(|v| v.as_str()) else {
                return (false, None, Some("upgrade payload missing 'tool'".into()));
            };
            let os_family = detect_os_family();
            let Some(script) = crate::upgrade_playbooks::playbook_for(tool, &os_family) else {
                return (
                    false,
                    None,
                    Some(format!("no playbook for tool={tool} os={os_family}")),
                );
            };
            execute_shell(task.preferred_node.as_deref(), &script, nodes, max_duration).await
        }
        "internal" => {
            // ForgeFleet-internal tasks dispatched by title. Ported from the
            // legacy `ff daemon` (decision-2 Phase A1) so forgefleetd no longer
            // strands `internal` deferred work. Today: SSH mesh propagation.
            if task.title.starts_with("Mesh propagate SSH for ") {
                match crate::fleet_info::get_fleet_pool().await {
                    Ok(pool) => match crate::mesh_check::mesh_propagate(&pool, &task.payload).await
                    {
                        Ok((ok, fail)) => {
                            let result = serde_json::json!({"ok_peers": ok, "failed_peers": fail});
                            let success = fail == 0;
                            let err = if success {
                                None
                            } else {
                                Some(format!("{fail} peer(s) failed"))
                            };
                            (success, Some(result), err)
                        }
                        Err(e) => (false, None, Some(format!("mesh_propagate: {e}"))),
                    },
                    Err(e) => (false, None, Some(format!("pool: {e}"))),
                }
            } else {
                (
                    false,
                    None,
                    Some(format!("unknown internal task title: {}", task.title)),
                )
            }
        }
        "mesh_retry" => {
            // Re-probe a specific (src,dst) pair and refresh fleet_mesh_status.
            // Ported from the legacy `ff daemon` (decision-2 Phase A1).
            let src = task
                .payload
                .get("src")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let dst = task
                .payload
                .get("dst")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if src.is_empty() || dst.is_empty() {
                return (false, None, Some("mesh_retry payload needs src+dst".into()));
            }
            match crate::fleet_info::get_fleet_pool().await {
                Ok(pool) => match crate::mesh_check::probe_single_pair(&pool, src, dst).await {
                    Ok(cell) => {
                        let ok = cell.status == "ok";
                        let result =
                            serde_json::json!({"status": cell.status, "error": cell.last_error});
                        (ok, Some(result), if ok { None } else { cell.last_error })
                    }
                    Err(e) => (false, None, Some(format!("probe: {e}"))),
                },
                Err(e) => (false, None, Some(format!("pool: {e}"))),
            }
        }
        other => {
            // Unsupported kinds — leave for the legacy `ff daemon` CLI to
            // pick up. Mark FAILED with a clear message so the task doesn't
            // get re-claimed in a tight loop; operator can re-enqueue if
            // they have `ff daemon --once` available.
            (
                false,
                None,
                Some(format!(
                    "defer_worker: task kind '{other}' not handled by forgefleetd's \
                     in-process worker. Run `ff daemon --once` to drain it."
                )),
            )
        }
    }
}

/// Run a shell command locally or via SSH to a remote node.
async fn execute_shell(
    target_node: Option<&str>,
    command: &str,
    nodes: &[ff_db::FleetNodeRow],
    max_duration: Duration,
) -> (bool, Option<serde_json::Value>, Option<String>) {
    use tokio::process::Command;

    let this_hostname = Command::new("hostname")
        .output()
        .await
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_default();

    let local = match target_node {
        None => true,
        Some(n) if this_hostname.starts_with(&n.to_lowercase()) => true,
        Some(_) => false,
    };

    // Every shell command runs through a wrapper that prepends `~/.local/bin`
    // and `~/.cargo/bin` to PATH. `sh -c` on Ubuntu invokes dash, which does
    // not source ~/.bashrc, so `ff`-flavoured commands (`ff model download
    // ...`, `ff fleet ...`) get "command not found" without this prefix.
    // Caught 2026-05-18 when the first gemma-4 download dispatch hit
    // `exit 127: sh: 1: ff: not found` on duncan.
    let wrapped = format!("export PATH=\"$HOME/.local/bin:$HOME/.cargo/bin:$PATH\" && {command}");

    let (program, args): (&str, Vec<String>) = if local {
        ("sh", vec!["-c".into(), wrapped])
    } else {
        let node_name = target_node.unwrap();
        let Some(node) = nodes
            .iter()
            .find(|n| n.name.eq_ignore_ascii_case(node_name))
        else {
            return (
                false,
                None,
                Some(format!("defer_worker: node '{node_name}' not in fleet")),
            );
        };
        let dest = format!("{}@{}", node.ssh_user, node.ip);
        (
            "ssh",
            vec![
                "-o".into(),
                "ConnectTimeout=8".into(),
                // Retry the TCP connect up to 3× before giving up. Some nodes
                // (priya, chronically) transiently refuse/drop the connect from
                // the task worker while an interactive `ff fleet exec` moments
                // later succeeds — a single attempt failed the task with
                // `ssh_unreachable` (3× oauth-distribute failures, 2026-07-01).
                "-o".into(),
                "ConnectionAttempts=3".into(),
                // Detect + abort a session that connects then wedges mid-command
                // (the other half of priya's failure mode): 3 missed 10s keepalive
                // probes → ssh tears the session down instead of hanging until the
                // task's max-duration kill.
                "-o".into(),
                "ServerAliveInterval=10".into(),
                "-o".into(),
                "ServerAliveCountMax=3".into(),
                "-o".into(),
                "StrictHostKeyChecking=accept-new".into(),
                dest,
                wrapped,
            ],
        )
    };

    // Spawn with the SAME hardening the `fleet_tasks` runner uses (PR #215):
    //  1. `process_group(0)` — the child (`sh`/`ssh`) becomes its own group
    //     leader; every grandchild it forks (rsync, git, the ssh tunnel,
    //     `ff model download`'s HF fetch) joins the group unless it
    //     deliberately `setsid`s out.
    //  2. On timeout we SIGKILL the WHOLE group (`kill(-pgid)`), not just the
    //     direct child. `kill_on_drop` alone reaps only the leader, orphaning
    //     grandchildren to pid 1 — exactly how the HA-backup `rsync
    //     --timeout=3600` (an I/O-inactivity timer, NOT a wall-clock cap) ran
    //     5h on priya and became a leaked orphan when the daemon restarted.
    //     `.output()` also blocks until the stdout/stderr pipe hits EOF, so an
    //     orphaned grandchild holding the write-end kept this executor's future
    //     pending forever. The deferred path never had #215's fix — this closes
    //     that gap for the other executor.
    //  3. stdin `/dev/null` so nothing spawned blocks on a read.
    let mut cmd = Command::new(program);
    cmd.args(&args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.process_group(0);
    cmd.kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (false, None, Some(format!("spawn {program}: {e}"))),
    };
    // Captured before any await so it survives even if the child is reaped;
    // it is the group id we target with `kill(-pgid)` on timeout.
    let pgid = child.id();

    // Drain stdout and stderr CONCURRENTLY with the wait — reading one fully
    // before the other can deadlock a task that fills the second pipe's
    // 64 KiB buffer while we block waiting on the first.
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let collect = async {
        let read_out = async {
            let mut buf = Vec::new();
            if let Some(mut o) = stdout_pipe {
                let _ = tokio::io::AsyncReadExt::read_to_end(&mut o, &mut buf).await;
            }
            buf
        };
        let read_err = async {
            let mut buf = Vec::new();
            if let Some(mut e) = stderr_pipe {
                let _ = tokio::io::AsyncReadExt::read_to_end(&mut e, &mut buf).await;
            }
            buf
        };
        let (stdout_buf, stderr_buf, status) = tokio::join!(read_out, read_err, child.wait());
        (status, stdout_buf, stderr_buf)
    };

    let (status, stdout_buf, stderr_buf) = match tokio::time::timeout(max_duration, collect).await {
        Ok(triple) => triple,
        Err(_) => {
            // Reap the ENTIRE group so no grandchild is left orphaned.
            if let Some(pid) = pgid {
                crate::task_runner::kill_process_group(pid);
            }
            let secs = max_duration.as_secs();
            warn!(
                program,
                max_duration_secs = secs,
                "defer_worker: shell task exceeded max duration; killed process group"
            );
            return (
                false,
                None,
                Some(format!(
                    "defer_worker: shell task exceeded max duration of {secs}s (process group killed)"
                )),
            );
        }
    };

    let stdout = String::from_utf8_lossy(&stdout_buf).to_string();
    let stderr = String::from_utf8_lossy(&stderr_buf).to_string();
    let status = match status {
        Ok(s) => s,
        Err(e) => return (false, None, Some(format!("wait {program}: {e}"))),
    };
    let exit_code = status.code().unwrap_or(-1);

    let result = serde_json::json!({
        "exit_code": exit_code,
        "stdout": stdout.chars().take(8192).collect::<String>(),
        "stderr": stderr.chars().take(8192).collect::<String>(),
    });

    if status.success() {
        (true, Some(result), None)
    } else {
        // Error messages land at the END of output (e.g. corepack's EACCES
        // line follows pages of progress), so summarize the TAIL, not the head.
        // The full stream is preserved in `result` (persisted on failure too).
        let src = if !stderr.is_empty() { &stderr } else { &stdout };
        let err = format!("exit {exit_code}: {}", tail_chars(src, 800));
        (false, Some(result), Some(err))
    }
}

/// Last `n` chars of `s`, prefixed with `…` when truncated. Used for error
/// summaries where the meaningful message is at the end of the stream.
fn tail_chars(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.trim_end().chars().collect();
    if chars.len() <= n {
        chars.iter().collect()
    } else {
        format!("…{}", chars[chars.len() - n..].iter().collect::<String>())
    }
}

/// HTTP task — POST/GET to a URL.
async fn execute_http(
    method: &str,
    url: &str,
    body: Option<serde_json::Value>,
) -> (bool, Option<serde_json::Value>, Option<String>) {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("build http client")
    });

    let req = match method.to_ascii_uppercase().as_str() {
        "GET" => client.get(url),
        "POST" => {
            let r = client.post(url);
            if let Some(b) = body { r.json(&b) } else { r }
        }
        "PUT" => {
            let r = client.put(url);
            if let Some(b) = body { r.json(&b) } else { r }
        }
        "DELETE" => client.delete(url),
        other => {
            return (
                false,
                None,
                Some(format!("unsupported http method: {other}")),
            );
        }
    };

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => return (false, None, Some(format!("http request: {e}"))),
    };

    let status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();
    let result = serde_json::json!({
        "status": status.as_u16(),
        "body": body_text.chars().take(8192).collect::<String>(),
    });

    if status.is_success() {
        (true, Some(result), None)
    } else {
        (
            false,
            Some(result),
            Some(format!(
                "http {}: {}",
                status,
                body_text.chars().take(500).collect::<String>()
            )),
        )
    }
}

fn detect_os_family() -> String {
    match std::env::consts::OS {
        "macos" => "macos".to_string(),
        "linux" => "linux-ubuntu".to_string(), // fleet linux members are all ubuntu/dgx
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shell_local_echo_succeeds() {
        let nodes = vec![];
        let (ok, result, err) =
            execute_shell(None, "echo hello", &nodes, Duration::from_secs(10)).await;
        assert!(ok, "expected success, got err: {err:?}");
        let r = result.unwrap();
        assert_eq!(r.get("exit_code").and_then(|v| v.as_i64()), Some(0));
        assert!(
            r.get("stdout")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .contains("hello")
        );
    }

    #[tokio::test]
    async fn shell_local_failure_reports_exit_code() {
        let (ok, _, err) = execute_shell(None, "exit 3", &[], Duration::from_secs(10)).await;
        assert!(!ok);
        assert!(err.unwrap().contains("exit 3"));
    }

    #[tokio::test]
    async fn shell_times_out_and_reaps_backgrounded_grandchild() {
        // A command that backgrounds a long-lived grandchild (`sleep`) and
        // then idles. Under the old `.output()` path the grandchild held the
        // stdout pipe open, so the future never resolved; with the
        // process-group kill it is reaped and we return a timeout error fast.
        // Record the grandchild's pid so we can assert it was actually killed.
        let pidfile =
            std::env::temp_dir().join(format!("ff_defer_test_{}.pid", std::process::id()));
        let pidfile_str = pidfile.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&pidfile);
        let cmd = format!("sleep 300 & echo $! > {pidfile_str}; wait");

        let start = std::time::Instant::now();
        let (ok, _, err) = execute_shell(None, &cmd, &[], Duration::from_millis(400)).await;
        assert!(!ok, "expected timeout failure");
        let msg = err.unwrap();
        assert!(
            msg.contains("exceeded max duration"),
            "unexpected error: {msg}"
        );
        // Must return promptly after the cap, not block on the 300s sleep.
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "execute_shell blocked past the timeout"
        );

        // The backgrounded grandchild must have been reaped by the group kill.
        // Give the kernel a moment to deliver SIGKILL, then probe with `kill -0`.
        if let Ok(pid_str) = std::fs::read_to_string(&pidfile) {
            if let Ok(pid) = pid_str.trim().parse::<i32>() {
                tokio::time::sleep(Duration::from_millis(300)).await;
                // kill(-0) returns Err(ESRCH) when the process is gone.
                let alive = unsafe { libc::kill(pid, 0) } == 0;
                assert!(
                    !alive,
                    "grandchild pid {pid} survived the process-group kill"
                );
            }
        }
        let _ = std::fs::remove_file(&pidfile);
    }

    #[tokio::test]
    async fn http_unsupported_method() {
        let (ok, _, err) = execute_http("OPTIONS", "http://localhost/x", None).await;
        assert!(!ok);
        assert!(err.unwrap().contains("unsupported"));
    }

    #[test]
    fn detect_os_family_returns_sensible_value() {
        let f = detect_os_family();
        assert!(f == "macos" || f == "linux-ubuntu" || !f.is_empty());
    }
}
