//! Mesh key sync — leader-gated tick that propagates the canonical fleet SSH
//! key set (`fleet_workers_ssh_keys`, key_purpose='user') into every online
//! node's `~/.ssh/authorized_keys` via the deferred-task queue.
//!
//! Why: enrollment merges peer keys only on the NEW node at enroll time. The
//! reverse direction (existing nodes learning the new node's key) and any
//! partial-import casualty (vinny 2026-08-04: mesh_import never completed, so
//! the fleet could not SSH in) had no repair path. Now, whenever the key set
//! changes, every node gets a merge task within an hour.

use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// Secret key storing the last-propagated key-set fingerprint.
const LAST_KEYSET_KEY: &str = "mesh_sync_last_keyset";

/// Build the idempotent merge command for one node: append any missing user
/// keys, deduped by a stable middle slice of the base64 body.
fn merge_command(keys: &[String]) -> String {
    let mut parts = vec![
        "mkdir -p ~/.ssh && chmod 700 ~/.ssh && touch ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys".to_string(),
    ];
    for key in keys {
        let body: String = key
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .chars()
            .take(48)
            .collect();
        if body.len() < 20 {
            continue;
        }
        parts.push(format!(
            "grep -qF '{body}' ~/.ssh/authorized_keys 2>/dev/null || echo '{key}' >> ~/.ssh/authorized_keys"
        ));
    }
    parts.push("wc -l ~/.ssh/authorized_keys".to_string());
    parts.join(" &&\n")
}

/// One pass: if the canonical user-key set changed since the last pass,
/// enqueue a merge task for every online node.
async fn run_once(pg: &sqlx::PgPool) -> Result<(), String> {
    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT worker_name, public_key FROM fleet_workers_ssh_keys \
          WHERE key_purpose = 'user' AND public_key LIKE 'ssh-%' ORDER BY worker_name",
    )
    .fetch_all(pg)
    .await
    .map_err(|e| format!("key query failed: {e}"))?;

    let mut keys: Vec<String> = rows.into_iter().map(|(_, k)| k).collect();
    keys.sort();
    keys.dedup();
    if keys.is_empty() {
        return Ok(());
    }

    // Fingerprint the set — skip enqueueing when nothing changed.
    let fingerprint = format!("{:x}", {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        keys.hash(&mut h);
        h.finish()
    });
    let last = ff_db::pg_get_secret(pg, LAST_KEYSET_KEY)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    if last == fingerprint {
        return Ok(());
    }

    let nodes = sqlx::query_scalar::<_, String>(
        "SELECT name FROM computers WHERE coalesce(status,'') = 'online' ORDER BY name",
    )
    .fetch_all(pg)
    .await
    .map_err(|e| format!("nodes query failed: {e}"))?;

    let command = merge_command(&keys);
    let mut enqueued = 0u32;
    for node in &nodes {
        let payload = serde_json::json!({"command": command, "max_duration_secs": 300});
        match ff_db::pg_enqueue_deferred(
            pg,
            &format!("mesh key sync to {node}"),
            "shell",
            &payload,
            "node_online",
            &serde_json::json!({"node": node}),
            Some(node),
            &serde_json::json!([]),
            Some("mesh-sync-tick"),
            Some(2),
        )
        .await
        {
            Ok(_) => enqueued += 1,
            Err(e) => warn!(node = %node, error = %e, "mesh sync enqueue failed"),
        }
    }

    if enqueued > 0 {
        let _ = ff_db::pg_set_secret(
            pg,
            LAST_KEYSET_KEY,
            &fingerprint,
            Some("last propagated fleet user-key set fingerprint"),
            Some("mesh-sync-tick"),
        )
        .await;
        info!(
            nodes = enqueued,
            keys = keys.len(),
            "mesh sync: propagated changed key set"
        );
    }
    Ok(())
}

/// Spawn the hourly mesh-sync tick (leader-gated).
pub fn spawn_mesh_sync_tick(
    pg: sqlx::PgPool,
    interval_secs: u64,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs.max(300)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !crate::leader_cache::is_current_leader() {
                        continue;
                    }
                    if let Err(e) = run_once(&pg).await {
                        warn!(error = %e, "mesh sync tick failed");
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("mesh sync tick shutting down");
                        return;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::merge_command;

    #[test]
    fn merge_command_dedups_and_appends() {
        let keys = vec![
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJsfSib56zLK+0p/4eKsH4l16UlbSBGbShnelmJJSVFX adele@adele".to_string(),
        ];
        let cmd = merge_command(&keys);
        assert!(cmd.contains("grep -qF 'AAAAC3NzaC1lZDI1NTE5AAAAIJsfSib56zLK"));
        assert!(cmd.contains(">> ~/.ssh/authorized_keys"));
        assert!(cmd.contains("chmod 600"));
    }

    #[test]
    fn merge_command_skips_short_bodies() {
        assert!(!merge_command(&["garbage".to_string()]).contains("grep -qF"));
    }
}
