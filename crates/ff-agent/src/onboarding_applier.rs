//! Desired-state applier for database-defined fleet onboarding steps.
//!
//! The leader audits every registered node. Each command is guarded by its
//! `verify_check`, making the periodic pass safe to repeat and ensuring nodes
//! enrolled since the previous pass converge without a manual bootstrap step.

use anyhow::Context;
use ff_ssh::{RemoteExecutor, SshNodeConfig};
use sqlx::{PgPool, Row};
use tracing::{info, warn};

const REMOTE_TIMEOUT_SECS: u64 = 30;
const PROFILE_PROBE: &str = r#"
printf '__FF_OS__=%s\n' "$(uname -s 2>/dev/null || true)"
if [ -r /proc/meminfo ]; then
  awk '/^MemTotal:/ { printf "__FF_RAM_BYTES__=%.0f\n", $2 * 1024 }' /proc/meminfo
else
  printf '__FF_RAM_BYTES__=%s\n' "$(sysctl -n hw.memsize 2>/dev/null || echo 0)"
fi
chassis="$(cat /sys/class/dmi/id/chassis_type 2>/dev/null || true)"
case "$chassis" in 8|9|10|14) laptop=1 ;; *) laptop=0 ;; esac
if ls /sys/class/power_supply/BAT* >/dev/null 2>&1 \
   || pmset -g batt 2>/dev/null | grep -q InternalBattery; then laptop=1; fi
printf '__FF_LAPTOP__=%s\n' "$laptop"
"#;

#[derive(Debug, Clone)]
struct OnboardingStep {
    key: String,
    applies_to: String,
    command: String,
    verify_check: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NodeProfile {
    os: String,
    laptop: bool,
    low_ram: bool,
    ring: bool,
}

#[derive(Debug)]
struct AuditNode {
    ssh: SshNodeConfig,
    ring: bool,
}

/// Audit every registered online node against all enabled onboarding steps.
pub async fn audit_fleet_onboarding(pg: &PgPool) -> anyhow::Result<usize> {
    audit_onboarding(pg, None).await
}

/// Audit one node immediately after its canonical self-enrollment completes.
pub async fn audit_enrolled_node(pg: &PgPool, node_name: &str) -> anyhow::Result<usize> {
    audit_onboarding(pg, Some(node_name)).await
}

async fn audit_onboarding(pg: &PgPool, node_name: Option<&str>) -> anyhow::Result<usize> {
    let steps = load_steps(pg).await?;
    if steps.is_empty() {
        return Ok(0);
    }

    let nodes = load_nodes(pg, node_name).await?;
    let executor = RemoteExecutor::new(REMOTE_TIMEOUT_SECS, true);
    let mut applied = 0;
    for node in nodes {
        match audit_node(&executor, &node, &steps).await {
            Ok(count) => applied += count,
            Err(error) => warn!(
                node = %node.ssh.name,
                error = %error,
                "fleet onboarding audit failed for node"
            ),
        }
    }
    Ok(applied)
}

async fn load_steps(pg: &PgPool) -> anyhow::Result<Vec<OnboardingStep>> {
    let rows = sqlx::query(
        "SELECT step_key, applies_to, command, verify_check \
         FROM fleet_onboarding_steps WHERE enabled ORDER BY id",
    )
    .fetch_all(pg)
    .await
    .context("load enabled fleet onboarding steps")?;

    Ok(rows
        .into_iter()
        .map(|row| OnboardingStep {
            key: row.get("step_key"),
            applies_to: row.get("applies_to"),
            command: row.get("command"),
            verify_check: row.get("verify_check"),
        })
        .collect())
}

async fn load_nodes(pg: &PgPool, node_name: Option<&str>) -> anyhow::Result<Vec<AuditNode>> {
    let rows = sqlx::query(
        "SELECT fw.name, fw.ip, fw.ssh_user, \
                EXISTS (SELECT 1 FROM fabric_pairs p \
                        WHERE p.fabric_kind ILIKE '%ring%' \
                          AND p.status <> 'disabled' \
                          AND (p.source_node = fw.name OR p.target_node = fw.name)) AS ring \
         FROM fleet_workers fw \
         WHERE fw.status <> 'offline' \
           AND ($1::text IS NULL OR fw.name = $1) \
         ORDER BY fw.name",
    )
    .bind(node_name)
    .fetch_all(pg)
    .await
    .context("load fleet nodes for onboarding audit")?;

    Ok(rows
        .into_iter()
        .map(|row| AuditNode {
            ssh: SshNodeConfig {
                name: row.get("name"),
                host: row.get("ip"),
                port: 22,
                username: row.get("ssh_user"),
                key_path: None,
                password: None,
                alternate_ips: Vec::new(),
                batch_mode: true,
                connect_timeout_secs: Some(10),
                known_hosts_path: None,
            },
            ring: row.get("ring"),
        })
        .collect())
}

async fn audit_node(
    executor: &RemoteExecutor,
    node: &AuditNode,
    steps: &[OnboardingStep],
) -> anyhow::Result<usize> {
    let probe = executor
        .run_on_node(node.ssh.clone(), PROFILE_PROBE, false)
        .await
        .context("run node profile probe")?;
    if !probe.success {
        anyhow::bail!("profile probe failed: {}", probe.stderr.trim());
    }
    let mut profile = parse_profile(&probe.stdout).context("parse node profile probe")?;
    profile.ring = node.ring;

    let mut applied = 0;
    for step in steps.iter().filter(|step| {
        applies_to(&step.applies_to, &profile)
            && executable(&step.verify_check)
            && executable(&step.command)
    }) {
        let verified = executor
            .run_on_node(node.ssh.clone(), step.verify_check.clone(), false)
            .await
            .with_context(|| format!("verify onboarding step {}", step.key))?;
        if verified.success {
            continue;
        }

        let result = executor
            .run_on_node(node.ssh.clone(), step.command.clone(), false)
            .await
            .with_context(|| format!("apply onboarding step {}", step.key))?;
        if result.success {
            let converged = executor
                .run_on_node(node.ssh.clone(), step.verify_check.clone(), false)
                .await
                .with_context(|| format!("re-verify onboarding step {}", step.key))?;
            if converged.success {
                applied += 1;
                info!(node = %node.ssh.name, step = %step.key, "applied fleet onboarding step");
            } else {
                warn!(
                    node = %node.ssh.name,
                    step = %step.key,
                    "fleet onboarding command completed but verification still fails"
                );
            }
        } else {
            warn!(
                node = %node.ssh.name,
                step = %step.key,
                exit_code = ?result.exit_code,
                stderr = %result.stderr.trim(),
                "fleet onboarding step failed"
            );
        }
    }
    Ok(applied)
}

fn executable(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    !value.is_empty()
        && value != "n/a"
        && !value.starts_with("handled by ")
        && !value.starts_with("see ")
        && !value.starts_with("deployed in ")
}

fn parse_profile(output: &str) -> Option<NodeProfile> {
    let value = |prefix: &str| {
        output
            .lines()
            .find_map(|line| line.strip_prefix(prefix))
            .map(str::trim)
    };
    let os = match value("__FF_OS__=")?.to_ascii_lowercase().as_str() {
        "darwin" => "macos".to_string(),
        value if value.starts_with("linux") => "linux".to_string(),
        value => value.to_string(),
    };
    let ram_bytes = value("__FF_RAM_BYTES__=")?.parse::<u64>().ok()?;
    Some(NodeProfile {
        os,
        laptop: value("__FF_LAPTOP__=") == Some("1"),
        low_ram: ram_bytes < 6 * 1024 * 1024 * 1024,
        ring: false,
    })
}

fn applies_to(expression: &str, profile: &NodeProfile) -> bool {
    expression.split(',').map(str::trim).any(|selector| {
        selector.eq_ignore_ascii_case("all")
            || selector.eq_ignore_ascii_case(&profile.os)
            || (selector.eq_ignore_ascii_case("laptop") && profile.laptop)
            || (selector.eq_ignore_ascii_case("low_ram") && profile.low_ram)
            || (selector.eq_ignore_ascii_case("ring") && profile.ring)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_linux_laptop_low_ram_profile() {
        let profile =
            parse_profile("noise\n__FF_OS__=Linux\n__FF_RAM_BYTES__=5368709120\n__FF_LAPTOP__=1\n")
                .unwrap();
        assert_eq!(
            profile,
            NodeProfile {
                os: "linux".into(),
                laptop: true,
                low_ram: true,
                ring: false,
            }
        );
    }

    #[test]
    fn normalizes_darwin_and_uses_strict_six_gib_threshold() {
        let profile =
            parse_profile("__FF_OS__=Darwin\n__FF_RAM_BYTES__=6442450944\n__FF_LAPTOP__=0\n")
                .unwrap();
        assert_eq!(profile.os, "macos");
        assert!(!profile.low_ram);
    }

    #[test]
    fn selectors_match_each_profile_dimension() {
        let profile = NodeProfile {
            os: "linux".into(),
            laptop: true,
            low_ram: false,
            ring: true,
        };
        assert!(applies_to("laptop", &profile));
        assert!(applies_to("macos, linux", &profile));
        assert!(applies_to("ring", &profile));
        assert!(applies_to("all", &profile));
        assert!(!applies_to("low_ram", &profile));
    }

    #[test]
    fn skips_declarative_sentinel_steps() {
        assert!(!executable("n/a"));
        assert!(!executable("handled by version_reconciler"));
        assert!(!executable("see ff-rpc-50052.service template"));
        assert!(!executable("deployed in backend_detect.rs"));
        assert!(executable("systemctl --user is-active forgefleetd"));
    }
}
