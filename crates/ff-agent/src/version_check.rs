//! Fleet version tracking + drift detection.
//! See plan: /Users/venkat/.claude/plans/gentle-questing-valley.md §3f.

use std::collections::BTreeMap;
use std::time::Duration;

use sqlx::PgPool;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct DriftSummary {
    pub total_keys: usize,
    pub drifted_keys: Vec<String>,
    pub checked_at: chrono::DateTime<chrono::Utc>,
}

/// Currently-installed versions on THIS host.
pub async fn collect_current() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();

    // OS
    if let Some(v) = read_os_pretty_name().await {
        out.insert("os".into(), v);
    }
    // Kernel
    if let Some(v) = cmd_capture("uname", &["-r"]).await {
        out.insert("kernel".into(), v);
    }
    // ff binary: embedded version plus git sha of the checked-out repo.
    if let Some(v) = cmd_capture("ff", &["--version"]).await {
        out.insert("ff".into(), v);
    }
    if let Some(sha) = git_head_sha().await {
        out.insert("ff_git".into(), sha);
    }
    for (key, bin, args) in [
        ("openclaw", "openclaw", vec!["--version"]),
        ("gh", "gh", vec!["--version"]),
        ("op", "op", vec!["--version"]),
        ("codex", "codex", vec!["--version"]),
        ("claude", "claude", vec!["--version"]),
    ] {
        if let Some(v) = cmd_capture(bin, &args).await {
            out.insert(key.into(), v);
        }
    }
    // llama-server (try both common build paths)
    if let Some(v) = probe_llama_server().await {
        out.insert("llama.cpp".into(), v);
    }
    // Python packages via pip show
    for (key, pkg) in [("mlx_lm", "mlx-lm"), ("vllm", "vllm")] {
        if let Some(v) = pip_show_version(pkg).await {
            out.insert(key.into(), v);
        }
    }
    out
}

/// Upstream "latest" for a set of keys. Concurrency cap = 4.
pub async fn fetch_latest(keys: &[&str]) -> BTreeMap<String, String> {
    use futures::stream::{FuturesUnordered, StreamExt};
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .user_agent("ForgeFleet/1.0")
        .build()
    {
        Ok(c) => c,
        Err(_) => return BTreeMap::new(),
    };

    let mut futs: FuturesUnordered<_> = FuturesUnordered::new();
    for key in keys {
        let client = client.clone();
        let key = (*key).to_string();
        futs.push(async move {
            let val = match key.as_str() {
                "gh" => gh_release(&client, "cli/cli").await,
                "op" => gh_release(&client, "1Password/cli-releases").await,
                "codex" => gh_release(&client, "microsoft/codex-cli").await,
                "claude" => gh_release(&client, "anthropics/claude-code").await,
                "openclaw" => gh_release(&client, "openclaw-ai/openclaw").await,
                "llama.cpp" => gh_release(&client, "ggerganov/llama.cpp").await,
                "mlx_lm" => pypi_version(&client, "mlx-lm").await,
                "vllm" => pypi_version(&client, "vllm").await,
                "ff_git" => git_ls_remote_main().await,
                _ => None,
            };
            (key, val)
        });
        if futs.len() >= 4 {
            if let Some((k, v)) = futs.next().await {
                if let Some(v) = v {
                    return_also(&k, &v);
                }
            }
        }
    }
    let mut out = BTreeMap::new();
    while let Some((k, v)) = futs.next().await {
        if let Some(v) = v {
            out.insert(k, v);
        }
    }
    out
}

fn return_also(_k: &str, _v: &str) { /* helper hook for tracing; noop */ }

/// Full roundtrip: merge current + latest, write to fleet_nodes.tooling.
pub async fn version_check_pass(pool: &PgPool) -> Result<DriftSummary, String> {
    let node_name = crate::fleet_info::resolve_this_node_name().await;
    let current = collect_current().await;
    let keys: Vec<&str> = current.keys().map(|s| s.as_str()).collect();
    let latest = fetch_latest(&keys).await;
    let now = chrono::Utc::now().to_rfc3339();

    // Build the tooling JSONB shape.
    let mut tooling_obj = serde_json::Map::new();
    let mut drifted = Vec::new();
    let mut total = 0;
    for (k, cur) in &current {
        total += 1;
        let lat = latest.get(k).cloned();
        if let Some(ref l) = lat {
            if l != cur {
                drifted.push(k.clone());
            }
        }
        let mut entry = serde_json::Map::new();
        entry.insert("current".into(), serde_json::Value::String(cur.clone()));
        entry.insert("latest".into(), lat.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
        entry.insert("checked_at".into(), serde_json::Value::String(now.clone()));
        tooling_obj.insert(k.clone(), serde_json::Value::Object(entry));
    }
    let tooling = serde_json::Value::Object(tooling_obj);

    // Read existing row, update tooling, upsert back.
    if let Some(mut row) = ff_db::pg_get_node(pool, &node_name)
        .await
        .map_err(|e| format!("pg_get_node: {e}"))?
    {
        row.tooling = tooling;
        ff_db::pg_upsert_node(pool, &row)
            .await
            .map_err(|e| format!("pg_upsert_node: {e}"))?;
    }

    // Best-effort Redis publish when drift is real.
    if !drifted.is_empty() {
        let _ = crate::fleet_events::publish_node_online(&format!("drift:{node_name}")).await;
    }

    Ok(DriftSummary {
        total_keys: total,
        drifted_keys: drifted,
        checked_at: chrono::Utc::now(),
    })
}

// ── helpers ──────────────────────────────────────────────────────

async fn cmd_capture(bin: &str, args: &[&str]) -> Option<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new(bin).args(args).output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

async fn read_os_pretty_name() -> Option<String> {
    // macOS
    if let Some(v) = cmd_capture("sw_vers", &["-productVersion"]).await {
        return Some(format!("macOS {v}"));
    }
    // Linux
    if let Ok(text) = tokio::fs::read_to_string("/etc/os-release").await {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("PRETTY_NAME=") {
                return Some(rest.trim_matches('"').to_string());
            }
        }
    }
    None
}

async fn git_head_sha() -> Option<String> {
    let dir = home_repo_dir();
    cmd_in_dir(&dir, "git", &["rev-parse", "--short=12", "HEAD"]).await
}

async fn git_ls_remote_main() -> Option<String> {
    let dir = home_repo_dir();
    let out = cmd_in_dir(&dir, "git", &["ls-remote", "origin", "refs/heads/main"]).await?;
    out.split_whitespace().next().map(|s| s.chars().take(12).collect())
}

fn home_repo_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    std::path::PathBuf::from(home).join("taylorProjects/forge-fleet")
}

async fn cmd_in_dir(dir: &std::path::Path, bin: &str, args: &[&str]) -> Option<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new(bin).args(args).current_dir(dir).output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

async fn probe_llama_server() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    for path in &[
        format!("{home}/llama.cpp/build/bin/llama-server"),
        format!("{home}/llama.cpp/build-new/bin/llama-server"),
        "/usr/local/bin/llama-server".into(),
        "llama-server".into(),
    ] {
        if let Some(v) = cmd_capture(path, &["--version"]).await {
            return Some(v);
        }
    }
    None
}

async fn pip_show_version(pkg: &str) -> Option<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new("pip").args(["show", pkg]).output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(v) = line.strip_prefix("Version:") {
            return Some(v.trim().to_string());
        }
    }
    None
}

async fn gh_release(client: &reqwest::Client, repo: &str) -> Option<String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    json.get("tag_name").and_then(|v| v.as_str()).map(str::to_string)
}

async fn pypi_version(client: &reqwest::Client, pkg: &str) -> Option<String> {
    let url = format!("https://pypi.org/pypi/{pkg}/json");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    json.get("info")
        .and_then(|i| i.get("version"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}
