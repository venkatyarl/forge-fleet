//! Applies and verifies the local node's point-to-point fabric configuration.

use std::process::Stdio;
use std::time::Duration;

use sqlx::{FromRow, PgPool};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;

const TICK_INTERVAL: Duration = Duration::from_secs(60);
const ALERT_POLICY: &str = "fabric_link_dead";

#[derive(Debug, FromRow)]
struct LocalEdge {
    id: Uuid,
    pair_name: String,
    interface_name: String,
    local_ip: String,
    target_ip: String,
    status: String,
    verified: bool,
}

async fn local_edges(pool: &PgPool, node: &str) -> Result<Vec<LocalEdge>, sqlx::Error> {
    sqlx::query_as(
        r#"SELECT id, pair_name,
                  CASE WHEN source_node = $1 THEN a_iface ELSE b_iface END AS interface_name,
                  CASE WHEN source_node = $1 THEN a_ip ELSE b_ip END AS local_ip,
                  CASE WHEN source_node = $1 THEN b_ip ELSE a_ip END AS target_ip,
                  status, verified
           FROM fabric_pairs
           WHERE source_node = $1 OR target_node = $1"#,
    )
    .bind(node)
    .fetch_all(pool)
    .await
}

fn connection_name(pair_name: &str) -> String {
    format!("ff-fabric-{pair_name}")
}

fn address_with_prefix(address: &str) -> String {
    if address.contains('/') {
        address.to_owned()
    } else {
        format!("{address}/30")
    }
}

/// Add NetworkManager connections for configured edges that are not present.
pub async fn apply_nmcli_config(pool: &PgPool, node: &str) -> anyhow::Result<()> {
    for edge in local_edges(pool, node).await? {
        if edge.interface_name.trim().is_empty() || edge.local_ip.trim().is_empty() {
            warn!(pair = %edge.pair_name, "fabric edge has no local interface or IP");
            continue;
        }

        let name = connection_name(&edge.pair_name);
        let exists = Command::new("nmcli")
            .args(["-t", "-f", "NAME", "connection", "show", &name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await?
            .success();
        if exists {
            continue;
        }

        let address = address_with_prefix(&edge.local_ip);
        let output = Command::new("nmcli")
            .args([
                "connection",
                "add",
                "type",
                "ethernet",
                "ifname",
                &edge.interface_name,
                "con-name",
                &name,
                "ipv4.method",
                "manual",
                "ipv4.addresses",
                &address,
                "ipv6.method",
                "disabled",
            ])
            .output()
            .await?;
        if !output.status.success() {
            anyhow::bail!(
                "nmcli connection add failed for {}: {}",
                edge.pair_name,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        info!(pair = %edge.pair_name, connection = %name, "fabric connection added");
    }
    Ok(())
}

/// Ping every local peer and persist verified/dead state. A transition to dead
/// fires the configured alert policy once; subsequent dead ticks are quiet.
pub async fn verify_edges(pool: &PgPool, node: &str) -> anyhow::Result<()> {
    for edge in local_edges(pool, node).await? {
        if edge.target_ip.trim().is_empty() {
            warn!(pair = %edge.pair_name, "fabric edge has no target IP");
            continue;
        }

        let alive = Command::new("ping")
            .args(["-c", "1", "-W", "2", &edge.target_ip])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .is_ok_and(|status| status.success());
        let status = if alive { "verified" } else { "dead" };

        sqlx::query(
            "UPDATE fabric_pairs SET status = $2, verified = $3, last_probed_at = NOW() WHERE id = $1",
        )
        .bind(edge.id)
        .bind(status)
        .bind(alive)
        .execute(pool)
        .await?;

        if !alive && (edge.verified || edge.status != "dead") {
            trigger_dead_alert(pool, &edge).await?;
        }
    }
    Ok(())
}

async fn trigger_dead_alert(pool: &PgPool, edge: &LocalEdge) -> anyhow::Result<()> {
    let policy: Option<(Uuid, String, String)> = sqlx::query_as(
        "SELECT id, severity, channel FROM alert_policies WHERE name = $1 AND enabled = true",
    )
    .bind(ALERT_POLICY)
    .fetch_optional(pool)
    .await?;
    let Some((policy_id, severity, channel)) = policy else {
        warn!(pair = %edge.pair_name, policy = ALERT_POLICY, "ping-dead fabric edge has no enabled alert policy");
        return Ok(());
    };

    let message = format!(
        "Fabric link {} is dead: ping to {} failed",
        edge.pair_name, edge.target_ip
    );
    let channel_result =
        crate::alert_evaluator::dispatch_alert(pool, &channel, &severity, &message).await;
    sqlx::query(
        "INSERT INTO alert_events (policy_id, value, value_text, message, channel_result) VALUES ($1, 1, $2, $3, $4)",
    )
    .bind(policy_id)
    .bind(&edge.pair_name)
    .bind(message)
    .bind(channel_result)
    .execute(pool)
    .await?;
    Ok(())
}

/// Run an immediate apply/verify tick and repeat until daemon shutdown.
pub async fn run(pool: PgPool, node: String, cancel: CancellationToken) {
    let mut ticker = tokio::time::interval(TICK_INTERVAL);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Err(error) = apply_nmcli_config(&pool, &node).await {
                    warn!(%error, "fabric configuration apply failed");
                }
                if let Err(error) = verify_edges(&pool, &node).await {
                    warn!(%error, "fabric edge verification failed");
                }
            }
            _ = cancel.cancelled() => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_stable_connection_name_and_prefix() {
        assert_eq!(
            connection_name("shakira-rihanna"),
            "ff-fabric-shakira-rihanna"
        );
        assert_eq!(address_with_prefix("10.42.0.1"), "10.42.0.1/30");
        assert_eq!(address_with_prefix("10.42.0.1/30"), "10.42.0.1/30");
    }
}
