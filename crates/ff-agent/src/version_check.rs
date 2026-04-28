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
    // Kernel / OS build — uname on Unix, Win32 OS version on Windows.
    #[cfg(not(windows))]
    if let Some(v) = cmd_capture("uname", &["-r"]).await {
        out.insert("kernel".into(), v);
    }
    #[cfg(windows)]
    if let Some(v) = cmd_capture("cmd", &["/C", "ver"]).await {
        out.insert("kernel".into(), v);
    }
    // ff binary: embedded version plus git sha of the checked-out repo.
    if let Some(v) = cmd_capture("ff", &["--version"]).await {
        out.insert("ff".into(), v);
    }
    if let Some(sha) = git_head_sha().await {
        // Both ff and forgefleetd are built from the same repo, so they
        // share the HEAD sha. Reporting both keys keeps the registry's
        // installed_version accurate for both software_registry rows.
        out.insert("ff_git".into(), sha.clone());
        out.insert("forgefleetd_git".into(), sha);
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
                "ff_git" | "forgefleetd_git" => git_ls_remote_main().await,
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

fn return_also(_k: &str, _v: &str) { /* helper hook for tracing; noop */
}

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
            if !versions_equivalent(cur, l) {
                drifted.push(k.clone());
            }
        }
        let mut entry = serde_json::Map::new();
        entry.insert("current".into(), serde_json::Value::String(cur.clone()));
        entry.insert(
            "latest".into(),
            lat.map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null),
        );
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
        // Enqueue one operator-triggered upgrade task per (node, tool) pair,
        // de-duplicated against any already-pending task for the same pair.
        let _ = enqueue_upgrade_tasks(pool, &node_name, &drifted, &current, &latest).await;
    }

    Ok(DriftSummary {
        total_keys: total,
        drifted_keys: drifted,
        checked_at: chrono::Utc::now(),
    })
}

/// For each drifted tool, enqueue a manual-trigger `upgrade` task targeting
/// this node — unless one is already pending. Returns count of new tasks.
async fn enqueue_upgrade_tasks(
    pool: &PgPool,
    node: &str,
    drifted: &[String],
    current: &BTreeMap<String, String>,
    latest: &BTreeMap<String, String>,
) -> Result<usize, String> {
    let existing = ff_db::pg_list_deferred(pool, Some("pending"), 500)
        .await
        .map_err(|e| format!("pg_list_deferred: {e}"))?;
    let already = |tool: &str| -> bool {
        existing.iter().any(|t| {
            t.kind == "upgrade"
                && t.preferred_node.as_deref() == Some(node)
                && t.payload.get("tool").and_then(|v| v.as_str()) == Some(tool)
        })
    };
    let mut created = 0;
    for tool in drifted {
        if already(tool) {
            continue;
        }
        let cur = current.get(tool).cloned().unwrap_or_default();
        let lat = latest.get(tool).cloned().unwrap_or_default();
        let title = format!("Upgrade {tool} on {node} ({cur} → {lat})");
        let payload = serde_json::json!({
            "tool":    tool,
            "node":    node,
            "current": cur,
            "latest":  lat,
        });
        let trigger_spec = serde_json::json!({});
        let required_caps = serde_json::json!([]);
        if ff_db::pg_enqueue_deferred(
            pool,
            &title,
            "upgrade",
            &payload,
            "operator",
            &trigger_spec,
            Some(node),
            &required_caps,
            Some("version_check"),
            Some(3),
        )
        .await
        .is_ok()
        {
            created += 1;
        }
    }
    Ok(created)
}

/// Best-effort version equivalence. Tool `--version` output and upstream
/// release tags are almost never byte-identical: "gh version 2.89.0 (date)"
/// vs "v2.89.0", "2.32.1" vs "v2.32.1", "OpenClaw 2.4.0" vs "2.4.0", etc.
///
/// For `ff` / `ff_git` / `forgefleetd_git` rows the inputs follow the
/// `<date>_<n> (pushed <sha>)` shape — those should compare on SHA only,
/// not on the per-machine build counter. `BuildVersion::is_same_code`
/// handles that path; for anything else we fall back to the dotted-numeric
/// extraction.
fn versions_equivalent(a: &str, b: &str) -> bool {
    use ff_core::build_version::BuildVersion;
    if let (Some(va), Some(vb)) = (BuildVersion::parse(a), BuildVersion::parse(b)) {
        return va.is_same_code(&vb);
    }
    match (extract_semver(a), extract_semver(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

fn extract_semver(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            let slice = &s[start..i];
            if slice.contains('.') {
                return Some(slice.trim_end_matches('.').to_string());
            }
        } else {
            i += 1;
        }
    }
    None
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
    // Windows: `ver` prints e.g. "Microsoft Windows [Version 10.0.19045.5011]".
    #[cfg(windows)]
    if let Some(v) = cmd_capture("cmd", &["/C", "ver"]).await {
        return Some(v);
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
    out.split_whitespace()
        .next()
        .map(|s| s.chars().take(12).collect())
}

fn home_repo_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| {
            if cfg!(windows) {
                "C:\\".into()
            } else {
                "/".into()
            }
        });
    std::path::PathBuf::from(home)
        .join("projects")
        .join("forge-fleet")
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
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
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
    json.get("tag_name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_equivalent_same_sha_different_build_counter() {
        // Two ff binaries built on different hosts at the same commit
        // legitimately disagree on the build counter. They must compare
        // equal — that's the whole point of the SHA-first display fix.
        let a = "2026.4.27_12 (pushed db1a950e4c)";
        let b = "2026.4.27_64 (pushed db1a950e4c)";
        assert!(versions_equivalent(a, b));
    }

    #[test]
    fn versions_equivalent_different_sha_is_drift() {
        let a = "2026.4.27_12 (pushed db1a950e4c)";
        let b = "2026.4.27_12 (pushed 33e05f9beb)";
        assert!(!versions_equivalent(a, b));
    }

    #[test]
    fn versions_equivalent_falls_back_to_semver_for_vendor_strings() {
        // Pre-existing semver path still works for `gh`, `op`, etc.
        assert!(versions_equivalent("gh version 2.89.0 (date)", "v2.89.0"));
        assert!(!versions_equivalent(
            "gh version 2.89.0",
            "gh version 2.90.0"
        ));
    }
}
