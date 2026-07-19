//! Fleet peer NFS mount inventory.
//!
//! Every node scans its own `/proc/mounts` (or `mount` on macOS), records NFS
//! mounts whose source resolves to another fleet computer in `fleet_peer_mounts`,
//! and marks each as `mounted` or `stale` based on a short `stat` probe. The
//! mesh check and `ff doctor` then correlate unreachable peers with stale mounts
//! and D-state pile-ups.

use std::collections::HashSet;
use std::time::Duration;

use sqlx::PgPool;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{info, warn};

/// One raw mount entry from the OS.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawMount {
    source: String,
    mount_point: String,
    fs_type: String,
    options: String,
}

/// Scan this node's mounts and persist/update `fleet_peer_mounts` rows.
/// Returns the number of NFS peer mounts observed.
pub async fn scan_local_peer_mounts(pool: &PgPool) -> Result<usize, String> {
    let worker_name = crate::fleet_info::resolve_this_worker_name().await;
    let computer_id = computer_id_by_name(pool, &worker_name)
        .await
        .map_err(|e| format!("lookup computer_id: {e}"))?
        .ok_or_else(|| format!("computer '{worker_name}' not in computers"))?;

    let mounts = read_local_mounts()
        .await
        .map_err(|e| format!("read mounts: {e}"))?;

    let scan_start = chrono::Utc::now();
    let mut seen_mount_points = HashSet::new();
    let mut observed = 0usize;

    for m in mounts {
        if !is_nfs(&m.fs_type) {
            continue;
        }
        let Some(source_host) = source_host(&m.source) else {
            continue;
        };

        let peer = resolve_peer_by_host(pool, &source_host)
            .await
            .ok()
            .flatten();
        let status = check_mount_liveness(&m.mount_point).await;
        let last_error = if status == "stale" {
            Some("mount point did not respond to stat probe")
        } else {
            None
        };

        ff_db::pg_upsert_fleet_peer_mount(
            pool,
            computer_id,
            peer.map(|(id, _)| id),
            &source_host,
            &m.mount_point,
            &m.fs_type,
            &m.options,
            &status,
            last_error,
        )
        .await
        .map_err(|e| format!("upsert fleet_peer_mounts: {e}"))?;

        seen_mount_points.insert(m.mount_point);
        observed += 1;
    }

    // Drop rows for this node that disappeared since the scan started.
    match ff_db::pg_prune_fleet_peer_mounts(pool, computer_id, scan_start).await {
        Ok(pruned) if pruned > 0 => {
            info!(worker = %worker_name, pruned, "pruned vanished peer mounts");
        }
        Ok(_) => {}
        Err(e) => warn!(worker = %worker_name, error = %e, "failed to prune peer mounts"),
    }

    info!(worker = %worker_name, observed, "peer mount scan complete");
    Ok(observed)
}

/// Spawn a periodic local peer-mount scan. Not leader-gated: every node reports
/// its own mounts.
pub fn spawn_peer_mount_scan_tick(
    pg: PgPool,
    worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    match scan_local_peer_mounts(&pg).await {
                        Ok(n) => tracing::debug!(worker = %worker_name, observed = n, "peer-mount scan"),
                        Err(e) => tracing::warn!(worker = %worker_name, error = %e, "peer-mount scan failed"),
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        tracing::info!(worker = %worker_name, "peer-mount scan loop stopped");
    })
}

async fn computer_id_by_name(
    pool: &PgPool,
    name: &str,
) -> Result<Option<sqlx::types::Uuid>, sqlx::Error> {
    let row: Option<(sqlx::types::Uuid,)> =
        sqlx::query_as("SELECT id FROM computers WHERE name = $1")
            .bind(name)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|r| r.0))
}

async fn resolve_peer_by_host(
    pool: &PgPool,
    host: &str,
) -> Result<Option<(sqlx::types::Uuid, String)>, sqlx::Error> {
    let row: Option<(sqlx::types::Uuid, String)> =
        sqlx::query_as("SELECT id, name FROM computers WHERE name = $1 OR primary_ip = $1")
            .bind(host)
            .fetch_optional(pool)
            .await?;
    Ok(row)
}

fn is_nfs(fs_type: &str) -> bool {
    matches!(fs_type.to_lowercase().as_str(), "nfs" | "nfs3" | "nfs4")
}

fn source_host(source: &str) -> Option<String> {
    // NFS source looks like "james:/mnt/james" or "10.0.0.5:/export".
    source.split_once(':').map(|(h, _)| h.to_string())
}

/// True if the mount point answers a stat quickly, false otherwise.
async fn check_mount_liveness(mount_point: &str) -> String {
    let fut = Command::new("stat").arg(mount_point).output();
    match timeout(Duration::from_secs(5), fut).await {
        Ok(Ok(out)) if out.status.success() => "mounted".into(),
        Ok(Ok(out)) => {
            let err = String::from_utf8_lossy(&out.stderr)
                .trim()
                .chars()
                .take(80)
                .collect::<String>();
            warn!(mount_point, error = %err, "stat probe failed");
            "stale".into()
        }
        Ok(Err(e)) => {
            warn!(mount_point, error = %e, "stat probe spawn failed");
            "stale".into()
        }
        Err(_) => {
            warn!(mount_point, "stat probe timed out (likely stale NFS)");
            "stale".into()
        }
    }
}

async fn read_local_mounts() -> Result<Vec<RawMount>, String> {
    // Linux exposes the canonical table; macOS does not.
    if let Ok(text) = tokio::fs::read_to_string("/proc/mounts").await {
        return Ok(parse_proc_mounts(&text));
    }

    let out = Command::new("mount")
        .output()
        .await
        .map_err(|e| format!("mount: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "mount failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(parse_mount_output(&text))
}

fn parse_proc_mounts(text: &str) -> Vec<RawMount> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let source = parts.next()?.to_string();
            let mount_point_escaped = parts.next()?.to_string();
            let fs_type = parts.next()?.to_string();
            let options = parts.next()?.to_string();
            let mount_point = unescape_mount_point(&mount_point_escaped);
            Some(RawMount {
                source,
                mount_point,
                fs_type,
                options,
            })
        })
        .collect()
}

fn unescape_mount_point(s: &str) -> String {
    // /proc/mounts encodes spaces as \040.
    s.replace("\\040", " ")
}

fn parse_mount_output(text: &str) -> Vec<RawMount> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let (source, rest) = line.split_once(" on ")?;
            let source = source.trim().to_string();

            // Linux: "<mp> type nfs4 (opts...)"
            // macOS:  "<mp> (nfs4, opts...)"
            let (mount_point, fs_type, options) =
                if let Some((mp, type_rest)) = rest.split_once(" type ") {
                    let mp = mp.trim().to_string();
                    let inner = type_rest.split_once('(')?.1.strip_suffix(')')?;
                    let mut opts = inner.split(',');
                    let fs_type = opts.next()?.trim().to_string();
                    let options = opts.collect::<Vec<_>>().join(",").trim().to_string();
                    (mp, fs_type, options)
                } else {
                    let (mp, inner) = rest.split_once(" (")?;
                    let mp = mp.trim().to_string();
                    let inner = inner.strip_suffix(')')?;
                    let mut opts = inner.split(',');
                    let fs_type = opts.next()?.trim().to_string();
                    let options = opts.collect::<Vec<_>>().join(",").trim().to_string();
                    (mp, fs_type, options)
                };

            Some(RawMount {
                source,
                mount_point,
                fs_type,
                options,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_mounts_with_nfs() {
        let text = concat!(
            "sysfs /sys sysfs rw,nosuid,nodev,noexec,relatime 0 0\n",
            "james:/mnt/james /mnt/james nfs4 rw,relatime,vers=4.0,soft,timeo=100,retrans=3,proto=tcp 0 0\n",
            "sia:/home/sia/models /home/adele/models nfs rw,hard,timeo=600 0 0\n"
        );
        let mounts = parse_proc_mounts(text);
        assert_eq!(mounts.len(), 3);
        let nfs = mounts
            .iter()
            .find(|m| m.mount_point == "/mnt/james")
            .unwrap();
        assert_eq!(nfs.source, "james:/mnt/james");
        assert_eq!(nfs.fs_type, "nfs4");
        assert!(nfs.options.contains("soft"));
    }

    #[test]
    fn parses_linux_mount_output() {
        let text = "james:/ on /mnt/james type nfs4 (rw,relatime,vers=4.0,hard,timeo=600,retrans=2,proto=tcp)";
        let mounts = parse_mount_output(text);
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].source, "james:/");
        assert_eq!(mounts[0].mount_point, "/mnt/james");
        assert_eq!(mounts[0].fs_type, "nfs4");
        assert!(mounts[0].options.contains("hard"));
    }

    #[test]
    fn parses_macos_mount_output() {
        let text = "james:/ on /Volumes/james (nfs4, nodev, nosuid, mounted by venkat)";
        let mounts = parse_mount_output(text);
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].source, "james:/");
        assert_eq!(mounts[0].mount_point, "/Volumes/james");
        assert_eq!(mounts[0].fs_type, "nfs4");
    }

    #[test]
    fn source_host_extracts_server() {
        assert_eq!(source_host("james:/mnt/james"), Some("james".into()));
        assert_eq!(source_host("10.0.0.5:/export"), Some("10.0.0.5".into()));
        assert_eq!(source_host("/dev/sda1"), None);
    }

    #[test]
    fn nfs_types_are_recognised() {
        assert!(is_nfs("nfs"));
        assert!(is_nfs("nfs4"));
        assert!(is_nfs("NFS3"));
        assert!(!is_nfs("ext4"));
    }
}
