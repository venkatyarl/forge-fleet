use anyhow::{Context, Result};
use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;
use crate::{CYAN, GREEN, RED, RESET, YELLOW};

pub async fn handle_stop() -> Result<()> {
    println!("{CYAN}▶ Stopping ForgeFleet{RESET}");

    // Kill forgefleetd
    let kill = tokio::process::Command::new("pkill")
        .args(["-f", "forgefleetd"])
        .output()
        .await;
    match kill {
        Ok(o) if o.status.success() => println!("  {GREEN}✓ Daemon stopped{RESET}"),
        _ => println!("  {YELLOW}⚠ No daemon process found{RESET}"),
    }

    // Verify
    tokio::time::sleep(Duration::from_secs(1)).await;
    static SHARED_HTTP: std::sync::LazyLock<reqwest::Client> =
        std::sync::LazyLock::new(|| reqwest::Client::new());
    let still_running = SHARED_HTTP
        .get(format!(
            "http://127.0.0.1:{}/health",
            ff_terminal::app::PORT_DAEMON
        ))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);

    if still_running {
        println!("  {RED}✗ Daemon still running — try: kill $(pgrep forgefleetd){RESET}");
    } else {
        println!("  {GREEN}✓ ForgeFleet stopped{RESET}");
    }
    Ok(())
}
pub fn resolve_config_path(p: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = p {
        return Ok(p);
    }
    Ok(PathBuf::from(env::var("HOME").context("HOME not set")?)
        .join(".forgefleet")
        .join("fleet.toml"))
}
pub async fn handle_start(leader: bool, config_path: &Path, working_dir: &Path) -> Result<()> {
    println!("{CYAN}▶ Starting ForgeFleet{RESET}");
    println!("  Config: {}", config_path.display());
    println!("  Mode:   {}", if leader { "leader" } else { "auto" });
    println!();

    // Check if daemon is already running (check web UI port — only daemon serves this)
    static SHARED_HTTP: std::sync::LazyLock<reqwest::Client> =
        std::sync::LazyLock::new(|| reqwest::Client::new());
    let daemon_running = SHARED_HTTP
        .get(format!(
            "http://127.0.0.1:{}/health",
            ff_terminal::app::PORT_WEB
        ))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    if daemon_running {
        println!("{GREEN}✓ ForgeFleet daemon is already running{RESET}");
        println!(
            "  Daemon:    http://localhost:{}",
            ff_terminal::app::PORT_DAEMON
        );
        println!(
            "  Web UI:    http://localhost:{}",
            ff_terminal::app::PORT_WEB
        );
        println!("  WebSocket: ws://localhost:{}", ff_terminal::app::PORT_WS);
        return Ok(());
    }

    // Step 1: Find and start LLM server
    println!("{YELLOW}1/4{RESET} Checking LLM server...");
    let llm_running = std::net::TcpStream::connect_timeout(
        &"127.0.0.1:51000".parse().unwrap(),
        Duration::from_millis(500),
    )
    .is_ok();

    if llm_running {
        println!("  {GREEN}✓ LLM server already running on :51000{RESET}");
    } else {
        println!("  {YELLOW}⚠ No LLM server detected locally{RESET}");
        println!("  Start one with: ollama serve & ollama run qwen2.5-coder:32b");
        println!(
            "  Or: llama-server -m /path/to/model.gguf --host 0.0.0.0 --port 51000 --ctx-size 32768"
        );
    }

    // Step 2: Start ForgeFleet daemon
    println!("{YELLOW}2/4{RESET} Starting ForgeFleet daemon...");

    // Find the forgefleetd binary
    let daemon_binary = find_daemon_binary(working_dir);
    match daemon_binary {
        Some(bin) => {
            let mut cmd = tokio::process::Command::new(&bin);
            cmd.arg("--config").arg(config_path);
            if leader {
                cmd.arg("start").arg("--leader");
            }

            // Spawn as background process
            match cmd
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(child) => {
                    println!(
                        "  {GREEN}✓ Daemon started (PID: {}){RESET}",
                        child.id().unwrap_or(0)
                    );

                    // Wait a moment for it to boot
                    tokio::time::sleep(Duration::from_secs(2)).await;

                    // Verify it's running
                    let health = SHARED_HTTP
                        .get(format!(
                            "http://127.0.0.1:{}/health",
                            ff_terminal::app::PORT_DAEMON
                        ))
                        .send()
                        .await;
                    match health {
                        Ok(r) if r.status().is_success() => {
                            println!("  {GREEN}✓ Daemon healthy{RESET}");
                        }
                        _ => {
                            println!("  {YELLOW}⚠ Daemon started but health check pending{RESET}");
                            println!("  It may still be initializing. Check: ff health");
                        }
                    }
                }
                Err(e) => {
                    println!("  {RED}✗ Failed to start daemon: {e}{RESET}");
                    println!("  Binary: {}", bin.display());
                    println!("  Try: cargo run --release (from forge-fleet directory)");
                }
            }
        }
        None => {
            println!("  {RED}✗ forgefleetd binary not found{RESET}");
            println!("  Build with: cargo build --release");
            println!("  Or run: cargo run --release");
        }
    }

    // Step 3: Check fleet connectivity
    println!("{YELLOW}3/4{RESET} Checking fleet nodes...");
    let nodes = [
        ("Taylor", "192.168.5.100"),
        ("Marcus", "192.168.5.102"),
        ("Sophie", "192.168.5.103"),
        ("Priya", "192.168.5.104"),
        ("James", "192.168.5.108"),
    ];
    let mut online = 0;
    for (name, ip) in &nodes {
        let ok = SHARED_HTTP
            .get(format!("http://{ip}:51000/health"))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if ok {
            online += 1;
        }
        let icon = if ok {
            format!("{GREEN}●{RESET}")
        } else {
            format!("{RED}○{RESET}")
        };
        println!("  {icon} {name} ({ip})");
    }

    // Step 4: Summary
    println!("{YELLOW}4/4{RESET} Summary");
    println!();
    println!("  {GREEN}ForgeFleet v{}{RESET}", env!("CARGO_PKG_VERSION"));
    println!("  Fleet: {online}/{} nodes online", nodes.len());
    println!();
    println!(
        "  Daemon:    http://localhost:{}",
        ff_terminal::app::PORT_DAEMON
    );
    println!(
        "  LLM API:   http://localhost:{}",
        ff_terminal::app::PORT_LLM
    );
    println!(
        "  Web UI:    http://localhost:{}",
        ff_terminal::app::PORT_WEB
    );
    println!("  WebSocket: ws://localhost:{}", ff_terminal::app::PORT_WS);
    println!(
        "  Metrics:   http://localhost:{}",
        ff_terminal::app::PORT_METRICS
    );
    println!();
    println!(
        "  Run {CYAN}ff{RESET} for terminal, or open {CYAN}http://localhost:{}{RESET} for web UI",
        ff_terminal::app::PORT_WEB
    );

    Ok(())
}
pub fn find_daemon_binary(working_dir: &Path) -> Option<PathBuf> {
    // Check common locations
    let candidates = [
        working_dir.join("target/release/forgefleetd"),
        working_dir.join("target/debug/forgefleetd"),
        PathBuf::from("/usr/local/bin/forgefleetd"),
        dirs::home_dir()
            .unwrap_or_default()
            .join(".local/bin/forgefleetd"),
        dirs::home_dir()
            .unwrap_or_default()
            .join(".cargo/bin/forgefleetd"),
    ];

    for path in candidates.iter() {
        if path.exists() {
            return Some(path.to_path_buf());
        }
    }

    // Try which
    if let Ok(output) = std::process::Command::new("which")
        .arg("forgefleetd")
        .output()
        && output.status.success()
    {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }

    None
}
pub async fn handle_models(c: &ff_agent::agent_loop::AgentSessionConfig) -> Result<()> {
    static SHARED_HTTP: std::sync::LazyLock<reqwest::Client> =
        std::sync::LazyLock::new(|| reqwest::Client::new());
    let url = format!("{}/v1/models", c.llm_base_url.trim_end_matches('/'));
    match SHARED_HTTP.get(&url).send().await {
        Ok(r) => println!("{}", r.text().await.unwrap_or_default()),
        Err(e) => println!("{RED}Failed: {e}{RESET}"),
    }
    Ok(())
}
