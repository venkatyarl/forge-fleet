//! Immutable local release-artifact registration.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Subcommand;

use ff_agent::artifact_registry::{LocalReleaseArtifactSpec, register_local_release_artifact};
use ff_agent::release_artifact_activation::{
    LocalReleaseActivationRequest, activate_local_release_pair,
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
        /// Emit the immutable activation receipt as JSON.
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
            json,
        } => {
            let pool = ff_agent::fleet_info::get_fleet_pool()
                .await
                .map_err(|error| anyhow::anyhow!("connect Postgres: {error}"))?;
            let receipt = activate_local_release_pair(
                &pool,
                &LocalReleaseActivationRequest { source_commit },
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
    }
}
