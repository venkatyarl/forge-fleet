//! Computer tools — interact with the OS, processes, clipboard, display, and system services.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

/// ProcessManager — list, kill, monitor running processes.
pub struct ProcessManagerTool;

#[async_trait]
impl AgentTool for ProcessManagerTool {
    fn name(&self) -> &str { "ProcessManager" }
    fn description(&self) -> &str { "List, search, and manage running processes. View CPU/memory usage, kill processes." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["list","search","kill","top"]},
            "query":{"type":"string","description":"Process name or PID to search/kill"},
            "signal":{"type":"string","description":"Signal to send (default: TERM)"}
        },"required":["action"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        match action {
            "list" => {
                let out = Command::new("ps").args(["aux", "--sort=-%mem"]).output().await
                    .or_else(|_| futures::executor::block_on(Command::new("ps").args(["aux"]).output()));
                match out {
                    Ok(o) => AgentToolResult::ok(truncate_output(&String::from_utf8_lossy(&o.stdout), MAX_TOOL_RESULT_CHARS)),
                    Err(e) => AgentToolResult::err(format!("ps failed: {e}")),
                }
            }
            "search" => {
                let query = input.get("query").and_then(Value::as_str).unwrap_or("");
                let cmd = format!("ps aux | grep -i '{}' | grep -v grep", query);
                match Command::new("bash").arg("-c").arg(&cmd).output().await {
                    Ok(o) => AgentToolResult::ok(truncate_output(&String::from_utf8_lossy(&o.stdout), MAX_TOOL_RESULT_CHARS)),
                    Err(e) => AgentToolResult::err(format!("Search failed: {e}")),
                }
            }
            "kill" => {
                let query = input.get("query").and_then(Value::as_str).unwrap_or("");
                let signal = input.get("signal").and_then(Value::as_str).unwrap_or("TERM");
                if query.is_empty() { return AgentToolResult::err("'query' (PID or process name) required"); }

                // If numeric, kill by PID
                if query.parse::<u32>().is_ok() {
                    match Command::new("kill").args([&format!("-{signal}"), query]).output().await {
                        Ok(o) if o.status.success() => AgentToolResult::ok(format!("Sent {signal} to PID {query}")),
                        _ => AgentToolResult::err(format!("Failed to kill PID {query}")),
                    }
                } else {
                    // Kill by name
                    match Command::new("pkill").args([&format!("-{signal}"), "-f", query]).output().await {
                        Ok(o) if o.status.success() => AgentToolResult::ok(format!("Sent {signal} to processes matching '{query}'")),
                        _ => AgentToolResult::err(format!("No processes matching '{query}'")),
                    }
                }
            }
            "top" => {
                // Get top 10 by CPU
                let cmd = "ps aux --sort=-%cpu 2>/dev/null | head -11 || ps aux | head -11";
                match Command::new("bash").arg("-c").arg(cmd).output().await {
                    Ok(o) => AgentToolResult::ok(format!("Top processes by CPU:\n{}", String::from_utf8_lossy(&o.stdout))),
                    Err(e) => AgentToolResult::err(format!("top failed: {e}")),
                }
            }
            _ => AgentToolResult::err(format!("Unknown action: {action}")),
        }
    }
}

/// ClipboardTool — read and write system clipboard.
pub struct ClipboardTool;

#[async_trait]
impl AgentTool for ClipboardTool {
    fn name(&self) -> &str { "Clipboard" }
    fn description(&self) -> &str { "Read from or write to the system clipboard." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["read","write"]},
            "content":{"type":"string","description":"Content to write to clipboard (for write action)"}
        },"required":["action"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        match action {
            "read" => {
                // macOS: pbpaste, Linux: xclip -o
                let result = Command::new("pbpaste").output().await
                    .or_else(|_| futures::executor::block_on(Command::new("xclip").args(["-selection", "clipboard", "-o"]).output()));
                match result {
                    Ok(o) if o.status.success() => AgentToolResult::ok(truncate_output(&String::from_utf8_lossy(&o.stdout), MAX_TOOL_RESULT_CHARS)),
                    _ => AgentToolResult::err("Clipboard read failed".to_string()),
                }
            }
            "write" => {
                let content = input.get("content").and_then(Value::as_str).unwrap_or("");
                // macOS: pbcopy, Linux: xclip
                let result = Command::new("bash").arg("-c")
                    .arg(format!("echo -n '{}' | pbcopy 2>/dev/null || echo -n '{}' | xclip -selection clipboard", content.replace('\'', "'\"'\"'"), content.replace('\'', "'\"'\"'")))
                    .output().await;
                match result {
                    Ok(o) if o.status.success() => AgentToolResult::ok("Content copied to clipboard".to_string()),
                    _ => AgentToolResult::err("Clipboard write failed".to_string()),
                }
            }
            _ => AgentToolResult::err(format!("Unknown action: {action}")),
        }
    }
}

/// SystemControl — system-level operations (sleep, lock, volume, brightness, notifications).
pub struct SystemControlTool;

#[async_trait]
impl AgentTool for SystemControlTool {
    fn name(&self) -> &str { "SystemControl" }
    fn description(&self) -> &str { "Control system settings: open apps, manage displays, check battery, system notifications." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["open_app","open_url","notify","battery","uptime","disk_usage","whoami"]},
            "target":{"type":"string","description":"App name, URL, or notification message"},
            "title":{"type":"string","description":"Notification title (for notify)"}
        },"required":["action"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        let target = input.get("target").and_then(Value::as_str).unwrap_or("");

        match action {
            "open_app" => {
                #[cfg(target_os = "macos")]
                { let _ = Command::new("open").arg("-a").arg(target).output().await; }
                #[cfg(target_os = "linux")]
                { let _ = Command::new("xdg-open").arg(target).output().await; }
                AgentToolResult::ok(format!("Opened: {target}"))
            }
            "open_url" => {
                #[cfg(target_os = "macos")]
                { let _ = Command::new("open").arg(target).output().await; }
                #[cfg(target_os = "linux")]
                { let _ = Command::new("xdg-open").arg(target).output().await; }
                AgentToolResult::ok(format!("Opened URL: {target}"))
            }
            "notify" => {
                let title = input.get("title").and_then(Value::as_str).unwrap_or("ForgeFleet");
                #[cfg(target_os = "macos")]
                {
                    let script = format!(r#"display notification "{}" with title "{}""#, target.replace('"', "\\\""), title.replace('"', "\\\""));
                    let _ = Command::new("osascript").arg("-e").arg(&script).output().await;
                }
                #[cfg(target_os = "linux")]
                { let _ = Command::new("notify-send").arg(title).arg(target).output().await; }
                AgentToolResult::ok(format!("Notification sent: {title} — {target}"))
            }
            "battery" => {
                #[cfg(target_os = "macos")]
                match Command::new("pmset").args(["-g", "batt"]).output().await {
                    Ok(o) => return AgentToolResult::ok(String::from_utf8_lossy(&o.stdout).to_string()),
                    Err(_) => {}
                }
                match Command::new("cat").arg("/sys/class/power_supply/BAT0/capacity").output().await {
                    Ok(o) => AgentToolResult::ok(format!("Battery: {}%", String::from_utf8_lossy(&o.stdout).trim())),
                    Err(_) => AgentToolResult::ok("Battery info not available (desktop?)".to_string()),
                }
            }
            "uptime" => {
                match Command::new("uptime").output().await {
                    Ok(o) => AgentToolResult::ok(String::from_utf8_lossy(&o.stdout).trim().to_string()),
                    Err(e) => AgentToolResult::err(format!("uptime failed: {e}")),
                }
            }
            "disk_usage" => {
                match Command::new("df").args(["-h"]).output().await {
                    Ok(o) => AgentToolResult::ok(truncate_output(&String::from_utf8_lossy(&o.stdout), MAX_TOOL_RESULT_CHARS)),
                    Err(e) => AgentToolResult::err(format!("df failed: {e}")),
                }
            }
            "whoami" => {
                let user = std::env::var("USER").unwrap_or_default();
                let home = std::env::var("HOME").unwrap_or_default();
                let hostname = Command::new("hostname").output().await.ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
                AgentToolResult::ok(format!("User: {user}\nHome: {home}\nHostname: {hostname}"))
            }
            _ => AgentToolResult::err(format!("Unknown action: {action}")),
        }
    }
}

/// ServiceManager — manage system services (systemd, launchd, brew services).
pub struct ServiceManagerTool;

#[async_trait]
impl AgentTool for ServiceManagerTool {
    fn name(&self) -> &str { "ServiceManager" }
    fn description(&self) -> &str { "Manage system services: list, start, stop, restart, status. Supports systemd (Linux) and launchd/brew services (macOS)." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["list","start","stop","restart","status"]},
            "service":{"type":"string","description":"Service name"}
        },"required":["action"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        let service = input.get("service").and_then(Value::as_str).unwrap_or("");

        // Detect service manager
        let is_macos = cfg!(target_os = "macos");

        match action {
            "list" => {
                let cmd = if is_macos { "brew services list 2>/dev/null || launchctl list | head -20" }
                    else { "systemctl list-units --type=service --state=running --no-pager | head -30" };
                match Command::new("bash").arg("-c").arg(cmd).output().await {
                    Ok(o) => AgentToolResult::ok(truncate_output(&String::from_utf8_lossy(&o.stdout), MAX_TOOL_RESULT_CHARS)),
                    Err(e) => AgentToolResult::err(format!("Service list failed: {e}")),
                }
            }
            "start" | "stop" | "restart" | "status" => {
                if service.is_empty() { return AgentToolResult::err("'service' name required"); }
                let cmd = if is_macos {
                    format!("brew services {action} {service} 2>/dev/null || launchctl {action} {service}")
                } else {
                    format!("systemctl {action} {service}")
                };
                match Command::new("bash").arg("-c").arg(&cmd).output().await {
                    Ok(o) => {
                        let combined = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
                        if o.status.success() { AgentToolResult::ok(combined) } else { AgentToolResult::err(combined) }
                    }
                    Err(e) => AgentToolResult::err(format!("Service {action} failed: {e}")),
                }
            }
            _ => AgentToolResult::err(format!("Unknown action: {action}")),
        }
    }
}

/// PackageManager — install, update, search system packages.
pub struct PackageManagerTool;

#[async_trait]
impl AgentTool for PackageManagerTool {
    fn name(&self) -> &str { "PackageManager" }
    fn description(&self) -> &str { "Manage system packages: search, install, update. Auto-detects brew (macOS), apt (Ubuntu), or dnf (Fedora)." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["search","install","update","list","info"]},
            "package":{"type":"string","description":"Package name"}
        },"required":["action"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        let package = input.get("package").and_then(Value::as_str).unwrap_or("");

        // Detect package manager
        let pm = if cfg!(target_os = "macos") { "brew" }
            else if std::path::Path::new("/usr/bin/apt").exists() { "apt" }
            else if std::path::Path::new("/usr/bin/dnf").exists() { "dnf" }
            else { "unknown" };

        if pm == "unknown" { return AgentToolResult::err("No supported package manager found".to_string()); }

        let cmd = match (action, pm) {
            ("search", "brew") => format!("brew search {package}"),
            ("search", "apt") => format!("apt search {package} 2>/dev/null | head -20"),
            ("search", _) => format!("dnf search {package} | head -20"),
            ("install", "brew") => format!("brew install {package}"),
            ("install", "apt") => format!("sudo apt install -y {package}"),
            ("install", _) => format!("sudo dnf install -y {package}"),
            ("update", "brew") => "brew update && brew upgrade".to_string(),
            ("update", "apt") => "sudo apt update && sudo apt upgrade -y".to_string(),
            ("update", _) => "sudo dnf update -y".to_string(),
            ("list", "brew") => "brew list".to_string(),
            ("list", "apt") => "dpkg --list | tail -20".to_string(),
            ("list", _) => "dnf list installed | tail -20".to_string(),
            ("info", "brew") => format!("brew info {package}"),
            ("info", "apt") => format!("apt show {package} 2>/dev/null"),
            ("info", _) => format!("dnf info {package}"),
            _ => return AgentToolResult::err(format!("Unknown action: {action}")),
        };

        match Command::new("bash").arg("-c").arg(&cmd).output().await {
            Ok(o) => {
                let output = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
                AgentToolResult::ok(truncate_output(&output, MAX_TOOL_RESULT_CHARS))
            }
            Err(e) => AgentToolResult::err(format!("{pm} {action} failed: {e}")),
        }
    }
}
