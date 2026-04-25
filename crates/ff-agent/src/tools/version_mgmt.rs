//! Version management — ForgeFleet versioning, upgrades, and fleet-wide updates.
//!
//! Versioning scheme: CalVer — vYYYY.MM.DD (e.g. v2026.04.07)
//! - Multiple releases same day: v2026.04.07.01, v2026.04.07.02
//!
//! Release channels:
//! - **stable**: weekly (Monday) or on-demand, auto-deploys to all fleet nodes overnight
//! - **nightly**: daily midnight build from passing commits, canary on Taylor (leader)
//! - **hotfix**: critical bug fixes, deploy immediately to all nodes
//!
//! Release flow:
//! 1. Commit → tests pass → nightly tag (canary on Taylor)
//! 2. If healthy 24h → promote to stable
//! 3. Stable auto-deploys to all fleet nodes
//! 4. If broken → auto-rollback to previous stable
//!
//! Bug fix: commit → canary 2h → fleet deploy
//! Feature: commit → nightly → canary 24h → next weekly stable
//! Critical: hotfix tag → skip canary → deploy everywhere immediately

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

pub struct VersionManagerTool;

#[async_trait]
impl AgentTool for VersionManagerTool {
    fn name(&self) -> &str {
        "VersionManager"
    }
    fn description(&self) -> &str {
        "Manage ForgeFleet versions: check current version, check for updates, upgrade, deploy to fleet nodes, rollback if needed."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["current","check_update","upgrade","deploy_fleet","rollback","changelog","tag_release"]},
            "version":{"type":"string","description":"Version to deploy or rollback to (auto-generated if empty)"},
            "channel":{"type":"string","enum":["stable","nightly","hotfix"],"description":"Release channel (default: stable)"},
            "node":{"type":"string","description":"Specific node to deploy to (default: all)"},
            "force":{"type":"boolean","description":"Force upgrade even if tests fail (default: false)"}
        },"required":["action"]})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");

        match action {
            "current" => {
                let version = env!("CARGO_PKG_VERSION");
                let git_hash = Command::new("git")
                    .args(["rev-parse", "--short", "HEAD"])
                    .current_dir(&ctx.working_dir)
                    .output()
                    .await
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_else(|| "unknown".into());

                let git_branch = Command::new("git")
                    .args(["branch", "--show-current"])
                    .current_dir(&ctx.working_dir)
                    .output()
                    .await
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_else(|| "unknown".into());

                let commit_count = Command::new("git")
                    .args(["rev-list", "--count", "HEAD"])
                    .current_dir(&ctx.working_dir)
                    .output()
                    .await
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_else(|| "?".into());

                let last_commit = Command::new("git")
                    .args(["log", "-1", "--format=%s (%ar)"])
                    .current_dir(&ctx.working_dir)
                    .output()
                    .await
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_default();

                AgentToolResult::ok(format!(
                    "ForgeFleet Version:\n\
                     \n  Version: {version}\
                     \n  Git: {git_hash} ({git_branch})\
                     \n  Commits: {commit_count}\
                     \n  Last: {last_commit}\
                     \n  Binary: {}\
                     \n  Built: {}",
                    std::env::current_exe()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| "unknown".into()),
                    env!("CARGO_PKG_VERSION"),
                ))
            }

            "check_update" => {
                // Check for new commits on remote
                let _fetch = Command::new("git")
                    .args(["fetch", "--dry-run"])
                    .current_dir(&ctx.working_dir)
                    .output()
                    .await;

                let local = Command::new("git")
                    .args(["rev-parse", "HEAD"])
                    .current_dir(&ctx.working_dir)
                    .output()
                    .await
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_default();

                let remote = Command::new("git")
                    .args(["rev-parse", "origin/master"])
                    .current_dir(&ctx.working_dir)
                    .output()
                    .await
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_default();

                if local == remote {
                    AgentToolResult::ok("ForgeFleet is up to date.".to_string())
                } else {
                    let behind = Command::new("git")
                        .args(["rev-list", "--count", &format!("{local}..origin/master")])
                        .current_dir(&ctx.working_dir)
                        .output()
                        .await
                        .ok()
                        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                        .unwrap_or_else(|| "?".into());

                    AgentToolResult::ok(format!(
                        "Update available: {behind} commits behind origin/master.\nRun VersionManager upgrade to update."
                    ))
                }
            }

            "upgrade" => {
                let mut steps = Vec::new();

                // Step 1: Pull latest
                let pull = Command::new("git")
                    .args(["pull", "--rebase"])
                    .current_dir(&ctx.working_dir)
                    .output()
                    .await;
                match pull {
                    Ok(o) if o.status.success() => steps.push("1. Git pull: OK".into()),
                    Ok(o) => {
                        return AgentToolResult::err(format!(
                            "Git pull failed:\n{}",
                            String::from_utf8_lossy(&o.stderr)
                        ));
                    }
                    Err(e) => return AgentToolResult::err(format!("Git pull failed: {e}")),
                }

                // Step 2: Build
                steps.push("2. Building (cargo build --release)...".into());
                let build = Command::new("cargo")
                    .args(["build", "--release", "-p", "ff-terminal"])
                    .current_dir(&ctx.working_dir)
                    .output()
                    .await;
                match build {
                    Ok(o) if o.status.success() => steps.push("   Build: OK".into()),
                    Ok(o) => {
                        return AgentToolResult::err(format!(
                            "Build failed:\n{}",
                            truncate_output(&String::from_utf8_lossy(&o.stderr), 2000)
                        ));
                    }
                    Err(e) => return AgentToolResult::err(format!("Build failed: {e}")),
                }

                // Step 3: Run tests
                let force = input.get("force").and_then(Value::as_bool).unwrap_or(false);
                let test = Command::new("cargo")
                    .args(["test", "--workspace"])
                    .current_dir(&ctx.working_dir)
                    .output()
                    .await;
                match test {
                    Ok(o) if o.status.success() => steps.push("3. Tests: PASS".into()),
                    Ok(o) if force => steps.push(format!(
                        "3. Tests: FAIL (forced upgrade)\n   {}",
                        String::from_utf8_lossy(&o.stderr)
                            .lines()
                            .last()
                            .unwrap_or("")
                    )),
                    Ok(_) => {
                        return AgentToolResult::err(
                            "Tests failed. Use force=true to upgrade anyway.".to_string(),
                        );
                    }
                    Err(e) => return AgentToolResult::err(format!("Test command failed: {e}")),
                }

                // Step 4: Get new version info
                let new_hash = Command::new("git")
                    .args(["rev-parse", "--short", "HEAD"])
                    .current_dir(&ctx.working_dir)
                    .output()
                    .await
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_default();

                steps.push(format!("4. Updated to: {new_hash}"));
                steps.push("5. Restart ForgeFleet to use the new version.".into());

                AgentToolResult::ok(format!("Upgrade Complete:\n\n{}", steps.join("\n")))
            }

            "deploy_fleet" => {
                let target_node = input.get("node").and_then(Value::as_str);
                let nodes: Vec<(&str, &str)> = if let Some(node) = target_node {
                    vec![(node, node)]
                } else {
                    vec![
                        ("Marcus", "192.168.5.102"),
                        ("Sophie", "192.168.5.103"),
                        ("Priya", "192.168.5.104"),
                        ("James", "192.168.5.108"),
                    ]
                };

                let binary_path = ctx.working_dir.join("target/release/ff");
                if !binary_path.exists() {
                    return AgentToolResult::err(
                        "Binary not found. Run 'VersionManager upgrade' first to build."
                            .to_string(),
                    );
                }

                let mut results = Vec::new();
                for (name, ip) in &nodes {
                    let scp = Command::new("scp")
                        .args([
                            &binary_path.to_string_lossy().to_string(),
                            &format!("root@{ip}:/usr/local/bin/ff"),
                        ])
                        .output()
                        .await;
                    match scp {
                        Ok(o) if o.status.success() => {
                            results.push(format!("  {name} ({ip}): deployed"))
                        }
                        _ => results.push(format!("  {name} ({ip}): FAILED")),
                    }
                }

                AgentToolResult::ok(format!("Fleet Deploy:\n{}", results.join("\n")))
            }

            "rollback" => {
                let version = input
                    .get("version")
                    .and_then(Value::as_str)
                    .unwrap_or("HEAD~1");
                let result = Command::new("git")
                    .args(["checkout", version, "--", "."])
                    .current_dir(&ctx.working_dir)
                    .output()
                    .await;
                match result {
                    Ok(o) if o.status.success() => AgentToolResult::ok(format!(
                        "Rolled back to {version}. Rebuild with: cargo build --release -p ff-terminal"
                    )),
                    _ => AgentToolResult::err(format!("Rollback to {version} failed")),
                }
            }

            "changelog" => {
                let since = input
                    .get("version")
                    .and_then(Value::as_str)
                    .unwrap_or("HEAD~20");
                let output = Command::new("git")
                    .args(["log", "--oneline", &format!("{since}..HEAD")])
                    .current_dir(&ctx.working_dir)
                    .output()
                    .await;
                match output {
                    Ok(o) if o.status.success() => AgentToolResult::ok(format!(
                        "Changelog since {since}:\n\n{}",
                        truncate_output(&String::from_utf8_lossy(&o.stdout), MAX_TOOL_RESULT_CHARS)
                    )),
                    _ => AgentToolResult::err("Changelog generation failed".to_string()),
                }
            }

            "tag_release" => {
                let channel = input
                    .get("channel")
                    .and_then(Value::as_str)
                    .unwrap_or("stable");
                let version = input.get("version").and_then(Value::as_str).unwrap_or("");

                let tag = if !version.is_empty() {
                    format!("v{version}")
                } else {
                    // Auto-generate CalVer: vYYYY.MM.DD
                    let now = chrono::Utc::now();
                    let base = format!(
                        "v{}.{:02}.{:02}",
                        now.format("%Y"),
                        now.format("%m"),
                        now.format("%d")
                    );

                    // Check if today's tag already exists — if so, add patch
                    let existing = Command::new("git")
                        .args(["tag", "-l", &format!("{base}*")])
                        .current_dir(&ctx.working_dir)
                        .output()
                        .await
                        .ok()
                        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                        .unwrap_or_default();

                    let existing_count = existing.lines().filter(|l| !l.trim().is_empty()).count();
                    if existing_count == 0 {
                        base
                    } else {
                        format!("{base}.{:02}", existing_count)
                    }
                };

                let suffix = match channel {
                    "nightly" => "-nightly",
                    "hotfix" => "-hotfix",
                    _ => "",
                };
                let full_tag = format!("{tag}{suffix}");

                let message = match channel {
                    "stable" => format!("Stable release {full_tag}"),
                    "nightly" => format!("Nightly build {full_tag}"),
                    "hotfix" => format!("Hotfix {full_tag}"),
                    _ => format!("Release {full_tag}"),
                };

                let result = Command::new("git")
                    .args(["tag", "-a", &full_tag, "-m", &message])
                    .current_dir(&ctx.working_dir)
                    .output()
                    .await;
                match result {
                    Ok(o) if o.status.success() => AgentToolResult::ok(format!(
                        "Tagged: {full_tag}\nChannel: {channel}\nMessage: {message}\n\n\
                             Next steps:\n\
                             - Push tag: git push origin {full_tag}\n\
                             - Deploy: VersionManager deploy_fleet\n\
                             - Verify: ff health"
                    )),
                    Ok(o) => AgentToolResult::err(format!(
                        "Tagging failed: {}",
                        String::from_utf8_lossy(&o.stderr)
                    )),
                    Err(e) => AgentToolResult::err(format!("Git tag command failed: {e}")),
                }
            }

            _ => AgentToolResult::err(format!("Unknown action: {action}")),
        }
    }
}
