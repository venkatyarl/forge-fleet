//! Ray cluster membership detection for ForgeFleet's pulse beat.

use crate::beat_v2::{MultiHostParticipation, RayClusterMembership};
use std::env;
use tokio::process::Command;

/// Detects if the current host is part of a Ray cluster.
///
/// Behavior:
/// 1. Checks for `raylet` process to determine if host is in a cluster.
/// 2. Checks for `gcs_server` process to determine if host is the head node.
/// 3. Reads `RAY_ADDRESS` environment variable for the head endpoint.
/// 4. Attempts to get `cluster_id` via `ray status --format=json`, falling back to a local hostname format.
pub async fn detect_ray_membership() -> Option<MultiHostParticipation> {
    // 1. Check if raylet is running
    if !check_process("raylet").await {
        return None;
    }

    // 2. Determine role (head vs worker)
    let is_head = check_process("gcs_server").await;
    let role = if is_head {
        "head".to_string()
    } else {
        "worker".to_string()
    };

    // 3. Get head endpoint from RAY_ADDRESS (empty when not set — workers
    //    leave this blank; the leader-side materializer figures out the
    //    cluster head from whichever beat reports `role=head`).
    let head_endpoint = env::var("RAY_ADDRESS").unwrap_or_default();

    // 4. Determine cluster_id
    let cluster_id = get_cluster_id().await;

    Some(MultiHostParticipation {
        ray_clusters: vec![RayClusterMembership {
            cluster_id,
            role,
            head_endpoint,
        }],
        shared_mounts: vec![],
    })
}

async fn check_process(name: &str) -> bool {
    let output = Command::new("pgrep")
        .arg("-x")
        .arg(name)
        .kill_on_drop(true)
        .output()
        .await;

    match output {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

async fn get_cluster_id() -> String {
    let output = Command::new("ray")
        .arg("status")
        .arg("--format=json")
        .kill_on_drop(true)
        .output()
        .await;

    if let Ok(out) = output {
        if let Ok(stdout) = String::from_utf8(out.stdout) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stdout) {
                if let Some(id) = json.get("cluster_id").and_then(|v| v.as_str()) {
                    return id.to_string();
                }
            }
        }
    }

    // Fallback: local-ray-{hostname}. Run `hostname` directly since the
    // HOSTNAME env var isn't always inherited into subprocess environments.
    let hostname = match Command::new("hostname").kill_on_drop(true).output().await {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string()),
    };

    format!("local-ray-{hostname}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_detect_ray_membership_no_panic() {
        // This test is expected to return None on most dev machines,
        // but it must not panic.
        let _ = detect_ray_membership().await;
    }
}
