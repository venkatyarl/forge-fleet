//! `ff-deploy` — deployment/release orchestration primitives for ForgeFleet.
//!
//! This crate provides:
//! - release domain models (`release`)
//! - deploy target resolution with retry (`resolution`)
//! - rollout strategy + planning (`strategy`, `rollout`)
//! - health gate evaluation (`health_gate`)
//! - rollback decisioning and planning (`rollback`)
//! - deployment orchestration interfaces (`deployer`)

pub mod config;
pub mod daemon;
pub mod deployer;
pub mod health_gate;
pub mod node;
pub mod release;
pub mod resolution;
pub mod rollback;
pub mod rollout;
pub mod strategy;

#[cfg(test)]
mod deploy_tests;

pub use config::DeployConfig;
pub use daemon::{ActiveLease, RestartReport, restart_with_lease_drain};
pub use deployer::{DeploymentAdapter, DeploymentOrchestrator, DeploymentReport, StepOutcome};
pub use health_gate::{
    HealthGate, HealthGateConfig, HealthGateEvaluation, HealthGateStatus, HealthSnapshot,
};
pub use node::{
    forgefleetd_restart_command, restart_forgefleetd_local, restart_forgefleetd_with_drain,
};
pub use release::{ReleaseChannel, ReleaseManifest, ReleaseRecord, ReleaseState};
pub use resolution::{ResolutionError, ResolutionRetryPolicy, ResolvedTarget, resolve_with_retry};
pub use rollback::{
    RollbackAction, RollbackCause, RollbackContext, RollbackDecider, RollbackDecision,
    RollbackPlan, RollbackPlanner, RollbackSeverity, RollbackStep,
};
pub use rollout::{RolloutError, RolloutPhase, RolloutPlan, RolloutPlanner, RolloutStep};
pub use strategy::{CanaryStrategy, FullStrategy, RolloutStrategy, StagedStrategy, StrategyError};

/// Crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Return `true` if the git working tree at `repo_path` has tracked modifications.
///
/// Mirrors the leader-local deploy guard in `ff-terminal/src/fleet_cmd.rs`:
/// only tracked changes count; untracked files are deliberately ignored so
/// operator artifacts (`research/`, `graphify-out/`, etc.) do not block a
/// deploy that only builds from tracked sources.
pub fn git_tree_is_dirty(repo_path: &Path) -> Result<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(repo_path)
        .output()
        .context("failed to spawn git status")?;

    if !output.status.success() {
        bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

/// Stash tracked modifications in `repo_path` with a recoverable label.
///
/// Returns the label used so callers can surface it in logs or error messages.
/// The stash is reachable via `git stash list` and `refs/stash`.
pub fn git_stash_dirty_tree(repo_path: &Path) -> Result<String> {
    let label = format!(
        "ff-deploy-dirty-guard-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
    );

    let output = Command::new("git")
        .args(["stash", "push", "-m", &label])
        .current_dir(repo_path)
        .output()
        .context("failed to spawn git stash")?;

    if !output.status.success() {
        bail!(
            "working tree is dirty and stash failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(label)
}

/// Fetch `origin` and hard-reset to `remote_ref`, first stashing any tracked
/// modifications if the tree is dirty.
///
/// This is the deploy-playbook equivalent of the leader-local guard: the tree
/// is checked before `git reset --hard` runs. If it is dirty, changes are
/// preserved in a labeled stash rather than discarded.
pub fn git_fetch_and_reset_hard(repo_path: &Path, remote_ref: &str) -> Result<()> {
    if git_tree_is_dirty(repo_path)? {
        let label = git_stash_dirty_tree(repo_path)
            .context("working tree is dirty and could not be stashed before reset --hard")?;
        tracing::warn!(
            label = %label,
            path = %repo_path.display(),
            "stashed dirty deploy tree before git reset --hard"
        );
    }

    git_run(repo_path, &["fetch", "origin"])
        .context("failed to fetch origin before reset --hard")?;
    git_run(repo_path, &["reset", "--hard", remote_ref])
        .with_context(|| format!("failed to reset --hard to {remote_ref}"))?;

    Ok(())
}

fn git_run(repo_path: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_path)
        .output()
        .with_context(|| format!("failed to spawn git {}", args.join(" ")))?;

    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}
