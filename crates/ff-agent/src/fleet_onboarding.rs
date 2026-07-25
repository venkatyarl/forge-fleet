//! Applies enabled, node-local onboarding policy from `fleet_onboarding_steps`.
//!
//! The applier is deliberately verify-first and idempotent: a command only
//! runs when its verification check fails, and the check is repeated after
//! the command. The table is operator-controlled, so commands are executed
//! verbatim without interpolating node or profile data.

use std::{process::Stdio, time::Duration};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use sqlx::{PgPool, Row};
use tokio::process::Command;
use tracing::{info, warn};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeProfile {
    pub os_family: String,
    pub laptop: bool,
    pub low_ram: bool,
    pub ring: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnboardingStep {
    pub step_key: String,
    pub applies_to: String,
    pub command: Option<String>,
    pub verify_check: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepOutcome {
    NotApplicable,
    AlreadyVerified,
    Applied,
    Declarative,
    Failed,
}

#[async_trait]
trait CommandExecutor: Send + Sync {
    async fn succeeds(&self, command: &str) -> bool;
}

struct ShellExecutor;

#[async_trait]
impl CommandExecutor for ShellExecutor {
    async fn succeeds(&self, command: &str) -> bool {
        let child = Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        let Ok(mut child) = child else {
            return false;
        };
        tokio::time::timeout(COMMAND_TIMEOUT, child.wait())
            .await
            .ok()
            .and_then(Result::ok)
            .is_some_and(|status| status.success())
    }
}

fn executable(value: Option<&str>) -> Option<&str> {
    let value = value?.trim();
    if value.is_empty()
        || value.eq_ignore_ascii_case("n/a")
        || value.starts_with("handled by ")
        || value.starts_with("deployed in ")
        || value.starts_with("see ")
    {
        None
    } else {
        Some(value)
    }
}

fn applies(step: &OnboardingStep, profile: &NodeProfile) -> bool {
    step.applies_to.split(',').map(str::trim).any(|selector| {
        selector.eq_ignore_ascii_case("all")
            || (selector.eq_ignore_ascii_case("laptop") && profile.laptop)
            || (selector.eq_ignore_ascii_case("low_ram") && profile.low_ram)
            || (selector.eq_ignore_ascii_case("ring") && profile.ring)
            || selector.eq_ignore_ascii_case(&profile.os_family)
            || selector
                .strip_prefix("os:")
                .is_some_and(|os| os.eq_ignore_ascii_case(&profile.os_family))
    })
}

async fn apply_step<E: CommandExecutor>(
    step: &OnboardingStep,
    profile: &NodeProfile,
    executor: &E,
) -> StepOutcome {
    if !applies(step, profile) {
        return StepOutcome::NotApplicable;
    }

    let Some(verify) = executable(step.verify_check.as_deref()) else {
        return StepOutcome::Declarative;
    };
    if executor.succeeds(verify).await {
        return StepOutcome::AlreadyVerified;
    }

    let Some(command) = executable(step.command.as_deref()) else {
        return StepOutcome::Declarative;
    };
    if !executor.succeeds(command).await {
        return StepOutcome::Failed;
    }
    if executor.succeeds(verify).await {
        StepOutcome::Applied
    } else {
        StepOutcome::Failed
    }
}

fn json_mentions_ring(value: &Value) -> bool {
    match value {
        Value::Bool(value) => *value,
        Value::String(value) => value.eq_ignore_ascii_case("ring"),
        Value::Array(values) => values.iter().any(json_mentions_ring),
        Value::Object(values) => values.iter().any(|(key, value)| {
            (key.eq_ignore_ascii_case("ring") || key.eq_ignore_ascii_case("ring_member"))
                && !value.is_null()
                && value != &Value::Bool(false)
                || json_mentions_ring(value)
        }),
        _ => false,
    }
}

fn local_has_battery() -> bool {
    let Ok(entries) = std::fs::read_dir("/sys/class/power_supply") else {
        return false;
    };
    entries
        .filter_map(Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().starts_with("BAT"))
}

async fn load_profile(pg: &PgPool, worker_name: &str) -> Result<NodeProfile> {
    let row = sqlx::query(
        "SELECT c.os_family, c.total_ram_gb, c.metadata,
                COALESCE(fw.capabilities, '{}'::jsonb) AS capabilities,
                COALESCE(fw.resources, '{}'::jsonb) AS resources,
                COALESCE(fw.preferences, '{}'::jsonb) AS preferences
           FROM computers c
           LEFT JOIN fleet_workers fw ON fw.name = c.name
          WHERE c.name = $1",
    )
    .bind(worker_name)
    .fetch_one(pg)
    .await
    .with_context(|| format!("load onboarding profile for {worker_name}"))?;

    let metadata: Value = row.get("metadata");
    let capabilities: Value = row.get("capabilities");
    let resources: Value = row.get("resources");
    let preferences: Value = row.get("preferences");
    let laptop = metadata
        .get("laptop")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || metadata
            .get("form_factor")
            .and_then(Value::as_str)
            .is_some_and(|value| value.eq_ignore_ascii_case("laptop"))
        || local_has_battery();

    Ok(NodeProfile {
        os_family: row.get("os_family"),
        laptop,
        low_ram: row
            .get::<Option<i32>, _>("total_ram_gb")
            .is_some_and(|ram| ram < 6),
        ring: [&metadata, &capabilities, &resources, &preferences]
            .into_iter()
            .any(json_mentions_ring),
    })
}

async fn load_steps(pg: &PgPool) -> Result<Vec<OnboardingStep>> {
    let rows = sqlx::query(
        "SELECT step_key, applies_to, command, verify_check
           FROM fleet_onboarding_steps
          WHERE enabled IS TRUE
          ORDER BY id",
    )
    .fetch_all(pg)
    .await
    .context("load enabled fleet onboarding steps")?;

    Ok(rows
        .into_iter()
        .map(|row| OnboardingStep {
            step_key: row.get("step_key"),
            applies_to: row.get("applies_to"),
            command: row.get("command"),
            verify_check: row.get("verify_check"),
        })
        .collect())
}

/// Apply all enabled onboarding steps for this node.
///
/// Called immediately when `forgefleetd` starts (the enrollment path starts
/// that service) and periodically thereafter to repair configuration drift.
pub async fn audit(pg: &PgPool, worker_name: &str) -> Result<()> {
    let profile = load_profile(pg, worker_name).await?;
    let steps = load_steps(pg).await?;
    let executor = ShellExecutor;

    for step in steps {
        let outcome = apply_step(&step, &profile, &executor).await;
        match outcome {
            StepOutcome::Applied => {
                info!(step = %step.step_key, "fleet onboarding step applied")
            }
            StepOutcome::Failed => {
                warn!(step = %step.step_key, "fleet onboarding step failed verification")
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct FakeExecutor {
        results: Mutex<Vec<bool>>,
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl CommandExecutor for FakeExecutor {
        async fn succeeds(&self, command: &str) -> bool {
            self.calls.lock().unwrap().push(command.to_string());
            self.results.lock().unwrap().remove(0)
        }
    }

    fn step(applies_to: &str) -> OnboardingStep {
        OnboardingStep {
            step_key: "test".into(),
            applies_to: applies_to.into(),
            command: Some("apply".into()),
            verify_check: Some("verify".into()),
        }
    }

    fn profile() -> NodeProfile {
        NodeProfile {
            os_family: "linux-ubuntu".into(),
            laptop: true,
            low_ram: true,
            ring: false,
        }
    }

    #[test]
    fn profiles_match_tags_and_os() {
        let profile = profile();
        assert!(applies(&step("laptop"), &profile));
        assert!(applies(&step("low_ram"), &profile));
        assert!(applies(&step("os:linux-ubuntu"), &profile));
        assert!(applies(&step("macos, ring, linux-ubuntu"), &profile));
        assert!(!applies(&step("ring"), &profile));
        assert!(!applies(&step("macos"), &profile));
    }

    #[tokio::test]
    async fn verified_step_never_runs_command() {
        let executor = FakeExecutor {
            results: Mutex::new(vec![true]),
            calls: Mutex::new(Vec::new()),
        };
        assert_eq!(
            apply_step(&step("all"), &profile(), &executor).await,
            StepOutcome::AlreadyVerified
        );
        assert_eq!(*executor.calls.lock().unwrap(), ["verify"]);
    }

    #[tokio::test]
    async fn failed_verify_applies_then_reverifies() {
        let executor = FakeExecutor {
            results: Mutex::new(vec![false, true, true]),
            calls: Mutex::new(Vec::new()),
        };
        assert_eq!(
            apply_step(&step("all"), &profile(), &executor).await,
            StepOutcome::Applied
        );
        assert_eq!(
            *executor.calls.lock().unwrap(),
            ["verify", "apply", "verify"]
        );
    }

    #[tokio::test]
    async fn declarative_and_non_applicable_steps_execute_nothing() {
        let executor = FakeExecutor {
            results: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
        };
        let mut declarative = step("all");
        declarative.verify_check = Some("n/a".into());
        assert_eq!(
            apply_step(&declarative, &profile(), &executor).await,
            StepOutcome::Declarative
        );
        assert_eq!(
            apply_step(&step("ring"), &profile(), &executor).await,
            StepOutcome::NotApplicable
        );
        assert!(executor.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn ring_profile_reads_nested_metadata() {
        assert!(json_mentions_ring(
            &serde_json::json!({"tags": ["gpu", "ring"]})
        ));
        assert!(json_mentions_ring(
            &serde_json::json!({"ring_member": "beyonce"})
        ));
        assert!(!json_mentions_ring(
            &serde_json::json!({"ring": false, "tags": ["gpu"]})
        ));
    }
}
