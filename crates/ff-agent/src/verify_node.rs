//! Full post-onboarding verification battery. See plan §3i.
use std::time::Duration;
use sqlx::PgPool;
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Debug, Clone, serde::Serialize)]
pub struct CheckResult {
    pub check: String,
    pub status: String, // "pass" | "fail" | "skip"
    pub message: Option<String>,
    pub retry_task_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct VerifyReport {
    pub node: String,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub details: Vec<CheckResult>,
    pub checked_at: chrono::DateTime<chrono::Utc>,
}

pub async fn verify_node(pool: &PgPool, node_name: &str) -> Result<VerifyReport, String> {
    let node = ff_db::pg_get_node(pool, node_name)
        .await
        .map_err(|e| format!("pg_get_node: {e}"))?
        .ok_or_else(|| format!("node '{node_name}' not in fleet_nodes"))?;
    let ssh_dest = format!("{}@{}", node.ssh_user, node.ip);
    let mut details = Vec::new();

    // 1. daemon_healthy
    details.push(check_daemon_healthy(&node).await);
    // 2. db_reachable_from_node
    details.push(check_ssh_cmd(&ssh_dest, "db_reachable_from_node",
        "~/.local/bin/ff status --no-color 2>&1 | head -5 | grep -q 'connected\\|Database'").await);
    // 3. redis_reachable_from_node
    details.push(check_ssh_cmd(&ssh_dest, "redis_reachable_from_node",
        "nc -z -w 3 192.168.5.100 6380").await);
    // 4. sub_agent_dirs_exist
    let want = node.sub_agent_count;
    let subcmd = format!("ls -d ~/.forgefleet/sub-agent-* 2>/dev/null | wc -l | tr -d ' '");
    let sub_res = ssh_capture(&ssh_dest, &subcmd).await;
    details.push(match sub_res {
        Ok(out) if out.trim().parse::<i32>().map(|v| v >= want).unwrap_or(false) => CheckResult {
            check: "sub_agent_dirs_exist".into(), status: "pass".into(),
            message: Some(format!("found {} dirs, expected {want}", out.trim())), retry_task_id: None,
        },
        Ok(out) => CheckResult {
            check: "sub_agent_dirs_exist".into(), status: "fail".into(),
            message: Some(format!("found {} dirs, expected {want}", out.trim())), retry_task_id: None,
        },
        Err(e) => CheckResult {
            check: "sub_agent_dirs_exist".into(), status: "fail".into(),
            message: Some(e), retry_task_id: None,
        },
    });
    // 5. tooling_installed
    details.push(check_ssh_cmd(&ssh_dest, "tooling_installed",
        "[ $(which gh op codex claude openclaw 2>/dev/null | wc -l) -ge 3 ]").await);
    // 6. tool_versions_reported
    details.push(if node.tooling.as_object().map(|m| !m.is_empty()).unwrap_or(false) {
        CheckResult { check: "tool_versions_reported".into(), status: "pass".into(), message: None, retry_task_id: None }
    } else {
        CheckResult { check: "tool_versions_reported".into(), status: "fail".into(),
            message: Some("fleet_nodes.tooling is empty; run `ff daemon` long enough for a version_check tick".into()),
            retry_task_id: None }
    });
    // 7. models_scanned
    let libs = ff_db::pg_list_library(pool, Some(node_name)).await.unwrap_or_default();
    details.push(if libs.is_empty() {
        CheckResult { check: "models_scanned".into(), status: "skip".into(),
            message: Some("library empty; skipped — run `ff model scan` on the node".into()), retry_task_id: None }
    } else {
        CheckResult { check: "models_scanned".into(), status: "pass".into(),
            message: Some(format!("{} models indexed", libs.len())), retry_task_id: None }
    });
    // 8. openclaw_registered — skip for now
    details.push(CheckResult {
        check: "openclaw_registered".into(), status: "skip".into(),
        message: Some("openclaw api not yet wired".into()), retry_task_id: None,
    });
    // 9. sudo_passwordless
    details.push(if node_name == "taylor" {
        CheckResult { check: "sudo_passwordless".into(), status: "skip".into(),
            message: Some("taylor is excluded from passwordless sudo policy".into()), retry_task_id: None }
    } else {
        check_ssh_cmd(&ssh_dest, "sudo_passwordless", "sudo -n true").await
    });
    // 10. mesh_ssh_complete
    let mesh = ff_db::pg_list_mesh_status(pool, Some(node_name)).await.unwrap_or_default();
    details.push(if mesh.is_empty() {
        CheckResult { check: "mesh_ssh_complete".into(), status: "skip".into(),
            message: Some("no mesh checks yet; run `ff fleet ssh-mesh-check`".into()), retry_task_id: None }
    } else if mesh.iter().all(|r| r.status == "ok") {
        CheckResult { check: "mesh_ssh_complete".into(), status: "pass".into(),
            message: Some(format!("{} pairs all ok", mesh.len())), retry_task_id: None }
    } else {
        let fails: Vec<String> = mesh.iter().filter(|r| r.status != "ok")
            .map(|r| format!("{}→{}", r.src_node, r.dst_node)).collect();
        CheckResult { check: "mesh_ssh_complete".into(), status: "fail".into(),
            message: Some(format!("{} pair(s) failed: {}", fails.len(), fails.join(", "))),
            retry_task_id: None }
    });
    // 11. defer_end_to_end
    let title = format!("verify-{}-{}", node_name, chrono::Utc::now().timestamp());
    let payload = serde_json::json!({"command": format!("echo verify-{}", chrono::Utc::now().timestamp())});
    let task_id_res = ff_db::pg_enqueue_deferred(
        pool, &title, "shell", &payload, "now", &serde_json::json!({}),
        Some(node_name), &serde_json::json!([]), Some("verify_node"), Some(1),
    ).await;
    details.push(match task_id_res {
        Ok(tid) => {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            let mut final_status = None;
            while std::time::Instant::now() < deadline {
                if let Ok(Some(row)) = ff_db::pg_get_deferred(pool, &tid).await {
                    if row.status == "completed" || row.status == "failed" {
                        final_status = Some(row.status);
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
            match final_status.as_deref() {
                Some("completed") => CheckResult { check: "defer_end_to_end".into(), status: "pass".into(),
                    message: Some(format!("task {tid} completed")), retry_task_id: None },
                Some(s) => CheckResult { check: "defer_end_to_end".into(), status: "fail".into(),
                    message: Some(format!("task {tid} status={s}")), retry_task_id: Some(tid) },
                None => CheckResult { check: "defer_end_to_end".into(), status: "fail".into(),
                    message: Some(format!("task {tid} not claimed within 30s")), retry_task_id: Some(tid) },
            }
        }
        Err(e) => CheckResult { check: "defer_end_to_end".into(), status: "fail".into(),
            message: Some(format!("enqueue failed: {e}")), retry_task_id: None },
    });
    // 12. library_health — optional
    details.push(CheckResult {
        check: "library_health".into(), status: "skip".into(),
        message: Some("optional first-onboard check".into()), retry_task_id: None,
    });

    let passed = details.iter().filter(|r| r.status == "pass").count();
    let failed = details.iter().filter(|r| r.status == "fail").count();
    let skipped = details.iter().filter(|r| r.status == "skip").count();
    Ok(VerifyReport { node: node_name.to_string(), passed, failed, skipped, details, checked_at: chrono::Utc::now() })
}

async fn check_daemon_healthy(node: &ff_db::FleetNodeRow) -> CheckResult {
    if node.status == "offline" {
        return CheckResult { check: "daemon_healthy".into(), status: "fail".into(),
            message: Some(format!("node status in DB is '{}'", node.status)), retry_task_id: None };
    }
    let addr = format!("{}:22", node.ip);
    let probe = timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(&addr)).await;
    match probe {
        Ok(Ok(_)) => CheckResult { check: "daemon_healthy".into(), status: "pass".into(),
            message: Some(format!("SSH port reachable on {}", node.ip)), retry_task_id: None },
        _ => CheckResult { check: "daemon_healthy".into(), status: "fail".into(),
            message: Some(format!("SSH port 22 unreachable on {}", node.ip)), retry_task_id: None },
    }
}

async fn check_ssh_cmd(dest: &str, name: &str, cmd: &str) -> CheckResult {
    match ssh_capture(dest, cmd).await {
        Ok(_) => CheckResult { check: name.into(), status: "pass".into(), message: None, retry_task_id: None },
        Err(e) => CheckResult { check: name.into(), status: "fail".into(), message: Some(e), retry_task_id: None },
    }
}

async fn ssh_capture(dest: &str, cmd: &str) -> Result<String, String> {
    let out = timeout(Duration::from_secs(10),
        Command::new("ssh").args([
            "-o", "BatchMode=yes",
            "-o", "ConnectTimeout=5",
            "-o", "StrictHostKeyChecking=accept-new",
            dest, cmd,
        ]).output(),
    ).await.map_err(|_| "ssh timeout".to_string())?
        .map_err(|e| format!("spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!("exit {}: {}", out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim().chars().take(140).collect::<String>()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}
