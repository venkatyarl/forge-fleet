//! Daemon, defer worker, and deferred task execution engine.

use std::time::Duration;

use anyhow::Result;

use crate::{CYAN, RED, RESET, YELLOW};

async fn probe_online_nodes(nodes: &[ff_db::FleetNodeRow]) -> Vec<String> {
    use tokio::net::TcpStream;
    use tokio::time::{Duration as TokDuration, timeout};
    // KNOWN LIMITATION: this probes SSH port 22, which means a node with its
    // OS up but its `ff daemon` dead will still appear online. As a result, the
    // Redis `fleet:node_online` publish only fires on OS-level transitions, not
    // daemon-level transitions. Proper fix would be a Redis heartbeat key per
    // daemon (TTL 30s) that workers refresh; the scheduler would read those
    // keys instead of SSH-probing. Out of scope for now — the 15s defer poll
    // catches daemon-only restarts within one cycle.
    let mut handles = Vec::new();
    for n in nodes {
        let name = n.name.clone();
        let ip = n.ip.clone();
        let handle: tokio::task::JoinHandle<Option<String>> = tokio::spawn(async move {
            let addr = format!("{ip}:22");
            match timeout(TokDuration::from_secs(3), TcpStream::connect(&addr)).await {
                Ok(Ok(_)) => Some(name),
                _ => None,
            }
        });
        handles.push(handle);
    }
    let mut online = Vec::new();
    for h in handles {
        if let Ok(Some(name)) = h.await {
            online.push(name);
        }
    }
    online
}

/// Execute a single deferred task. Returns (success, result_json, error).
///
/// `workspace` — optional sub-agent workspace dir. Shell tasks use this
/// as `cwd` when running locally; SSH-dispatched shell tasks ignore it
/// (the remote node sets its own cwd). Future `agent_run` kind will use
/// this for checkpoint/scratch isolation across concurrent sub-agents.
// Parse shorthand duration specs like "1h", "30m", "2d", "45s".
async fn execute_deferred(
    task: &ff_db::DeferredTaskRow,
    nodes: &[ff_db::FleetNodeRow],
    workspace: Option<&std::path::Path>,
) -> (bool, Option<serde_json::Value>, Option<String>) {
    match task.kind.as_str() {
        "shell" => {
            let command = match task.payload.get("command").and_then(|v| v.as_str()) {
                Some(c) => c,
                None => {
                    return (
                        false,
                        None,
                        Some("shell payload missing 'command' field".into()),
                    );
                }
            };
            // preferred_node tells us where to run. If None, run locally.
            let target = task.preferred_node.as_deref();
            execute_shell(target, command, nodes, workspace).await
        }
        "http" => {
            let url = match task.payload.get("url").and_then(|v| v.as_str()) {
                Some(u) => u,
                None => return (false, None, Some("http payload missing 'url' field".into())),
            };
            let method = task
                .payload
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("GET");
            let body = task.payload.get("body").cloned();
            execute_http(method, url, body).await
        }
        "internal" => {
            // Internal ForgeFleet tasks dispatched by title. Requires DB pool —
            // we open a short-lived one here so execute_deferred stays pure.
            if task.title.starts_with("Mesh propagate SSH for ") {
                match ff_agent::fleet_info::get_fleet_pool().await {
                    Ok(pool) => match ff_agent::mesh_check::mesh_propagate(&pool, &task.payload)
                        .await
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
        "upgrade" => {
            // Run the tool-specific upgrade playbook.
            let tool = match task.payload.get("tool").and_then(|v| v.as_str()) {
                Some(t) => t,
                None => return (false, None, Some("upgrade payload missing 'tool'".into())),
            };
            let os_family = crate::helpers::detect_os_family();
            let script = match ff_agent::upgrade_playbooks::playbook_for(tool, &os_family) {
                Some(s) => s,
                None => {
                    return (
                        false,
                        None,
                        Some(format!("no playbook for tool={tool} os={os_family}")),
                    );
                }
            };
            let target = task.preferred_node.as_deref();
            execute_shell(target, &script, nodes, workspace).await
        }
        "mesh_retry" => {
            // Re-probe a specific (src, dst) pair and refresh fleet_mesh_status.
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
            match ff_agent::fleet_info::get_fleet_pool().await {
                Ok(pool) => match ff_agent::mesh_check::probe_single_pair(&pool, src, dst).await {
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
        other => (false, None, Some(format!("unknown task kind: {other}"))),
    }
}

/// Threshold for auto-upgrade `consecutive_failures` → `upgrade_blocked`.
/// Hit this count and the row stops getting auto-retried until an operator
/// clears the block manually. 3 = "transient flake retried twice, third
/// strike means there's a real problem".
const AUTO_UPGRADE_FAILURE_THRESHOLD: i32 = 3;

/// Post-completion hook for `meta.auto_upgrade` deferred tasks.
///
/// Runs whether the task succeeded or failed. Always:
///   1a. On success: writes `installed_version=$latest_version` (authoritative —
///       don't wait for the next beat to refresh it), resets
///       `consecutive_failures=0`, clears `last_upgrade_error`, sets `status='ok'`.
///   1b. On failure: bumps `consecutive_failures` and sets
///       `last_upgrade_error=$err`. If the bumped count reaches
///       `AUTO_UPGRADE_FAILURE_THRESHOLD`, flips `status='upgrade_blocked'`
///       so the next tick won't redispatch; otherwise sets
///       `status='upgrade_available'` for retry.
///   2. Publishes `fleet.events.software.upgrade_completed.{computer}` on NATS.
///   3. Fires a Telegram message via fleet_secrets (no-op if not configured).
async fn finalize_upgrade_event(
    pool: &sqlx::PgPool,
    task: &ff_db::DeferredTaskRow,
    ok: bool,
    meta: &serde_json::Value,
    err: Option<&str>,
) {
    let software_id = meta
        .get("software_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let display_name = meta
        .get("display_name")
        .and_then(|v| v.as_str())
        .unwrap_or(software_id);
    let computer = meta.get("computer").and_then(|v| v.as_str()).unwrap_or("");
    let old_version = meta
        .get("old_version")
        .and_then(|v| v.as_str())
        .unwrap_or("-");
    let latest_version = meta
        .get("latest_version")
        .and_then(|v| v.as_str())
        .unwrap_or("-");

    // 1. Record outcome.
    if ok {
        // Success path — write authoritative installed_version, reset counter.
        // Skip the installed_version update if meta didn't carry a usable
        // latest_version (placeholder "-" or empty); fall back to the next
        // beat's collector-reported version.
        let installed_version_to_write = if latest_version == "-" || latest_version.is_empty() {
            None
        } else {
            Some(latest_version.to_string())
        };
        let _ = sqlx::query(
            "UPDATE computer_software cs
                SET status               = 'ok',
                    installed_version    = COALESCE($3, cs.installed_version),
                    last_upgraded_at     = NOW(),
                    last_checked_at      = NOW(),
                    last_upgrade_error   = NULL,
                    consecutive_failures = 0
               FROM computers c
              WHERE cs.computer_id = c.id
                AND cs.software_id = $1
                AND LOWER(c.name)  = LOWER($2)",
        )
        .bind(software_id)
        .bind(computer)
        .bind(installed_version_to_write)
        .execute(pool)
        .await;
    } else {
        // Failure path — bump counter, flip to upgrade_blocked at threshold.
        // Only triggers when status is currently 'upgrading' (i.e. we're
        // finalizing a real dispatched run, not a phantom).
        let truncated_err = err.map(|s| s.chars().take(2000).collect::<String>());
        let _ = sqlx::query(
            "UPDATE computer_software cs
                SET consecutive_failures = cs.consecutive_failures + 1,
                    last_upgrade_error   = $3,
                    last_checked_at      = NOW(),
                    status = CASE
                        WHEN cs.consecutive_failures + 1 >= $4
                        THEN 'upgrade_blocked'
                        ELSE 'upgrade_available'
                    END
               FROM computers c
              WHERE cs.computer_id = c.id
                AND cs.software_id = $1
                AND LOWER(c.name)  = LOWER($2)
                AND cs.status      = 'upgrading'",
        )
        .bind(software_id)
        .bind(computer)
        .bind(truncated_err)
        .bind(AUTO_UPGRADE_FAILURE_THRESHOLD)
        .execute(pool)
        .await;
    }

    // 2. NATS event — everyone subscribed to fleet.events.software.> sees it.
    let status_word = if ok { "success" } else { "failed" };
    let subject = format!(
        "fleet.events.software.upgrade_completed.{}",
        if computer.is_empty() {
            "unknown"
        } else {
            computer
        },
    );
    let payload = serde_json::json!({
        "software_id":    software_id,
        "display_name":   display_name,
        "computer":       computer,
        "old_version":    old_version,
        "latest_version": latest_version,
        "status":         status_word,
        "error":          err,
        "defer_id":       task.id,
        "ts":             chrono::Utc::now().to_rfc3339(),
    });
    ff_agent::nats_client::publish_json(subject, &payload).await;

    // 3. Telegram (best-effort — never crashes the worker).
    let title = if ok {
        format!("✅ ForgeFleet upgraded {display_name} on {computer}")
    } else {
        format!("❌ ForgeFleet upgrade failed: {display_name} on {computer}")
    };
    let body = if ok {
        format!("{old_version} → {latest_version}\nNo operator action needed.",)
    } else {
        // Read the post-update consecutive_failures count so the message
        // tells the operator whether more retries are coming or the row
        // just got blocked.
        let count: i32 = sqlx::query_scalar::<_, i32>(
            "SELECT cs.consecutive_failures
               FROM computer_software cs
               JOIN computers c ON c.id = cs.computer_id
              WHERE cs.software_id = $1
                AND LOWER(c.name)  = LOWER($2)
              LIMIT 1",
        )
        .bind(software_id)
        .bind(computer)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(0);
        let tail = if count >= AUTO_UPGRADE_FAILURE_THRESHOLD {
            format!(
                "Hit {AUTO_UPGRADE_FAILURE_THRESHOLD} consecutive failures — \
                 status flipped to upgrade_blocked. Auto-retry stopped. \
                 Clear with: ff software auto-upgrade-run-once after fixing the root cause."
            )
        } else {
            format!(
                "Failure {count}/{AUTO_UPGRADE_FAILURE_THRESHOLD} — will retry on next hourly tick."
            )
        };
        format!(
            "Tried to bump {old_version} → {latest_version}\nerror: {}\n{tail}",
            err.unwrap_or("(unknown)"),
        )
    };
    if let Err(e) = ff_agent::telegram::send_telegram_from_secrets(pool, &title, &body).await {
        tracing::warn!(error = %e, software_id, computer, "telegram send failed");
    }
}

/// Best-effort register an external tool as an MCP stdio server in the
/// local `.mcp.json` config. The config is searched in the current working
/// directory first, then the user's home directory.
async fn register_mcp_server(tool_id: &str, server_command: &str) -> anyhow::Result<()> {
    let candidates = [
        std::path::PathBuf::from(".mcp.json"),
        dirs::home_dir()
            .map(|h| h.join(".mcp.json"))
            .unwrap_or_default(),
    ];

    let path = candidates.iter().find(|p| p.exists()).cloned();
    let path = match path {
        Some(p) => p,
        None => candidates[0].clone(), // create in cwd
    };

    let mut config: serde_json::Value = if path.exists() {
        let text = tokio::fs::read_to_string(&path).await?;
        serde_json::from_str(&text).unwrap_or_else(|_| serde_json::json!({ "mcpServers": {} }))
    } else {
        serde_json::json!({ "mcpServers": {} })
    };

    let servers = config
        .get_mut("mcpServers")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| anyhow::anyhow!(".mcp.json missing mcpServers object"))?;

    // Parse command into command + args (simple whitespace split).
    let parts: Vec<&str> = server_command.split_whitespace().collect();
    let (cmd, args) = parts.split_first().unwrap_or((&"", &[]));

    servers.insert(
        tool_id.to_string(),
        serde_json::json!({
            "command": cmd,
            "args": args,
            "type": "stdio",
        }),
    );

    let text = serde_json::to_string_pretty(&config)?;
    tokio::fs::write(&path, text).await?;

    Ok(())
}

/// Post-completion hook for `meta.external_tool` deferred tasks.
///
/// Runs whether the task succeeded or failed. Flips
/// `computer_external_tools.status` from `'installing'` / `'upgrading'`
/// to `'ok'` (success) or `'install_failed'` (failure), and makes a
/// best-effort attempt to parse `installed_version` / `install_path`
/// out of the task stdout. Also handles MCP auto-registration.
async fn finalize_external_tool_event(
    pool: &sqlx::PgPool,
    task: &ff_db::DeferredTaskRow,
    ok: bool,
    meta: &serde_json::Value,
    err: Option<&str>,
) {
    let tool_id = meta.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let display_name = meta
        .get("display_name")
        .and_then(|v| v.as_str())
        .unwrap_or(tool_id);
    let computer = meta.get("computer").and_then(|v| v.as_str()).unwrap_or("");
    let old_version = meta
        .get("old_version")
        .and_then(|v| v.as_str())
        .unwrap_or("-");
    let latest_version = meta
        .get("latest_version")
        .and_then(|v| v.as_str())
        .unwrap_or("-");

    // Best-effort: extract installed_version + install_path from task stdout.
    // The result JSON written by pg_finish_deferred stores the shell result
    // under `result` with `stdout`/`stderr`/`exit_code`.
    let stdout = task
        .result
        .as_ref()
        .and_then(|r| r.get("stdout"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Matches "installed X.Y.Z" / "version X.Y.Z" / "v1.2.3" patterns.
    let version_guess: Option<String> = stdout.lines().rev().find_map(|line| {
        let l = line.to_lowercase();
        if l.contains("installed") || l.contains("version") || l.contains("updated") {
            line.split_whitespace()
                .rev()
                .find(|tok| {
                    let s = tok.trim_start_matches('v');
                    s.chars()
                        .next()
                        .map(|c| c.is_ascii_digit())
                        .unwrap_or(false)
                })
                .map(|s| s.trim_start_matches('v').to_string())
        } else {
            None
        }
    });

    // Matches "Installing to /path/to/bin" or "/home/.../bin/<cli>".
    let path_guess: Option<String> = stdout.lines().rev().find_map(|line| {
        line.strip_prefix("Installing to ")
            .map(|rest| rest.trim().to_string())
    });

    let new_status = if ok { "ok" } else { "install_failed" };

    let register_as_mcp = meta
        .get("register_as_mcp")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mcp_server_command = meta
        .get("mcp_server_command")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());

    let mut mcp_registered = false;
    if ok
        && register_as_mcp
        && let Some(cmd) = mcp_server_command
    {
        match register_mcp_server(tool_id, cmd).await {
            Ok(_) => {
                mcp_registered = true;
                tracing::info!(tool_id, computer, "MCP auto-registration succeeded");
            }
            Err(e) => {
                tracing::warn!(tool_id, computer, error = %e, "MCP auto-registration failed");
            }
        }
    }

    let _ = sqlx::query(
        "UPDATE computer_external_tools cet
            SET status = $1,
                last_upgraded_at = CASE WHEN $1 = 'ok' THEN NOW() ELSE last_upgraded_at END,
                last_checked_at  = NOW(),
                installed_version = COALESCE($4, cet.installed_version),
                install_path      = COALESCE($5, cet.install_path),
                last_error        = CASE WHEN $1 = 'ok' THEN NULL ELSE $6 END,
                mcp_registered    = CASE WHEN $7 THEN true ELSE mcp_registered END
           FROM computers c
          WHERE cet.computer_id = c.id
            AND cet.tool_id     = $2
            AND LOWER(c.name)   = LOWER($3)",
    )
    .bind(new_status)
    .bind(tool_id)
    .bind(computer)
    .bind(version_guess.as_deref())
    .bind(path_guess.as_deref())
    .bind(err)
    .bind(mcp_registered)
    .execute(pool)
    .await;

    // NATS event on the same subject tree as software upgrades so dashboards
    // can subscribe to `fleet.events.software.>` and pick both up.
    let status_word = if ok { "success" } else { "failed" };
    let subject = format!(
        "fleet.events.external_tools.install_completed.{}",
        if computer.is_empty() {
            "unknown"
        } else {
            computer
        },
    );
    let payload = serde_json::json!({
        "tool_id":        tool_id,
        "display_name":   display_name,
        "computer":       computer,
        "old_version":    old_version,
        "latest_version": latest_version,
        "status":         status_word,
        "error":          err,
        "defer_id":       task.id,
        "ts":             chrono::Utc::now().to_rfc3339(),
    });
    ff_agent::nats_client::publish_json(subject, &payload).await;
}

/// Wrap a user shell command so any `&`-spawned children survive after the
/// wrapper exits. Without this, `nohup llama-server ... &` inside a defer
/// task would launch successfully and then be killed seconds later — either
/// by SIGHUP when the SSH session tears down, or by the parent's process
/// group cleanup on the local side.
///
/// Strategy: run the user command inside `setsid sh -c '...'` so it gets a
/// fresh session + process group. Children inherit that group and survive
/// the parent's exit. `setsid` is ubiquitous on Linux; on macOS it's not
/// present, so we fall back to plain `sh -c` (Taylor is the only macOS
/// defer-worker host, and it's the leader/human-in-loop — operators should
/// prefer `nohup <cmd> </dev/null >/dev/null 2>&1 & disown` there).
fn wrap_for_detachment(user_cmd: &str, is_linux_target: bool) -> String {
    if is_linux_target {
        // Single-quote-escape the user script for `setsid sh -c '...'`.
        let escaped = user_cmd.replace('\'', "'\\''");
        format!("setsid sh -c '{escaped}'")
    } else {
        // macOS or unknown — caller must detach manually.
        // TODO: background processes in shell payloads on macOS must use
        // `nohup <cmd> </dev/null >/dev/null 2>&1 & disown` — operator
        // responsibility (setsid is unavailable).
        user_cmd.to_string()
    }
}

/// Run a shell command either locally (when target is this host or None) or via SSH.
/// Max time a shell payload may run before it is killed.
const SHELL_TIMEOUT: Duration = Duration::from_secs(1800); // 30 min
/// Max bytes to capture per stream (stdout / stderr). Anything beyond this
/// is dropped and the pipe is closed so the child gets SIGPIPE.
const MAX_SHELL_OUTPUT_BYTES: usize = 10 * 1024 * 1024; // 10 MB

async fn execute_shell(
    target_node: Option<&str>,
    command: &str,
    nodes: &[ff_db::FleetNodeRow],
    workspace: Option<&std::path::Path>,
) -> (bool, Option<serde_json::Value>, Option<String>) {
    use tokio::io::AsyncReadExt;
    use tokio::process::Command as TokCmd;
    use tokio::time::timeout;

    let this_hostname = tokio::process::Command::new("hostname")
        .output()
        .await
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_default();

    // Local host is Linux if uname reports Linux.
    let local_is_linux = std::env::consts::OS == "linux";

    let mut local = true;
    let (program, args): (&str, Vec<String>) = match target_node {
        None => (
            "sh",
            vec!["-c".into(), wrap_for_detachment(command, local_is_linux)],
        ),
        Some(n) if this_hostname.starts_with(&n.to_lowercase()) => (
            "sh",
            vec!["-c".into(), wrap_for_detachment(command, local_is_linux)],
        ),
        Some(n) => {
            local = false;
            // SSH to target: look up user@ip from DB.
            let node = match nodes.iter().find(|x| x.name.eq_ignore_ascii_case(n)) {
                Some(n) => n,
                None => return (false, None, Some(format!("node '{n}' not in fleet_nodes"))),
            };
            let dest = format!("{}@{}", node.ssh_user, node.ip);
            // Assume remote targets are Linux (Marcus/Sophie/Priya are Ubuntu;
            // James is macOS — but gets same treatment: wrap_for_detachment
            // returns plain cmd for non-Linux, which is safe).
            // `-n` closes stdin so backgrounded children aren't wedged on it.
            let os_hint = node.os.to_lowercase();
            // Default to Linux (most fleet nodes): covers ubuntu, debian,
            // dgx-os, generic "linux". Exclude darwin/macos explicitly.
            let remote_is_linux = !(os_hint.contains("darwin") || os_hint.contains("macos"));
            (
                "ssh",
                vec![
                    "-n".into(),
                    "-o".into(),
                    "ConnectTimeout=8".into(),
                    "-o".into(),
                    "StrictHostKeyChecking=accept-new".into(),
                    "-o".into(),
                    "BatchMode=yes".into(),
                    dest,
                    wrap_for_detachment(command, remote_is_linux),
                ],
            )
        }
    };

    let mut cmd = TokCmd::new(program);
    cmd.args(&args);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    if local && let Some(ws) = workspace {
        cmd.current_dir(ws);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (false, None, Some(format!("spawn {program} failed: {e}"))),
    };

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let stdout_fut = async move {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stdout_pipe {
            let mut chunk = [0u8; 8192];
            while buf.len() < MAX_SHELL_OUTPUT_BYTES {
                match pipe.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let to_add = n.min(MAX_SHELL_OUTPUT_BYTES - buf.len());
                        buf.extend_from_slice(&chunk[..to_add]);
                    }
                    Err(_) => break,
                }
            }
            // Pipe dropped here → child gets SIGPIPE on further writes.
        }
        buf
    };

    let stderr_fut = async move {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stderr_pipe {
            let mut chunk = [0u8; 8192];
            while buf.len() < MAX_SHELL_OUTPUT_BYTES {
                match pipe.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let to_add = n.min(MAX_SHELL_OUTPUT_BYTES - buf.len());
                        buf.extend_from_slice(&chunk[..to_add]);
                    }
                    Err(_) => break,
                }
            }
        }
        buf
    };

    let (stdout, stderr, status) = match timeout(SHELL_TIMEOUT, async {
        let (stdout, stderr) = tokio::join!(stdout_fut, stderr_fut);
        let status = child.wait().await.map_err(|e| e.to_string())?;
        Ok::<_, String>((stdout, stderr, status))
    })
    .await
    {
        Ok(Ok(triple)) => triple,
        Ok(Err(e)) => return (false, None, Some(format!("shell execution failed: {e}"))),
        Err(_) => {
            let _ = child.start_kill();
            return (
                false,
                None,
                Some(format!(
                    "shell command timed out after {}s",
                    SHELL_TIMEOUT.as_secs()
                )),
            );
        }
    };

    let stdout = String::from_utf8_lossy(&stdout).to_string();
    let stderr = String::from_utf8_lossy(&stderr).to_string();
    let result = serde_json::json!({
        "exit_code": status.code(),
        "stdout": stdout,
        "stderr": stderr,
    });
    if status.success() {
        (true, Some(result), None)
    } else {
        let err = format!(
            "exit {}: {}",
            status.code().unwrap_or(-1),
            stderr.trim().lines().last().unwrap_or("")
        );
        (false, Some(result), Some(err))
    }
}

/// Shared reqwest client for HTTP deferred tasks (avoids creating a new
/// connection pool on every call).
static HTTP_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();

fn http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client build must succeed")
    })
}

/// Max HTTP response body we will load into memory (prevents unbounded
/// buffering if a server returns a massive payload).
const MAX_HTTP_RESPONSE_BYTES: usize = 10 * 1024 * 1024; // 10 MB

/// Execute an HTTP request task.
async fn execute_http(
    method: &str,
    url: &str,
    body: Option<serde_json::Value>,
) -> (bool, Option<serde_json::Value>, Option<String>) {
    let method_obj = match method.to_uppercase().as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "DELETE" => reqwest::Method::DELETE,
        "PATCH" => reqwest::Method::PATCH,
        other => return (false, None, Some(format!("bad http method: {other}"))),
    };
    let mut req = http_client().request(method_obj, url);
    if let Some(b) = body {
        req = req.json(&b);
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            // Reject early if the server advertises a body larger than our cap.
            if resp
                .content_length()
                .is_some_and(|len| len > MAX_HTTP_RESPONSE_BYTES as u64)
            {
                return (
                    false,
                    None,
                    Some(format!(
                        "HTTP response body exceeds {}MB (Content-Length)",
                        MAX_HTTP_RESPONSE_BYTES / 1_048_576
                    )),
                );
            }
            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => return (false, None, Some(format!("http body read: {e}"))),
            };
            if bytes.len() > MAX_HTTP_RESPONSE_BYTES {
                return (
                    false,
                    None,
                    Some(format!(
                        "HTTP response body exceeds {}MB",
                        MAX_HTTP_RESPONSE_BYTES / 1_048_576
                    )),
                );
            }
            let text = String::from_utf8_lossy(&bytes).to_string();
            let result = serde_json::json!({"status": status.as_u16(), "body": text});
            if status.is_success() {
                (true, Some(result), None)
            } else {
                (false, Some(result), Some(format!("HTTP {status}")))
            }
        }
        Err(e) => (false, None, Some(format!("http send: {e}"))),
    }
}

// ─── Versions / Fleet / Onboard CLI handlers (Phase 3+5) ──────────────────











/// `ff pm import-claude-tasks` — parses the Claude Code session JSONL
/// and upserts each task as a `work_items` row.
///
/// Claude Code doesn't persist its task list to a separate file; the
/// state is embedded in the session transcript as `tool_result` content
/// on TaskCreate/TaskList/TaskUpdate calls. The format per line is
/// `#<id> [<status>] <subject>`. We scan for the LAST occurrence of
/// this format in the transcript and treat that as the authoritative
/// snapshot (older lines are stale).
///
/// Dedupe key: the Claude task ID is stored in
/// `work_items.metadata->>'claude_task_id'`; repeat imports UPDATE the
/// same row rather than creating a new one.
pub async fn handle_defer_worker(
    as_node: Option<String>,
    interval: u64,
    scheduler: bool,
    once: bool,
) -> Result<()> {
    let worker_name = match as_node {
        Some(n) => n,
        None => ff_agent::fleet_info::resolve_this_node_name().await,
    };

    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    // Sub-agent concurrency slots — read fleet_nodes.sub_agent_count for this node.
    let slot_count = ff_db::pg_get_node(&pool, &worker_name)
        .await
        .ok()
        .flatten()
        .map(|n| n.sub_agent_count.max(1) as u32)
        .unwrap_or(1);
    let _ = ff_agent::sub_agents::ensure_workspaces(slot_count);
    let slots = ff_agent::sub_agents::Slots::new(slot_count);

    println!("{CYAN}▶ defer-worker starting{RESET}");
    println!("  node:      {worker_name}");
    println!("  scheduler: {scheduler}");
    println!("  interval:  {interval}s");
    println!(
        "  mode:      {}",
        if once { "single-pass" } else { "continuous" }
    );

    // Subscribe to fleet:node_online so this worker wakes instantly when
    // the scheduler reports that this node is back online.
    let (wake_tx, mut wake_rx) = tokio::sync::mpsc::channel::<()>(8);
    if !once {
        let my_node = worker_name.clone();
        tokio::spawn(async move {
            use futures::StreamExt;
            let mut stream = ff_agent::fleet_events::subscribe_node_online();
            while let Some(node) = stream.next().await {
                if node.eq_ignore_ascii_case(&my_node) {
                    let _ = wake_tx.try_send(());
                }
            }
        });
    }

    loop {
        let pass_start = std::time::Instant::now();
        let ran_any = defer_pass(&pool, &worker_name, scheduler, &slots).await? > 0;

        if once {
            println!("{CYAN}▶ defer-worker: --once set, exiting{RESET}");
            return Ok(());
        }

        let elapsed = pass_start.elapsed();
        let sleep_for = Duration::from_secs(interval).saturating_sub(elapsed);
        if !ran_any && sleep_for.as_millis() > 0 {
            tokio::select! {
                _ = tokio::time::sleep(sleep_for) => {}
                Some(_) = wake_rx.recv() => {
                    println!("{CYAN}[worker]{RESET} woken by fleet:node_online");
                }
            }
        } else if sleep_for.as_millis() > 0 {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

/// One scheduler+worker pass. Returns number of tasks executed.
///
/// `slots` — sub-agent concurrency pool. On hosts with capacity > 1
/// the pass claims and spawns up to `capacity` tasks in parallel.
async fn defer_pass(
    pool: &sqlx::PgPool,
    worker_name: &str,
    scheduler: bool,
    slots: &ff_agent::sub_agents::Slots,
) -> Result<usize> {
    // Scheduler pass: promote pending tasks whose trigger fired.
    if scheduler {
        match ff_db::pg_list_nodes(pool).await {
            Ok(nodes) => {
                let online = probe_online_nodes(&nodes).await;

                // Detect online/offline transitions and publish to Redis so
                // workers on newly-online nodes can wake up immediately
                // instead of waiting for the next poll tick.
                static LAST_ONLINE: std::sync::OnceLock<
                    std::sync::Mutex<std::collections::HashSet<String>>,
                > = std::sync::OnceLock::new();
                let last_online = LAST_ONLINE
                    .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
                let current: std::collections::HashSet<String> = online.iter().cloned().collect();
                let (newly_online, newly_offline) = {
                    let mut prev = last_online.lock().unwrap();
                    let newly_online: Vec<String> = current.difference(&*prev).cloned().collect();
                    let newly_offline: Vec<String> = prev.difference(&current).cloned().collect();
                    *prev = current.clone();
                    (newly_online, newly_offline)
                };
                for n in &newly_online {
                    if let Err(e) = ff_agent::fleet_events::publish_node_online(n).await {
                        eprintln!("{YELLOW}[sched] publish_node_online({n}): {e}{RESET}");
                    } else {
                        println!("{CYAN}[sched]{RESET} node online → {n} (published)");
                    }
                }
                for n in &newly_offline {
                    if let Err(e) = ff_agent::fleet_events::publish_node_offline(n).await {
                        eprintln!("{YELLOW}[sched] publish_node_offline({n}): {e}{RESET}");
                    } else {
                        println!("{CYAN}[sched]{RESET} node offline → {n} (published)");
                    }
                }

                let now = chrono::Utc::now();
                match ff_db::pg_scheduler_pass(pool, &online, now).await {
                    Ok(n) if n > 0 => {
                        println!(
                            "{CYAN}[sched]{RESET} promoted {n} task(s) to dispatchable (online: {})",
                            online.join(",")
                        );
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("{RED}[sched] pg_scheduler_pass: {e}{RESET}"),
                }
            }
            Err(e) => eprintln!("{RED}[sched] list nodes: {e}{RESET}"),
        }
    }

    // Worker pass: reserve a sub-agent slot, claim one task per slot,
    // spawn each in its own tokio task. We keep looping until either
    // the queue is empty or all slots are busy.
    let mut count = 0usize;
    let mut spawned = Vec::new();
    loop {
        let guard = match slots.try_reserve_owned() {
            Some(g) => g,
            None => break, // all slots busy — try next tick
        };

        let claimed = match ff_db::pg_claim_deferred(pool, worker_name).await {
            Ok(Some(t)) => t,
            Ok(None) => break, // queue empty
            Err(e) => {
                eprintln!("{RED}[worker] claim error: {e}{RESET}");
                break;
            }
        };
        count += 1;
        println!(
            "{YELLOW}[worker]{RESET} slot#{} claimed {} — {}",
            guard.index(),
            claimed.id,
            claimed.title,
        );

        let pool2 = pool.clone();
        let nodes = ff_db::pg_list_nodes(pool).await.unwrap_or_default();
        let h = tokio::spawn(async move {
            let workspace = guard.workspace().to_path_buf();
            let (ok, result, err) = execute_deferred(&claimed, &nodes, Some(&workspace)).await;
            match ff_db::pg_finish_deferred(
                &pool2,
                &claimed.id,
                ok,
                result.as_ref(),
                err.as_deref(),
            )
            .await
            {
                Ok(()) => {
                    if ok {
                        println!(
                            "  {CYAN}✓ completed{RESET} (slot#{} id={})",
                            guard.index(),
                            claimed.id,
                        );
                    } else {
                        println!(
                            "  {RED}✗ failed{RESET} (slot#{} id={}): {}",
                            guard.index(),
                            claimed.id,
                            err.clone().unwrap_or_default(),
                        );
                    }
                }
                Err(e) => eprintln!("{RED}  finalize error: {e}{RESET}"),
            }

            // Auto-upgrade finalizer: if this task was an auto-upgrade (or
            // ff fleet upgrade), publish the completion event + ping Telegram
            // and clear the `status='upgrading'` flag in computer_software.
            if let Some(meta) = claimed
                .payload
                .get("meta")
                .and_then(|v| v.get("auto_upgrade"))
            {
                finalize_upgrade_event(&pool2, &claimed, ok, meta, err.as_deref()).await;
            }

            // External-tool finalizer: `ff ext install` / auto drift →
            // install path. Flips computer_external_tools.status and
            // best-effort extracts installed_version from stdout.
            if let Some(meta) = claimed
                .payload
                .get("meta")
                .and_then(|v| v.get("external_tool"))
            {
                finalize_external_tool_event(&pool2, &claimed, ok, meta, err.as_deref()).await;
            }

            // guard drops here, releasing the slot.
            drop(guard);
        });
        spawned.push(h);
    }

    // If this pass only has one slot (legacy single-claim behaviour),
    // await the task so callers see the same semantics as before.
    if slots.capacity() == 1 {
        for h in spawned {
            let _ = h.await;
        }
    }
    Ok(count)
}
pub async fn handle_daemon(
    as_node: Option<String>,
    scheduler: bool,
    defer_interval: u64,
    disk_interval: u64,
    reconcile_interval: u64,
    once: bool,
) -> Result<()> {
    let worker_name = match as_node {
        Some(n) => n,
        None => ff_agent::fleet_info::resolve_this_node_name().await,
    };

    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    // Sub-agent concurrency slots — read fleet_nodes.sub_agent_count for this node.
    let slot_count = ff_db::pg_get_node(&pool, &worker_name)
        .await
        .ok()
        .flatten()
        .map(|n| n.sub_agent_count.max(1) as u32)
        .unwrap_or(1);
    let _ = ff_agent::sub_agents::ensure_workspaces(slot_count);
    let slots = ff_agent::sub_agents::Slots::new(slot_count);

    // Sub-agent DB rows — seed slot 0 for every computer so `ff agent dispatch`
    // has a worker row to claim. Scheduler-only (one node writes).
    if scheduler {
        match ff_agent::agent_coordinator::seed_slot_zero_for_all(&pool).await {
            Ok(n) if n > 0 => println!("{CYAN}[coord]{RESET} seeded {n} new sub_agent row(s)"),
            Ok(_) => {}
            Err(e) => eprintln!("{RED}[coord] seed error: {e}{RESET}"),
        }
    }

    println!("{CYAN}▶ ForgeFleet daemon starting{RESET}");
    println!("  node:       {worker_name}");
    println!("  scheduler:  {scheduler}");
    println!("  sub-agents: {slot_count}");
    println!("  defer:      every {defer_interval}s");
    println!("  disk:       every {disk_interval}s");
    println!("  reconcile:  every {reconcile_interval}s");

    if once {
        // Run one pass of each sequentially, then exit.
        match defer_pass(&pool, &worker_name, scheduler, &slots).await {
            Ok(n) => println!("{CYAN}[defer]{RESET} one-pass complete ({n} task(s))"),
            Err(e) => eprintln!("{RED}[defer] pass error: {e}{RESET}"),
        }
        match ff_agent::disk_sampler::sample_local_disk(&pool).await {
            Ok(s) => println!(
                "{CYAN}[disk]{RESET} {} total={}MB used={}MB free={}MB models={}MB quota={}%{}",
                s.node_name,
                s.total_bytes / 1_048_576,
                s.used_bytes / 1_048_576,
                s.free_bytes / 1_048_576,
                s.models_bytes / 1_048_576,
                s.quota_pct,
                if s.over_quota { " OVER" } else { "" },
            ),
            Err(e) => eprintln!("{RED}[disk] sample error: {e}{RESET}"),
        }
        match ff_agent::deployment_reconciler::reconcile_local(&pool).await {
            Ok(r) => println!(
                "{CYAN}[reconcile]{RESET} adopted={} removed={} refreshed={}",
                r.adopted, r.removed, r.refreshed,
            ),
            Err(e) => eprintln!("{RED}[reconcile] error: {e}{RESET}"),
        }
        // Sweeper — only the scheduler needs to do this fleet-wide.
        if scheduler {
            match ff_agent::job_sweeper::sweep_stale(
                &pool,
                &ff_agent::job_sweeper::SweepPolicy::default(),
            )
            .await
            {
                Ok(s) if s.jobs_failed + s.deferred_failed > 0 => println!(
                    "{CYAN}[sweeper]{RESET} jobs_failed={} deferred_failed={}",
                    s.jobs_failed, s.deferred_failed,
                ),
                Ok(_) => println!("{CYAN}[sweeper]{RESET} no stale work"),
                Err(e) => eprintln!("{RED}[sweeper] error: {e}{RESET}"),
            }
        }
        println!("{CYAN}▶ daemon: --once set, exiting{RESET}");
        return Ok(());
    }

    let mut defer_tick = tokio::time::interval(Duration::from_secs(defer_interval));
    let mut disk_tick = tokio::time::interval(Duration::from_secs(disk_interval));
    let mut recon_tick = tokio::time::interval(Duration::from_secs(reconcile_interval));
    // Sweeper: every 5 minutes, only on the scheduler node.
    let mut sweep_tick = tokio::time::interval(Duration::from_secs(300));
    // Version check: every 6 hours (fleet-wide drift detection).
    let mut version_tick = tokio::time::interval(Duration::from_secs(6 * 3600));
    // Brain vault re-index: every 30 minutes (pick up Obsidian edits).
    let mut brain_tick = tokio::time::interval(Duration::from_secs(30 * 60));
    // Project GitHub sync: every 5 minutes (leader-only to avoid rate-limit waste).
    let mut gh_sync_tick = tokio::time::interval(Duration::from_secs(5 * 60));
    // Fabric benchmark: every 24h (leader-only). Fires `ff fabric
    // benchmark-all` so `fabric_pairs.measured_bandwidth_gbps` stays
    // fresh across the fleet without operator intervention.
    let mut fabric_tick = tokio::time::interval(Duration::from_secs(24 * 3600));
    // OAuth probe: every 6h (leader-only). Hits each oauth_subscription
    // provider's /v1/models with the harvested token and logs the
    // result. Catches token expiry before the next inference call
    // surfaces it as a 401 to a user.
    let mut oauth_tick = tokio::time::interval(Duration::from_secs(6 * 3600));
    // First tick fires immediately for each — prime all nine.
    defer_tick.tick().await;
    disk_tick.tick().await;
    recon_tick.tick().await;
    sweep_tick.tick().await;
    version_tick.tick().await;
    brain_tick.tick().await;
    gh_sync_tick.tick().await;
    fabric_tick.tick().await;
    oauth_tick.tick().await;

    // Do an initial pass immediately on startup.
    let _ = defer_pass(&pool, &worker_name, scheduler, &slots).await;
    // Initial version check on daemon startup so operators see data within
    // seconds instead of waiting 6 hours for the first tick.
    match ff_agent::version_check::version_check_pass(&pool).await {
        Ok(s) if !s.drifted_keys.is_empty() => println!(
            "{CYAN}[versions]{RESET} drift: {}",
            s.drifted_keys.join(", ")
        ),
        Ok(s) => println!(
            "{CYAN}[versions]{RESET} initial pass: {} tools ✓",
            s.total_keys
        ),
        Err(e) => eprintln!("{RED}[versions] startup: {e}{RESET}"),
    }
    match ff_agent::disk_sampler::sample_local_disk(&pool).await {
        Ok(s) => println!(
            "{CYAN}[disk]{RESET} {} total={}MB used={}MB free={}MB models={}MB quota={}%{}",
            s.node_name,
            s.total_bytes / 1_048_576,
            s.used_bytes / 1_048_576,
            s.free_bytes / 1_048_576,
            s.models_bytes / 1_048_576,
            s.quota_pct,
            if s.over_quota { " OVER" } else { "" },
        ),
        Err(e) => eprintln!("{RED}[disk] sample error: {e}{RESET}"),
    }
    match ff_agent::deployment_reconciler::reconcile_local(&pool).await {
        Ok(r) if r.adopted + r.removed + r.refreshed > 0 => println!(
            "{CYAN}[reconcile]{RESET} adopted={} removed={} refreshed={}",
            r.adopted, r.removed, r.refreshed,
        ),
        Ok(_) => {}
        Err(e) => eprintln!("{RED}[reconcile] error: {e}{RESET}"),
    }

    // Subscribe to fleet:node_online so the daemon runs an immediate
    // defer_pass when this node comes back online (instant wake-up
    // instead of waiting for the next defer_tick).
    let (wake_tx, mut wake_rx) = tokio::sync::mpsc::channel::<()>(8);
    {
        let my_node = worker_name.clone();
        tokio::spawn(async move {
            use futures::StreamExt;
            let mut stream = ff_agent::fleet_events::subscribe_node_online();
            while let Some(node) = stream.next().await {
                if node.eq_ignore_ascii_case(&my_node) {
                    let _ = wake_tx.try_send(());
                }
            }
        });
    }

    // ─── Phase 7: model portfolio intelligence ──────────────────────────
    // These three long-lived loops only run on the elected leader so we
    // don't burn HF API quota from every box. Non-leaders skip the spawn
    // entirely and rely on the leader to keep the catalog + coverage fresh.
    let (_portfolio_shutdown_tx, portfolio_shutdown_rx) = tokio::sync::watch::channel(false);

    // Local self-healer — runs on EVERY host (not leader-gated) so each
    // box restarts its own forgefleetd if it dies. Closes the split-brain
    // window where `ff daemon` keeps updating leader heartbeat while
    // forgefleetd is dead and peers have no reason to fail over.
    println!(
        "{CYAN}[healer]{RESET} spawning local forgefleetd self-healer (30s interval, 60s kickoff)"
    );
    let healer = ff_agent::local_healer::LocalHealer::new(worker_name.clone());
    let _healer_handle = healer.spawn(portfolio_shutdown_rx.clone());

    let is_leader = ff_db::pg_get_current_leader(&pool)
        .await
        .ok()
        .flatten()
        .map(|l| l.member_name == worker_name)
        .unwrap_or(false);
    if scheduler || is_leader {
        println!(
            "{CYAN}[portfolio]{RESET} spawning model-upstream (24h) + coverage-guard (15min) + scout (168h)"
        );
        let upstream = ff_agent::model_upstream::ModelUpstreamChecker::new(pool.clone());
        let _upstream_handle = upstream.spawn(24, portfolio_shutdown_rx.clone());

        let guard = ff_agent::coverage_guard::CoverageGuard::new_dbonly(pool.clone());
        let _guard_handle = guard.spawn(15, portfolio_shutdown_rx.clone());

        let scout = ff_agent::model_scout::ModelScout::new(pool.clone());
        let _scout_handle = scout.spawn(168, portfolio_shutdown_rx.clone());

        // Hourly auto-upgrade loop: dispatches drift → playbook → Telegram
        // without operator interaction. Gated by fleet_secrets.auto_upgrade_enabled.
        println!("{CYAN}[auto-upgrade]{RESET} spawning hourly drift→upgrade→telegram loop");
        let auto = ff_agent::auto_upgrade::AutoUpgradeTick::new(pool.clone(), worker_name.clone());
        let _auto_handle = auto.spawn(portfolio_shutdown_rx.clone());

        // External-tools upstream drift checker (6h). Scans the V24
        // `external_tools` catalog for new GitHub releases / brew / pip
        // versions and flips `computer_external_tools.status` rows to
        // `'upgrade_available'`. Pure detector — install dispatch is a
        // separate concern (see `ff ext install`).
        println!("{CYAN}[ext-upstream]{RESET} spawning 6h external-tools upstream checker");
        let ext_upstream =
            ff_agent::external_tools_upstream::ExternalToolsUpstreamChecker::new(pool.clone());
        let _ext_upstream_handle = ext_upstream.spawn(6, portfolio_shutdown_rx.clone());

        // Stuck-slot reaper: resets sub_agents rows stuck in 'error' or 'busy'
        // with a stale started_at so the dispatch queue can't lock up.
        println!(
            "{CYAN}[reaper]{RESET} spawning stuck-slot reaper (10min interval, 10min timeout)"
        );
        let reaper =
            ff_agent::sub_agent_reaper::SubAgentReaper::new(pool.clone(), worker_name.clone());
        let _reaper_handle = reaper.spawn(portfolio_shutdown_rx.clone());
    } else {
        println!("{CYAN}[portfolio]{RESET} skipping — not leader / scheduler");
    }

    loop {
        tokio::select! {
            _ = defer_tick.tick() => {
                if let Err(e) = defer_pass(&pool, &worker_name, scheduler, &slots).await {
                    eprintln!("{RED}[defer] pass error: {e}{RESET}");
                }
            }
            Some(_) = wake_rx.recv() => {
                println!("{CYAN}[defer]{RESET} woken by fleet:node_online");
                if let Err(e) = defer_pass(&pool, &worker_name, scheduler, &slots).await {
                    eprintln!("{RED}[defer] pass error: {e}{RESET}");
                }
            }
            _ = disk_tick.tick() => {
                match ff_agent::disk_sampler::sample_local_disk(&pool).await {
                    Ok(s) => println!(
                        "{CYAN}[disk]{RESET} {} total={}MB used={}MB free={}MB models={}MB quota={}%{}",
                        s.node_name,
                        s.total_bytes / 1_048_576,
                        s.used_bytes / 1_048_576,
                        s.free_bytes / 1_048_576,
                        s.models_bytes / 1_048_576,
                        s.quota_pct,
                        if s.over_quota { " OVER" } else { "" },
                    ),
                    Err(e) => eprintln!("{RED}[disk] sample error: {e}{RESET}"),
                }
            }
            _ = recon_tick.tick() => {
                match ff_agent::deployment_reconciler::reconcile_local(&pool).await {
                    Ok(r) if r.adopted + r.removed + r.refreshed > 0 => println!(
                        "{CYAN}[reconcile]{RESET} adopted={} removed={} refreshed={}",
                        r.adopted, r.removed, r.refreshed,
                    ),
                    Ok(_) => {}
                    Err(e) => eprintln!("{RED}[reconcile] error: {e}{RESET}"),
                }
            }
            _ = sweep_tick.tick(), if scheduler => {
                match ff_agent::job_sweeper::sweep_stale(
                    &pool,
                    &ff_agent::job_sweeper::SweepPolicy::default(),
                ).await {
                    Ok(s) if s.jobs_failed + s.deferred_failed > 0 => println!(
                        "{CYAN}[sweeper]{RESET} jobs_failed={} deferred_failed={}",
                        s.jobs_failed, s.deferred_failed,
                    ),
                    Ok(_) => {}
                    Err(e) => eprintln!("{RED}[sweeper] error: {e}{RESET}"),
                }
            }
            _ = version_tick.tick() => {
                match ff_agent::version_check::version_check_pass(&pool).await {
                    Ok(s) if !s.drifted_keys.is_empty() => println!(
                        "{CYAN}[versions]{RESET} drift detected: {}",
                        s.drifted_keys.join(", ")),
                    Ok(_) => {}
                    Err(e) => eprintln!("{RED}[versions] {e}{RESET}"),
                }
                // Leader-only: refresh the mesh matrix at the same cadence so
                // stale rows don't accumulate and operators see fresh status.
                if worker_name == "taylor" {
                    match ff_agent::mesh_check::pairwise_ssh_check(&pool).await {
                        Ok(m) => {
                            let (ok, fail) = m.cells.iter()
                                .fold((0usize, 0usize), |(o, f), c| {
                                    if c.status == "ok" { (o + 1, f) } else { (o, f + 1) }
                                });
                            println!("{CYAN}[mesh]{RESET} refreshed: {ok} ok, {fail} fail");
                            // Auto-retry any failed pair whose last check was
                            // more than 10 minutes ago — capped at 5 retries
                            // per 24h by pg_enqueue_deferred's max_attempts.
                            let _ = ff_agent::mesh_check::enqueue_retries(&pool).await;
                        }
                        Err(e) => eprintln!("{RED}[mesh] refresh error: {e}{RESET}"),
                    }
                }
            }
            _ = brain_tick.tick() => {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/venkat".into());
                let vault_path = std::path::PathBuf::from(format!("{home}/projects/Yarli_KnowledgeBase"));
                if vault_path.exists() {
                    let config = ff_brain::VaultConfig {
                        vault_path,
                        brain_subfolder: String::new(),
                    };
                    match ff_brain::index_vault(&pool, &config).await {
                        Ok(r) if r.nodes_upserted > 0 => println!(
                            "{CYAN}[brain]{RESET} vault re-indexed: {} new/changed, {} skipped",
                            r.nodes_upserted, r.unchanged_skipped),
                        Ok(_) => {}
                        Err(e) => eprintln!("{RED}[brain] vault index error: {e}{RESET}"),
                    }
                }
            }
            _ = gh_sync_tick.tick(), if scheduler => {
                let sync = ff_agent::project_github_sync::GitHubSync::new(pool.clone());
                match sync.sync_all_projects().await {
                    Ok(r) if r.updated_main > 0 || !r.errors.is_empty() => println!(
                        "{CYAN}[projects]{RESET} gh sync: {} main updated, {} branches, {} PRs, {} errors",
                        r.updated_main, r.branches_upserted, r.prs_attached, r.errors.len()),
                    Ok(_) => {}
                    Err(e) => eprintln!("{RED}[projects] gh sync error: {e}{RESET}"),
                }
            }
            _ = fabric_tick.tick(), if scheduler => {
                // Short duration (5s) — sweeping every pair, not benchmarking
                // throughput exhaustively. Operators run the full 30s probe
                // manually via `ff fabric benchmark <a> <b>` when needed.
                match crate::fabric_cmd::handle_fabric_benchmark_all(&pool, 5, 1).await {
                    Ok(()) => println!("{CYAN}[fabric]{RESET} 24h benchmark sweep complete"),
                    Err(e) => eprintln!("{RED}[fabric] sweep error: {e}{RESET}"),
                }
            }
            _ = oauth_tick.tick(), if scheduler => {
                let results = ff_agent::oauth_distributor::probe_all(&pool).await;
                let mut bad = 0usize;
                for r in &results {
                    match r.status.as_str() {
                        "ok" => tracing::debug!(provider = %r.provider, "oauth_probe ok"),
                        "no_token" => tracing::debug!(
                            provider = %r.provider, "oauth_probe: no token configured"
                        ),
                        "unauthorized" | "forbidden" => {
                            tracing::error!(
                                provider = %r.provider,
                                status = %r.status,
                                http = ?r.http_status,
                                "oauth_probe: token rejected — re-import via `ff oauth import {} && ff oauth distribute {}`",
                                r.provider, r.provider
                            );
                            bad += 1;
                        }
                        _ => {
                            tracing::warn!(
                                provider = %r.provider,
                                status = %r.status,
                                http = ?r.http_status,
                                msg = ?r.message,
                                "oauth_probe: unexpected status"
                            );
                            bad += 1;
                        }
                    }
                }
                if bad > 0 {
                    println!(
                        "{YELLOW}[oauth]{RESET} probe: {}/{} provider(s) need attention — see logs",
                        bad,
                        results.len(),
                    );
                }
            }
        }
    }
}
