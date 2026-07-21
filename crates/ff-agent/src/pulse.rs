use ff_core::{ActivityLevel, WorkerRole, health::InstallDiff};
use ff_discovery::{HardwareProfile, HealthSnapshot, collect_health_snapshot};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use tracing::{info, warn};

use crate::leader::LeaderClient;

/// Agent heartbeat payload. `install_diff` is populated only on the first pulse.
#[derive(Debug, Serialize)]
pub struct PulsePayload<'a> {
    pub node_id: &'a str,
    pub role: WorkerRole,
    pub activity_level: ActivityLevel,
    pub health: &'a HealthSnapshot,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub install_diff: Vec<InstallDiff>,
}

/// Verify required software immediately after registration and report the result.
pub async fn on_first_pulse(
    leader: &LeaderClient,
    node_id: &str,
    hardware: &HardwareProfile,
    role: WorkerRole,
) {
    let os = hardware.os.clone();
    let install_diff =
        match tokio::task::spawn_blocking(move || ff_core::health::verify_required_installs(&os))
            .await
        {
            Ok(Ok(diff)) => missing_only(diff),
            Ok(Err(err)) => {
                warn!(error = %err, "required-install verification failed");
                Vec::new()
            }
            Err(err) => {
                warn!(error = %err, "required-install verification task failed");
                Vec::new()
            }
        };

    let health = collect_health_snapshot(0, Vec::new());
    let payload = PulsePayload {
        node_id,
        role,
        activity_level: ActivityLevel::Idle,
        health: &health,
        install_diff: install_diff.clone(),
    };
    if let Err(err) = leader.send_pulse(&payload).await {
        warn!(error = %err, "first pulse failed");
    }

    if !install_diff.is_empty() {
        let node = node_id.to_owned();
        tokio::spawn(async move {
            if let Err(err) = enqueue_install_sync(&node, &install_diff).await {
                warn!(error = %err, "required-install background sync failed");
            }
        });
    }
}

fn missing_only(diff: Vec<InstallDiff>) -> Vec<InstallDiff> {
    diff.into_iter().filter(|item| !item.installed).collect()
}

async fn enqueue_install_sync(node: &str, missing: &[InstallDiff]) -> anyhow::Result<()> {
    let database_url = std::env::var("FORGEFLEET_POSTGRES_URL")
        .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))?;
    let pool = PgPool::connect(&database_url).await?;

    for item in missing {
        let playbooks: Option<Value> =
            sqlx::query_scalar("SELECT upgrade_playbook FROM software_registry WHERE id = $1")
                .bind(&item.software_id)
                .fetch_optional(&pool)
                .await?;
        let Some(command) = playbooks
            .as_ref()
            .and_then(|value| value.get(&item.playbook_key))
            .and_then(Value::as_str)
            .filter(|command| !command.trim().is_empty())
        else {
            warn!(software_id = %item.software_id, "required-install playbook is unavailable");
            continue;
        };

        let payload = json!({
            "command": command,
            "meta": { "install_sync": { "software_id": item.software_id } }
        });
        ff_db::pg_enqueue_deferred(
            &pool,
            &format!("Install {} on {node}", item.display_name),
            "shell",
            &payload,
            "node_online",
            &json!({ "node": node }),
            Some(node),
            &json!([]),
            Some("agent-first-pulse"),
            Some(3),
        )
        .await?;
        info!(software_id = %item.software_id, %node, "queued required-install sync");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(installed: bool) -> InstallDiff {
        InstallDiff {
            software_id: "tool".into(),
            display_name: "Tool".into(),
            installed,
            installed_version: installed.then(|| "1.0".into()),
            playbook_key: "linux".into(),
        }
    }

    #[test]
    fn missing_only_excludes_installed_packages() {
        assert_eq!(
            missing_only(vec![diff(true), diff(false)]),
            vec![diff(false)]
        );
    }
}
