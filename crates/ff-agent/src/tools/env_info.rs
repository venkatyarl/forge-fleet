//! EnvInfo tool — system information and fleet diagnostics.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{AgentTool, AgentToolContext, AgentToolResult};

pub struct EnvInfoTool;

#[async_trait]
impl AgentTool for EnvInfoTool {
    fn name(&self) -> &str {
        "EnvInfo"
    }

    fn description(&self) -> &str {
        "Get system environment information: OS, CPU, memory, disk, network interfaces, and running processes."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "section": {
                    "type": "string",
                    "enum": ["all", "os", "cpu", "memory", "disk", "network", "env_vars"],
                    "description": "What info to retrieve (default: all)"
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let section = input
            .get("section")
            .and_then(Value::as_str)
            .unwrap_or("all");
        let mut info = Vec::new();

        if section == "all" || section == "os" {
            info.push(format!(
                "OS: {} {}",
                std::env::consts::OS,
                std::env::consts::ARCH
            ));
            if let Ok(hostname) = tokio::process::Command::new("hostname").output().await {
                info.push(format!(
                    "Hostname: {}",
                    String::from_utf8_lossy(&hostname.stdout).trim()
                ));
            }
        }

        if section == "all" || section == "cpu" {
            if let Ok(out) = tokio::process::Command::new("sysctl")
                .args(["-n", "hw.ncpu"])
                .output()
                .await
            {
                info.push(format!(
                    "CPU cores: {}",
                    String::from_utf8_lossy(&out.stdout).trim()
                ));
            } else if let Ok(out) = tokio::process::Command::new("nproc").output().await {
                info.push(format!(
                    "CPU cores: {}",
                    String::from_utf8_lossy(&out.stdout).trim()
                ));
            }
        }

        if section == "all" || section == "memory" {
            #[cfg(target_os = "macos")]
            if let Ok(out) = tokio::process::Command::new("sysctl")
                .args(["-n", "hw.memsize"])
                .output()
                .await
            {
                let bytes: u64 = String::from_utf8_lossy(&out.stdout)
                    .trim()
                    .parse()
                    .unwrap_or(0);
                info.push(format!("Memory: {} GB", bytes / 1_073_741_824));
            }
            #[cfg(target_os = "linux")]
            if let Ok(out) = tokio::process::Command::new("free")
                .args(["-h"])
                .output()
                .await
            {
                info.push(format!("Memory:\n{}", String::from_utf8_lossy(&out.stdout)));
            }
        }

        if section == "all" || section == "disk" {
            if let Ok(out) = tokio::process::Command::new("df")
                .args(["-h", "."])
                .current_dir(&ctx.working_dir)
                .output()
                .await
            {
                info.push(format!("Disk:\n{}", String::from_utf8_lossy(&out.stdout)));
            }
        }

        if section == "all" || section == "env_vars" {
            info.push(format!("Working dir: {}", ctx.working_dir.display()));
            info.push(format!(
                "HOME: {}",
                std::env::var("HOME").unwrap_or_default()
            ));
            info.push(format!(
                "USER: {}",
                std::env::var("USER").unwrap_or_default()
            ));
            info.push(format!(
                "SHELL: {}",
                std::env::var("SHELL").unwrap_or_default()
            ));
        }

        AgentToolResult::ok(info.join("\n"))
    }
}
