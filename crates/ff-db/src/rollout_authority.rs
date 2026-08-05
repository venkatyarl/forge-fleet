//! Exact V295 release-rollout authority and leased/CAS execution APIs.
//!
//! V291 owns immutable artifact bytes. This module binds exactly two V291
//! artifacts (`ff`, `forgefleetd`) to each exact target identity, seals the
//! complete set atomically, and exposes only lease-fenced CAS state changes.

use std::collections::BTreeSet;

use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::{DbError, Result};

pub const FORBIDDEN_VINNY_NAME: &str = "vinny";
pub const FORBIDDEN_VINNY_ID: Uuid = Uuid::from_u128(0xe7f5d063_d7b7_4338_bd6e_5d02d74770ad);
const AUTHORITY_XACT_LOCK_KEY: i64 = 0x4646_524f_4c4c_4155;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutArtifactAuthority {
    pub artifact_name: String,
    pub artifact_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutTargetAuthority {
    pub target_ordinal: u32,
    pub computer_id: Uuid,
    pub computer_name: String,
    pub target_triple: String,
    pub artifact_version: String,
    pub artifacts: Vec<RolloutArtifactAuthority>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseRolloutAuthoritySpec {
    pub source_commit: String,
    pub created_by: String,
    pub targets: Vec<RolloutTargetAuthority>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloutAuthorityRegistrationOutcome {
    Inserted,
    ExactExisting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseRolloutAuthorityRow {
    pub id: Uuid,
    pub source_commit: String,
    pub expected_target_count: i32,
    pub expected_artifact_count: i32,
    pub created_by: String,
    pub sealed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutAuthorityRegistration {
    pub authority: ReleaseRolloutAuthorityRow,
    pub outcome: RolloutAuthorityRegistrationOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReleaseRolloutTransactionRow {
    pub id: Uuid,
    pub request_id: Uuid,
    pub authority_id: Uuid,
    pub state: String,
    pub lease_token: Uuid,
    pub lease_owner: String,
    pub lease_expires_at: chrono::DateTime<chrono::Utc>,
    pub lease_seconds: i32,
    pub cas_revision: i64,
    pub expected_target_count: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloutTransactionBeginOutcome {
    Inserted,
    ExactExisting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutTransactionBegin {
    pub transaction: ReleaseRolloutTransactionRow,
    pub outcome: RolloutTransactionBeginOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseRolloutTargetStateRow {
    pub transaction_id: Uuid,
    pub computer_id: Uuid,
    pub computer_name: String,
    pub state: String,
    pub cas_revision: i64,
    pub detail: Option<String>,
}

fn is_full_lower_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn canonical_operator(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'@' | b'-'))
}

fn canonical_computer_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
}

fn canonical_target_triple(value: &str) -> bool {
    let parts = value.split('-').collect::<Vec<_>>();
    (3..=5).contains(&parts.len())
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'_' | b'.')
                })
        })
}

fn canonical_artifact_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'_' | b'+' | b'-')
        })
}

fn validate_authority_spec(spec: &ReleaseRolloutAuthoritySpec) -> Result<()> {
    if !is_full_lower_sha(&spec.source_commit) {
        return Err(DbError::ArtifactIntegrity(
            "rollout authority requires a full lowercase source commit".to_string(),
        ));
    }
    if !canonical_operator(&spec.created_by) {
        return Err(DbError::ArtifactIntegrity(
            "rollout authority creator is not canonical".to_string(),
        ));
    }
    if spec.targets.is_empty() || spec.targets.len() > 64 {
        return Err(DbError::ArtifactIntegrity(
            "rollout authority requires 1-64 exact targets".to_string(),
        ));
    }
    let mut ids = BTreeSet::new();
    let mut names = BTreeSet::new();
    let mut ordinals = BTreeSet::new();
    for (expected_ordinal, target) in spec.targets.iter().enumerate() {
        if target.computer_id == FORBIDDEN_VINNY_ID
            || target
                .computer_name
                .eq_ignore_ascii_case(FORBIDDEN_VINNY_NAME)
        {
            return Err(DbError::ArtifactIntegrity(
                "release rollout authority forbids Vinny by exact name and UUID".to_string(),
            ));
        }
        if !canonical_computer_name(&target.computer_name)
            || !canonical_target_triple(&target.target_triple)
            || !canonical_artifact_version(&target.artifact_version)
        {
            return Err(DbError::ArtifactIntegrity(format!(
                "rollout target {} has non-canonical identity",
                target.computer_name
            )));
        }
        if target.target_ordinal as usize != expected_ordinal
            || !ordinals.insert(target.target_ordinal)
            || !ids.insert(target.computer_id)
            || !names.insert(target.computer_name.as_str())
        {
            return Err(DbError::ArtifactIntegrity(
                "rollout targets contain duplicate or non-contiguous identity".to_string(),
            ));
        }
        if target.artifacts.len() != 2 {
            return Err(DbError::ArtifactIntegrity(format!(
                "rollout target {} requires exactly two artifacts",
                target.computer_name
            )));
        }
        let artifact_names = target
            .artifacts
            .iter()
            .map(|artifact| artifact.artifact_name.as_str())
            .collect::<BTreeSet<_>>();
        if artifact_names != BTreeSet::from(["ff", "forgefleetd"])
            || target.artifacts[0].artifact_id == target.artifacts[1].artifact_id
        {
            return Err(DbError::ArtifactIntegrity(format!(
                "rollout target {} artifact set is not exactly ff + forgefleetd",
                target.computer_name
            )));
        }
    }
    if ordinals != (0..spec.targets.len() as u32).collect() {
        return Err(DbError::ArtifactIntegrity(
            "rollout target ordinals must be contiguous from zero".to_string(),
        ));
    }
    Ok(())
}

fn authority_from_row(row: &sqlx::postgres::PgRow) -> ReleaseRolloutAuthorityRow {
    ReleaseRolloutAuthorityRow {
        id: row.get("id"),
        source_commit: row.get("source_commit"),
        expected_target_count: row.get("expected_target_count"),
        expected_artifact_count: row.get("expected_artifact_count"),
        created_by: row.get("created_by"),
        sealed: row
            .get::<Option<chrono::DateTime<chrono::Utc>>, _>("sealed_at")
            .is_some(),
    }
}

fn transaction_from_row(row: &sqlx::postgres::PgRow) -> ReleaseRolloutTransactionRow {
    ReleaseRolloutTransactionRow {
        id: row.get("id"),
        request_id: row.get("request_id"),
        authority_id: row.get("authority_id"),
        state: row.get("state"),
        lease_token: row.get("lease_token"),
        lease_owner: row.get("lease_owner"),
        lease_expires_at: row.get("lease_expires_at"),
        lease_seconds: row.get("lease_seconds"),
        cas_revision: row.get("cas_revision"),
        expected_target_count: row.get("expected_target_count"),
    }
}

async fn exact_existing_authority(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    row: ReleaseRolloutAuthorityRow,
    spec: &ReleaseRolloutAuthoritySpec,
) -> Result<RolloutAuthorityRegistration> {
    if row.source_commit != spec.source_commit
        || row.created_by != spec.created_by
        || row.expected_target_count != spec.targets.len() as i32
        || row.expected_artifact_count != (spec.targets.len() * 2) as i32
        || !row.sealed
    {
        return Err(DbError::ArtifactIntegrity(
            "existing rollout authority parent drifted".to_string(),
        ));
    }
    let targets = sqlx::query(
        "SELECT target_ordinal, computer_id, computer_name, target_triple, artifact_version
           FROM release_rollout_authority_targets
          WHERE authority_id = $1 ORDER BY target_ordinal",
    )
    .bind(row.id)
    .fetch_all(&mut **transaction)
    .await?;
    if targets.len() != spec.targets.len() {
        return Err(DbError::ArtifactIntegrity(
            "existing rollout authority target count drifted".to_string(),
        ));
    }
    for (stored, expected) in targets.iter().zip(&spec.targets) {
        if stored.get::<i32, _>("target_ordinal") != expected.target_ordinal as i32
            || stored.get::<Uuid, _>("computer_id") != expected.computer_id
            || stored.get::<String, _>("computer_name") != expected.computer_name
            || stored.get::<String, _>("target_triple") != expected.target_triple
            || stored.get::<String, _>("artifact_version") != expected.artifact_version
        {
            return Err(DbError::ArtifactIntegrity(
                "existing rollout authority target identity drifted".to_string(),
            ));
        }
        let artifacts = sqlx::query(
            "SELECT artifact_name, artifact_id
               FROM release_rollout_authority_artifacts
              WHERE authority_id = $1 AND computer_id = $2
              ORDER BY artifact_name",
        )
        .bind(row.id)
        .bind(expected.computer_id)
        .fetch_all(&mut **transaction)
        .await?;
        let mut expected_artifacts = expected.artifacts.clone();
        expected_artifacts.sort_by(|a, b| a.artifact_name.cmp(&b.artifact_name));
        if artifacts.len() != expected_artifacts.len()
            || artifacts
                .iter()
                .zip(expected_artifacts)
                .any(|(stored, expected)| {
                    stored.get::<String, _>("artifact_name") != expected.artifact_name
                        || stored.get::<Uuid, _>("artifact_id") != expected.artifact_id
                })
        {
            return Err(DbError::ArtifactIntegrity(
                "existing rollout authority release identity drifted".to_string(),
            ));
        }
    }
    Ok(RolloutAuthorityRegistration {
        authority: row,
        outcome: RolloutAuthorityRegistrationOutcome::ExactExisting,
    })
}

/// Insert and seal one exact source/release/target authority, or return the
/// byte-for-byte equivalent sealed authority for an idempotent retry.
pub async fn pg_register_release_rollout_authority(
    pool: &PgPool,
    spec: &ReleaseRolloutAuthoritySpec,
) -> Result<RolloutAuthorityRegistration> {
    validate_authority_spec(spec)?;
    if !release_rollout_schema_is_exact(pool).await? {
        return Err(DbError::ArtifactIntegrity(
            "rollout authority schema or committed authority data drifted".to_string(),
        ));
    }
    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(AUTHORITY_XACT_LOCK_KEY)
        .execute(&mut *transaction)
        .await?;
    if let Some(existing) = sqlx::query(
        "SELECT id, source_commit, expected_target_count, expected_artifact_count,
                created_by, sealed_at
           FROM release_rollout_authorities
          WHERE source_commit = $1 FOR UPDATE",
    )
    .bind(&spec.source_commit)
    .fetch_optional(&mut *transaction)
    .await?
    {
        let result =
            exact_existing_authority(&mut transaction, authority_from_row(&existing), spec).await?;
        transaction.commit().await?;
        return Ok(result);
    }

    let inserted = sqlx::query(
        "INSERT INTO release_rollout_authorities
            (source_commit, expected_target_count, expected_artifact_count, created_by)
         VALUES ($1, $2, $3, $4)
         RETURNING id, source_commit, expected_target_count, expected_artifact_count,
                   created_by, sealed_at",
    )
    .bind(&spec.source_commit)
    .bind(spec.targets.len() as i32)
    .bind((spec.targets.len() * 2) as i32)
    .bind(&spec.created_by)
    .fetch_one(&mut *transaction)
    .await?;
    let authority_id: Uuid = inserted.get("id");
    for target in &spec.targets {
        sqlx::query(
            "INSERT INTO release_rollout_authority_targets
                (authority_id, target_ordinal, computer_id, computer_name,
                 target_triple, artifact_version)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(authority_id)
        .bind(target.target_ordinal as i32)
        .bind(target.computer_id)
        .bind(&target.computer_name)
        .bind(&target.target_triple)
        .bind(&target.artifact_version)
        .execute(&mut *transaction)
        .await?;
        for artifact in &target.artifacts {
            sqlx::query(
                "INSERT INTO release_rollout_authority_artifacts
                    (authority_id, computer_id, artifact_name, artifact_id)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(authority_id)
            .bind(target.computer_id)
            .bind(&artifact.artifact_name)
            .bind(artifact.artifact_id)
            .execute(&mut *transaction)
            .await?;
        }
    }
    let sealed = sqlx::query(
        "UPDATE release_rollout_authorities SET sealed_at = clock_timestamp()
          WHERE id = $1 AND sealed_at IS NULL
          RETURNING id, source_commit, expected_target_count, expected_artifact_count,
                    created_by, sealed_at",
    )
    .bind(authority_id)
    .fetch_one(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(RolloutAuthorityRegistration {
        authority: authority_from_row(&sealed),
        outcome: RolloutAuthorityRegistrationOutcome::Inserted,
    })
}

/// Atomically create a leased transaction and its complete exact target-state
/// set. `request_id` is the idempotency key; a mismatched replay fails closed.
pub async fn pg_begin_release_rollout(
    pool: &PgPool,
    request_id: Uuid,
    authority_id: Uuid,
    lease_owner: &str,
    lease_seconds: i32,
) -> Result<RolloutTransactionBegin> {
    if !canonical_operator(lease_owner) || !(30..=3600).contains(&lease_seconds) {
        return Err(DbError::ArtifactIntegrity(
            "rollout lease owner or duration is invalid".to_string(),
        ));
    }
    if !release_rollout_schema_is_exact(pool).await? {
        return Err(DbError::ArtifactIntegrity(
            "rollout authority schema or committed authority data drifted".to_string(),
        ));
    }
    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(AUTHORITY_XACT_LOCK_KEY)
        .execute(&mut *transaction)
        .await?;
    if let Some(existing) = sqlx::query(
        "SELECT id, request_id, authority_id, state, lease_token, lease_owner,
                lease_expires_at, lease_seconds, cas_revision, expected_target_count
           FROM release_rollout_transactions WHERE request_id = $1 FOR UPDATE",
    )
    .bind(request_id)
    .fetch_optional(&mut *transaction)
    .await?
    {
        let existing = transaction_from_row(&existing);
        if existing.authority_id != authority_id
            || existing.lease_owner != lease_owner
            || existing.lease_seconds != lease_seconds
        {
            return Err(DbError::ArtifactIntegrity(
                "rollout request id replay drifted".to_string(),
            ));
        }
        transaction.commit().await?;
        return Ok(RolloutTransactionBegin {
            transaction: existing,
            outcome: RolloutTransactionBeginOutcome::ExactExisting,
        });
    }
    let target_count: i32 = sqlx::query_scalar(
        "SELECT expected_target_count
           FROM release_rollout_authorities
          WHERE id = $1 AND sealed_at IS NOT NULL FOR KEY SHARE",
    )
    .bind(authority_id)
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or_else(|| {
        DbError::ArtifactIntegrity("rollout authority is missing or unsealed".to_string())
    })?;
    let inserted = sqlx::query(
        "INSERT INTO release_rollout_transactions
            (request_id, authority_id, lease_owner, lease_expires_at,
             lease_seconds, expected_target_count)
         VALUES ($1, $2, $3, clock_timestamp() + make_interval(secs => $4), $4, $5)
         RETURNING id, request_id, authority_id, state, lease_token, lease_owner,
                   lease_expires_at, lease_seconds, cas_revision, expected_target_count",
    )
    .bind(request_id)
    .bind(authority_id)
    .bind(lease_owner)
    .bind(lease_seconds)
    .bind(target_count)
    .fetch_one(&mut *transaction)
    .await?;
    let rollout = transaction_from_row(&inserted);
    sqlx::query(
        "INSERT INTO release_rollout_target_states
            (transaction_id, computer_id, computer_name, target_ordinal,
             target_triple, artifact_version)
         SELECT $1, computer_id, computer_name, target_ordinal,
                target_triple, artifact_version
           FROM release_rollout_authority_targets
          WHERE authority_id = $2
          ORDER BY target_ordinal",
    )
    .bind(rollout.id)
    .bind(authority_id)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(RolloutTransactionBegin {
        transaction: rollout,
        outcome: RolloutTransactionBeginOutcome::Inserted,
    })
}

/// Renew one live lease under its exact token and CAS revision.
pub async fn pg_renew_release_rollout_lease(
    pool: &PgPool,
    transaction_id: Uuid,
    lease_token: Uuid,
    expected_revision: i64,
) -> Result<Option<ReleaseRolloutTransactionRow>> {
    let row = sqlx::query(
        "UPDATE release_rollout_transactions
            SET lease_expires_at = clock_timestamp() + make_interval(secs => lease_seconds),
                cas_revision = cas_revision + 1
          WHERE id = $1 AND lease_token = $2 AND cas_revision = $3
            AND lease_expires_at > clock_timestamp()
            AND state IN ('planned', 'running', 'rolling_back')
          RETURNING id, request_id, authority_id, state, lease_token, lease_owner,
                    lease_expires_at, lease_seconds, cas_revision, expected_target_count",
    )
    .bind(transaction_id)
    .bind(lease_token)
    .bind(expected_revision)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(transaction_from_row))
}

/// Take over one expired active lease under CAS, rotating the token.
pub async fn pg_take_over_release_rollout_lease(
    pool: &PgPool,
    transaction_id: Uuid,
    expected_revision: i64,
    lease_owner: &str,
) -> Result<Option<ReleaseRolloutTransactionRow>> {
    if !canonical_operator(lease_owner) {
        return Err(DbError::ArtifactIntegrity(
            "rollout lease owner is not canonical".to_string(),
        ));
    }
    let row = sqlx::query(
        "UPDATE release_rollout_transactions
            SET lease_token = gen_random_uuid(), lease_owner = $3,
                lease_expires_at = clock_timestamp() + make_interval(secs => lease_seconds),
                cas_revision = cas_revision + 1
          WHERE id = $1 AND cas_revision = $2
            AND lease_expires_at <= clock_timestamp()
            AND state IN ('planned', 'running', 'rolling_back')
          RETURNING id, request_id, authority_id, state, lease_token, lease_owner,
                    lease_expires_at, lease_seconds, cas_revision, expected_target_count",
    )
    .bind(transaction_id)
    .bind(expected_revision)
    .bind(lease_owner)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(transaction_from_row))
}

/// CAS the parent rollout state while the caller's exact lease remains live.
pub async fn pg_cas_release_rollout_transaction_state(
    pool: &PgPool,
    transaction_id: Uuid,
    lease_token: Uuid,
    expected_revision: i64,
    expected_state: &str,
    new_state: &str,
) -> Result<Option<ReleaseRolloutTransactionRow>> {
    let terminal = matches!(
        new_state,
        "succeeded" | "failed" | "rolled_back" | "cancelled"
    );
    let row = sqlx::query(
        "UPDATE release_rollout_transactions
            SET state = $5,
                completed_at = CASE WHEN $6 THEN clock_timestamp() ELSE NULL END,
                cas_revision = cas_revision + 1
          WHERE id = $1 AND lease_token = $2 AND cas_revision = $3
            AND state = $4 AND lease_expires_at > clock_timestamp()
            AND state IN ('planned', 'running', 'rolling_back')
          RETURNING id, request_id, authority_id, state, lease_token, lease_owner,
                    lease_expires_at, lease_seconds, cas_revision, expected_target_count",
    )
    .bind(transaction_id)
    .bind(lease_token)
    .bind(expected_revision)
    .bind(expected_state)
    .bind(new_state)
    .bind(terminal)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(transaction_from_row))
}

/// CAS one per-target state while the exact parent lease is live.
#[allow(clippy::too_many_arguments)]
pub async fn pg_cas_release_rollout_target_state(
    pool: &PgPool,
    transaction_id: Uuid,
    computer_id: Uuid,
    lease_token: Uuid,
    expected_revision: i64,
    expected_state: &str,
    new_state: &str,
    detail: Option<&str>,
) -> Result<Option<ReleaseRolloutTargetStateRow>> {
    let row = sqlx::query(
        "UPDATE release_rollout_target_states target
            SET state = $6, detail = $7, cas_revision = target.cas_revision + 1
           FROM release_rollout_transactions rollout
          WHERE target.transaction_id = $1 AND target.computer_id = $2
            AND target.cas_revision = $4 AND target.state = $5
            AND rollout.id = target.transaction_id AND rollout.lease_token = $3
            AND rollout.lease_expires_at > clock_timestamp()
            AND rollout.state IN ('planned', 'running', 'rolling_back')
          RETURNING target.transaction_id, target.computer_id, target.computer_name,
                    target.state, target.cas_revision, target.detail",
    )
    .bind(transaction_id)
    .bind(computer_id)
    .bind(lease_token)
    .bind(expected_revision)
    .bind(expected_state)
    .bind(new_state)
    .bind(detail)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(|row| ReleaseRolloutTargetStateRow {
        transaction_id: row.get("transaction_id"),
        computer_id: row.get("computer_id"),
        computer_name: row.get("computer_name"),
        state: row.get("state"),
        cas_revision: row.get("cas_revision"),
        detail: row.get("detail"),
    }))
}

/// Read-only exact-schema/data predicate used by migration status and replay.
pub async fn release_rollout_schema_is_exact(pool: &PgPool) -> Result<bool> {
    let required_objects_exist: bool = sqlx::query_scalar(
        r#"
        SELECT
            to_regclass('public.release_rollout_authorities') IS NOT NULL
        AND to_regclass('public.release_rollout_authority_targets') IS NOT NULL
        AND to_regclass('public.release_rollout_authority_artifacts') IS NOT NULL
        AND to_regclass('public.release_rollout_transactions') IS NOT NULL
        AND to_regclass('public.release_rollout_target_states') IS NOT NULL
        AND to_regclass('public.release_rollout_one_active_transaction') IS NOT NULL
        "#,
    )
    .fetch_one(pool)
    .await?;
    if !required_objects_exist {
        return Ok(false);
    }

    Ok(sqlx::query_scalar(
        r#"
        SELECT
            (SELECT count(*) FROM pg_trigger
              WHERE tgrelid IN (
                    'release_rollout_authorities'::regclass,
                    'release_rollout_authority_targets'::regclass,
                    'release_rollout_authority_artifacts'::regclass,
                    'release_rollout_transactions'::regclass,
                    'release_rollout_target_states'::regclass)
                AND NOT tgisinternal
                AND tgenabled = 'O') = 12
        AND NOT EXISTS (
            SELECT 1 FROM release_rollout_authorities authority
             WHERE authority.sealed_at IS NULL
                OR (SELECT count(*) FROM release_rollout_authority_targets target
                     WHERE target.authority_id = authority.id)
                    <> authority.expected_target_count
                OR (SELECT count(*) FROM release_rollout_authority_artifacts artifact
                     WHERE artifact.authority_id = authority.id)
                    <> authority.expected_artifact_count)
        AND NOT EXISTS (
            SELECT 1 FROM release_rollout_authority_targets target
             LEFT JOIN computers computer ON computer.id = target.computer_id
             WHERE computer.id IS NULL
                OR computer.name IS DISTINCT FROM target.computer_name
                OR lower(target.computer_name) = 'vinny'
                OR target.computer_id = 'e7f5d063-d7b7-4338-bd6e-5d02d74770ad'::uuid)
        AND NOT EXISTS (
            SELECT 1
              FROM release_rollout_authority_targets target
              JOIN release_rollout_authorities authority ON authority.id = target.authority_id
              LEFT JOIN release_rollout_authority_artifacts exact_artifact
                ON exact_artifact.authority_id = target.authority_id
               AND exact_artifact.computer_id = target.computer_id
              LEFT JOIN release_artifacts artifact ON artifact.id = exact_artifact.artifact_id
             GROUP BY target.authority_id, target.computer_id,
                      target.target_triple, target.artifact_version,
                      authority.source_commit
            HAVING count(exact_artifact.*) <> 2
                OR count(*) FILTER (WHERE exact_artifact.artifact_name = 'ff') <> 1
                OR count(*) FILTER (WHERE exact_artifact.artifact_name = 'forgefleetd') <> 1
                OR bool_or(artifact.id IS NULL)
                OR bool_or(artifact.artifact_name IS DISTINCT FROM exact_artifact.artifact_name)
                OR bool_or(artifact.source_commit IS DISTINCT FROM authority.source_commit)
                OR bool_or(artifact.target_triple IS DISTINCT FROM target.target_triple)
                OR bool_or(artifact.artifact_version IS DISTINCT FROM target.artifact_version)
        )
        AND NOT EXISTS (
            SELECT 1 FROM release_rollout_target_states target
             WHERE lower(target.computer_name) = 'vinny'
                OR target.computer_id = 'e7f5d063-d7b7-4338-bd6e-5d02d74770ad'::uuid)
        "#,
    )
    .fetch_one(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "39b017341b7536df64b61f42672ab33fb62343f8";

    fn artifact(name: &str, id: u128) -> RolloutArtifactAuthority {
        RolloutArtifactAuthority {
            artifact_name: name.to_string(),
            artifact_id: Uuid::from_u128(id),
        }
    }

    fn target(name: &str, id: Uuid) -> RolloutTargetAuthority {
        RolloutTargetAuthority {
            target_ordinal: 0,
            computer_id: id,
            computer_name: name.to_string(),
            target_triple: "aarch64-unknown-linux-gnu".to_string(),
            artifact_version: format!("recovery.{SOURCE}.ubuntu24-arm64"),
            artifacts: vec![artifact("ff", 1), artifact("forgefleetd", 2)],
        }
    }

    #[test]
    fn exact_authority_validation_accepts_one_bounded_target() {
        let spec = ReleaseRolloutAuthoritySpec {
            source_commit: SOURCE.to_string(),
            created_by: "operator@adele".to_string(),
            targets: vec![target("beyonce", Uuid::from_u128(3))],
        };
        validate_authority_spec(&spec).unwrap();
    }

    #[test]
    fn authority_rejects_vinny_by_name_and_uuid_without_override() {
        let base = ReleaseRolloutAuthoritySpec {
            source_commit: SOURCE.to_string(),
            created_by: "operator@adele".to_string(),
            targets: vec![target("vinny", Uuid::from_u128(3))],
        };
        assert!(validate_authority_spec(&base).is_err());
        let mut by_id = base;
        by_id.targets[0].computer_name = "ace".to_string();
        by_id.targets[0].computer_id = FORBIDDEN_VINNY_ID;
        assert!(validate_authority_spec(&by_id).is_err());
    }

    #[test]
    fn authority_rejects_partial_duplicate_and_noncontiguous_sets() {
        let mut spec = ReleaseRolloutAuthoritySpec {
            source_commit: SOURCE.to_string(),
            created_by: "operator@adele".to_string(),
            targets: vec![target("beyonce", Uuid::from_u128(3))],
        };
        spec.targets[0].artifacts.pop();
        assert!(validate_authority_spec(&spec).is_err());

        let mut duplicate = target("lily", Uuid::from_u128(4));
        duplicate.target_ordinal = 0;
        let duplicate_spec = ReleaseRolloutAuthoritySpec {
            source_commit: SOURCE.to_string(),
            created_by: "operator@adele".to_string(),
            targets: vec![target("beyonce", Uuid::from_u128(3)), duplicate],
        };
        assert!(validate_authority_spec(&duplicate_spec).is_err());
    }
}
