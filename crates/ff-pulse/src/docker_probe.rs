//! DockerProbe — queries the local Docker daemon via the `docker` CLI and
//! produces a fully-populated `DockerStatus`.
//!
//! Using the CLI (rather than the raw HTTP API over the unix socket) keeps
//! this dependency-free and survives the variations between Docker Desktop,
//! colima, rootless docker, and Linux system docker.

use std::collections::BTreeMap;
use std::process::Command;

use serde::Deserialize;
use tracing::debug;

use crate::beat_v2::{DockerContainer, DockerProject, DockerStatus};

pub struct DockerProbe;

impl DockerProbe {
    /// Query the local Docker daemon for container status, grouped by compose
    /// project. Returns `DockerStatus { daemon_running: false, .. }` if the
    /// CLI is missing or the daemon is unreachable.
    pub async fn detect() -> DockerStatus {
        // Spawn blocking work off the async runtime so we don't stall it.
        tokio::task::spawn_blocking(probe_sync)
            .await
            .unwrap_or_else(|e| {
                debug!("docker_probe: join error: {e}");
                empty_status()
            })
    }
}

fn probe_sync() -> DockerStatus {
    // Quick check — if the binary isn't on PATH we're done.
    let ps_out = match Command::new("docker")
        .args(["ps", "-a", "--format", "{{json .}}"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return empty_status(),
    };
    if !ps_out.status.success() {
        // Non-zero typically means daemon not running ("Cannot connect to ...").
        return empty_status();
    }

    let ps_text = String::from_utf8_lossy(&ps_out.stdout);
    let mut containers: Vec<RawContainer> = Vec::new();
    for line in ps_text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<RawContainer>(line) {
            Ok(c) => containers.push(c),
            Err(e) => {
                debug!("docker_probe: skipping unparseable line: {e}");
            }
        }
    }

    // Stats pass (running containers only).
    let stats_out = Command::new("docker")
        .args([
            "stats",
            "--no-stream",
            "--format",
            "{{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}",
        ])
        .output();

    let mut stats_by_name: BTreeMap<String, (f64, f64, f64)> = BTreeMap::new();
    let mut total_cpu_pct = 0.0f64;
    let mut total_memory_mb = 0.0f64;
    if let Ok(out) = stats_out {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                let parts: Vec<&str> = line.split('\t').collect();
                if parts.len() < 3 {
                    continue;
                }
                let name = parts[0].trim().to_string();
                let cpu_pct = parse_percent(parts[1]);
                let (mem_mb, limit_mb) = parse_mem_usage(parts[2]);
                total_cpu_pct += cpu_pct;
                total_memory_mb += mem_mb;
                stats_by_name.insert(name, (cpu_pct, mem_mb, limit_mb));
            }
        }
    }

    // Group by compose project.
    let mut projects: BTreeMap<String, Vec<DockerContainer>> = BTreeMap::new();
    let mut compose_files: BTreeMap<String, String> = BTreeMap::new();

    for c in containers {
        let labels = parse_labels(&c.labels);
        let project = labels
            .get("com.docker.compose.project")
            .cloned()
            .unwrap_or_else(|| "unmanaged".to_string());
        if let Some(cf) = labels.get("com.docker.compose.project.config_files") {
            compose_files
                .entry(project.clone())
                .or_insert_with(|| cf.clone());
        }

        let name = c
            .names
            .split(',')
            .next()
            .unwrap_or(&c.names)
            .trim()
            .to_string();
        let container_id = c.id.chars().take(12).collect::<String>();
        let ports = parse_ports(&c.ports);
        let status_enum = map_state(&c.state);
        let health = extract_health(&c.status);
        let uptime_sec = parse_uptime(&c.status);

        let (cpu_pct, mem_mb, mem_limit_mb) =
            stats_by_name.get(&name).copied().unwrap_or((0.0, 0.0, 0.0));

        projects.entry(project).or_default().push(DockerContainer {
            name,
            container_id,
            image: c.image,
            ports,
            status: status_enum.to_string(),
            health,
            cpu_pct,
            memory_mb: mem_mb,
            memory_limit_mb: mem_limit_mb,
            uptime_sec,
        });
    }

    let project_rows: Vec<DockerProject> = projects
        .into_iter()
        .map(|(name, containers)| {
            let overall = project_status(&containers);
            DockerProject {
                compose_file: compose_files.get(&name).cloned(),
                name,
                status: overall,
                containers,
            }
        })
        .collect();

    DockerStatus {
        daemon_running: true,
        total_cpu_pct,
        total_memory_mb,
        memory_limit_mb: 0.0,
        projects: project_rows,
    }
}

fn empty_status() -> DockerStatus {
    DockerStatus {
        daemon_running: false,
        total_cpu_pct: 0.0,
        total_memory_mb: 0.0,
        memory_limit_mb: 0.0,
        projects: Vec::new(),
    }
}

#[derive(Debug, Deserialize)]
struct RawContainer {
    #[serde(alias = "ID")]
    id: String,
    #[serde(alias = "Names")]
    names: String,
    #[serde(alias = "Image")]
    image: String,
    #[serde(alias = "State")]
    state: String,
    #[serde(alias = "Status")]
    status: String,
    #[serde(alias = "Ports", default)]
    ports: String,
    #[serde(alias = "Labels", default)]
    labels: String,
}

fn parse_labels(s: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for entry in s.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if let Some((k, v)) = entry.split_once('=') {
            out.insert(k.to_string(), v.to_string());
        }
    }
    out
}

fn parse_ports(s: &str) -> Vec<String> {
    s.split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

fn map_state(state: &str) -> &'static str {
    match state.to_ascii_lowercase().as_str() {
        "running" => "running",
        "paused" => "paused",
        "restarting" => "restarting",
        "exited" => "exited",
        "dead" => "exited",
        "created" => "stopped",
        _ => "stopped",
    }
}

fn extract_health(status: &str) -> Option<String> {
    let lower = status.to_ascii_lowercase();
    if lower.contains("(healthy)") {
        Some("healthy".into())
    } else if lower.contains("(unhealthy)") {
        Some("unhealthy".into())
    } else if lower.contains("(health: starting)") || lower.contains("(starting)") {
        Some("starting".into())
    } else {
        None
    }
}

/// Parse `Status` strings like:
///   "Up 5 seconds", "Up 2 minutes", "Up 3 hours (healthy)", "Up About an hour",
///   "Exited (0) 2 hours ago" — exited → 0.
fn parse_uptime(status: &str) -> u64 {
    let lower = status.to_ascii_lowercase();
    if !lower.starts_with("up ") {
        return 0;
    }
    let tail = &lower[3..];
    // "about an hour" style
    if tail.starts_with("about an hour") || tail.starts_with("an hour") {
        return 3600;
    }
    if tail.starts_with("less than a second") {
        return 0;
    }

    let mut parts = tail.split_whitespace();
    let num_raw = match parts.next() {
        Some(n) => n,
        None => return 0,
    };
    let num: u64 = num_raw.parse().unwrap_or(0);
    let unit = parts.next().unwrap_or("");
    match unit {
        u if u.starts_with("second") => num,
        u if u.starts_with("minute") => num * 60,
        u if u.starts_with("hour") => num * 3600,
        u if u.starts_with("day") => num * 86_400,
        u if u.starts_with("week") => num * 604_800,
        u if u.starts_with("month") => num * 2_592_000,
        _ => 0,
    }
}

fn parse_percent(s: &str) -> f64 {
    s.trim().trim_end_matches('%').parse().unwrap_or(0.0)
}

/// Parse Docker's memory-usage format like "150.3MiB / 2GiB".
/// Returns (used_mb, limit_mb).
fn parse_mem_usage(s: &str) -> (f64, f64) {
    let mut iter = s.split('/');
    let used = iter.next().unwrap_or("").trim();
    let limit = iter.next().unwrap_or("").trim();
    (parse_bytes_to_mb(used), parse_bytes_to_mb(limit))
}

fn parse_bytes_to_mb(s: &str) -> f64 {
    // Strip trailing unit.
    let s = s.trim();
    if s.is_empty() {
        return 0.0;
    }
    // Find where the numeric portion ends.
    let split_at = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split_at);
    let n: f64 = num.parse().unwrap_or(0.0);
    match unit.trim().to_ascii_lowercase().as_str() {
        "b" | "" => n / 1_048_576.0,
        "kib" | "kb" | "k" => n / 1024.0,
        "mib" | "mb" | "m" => n,
        "gib" | "gb" | "g" => n * 1024.0,
        "tib" | "tb" | "t" => n * 1_048_576.0,
        _ => n,
    }
}

fn project_status(containers: &[DockerContainer]) -> String {
    if containers.is_empty() {
        return "empty".into();
    }
    let running = containers.iter().filter(|c| c.status == "running").count();
    if running == containers.len() {
        "running".into()
    } else if running == 0 {
        "stopped".into()
    } else {
        "degraded".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_parses_with_suffix() {
        assert_eq!(parse_percent("12.5%"), 12.5);
        assert_eq!(parse_percent("0.00%"), 0.0);
    }

    #[test]
    fn mem_usage_parses_mixed_units() {
        let (u, l) = parse_mem_usage("150.3MiB / 2GiB");
        assert!((u - 150.3).abs() < 0.001);
        assert!((l - 2048.0).abs() < 0.001);

        let (u, l) = parse_mem_usage("1.5GiB / 8GiB");
        assert!((u - 1536.0).abs() < 0.001);
        assert!((l - 8192.0).abs() < 0.001);
    }

    #[test]
    fn uptime_parses_common_forms() {
        assert_eq!(parse_uptime("Up 5 seconds"), 5);
        assert_eq!(parse_uptime("Up 3 minutes"), 180);
        assert_eq!(parse_uptime("Up 2 hours (healthy)"), 7200);
        assert_eq!(parse_uptime("Up About an hour"), 3600);
        assert_eq!(parse_uptime("Exited (0) 2 hours ago"), 0);
    }

    #[test]
    fn health_extracts_correctly() {
        assert_eq!(
            extract_health("Up 2 hours (healthy)"),
            Some("healthy".into())
        );
        assert_eq!(
            extract_health("Up 2 hours (unhealthy)"),
            Some("unhealthy".into())
        );
        assert_eq!(extract_health("Up 2 hours"), None);
    }

    #[test]
    fn map_state_covers_enum() {
        assert_eq!(map_state("running"), "running");
        assert_eq!(map_state("Exited"), "exited");
        assert_eq!(map_state("paused"), "paused");
        assert_eq!(map_state("restarting"), "restarting");
        assert_eq!(map_state("dead"), "exited");
        assert_eq!(map_state("unknown_state"), "stopped");
    }

    #[test]
    fn labels_parse_comma_separated() {
        let l = parse_labels("com.docker.compose.project=foo,foo.bar=baz");
        assert_eq!(
            l.get("com.docker.compose.project"),
            Some(&"foo".to_string())
        );
        assert_eq!(l.get("foo.bar"), Some(&"baz".to_string()));
    }

    #[test]
    fn empty_status_when_no_daemon() {
        let s = empty_status();
        assert!(!s.daemon_running);
        assert!(s.projects.is_empty());
    }

    #[tokio::test]
    async fn detect_does_not_panic() {
        // Works whether or not docker is installed — returns empty status in the
        // latter case. We only assert it completes.
        let _ = DockerProbe::detect().await;
    }
}
