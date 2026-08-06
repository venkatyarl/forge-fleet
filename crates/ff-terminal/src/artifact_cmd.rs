//! Immutable local release-artifact registration.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Subcommand;
use uuid::Uuid;

use ff_agent::artifact_registry::{LocalReleaseArtifactSpec, register_local_release_artifact};
use ff_agent::release_artifact_activation::{
    LocalReleaseActivationRequest, LocalReleaseRollbackRequest, activate_local_release_pair,
    prove_local_release_rollback, rollback_local_release_transaction,
};
use ff_agent::release_rollout_coordinator::{
    PgReleaseRolloutDatabase, ReleaseRolloutCoordinator, RolloutCoordinatorConfig,
    RolloutRosterEntry, SystemReleaseRolloutTransport,
};
use ff_db::ReleaseArtifactRegistrationOutcome;

use crate::{GREEN, RESET};

#[derive(Debug, Clone, Subcommand)]
pub enum ArtifactCommand {
    /// Verify a local release binary and record immutable content + custody.
    Register {
        /// Canonical artifact name (for example `ff`).
        #[arg(long)]
        name: String,
        /// Canonical release/build version (for example `2026.8.5_1`).
        #[arg(long)]
        version: String,
        /// Exact full lowercase 40-hex Git source commit.
        #[arg(long)]
        source_commit: String,
        /// Rust-style target triple (for example `aarch64-unknown-linux-gnu`).
        #[arg(long)]
        target: String,
        /// Exact expected lowercase SHA-256 digest.
        #[arg(long)]
        sha256: String,
        /// Exact expected file size in bytes.
        #[arg(long)]
        size_bytes: i64,
        /// Normal relative path beneath `~/.forgefleet/release-builds`.
        #[arg(long)]
        path: PathBuf,
        /// Emit the resulting immutable receipt as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Acquire local V291 custody when needed and transactionally activate the
    /// exact platform-qualified `ff` + `forgefleetd` release pair.
    Activate {
        /// Exact full lowercase 40-hex Git source commit. The release version,
        /// target platform, custody source, and all destinations are derived.
        #[arg(long)]
        source_commit: String,
        /// Exact durable transaction identity supplied by the sealed fleet
        /// coordinator. Omit for a standalone local activation.
        #[arg(long)]
        transaction_id: Option<Uuid>,
        /// Emit the immutable activation receipt as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Restore the exact predecessor pair retained by one local activation.
    Rollback {
        /// Exact local activation transaction UUID. All paths and bytes are
        /// re-derived from private durable authority; there is no force mode.
        #[arg(long)]
        transaction_id: Uuid,
        /// Emit the immutable rollback receipt as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Prove that one committed activation retains exact predecessor bytes.
    #[command(hide = true)]
    RollbackProof {
        #[arg(long)]
        transaction_id: Uuid,
        #[arg(long)]
        json: bool,
    },
    /// Plan or synchronously drive an exact V295 fleet rollout.
    Rollout {
        #[command(subcommand)]
        command: ArtifactRolloutCommand,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum ArtifactRolloutCommand {
    /// Probe an explicit operator roster and seal exact V291 artifacts into V295.
    Plan {
        #[arg(long)]
        source_commit: String,
        /// Repeatable canonical target identity in exact rollout order.
        #[arg(long = "target", value_name = "NAME=UUID", required = true)]
        targets: Vec<RolloutRosterEntry>,
        #[arg(long)]
        json: bool,
    },
    /// Create and synchronously drive one leased rollout transaction.
    Start {
        #[arg(long)]
        authority_id: Uuid,
        /// Required idempotency identity for safe command replay.
        #[arg(long)]
        request_id: Uuid,
        #[arg(long)]
        json: bool,
    },
    /// Read the durable rollout and per-target receipts without mutation.
    Status {
        #[arg(
            long,
            required_unless_present = "request_id",
            conflicts_with = "request_id"
        )]
        transaction_id: Option<Uuid>,
        #[arg(
            long,
            required_unless_present = "transaction_id",
            conflicts_with = "transaction_id"
        )]
        request_id: Option<Uuid>,
        #[arg(long)]
        json: bool,
    },
    /// Resume one crash-interrupted transaction, taking over only an expired lease.
    Resume {
        #[arg(
            long,
            required_unless_present = "request_id",
            conflicts_with = "request_id"
        )]
        transaction_id: Option<Uuid>,
        #[arg(
            long,
            required_unless_present = "transaction_id",
            conflicts_with = "transaction_id"
        )]
        request_id: Option<Uuid>,
        #[arg(long)]
        json: bool,
    },
    /// Explicitly roll back succeeded targets in reverse exact order.
    Rollback {
        #[arg(long)]
        transaction_id: Uuid,
        #[arg(long)]
        json: bool,
    },
}

pub async fn handle_artifact(command: ArtifactCommand) -> Result<()> {
    match command {
        ArtifactCommand::Register {
            name,
            version,
            source_commit,
            target,
            sha256,
            size_bytes,
            path,
            json,
        } => {
            let pool = ff_agent::fleet_info::get_fleet_pool()
                .await
                .map_err(|error| anyhow::anyhow!("connect Postgres: {error}"))?;
            let identity = ff_agent::fleet_info::resolve_this_computer_identity_strict(&pool)
                .await
                .map_err(|error| anyhow::anyhow!("resolve local custody holder: {error}"))?;
            let spec = LocalReleaseArtifactSpec {
                artifact_name: name,
                artifact_version: version,
                source_commit,
                target_triple: target,
                expected_sha256: sha256,
                expected_size_bytes: size_bytes,
                relative_path: path,
            };
            let receipt = register_local_release_artifact(&pool, &identity, &spec)
                .await
                .context("verify and register release artifact")?;

            let outcome = match receipt.outcome {
                ReleaseArtifactRegistrationOutcome::Registered => "registered",
                ReleaseArtifactRegistrationOutcome::Verified => "verified",
            };
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "outcome": outcome,
                        "artifact": {
                            "id": receipt.artifact.id,
                            "name": receipt.artifact.artifact_name,
                            "version": receipt.artifact.artifact_version,
                            "source_commit": receipt.artifact.source_commit,
                            "target_triple": receipt.artifact.target_triple,
                            "sha256": receipt.artifact.sha256,
                            "size_bytes": receipt.artifact.size_bytes,
                            "created_at": receipt.artifact.created_at,
                        },
                        "custody": {
                            "computer_id": receipt.custody.computer_id,
                            "holder": receipt.custody.holder_name_at_registration,
                            "relative_path": receipt.custody.relative_path,
                            "first_verified_at": receipt.custody.first_verified_at,
                            "last_verified_at": receipt.custody.last_verified_at,
                        }
                    }))?
                );
            } else {
                println!("{GREEN}✓ artifact {outcome}{RESET}");
                println!("  artifact_id:   {}", receipt.artifact.id);
                println!(
                    "  holder:        {}",
                    receipt.custody.holder_name_at_registration
                );
                println!("  computer_id:   {}", receipt.custody.computer_id);
                println!("  target:        {}", receipt.artifact.target_triple);
                println!("  sha256:        {}", receipt.artifact.sha256);
                println!("  size_bytes:    {}", receipt.artifact.size_bytes);
                println!("  relative_path: {}", receipt.custody.relative_path);
            }
            Ok(())
        }
        ArtifactCommand::Activate {
            source_commit,
            transaction_id,
            json,
        } => {
            let pool = ff_agent::fleet_info::get_fleet_pool()
                .await
                .map_err(|error| anyhow::anyhow!("connect Postgres: {error}"))?;
            let receipt = activate_local_release_pair(
                &pool,
                &LocalReleaseActivationRequest {
                    source_commit,
                    transaction_id,
                },
            )
            .await
            .context("acquire custody and activate exact release pair")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&receipt)?);
            } else {
                println!("{GREEN}✓ exact release pair activated{RESET}");
                println!("  transaction:  {}", receipt.transaction_id);
                println!("  computer:     {}", receipt.computer_name);
                println!("  version:      {}", receipt.artifact_version);
                println!("  source:       {}", receipt.source_commit);
                println!("  target:       {}", receipt.target_triple);
                println!("  receipt:      {}", receipt.receipt_path);
                for artifact in receipt.artifacts {
                    println!("  {}: {}", artifact.artifact_name, artifact.sha256);
                    for destination in artifact.destinations {
                        println!("    -> {destination}");
                    }
                }
            }
            Ok(())
        }
        ArtifactCommand::Rollback {
            transaction_id,
            json,
        } => {
            let pool = ff_agent::fleet_info::get_fleet_pool()
                .await
                .map_err(|error| anyhow::anyhow!("connect Postgres: {error}"))?;
            let receipt = rollback_local_release_transaction(
                &pool,
                &LocalReleaseRollbackRequest { transaction_id },
            )
            .await
            .context("restore exact predecessor release pair")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&receipt)?);
            } else {
                println!("{GREEN}✓ exact predecessor pair restored{RESET}");
                println!("  transaction: {}", receipt.transaction_id);
                println!("  computer:    {}", receipt.computer_name);
                println!("  replaced:    {}", receipt.replaced_source_commit);
                println!("  receipt:     {}", receipt.receipt_path);
            }
            Ok(())
        }
        ArtifactCommand::RollbackProof {
            transaction_id,
            json,
        } => {
            let pool = ff_agent::fleet_info::get_fleet_pool()
                .await
                .map_err(|error| anyhow::anyhow!("connect Postgres: {error}"))?;
            let proof = prove_local_release_rollback(
                &pool,
                &LocalReleaseRollbackRequest { transaction_id },
            )
            .await
            .context("prove exact predecessor release authority")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&proof)?);
            } else {
                println!("{GREEN}✓ exact rollback authority verified{RESET}");
                println!("  transaction: {}", proof.transaction_id);
                println!("  computer:    {}", proof.computer_name);
                println!("  source:      {}", proof.source_commit);
            }
            Ok(())
        }
        ArtifactCommand::Rollout { command } => handle_rollout(command).await,
    }
}

async fn handle_rollout(command: ArtifactRolloutCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|error| anyhow::anyhow!("connect Postgres: {error}"))?;
    let database = PgReleaseRolloutDatabase::new(&pool);
    let transport = SystemReleaseRolloutTransport;
    let coordinator =
        ReleaseRolloutCoordinator::new(&database, &transport, RolloutCoordinatorConfig::default());
    let (value, json, label) = match command {
        ArtifactRolloutCommand::Plan {
            source_commit,
            targets,
            json,
        } => (
            serde_json::to_value(coordinator.plan(&source_commit, &targets).await?)?,
            json,
            "exact rollout authority sealed",
        ),
        ArtifactRolloutCommand::Start {
            authority_id,
            request_id,
            json,
        } => (
            serde_json::to_value(coordinator.start(authority_id, request_id).await?)?,
            json,
            "synchronous rollout command finished",
        ),
        ArtifactRolloutCommand::Status {
            transaction_id,
            request_id,
            json,
        } => (
            serde_json::to_value(
                coordinator
                    .status_for_identity(transaction_id, request_id)
                    .await?,
            )?,
            json,
            "rollout status loaded",
        ),
        ArtifactRolloutCommand::Resume {
            transaction_id,
            request_id,
            json,
        } => (
            serde_json::to_value(
                coordinator
                    .resume_for_identity(transaction_id, request_id)
                    .await?,
            )?,
            json,
            "rollout resume command finished",
        ),
        ArtifactRolloutCommand::Rollback {
            transaction_id,
            json,
        } => (
            serde_json::to_value(coordinator.rollback(transaction_id).await?)?,
            json,
            "rollout rollback command finished",
        ),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("{GREEN}✓ {label}{RESET}");
        if let Some(id) = value
            .get("transaction")
            .and_then(|transaction| transaction.get("id"))
            .or_else(|| value.get("authority_id"))
        {
            println!("  id: {id}");
        }
        if let Some(state) = value
            .get("transaction")
            .and_then(|transaction| transaction.get("state"))
        {
            println!("  state: {state}");
        }
        if let Some(targets) = value.get("targets").and_then(|targets| targets.as_array()) {
            println!("  targets: {}", targets.len());
        }
        if let Some(extras) = value
            .get("excluded_active_extras")
            .and_then(|extras| extras.as_array())
        {
            println!("  excluded active extras: {}", extras.len());
            for extra in extras {
                if let (Some(name), Some(id)) = (
                    extra.get("computer_name").and_then(|name| name.as_str()),
                    extra.get("computer_id").and_then(|id| id.as_str()),
                ) {
                    println!("    {name}={id}");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct RolloutCli {
        #[command(subcommand)]
        command: ArtifactRolloutCommand,
    }

    #[test]
    fn status_and_resume_require_exactly_one_transaction_or_request_identity() {
        let transaction_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();
        assert!(RolloutCli::try_parse_from(["rollout", "status"]).is_err());
        assert!(
            RolloutCli::try_parse_from([
                "rollout",
                "status",
                "--transaction-id",
                &transaction_id.to_string(),
                "--request-id",
                &request_id.to_string(),
            ])
            .is_err()
        );
        let parsed = RolloutCli::try_parse_from([
            "rollout",
            "resume",
            "--request-id",
            &request_id.to_string(),
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            ArtifactRolloutCommand::Resume {
                transaction_id: None,
                request_id: Some(id),
                ..
            } if id == request_id
        ));
    }

    #[test]
    fn plan_requires_repeatable_canonical_name_uuid_targets() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        assert!(
            RolloutCli::try_parse_from(["rollout", "plan", "--source-commit", &"a".repeat(40),])
                .is_err()
        );
        let parsed = RolloutCli::try_parse_from([
            "rollout",
            "plan",
            "--source-commit",
            &"a".repeat(40),
            "--target",
            &format!("beyonce={first}"),
            "--target",
            &format!("lily={second}"),
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            ArtifactRolloutCommand::Plan { targets, .. } if targets.len() == 2
        ));
    }
}
