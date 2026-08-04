//! Full post-onboarding verification battery. See plan §3i.
use sqlx::PgPool;
use std::time::Duration;
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

pub async fn verify_computer(pool: &PgPool, worker_name: &str) -> Result<VerifyReport, String> {
    let exclusions = crate::mesh_check::MeshExclusions::default();
    verify_computer_with_exclusions(pool, worker_name, &exclusions).await
}

pub async fn verify_computer_with_exclusions(
    pool: &PgPool,
    worker_name: &str,
    exclusions: &crate::mesh_check::MeshExclusions,
) -> Result<VerifyReport, String> {
    let node = ff_db::pg_get_node(pool, worker_name)
        .await
        .map_err(|e| format!("pg_get_node: {e}"))?
        .ok_or_else(|| format!("node '{worker_name}' not in fleet_workers"))?;
    let ssh_dest = format!("{}@{}", node.ssh_user, node.ip);
    let is_windows = node.os.to_lowercase().contains("windows");
    let mut details = Vec::new();

    // 1. daemon_healthy
    details.push(check_daemon_healthy(&node).await);
    // 2. db_reachable_from_node
    let db_cmd = if is_windows {
        r#"powershell -NoProfile -Command "& { $out = & \"$env:USERPROFILE\.local\bin\ff.exe\" status --no-color 2>&1 | Out-String; if ($out -match 'connected|Database') { exit 0 } else { exit 1 } }""#
    } else {
        "~/.local/bin/ff status 2>&1 | head -5 | grep -q 'connected\\|Database'"
    };
    details.push(check_ssh_cmd(&ssh_dest, "db_reachable_from_node", db_cmd).await);
    // 3. redis_reachable_from_node
    // Ask the installed FF client to exercise its own configured Redis path.
    // This keeps verification on the same authority as the runtime instead of
    // baking a host, port, or auxiliary redis-cli/nc dependency into the check.
    let redis_cmd = redis_check_command(is_windows);
    details.push(check_ssh_cmd(&ssh_dest, "redis_reachable_from_node", redis_cmd).await);
    // 4. sub_agent_dirs_exist
    let want = node.sub_agent_count;
    let subcmd = if is_windows {
        r#"powershell -NoProfile -Command "(Get-ChildItem -Directory \"$env:USERPROFILE\.forgefleet\sub-agents\sub-agent-*\" -ErrorAction SilentlyContinue).Count""#.to_string()
    } else {
        "ls -d ~/.forgefleet/sub-agents/sub-agent-* 2>/dev/null | wc -l | tr -d ' '".to_string()
    };
    let sub_res = ssh_capture(&ssh_dest, &subcmd).await;
    details.push(match sub_res {
        Ok(out)
            if out
                .trim()
                .parse::<i32>()
                .map(|v| v >= want)
                .unwrap_or(false) =>
        {
            CheckResult {
                check: "sub_agent_dirs_exist".into(),
                status: "pass".into(),
                message: Some(format!("found {} dirs, expected {want}", out.trim())),
                retry_task_id: None,
            }
        }
        Ok(out) => CheckResult {
            check: "sub_agent_dirs_exist".into(),
            status: "fail".into(),
            message: Some(format!("found {} dirs, expected {want}", out.trim())),
            retry_task_id: None,
        },
        Err(e) => CheckResult {
            check: "sub_agent_dirs_exist".into(),
            status: "fail".into(),
            message: Some(e),
            retry_task_id: None,
        },
    });
    // 5. tooling_installed
    let tool_cmd = tooling_check_command(is_windows);
    details.push(check_ssh_cmd(&ssh_dest, "tooling_installed", tool_cmd).await);
    // 6. tool_versions_reported
    details.push(if node.tooling.as_object().map(|m| !m.is_empty()).unwrap_or(false) {
        CheckResult { check: "tool_versions_reported".into(), status: "pass".into(), message: None, retry_task_id: None }
    } else {
        CheckResult { check: "tool_versions_reported".into(), status: "fail".into(),
            message: Some("fleet_workers.tooling is empty; run `ff daemon` long enough for a version_check tick".into()),
            retry_task_id: None }
    });
    // 7. models_scanned
    let libs = ff_db::pg_list_library(pool, Some(worker_name))
        .await
        .unwrap_or_default();
    details.push(if libs.is_empty() {
        CheckResult {
            check: "models_scanned".into(),
            status: "skip".into(),
            message: Some("library empty; skipped — run `ff model scan` on the node".into()),
            retry_task_id: None,
        }
    } else {
        CheckResult {
            check: "models_scanned".into(),
            status: "pass".into(),
            message: Some(format!("{} models indexed", libs.len())),
            retry_task_id: None,
        }
    });
    // 9. sudo_passwordless (N/A on Windows — UAC is the equivalent and is
    //    always interactive; Windows daemons run as services, so skip.)
    details.push(if worker_name == "vinny" {
        CheckResult {
            check: "sudo_passwordless".into(),
            status: "skip".into(),
            message: Some("vinny is excluded from passwordless sudo policy".into()),
            retry_task_id: None,
        }
    } else if is_windows {
        CheckResult {
            check: "sudo_passwordless".into(),
            status: "skip".into(),
            message: Some("N/A on Windows (service runs elevated via nssm/Task Scheduler)".into()),
            retry_task_id: None,
        }
    } else {
        check_ssh_cmd(&ssh_dest, "sudo_passwordless", "sudo -n true").await
    });
    // 10. mesh_ssh_complete
    let mesh = ff_db::queries::pg_list_active_mesh_status(pool, Some(worker_name))
        .await
        .unwrap_or_default();
    details.push(mesh_ssh_complete_result(&mesh, exclusions));
    // 11. defer_end_to_end
    // Skip on nodes whose ff binary predates the defer-worker subcommand
    // (added 2026-05-04). Without defer-worker the task enqueues but is
    // never claimed, causing a 30s timeout that looks like a failure.
    let defer_worker_check = if is_windows {
        ssh_capture(
            &ssh_dest,
            r#"powershell -NoProfile -Command "$out = & \"$env:USERPROFILE\.local\bin\ff.exe\" --help 2>&1 | Out-String; if ($out -match 'defer-worker') { exit 0 } else { exit 1 }""#,
        )
        .await
    } else {
        ssh_capture(
            &ssh_dest,
            "~/.local/bin/ff --help 2>&1 | grep -q defer-worker",
        )
        .await
    };
    if let Err(ref e) = defer_worker_check {
        details.push(CheckResult {
            check: "defer_end_to_end".into(),
            status: "skip".into(),
            message: Some(format!(
                "remote ff binary lacks defer-worker subcommand ({e}); upgrade binary to enable"
            )),
            retry_task_id: None,
        });
    } else {
        let title = format!("verify-{}-{}", worker_name, chrono::Utc::now().timestamp());
        let payload = serde_json::json!({
            "command": format!("echo verify-{}", chrono::Utc::now().timestamp())
        });
        let task_id_res = ff_db::pg_enqueue_deferred(
            pool,
            &title,
            "shell",
            &payload,
            "now",
            &serde_json::json!({}),
            Some(worker_name),
            &serde_json::json!([]),
            Some("verify_computer"),
            Some(1),
        )
        .await;
        details.push(match task_id_res {
            Ok(tid) => {
                let deadline = std::time::Instant::now() + Duration::from_secs(30);
                let mut final_status = None;
                while std::time::Instant::now() < deadline {
                    if let Ok(Some(row)) = ff_db::pg_get_deferred(pool, &tid).await
                        && (row.status == "completed" || row.status == "failed")
                    {
                        final_status = Some(row.status);
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                }
                match final_status.as_deref() {
                    Some("completed") => CheckResult {
                        check: "defer_end_to_end".into(),
                        status: "pass".into(),
                        message: Some(format!("task {tid} completed")),
                        retry_task_id: None,
                    },
                    Some(s) => CheckResult {
                        check: "defer_end_to_end".into(),
                        status: "fail".into(),
                        message: Some(format!("task {tid} status={s}")),
                        retry_task_id: Some(tid),
                    },
                    None => CheckResult {
                        check: "defer_end_to_end".into(),
                        status: "fail".into(),
                        message: Some(format!("task {tid} not claimed within 30s")),
                        retry_task_id: Some(tid),
                    },
                }
            }
            Err(e) => CheckResult {
                check: "defer_end_to_end".into(),
                status: "fail".into(),
                message: Some(format!("enqueue failed: {e}")),
                retry_task_id: None,
            },
        });
    }
    // 12. library_health — optional
    details.push(CheckResult {
        check: "library_health".into(),
        status: "skip".into(),
        message: Some("optional first-onboard check".into()),
        retry_task_id: None,
    });

    let passed = details.iter().filter(|r| r.status == "pass").count();
    let failed = details.iter().filter(|r| r.status == "fail").count();
    let skipped = details.iter().filter(|r| r.status == "skip").count();
    Ok(VerifyReport {
        node: worker_name.to_string(),
        passed,
        failed,
        skipped,
        details,
        checked_at: chrono::Utc::now(),
    })
}

fn mesh_ssh_complete_result(
    rows: &[ff_db::MeshStatusRow],
    exclusions: &crate::mesh_check::MeshExclusions,
) -> CheckResult {
    let scoped: Vec<&ff_db::MeshStatusRow> = rows
        .iter()
        .filter(|row| exclusions.allows_edge(&row.src_node, &row.dst_node))
        .collect();
    if scoped.is_empty() {
        return CheckResult {
            check: "mesh_ssh_complete".into(),
            status: "skip".into(),
            message: Some(if exclusions.is_empty() {
                "no mesh checks yet; run `ff fleet ssh-mesh-check`".into()
            } else {
                "no in-scope mesh checks; run `ff fleet ssh-mesh-check`".into()
            }),
            retry_task_id: None,
        };
    }
    if scoped.iter().all(|row| row.status == "ok") {
        return CheckResult {
            check: "mesh_ssh_complete".into(),
            status: "pass".into(),
            message: Some(if exclusions.is_empty() {
                format!("{} pairs all ok", scoped.len())
            } else {
                format!("{} in-scope pairs all ok", scoped.len())
            }),
            retry_task_id: None,
        };
    }

    let failures: Vec<String> = scoped
        .into_iter()
        .filter(|row| row.status != "ok")
        .map(|row| format!("{}→{}", row.src_node, row.dst_node))
        .collect();
    CheckResult {
        check: "mesh_ssh_complete".into(),
        status: "fail".into(),
        message: Some(if exclusions.is_empty() {
            format!("{} pair(s) failed: {}", failures.len(), failures.join(", "))
        } else {
            format!(
                "{} in-scope pair(s) failed: {}",
                failures.len(),
                failures.join(", ")
            )
        }),
        retry_task_id: None,
    }
}

async fn check_daemon_healthy(node: &ff_db::FleetNodeRow) -> CheckResult {
    if node.status == "offline" {
        return CheckResult {
            check: "daemon_healthy".into(),
            status: "fail".into(),
            message: Some(format!("node status in DB is '{}'", node.status)),
            retry_task_id: None,
        };
    }
    let addr = format!("{}:22", node.ip);
    let probe = timeout(
        Duration::from_secs(3),
        tokio::net::TcpStream::connect(&addr),
    )
    .await;
    match probe {
        Ok(Ok(_)) => CheckResult {
            check: "daemon_healthy".into(),
            status: "pass".into(),
            message: Some(format!("SSH port reachable on {}", node.ip)),
            retry_task_id: None,
        },
        _ => CheckResult {
            check: "daemon_healthy".into(),
            status: "fail".into(),
            message: Some(format!("SSH port 22 unreachable on {}", node.ip)),
            retry_task_id: None,
        },
    }
}

async fn check_ssh_cmd(dest: &str, name: &str, cmd: &str) -> CheckResult {
    match ssh_capture(dest, cmd).await {
        Ok(_) => CheckResult {
            check: name.into(),
            status: "pass".into(),
            message: None,
            retry_task_id: None,
        },
        Err(e) => CheckResult {
            check: name.into(),
            status: "fail".into(),
            message: Some(e),
            retry_task_id: None,
        },
    }
}

async fn ssh_capture(dest: &str, cmd: &str) -> Result<String, String> {
    let out = timeout(
        Duration::from_secs(10),
        Command::new("ssh")
            .args(crate::ssh_opts::ssh_bypass_args())
            .args([
                "-o",
                "ConnectTimeout=5",
                "-o",
                "StrictHostKeyChecking=accept-new",
                dest,
                cmd,
            ])
            .output(),
    )
    .await
    .map_err(|_| "ssh timeout".to_string())?
    .map_err(|e| format!("spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "exit {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr)
                .trim()
                .chars()
                .take(140)
                .collect::<String>()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn redis_check_command(is_windows: bool) -> &'static str {
    if is_windows {
        r#"powershell -NoProfile -Command "$out = & \"$env:USERPROFILE\.local\bin\ff.exe\" status 2>&1 | Out-String; if ($out -match 'Redis.*PONG') { exit 0 } else { exit 1 }""#
    } else {
        "PATH=\"$HOME/.local/bin:$HOME/.cargo/bin:$PATH\" && ff status 2>&1 | grep -q 'Redis.*PONG'"
    }
}

fn tooling_check_command(is_windows: bool) -> &'static str {
    if is_windows {
        r#"powershell -NoProfile -Command "$missing = @(); foreach ($t in 'gh','git','codex','claude') { if (-not (Get-Command $t -ErrorAction SilentlyContinue)) { $missing += $t } }; if ($missing.Count -eq 0) { exit 0 } else { [Console]::Error.WriteLine(('missing required tools: ' + ($missing -join ', '))); exit 1 }""#
    } else {
        "PATH=\"$HOME/.local/bin:$HOME/.cargo/bin:$PATH\"; missing=\"\"; for t in gh op codex claude; do if ! command -v \"$t\" >/dev/null 2>&1; then missing=\"$missing $t\"; fi; done; if [ -n \"$missing\" ]; then printf 'missing required tools:%s\\n' \"$missing\" >&2; exit 1; fi"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_redis_command_uses_ff_configured_status() {
        let cmd = redis_check_command(false);
        assert!(!cmd.contains("192.168.5.100"));
        assert!(!cmd.contains("nc"));
        assert!(cmd.contains("ff status"));
        assert!(cmd.contains("Redis.*PONG"));
        assert!(cmd.contains("$HOME/.local/bin"));
        assert!(cmd.contains("$HOME/.cargo/bin"));
        assert!(!cmd.contains("--no-color"));
    }

    #[test]
    fn windows_redis_command_uses_ff_configured_status() {
        let cmd = redis_check_command(true);
        assert!(!cmd.contains("192.168.5.100"));
        assert!(!cmd.contains("nc"));
        assert!(cmd.contains("ff.exe"));
        assert!(cmd.contains("Redis.*PONG"));
        assert!(!cmd.contains("--no-color"));
    }

    #[test]
    fn unix_tooling_command_normalizes_non_login_path() {
        let cmd = tooling_check_command(false);
        assert!(cmd.contains("$HOME/.local/bin"));
        assert!(cmd.contains("$HOME/.cargo/bin"));
        assert!(cmd.contains("command -v"));
        assert!(!cmd.contains("which"));
    }

    #[test]
    fn unix_tooling_command_requires_and_reports_every_tool() {
        let cmd = tooling_check_command(false);
        assert!(cmd.contains("for t in gh op codex claude"));
        assert!(cmd.contains("missing required tools:"));
        assert!(cmd.contains("[ -n \"$missing\" ]"));
        assert!(!cmd.contains("-ge 3"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_tooling_command_fails_three_of_four_then_passes_four_of_four() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().expect("temp home");
        let bin = home.path().join(".local/bin");
        let empty_path = home.path().join("empty-path");
        std::fs::create_dir_all(&bin).expect("create fake bin");
        std::fs::create_dir_all(&empty_path).expect("create empty PATH");

        let install_tool = |name: &str| {
            let path = bin.join(name);
            std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write fake tool");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("make fake tool executable");
        };
        for tool in ["gh", "op", "codex"] {
            install_tool(tool);
        }

        let run_check = || {
            std::process::Command::new("/bin/sh")
                .args(["-c", tooling_check_command(false)])
                .env("HOME", home.path())
                .env("PATH", &empty_path)
                .output()
                .expect("run tooling check")
        };

        let missing_one = run_check();
        assert!(!missing_one.status.success());
        assert!(String::from_utf8_lossy(&missing_one.stderr).contains("claude"));

        install_tool("claude");
        let complete = run_check();
        assert!(complete.status.success());
        assert!(complete.stderr.is_empty());
    }

    #[test]
    fn windows_tooling_command_requires_and_reports_every_tool() {
        let cmd = tooling_check_command(true);
        assert!(cmd.contains("'gh','git','codex','claude'"));
        assert!(cmd.contains("missing required tools:"));
        assert!(cmd.contains("$missing.Count -eq 0"));
        assert!(!cmd.contains("$c -ge 3"));
    }

    fn mesh_row(src: &str, dst: &str, status: &str) -> ff_db::MeshStatusRow {
        ff_db::MeshStatusRow {
            src_node: src.into(),
            dst_node: dst.into(),
            status: status.into(),
            last_checked: Some(chrono::Utc::now()),
            last_error: (status != "ok").then(|| "unreachable".into()),
            attempts: 1,
        }
    }

    #[test]
    fn mesh_verification_ignores_excluded_rows_only_when_explicit() {
        let rows = vec![
            mesh_row("Logan", "Vinny", "failed"),
            mesh_row("Vinny", "Logan", "failed"),
            mesh_row("Logan", "Sia", "ok"),
        ];

        let legacy = mesh_ssh_complete_result(&rows, &crate::mesh_check::MeshExclusions::default());
        assert_eq!(legacy.status, "fail");
        let legacy_message = legacy.message.expect("legacy failure message");
        assert!(legacy_message.contains("Logan→Vinny"));
        assert!(legacy_message.contains("Vinny→Logan"));
        assert!(!legacy_message.contains("in-scope"));

        let exclusions = crate::mesh_check::MeshExclusions::from_canonical_names(["Vinny".into()]);
        let scoped = mesh_ssh_complete_result(&rows, &exclusions);
        assert_eq!(scoped.status, "pass");
        assert_eq!(scoped.message.as_deref(), Some("1 in-scope pairs all ok"));

        let mut other_failure = rows;
        other_failure.push(mesh_row("Logan", "Beyonce", "failed"));
        let scoped = mesh_ssh_complete_result(&other_failure, &exclusions);
        assert_eq!(scoped.status, "fail");
        let scoped_message = scoped.message.expect("scoped failure message");
        assert!(scoped_message.contains("Logan→Beyonce"));
        assert!(!scoped_message.contains("Vinny"));
    }
}
