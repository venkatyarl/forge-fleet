//! Operator-driven, exact-artifact fleet rollout coordination.
//!
//! V291 remains the byte/custody authority and V295 remains the sealed target
//! and lease/CAS authority. This module deliberately has no daemon tick: every
//! transition is made synchronously by `ff artifact rollout ...` while a live
//! lease is renewed by that foreground command.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::net::IpAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;
use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ff_db::rollout_authority::pg_claim_succeeded_release_rollout_rollback;
use ff_db::{
    FORBIDDEN_VINNY_ID, PgPool, ReleaseArtifactRow, ReleaseRolloutAuthoritySpec,
    ReleaseRolloutTransactionRow, RolloutArtifactAuthority, RolloutAuthorityRegistration,
    RolloutTargetAuthority, RolloutTransactionBegin, RolloutTransactionBeginOutcome,
    pg_begin_release_rollout, pg_cas_release_rollout_target_state,
    pg_cas_release_rollout_transaction_state, pg_get_release_rollout_transaction_by_request_id,
    pg_register_release_rollout_authority, pg_renew_release_rollout_lease,
    pg_take_over_release_rollout_lease,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use uuid::Uuid;

use crate::release_artifact_activation::{
    ReleaseActivationReceipt, ReleaseRollbackProof, ReleaseRollbackReceipt,
};

pub const ROLLOUT_CANARIES: [&str; 4] = ["beyonce", "lily", "ace", "logan"];
pub const MAX_ROLLOUT_TARGETS: usize = 64;
const LEASE_OWNER: &str = "ff-artifact-rollout";
const DEFAULT_LEASE_SECONDS: i32 = 120;
const RUN_CANDIDATE_SCRIPT: &str = r#"set -eu
tx=$1; expected_sha=$2; expected_size=$3; platform=$4; provided_candidate=$5; shift 5
base="$HOME/.forgefleet/release-rollout/candidates/$tx"
candidate="$base/ff"
[ "$candidate" = "$provided_candidate" ]
# Do not add a filesystem-presence lock here.  Each artifact subcommand holds
# the release activation layer's kernel flock, which is released on process
# death; a mkdir/trap lock can survive a severed SSH session and block resume.
[ -f "$candidate" ] && [ ! -L "$candidate" ]
actual_size=$(/usr/bin/wc -c < "$candidate" | /usr/bin/tr -d ' ')
if command -v sha256sum >/dev/null 2>&1; then actual_sha=$(sha256sum "$candidate" | /usr/bin/awk '{print $1}'); else actual_sha=$(shasum -a 256 "$candidate" | /usr/bin/awk '{print $1}'); fi
[ "$actual_size" = "$expected_size" ] && [ "$actual_sha" = "$expected_sha" ]
if [ "$platform" = macos ]; then /usr/bin/codesign --verify --strict "$candidate"; fi
"$candidate" "$@"
"#;

#[derive(Debug, thiserror::Error)]
pub enum ReleaseRolloutError {
    #[error("release rollout refused: {0}")]
    Refused(String),
    #[error("release rollout database operation failed: {0}")]
    Database(#[from] ff_db::DbError),
    #[error("release rollout transport failed: {0}")]
    Transport(String),
    #[error("release rollout lease was lost during {0}; remote state must be adopted by resume")]
    LeaseLost(String),
    #[error("release rollout rollback remains incomplete: {0}")]
    RollbackIncomplete(String),
    #[error("release rollout JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("release rollout I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("release rollout worker failed: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error(
        "release rollout began as transaction {transaction_id} for request {request_id}, then failed: {source}"
    )]
    AfterBegin {
        transaction_id: Uuid,
        request_id: Uuid,
        source: Box<ReleaseRolloutError>,
    },
}

type Result<T> = std::result::Result<T, ReleaseRolloutError>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RolloutEndpoint {
    pub computer_id: Uuid,
    pub computer_name: String,
    pub ssh_user: String,
    pub ip: String,
    pub ssh_port: u16,
    pub computer_status: String,
    pub worker_status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RolloutRosterEntry {
    pub computer_name: String,
    pub computer_id: Uuid,
}

impl FromStr for RolloutRosterEntry {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let (computer_name, computer_id) = value
            .split_once('=')
            .filter(|(_, id)| !id.contains('='))
            .ok_or_else(|| "target must be exactly NAME=UUID".to_string())?;
        if !canonical_name(computer_name) || computer_name == "vinny" {
            return Err("target name must be canonical lowercase and cannot be vinny".to_string());
        }
        let parsed = Uuid::parse_str(computer_id)
            .map_err(|_| "target UUID must be canonical lowercase hyphenated form".to_string())?;
        if parsed.is_nil() || parsed.hyphenated().to_string() != computer_id {
            return Err("target UUID must be canonical lowercase hyphenated form".to_string());
        }
        if parsed == FORBIDDEN_VINNY_ID {
            return Err("Vinny is forbidden by exact UUID".to_string());
        }
        Ok(Self {
            computer_name: computer_name.to_string(),
            computer_id: parsed,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlatformProbe {
    pub target_triple: String,
    pub release_qualifier: String,
    pub platform: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactCustodySource {
    pub endpoint: RolloutEndpoint,
    pub relative_path: String,
    pub first_verified_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RolloutArtifactMaterial {
    pub artifact: ReleaseArtifactRow,
    pub origin: ArtifactCustodySource,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RolloutTarget {
    pub target_ordinal: u32,
    pub endpoint: RolloutEndpoint,
    pub target_triple: String,
    pub artifact_version: String,
    pub artifacts: Vec<RolloutArtifactMaterial>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RolloutPlanReceipt {
    pub authority_id: Uuid,
    pub source_commit: String,
    pub outcome: String,
    pub targets: Vec<RolloutTarget>,
    pub excluded_active_extras: Vec<RolloutRosterEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RolloutTargetState {
    pub target: RolloutTarget,
    pub state: String,
    pub cas_revision: i64,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RolloutStatus {
    pub transaction: ReleaseRolloutTransactionRow,
    pub source_commit: String,
    pub targets: Vec<RolloutTargetState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CandidateReceipt {
    pub transaction_id: Uuid,
    pub computer_id: Uuid,
    pub computer_name: String,
    pub absolute_path: String,
    pub sha256: String,
    pub size_bytes: i64,
    pub platform: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HealthBakeEvidence {
    pub transaction_id: Uuid,
    pub computer_id: Uuid,
    pub computer_name: String,
    pub source_commit: String,
    pub bake_seconds: u64,
    pub verified_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct TargetEvidence {
    phase: String,
    candidate: Option<CandidateReceipt>,
    activation: Option<ReleaseActivationReceipt>,
    rollback_proof: Option<ReleaseRollbackProof>,
    health: Option<HealthBakeEvidence>,
    rollback: Option<ReleaseRollbackReceipt>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RolloutCoordinatorConfig {
    pub lease_seconds: i32,
    pub lease_renew_interval: Duration,
    pub canary_bake: Duration,
    pub remaining_bake: Duration,
}

impl Default for RolloutCoordinatorConfig {
    fn default() -> Self {
        Self {
            lease_seconds: DEFAULT_LEASE_SECONDS,
            lease_renew_interval: Duration::from_secs(30),
            canary_bake: Duration::from_secs(60),
            remaining_bake: Duration::from_secs(15),
        }
    }
}

#[async_trait]
pub trait ReleaseRolloutDatabase: Send + Sync {
    async fn planning_targets(&self) -> Result<Vec<RolloutEndpoint>>;
    async fn artifact_materials(&self, source_commit: &str)
    -> Result<Vec<RolloutArtifactMaterial>>;
    async fn register_authority(
        &self,
        spec: &ReleaseRolloutAuthoritySpec,
    ) -> Result<RolloutAuthorityRegistration>;
    async fn begin(
        &self,
        request_id: Uuid,
        authority_id: Uuid,
        lease_seconds: i32,
    ) -> Result<RolloutTransactionBegin>;
    async fn transaction_by_request_id(
        &self,
        request_id: Uuid,
    ) -> Result<Option<ReleaseRolloutTransactionRow>>;
    async fn status(&self, transaction_id: Uuid) -> Result<Option<RolloutStatus>>;
    async fn renew(
        &self,
        transaction: &ReleaseRolloutTransactionRow,
    ) -> Result<Option<ReleaseRolloutTransactionRow>>;
    async fn take_over(
        &self,
        transaction: &ReleaseRolloutTransactionRow,
    ) -> Result<Option<ReleaseRolloutTransactionRow>>;
    async fn claim_succeeded_rollback(
        &self,
        transaction: &ReleaseRolloutTransactionRow,
    ) -> Result<Option<ReleaseRolloutTransactionRow>>;
    async fn cas_transaction(
        &self,
        transaction: &ReleaseRolloutTransactionRow,
        expected_state: &str,
        new_state: &str,
    ) -> Result<Option<ReleaseRolloutTransactionRow>>;
    async fn cas_target(
        &self,
        transaction: &ReleaseRolloutTransactionRow,
        target: &RolloutTargetState,
        expected_state: &str,
        new_state: &str,
        detail: Option<&str>,
    ) -> Result<bool>;
}

#[async_trait]
pub trait ReleaseRolloutTransport: Send + Sync {
    async fn probe_platform(
        &self,
        target: &RolloutEndpoint,
        source_commit: &str,
    ) -> Result<PlatformProbe>;
    async fn bootstrap_candidate(
        &self,
        transaction_id: Uuid,
        target: &RolloutTarget,
    ) -> Result<CandidateReceipt>;
    async fn activate(
        &self,
        candidate: &CandidateReceipt,
        target: &RolloutTarget,
        source_commit: &str,
    ) -> Result<ReleaseActivationReceipt>;
    async fn prove_rollback(
        &self,
        candidate: &CandidateReceipt,
        target: &RolloutTarget,
    ) -> Result<ReleaseRollbackProof>;
    async fn health_and_bake(
        &self,
        candidate: &CandidateReceipt,
        target: &RolloutTarget,
        source_commit: &str,
        bake: Duration,
    ) -> Result<HealthBakeEvidence>;
    async fn rollback(
        &self,
        candidate: &CandidateReceipt,
        target: &RolloutTarget,
    ) -> Result<ReleaseRollbackReceipt>;
}

pub struct PgReleaseRolloutDatabase<'a> {
    pool: &'a PgPool,
}

impl<'a> PgReleaseRolloutDatabase<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }
}

fn endpoint_from_row(row: &sqlx::postgres::PgRow) -> Result<RolloutEndpoint> {
    let ssh_port: i32 = row.get("ssh_port");
    let ssh_port = u16::try_from(ssh_port)
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(|| ReleaseRolloutError::Refused("invalid canonical SSH port".into()))?;
    let endpoint = RolloutEndpoint {
        computer_id: row.get("computer_id"),
        computer_name: row.get("computer_name"),
        ssh_user: row.get("ssh_user"),
        ip: row.get("primary_ip"),
        ssh_port,
        computer_status: row.get("computer_status"),
        worker_status: row.get("worker_status"),
    };
    validate_endpoint_shape(&endpoint)?;
    Ok(endpoint)
}

#[async_trait]
impl ReleaseRolloutDatabase for PgReleaseRolloutDatabase<'_> {
    async fn planning_targets(&self) -> Result<Vec<RolloutEndpoint>> {
        let rows = sqlx::query(
            "SELECT c.id AS computer_id, c.name AS computer_name, c.primary_ip,
                    c.ssh_user, c.ssh_port, c.status AS computer_status,
                    fw.status AS worker_status
               FROM computers c
               JOIN fleet_workers fw ON fw.name = c.name
              ORDER BY c.name, c.id",
        )
        .fetch_all(self.pool)
        .await
        .map_err(ff_db::DbError::from)?;
        rows.iter().map(endpoint_from_row).collect()
    }

    async fn artifact_materials(
        &self,
        source_commit: &str,
    ) -> Result<Vec<RolloutArtifactMaterial>> {
        validate_source_commit(source_commit)?;
        let rows = sqlx::query(
            "SELECT a.id, a.artifact_name, a.artifact_version, a.source_commit,
                    a.target_triple, a.sha256, a.size_bytes, a.created_at,
                    custody.computer_id, c.name AS computer_name,
                    custody.holder_name_at_registration,
                    custody.relative_path, custody.first_verified_at,
                    c.primary_ip, c.ssh_user, c.ssh_port,
                    c.status AS computer_status, fw.status AS worker_status
               FROM release_artifacts a
               JOIN release_artifact_custody custody ON custody.artifact_id = a.id
               JOIN computers c ON c.id = custody.computer_id
               JOIN fleet_workers fw ON fw.name = c.name
              WHERE a.source_commit = $1
              ORDER BY a.artifact_name, a.target_triple, a.artifact_version,
                       custody.first_verified_at, custody.computer_id",
        )
        .bind(source_commit)
        .fetch_all(self.pool)
        .await
        .map_err(ff_db::DbError::from)?;
        let mut grouped: BTreeMap<Uuid, (ReleaseArtifactRow, Vec<ArtifactCustodySource>)> =
            BTreeMap::new();
        for row in rows {
            let artifact = ReleaseArtifactRow {
                id: row.get("id"),
                artifact_name: row.get("artifact_name"),
                artifact_version: row.get("artifact_version"),
                source_commit: row.get("source_commit"),
                target_triple: row.get("target_triple"),
                sha256: row.get("sha256"),
                size_bytes: row.get("size_bytes"),
                created_at: row.get("created_at"),
            };
            let endpoint = endpoint_from_row(&row)?;
            let holder: String = row.get("holder_name_at_registration");
            if endpoint.computer_name != holder || forbidden_target(&endpoint) {
                return Err(ReleaseRolloutError::Refused(
                    "artifact custody holder identity drifted or names Vinny".into(),
                ));
            }
            grouped
                .entry(artifact.id)
                .or_insert_with(|| (artifact.clone(), Vec::new()))
                .1
                .push(ArtifactCustodySource {
                    endpoint,
                    relative_path: row.get("relative_path"),
                    first_verified_at: row.get("first_verified_at"),
                });
        }
        grouped
            .into_values()
            .map(|(artifact, custody)| {
                let earliest = custody
                    .iter()
                    .map(|entry| entry.first_verified_at)
                    .min()
                    .ok_or_else(|| {
                        ReleaseRolloutError::Refused(format!(
                            "artifact {} has no custody origin",
                            artifact.id
                        ))
                    })?;
                let origins = custody
                    .into_iter()
                    .filter(|entry| entry.first_verified_at == earliest)
                    .collect::<Vec<_>>();
                if origins.len() != 1 {
                    return Err(ReleaseRolloutError::Refused(format!(
                        "artifact {} has ambiguous earliest custody",
                        artifact.id
                    )));
                }
                Ok(RolloutArtifactMaterial {
                    artifact,
                    origin: origins.into_iter().next().expect("one origin"),
                })
            })
            .collect()
    }

    async fn register_authority(
        &self,
        spec: &ReleaseRolloutAuthoritySpec,
    ) -> Result<RolloutAuthorityRegistration> {
        Ok(pg_register_release_rollout_authority(self.pool, spec).await?)
    }

    async fn begin(
        &self,
        request_id: Uuid,
        authority_id: Uuid,
        lease_seconds: i32,
    ) -> Result<RolloutTransactionBegin> {
        Ok(pg_begin_release_rollout(
            self.pool,
            request_id,
            authority_id,
            LEASE_OWNER,
            lease_seconds,
        )
        .await?)
    }

    async fn status(&self, transaction_id: Uuid) -> Result<Option<RolloutStatus>> {
        load_pg_status(self.pool, transaction_id).await
    }

    async fn transaction_by_request_id(
        &self,
        request_id: Uuid,
    ) -> Result<Option<ReleaseRolloutTransactionRow>> {
        Ok(pg_get_release_rollout_transaction_by_request_id(self.pool, request_id).await?)
    }

    async fn renew(
        &self,
        transaction: &ReleaseRolloutTransactionRow,
    ) -> Result<Option<ReleaseRolloutTransactionRow>> {
        Ok(pg_renew_release_rollout_lease(
            self.pool,
            transaction.id,
            transaction.lease_token,
            transaction.cas_revision,
        )
        .await?)
    }

    async fn take_over(
        &self,
        transaction: &ReleaseRolloutTransactionRow,
    ) -> Result<Option<ReleaseRolloutTransactionRow>> {
        Ok(pg_take_over_release_rollout_lease(
            self.pool,
            transaction.id,
            transaction.cas_revision,
            LEASE_OWNER,
        )
        .await?)
    }

    async fn claim_succeeded_rollback(
        &self,
        transaction: &ReleaseRolloutTransactionRow,
    ) -> Result<Option<ReleaseRolloutTransactionRow>> {
        Ok(pg_claim_succeeded_release_rollout_rollback(
            self.pool,
            transaction.id,
            transaction.cas_revision,
            LEASE_OWNER,
        )
        .await?)
    }

    async fn cas_transaction(
        &self,
        transaction: &ReleaseRolloutTransactionRow,
        expected_state: &str,
        new_state: &str,
    ) -> Result<Option<ReleaseRolloutTransactionRow>> {
        Ok(pg_cas_release_rollout_transaction_state(
            self.pool,
            transaction.id,
            transaction.lease_token,
            transaction.cas_revision,
            expected_state,
            new_state,
        )
        .await?)
    }

    async fn cas_target(
        &self,
        transaction: &ReleaseRolloutTransactionRow,
        target: &RolloutTargetState,
        expected_state: &str,
        new_state: &str,
        detail: Option<&str>,
    ) -> Result<bool> {
        Ok(pg_cas_release_rollout_target_state(
            self.pool,
            transaction.id,
            target.target.endpoint.computer_id,
            transaction.lease_token,
            target.cas_revision,
            expected_state,
            new_state,
            detail,
        )
        .await?
        .is_some())
    }
}

async fn load_pg_status(pool: &PgPool, transaction_id: Uuid) -> Result<Option<RolloutStatus>> {
    if !ff_db::release_rollout_schema_is_exact(pool).await? {
        return Err(ReleaseRolloutError::Refused(
            "V295/V297 rollout schema or sealed authority data drifted".into(),
        ));
    }
    let Some(parent) = sqlx::query(
        "SELECT rollout.id, rollout.request_id, rollout.authority_id, rollout.state,
                rollout.lease_token, rollout.lease_owner, rollout.lease_expires_at,
                rollout.lease_seconds, rollout.cas_revision,
                rollout.expected_target_count, authority.source_commit
           FROM release_rollout_transactions rollout
           JOIN release_rollout_authorities authority ON authority.id = rollout.authority_id
          WHERE rollout.id = $1 AND authority.sealed_at IS NOT NULL",
    )
    .bind(transaction_id)
    .fetch_optional(pool)
    .await
    .map_err(ff_db::DbError::from)?
    else {
        return Ok(None);
    };
    let transaction = ReleaseRolloutTransactionRow {
        id: parent.get("id"),
        request_id: parent.get("request_id"),
        authority_id: parent.get("authority_id"),
        state: parent.get("state"),
        lease_token: parent.get("lease_token"),
        lease_owner: parent.get("lease_owner"),
        lease_expires_at: parent.get("lease_expires_at"),
        lease_seconds: parent.get("lease_seconds"),
        cas_revision: parent.get("cas_revision"),
        expected_target_count: parent.get("expected_target_count"),
    };
    let source_commit: String = parent.get("source_commit");
    let rows = sqlx::query(
        "SELECT state.computer_id, state.computer_name, state.target_ordinal,
                state.target_triple, state.artifact_version, state.state,
                state.cas_revision, state.detail, c.primary_ip, c.ssh_user,
                c.ssh_port, c.status AS computer_status, fw.status AS worker_status
           FROM release_rollout_target_states state
           JOIN computers c ON c.id = state.computer_id AND c.name = state.computer_name
           JOIN fleet_workers fw ON fw.name = c.name
          WHERE state.transaction_id = $1
          ORDER BY state.target_ordinal",
    )
    .bind(transaction_id)
    .fetch_all(pool)
    .await
    .map_err(ff_db::DbError::from)?;
    if rows.len() != transaction.expected_target_count as usize {
        return Err(ReleaseRolloutError::Refused(
            "rollout target state set is partial".into(),
        ));
    }
    let artifact_rows = sqlx::query(
        "SELECT exact.computer_id, artifact.id, artifact.artifact_name,
                artifact.artifact_version, artifact.source_commit,
                artifact.target_triple, artifact.sha256, artifact.size_bytes,
                artifact.created_at, custody.computer_id AS origin_computer_id,
                custody.holder_name_at_registration, custody.relative_path,
                custody.first_verified_at, origin.primary_ip, origin.ssh_user,
                origin.ssh_port, origin.status AS computer_status,
                fw.status AS worker_status
           FROM release_rollout_authority_artifacts exact
           JOIN release_artifacts artifact ON artifact.id = exact.artifact_id
           JOIN release_artifact_custody custody ON custody.artifact_id = artifact.id
           JOIN computers origin ON origin.id = custody.computer_id
           JOIN fleet_workers fw ON fw.name = origin.name
          WHERE exact.authority_id = $1
          ORDER BY exact.computer_id, artifact.artifact_name,
                   custody.first_verified_at, custody.computer_id",
    )
    .bind(transaction.authority_id)
    .fetch_all(pool)
    .await
    .map_err(ff_db::DbError::from)?;
    let mut materials: BTreeMap<(Uuid, Uuid), (ReleaseArtifactRow, Vec<ArtifactCustodySource>)> =
        BTreeMap::new();
    for row in artifact_rows {
        let target_id: Uuid = row.get("computer_id");
        let artifact = ReleaseArtifactRow {
            id: row.get("id"),
            artifact_name: row.get("artifact_name"),
            artifact_version: row.get("artifact_version"),
            source_commit: row.get("source_commit"),
            target_triple: row.get("target_triple"),
            sha256: row.get("sha256"),
            size_bytes: row.get("size_bytes"),
            created_at: row.get("created_at"),
        };
        let endpoint = RolloutEndpoint {
            computer_id: row.get("origin_computer_id"),
            computer_name: row.get("holder_name_at_registration"),
            ssh_user: row.get("ssh_user"),
            ip: row.get("primary_ip"),
            ssh_port: u16::try_from(row.get::<i32, _>("ssh_port"))
                .map_err(|_| ReleaseRolloutError::Refused("invalid custody SSH port".into()))?,
            computer_status: row.get("computer_status"),
            worker_status: row.get("worker_status"),
        };
        validate_endpoint(&endpoint)?;
        materials
            .entry((target_id, artifact.id))
            .or_insert_with(|| (artifact, Vec::new()))
            .1
            .push(ArtifactCustodySource {
                endpoint,
                relative_path: row.get("relative_path"),
                first_verified_at: row.get("first_verified_at"),
            });
    }
    let mut by_target: BTreeMap<Uuid, Vec<RolloutArtifactMaterial>> = BTreeMap::new();
    for ((target_id, _), (artifact, custody)) in materials {
        let earliest = custody
            .iter()
            .map(|c| c.first_verified_at)
            .min()
            .ok_or_else(|| ReleaseRolloutError::Refused("sealed artifact has no custody".into()))?;
        let origins = custody
            .into_iter()
            .filter(|c| c.first_verified_at == earliest)
            .collect::<Vec<_>>();
        if origins.len() != 1 {
            return Err(ReleaseRolloutError::Refused(
                "sealed artifact has ambiguous custody origin".into(),
            ));
        }
        by_target
            .entry(target_id)
            .or_default()
            .push(RolloutArtifactMaterial {
                artifact,
                origin: origins.into_iter().next().expect("one origin"),
            });
    }
    let mut targets = Vec::with_capacity(rows.len());
    for row in rows {
        let endpoint = endpoint_from_row(&row)?;
        let artifacts = by_target.remove(&endpoint.computer_id).ok_or_else(|| {
            ReleaseRolloutError::Refused("sealed target artifact set is missing".into())
        })?;
        let target = RolloutTarget {
            target_ordinal: u32::try_from(row.get::<i32, _>("target_ordinal"))
                .map_err(|_| ReleaseRolloutError::Refused("negative target ordinal".into()))?,
            endpoint,
            target_triple: row.get("target_triple"),
            artifact_version: row.get("artifact_version"),
            artifacts,
        };
        validate_target_material(&target, &source_commit)?;
        targets.push(RolloutTargetState {
            target,
            state: row.get("state"),
            cas_revision: row.get("cas_revision"),
            detail: row.get("detail"),
        });
    }
    validate_status_invariants(&targets)?;
    Ok(Some(RolloutStatus {
        transaction,
        source_commit,
        targets,
    }))
}

fn validate_source_commit(value: &str) -> Result<()> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ReleaseRolloutError::Refused(
            "source commit must be full lowercase 40-hex".into(),
        ));
    }
    Ok(())
}

fn canonical_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
}

fn forbidden_target(target: &RolloutEndpoint) -> bool {
    target.computer_id == FORBIDDEN_VINNY_ID || target.computer_name.eq_ignore_ascii_case("vinny")
}

fn validate_endpoint(target: &RolloutEndpoint) -> Result<()> {
    if forbidden_target(target) {
        return Err(ReleaseRolloutError::Refused(
            "Vinny is forbidden by exact name and UUID".into(),
        ));
    }
    validate_endpoint_shape(target)
}

fn validate_endpoint_shape(target: &RolloutEndpoint) -> Result<()> {
    if target.computer_id.is_nil()
        || !canonical_name(&target.computer_name)
        || target.ssh_user.is_empty()
        || target.ssh_user.len() > 64
        || !target
            .ssh_user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        || target.ssh_port == 0
    {
        return Err(ReleaseRolloutError::Refused(
            "target endpoint identity is not canonical".into(),
        ));
    }
    let ip: IpAddr = target
        .ip
        .parse()
        .map_err(|_| ReleaseRolloutError::Refused("target address is not an IP literal".into()))?;
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return Err(ReleaseRolloutError::Refused(
            "target address is not a usable fleet address".into(),
        ));
    }
    Ok(())
}

fn validate_relative_path(path: &str) -> Result<()> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            !matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            ) || matches!(component, std::path::Component::CurDir)
        })
        || !path.as_os_str().as_encoded_bytes().iter().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-' | b'+')
        })
    {
        return Err(ReleaseRolloutError::Refused(
            "custody path is not a safe normal relative path".into(),
        ));
    }
    Ok(())
}

fn validate_artifact(material: &RolloutArtifactMaterial) -> Result<()> {
    validate_endpoint(&material.origin.endpoint)?;
    validate_relative_path(&material.origin.relative_path)?;
    let row = &material.artifact;
    validate_source_commit(&row.source_commit)?;
    if !matches!(row.artifact_name.as_str(), "ff" | "forgefleetd")
        || row.size_bytes <= 0
        || row.sha256.len() != 64
        || !row
            .sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(ReleaseRolloutError::Refused(
            "artifact material identity is invalid".into(),
        ));
    }
    Ok(())
}

fn validate_target_material(target: &RolloutTarget, source_commit: &str) -> Result<()> {
    validate_endpoint(&target.endpoint)?;
    if target.artifacts.len() != 2 {
        return Err(ReleaseRolloutError::Refused(
            "target requires exactly ff and forgefleetd".into(),
        ));
    }
    let mut names = BTreeSet::new();
    let mut origins = BTreeSet::new();
    for material in &target.artifacts {
        validate_artifact(material)?;
        let row = &material.artifact;
        if row.source_commit != source_commit
            || row.target_triple != target.target_triple
            || row.artifact_version != target.artifact_version
        {
            return Err(ReleaseRolloutError::Refused(
                "target artifact pair drifted from exact authority".into(),
            ));
        }
        names.insert(row.artifact_name.as_str());
        origins.insert(material.origin.endpoint.computer_id);
    }
    if names != BTreeSet::from(["ff", "forgefleetd"]) || origins.len() != 1 {
        return Err(ReleaseRolloutError::Refused(
            "target pair is partial or lacks one canonical shared origin".into(),
        ));
    }
    Ok(())
}

fn validate_requested_roster_order(requested: &[RolloutRosterEntry]) -> Result<()> {
    if requested.len() < ROLLOUT_CANARIES.len() || requested.len() > MAX_ROLLOUT_TARGETS {
        return Err(ReleaseRolloutError::Refused(
            "fleet rollout requires the complete bounded canary roster".into(),
        ));
    }
    let mut ids = BTreeSet::new();
    let mut names = BTreeSet::new();
    for target in requested {
        if !canonical_name(&target.computer_name)
            || target.computer_id.is_nil()
            || target.computer_id == FORBIDDEN_VINNY_ID
            || target.computer_name == "vinny"
            || !ids.insert(target.computer_id)
            || !names.insert(target.computer_name.as_str())
        {
            return Err(ReleaseRolloutError::Refused(
                "operator roster contains non-canonical, duplicate, or forbidden identity".into(),
            ));
        }
    }
    if requested
        .iter()
        .take(ROLLOUT_CANARIES.len())
        .map(|target| target.computer_name.as_str())
        .ne(ROLLOUT_CANARIES)
    {
        return Err(ReleaseRolloutError::Refused(
            "operator roster must begin Beyoncé -> Lily -> Ace -> Logan".into(),
        ));
    }
    if requested[ROLLOUT_CANARIES.len()..]
        .windows(2)
        .any(|pair| pair[0].computer_name >= pair[1].computer_name)
    {
        return Err(ReleaseRolloutError::Refused(
            "operator roster after the canaries must be in strict lexical order".into(),
        ));
    }
    Ok(())
}

fn resolve_requested_roster(
    requested: &[RolloutRosterEntry],
    registry: Vec<RolloutEndpoint>,
) -> Result<(Vec<RolloutEndpoint>, Vec<RolloutRosterEntry>)> {
    validate_requested_roster_order(requested)?;
    let mut by_id = BTreeMap::new();
    let mut by_name = BTreeMap::new();
    for target in registry {
        if forbidden_target(&target) {
            continue;
        }
        if by_id.insert(target.computer_id, target.clone()).is_some()
            || by_name
                .insert(target.computer_name.clone(), target)
                .is_some()
        {
            return Err(ReleaseRolloutError::Refused(
                "live fleet registry contains duplicate target identity".into(),
            ));
        }
    }

    let mut selected = Vec::with_capacity(requested.len());
    for exact in requested {
        let by_exact_id = by_id.get(&exact.computer_id);
        let by_exact_name = by_name.get(&exact.computer_name);
        let endpoint = match (by_exact_id, by_exact_name) {
            (None, None) => {
                return Err(ReleaseRolloutError::Refused(format!(
                    "operator target {}={} is missing from the live registry",
                    exact.computer_name, exact.computer_id
                )));
            }
            (Some(by_id), Some(by_name))
                if by_id.computer_id == by_name.computer_id
                    && by_id.computer_name == exact.computer_name
                    && by_id.computer_id == exact.computer_id =>
            {
                by_id.clone()
            }
            _ => {
                return Err(ReleaseRolloutError::Refused(format!(
                    "operator target {}={} drifted from the live name+UUID registry",
                    exact.computer_name, exact.computer_id
                )));
            }
        };
        validate_endpoint(&endpoint)?;
        if !matches!(endpoint.computer_status.as_str(), "active" | "online")
            || !matches!(endpoint.worker_status.as_str(), "active" | "online")
        {
            return Err(ReleaseRolloutError::Refused(format!(
                "target {} is not active in both registries",
                endpoint.computer_name
            )));
        }
        selected.push(endpoint);
    }

    let selected_ids = selected
        .iter()
        .map(|target| target.computer_id)
        .collect::<BTreeSet<_>>();
    let mut extras = Vec::new();
    for target in by_id.into_values().filter(|target| {
        !selected_ids.contains(&target.computer_id)
            && matches!(target.computer_status.as_str(), "active" | "online")
            && matches!(target.worker_status.as_str(), "active" | "online")
    }) {
        validate_endpoint(&target)?;
        extras.push(RolloutRosterEntry {
            computer_name: target.computer_name,
            computer_id: target.computer_id,
        });
    }
    extras.sort_by(|a, b| {
        a.computer_name
            .cmp(&b.computer_name)
            .then(a.computer_id.cmp(&b.computer_id))
    });
    Ok((selected, extras))
}

fn validate_live_target_readiness(status: &RolloutStatus) -> Result<()> {
    for target in &status.targets {
        let endpoint = &target.target.endpoint;
        if !matches!(endpoint.computer_status.as_str(), "active" | "online")
            || !matches!(endpoint.worker_status.as_str(), "active" | "online")
        {
            return Err(ReleaseRolloutError::Refused(format!(
                "sealed target {} is not active in both live registries",
                endpoint.computer_name
            )));
        }
    }
    Ok(())
}

fn validate_status_invariants(targets: &[RolloutTargetState]) -> Result<()> {
    if targets.len() < ROLLOUT_CANARIES.len() || targets.len() > MAX_ROLLOUT_TARGETS {
        return Err(ReleaseRolloutError::Refused(
            "transaction target set cannot contain the complete bounded canary order".into(),
        ));
    }
    let mut ids = BTreeSet::new();
    let mut names = BTreeSet::new();
    for (ordinal, target) in targets.iter().enumerate() {
        if target.target.target_ordinal as usize != ordinal
            || forbidden_target(&target.target.endpoint)
            || !ids.insert(target.target.endpoint.computer_id)
            || !names.insert(target.target.endpoint.computer_name.as_str())
        {
            return Err(ReleaseRolloutError::Refused(
                "transaction target order or Vinny fence drifted".into(),
            ));
        }
    }
    if targets
        .iter()
        .take(ROLLOUT_CANARIES.len())
        .map(|target| target.target.endpoint.computer_name.as_str())
        .ne(ROLLOUT_CANARIES)
    {
        return Err(ReleaseRolloutError::Refused(
            "sealed transaction does not begin Beyoncé -> Lily -> Ace -> Logan".into(),
        ));
    }
    if targets[ROLLOUT_CANARIES.len()..]
        .windows(2)
        .any(|pair| pair[0].target.endpoint.computer_name >= pair[1].target.endpoint.computer_name)
    {
        return Err(ReleaseRolloutError::Refused(
            "sealed remaining target order is not strict lexical order".into(),
        ));
    }
    let active = targets
        .iter()
        .filter(|target| {
            matches!(
                target.state.as_str(),
                "installing" | "verifying" | "rolling_back"
            )
        })
        .count();
    if active > 1 {
        return Err(ReleaseRolloutError::Refused(
            "more than one rollout target is active".into(),
        ));
    }
    Ok(())
}

fn validate_post_success_rollback_preconditions(
    status: &RolloutStatus,
    config: &RolloutCoordinatorConfig,
) -> Result<()> {
    validate_source_commit(&status.source_commit)?;
    validate_status_invariants(&status.targets)?;
    if status.transaction.state != "succeeded"
        || status.transaction.expected_target_count as usize != status.targets.len()
    {
        return Err(ReleaseRolloutError::Refused(
            "post-success rollback requires one complete succeeded transaction".into(),
        ));
    }
    for target in &status.targets {
        if target.state != "succeeded" {
            return Err(ReleaseRolloutError::Refused(format!(
                "post-success rollback target {} is not succeeded",
                target.target.endpoint.computer_name
            )));
        }
        validate_target_material(&target.target, &status.source_commit)?;
        let evidence = parse_evidence(target)?;
        if evidence.phase != "succeeded" || evidence.error.is_some() || evidence.rollback.is_some()
        {
            return Err(ReleaseRolloutError::Refused(format!(
                "post-success rollback target {} has unsafe terminal evidence",
                target.target.endpoint.computer_name
            )));
        }
        let candidate = evidence.candidate.as_ref().ok_or_else(|| {
            ReleaseRolloutError::Refused(format!(
                "post-success rollback target {} lacks its durable candidate",
                target.target.endpoint.computer_name
            ))
        })?;
        validate_candidate(candidate, status.transaction.id, &target.target)?;
        let activation = evidence.activation.as_ref().ok_or_else(|| {
            ReleaseRolloutError::Refused(format!(
                "post-success rollback target {} lacks its activation receipt",
                target.target.endpoint.computer_name
            ))
        })?;
        validate_activation(
            activation,
            status.transaction.id,
            &target.target,
            &status.source_commit,
        )?;
        let rollback_proof = evidence.rollback_proof.as_ref().ok_or_else(|| {
            ReleaseRolloutError::Refused(format!(
                "post-success rollback target {} lacks its rollback proof",
                target.target.endpoint.computer_name
            ))
        })?;
        validate_rollback_proof(
            rollback_proof,
            status.transaction.id,
            &target.target,
            &status.source_commit,
        )?;
        let health = evidence.health.as_ref().ok_or_else(|| {
            ReleaseRolloutError::Refused(format!(
                "post-success rollback target {} lacks its health/bake evidence",
                target.target.endpoint.computer_name
            ))
        })?;
        let bake = if target.target.target_ordinal < ROLLOUT_CANARIES.len() as u32 {
            config.canary_bake
        } else {
            config.remaining_bake
        };
        validate_health(
            health,
            status.transaction.id,
            &target.target,
            &status.source_commit,
            bake,
        )?;
    }
    Ok(())
}

pub struct ReleaseRolloutCoordinator<'a, D, T> {
    database: &'a D,
    transport: &'a T,
    config: RolloutCoordinatorConfig,
}

impl<'a, D, T> ReleaseRolloutCoordinator<'a, D, T>
where
    D: ReleaseRolloutDatabase,
    T: ReleaseRolloutTransport,
{
    pub fn new(database: &'a D, transport: &'a T, config: RolloutCoordinatorConfig) -> Self {
        Self {
            database,
            transport,
            config,
        }
    }

    pub async fn plan(
        &self,
        source_commit: &str,
        requested_roster: &[RolloutRosterEntry],
    ) -> Result<RolloutPlanReceipt> {
        validate_source_commit(source_commit)?;
        let (ordered, excluded_active_extras) =
            resolve_requested_roster(requested_roster, self.database.planning_targets().await?)?;
        let materials = self.database.artifact_materials(source_commit).await?;
        let mut targets = Vec::with_capacity(ordered.len());
        for (ordinal, endpoint) in ordered.into_iter().enumerate() {
            let probe = self
                .transport
                .probe_platform(&endpoint, source_commit)
                .await?;
            let artifact_version = format!("recovery.{source_commit}.{}", probe.release_qualifier);
            let pair = materials
                .iter()
                .filter(|material| {
                    material.artifact.target_triple == probe.target_triple
                        && material.artifact.artifact_version == artifact_version
                })
                .cloned()
                .collect::<Vec<_>>();
            let target = RolloutTarget {
                target_ordinal: ordinal as u32,
                endpoint,
                target_triple: probe.target_triple,
                artifact_version,
                artifacts: pair,
            };
            validate_target_material(&target, source_commit)?;
            targets.push(target);
        }
        let spec = ReleaseRolloutAuthoritySpec {
            source_commit: source_commit.to_string(),
            created_by: LEASE_OWNER.to_string(),
            targets: targets
                .iter()
                .map(|target| RolloutTargetAuthority {
                    target_ordinal: target.target_ordinal,
                    computer_id: target.endpoint.computer_id,
                    computer_name: target.endpoint.computer_name.clone(),
                    target_triple: target.target_triple.clone(),
                    artifact_version: target.artifact_version.clone(),
                    artifacts: target
                        .artifacts
                        .iter()
                        .map(|material| RolloutArtifactAuthority {
                            artifact_name: material.artifact.artifact_name.clone(),
                            artifact_id: material.artifact.id,
                        })
                        .collect(),
                })
                .collect(),
        };
        let registered = self.database.register_authority(&spec).await?;
        Ok(RolloutPlanReceipt {
            authority_id: registered.authority.id,
            source_commit: source_commit.to_string(),
            outcome: match registered.outcome {
                ff_db::RolloutAuthorityRegistrationOutcome::Inserted => "inserted",
                ff_db::RolloutAuthorityRegistrationOutcome::ExactExisting => "exact_existing",
            }
            .to_string(),
            targets,
            excluded_active_extras,
        })
    }

    pub async fn start(&self, authority_id: Uuid, request_id: Uuid) -> Result<RolloutStatus> {
        let begun = self
            .database
            .begin(request_id, authority_id, self.config.lease_seconds)
            .await?;
        let owned_lease = matches!(begun.outcome, RolloutTransactionBeginOutcome::Inserted)
            .then_some(begun.transaction.lease_token);
        self.drive(begun.transaction.id, false, owned_lease)
            .await
            .map_err(|source| ReleaseRolloutError::AfterBegin {
                transaction_id: begun.transaction.id,
                request_id: begun.transaction.request_id,
                source: Box::new(source),
            })
    }

    async fn resolve_transaction_identity(
        &self,
        transaction_id: Option<Uuid>,
        request_id: Option<Uuid>,
    ) -> Result<Uuid> {
        match (transaction_id, request_id) {
            (Some(transaction_id), None) => Ok(transaction_id),
            (None, Some(request_id)) => self
                .database
                .transaction_by_request_id(request_id)
                .await?
                .map(|transaction| transaction.id)
                .ok_or_else(|| {
                    ReleaseRolloutError::Refused(format!("unknown rollout request {request_id}"))
                }),
            _ => Err(ReleaseRolloutError::Refused(
                "provide exactly one rollout transaction id or request id".into(),
            )),
        }
    }

    pub async fn status_for_identity(
        &self,
        transaction_id: Option<Uuid>,
        request_id: Option<Uuid>,
    ) -> Result<RolloutStatus> {
        let transaction_id = self
            .resolve_transaction_identity(transaction_id, request_id)
            .await?;
        self.status(transaction_id).await
    }

    pub async fn resume_for_identity(
        &self,
        transaction_id: Option<Uuid>,
        request_id: Option<Uuid>,
    ) -> Result<RolloutStatus> {
        let transaction_id = self
            .resolve_transaction_identity(transaction_id, request_id)
            .await?;
        self.resume(transaction_id).await
    }

    pub async fn status(&self, transaction_id: Uuid) -> Result<RolloutStatus> {
        self.database
            .status(transaction_id)
            .await?
            .ok_or_else(|| ReleaseRolloutError::Refused("unknown rollout transaction".into()))
    }

    pub async fn resume(&self, transaction_id: Uuid) -> Result<RolloutStatus> {
        self.drive(transaction_id, true, None).await
    }

    pub async fn rollback(&self, transaction_id: Uuid) -> Result<RolloutStatus> {
        let mut initial = self.status(transaction_id).await?;
        match initial.transaction.state.as_str() {
            "succeeded" => {
                // Completed target evidence is immutable until this exact
                // claim wins. Validate the entire rollback authority before
                // the single succeeded -> rolling_back database write.
                validate_live_target_readiness(&initial)?;
                validate_post_success_rollback_preconditions(&initial, &self.config)?;
                let Some(claimed) = self
                    .database
                    .claim_succeeded_rollback(&initial.transaction)
                    .await?
                else {
                    return Err(ReleaseRolloutError::Refused(
                        "post-success rollback was already claimed or its CAS fence drifted".into(),
                    ));
                };
                initial.transaction = claimed;
                return self.drive_rollback(initial).await;
            }
            "failed" | "rolled_back" | "cancelled" => {
                return Err(ReleaseRolloutError::Refused(format!(
                    "cannot request rollback from terminal state {}",
                    initial.transaction.state
                )));
            }
            _ => {}
        }

        let mut status = self.acquire_status(transaction_id, true, None).await?;
        match status.transaction.state.as_str() {
            "planned" => {
                self.cas_parent(&mut status, "planned", "cancelled").await?;
                self.status(transaction_id).await
            }
            "running" => {
                if let Some(active) = status
                    .targets
                    .iter()
                    .find(|target| matches!(target.state.as_str(), "installing" | "verifying"))
                    .cloned()
                {
                    let mut evidence = parse_evidence(&active)?;
                    evidence.phase = "operator_rollback".into();
                    evidence.error = Some("operator requested rollback".into());
                    let detail = serde_json::to_string(&evidence)?;
                    if !self
                        .database
                        .cas_target(
                            &status.transaction,
                            &active,
                            &active.state,
                            "failed",
                            Some(&detail),
                        )
                        .await?
                    {
                        return Err(ReleaseRolloutError::LeaseLost(
                            "operator rollback target fence".into(),
                        ));
                    }
                    status = self.status(transaction_id).await?;
                }
                self.cas_parent(&mut status, "running", "rolling_back")
                    .await?;
                self.drive_rollback(status).await
            }
            "rolling_back" => self.drive_rollback(status).await,
            state => Err(ReleaseRolloutError::Refused(format!(
                "cannot request rollback from state {state}"
            ))),
        }
    }

    async fn acquire_status(
        &self,
        transaction_id: Uuid,
        allow_takeover: bool,
        owned_lease: Option<Uuid>,
    ) -> Result<RolloutStatus> {
        let mut status = self.status(transaction_id).await?;
        if matches!(
            status.transaction.state.as_str(),
            "succeeded" | "failed" | "rolled_back" | "cancelled"
        ) {
            return Ok(status);
        }
        validate_live_target_readiness(&status)?;
        if owned_lease.is_some_and(|token| token == status.transaction.lease_token) {
            return Ok(status);
        }
        if status.transaction.lease_expires_at <= Utc::now() {
            if !allow_takeover {
                return Err(ReleaseRolloutError::LeaseLost("initial lease".into()));
            }
            let Some(taken) = self.database.take_over(&status.transaction).await? else {
                return Err(ReleaseRolloutError::LeaseLost("lease takeover CAS".into()));
            };
            status.transaction = taken;
            return Ok(status);
        }
        Err(ReleaseRolloutError::LeaseLost(
            "another foreground coordinator owns the live lease".into(),
        ))
    }

    async fn drive(
        &self,
        transaction_id: Uuid,
        allow_takeover: bool,
        owned_lease: Option<Uuid>,
    ) -> Result<RolloutStatus> {
        let mut status = self
            .acquire_status(transaction_id, allow_takeover, owned_lease)
            .await?;
        match status.transaction.state.as_str() {
            "planned" => {
                self.cas_parent(&mut status, "planned", "running").await?;
                self.drive_running(status).await
            }
            "running" => self.drive_running(status).await,
            "rolling_back" => self.drive_rollback(status).await,
            _ => Ok(status),
        }
    }

    async fn cas_parent(
        &self,
        status: &mut RolloutStatus,
        expected: &str,
        new_state: &str,
    ) -> Result<()> {
        let Some(updated) = self
            .database
            .cas_transaction(&status.transaction, expected, new_state)
            .await?
        else {
            return Err(ReleaseRolloutError::LeaseLost(format!(
                "transaction {expected}->{new_state} CAS"
            )));
        };
        status.transaction = updated;
        Ok(())
    }

    async fn refresh_and_renew(
        &self,
        transaction_id: Uuid,
        expected_state: &str,
    ) -> Result<RolloutStatus> {
        let mut status = self.status(transaction_id).await?;
        if status.transaction.state != expected_state {
            return Ok(status);
        }
        validate_live_target_readiness(&status)?;
        let lease_token = status.transaction.lease_token;
        let Some(renewed) = self.database.renew(&status.transaction).await? else {
            // A same-token parent transition may have won the revision CAS.
            // Adopt that authoritative state; every other renewal failure is
            // a lost fence and must stop before selecting or mutating a target.
            let latest = self.status(transaction_id).await?;
            if latest.transaction.lease_token == lease_token
                && latest.transaction.state != expected_state
            {
                return Ok(latest);
            }
            return Err(ReleaseRolloutError::LeaseLost(format!(
                "{expected_state} loop lease renewal"
            )));
        };
        status.transaction = renewed;
        Ok(status)
    }

    async fn drive_running(&self, mut status: RolloutStatus) -> Result<RolloutStatus> {
        loop {
            status = self
                .refresh_and_renew(status.transaction.id, "running")
                .await?;
            match status.transaction.state.as_str() {
                "running" => {}
                "rolling_back" => return self.drive_rollback(status).await,
                "succeeded" | "failed" | "rolled_back" | "cancelled" => return Ok(status),
                state => {
                    return Err(ReleaseRolloutError::Refused(format!(
                        "running rollout changed to unexpected parent state {state}"
                    )));
                }
            }
            validate_status_invariants(&status.targets)?;
            if status
                .targets
                .iter()
                .all(|target| target.state == "succeeded")
            {
                self.cas_parent(&mut status, "running", "succeeded").await?;
                return self.status(status.transaction.id).await;
            }
            if status.targets.iter().any(|target| target.state == "failed") {
                self.cas_parent(&mut status, "running", "rolling_back")
                    .await?;
                return self.drive_rollback(status).await;
            }
            let target = status
                .targets
                .iter()
                .find(|target| matches!(target.state.as_str(), "installing" | "verifying"))
                .or_else(|| {
                    status
                        .targets
                        .iter()
                        .find(|target| target.state == "pending")
                })
                .cloned()
                .ok_or_else(|| {
                    ReleaseRolloutError::Refused(
                        "running rollout has no deterministic next target".into(),
                    )
                })?;
            match target.state.as_str() {
                "pending" => {
                    let evidence = TargetEvidence {
                        phase: "bootstrap".into(),
                        ..Default::default()
                    };
                    let detail = serde_json::to_string(&evidence)?;
                    if !self
                        .database
                        .cas_target(
                            &status.transaction,
                            &target,
                            "pending",
                            "installing",
                            Some(&detail),
                        )
                        .await?
                    {
                        return Err(ReleaseRolloutError::LeaseLost("pending target CAS".into()));
                    }
                }
                "installing" => {
                    let transaction_id = status.transaction.id;
                    let source_commit = status.source_commit.clone();
                    let mut evidence = parse_evidence(&target)?;
                    let candidate = if let Some(candidate) = evidence.candidate.clone() {
                        validate_candidate(&candidate, transaction_id, &target.target)?;
                        candidate
                    } else {
                        let operation = self
                            .transport
                            .bootstrap_candidate(transaction_id, &target.target);
                        let candidate = match self
                            .lease_fenced(
                                &mut status.transaction,
                                "exact candidate bootstrap",
                                operation,
                            )
                            .await
                        {
                            Ok(candidate) => candidate,
                            Err(error @ ReleaseRolloutError::LeaseLost(_)) => return Err(error),
                            Err(error) => {
                                return self
                                    .fail_and_rollback(status, target, error.to_string())
                                    .await;
                            }
                        };
                        validate_candidate(&candidate, transaction_id, &target.target)?;
                        evidence.phase = "activation".into();
                        evidence.candidate = Some(candidate);
                        let detail = serde_json::to_string(&evidence)?;
                        if !self
                            .database
                            .cas_target(
                                &status.transaction,
                                &target,
                                "installing",
                                "installing",
                                Some(&detail),
                            )
                            .await?
                        {
                            return Err(ReleaseRolloutError::LeaseLost(
                                "candidate receipt CAS".into(),
                            ));
                        }
                        continue;
                    };
                    let operation = async {
                        let activation = self
                            .transport
                            .activate(&candidate, &target.target, &source_commit)
                            .await?;
                        validate_activation(
                            &activation,
                            transaction_id,
                            &target.target,
                            &source_commit,
                        )?;
                        Ok::<_, ReleaseRolloutError>(activation)
                    };
                    match self
                        .lease_fenced(&mut status.transaction, "target activation", operation)
                        .await
                    {
                        Ok(activation) => {
                            evidence.phase = "rollback_proof".into();
                            evidence.activation = Some(activation);
                            let detail = serde_json::to_string(&evidence)?;
                            if !self
                                .database
                                .cas_target(
                                    &status.transaction,
                                    &target,
                                    "installing",
                                    "verifying",
                                    Some(&detail),
                                )
                                .await?
                            {
                                return Err(ReleaseRolloutError::LeaseLost(
                                    "activation receipt CAS".into(),
                                ));
                            }
                        }
                        Err(error @ ReleaseRolloutError::LeaseLost(_)) => return Err(error),
                        // A lost SSH response is indistinguishable from a
                        // committed local activation. Keep the durably stored
                        // exact candidate and installing state; after lease
                        // expiry, resume replays the same transaction UUID and
                        // the local activation code adopts its receipt.
                        Err(error) => return Err(error),
                    }
                }
                "verifying" => {
                    let mut evidence = parse_evidence(&target)?;
                    let activation = evidence.activation.clone().ok_or_else(|| {
                        ReleaseRolloutError::Refused(
                            "verifying target lacks durable activation receipt".into(),
                        )
                    })?;
                    validate_activation(
                        &activation,
                        status.transaction.id,
                        &target.target,
                        &status.source_commit,
                    )?;
                    let candidate = evidence.candidate.clone().ok_or_else(|| {
                        ReleaseRolloutError::Refused(
                            "verifying target lacks durable exact candidate receipt".into(),
                        )
                    })?;
                    validate_candidate(&candidate, status.transaction.id, &target.target)?;
                    let bake = if (target.target.target_ordinal as usize) < ROLLOUT_CANARIES.len() {
                        self.config.canary_bake
                    } else {
                        self.config.remaining_bake
                    };
                    let transaction_id = status.transaction.id;
                    let source_commit = status.source_commit.clone();
                    let operation = async {
                        let proof = self
                            .transport
                            .prove_rollback(&candidate, &target.target)
                            .await?;
                        validate_rollback_proof(
                            &proof,
                            transaction_id,
                            &target.target,
                            &source_commit,
                        )?;
                        let health = self
                            .transport
                            .health_and_bake(&candidate, &target.target, &source_commit, bake)
                            .await?;
                        validate_health(
                            &health,
                            transaction_id,
                            &target.target,
                            &source_commit,
                            bake,
                        )?;
                        Ok::<_, ReleaseRolloutError>((proof, health))
                    };
                    match self
                        .lease_fenced(
                            &mut status.transaction,
                            "rollback proof and bake",
                            operation,
                        )
                        .await
                    {
                        Ok((proof, health)) => {
                            evidence.phase = "succeeded".into();
                            evidence.rollback_proof = Some(proof);
                            evidence.health = Some(health);
                            let detail = serde_json::to_string(&evidence)?;
                            if !self
                                .database
                                .cas_target(
                                    &status.transaction,
                                    &target,
                                    "verifying",
                                    "succeeded",
                                    Some(&detail),
                                )
                                .await?
                            {
                                return Err(ReleaseRolloutError::LeaseLost(
                                    "health evidence CAS".into(),
                                ));
                            }
                        }
                        Err(error @ ReleaseRolloutError::LeaseLost(_)) => return Err(error),
                        Err(error) => {
                            return self
                                .fail_and_rollback(status, target, error.to_string())
                                .await;
                        }
                    }
                }
                _ => unreachable!("filtered target state"),
            }
        }
    }

    async fn fail_and_rollback(
        &self,
        mut status: RolloutStatus,
        target: RolloutTargetState,
        error: String,
    ) -> Result<RolloutStatus> {
        let mut evidence = parse_evidence(&target)?;
        evidence.phase = "failed".into();
        evidence.error = Some(error);
        let detail = serde_json::to_string(&evidence)?;
        if !self
            .database
            .cas_target(
                &status.transaction,
                &target,
                &target.state,
                "failed",
                Some(&detail),
            )
            .await?
        {
            return Err(ReleaseRolloutError::LeaseLost("failure target CAS".into()));
        }
        status = self.status(status.transaction.id).await?;
        self.cas_parent(&mut status, "running", "rolling_back")
            .await?;
        self.drive_rollback(status).await
    }

    async fn drive_rollback(&self, mut status: RolloutStatus) -> Result<RolloutStatus> {
        loop {
            status = self
                .refresh_and_renew(status.transaction.id, "rolling_back")
                .await?;
            if status.transaction.state != "rolling_back" {
                return Ok(status);
            }
            validate_status_invariants(&status.targets)?;
            let candidate = status
                .targets
                .iter()
                .rev()
                .find(|target| {
                    if target.state == "rolling_back" {
                        return true;
                    }
                    if matches!(target.state.as_str(), "succeeded" | "verifying" | "failed") {
                        return parse_evidence(target)
                            .ok()
                            .is_some_and(|evidence| evidence.activation.is_some());
                    }
                    false
                })
                .cloned();
            if let Some(target) = candidate {
                let mut evidence = parse_evidence(&target)?;
                if target.state != "rolling_back" {
                    evidence.phase = "rolling_back".into();
                    let detail = serde_json::to_string(&evidence)?;
                    if !self
                        .database
                        .cas_target(
                            &status.transaction,
                            &target,
                            &target.state,
                            "rolling_back",
                            Some(&detail),
                        )
                        .await?
                    {
                        return Err(ReleaseRolloutError::LeaseLost("rollback target CAS".into()));
                    }
                    continue;
                }
                let candidate = evidence.candidate.clone().ok_or_else(|| {
                    ReleaseRolloutError::RollbackIncomplete(format!(
                        "target {} lacks the durable candidate used for activation",
                        target.target.endpoint.computer_name
                    ))
                })?;
                validate_candidate(&candidate, status.transaction.id, &target.target)?;
                let operation = self.transport.rollback(&candidate, &target.target);
                match self
                    .lease_fenced(&mut status.transaction, "target rollback", operation)
                    .await
                {
                    Ok(receipt) => {
                        validate_rollback_receipt(&receipt, status.transaction.id, &target.target)?;
                        evidence.phase = "rolled_back".into();
                        evidence.rollback = Some(receipt);
                        let detail = serde_json::to_string(&evidence)?;
                        if !self
                            .database
                            .cas_target(
                                &status.transaction,
                                &target,
                                "rolling_back",
                                "rolled_back",
                                Some(&detail),
                            )
                            .await?
                        {
                            return Err(ReleaseRolloutError::LeaseLost(
                                "rollback receipt CAS".into(),
                            ));
                        }
                    }
                    Err(error @ ReleaseRolloutError::LeaseLost(_)) => return Err(error),
                    Err(error) => {
                        // Keep both states resumable. A later foreground resume
                        // adopts an already-committed local rollback receipt or
                        // retries the same exact transaction; it never skips it.
                        return Err(ReleaseRolloutError::RollbackIncomplete(error.to_string()));
                    }
                }
                continue;
            }
            if let Some(pending) = status
                .targets
                .iter()
                .find(|target| target.state == "pending")
                .cloned()
            {
                let evidence = TargetEvidence {
                    phase: "skipped_by_rollback".into(),
                    ..Default::default()
                };
                let detail = serde_json::to_string(&evidence)?;
                if !self
                    .database
                    .cas_target(
                        &status.transaction,
                        &pending,
                        "pending",
                        "skipped",
                        Some(&detail),
                    )
                    .await?
                {
                    return Err(ReleaseRolloutError::LeaseLost("skip pending CAS".into()));
                }
                continue;
            }
            if status.targets.iter().any(|target| {
                matches!(
                    target.state.as_str(),
                    "installing" | "verifying" | "rolling_back"
                )
            }) {
                return Err(ReleaseRolloutError::RollbackIncomplete(
                    "an in-flight target lacks adoptable committed activation evidence".into(),
                ));
            }
            self.cas_parent(&mut status, "rolling_back", "rolled_back")
                .await?;
            return self.status(status.transaction.id).await;
        }
    }

    async fn lease_fenced<F, O>(
        &self,
        transaction: &mut ReleaseRolloutTransactionRow,
        operation_name: &str,
        operation: F,
    ) -> Result<O>
    where
        F: std::future::Future<Output = Result<O>>,
    {
        tokio::pin!(operation);
        let mut ticker = tokio::time::interval(self.config.lease_renew_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            tokio::select! {
                biased;
                result = &mut operation => return result,
                _ = ticker.tick() => {
                    let Some(renewed) = self.database.renew(transaction).await? else {
                        return Err(ReleaseRolloutError::LeaseLost(operation_name.to_string()));
                    };
                    *transaction = renewed;
                }
            }
        }
    }
}

fn parse_evidence(target: &RolloutTargetState) -> Result<TargetEvidence> {
    match target.detail.as_deref() {
        Some(detail) => serde_json::from_str(detail).map_err(|error| {
            ReleaseRolloutError::Refused(format!(
                "target {} evidence is malformed: {error}",
                target.target.endpoint.computer_name
            ))
        }),
        None if target.state == "pending" => Ok(TargetEvidence::default()),
        None => Err(ReleaseRolloutError::Refused(format!(
            "target {} state {} lacks durable evidence",
            target.target.endpoint.computer_name, target.state
        ))),
    }
}

fn ff_material(target: &RolloutTarget) -> Result<&RolloutArtifactMaterial> {
    target
        .artifacts
        .iter()
        .find(|material| material.artifact.artifact_name == "ff")
        .ok_or_else(|| ReleaseRolloutError::Refused("target has no exact ff artifact".into()))
}

fn fixed_candidate_path(transaction_id: Uuid, target: &RolloutTarget) -> String {
    let home_root = if target.target_triple.ends_with("-apple-darwin") {
        "/Users"
    } else {
        "/home"
    };
    format!(
        "{home_root}/{}/.forgefleet/release-rollout/candidates/{transaction_id}/ff",
        target.endpoint.ssh_user
    )
}

fn candidate_from_target(transaction_id: Uuid, target: &RolloutTarget) -> Result<CandidateReceipt> {
    let material = ff_material(target)?;
    Ok(CandidateReceipt {
        transaction_id,
        computer_id: target.endpoint.computer_id,
        computer_name: target.endpoint.computer_name.clone(),
        absolute_path: fixed_candidate_path(transaction_id, target),
        sha256: material.artifact.sha256.clone(),
        size_bytes: material.artifact.size_bytes,
        platform: if target.target_triple.ends_with("-apple-darwin") {
            "macos"
        } else {
            "linux"
        }
        .into(),
    })
}

fn validate_candidate(
    candidate: &CandidateReceipt,
    transaction_id: Uuid,
    target: &RolloutTarget,
) -> Result<()> {
    let expected = candidate_from_target(transaction_id, target)?;
    if candidate.transaction_id != expected.transaction_id
        || candidate.computer_id != expected.computer_id
        || candidate.computer_name != expected.computer_name
        || candidate.sha256 != expected.sha256
        || candidate.size_bytes != expected.size_bytes
        || candidate.platform != expected.platform
        || candidate.absolute_path != expected.absolute_path
        || !candidate
            .absolute_path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-'))
    {
        return Err(ReleaseRolloutError::Refused(
            "candidate receipt differs from fixed exact authority".into(),
        ));
    }
    Ok(())
}

fn validate_activation(
    receipt: &ReleaseActivationReceipt,
    transaction_id: Uuid,
    target: &RolloutTarget,
    source_commit: &str,
) -> Result<()> {
    if receipt.transaction_id != transaction_id
        || receipt.source_commit != source_commit
        || receipt.computer_id != target.endpoint.computer_id
        || receipt.computer_name != target.endpoint.computer_name
        || receipt.target_triple != target.target_triple
        || receipt.artifact_version != target.artifact_version
    {
        return Err(ReleaseRolloutError::Refused(
            "activation receipt identity drifted".into(),
        ));
    }
    for material in &target.artifacts {
        let matches = receipt.artifacts.iter().filter(|artifact| {
            artifact.artifact_name == material.artifact.artifact_name
                && artifact.sha256 == material.artifact.sha256
                && artifact.size_bytes == material.artifact.size_bytes
        });
        if matches.count() != 1 {
            return Err(ReleaseRolloutError::Refused(
                "activation receipt artifact identity drifted".into(),
            ));
        }
    }
    Ok(())
}

fn validate_rollback_proof(
    proof: &ReleaseRollbackProof,
    transaction_id: Uuid,
    target: &RolloutTarget,
    source_commit: &str,
) -> Result<()> {
    if proof.transaction_id != transaction_id
        || proof.source_commit != source_commit
        || proof.computer_id != target.endpoint.computer_id
        || proof.computer_name != target.endpoint.computer_name
        || proof.manifest_sha256.len() != 64
        || proof.activation_receipt_sha256.len() != 64
    {
        return Err(ReleaseRolloutError::Refused(
            "rollback proof identity drifted".into(),
        ));
    }
    Ok(())
}

fn validate_health(
    health: &HealthBakeEvidence,
    transaction_id: Uuid,
    target: &RolloutTarget,
    source_commit: &str,
    bake: Duration,
) -> Result<()> {
    if health.transaction_id != transaction_id
        || health.source_commit != source_commit
        || health.computer_id != target.endpoint.computer_id
        || health.computer_name != target.endpoint.computer_name
        || health.bake_seconds != bake.as_secs()
    {
        return Err(ReleaseRolloutError::Refused(
            "health/bake evidence identity drifted".into(),
        ));
    }
    Ok(())
}

fn validate_rollback_receipt(
    receipt: &ReleaseRollbackReceipt,
    transaction_id: Uuid,
    target: &RolloutTarget,
) -> Result<()> {
    if receipt.transaction_id != transaction_id
        || receipt.computer_id != target.endpoint.computer_id
        || receipt.computer_name != target.endpoint.computer_name
    {
        return Err(ReleaseRolloutError::Refused(
            "rollback receipt identity drifted".into(),
        ));
    }
    Ok(())
}

pub struct SystemReleaseRolloutTransport;

#[async_trait]
impl ReleaseRolloutTransport for SystemReleaseRolloutTransport {
    async fn probe_platform(
        &self,
        target: &RolloutEndpoint,
        source_commit: &str,
    ) -> Result<PlatformProbe> {
        validate_endpoint(target)?;
        validate_source_commit(source_commit)?;
        const SCRIPT: &str = r#"set -eu
os=$(/usr/bin/uname -s)
arch=$(/usr/bin/uname -m)
if [ "$os" = Linux ]; then
  release=$(/usr/bin/awk -F= '$1=="VERSION_ID" {gsub(/\"/,"",$2); print $2}' /usr/lib/os-release)
  release=${release%%.*}
elif [ "$os" = Darwin ]; then
  release=$(/usr/bin/sw_vers -productVersion); release=${release%%.*}
else
  exit 64
fi
/usr/bin/printf '%s|%s|%s\n' "$os" "$arch" "$release"
"#;
        let output = ssh_script(target, SCRIPT, &[], None).await?;
        let fields = output.trim().split('|').collect::<Vec<_>>();
        let probe = match fields.as_slice() {
            ["Linux", "x86_64", "24"] => PlatformProbe {
                target_triple: "x86_64-unknown-linux-gnu".into(),
                release_qualifier: "ubuntu24-x86_64".into(),
                platform: "linux".into(),
            },
            ["Linux", "aarch64", "24"] => PlatformProbe {
                target_triple: "aarch64-unknown-linux-gnu".into(),
                release_qualifier: "ubuntu24-aarch64".into(),
                platform: "linux".into(),
            },
            ["Linux", "x86_64", "26"] => PlatformProbe {
                target_triple: "x86_64-unknown-linux-gnu".into(),
                release_qualifier: "ubuntu26-x86_64".into(),
                platform: "linux".into(),
            },
            ["Darwin", "arm64", "26"] => PlatformProbe {
                target_triple: "aarch64-apple-darwin".into(),
                release_qualifier: "macos26-arm64".into(),
                platform: "macos".into(),
            },
            _ => {
                return Err(ReleaseRolloutError::Transport(format!(
                    "unsupported platform probe from {}: {output:?}",
                    target.computer_name
                )));
            }
        };
        Ok(probe)
    }

    async fn bootstrap_candidate(
        &self,
        transaction_id: Uuid,
        target: &RolloutTarget,
    ) -> Result<CandidateReceipt> {
        validate_target_material(target, &ff_material(target)?.artifact.source_commit)?;
        let material = ff_material(target)?.clone();
        let cache_file = acquire_to_coordinator_cache(&material).await?;
        let platform = if target.target_triple.ends_with("-apple-darwin") {
            "macos"
        } else {
            "linux"
        };
        const SCRIPT: &str = r#"set -eu
umask 077
tx=$1; expected_sha=$2; expected_size=$3; platform=$4
base="$HOME/.forgefleet/release-rollout/candidates/$tx"
mkdir -p "$HOME/.forgefleet/release-rollout/candidates"
chmod 700 "$HOME/.forgefleet" "$HOME/.forgefleet/release-rollout" "$HOME/.forgefleet/release-rollout/candidates" 2>/dev/null || true
# Concurrent lease handoff can only publish the same registered bytes.  The
# PID-scoped temp plus verified atomic rename is crash-safe without a stale
# directory lock, and RUN_CANDIDATE_SCRIPT verifies the final file again.
trap 'rm -f "$base/.ff.incoming.$$"' EXIT HUP INT TERM
mkdir -p "$base"; chmod 700 "$base"
tmp="$base/.ff.incoming.$$"
/bin/cat > "$tmp"; chmod 500 "$tmp"
actual_size=$(/usr/bin/wc -c < "$tmp" | /usr/bin/tr -d ' ')
if command -v sha256sum >/dev/null 2>&1; then actual_sha=$(sha256sum "$tmp" | /usr/bin/awk '{print $1}'); else actual_sha=$(shasum -a 256 "$tmp" | /usr/bin/awk '{print $1}'); fi
[ "$actual_size" = "$expected_size" ] && [ "$actual_sha" = "$expected_sha" ]
if [ "$platform" = macos ]; then /usr/bin/codesign --verify --strict "$tmp"; fi
/bin/mv -f "$tmp" "$base/ff"; chmod 500 "$base/ff"
actual_size=$(/usr/bin/wc -c < "$base/ff" | /usr/bin/tr -d ' ')
if command -v sha256sum >/dev/null 2>&1; then actual_sha=$(sha256sum "$base/ff" | /usr/bin/awk '{print $1}'); else actual_sha=$(shasum -a 256 "$base/ff" | /usr/bin/awk '{print $1}'); fi
[ "$actual_size" = "$expected_size" ] && [ "$actual_sha" = "$expected_sha" ]
/usr/bin/printf '%s\n' "$base/ff"
"#;
        let file = cache_file.reopen()?;
        let output = ssh_script(
            &target.endpoint,
            SCRIPT,
            &[
                transaction_id.to_string(),
                material.artifact.sha256.clone(),
                material.artifact.size_bytes.to_string(),
                platform.to_string(),
            ],
            Some(file),
        )
        .await?;
        let absolute = output.trim();
        let expected_suffix =
            format!("/.forgefleet/release-rollout/candidates/{transaction_id}/ff");
        if !absolute.starts_with('/')
            || !absolute.ends_with(&expected_suffix)
            || !absolute
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-'))
        {
            return Err(ReleaseRolloutError::Transport(
                "target returned a non-fixed candidate path".into(),
            ));
        }
        let mut receipt = candidate_from_target(transaction_id, target)?;
        receipt.absolute_path = absolute.to_string();
        Ok(receipt)
    }

    async fn activate(
        &self,
        candidate: &CandidateReceipt,
        target: &RolloutTarget,
        source_commit: &str,
    ) -> Result<ReleaseActivationReceipt> {
        let output = run_candidate(
            candidate,
            target,
            &[
                "artifact".into(),
                "activate".into(),
                "--source-commit".into(),
                source_commit.into(),
                "--transaction-id".into(),
                candidate.transaction_id.to_string(),
                "--json".into(),
            ],
        )
        .await?;
        Ok(serde_json::from_str(output.trim())?)
    }

    async fn prove_rollback(
        &self,
        candidate: &CandidateReceipt,
        target: &RolloutTarget,
    ) -> Result<ReleaseRollbackProof> {
        let output = run_candidate(
            candidate,
            target,
            &[
                "artifact".into(),
                "rollback-proof".into(),
                "--transaction-id".into(),
                candidate.transaction_id.to_string(),
                "--json".into(),
            ],
        )
        .await?;
        Ok(serde_json::from_str(output.trim())?)
    }

    async fn health_and_bake(
        &self,
        candidate: &CandidateReceipt,
        target: &RolloutTarget,
        source_commit: &str,
        bake: Duration,
    ) -> Result<HealthBakeEvidence> {
        tokio::time::sleep(bake).await;
        let receipt = self.activate(candidate, target, source_commit).await?;
        validate_activation(&receipt, candidate.transaction_id, target, source_commit)?;
        Ok(HealthBakeEvidence {
            transaction_id: candidate.transaction_id,
            computer_id: target.endpoint.computer_id,
            computer_name: target.endpoint.computer_name.clone(),
            source_commit: source_commit.to_string(),
            bake_seconds: bake.as_secs(),
            verified_at: Utc::now(),
        })
    }

    async fn rollback(
        &self,
        candidate: &CandidateReceipt,
        target: &RolloutTarget,
    ) -> Result<ReleaseRollbackReceipt> {
        let output = run_candidate(
            candidate,
            target,
            &[
                "artifact".into(),
                "rollback".into(),
                "--transaction-id".into(),
                candidate.transaction_id.to_string(),
                "--json".into(),
            ],
        )
        .await?;
        Ok(serde_json::from_str(output.trim())?)
    }
}

async fn acquire_to_coordinator_cache(
    material: &RolloutArtifactMaterial,
) -> Result<tempfile::NamedTempFile> {
    validate_artifact(material)?;
    let home = dirs::home_dir()
        .ok_or_else(|| ReleaseRolloutError::Refused("coordinator home unavailable".into()))?;
    let cache = home
        .join(".forgefleet")
        .join("release-rollout")
        .join("coordinator-cache");
    fs::create_dir_all(&cache)?;
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o700))?;
    let temp = tempfile::Builder::new()
        .prefix(".ff-candidate-")
        .tempfile_in(&cache)?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    let remote_path = format!(
        "~/.forgefleet/release-builds/{}",
        material.origin.relative_path
    );
    let destination = ssh_destination(&material.origin.endpoint)?;
    let mut command = tokio::process::Command::new("ssh");
    command
        .kill_on_drop(true)
        .args(crate::ssh_opts::ssh_bypass_args())
        .args([
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=15",
            "-o",
            "ConnectionAttempts=1",
            "-p",
            &material.origin.endpoint.ssh_port.to_string(),
            "--",
            &destination,
            "cat",
            "--",
            &remote_path,
        ])
        .stdout(Stdio::from(temp.reopen()?))
        .stderr(Stdio::piped());
    let output = wait_with_preserved_stdio(&mut command).await?;
    if !output.status.success() {
        return Err(ReleaseRolloutError::Transport(format!(
            "candidate custody transfer failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mut file = temp.reopen()?;
    let initial = file.metadata()?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = format!("{:x}", hasher.finalize());
    let final_meta = file.metadata()?;
    if initial.len() != material.artifact.size_bytes as u64
        || final_meta.len() != initial.len()
        || digest != material.artifact.sha256
    {
        return Err(ReleaseRolloutError::Transport(
            "candidate custody bytes failed exact size/SHA verification".into(),
        ));
    }
    Ok(temp)
}

async fn wait_with_preserved_stdio(
    command: &mut tokio::process::Command,
) -> std::io::Result<std::process::Output> {
    command.spawn()?.wait_with_output().await
}

fn ssh_destination(endpoint: &RolloutEndpoint) -> Result<String> {
    validate_endpoint(endpoint)?;
    Ok(format!("{}@{}", endpoint.ssh_user, endpoint.ip))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

async fn ssh_script(
    endpoint: &RolloutEndpoint,
    script: &str,
    args: &[String],
    stdin_file: Option<File>,
) -> Result<String> {
    validate_endpoint(endpoint)?;
    let destination = ssh_destination(endpoint)?;
    let mut remote = format!("/bin/sh -ceu {} --", shell_quote(script));
    for arg in args {
        remote.push(' ');
        remote.push_str(&shell_quote(arg));
    }
    let mut command = tokio::process::Command::new("ssh");
    command
        .kill_on_drop(true)
        .args(crate::ssh_opts::ssh_bypass_args())
        .args([
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=15",
            "-o",
            "ConnectionAttempts=1",
            "-p",
            &endpoint.ssh_port.to_string(),
            "--",
            &destination,
            &remote,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(file) = stdin_file {
        command.stdin(Stdio::from(file));
    } else {
        command.stdin(Stdio::null());
    }
    let output = command.output().await?;
    if !output.status.success() {
        return Err(ReleaseRolloutError::Transport(format!(
            "fixed SSH operation on {} failed: {}",
            endpoint.computer_name,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| ReleaseRolloutError::Transport("SSH output was not UTF-8".into()))
}

async fn run_candidate(
    candidate: &CandidateReceipt,
    target: &RolloutTarget,
    args: &[String],
) -> Result<String> {
    validate_candidate(candidate, candidate.transaction_id, target)?;
    if args.is_empty()
        || args.iter().any(|arg| {
            arg.is_empty()
                || !arg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'))
        })
    {
        return Err(ReleaseRolloutError::Refused(
            "candidate command arguments are not from the fixed safe vocabulary".into(),
        ));
    }
    let material = ff_material(target)?;
    let mut script_args = vec![
        candidate.transaction_id.to_string(),
        material.artifact.sha256.clone(),
        material.artifact.size_bytes.to_string(),
        candidate.platform.clone(),
        candidate.absolute_path.clone(),
    ];
    script_args.extend_from_slice(args);
    ssh_script(&target.endpoint, RUN_CANDIDATE_SCRIPT, &script_args, None).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::{Arc, Mutex};

    const SOURCE: &str = "39b017341b7536df64b61f42672ab33fb62343f8";

    fn endpoint(name: &str, ordinal: u128) -> RolloutEndpoint {
        RolloutEndpoint {
            computer_id: Uuid::from_u128(ordinal + 1),
            computer_name: name.into(),
            ssh_user: name.into(),
            ip: format!("192.0.2.{}", ordinal + 1),
            ssh_port: 22,
            computer_status: "online".into(),
            worker_status: "active".into(),
        }
    }

    fn roster_entry(endpoint: &RolloutEndpoint) -> RolloutRosterEntry {
        RolloutRosterEntry {
            computer_name: endpoint.computer_name.clone(),
            computer_id: endpoint.computer_id,
        }
    }

    #[test]
    fn known_seventeen_operator_roster_resolves_in_exact_order_and_reports_extras() {
        let names = [
            "beyonce", "lily", "ace", "logan", "adele", "aura", "duncan", "james", "marcus",
            "priya", "rihanna", "sarah", "shakira", "sia", "sophie", "thalia", "veronica",
        ];
        let ordered = names
            .iter()
            .enumerate()
            .map(|(ordinal, name)| endpoint(name, ordinal as u128))
            .collect::<Vec<_>>();
        let requested = ordered.iter().map(roster_entry).collect::<Vec<_>>();
        let mut registry = ordered.clone();
        registry.reverse();
        registry.push(endpoint("zeta", 30));
        let mut vinny = endpoint("vinny", 50);
        vinny.computer_id = FORBIDDEN_VINNY_ID;
        registry.push(vinny);

        let (selected, extras) = resolve_requested_roster(&requested, registry).unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|target| target.computer_name.as_str())
                .collect::<Vec<_>>(),
            names
        );
        assert_eq!(extras, vec![roster_entry(&endpoint("zeta", 30))]);
    }

    #[test]
    fn operator_roster_refuses_missing_drift_duplicates_canary_and_order_failures() {
        let registry = ["beyonce", "lily", "ace", "logan", "zeta"]
            .iter()
            .enumerate()
            .map(|(ordinal, name)| endpoint(name, ordinal as u128))
            .collect::<Vec<_>>();
        let exact = registry[..4].iter().map(roster_entry).collect::<Vec<_>>();

        let mut missing = exact.clone();
        missing[3].computer_id = Uuid::new_v4();
        assert!(resolve_requested_roster(&missing, registry.clone()).is_err());

        let mut drift = exact.clone();
        drift[3].computer_id = registry[4].computer_id;
        assert!(resolve_requested_roster(&drift, registry.clone()).is_err());

        let mut duplicate = exact.clone();
        duplicate.push(exact[3].clone());
        assert!(resolve_requested_roster(&duplicate, registry.clone()).is_err());

        let mut missing_canary = exact.clone();
        missing_canary.swap(0, 1);
        assert!(resolve_requested_roster(&missing_canary, registry.clone()).is_err());

        let mut inactive_registry = registry.clone();
        inactive_registry[3].worker_status = "offline".into();
        assert!(resolve_requested_roster(&exact, inactive_registry).is_err());

        let mut wrong_tail = exact;
        wrong_tail.extend([
            roster_entry(&endpoint("zeta", 4)),
            roster_entry(&endpoint("alpha", 5)),
        ]);
        let mut expanded_registry = registry;
        expanded_registry.push(endpoint("alpha", 5));
        assert!(resolve_requested_roster(&wrong_tail, expanded_registry).is_err());
    }

    #[test]
    fn roster_parser_requires_canonical_name_uuid_pair_and_forbids_vinny() {
        let id = Uuid::new_v4();
        assert_eq!(
            format!("beyonce={id}")
                .parse::<RolloutRosterEntry>()
                .unwrap(),
            RolloutRosterEntry {
                computer_name: "beyonce".into(),
                computer_id: id,
            }
        );
        assert!("beyonce=NOT-A-UUID".parse::<RolloutRosterEntry>().is_err());
        assert!(
            format!("Beyonce={id}")
                .parse::<RolloutRosterEntry>()
                .is_err()
        );
        assert!(format!("vinny={id}").parse::<RolloutRosterEntry>().is_err());
        assert!(
            format!("renamed-vinny={FORBIDDEN_VINNY_ID}")
                .parse::<RolloutRosterEntry>()
                .is_err()
        );
    }

    #[test]
    fn no_force_or_arbitrary_host_path_command_exists_in_public_requests() {
        let target = test_target("beyonce", 0);
        let candidate = fixed_candidate_path(Uuid::nil(), &target);
        assert_eq!(
            candidate,
            "/home/beyonce/.forgefleet/release-rollout/candidates/00000000-0000-0000-0000-000000000000/ff"
        );
        assert!(validate_relative_path("../../tmp/ff").is_err());
        let mut target = endpoint("beyonce", 0);
        target.ip = "host; touch /tmp/pwn".into();
        assert!(validate_endpoint(&target).is_err());
    }

    #[test]
    fn more_than_one_active_target_is_rejected() {
        let targets = ["beyonce", "lily"]
            .iter()
            .enumerate()
            .map(|(ordinal, name)| RolloutTargetState {
                target: RolloutTarget {
                    target_ordinal: ordinal as u32,
                    endpoint: endpoint(name, ordinal as u128),
                    target_triple: "x86_64-unknown-linux-gnu".into(),
                    artifact_version: "v".into(),
                    artifacts: vec![],
                },
                state: "installing".into(),
                cas_revision: 0,
                detail: Some("{}".into()),
            })
            .collect::<Vec<_>>();
        assert!(validate_status_invariants(&targets).is_err());
    }

    #[test]
    fn candidate_and_receipts_reject_tampered_identity() {
        let target = test_target("beyonce", 0);
        let mut candidate = candidate_from_target(Uuid::nil(), &target).unwrap();
        candidate.sha256 = "0".repeat(64);
        assert!(validate_candidate(&candidate, Uuid::nil(), &target).is_err());
        candidate = candidate_from_target(Uuid::nil(), &target).unwrap();
        candidate.absolute_path = format!(
            "/tmp/other{}",
            candidate
                .absolute_path
                .strip_prefix("/home/beyonce")
                .unwrap()
        );
        assert!(validate_candidate(&candidate, Uuid::nil(), &target).is_err());
        let mut activation = activation_receipt(Uuid::nil(), &target);
        activation.computer_name = "vinny".into();
        assert!(validate_activation(&activation, Uuid::nil(), &target, SOURCE).is_err());
    }

    fn material(name: &str, target: &str, qualifier: &str, byte: char) -> RolloutArtifactMaterial {
        RolloutArtifactMaterial {
            artifact: ReleaseArtifactRow {
                id: Uuid::new_v4(),
                artifact_name: name.into(),
                artifact_version: format!("recovery.{SOURCE}.{qualifier}"),
                source_commit: SOURCE.into(),
                target_triple: target.into(),
                sha256: byte.to_string().repeat(64),
                size_bytes: if name == "ff" { 10 } else { 20 },
                created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            },
            origin: ArtifactCustodySource {
                endpoint: endpoint("origin", 90),
                relative_path: format!("build/{name}"),
                first_verified_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            },
        }
    }

    fn test_target(name: &str, ordinal: u32) -> RolloutTarget {
        let triple = "x86_64-unknown-linux-gnu";
        let qualifier = "ubuntu24-x86_64";
        RolloutTarget {
            target_ordinal: ordinal,
            endpoint: endpoint(name, ordinal as u128),
            target_triple: triple.into(),
            artifact_version: format!("recovery.{SOURCE}.{qualifier}"),
            artifacts: vec![
                material("ff", triple, qualifier, '1'),
                material("forgefleetd", triple, qualifier, '2'),
            ],
        }
    }

    fn activation_receipt(id: Uuid, target: &RolloutTarget) -> ReleaseActivationReceipt {
        ReleaseActivationReceipt {
            transaction_id: id,
            artifact_version: target.artifact_version.clone(),
            source_commit: SOURCE.into(),
            prior_release_identity:
                crate::release_artifact_activation::PriorReleaseIdentity::LegacyReported {
                    short_sha: "12345678".into(),
                },
            target_triple: target.target_triple.clone(),
            computer_id: target.endpoint.computer_id,
            computer_name: target.endpoint.computer_name.clone(),
            activated_at: Utc::now(),
            mcp_service: "forgefleet-mcp.service".into(),
            daemon_service: "forgefleetd.service".into(),
            artifacts: target
                .artifacts
                .iter()
                .map(
                    |m| crate::release_artifact_activation::ActivatedArtifactReceipt {
                        artifact_name: m.artifact.artifact_name.clone(),
                        sha256: m.artifact.sha256.clone(),
                        size_bytes: m.artifact.size_bytes,
                        destinations: vec![format!("/fixed/{}", m.artifact.artifact_name)],
                    },
                )
                .collect(),
            receipt_path: format!("/fixed/{id}.receipt.json"),
        }
    }

    // The full mocked DB/transport state-machine tests live below the pure
    // boundary tests. Keeping both fakes deterministic makes crash ordering,
    // lease loss, and reverse rollback assertions exact rather than timing
    // dependent.

    #[derive(Clone)]
    struct FakeDb {
        inner: Arc<Mutex<FakeDbState>>,
    }

    struct FakeDbState {
        status: RolloutStatus,
        fail_renew: bool,
        renew_count: usize,
        transition_on_renew_to: Option<String>,
        fail_next_target_cas: bool,
        fail_target_cas_to: Option<String>,
        claim_calls: usize,
    }

    impl FakeDb {
        fn new(status: RolloutStatus) -> Self {
            Self {
                inner: Arc::new(Mutex::new(FakeDbState {
                    status,
                    fail_renew: false,
                    renew_count: 0,
                    transition_on_renew_to: None,
                    fail_next_target_cas: false,
                    fail_target_cas_to: None,
                    claim_calls: 0,
                })),
            }
        }

        fn state(&self) -> RolloutStatus {
            self.inner.lock().unwrap().status.clone()
        }

        fn expire_lease(&self) {
            self.inner
                .lock()
                .unwrap()
                .status
                .transaction
                .lease_expires_at = Utc::now() - chrono::TimeDelta::seconds(1);
        }

        fn renew_count(&self) -> usize {
            self.inner.lock().unwrap().renew_count
        }

        fn claim_calls(&self) -> usize {
            self.inner.lock().unwrap().claim_calls
        }
    }

    #[derive(Clone)]
    struct PlanDb {
        captured: Arc<Mutex<Option<ReleaseRolloutAuthoritySpec>>>,
        authority_id: Uuid,
    }

    #[async_trait]
    impl ReleaseRolloutDatabase for PlanDb {
        async fn planning_targets(&self) -> Result<Vec<RolloutEndpoint>> {
            let mut vinny = endpoint("vinny", 80);
            vinny.computer_id = FORBIDDEN_VINNY_ID;
            Ok(vec![
                endpoint("logan", 3),
                endpoint("ace", 2),
                vinny,
                endpoint("lily", 1),
                endpoint("beyonce", 0),
                endpoint("zeta", 4),
                endpoint("omega", 5),
            ])
        }

        async fn artifact_materials(
            &self,
            _source_commit: &str,
        ) -> Result<Vec<RolloutArtifactMaterial>> {
            Ok(vec![
                material("ff", "x86_64-unknown-linux-gnu", "ubuntu24-x86_64", '1'),
                material(
                    "forgefleetd",
                    "x86_64-unknown-linux-gnu",
                    "ubuntu24-x86_64",
                    '2',
                ),
            ])
        }

        async fn register_authority(
            &self,
            spec: &ReleaseRolloutAuthoritySpec,
        ) -> Result<RolloutAuthorityRegistration> {
            *self.captured.lock().unwrap() = Some(spec.clone());
            Ok(RolloutAuthorityRegistration {
                authority: ff_db::ReleaseRolloutAuthorityRow {
                    id: self.authority_id,
                    source_commit: spec.source_commit.clone(),
                    expected_target_count: spec.targets.len() as i32,
                    expected_artifact_count: (spec.targets.len() * 2) as i32,
                    created_by: spec.created_by.clone(),
                    sealed: true,
                },
                outcome: ff_db::RolloutAuthorityRegistrationOutcome::Inserted,
            })
        }

        async fn begin(
            &self,
            _request_id: Uuid,
            _authority_id: Uuid,
            _lease_seconds: i32,
        ) -> Result<RolloutTransactionBegin> {
            unreachable!()
        }
        async fn status(&self, _transaction_id: Uuid) -> Result<Option<RolloutStatus>> {
            unreachable!()
        }
        async fn transaction_by_request_id(
            &self,
            _request_id: Uuid,
        ) -> Result<Option<ReleaseRolloutTransactionRow>> {
            unreachable!()
        }
        async fn renew(
            &self,
            _transaction: &ReleaseRolloutTransactionRow,
        ) -> Result<Option<ReleaseRolloutTransactionRow>> {
            unreachable!()
        }
        async fn take_over(
            &self,
            _transaction: &ReleaseRolloutTransactionRow,
        ) -> Result<Option<ReleaseRolloutTransactionRow>> {
            unreachable!()
        }
        async fn claim_succeeded_rollback(
            &self,
            _transaction: &ReleaseRolloutTransactionRow,
        ) -> Result<Option<ReleaseRolloutTransactionRow>> {
            unreachable!()
        }
        async fn cas_transaction(
            &self,
            _transaction: &ReleaseRolloutTransactionRow,
            _expected_state: &str,
            _new_state: &str,
        ) -> Result<Option<ReleaseRolloutTransactionRow>> {
            unreachable!()
        }
        async fn cas_target(
            &self,
            _transaction: &ReleaseRolloutTransactionRow,
            _target: &RolloutTargetState,
            _expected_state: &str,
            _new_state: &str,
            _detail: Option<&str>,
        ) -> Result<bool> {
            unreachable!()
        }
    }

    #[async_trait]
    impl ReleaseRolloutDatabase for FakeDb {
        async fn planning_targets(&self) -> Result<Vec<RolloutEndpoint>> {
            unreachable!("not used by execution tests")
        }

        async fn artifact_materials(
            &self,
            _source_commit: &str,
        ) -> Result<Vec<RolloutArtifactMaterial>> {
            unreachable!("not used by execution tests")
        }

        async fn register_authority(
            &self,
            _spec: &ReleaseRolloutAuthoritySpec,
        ) -> Result<RolloutAuthorityRegistration> {
            unreachable!("not used by execution tests")
        }

        async fn begin(
            &self,
            request_id: Uuid,
            authority_id: Uuid,
            lease_seconds: i32,
        ) -> Result<RolloutTransactionBegin> {
            let state = self.inner.lock().unwrap();
            if state.status.transaction.request_id != request_id
                || state.status.transaction.authority_id != authority_id
                || state.status.transaction.lease_seconds != lease_seconds
            {
                return Err(ReleaseRolloutError::Refused(
                    "fake begin identity drifted".into(),
                ));
            }
            Ok(RolloutTransactionBegin {
                transaction: state.status.transaction.clone(),
                outcome: RolloutTransactionBeginOutcome::ExactExisting,
            })
        }

        async fn status(&self, transaction_id: Uuid) -> Result<Option<RolloutStatus>> {
            let state = self.inner.lock().unwrap();
            Ok((state.status.transaction.id == transaction_id).then(|| state.status.clone()))
        }

        async fn transaction_by_request_id(
            &self,
            request_id: Uuid,
        ) -> Result<Option<ReleaseRolloutTransactionRow>> {
            let state = self.inner.lock().unwrap();
            Ok((state.status.transaction.request_id == request_id)
                .then(|| state.status.transaction.clone()))
        }

        async fn renew(
            &self,
            transaction: &ReleaseRolloutTransactionRow,
        ) -> Result<Option<ReleaseRolloutTransactionRow>> {
            let mut state = self.inner.lock().unwrap();
            state.renew_count += 1;
            if let Some(new_state) = state.transition_on_renew_to.take() {
                state.status.transaction.state = new_state;
                state.status.transaction.cas_revision += 1;
                return Ok(None);
            }
            if state.fail_renew
                || state.status.transaction.lease_token != transaction.lease_token
                || state.status.transaction.cas_revision != transaction.cas_revision
            {
                return Ok(None);
            }
            state.status.transaction.cas_revision += 1;
            state.status.transaction.lease_expires_at = Utc::now() + chrono::TimeDelta::minutes(2);
            Ok(Some(state.status.transaction.clone()))
        }

        async fn take_over(
            &self,
            transaction: &ReleaseRolloutTransactionRow,
        ) -> Result<Option<ReleaseRolloutTransactionRow>> {
            let mut state = self.inner.lock().unwrap();
            if state.status.transaction.cas_revision != transaction.cas_revision
                || state.status.transaction.lease_expires_at > Utc::now()
            {
                return Ok(None);
            }
            state.status.transaction.cas_revision += 1;
            state.status.transaction.lease_token = Uuid::new_v4();
            state.status.transaction.lease_expires_at = Utc::now() + chrono::TimeDelta::minutes(2);
            Ok(Some(state.status.transaction.clone()))
        }

        async fn claim_succeeded_rollback(
            &self,
            transaction: &ReleaseRolloutTransactionRow,
        ) -> Result<Option<ReleaseRolloutTransactionRow>> {
            let mut state = self.inner.lock().unwrap();
            state.claim_calls += 1;
            if state.status.transaction.state != "succeeded"
                || state.status.transaction.cas_revision != transaction.cas_revision
            {
                return Ok(None);
            }
            state.status.transaction.state = "rolling_back".into();
            state.status.transaction.cas_revision += 1;
            state.status.transaction.lease_token = Uuid::new_v4();
            state.status.transaction.lease_owner = LEASE_OWNER.into();
            state.status.transaction.lease_expires_at = Utc::now() + chrono::TimeDelta::minutes(2);
            Ok(Some(state.status.transaction.clone()))
        }

        async fn cas_transaction(
            &self,
            transaction: &ReleaseRolloutTransactionRow,
            expected_state: &str,
            new_state: &str,
        ) -> Result<Option<ReleaseRolloutTransactionRow>> {
            let mut state = self.inner.lock().unwrap();
            if state.status.transaction.lease_token != transaction.lease_token
                || state.status.transaction.cas_revision != transaction.cas_revision
                || state.status.transaction.state != expected_state
            {
                return Ok(None);
            }
            state.status.transaction.state = new_state.into();
            state.status.transaction.cas_revision += 1;
            Ok(Some(state.status.transaction.clone()))
        }

        async fn cas_target(
            &self,
            transaction: &ReleaseRolloutTransactionRow,
            target: &RolloutTargetState,
            expected_state: &str,
            new_state: &str,
            detail: Option<&str>,
        ) -> Result<bool> {
            let mut state = self.inner.lock().unwrap();
            if state.fail_next_target_cas {
                state.fail_next_target_cas = false;
                return Ok(false);
            }
            if state.fail_target_cas_to.as_deref() == Some(new_state) {
                state.fail_target_cas_to = None;
                return Ok(false);
            }
            if state.status.transaction.lease_token != transaction.lease_token {
                return Ok(false);
            }
            let Some(stored) = state.status.targets.iter_mut().find(|stored| {
                stored.target.endpoint.computer_id == target.target.endpoint.computer_id
            }) else {
                return Ok(false);
            };
            if stored.cas_revision != target.cas_revision || stored.state != expected_state {
                return Ok(false);
            }
            stored.state = new_state.into();
            stored.cas_revision += 1;
            stored.detail = detail.map(str::to_string);
            Ok(true)
        }
    }

    #[tokio::test]
    async fn plan_seals_only_exact_v291_rows_in_required_order() {
        let captured = Arc::new(Mutex::new(None));
        let authority_id = Uuid::new_v4();
        let db = PlanDb {
            captured: captured.clone(),
            authority_id,
        };
        let transport = FakeTransport::default();
        let coordinator = ReleaseRolloutCoordinator::new(&db, &transport, fast_config());
        let registry = db.planning_targets().await.unwrap();
        let requested = ["beyonce", "lily", "ace", "logan", "zeta"]
            .iter()
            .map(|name| {
                roster_entry(
                    registry
                        .iter()
                        .find(|target| target.computer_name == *name)
                        .unwrap(),
                )
            })
            .collect::<Vec<_>>();
        let receipt = coordinator.plan(SOURCE, &requested).await.unwrap();
        assert_eq!(receipt.authority_id, authority_id);
        assert_eq!(
            receipt.excluded_active_extras,
            vec![roster_entry(&endpoint("omega", 5))]
        );
        let spec = captured.lock().unwrap().clone().unwrap();
        assert_eq!(spec.created_by, LEASE_OWNER);
        assert_eq!(
            spec.targets
                .iter()
                .map(|target| target.computer_name.as_str())
                .collect::<Vec<_>>(),
            ["beyonce", "lily", "ace", "logan", "zeta"]
        );
        assert!(spec.targets.iter().all(|target| {
            target.artifacts.len() == 2
                && target
                    .artifacts
                    .iter()
                    .map(|artifact| artifact.artifact_name.as_str())
                    .collect::<BTreeSet<_>>()
                    == BTreeSet::from(["ff", "forgefleetd"])
        }));
        assert!(
            transport.calls().iter().all(|call| call != "probe:omega"),
            "excluded registry extras must never be probed"
        );
    }

    #[derive(Default)]
    struct FakeTransportState {
        calls: Vec<String>,
        activation_delay: Duration,
        lose_activation_response_once: bool,
        fail_health_for: Option<String>,
        fail_rollback_for: Option<String>,
        rollback_effects: BTreeSet<String>,
    }

    #[derive(Clone, Default)]
    struct FakeTransport {
        inner: Arc<Mutex<FakeTransportState>>,
    }

    impl FakeTransport {
        fn calls(&self) -> Vec<String> {
            self.inner.lock().unwrap().calls.clone()
        }

        fn push(&self, operation: &str, target: &RolloutTarget) {
            self.inner
                .lock()
                .unwrap()
                .calls
                .push(format!("{operation}:{}", target.endpoint.computer_name));
        }

        fn rollback_effect_count(&self, computer_name: &str) -> usize {
            usize::from(
                self.inner
                    .lock()
                    .unwrap()
                    .rollback_effects
                    .contains(computer_name),
            )
        }
    }

    #[async_trait]
    impl ReleaseRolloutTransport for FakeTransport {
        async fn probe_platform(
            &self,
            target: &RolloutEndpoint,
            _source_commit: &str,
        ) -> Result<PlatformProbe> {
            self.inner
                .lock()
                .unwrap()
                .calls
                .push(format!("probe:{}", target.computer_name));
            Ok(PlatformProbe {
                target_triple: "x86_64-unknown-linux-gnu".into(),
                release_qualifier: "ubuntu24-x86_64".into(),
                platform: "linux".into(),
            })
        }

        async fn bootstrap_candidate(
            &self,
            transaction_id: Uuid,
            target: &RolloutTarget,
        ) -> Result<CandidateReceipt> {
            self.push("bootstrap", target);
            candidate_from_target(transaction_id, target)
        }

        async fn activate(
            &self,
            candidate: &CandidateReceipt,
            target: &RolloutTarget,
            _source_commit: &str,
        ) -> Result<ReleaseActivationReceipt> {
            let delay = self.inner.lock().unwrap().activation_delay;
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            self.push("activate_candidate", target);
            let mut state = self.inner.lock().unwrap();
            if state.lose_activation_response_once {
                state.lose_activation_response_once = false;
                return Err(ReleaseRolloutError::Transport(
                    "injected lost activation response".into(),
                ));
            }
            drop(state);
            Ok(activation_receipt(candidate.transaction_id, target))
        }

        async fn prove_rollback(
            &self,
            candidate: &CandidateReceipt,
            target: &RolloutTarget,
        ) -> Result<ReleaseRollbackProof> {
            self.push("prove_rollback", target);
            Ok(ReleaseRollbackProof {
                transaction_id: candidate.transaction_id,
                source_commit: SOURCE.into(),
                prior_release_identity:
                    crate::release_artifact_activation::PriorReleaseIdentity::LegacyReported {
                        short_sha: "12345678".into(),
                    },
                computer_id: target.endpoint.computer_id,
                computer_name: target.endpoint.computer_name.clone(),
                manifest_sha256: "3".repeat(64),
                activation_receipt_sha256: "4".repeat(64),
                verified_at: Utc::now(),
            })
        }

        async fn health_and_bake(
            &self,
            candidate: &CandidateReceipt,
            target: &RolloutTarget,
            source_commit: &str,
            bake: Duration,
        ) -> Result<HealthBakeEvidence> {
            self.push("health_bake", target);
            if self.inner.lock().unwrap().fail_health_for.as_deref()
                == Some(target.endpoint.computer_name.as_str())
            {
                return Err(ReleaseRolloutError::Transport(
                    "injected health failure".into(),
                ));
            }
            Ok(HealthBakeEvidence {
                transaction_id: candidate.transaction_id,
                computer_id: target.endpoint.computer_id,
                computer_name: target.endpoint.computer_name.clone(),
                source_commit: source_commit.into(),
                bake_seconds: bake.as_secs(),
                verified_at: Utc::now(),
            })
        }

        async fn rollback(
            &self,
            candidate: &CandidateReceipt,
            target: &RolloutTarget,
        ) -> Result<ReleaseRollbackReceipt> {
            self.push("rollback", target);
            if self.inner.lock().unwrap().fail_rollback_for.as_deref()
                == Some(target.endpoint.computer_name.as_str())
            {
                return Err(ReleaseRolloutError::Transport(
                    "injected rollback failure".into(),
                ));
            }
            self.inner
                .lock()
                .unwrap()
                .rollback_effects
                .insert(target.endpoint.computer_name.clone());
            Ok(ReleaseRollbackReceipt {
                transaction_id: candidate.transaction_id,
                replaced_source_commit: SOURCE.into(),
                restored_release_identity:
                    crate::release_artifact_activation::PriorReleaseIdentity::LegacyReported {
                        short_sha: "12345678".into(),
                    },
                computer_id: target.endpoint.computer_id,
                computer_name: target.endpoint.computer_name.clone(),
                rolled_back_at: Utc::now(),
                artifacts: vec![],
                receipt_path: format!("/fixed/{}.rollback.json", candidate.transaction_id),
            })
        }
    }

    fn evidence_with_activation(id: Uuid, target: &RolloutTarget) -> String {
        let candidate = candidate_from_target(id, target).unwrap();
        serde_json::to_string(&TargetEvidence {
            phase: "succeeded".into(),
            candidate: Some(candidate),
            activation: Some(activation_receipt(id, target)),
            rollback_proof: Some(ReleaseRollbackProof {
                transaction_id: id,
                source_commit: SOURCE.into(),
                prior_release_identity:
                    crate::release_artifact_activation::PriorReleaseIdentity::LegacyReported {
                        short_sha: "12345678".into(),
                    },
                computer_id: target.endpoint.computer_id,
                computer_name: target.endpoint.computer_name.clone(),
                manifest_sha256: "3".repeat(64),
                activation_receipt_sha256: "4".repeat(64),
                verified_at: Utc::now(),
            }),
            health: Some(HealthBakeEvidence {
                transaction_id: id,
                computer_id: target.endpoint.computer_id,
                computer_name: target.endpoint.computer_name.clone(),
                source_commit: SOURCE.into(),
                bake_seconds: 0,
                verified_at: Utc::now(),
            }),
            ..Default::default()
        })
        .unwrap()
    }

    fn status(states: &[&str]) -> RolloutStatus {
        let id = Uuid::new_v4();
        let names = ["beyonce", "lily", "ace", "logan"];
        let targets = names
            .iter()
            .enumerate()
            .map(|(ordinal, name)| {
                let state = states.get(ordinal).copied().unwrap_or("pending");
                let target = test_target(name, ordinal as u32);
                let detail = match state {
                    "pending" => None,
                    "installing" => Some(
                        serde_json::to_string(&TargetEvidence {
                            phase: "bootstrap".into(),
                            ..Default::default()
                        })
                        .unwrap(),
                    ),
                    "verifying" | "succeeded" | "failed" | "rolling_back" => {
                        Some(evidence_with_activation(id, &target))
                    }
                    "rolled_back" | "skipped" => Some(
                        serde_json::to_string(&TargetEvidence {
                            phase: state.to_string(),
                            ..Default::default()
                        })
                        .unwrap(),
                    ),
                    _ => panic!("unsupported fake state"),
                };
                RolloutTargetState {
                    target,
                    state: state.to_string(),
                    cas_revision: 0,
                    detail,
                }
            })
            .collect();
        RolloutStatus {
            transaction: ReleaseRolloutTransactionRow {
                id,
                request_id: Uuid::new_v4(),
                authority_id: Uuid::new_v4(),
                state: "running".into(),
                lease_token: Uuid::new_v4(),
                lease_owner: LEASE_OWNER.into(),
                lease_expires_at: Utc::now() - chrono::TimeDelta::seconds(1),
                lease_seconds: 120,
                cas_revision: 0,
                expected_target_count: names.len() as i32,
            },
            source_commit: SOURCE.into(),
            targets,
        }
    }

    fn succeeded_status() -> RolloutStatus {
        let mut state = status(&["succeeded", "succeeded", "succeeded", "succeeded"]);
        state.transaction.state = "succeeded".into();
        state
    }

    fn fast_config() -> RolloutCoordinatorConfig {
        RolloutCoordinatorConfig {
            lease_seconds: 120,
            lease_renew_interval: Duration::from_secs(60),
            canary_bake: Duration::ZERO,
            remaining_bake: Duration::ZERO,
        }
    }

    #[tokio::test]
    async fn operator_post_success_rollback_claims_once_and_reverses_exact_order() {
        let state = succeeded_status();
        let id = state.transaction.id;
        let db = FakeDb::new(state);
        let transport = FakeTransport::default();
        let coordinator = ReleaseRolloutCoordinator::new(&db, &transport, fast_config());

        let result = coordinator.rollback(id).await.unwrap();

        assert_eq!(result.transaction.state, "rolled_back");
        assert_eq!(db.claim_calls(), 1);
        let rollbacks = transport
            .calls()
            .into_iter()
            .filter(|call| call.starts_with("rollback:"))
            .collect::<Vec<_>>();
        assert_eq!(
            rollbacks,
            [
                "rollback:logan",
                "rollback:ace",
                "rollback:lily",
                "rollback:beyonce"
            ]
        );
        assert!(matches!(
            coordinator.rollback(id).await,
            Err(ReleaseRolloutError::Refused(message))
                if message.contains("terminal state rolled_back")
        ));
        assert_eq!(db.claim_calls(), 1, "repeated rollback must not reclaim");
    }

    #[tokio::test]
    async fn post_success_missing_evidence_refuses_before_claim_or_transport() {
        let mut state = succeeded_status();
        state.targets[3].detail = None;
        let id = state.transaction.id;
        let db = FakeDb::new(state);
        let transport = FakeTransport::default();
        let coordinator = ReleaseRolloutCoordinator::new(&db, &transport, fast_config());

        assert!(matches!(
            coordinator.rollback(id).await,
            Err(ReleaseRolloutError::Refused(_))
        ));
        assert_eq!(db.claim_calls(), 0);
        assert_eq!(db.state().transaction.state, "succeeded");
        assert!(transport.calls().is_empty());
    }

    #[tokio::test]
    async fn post_success_rollback_crash_resume_replays_without_duplicate_effect() {
        let state = succeeded_status();
        let id = state.transaction.id;
        let db = FakeDb::new(state);
        db.inner.lock().unwrap().fail_target_cas_to = Some("rolled_back".into());
        let transport = FakeTransport::default();
        let coordinator = ReleaseRolloutCoordinator::new(&db, &transport, fast_config());

        assert!(matches!(
            coordinator.rollback(id).await,
            Err(ReleaseRolloutError::LeaseLost(message))
                if message == "rollback receipt CAS"
        ));
        assert_eq!(db.state().transaction.state, "rolling_back");
        assert_eq!(db.state().targets[3].state, "rolling_back");
        assert_eq!(transport.rollback_effect_count("logan"), 1);

        db.expire_lease();
        let resumed = coordinator.resume(id).await.unwrap();
        assert_eq!(resumed.transaction.state, "rolled_back");
        for name in ["beyonce", "lily", "ace", "logan"] {
            assert_eq!(
                transport.rollback_effect_count(name),
                1,
                "deterministic local rollback replay must not duplicate its effect"
            );
        }
        assert_eq!(db.claim_calls(), 1);
    }

    #[tokio::test]
    async fn operator_rollback_refuses_unsafe_terminal_states_without_claim() {
        for terminal in ["failed", "rolled_back", "cancelled"] {
            let mut state = succeeded_status();
            state.transaction.state = terminal.into();
            let id = state.transaction.id;
            let db = FakeDb::new(state);
            let transport = FakeTransport::default();
            let coordinator = ReleaseRolloutCoordinator::new(&db, &transport, fast_config());
            assert!(matches!(
                coordinator.rollback(id).await,
                Err(ReleaseRolloutError::Refused(message))
                    if message.contains(terminal)
            ));
            assert_eq!(db.claim_calls(), 0);
            assert!(transport.calls().is_empty());
        }
    }

    #[tokio::test]
    async fn post_success_rollback_refuses_vinny_before_claim() {
        let mut state = succeeded_status();
        state.targets[2].target.endpoint.computer_name = "vinny".into();
        let id = state.transaction.id;
        let db = FakeDb::new(state);
        let transport = FakeTransport::default();
        let coordinator = ReleaseRolloutCoordinator::new(&db, &transport, fast_config());

        assert!(matches!(
            coordinator.rollback(id).await,
            Err(ReleaseRolloutError::Refused(message))
                if message.contains("Vinny")
        ));
        assert_eq!(db.claim_calls(), 0);
        assert!(transport.calls().is_empty());
    }

    #[tokio::test]
    async fn stale_installed_binary_is_never_invoked_before_exact_candidate_bootstrap() {
        let state = status(&["installing"]);
        let id = state.transaction.id;
        let db = FakeDb::new(state);
        let transport = FakeTransport::default();
        let coordinator = ReleaseRolloutCoordinator::new(&db, &transport, fast_config());
        let result = coordinator.resume(id).await.unwrap();
        assert_eq!(result.transaction.state, "succeeded");
        let calls = transport.calls();
        assert_eq!(calls[0], "bootstrap:beyonce");
        assert_eq!(calls[1], "activate_candidate:beyonce");
        assert!(
            calls
                .iter()
                .position(|call| call == "prove_rollback:beyonce")
                .unwrap()
                < calls
                    .iter()
                    .position(|call| call == "health_bake:beyonce")
                    .unwrap()
        );
    }

    #[tokio::test]
    async fn serial_short_phases_renew_the_parent_lease_between_iterations() {
        let state = status(&["pending", "pending", "pending"]);
        let id = state.transaction.id;
        let db = FakeDb::new(state);
        let transport = FakeTransport::default();
        let coordinator = ReleaseRolloutCoordinator::new(&db, &transport, fast_config());

        let result = coordinator.resume(id).await.unwrap();

        assert_eq!(result.transaction.state, "succeeded");
        assert!(
            result
                .targets
                .iter()
                .all(|target| target.state == "succeeded")
        );
        assert!(
            db.renew_count() >= 10,
            "every short serial phase must cross an explicit parent lease fence"
        );
    }

    #[tokio::test]
    async fn loop_renewal_loss_stops_before_selecting_or_mutating_a_target() {
        let state = status(&["pending", "pending"]);
        let id = state.transaction.id;
        let db = FakeDb::new(state.clone());
        db.inner.lock().unwrap().fail_renew = true;
        let transport = FakeTransport::default();
        let coordinator = ReleaseRolloutCoordinator::new(&db, &transport, fast_config());

        assert!(matches!(
            coordinator.resume(id).await,
            Err(ReleaseRolloutError::LeaseLost(message))
                if message == "running loop lease renewal"
        ));
        assert_eq!(db.state().targets, state.targets);
        assert!(transport.calls().is_empty());
    }

    #[tokio::test]
    async fn same_token_parent_transition_is_adopted_after_renewal_cas_loss() {
        let state = status(&["pending"]);
        let id = state.transaction.id;
        let db = FakeDb::new(state);
        db.inner.lock().unwrap().transition_on_renew_to = Some("rolling_back".into());
        let transport = FakeTransport::default();
        let coordinator = ReleaseRolloutCoordinator::new(&db, &transport, fast_config());

        let result = coordinator.resume(id).await.unwrap();

        assert_eq!(result.transaction.state, "rolled_back");
        assert_eq!(result.targets[0].state, "skipped");
        assert!(transport.calls().is_empty());
    }

    #[tokio::test]
    async fn crash_after_remote_receipt_before_db_cas_is_adopted_on_resume() {
        let mut state = status(&["installing"]);
        let id = state.transaction.id;
        let mut evidence = parse_evidence(&state.targets[0]).unwrap();
        evidence.phase = "activation".into();
        evidence.candidate = Some(candidate_from_target(id, &state.targets[0].target).unwrap());
        state.targets[0].detail = Some(serde_json::to_string(&evidence).unwrap());
        let db = FakeDb::new(state);
        db.inner.lock().unwrap().fail_next_target_cas = true;
        let transport = FakeTransport::default();
        let coordinator = ReleaseRolloutCoordinator::new(&db, &transport, fast_config());
        assert!(matches!(
            coordinator.resume(id).await,
            Err(ReleaseRolloutError::LeaseLost(_))
        ));
        assert_eq!(db.state().targets[0].state, "installing");
        db.expire_lease();
        let resumed = coordinator.resume(id).await.unwrap();
        assert_eq!(resumed.transaction.state, "succeeded");
        assert_eq!(
            transport
                .calls()
                .iter()
                .filter(|call| *call == "activate_candidate:beyonce")
                .count(),
            2,
            "the same deterministic local transaction is safely adopted"
        );
    }

    #[tokio::test]
    async fn lost_activation_response_keeps_candidate_durable_for_exact_replay() {
        let state = status(&["installing"]);
        let id = state.transaction.id;
        let db = FakeDb::new(state);
        let transport = FakeTransport::default();
        transport
            .inner
            .lock()
            .unwrap()
            .lose_activation_response_once = true;
        let coordinator = ReleaseRolloutCoordinator::new(&db, &transport, fast_config());

        assert!(matches!(
            coordinator.resume(id).await,
            Err(ReleaseRolloutError::Transport(message))
                if message.contains("lost activation response")
        ));
        let parked = db.state();
        assert_eq!(parked.targets[0].state, "installing");
        let evidence = parse_evidence(&parked.targets[0]).unwrap();
        assert!(evidence.candidate.is_some());
        assert!(evidence.activation.is_none());

        db.expire_lease();
        let resumed = coordinator.resume(id).await.unwrap();
        assert_eq!(resumed.transaction.state, "succeeded");
        let calls = transport.calls();
        assert_eq!(
            calls
                .iter()
                .filter(|call| *call == "bootstrap:beyonce")
                .count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| *call == "activate_candidate:beyonce")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn lease_loss_during_long_activation_parks_installing_for_resume() {
        let state = status(&["installing"]);
        let id = state.transaction.id;
        let db = FakeDb::new(state);
        db.inner.lock().unwrap().fail_renew = true;
        let transport = FakeTransport::default();
        transport.inner.lock().unwrap().activation_delay = Duration::from_millis(50);
        let mut config = fast_config();
        config.lease_renew_interval = Duration::from_millis(1);
        let coordinator = ReleaseRolloutCoordinator::new(&db, &transport, config);
        assert!(matches!(
            coordinator.resume(id).await,
            Err(ReleaseRolloutError::LeaseLost(_))
        ));
        assert_eq!(db.state().targets[0].state, "installing");
        assert!(
            !transport
                .calls()
                .iter()
                .any(|call| call.starts_with("health_bake"))
        );
    }

    #[tokio::test]
    async fn health_failure_rolls_back_current_and_succeeded_targets_in_reverse_order() {
        let state = status(&["succeeded", "verifying"]);
        let id = state.transaction.id;
        let db = FakeDb::new(state);
        let transport = FakeTransport::default();
        transport.inner.lock().unwrap().fail_health_for = Some("lily".into());
        let coordinator = ReleaseRolloutCoordinator::new(&db, &transport, fast_config());
        let result = coordinator.resume(id).await.unwrap();
        assert_eq!(result.transaction.state, "rolled_back");
        let rollbacks = transport
            .calls()
            .into_iter()
            .filter(|call| call.starts_with("rollback:"))
            .collect::<Vec<_>>();
        assert_eq!(rollbacks, ["rollback:lily", "rollback:beyonce"]);
    }

    #[tokio::test]
    async fn rollback_failure_remains_durably_resumable_by_another_process() {
        let mut state = status(&["rolling_back"]);
        state.transaction.state = "rolling_back".into();
        let id = state.transaction.id;
        let db = FakeDb::new(state);
        let transport = FakeTransport::default();
        transport.inner.lock().unwrap().fail_rollback_for = Some("beyonce".into());
        let coordinator = ReleaseRolloutCoordinator::new(&db, &transport, fast_config());
        assert!(matches!(
            coordinator.resume(id).await,
            Err(ReleaseRolloutError::RollbackIncomplete(_))
        ));
        assert_eq!(db.state().transaction.state, "rolling_back");
        assert_eq!(db.state().targets[0].state, "rolling_back");
        transport.inner.lock().unwrap().fail_rollback_for = None;
        db.expire_lease();
        let second_process = ReleaseRolloutCoordinator::new(&db, &transport, fast_config());
        assert_eq!(
            second_process.resume(id).await.unwrap().transaction.state,
            "rolled_back"
        );
    }

    #[tokio::test]
    async fn tampered_durable_receipt_blocks_before_remote_proof_or_progress() {
        let mut state = status(&["verifying"]);
        let id = state.transaction.id;
        let mut evidence = parse_evidence(&state.targets[0]).unwrap();
        evidence.activation.as_mut().unwrap().source_commit = "0".repeat(40);
        state.targets[0].detail = Some(serde_json::to_string(&evidence).unwrap());
        let db = FakeDb::new(state);
        let transport = FakeTransport::default();
        let coordinator = ReleaseRolloutCoordinator::new(&db, &transport, fast_config());
        assert!(matches!(
            coordinator.resume(id).await,
            Err(ReleaseRolloutError::Refused(_))
        ));
        assert!(transport.calls().is_empty());
    }

    #[tokio::test]
    async fn resume_cannot_borrow_another_foreground_process_live_lease() {
        let mut state = status(&["installing"]);
        state.transaction.lease_expires_at = Utc::now() + chrono::TimeDelta::minutes(2);
        let id = state.transaction.id;
        let db = FakeDb::new(state);
        let transport = FakeTransport::default();
        let coordinator = ReleaseRolloutCoordinator::new(&db, &transport, fast_config());

        assert!(matches!(
            coordinator.resume(id).await,
            Err(ReleaseRolloutError::LeaseLost(message))
                if message.contains("another foreground coordinator")
        ));
        assert!(transport.calls().is_empty());
    }

    #[tokio::test]
    async fn request_id_recovers_the_exact_transaction_for_status_and_resume() {
        let state = succeeded_status();
        let transaction_id = state.transaction.id;
        let request_id = state.transaction.request_id;
        let db = FakeDb::new(state);
        let transport = FakeTransport::default();
        let coordinator = ReleaseRolloutCoordinator::new(&db, &transport, fast_config());

        assert_eq!(
            coordinator
                .status_for_identity(None, Some(request_id))
                .await
                .unwrap()
                .transaction
                .id,
            transaction_id
        );
        assert_eq!(
            coordinator
                .resume_for_identity(None, Some(request_id))
                .await
                .unwrap()
                .transaction
                .id,
            transaction_id
        );
        assert!(
            coordinator
                .status_for_identity(Some(transaction_id), Some(request_id))
                .await
                .is_err()
        );
        assert!(coordinator.status_for_identity(None, None).await.is_err());
        assert!(
            coordinator
                .status_for_identity(None, Some(Uuid::new_v4()))
                .await
                .is_err()
        );
        assert!(transport.calls().is_empty());
    }

    #[tokio::test]
    async fn start_failure_after_begin_always_reports_both_recovery_ids() {
        let state = status(&["pending"]);
        let transaction_id = state.transaction.id;
        let request_id = state.transaction.request_id;
        let authority_id = state.transaction.authority_id;
        let db = FakeDb::new(state);
        let transport = FakeTransport::default();
        let coordinator = ReleaseRolloutCoordinator::new(&db, &transport, fast_config());

        let error = coordinator
            .start(authority_id, request_id)
            .await
            .expect_err("an exact replay cannot borrow its still-expired initial lease");
        assert!(matches!(
            &error,
            ReleaseRolloutError::AfterBegin {
                transaction_id: actual_transaction_id,
                request_id: actual_request_id,
                ..
            } if *actual_transaction_id == transaction_id && *actual_request_id == request_id
        ));
        let rendered = error.to_string();
        assert!(rendered.contains(&transaction_id.to_string()));
        assert!(rendered.contains(&request_id.to_string()));
    }

    #[tokio::test]
    async fn registry_readiness_drift_refuses_before_takeover_renewal_or_transport() {
        let mut state = status(&["pending"]);
        state.targets[0].target.endpoint.worker_status = "offline".into();
        let before = state.transaction.clone();
        let id = before.id;
        let db = FakeDb::new(state);
        let transport = FakeTransport::default();
        let coordinator = ReleaseRolloutCoordinator::new(&db, &transport, fast_config());

        assert!(matches!(
            coordinator.resume(id).await,
            Err(ReleaseRolloutError::Refused(message))
                if message.contains("not active in both live registries")
        ));
        assert_eq!(db.state().transaction, before);
        assert_eq!(db.renew_count(), 0);
        assert!(transport.calls().is_empty());
    }

    #[tokio::test]
    async fn candidate_capture_preserves_preconfigured_stdout_file() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", "printf candidate-bytes; printf diagnostic >&2"])
            .stdout(Stdio::from(temp.reopen().unwrap()))
            .stderr(Stdio::piped());

        let output = wait_with_preserved_stdio(&mut command).await.unwrap();

        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert_eq!(output.stderr, b"diagnostic");
        assert_eq!(std::fs::read(temp.path()).unwrap(), b"candidate-bytes");
    }

    #[tokio::test]
    async fn candidate_execution_leaves_locking_to_the_artifact_layer() {
        let home = tempfile::tempdir().unwrap();
        let transaction_id = Uuid::new_v4();
        let base = home
            .path()
            .join(".forgefleet")
            .join("release-rollout")
            .join("candidates")
            .join(transaction_id.to_string());
        std::fs::create_dir_all(&base).unwrap();
        let candidate = base.join("ff");
        std::fs::write(&candidate, b"#!/bin/sh\nexit 7\n").unwrap();
        std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o500)).unwrap();
        let bytes = std::fs::read(&candidate).unwrap();
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        let lock = std::path::PathBuf::from(format!("{}.lock", base.display()));

        let output = tokio::process::Command::new("/bin/sh")
            .args([
                "-ceu",
                RUN_CANDIDATE_SCRIPT,
                "--",
                &transaction_id.to_string(),
                &sha256,
                &bytes.len().to_string(),
                "linux",
                candidate.to_str().unwrap(),
            ])
            .env("HOME", home.path())
            .output()
            .await
            .unwrap();

        assert_eq!(output.status.code(), Some(7));
        assert!(
            !lock.exists(),
            "the rollout wrapper must not create a crash-stale directory lock"
        );
        assert!(!RUN_CANDIDATE_SCRIPT.contains("mkdir \"$lock\""));
    }
}
