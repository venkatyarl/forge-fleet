//! Node-local health checks backed by the fleet software registry.

use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Connection, Row, postgres::PgConnection};

use crate::{ForgeFleetError, Result};

/// Installed-state difference for one registry item required by an OS playbook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallDiff {
    pub software_id: String,
    pub display_name: String,
    pub installed: bool,
    pub installed_version: Option<String>,
    pub playbook_key: String,
}

#[derive(Debug)]
struct RequiredInstall {
    software_id: String,
    display_name: String,
    detection: Value,
    playbook_key: String,
}

/// Compare packages required by `os` with the software installed on this node.
///
/// A package is required when its `software_registry` row applies to `os` and
/// its `upgrade_playbook` defines an exact OS, broad OS-family, or `all`
/// playbook. Detection uses the registry's configured binary and arguments, so
/// the check stays aligned with the pulse software collector.
pub fn verify_required_installs(os: &str) -> Result<Vec<InstallDiff>> {
    let os = os.trim();
    if os.is_empty() {
        return Err(ForgeFleetError::Config(
            "required-install health check needs a non-empty OS".into(),
        ));
    }

    let database_url = std::env::var("FORGEFLEET_POSTGRES_URL")
        .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
        .map_err(|_| {
            ForgeFleetError::Config("set FORGEFLEET_POSTGRES_URL or FORGEFLEET_DATABASE_URL".into())
        })?;
    let os = os.to_owned();

    // This API is deliberately synchronous because it runs once during the
    // first-pulse path. Keep sqlx's runtime isolated so callers may invoke it
    // safely from either synchronous code or an existing Tokio runtime.
    let required = std::thread::spawn(move || -> Result<Vec<RequiredInstall>> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(ForgeFleetError::Io)?;
        runtime.block_on(load_required_installs(&database_url, &os))
    })
    .join()
    .map_err(|_| ForgeFleetError::Internal("required-install query thread panicked".into()))??;

    Ok(required
        .into_iter()
        .map(|item| {
            let installed_version = run_health_probe(&item.detection);
            InstallDiff {
                software_id: item.software_id,
                display_name: item.display_name,
                installed: installed_version.is_some(),
                installed_version,
                playbook_key: item.playbook_key,
            }
        })
        .collect())
}

async fn load_required_installs(database_url: &str, os: &str) -> Result<Vec<RequiredInstall>> {
    let mut connection = PgConnection::connect(database_url).await?;
    let rows = sqlx::query(
        "SELECT id, display_name, applies_to_os_family, detection, upgrade_playbook \
         FROM software_registry ORDER BY id",
    )
    .fetch_all(&mut connection)
    .await?;

    let mut required = Vec::new();
    for row in rows {
        let applies_to: Option<String> = row.try_get("applies_to_os_family")?;
        if !applies_to_os(applies_to.as_deref(), os) {
            continue;
        }

        let playbooks: Value = row.try_get("upgrade_playbook")?;
        let Some(playbook_key) = resolve_playbook_key(&playbooks, os) else {
            continue;
        };
        required.push(RequiredInstall {
            software_id: row.try_get("id")?,
            display_name: row.try_get("display_name")?,
            detection: row.try_get("detection")?,
            playbook_key,
        });
    }
    Ok(required)
}

fn applies_to_os(applies_to: Option<&str>, os: &str) -> bool {
    let Some(applies_to) = applies_to.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    applies_to
        .split(',')
        .map(str::trim)
        .any(|candidate| candidate == os || broad_os(candidate) == broad_os(os))
}

fn resolve_playbook_key(playbooks: &Value, os: &str) -> Option<String> {
    let playbooks = playbooks.as_object()?;
    [os, broad_os(os), "all"]
        .into_iter()
        .find(|key| {
            playbooks
                .get(*key)
                .and_then(Value::as_str)
                .is_some_and(|v| !v.trim().is_empty())
        })
        .map(str::to_owned)
}

fn broad_os(os: &str) -> &str {
    if os.starts_with("linux") {
        "linux"
    } else if os.starts_with("macos") || os.starts_with("darwin") {
        "macos"
    } else if os.starts_with("windows") {
        "windows"
    } else {
        os
    }
}

/// Run the registry-defined, side-effect-free version probe.
fn run_health_probe(detection: &Value) -> Option<String> {
    let binary = detection.get("binary")?.as_str()?;
    let args = detection
        .get("args")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str);
    let output = Command::new(binary).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Some(version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolves_exact_then_family_then_all_playbooks() {
        let playbooks = json!({"linux": "family", "linux-dgx": "exact", "all": "fallback"});
        assert_eq!(
            resolve_playbook_key(&playbooks, "linux-dgx").as_deref(),
            Some("linux-dgx")
        );
        assert_eq!(
            resolve_playbook_key(&playbooks, "linux-ubuntu").as_deref(),
            Some("linux")
        );
        assert_eq!(
            resolve_playbook_key(&json!({"all": "fallback"}), "plan9").as_deref(),
            Some("all")
        );
    }

    #[test]
    fn os_applicability_accepts_family_and_rejects_other_os() {
        assert!(applies_to_os(None, "linux-ubuntu"));
        assert!(applies_to_os(Some("linux"), "linux-dgx"));
        assert!(applies_to_os(Some("windows, macos"), "macos-15"));
        assert!(!applies_to_os(Some("macos"), "linux-ubuntu"));
    }
}
