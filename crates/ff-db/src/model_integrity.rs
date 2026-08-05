//! Compare-or-CAS persistence for verified model-library hashes.
//!
//! Existing non-NULL hashes are comparison-only. A NULL hash is initialized
//! with one CAS constrained by row id, owner, path, size, and `downloaded_at`.
//! The assertion must come from a freshly completed filesystem verifier final
//! identity pass and be submitted without caching/replay. This transaction
//! fences database identity, but no userspace API can make the preceding
//! filesystem scan and PostgreSQL commit one atomic snapshot. Directory
//! manifests fail closed until the live schema records algorithm and kind.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use ff_core::model_integrity::{
    ModelArtifactKind, constant_time_sha256_eq, model_integrity_worker_allowed, parse_sha256_hex,
};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

use crate::PgPool;
use crate::error::{DbError, Result};

const MODEL_LIBRARY_SHA256_CAS_SQL: &str = "UPDATE fleet_model_library
        SET sha256 = $6
      WHERE id = $1
        AND worker_name = $2
        AND file_path = $3
        AND size_bytes = $4
        AND downloaded_at = $5
        AND sha256 IS NULL";

const MODEL_LIBRARY_HASH_SELECT_FOR_UPDATE_SQL: &str =
    "SELECT worker_name, file_path, size_bytes, downloaded_at, sha256
       FROM fleet_model_library
      WHERE id = $1
      FOR UPDATE";

const RELEASE_ARTIFACT_INSERT_SQL: &str = "INSERT INTO release_artifacts
        (artifact_name, artifact_version, source_commit, target_triple, sha256, size_bytes)
    VALUES ($1, $2, $3, $4, $5, $6)
    ON CONFLICT (artifact_name, artifact_version, source_commit, target_triple) DO NOTHING
    RETURNING id, artifact_name, artifact_version, source_commit, target_triple,
              sha256, size_bytes, created_at";

const RELEASE_ARTIFACT_SELECT_FOR_UPDATE_SQL: &str = "SELECT id, artifact_name,
           artifact_version, source_commit, target_triple, sha256, size_bytes, created_at
      FROM release_artifacts
     WHERE artifact_name = $1 AND artifact_version = $2
       AND source_commit = $3 AND target_triple = $4
     FOR UPDATE";

const RELEASE_CUSTODY_INSERT_SQL: &str = "INSERT INTO release_artifact_custody
        (artifact_id, computer_id, holder_name_at_registration, relative_path)
    VALUES ($1, $2, $3, $4)
    ON CONFLICT (artifact_id, computer_id) DO NOTHING
    RETURNING artifact_id, computer_id, holder_name_at_registration, relative_path,
              first_verified_at, last_verified_at";

const RELEASE_CUSTODY_SELECT_FOR_UPDATE_SQL: &str = "SELECT artifact_id, computer_id,
           holder_name_at_registration, relative_path, first_verified_at, last_verified_at
      FROM release_artifact_custody
     WHERE artifact_id = $1 AND computer_id = $2
     FOR UPDATE";

const RELEASE_HOLDER_SELECT_FOR_UPDATE_SQL: &str =
    "SELECT name FROM computers WHERE id = $1 FOR UPDATE";

#[derive(Debug, Clone)]
pub struct ModelLibraryHashAssertion {
    pub row_id: Uuid,
    pub worker_name: String,
    pub file_path: String,
    pub size_bytes: i64,
    pub downloaded_at: DateTime<Utc>,
    pub artifact_kind: ModelArtifactKind,
    pub digest_algorithm: String,
    pub computed_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashWriteDecision {
    Initialize,
    Match,
    Mismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelLibraryHashCasOutcome {
    Initialized,
    Match,
    Mismatch,
    StaleIdentity,
    LostRace,
}

/// Fresh, verified evidence submitted to the immutable release registry.
///
/// The caller must construct this immediately after a descriptor-relative
/// filesystem verification. The transaction independently fences the current
/// computer name so a concurrent rename cannot misattribute custody.
#[derive(Debug, Clone)]
pub struct ReleaseArtifactAssertion {
    pub artifact_name: String,
    pub artifact_version: String,
    pub source_commit: String,
    pub target_triple: String,
    pub sha256: String,
    pub size_bytes: i64,
    pub computer_id: Uuid,
    pub holder_name: String,
    pub relative_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseArtifactRow {
    pub id: Uuid,
    pub artifact_name: String,
    pub artifact_version: String,
    pub source_commit: String,
    pub target_triple: String,
    pub sha256: String,
    pub size_bytes: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseArtifactCustodyRow {
    pub artifact_id: Uuid,
    pub computer_id: Uuid,
    pub holder_name_at_registration: String,
    pub relative_path: String,
    pub first_verified_at: DateTime<Utc>,
    pub last_verified_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseArtifactRegistrationOutcome {
    Registered,
    Verified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseArtifactRegistration {
    pub artifact: ReleaseArtifactRow,
    pub custody: ReleaseArtifactCustodyRow,
    pub outcome: ReleaseArtifactRegistrationOutcome,
}

/// One logical immutable bundle represented entirely by V291 file artifacts.
///
/// The manifest is itself one artifact and must appear exactly once in
/// `artifacts`. Every entry shares one release identity and custody holder.
#[derive(Debug, Clone)]
pub struct ReleaseArtifactBatchAssertion {
    pub manifest_artifact_name: String,
    pub artifacts: Vec<ReleaseArtifactAssertion>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseArtifactBatchRegistration {
    pub manifest_artifact_name: String,
    pub registrations: Vec<ReleaseArtifactRegistration>,
    pub origin_computer_id: Uuid,
    pub origin_holder: String,
    pub outcome: ReleaseArtifactRegistrationOutcome,
}

/// Pure comparison policy: a known digest is never rewritten.
pub fn decide_hash_write(existing: Option<&str>, computed: &str) -> HashWriteDecision {
    match existing {
        None => HashWriteDecision::Initialize,
        Some(stored) if constant_time_sha256_eq(stored, computed) => HashWriteDecision::Match,
        Some(_) => HashWriteDecision::Mismatch,
    }
}

/// Compare a verified digest or initialize a NULL digest with a fenced CAS.
pub async fn pg_compare_or_cas_model_library_sha256(
    pool: &PgPool,
    assertion: &ModelLibraryHashAssertion,
) -> Result<ModelLibraryHashCasOutcome> {
    validate_assertion(assertion)?;

    // The row lock makes Match/Mismatch final at this transaction's commit
    // boundary and keeps NULL comparison + initialization in one serialized
    // critical section. No comparison result is returned from a stale SELECT.
    let mut transaction = pool.begin().await?;
    let row = sqlx::query(MODEL_LIBRARY_HASH_SELECT_FOR_UPDATE_SQL)
        .bind(assertion.row_id)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or_else(|| DbError::NotFound(format!("model library row {}", assertion.row_id)))?;

    let worker_name: String = row.get("worker_name");
    let file_path: String = row.get("file_path");
    let size_bytes: i64 = row.get("size_bytes");
    let downloaded_at: DateTime<Utc> = row.get("downloaded_at");
    let existing: Option<String> = row.get("sha256");

    if worker_name != assertion.worker_name
        || file_path != assertion.file_path
        || size_bytes != assertion.size_bytes
        || downloaded_at != assertion.downloaded_at
    {
        transaction.commit().await?;
        return Ok(ModelLibraryHashCasOutcome::StaleIdentity);
    }

    match decide_hash_write(existing.as_deref(), &assertion.computed_sha256) {
        HashWriteDecision::Match => {
            transaction.commit().await?;
            return Ok(ModelLibraryHashCasOutcome::Match);
        }
        HashWriteDecision::Mismatch => {
            transaction.commit().await?;
            return Ok(ModelLibraryHashCasOutcome::Mismatch);
        }
        HashWriteDecision::Initialize => {}
    }

    let affected = sqlx::query(MODEL_LIBRARY_SHA256_CAS_SQL)
        .bind(assertion.row_id)
        .bind(&assertion.worker_name)
        .bind(&assertion.file_path)
        .bind(assertion.size_bytes)
        .bind(assertion.downloaded_at)
        .bind(&assertion.computed_sha256)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
    let outcome = classify_cas_rows(affected)?;
    transaction.commit().await?;
    Ok(outcome)
}

/// Register immutable artifact content and local custody in one transaction.
///
/// A repeated, byte-for-byte identical assertion refreshes only
/// `last_verified_at`. Any content, holder-name, or path disagreement aborts
/// the transaction. There is intentionally no force, rewrite, or delete twin.
pub async fn pg_register_release_artifact(
    pool: &PgPool,
    assertion: &ReleaseArtifactAssertion,
) -> Result<ReleaseArtifactRegistration> {
    validate_release_artifact_assertion(assertion)?;

    let mut transaction = pool.begin().await?;
    lock_release_identity(&mut transaction, assertion).await?;
    validate_release_holder(&mut transaction, assertion).await?;
    let registration =
        register_release_artifact_in_transaction(&mut transaction, assertion).await?;
    transaction.commit().await?;
    Ok(registration)
}

/// Atomically register or verify every file in one immutable runtime bundle.
///
/// V291 remains the authority: the canonical JSON manifest and each executable
/// or shared library are ordinary file artifacts. The batch API adds no
/// synthetic directory artifact or digest. It serializes all users of the
/// release identity, requires an exact artifact set, requires every custodian
/// to hold that complete set, and proves that every file has the same first
/// custodian before refreshing any verification timestamp.
pub async fn pg_register_release_artifact_batch(
    pool: &PgPool,
    batch: &ReleaseArtifactBatchAssertion,
) -> Result<ReleaseArtifactBatchRegistration> {
    let assertions = validate_release_artifact_batch(batch)?;
    let anchor = assertions
        .first()
        .expect("validated release artifact batch is non-empty");
    let expected_names: BTreeSet<_> = assertions
        .iter()
        .map(|assertion| assertion.artifact_name.clone())
        .collect();

    let mut transaction = pool.begin().await?;
    lock_release_identity(&mut transaction, anchor).await?;
    validate_release_holder(&mut transaction, anchor).await?;
    preflight_release_bundle_identity(&mut transaction, &assertions, &expected_names).await?;
    preflight_release_bundle_custody(&mut transaction, &assertions, &expected_names).await?;
    let existing_origin =
        preflight_release_bundle_origin(&mut transaction, anchor, &expected_names).await?;

    let mut registrations = Vec::with_capacity(assertions.len());
    for assertion in &assertions {
        registrations
            .push(register_release_artifact_in_transaction(&mut transaction, assertion).await?);
    }
    let outcome = if registrations
        .iter()
        .any(|registration| registration.outcome == ReleaseArtifactRegistrationOutcome::Registered)
    {
        ReleaseArtifactRegistrationOutcome::Registered
    } else {
        ReleaseArtifactRegistrationOutcome::Verified
    };
    let (origin_computer_id, origin_holder) =
        existing_origin.unwrap_or_else(|| (anchor.computer_id, anchor.holder_name.clone()));

    transaction.commit().await?;
    Ok(ReleaseArtifactBatchRegistration {
        manifest_artifact_name: batch.manifest_artifact_name.clone(),
        registrations,
        origin_computer_id,
        origin_holder,
        outcome,
    })
}

async fn lock_release_identity(
    transaction: &mut Transaction<'_, Postgres>,
    assertion: &ReleaseArtifactAssertion,
) -> Result<()> {
    let identity = format!(
        "{}\n{}\n{}",
        assertion.artifact_version, assertion.source_commit, assertion.target_triple
    );
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('ff.release-bundle'), hashtext($1))")
        .bind(identity)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

async fn validate_release_holder(
    transaction: &mut Transaction<'_, Postgres>,
    assertion: &ReleaseArtifactAssertion,
) -> Result<()> {
    let canonical_holder: Option<String> = sqlx::query_scalar(RELEASE_HOLDER_SELECT_FOR_UPDATE_SQL)
        .bind(assertion.computer_id)
        .fetch_optional(&mut **transaction)
        .await?;
    let canonical_holder = canonical_holder.ok_or_else(|| {
        DbError::ArtifactIntegrity(format!(
            "custody computer {} is not canonical",
            assertion.computer_id
        ))
    })?;
    if canonical_holder != assertion.holder_name {
        return Err(DbError::ArtifactIntegrity(format!(
            "custody holder changed during verification: expected {}, found {}",
            assertion.holder_name, canonical_holder
        )));
    }
    if !model_integrity_worker_allowed(&canonical_holder) {
        return Err(DbError::ArtifactIntegrity(
            "release artifact operations are forbidden on Vinny".to_string(),
        ));
    }
    Ok(())
}

async fn register_release_artifact_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    assertion: &ReleaseArtifactAssertion,
) -> Result<ReleaseArtifactRegistration> {
    let inserted_artifact = sqlx::query(RELEASE_ARTIFACT_INSERT_SQL)
        .bind(&assertion.artifact_name)
        .bind(&assertion.artifact_version)
        .bind(&assertion.source_commit)
        .bind(&assertion.target_triple)
        .bind(&assertion.sha256)
        .bind(assertion.size_bytes)
        .fetch_optional(&mut **transaction)
        .await?;
    let artifact_was_inserted = inserted_artifact.is_some();
    let artifact = match inserted_artifact {
        Some(row) => release_artifact_from_row(&row),
        None => {
            let row = sqlx::query(RELEASE_ARTIFACT_SELECT_FOR_UPDATE_SQL)
                .bind(&assertion.artifact_name)
                .bind(&assertion.artifact_version)
                .bind(&assertion.source_commit)
                .bind(&assertion.target_triple)
                .fetch_optional(&mut **transaction)
                .await?
                .ok_or_else(|| {
                    DbError::ArtifactIntegrity(
                        "release identity disappeared during compare-and-set".to_string(),
                    )
                })?;
            let artifact = release_artifact_from_row(&row);
            if !release_content_matches(&artifact, assertion) {
                return Err(DbError::ArtifactIntegrity(format!(
                    "release identity already exists with different content (stored sha256={}, size_bytes={})",
                    artifact.sha256, artifact.size_bytes
                )));
            }
            artifact
        }
    };

    let inserted_custody = sqlx::query(RELEASE_CUSTODY_INSERT_SQL)
        .bind(artifact.id)
        .bind(assertion.computer_id)
        .bind(&assertion.holder_name)
        .bind(&assertion.relative_path)
        .fetch_optional(&mut **transaction)
        .await?;
    let custody_was_inserted = inserted_custody.is_some();
    let custody = match inserted_custody {
        Some(row) => release_custody_from_row(&row),
        None => {
            let row = sqlx::query(RELEASE_CUSTODY_SELECT_FOR_UPDATE_SQL)
                .bind(artifact.id)
                .bind(assertion.computer_id)
                .fetch_optional(&mut **transaction)
                .await?
                .ok_or_else(|| {
                    DbError::ArtifactIntegrity(
                        "custody identity disappeared during compare-and-set".to_string(),
                    )
                })?;
            let existing = release_custody_from_row(&row);
            if !release_custody_matches(&existing, assertion) {
                return Err(DbError::ArtifactIntegrity(format!(
                    "custody identity already exists with different holder/path (stored holder={}, path={})",
                    existing.holder_name_at_registration, existing.relative_path
                )));
            }
            let row = sqlx::query(
                "UPDATE release_artifact_custody
                    SET last_verified_at = clock_timestamp()
                  WHERE artifact_id = $1 AND computer_id = $2
                RETURNING artifact_id, computer_id, holder_name_at_registration, relative_path,
                          first_verified_at, last_verified_at",
            )
            .bind(artifact.id)
            .bind(assertion.computer_id)
            .fetch_one(&mut **transaction)
            .await?;
            release_custody_from_row(&row)
        }
    };

    Ok(ReleaseArtifactRegistration {
        artifact,
        custody,
        outcome: if artifact_was_inserted || custody_was_inserted {
            ReleaseArtifactRegistrationOutcome::Registered
        } else {
            ReleaseArtifactRegistrationOutcome::Verified
        },
    })
}

fn validate_release_artifact_batch(
    batch: &ReleaseArtifactBatchAssertion,
) -> Result<Vec<&ReleaseArtifactAssertion>> {
    if batch.artifacts.is_empty() {
        return Err(DbError::ArtifactIntegrity(
            "release artifact batch must not be empty".to_string(),
        ));
    }
    if batch.manifest_artifact_name.is_empty() {
        return Err(DbError::ArtifactIntegrity(
            "release artifact batch must name its manifest artifact".to_string(),
        ));
    }

    let anchor = &batch.artifacts[0];
    let mut names = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut manifest_count = 0;
    for assertion in &batch.artifacts {
        validate_release_artifact_assertion(assertion)?;
        if assertion.artifact_version != anchor.artifact_version
            || assertion.source_commit != anchor.source_commit
            || assertion.target_triple != anchor.target_triple
            || assertion.computer_id != anchor.computer_id
            || assertion.holder_name != anchor.holder_name
        {
            return Err(DbError::ArtifactIntegrity(
                "every release bundle artifact must share version, source commit, target triple, computer, and holder"
                    .to_string(),
            ));
        }
        if !names.insert(assertion.artifact_name.clone()) {
            return Err(DbError::ArtifactIntegrity(format!(
                "duplicate release bundle artifact name {}",
                assertion.artifact_name
            )));
        }
        if !paths.insert(assertion.relative_path.clone()) {
            return Err(DbError::ArtifactIntegrity(format!(
                "duplicate release bundle relative path {}",
                assertion.relative_path
            )));
        }
        manifest_count += usize::from(assertion.artifact_name == batch.manifest_artifact_name);
    }
    if manifest_count != 1 {
        return Err(DbError::ArtifactIntegrity(format!(
            "release bundle manifest artifact {} must appear exactly once",
            batch.manifest_artifact_name
        )));
    }

    let mut assertions: Vec<_> = batch.artifacts.iter().collect();
    assertions.sort_by(|left, right| left.artifact_name.cmp(&right.artifact_name));
    Ok(assertions)
}

async fn preflight_release_bundle_identity(
    transaction: &mut Transaction<'_, Postgres>,
    assertions: &[&ReleaseArtifactAssertion],
    expected_names: &BTreeSet<String>,
) -> Result<()> {
    let anchor = assertions[0];
    let rows = sqlx::query(
        "SELECT id, artifact_name, artifact_version, source_commit, target_triple,
                sha256, size_bytes, created_at
           FROM release_artifacts
          WHERE artifact_version = $1 AND source_commit = $2 AND target_triple = $3
          ORDER BY artifact_name
          FOR UPDATE",
    )
    .bind(&anchor.artifact_version)
    .bind(&anchor.source_commit)
    .bind(&anchor.target_triple)
    .fetch_all(&mut **transaction)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }

    let stored: BTreeMap<_, _> = rows
        .iter()
        .map(|row| {
            let artifact = release_artifact_from_row(row);
            (artifact.artifact_name.clone(), artifact)
        })
        .collect();
    let stored_names: BTreeSet<_> = stored.keys().cloned().collect();
    if &stored_names != expected_names {
        return Err(DbError::ArtifactIntegrity(format!(
            "release bundle artifact set is partial or drifted: expected {expected_names:?}, stored {stored_names:?}"
        )));
    }
    for assertion in assertions {
        let existing = &stored[&assertion.artifact_name];
        if !release_content_matches(existing, assertion) {
            return Err(DbError::ArtifactIntegrity(format!(
                "release bundle artifact {} has different stored content",
                assertion.artifact_name
            )));
        }
    }
    Ok(())
}

async fn preflight_release_bundle_custody(
    transaction: &mut Transaction<'_, Postgres>,
    assertions: &[&ReleaseArtifactAssertion],
    expected_names: &BTreeSet<String>,
) -> Result<()> {
    let anchor = assertions[0];
    let rows = sqlx::query(
        "SELECT a.artifact_name, c.artifact_id, c.computer_id,
                c.holder_name_at_registration, c.relative_path,
                c.first_verified_at, c.last_verified_at
           FROM release_artifacts a
           JOIN release_artifact_custody c ON c.artifact_id = a.id
          WHERE a.artifact_version = $1 AND a.source_commit = $2 AND a.target_triple = $3
          ORDER BY c.computer_id, a.artifact_name
          FOR UPDATE OF c",
    )
    .bind(&anchor.artifact_version)
    .bind(&anchor.source_commit)
    .bind(&anchor.target_triple)
    .fetch_all(&mut **transaction)
    .await?;

    let expected_by_name: BTreeMap<_, _> = assertions
        .iter()
        .map(|assertion| (assertion.artifact_name.as_str(), *assertion))
        .collect();
    let mut names_by_computer: BTreeMap<Uuid, BTreeSet<String>> = BTreeMap::new();
    for row in &rows {
        names_by_computer
            .entry(row.get("computer_id"))
            .or_default()
            .insert(row.get("artifact_name"));
    }
    for (computer_id, names) in &names_by_computer {
        if names != expected_names {
            return Err(DbError::ArtifactIntegrity(format!(
                "release bundle custody is partial for computer {computer_id}: expected {expected_names:?}, stored {names:?}"
            )));
        }
    }

    for row in rows
        .iter()
        .filter(|row| row.get::<Uuid, _>("computer_id") == anchor.computer_id)
    {
        let artifact_name: String = row.get("artifact_name");
        let assertion = expected_by_name[artifact_name.as_str()];
        let custody = release_custody_from_row(row);
        if !release_custody_matches(&custody, assertion) {
            return Err(DbError::ArtifactIntegrity(format!(
                "release bundle custody for {artifact_name} has different stored holder/path"
            )));
        }
    }
    Ok(())
}

async fn preflight_release_bundle_origin(
    transaction: &mut Transaction<'_, Postgres>,
    anchor: &ReleaseArtifactAssertion,
    expected_names: &BTreeSet<String>,
) -> Result<Option<(Uuid, String)>> {
    let rows = sqlx::query(
        "SELECT a.artifact_name, c.computer_id, c.holder_name_at_registration,
                c.first_verified_at
           FROM release_artifacts a
           JOIN release_artifact_custody c ON c.artifact_id = a.id
          WHERE a.artifact_version = $1 AND a.source_commit = $2 AND a.target_triple = $3
          ORDER BY a.artifact_name, c.first_verified_at, c.computer_id",
    )
    .bind(&anchor.artifact_version)
    .bind(&anchor.source_commit)
    .bind(&anchor.target_triple)
    .fetch_all(&mut **transaction)
    .await?;
    if rows.is_empty() {
        return Ok(None);
    }

    let mut by_artifact: BTreeMap<String, Vec<(Uuid, String, DateTime<Utc>)>> = BTreeMap::new();
    for row in rows {
        by_artifact
            .entry(row.get("artifact_name"))
            .or_default()
            .push((
                row.get("computer_id"),
                row.get("holder_name_at_registration"),
                row.get("first_verified_at"),
            ));
    }
    let stored_names: BTreeSet<_> = by_artifact.keys().cloned().collect();
    if &stored_names != expected_names {
        return Err(DbError::ArtifactIntegrity(
            "release bundle origin cannot be proven for every exact artifact".to_string(),
        ));
    }

    let mut common_origin: Option<(Uuid, String)> = None;
    for (artifact_name, custodians) in by_artifact {
        let earliest = custodians[0].2;
        let earliest_rows: Vec<_> = custodians
            .iter()
            .filter(|(_, _, first_verified_at)| *first_verified_at == earliest)
            .collect();
        if earliest_rows.len() != 1 {
            return Err(DbError::ArtifactIntegrity(format!(
                "release bundle artifact {artifact_name} has ambiguous first custody"
            )));
        }
        let candidate = (earliest_rows[0].0, earliest_rows[0].1.clone());
        match &common_origin {
            None => common_origin = Some(candidate),
            Some(origin) if origin == &candidate => {}
            Some(_) => {
                return Err(DbError::ArtifactIntegrity(
                    "release bundle artifacts do not share one custody origin".to_string(),
                ));
            }
        }
    }
    Ok(common_origin)
}

pub async fn pg_get_release_artifact(
    pool: &PgPool,
    artifact_name: &str,
    artifact_version: &str,
    source_commit: &str,
    target_triple: &str,
) -> Result<Option<ReleaseArtifactRow>> {
    let row = sqlx::query(
        "SELECT id, artifact_name, artifact_version, source_commit, target_triple,
                sha256, size_bytes, created_at
           FROM release_artifacts
          WHERE artifact_name = $1 AND artifact_version = $2
            AND source_commit = $3 AND target_triple = $4",
    )
    .bind(artifact_name)
    .bind(artifact_version)
    .bind(source_commit)
    .bind(target_triple)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(release_artifact_from_row))
}

pub async fn pg_list_release_artifacts(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<ReleaseArtifactRow>> {
    let rows = sqlx::query(
        "SELECT id, artifact_name, artifact_version, source_commit, target_triple,
                sha256, size_bytes, created_at
           FROM release_artifacts
          ORDER BY created_at DESC, artifact_name, target_triple
          LIMIT $1",
    )
    .bind(limit.clamp(1, 1000))
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(release_artifact_from_row).collect())
}

pub async fn pg_list_release_artifact_custody(
    pool: &PgPool,
    artifact_id: Uuid,
) -> Result<Vec<ReleaseArtifactCustodyRow>> {
    let rows = sqlx::query(
        "SELECT artifact_id, computer_id, holder_name_at_registration, relative_path,
                first_verified_at, last_verified_at
           FROM release_artifact_custody
          WHERE artifact_id = $1
          ORDER BY holder_name_at_registration, computer_id",
    )
    .bind(artifact_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(release_custody_from_row).collect())
}

fn release_artifact_from_row(row: &sqlx::postgres::PgRow) -> ReleaseArtifactRow {
    ReleaseArtifactRow {
        id: row.get("id"),
        artifact_name: row.get("artifact_name"),
        artifact_version: row.get("artifact_version"),
        source_commit: row.get("source_commit"),
        target_triple: row.get("target_triple"),
        sha256: row.get("sha256"),
        size_bytes: row.get("size_bytes"),
        created_at: row.get("created_at"),
    }
}

fn release_custody_from_row(row: &sqlx::postgres::PgRow) -> ReleaseArtifactCustodyRow {
    ReleaseArtifactCustodyRow {
        artifact_id: row.get("artifact_id"),
        computer_id: row.get("computer_id"),
        holder_name_at_registration: row.get("holder_name_at_registration"),
        relative_path: row.get("relative_path"),
        first_verified_at: row.get("first_verified_at"),
        last_verified_at: row.get("last_verified_at"),
    }
}

fn release_content_matches(
    existing: &ReleaseArtifactRow,
    assertion: &ReleaseArtifactAssertion,
) -> bool {
    existing.artifact_name == assertion.artifact_name
        && existing.artifact_version == assertion.artifact_version
        && existing.source_commit == assertion.source_commit
        && existing.target_triple == assertion.target_triple
        && constant_time_sha256_eq(&existing.sha256, &assertion.sha256)
        && existing.size_bytes == assertion.size_bytes
}

fn release_custody_matches(
    existing: &ReleaseArtifactCustodyRow,
    assertion: &ReleaseArtifactAssertion,
) -> bool {
    existing.computer_id == assertion.computer_id
        && existing.holder_name_at_registration == assertion.holder_name
        && existing.relative_path == assertion.relative_path
}

fn validate_release_artifact_assertion(assertion: &ReleaseArtifactAssertion) -> Result<()> {
    let canonical_token = |value: &str| {
        !value.is_empty()
            && value.len() <= 128
            && (value.as_bytes()[0].is_ascii_lowercase() || value.as_bytes()[0].is_ascii_digit())
    };
    let token_tail = |value: &str| {
        value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._+-".contains(&byte)
        })
    };
    if !canonical_token(&assertion.artifact_name) || !token_tail(&assertion.artifact_name) {
        return Err(DbError::ArtifactIntegrity(
            "artifact name must be a canonical lowercase token".to_string(),
        ));
    }
    if !canonical_token(&assertion.artifact_version) || !token_tail(&assertion.artifact_version) {
        return Err(DbError::ArtifactIntegrity(
            "artifact version must be a canonical lowercase token".to_string(),
        ));
    }
    if !is_lower_hex(&assertion.source_commit, 40) {
        return Err(DbError::ArtifactIntegrity(
            "source commit must be exactly 40 lowercase hexadecimal characters".to_string(),
        ));
    }
    if !is_canonical_target_triple(&assertion.target_triple) {
        return Err(DbError::ArtifactIntegrity(
            "target triple must contain 3-5 canonical lowercase components".to_string(),
        ));
    }
    if !is_lower_hex(&assertion.sha256, 64) {
        return Err(DbError::ArtifactIntegrity(
            "sha256 must be exactly 64 lowercase hexadecimal characters".to_string(),
        ));
    }
    if assertion.size_bytes <= 0 {
        return Err(DbError::ArtifactIntegrity(
            "artifact size must be positive".to_string(),
        ));
    }
    if !is_canonical_holder_name(&assertion.holder_name)
        || !model_integrity_worker_allowed(&assertion.holder_name)
    {
        return Err(DbError::ArtifactIntegrity(
            "custody holder is non-canonical or excluded".to_string(),
        ));
    }
    if !is_normal_relative_path(&assertion.relative_path) {
        return Err(DbError::ArtifactIntegrity(
            "custody path must be a normal UTF-8 path relative to the release root".to_string(),
        ));
    }
    Ok(())
}

fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_canonical_target_triple(value: &str) -> bool {
    if value.is_empty() || value.len() > 128 {
        return false;
    }
    let components: Vec<_> = value.split('-').collect();
    (3..=5).contains(&components.len())
        && components.iter().all(|component| {
            !component.is_empty()
                && component.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || byte == b'_'
                        || byte == b'.'
                })
        })
}

fn is_canonical_holder_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && value.as_bytes()[0] != b'-'
        && value.as_bytes()[value.len() - 1] != b'-'
}

fn is_normal_relative_path(value: &str) -> bool {
    use std::path::{Component, Path};

    !value.is_empty()
        && !value.contains('\\')
        && !value.contains("//")
        && !value.ends_with('/')
        && !Path::new(value).is_absolute()
        && Path::new(value)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn validate_assertion(assertion: &ModelLibraryHashAssertion) -> Result<()> {
    if !model_integrity_worker_allowed(&assertion.worker_name) {
        return Err(DbError::NotFound(
            "model integrity operations are forbidden on Vinny".to_string(),
        ));
    }
    // Live fleet_model_library has only the legacy `sha256` column and no
    // algorithm/kind evidence. Persisting ff-dir-v1 there would be ambiguous,
    // so directory manifests remain comparison/reporting-only until an
    // explicit schema field is designed and coordinated.
    if assertion.artifact_kind != ModelArtifactKind::File || assertion.digest_algorithm != "sha256"
    {
        return Err(DbError::NotFound(
            "fleet_model_library.sha256 accepts only file sha256 digests; directory manifests require explicit algorithm and artifact-kind storage"
                .to_string(),
        ));
    }
    if parse_sha256_hex(&assertion.computed_sha256).is_none()
        || assertion
            .computed_sha256
            .bytes()
            .any(|byte| byte.is_ascii_uppercase())
    {
        return Err(DbError::NotFound(
            "computed sha256 must be exactly 64 lowercase hexadecimal characters".to_string(),
        ));
    }
    Ok(())
}

fn classify_cas_rows(affected: u64) -> Result<ModelLibraryHashCasOutcome> {
    match affected {
        1 => Ok(ModelLibraryHashCasOutcome::Initialized),
        0 => Ok(ModelLibraryHashCasOutcome::LostRace),
        _ => Err(DbError::Postgres(sqlx::Error::Protocol(format!(
            "model hash CAS unexpectedly changed {affected} rows"
        )))),
    }
}

#[cfg(test)]
mod tests {
    use std::env;

    use super::*;
    use sqlx::postgres::PgPoolOptions;

    const HASH: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn assertion(kind: ModelArtifactKind, algorithm: &str) -> ModelLibraryHashAssertion {
        ModelLibraryHashAssertion {
            row_id: Uuid::nil(),
            worker_name: "adele".to_string(),
            file_path: "/models/test".to_string(),
            size_bytes: 3,
            downloaded_at: DateTime::<Utc>::UNIX_EPOCH,
            artifact_kind: kind,
            digest_algorithm: algorithm.to_string(),
            computed_sha256: HASH.to_string(),
        }
    }

    fn release_assertion(computer_id: Uuid, holder: &str) -> ReleaseArtifactAssertion {
        ReleaseArtifactAssertion {
            artifact_name: "ff".to_string(),
            artifact_version: "2026.8.5_1".to_string(),
            source_commit: "6dc4086b7217cb8c2ccc1945b1e1f3213b9b1941".to_string(),
            target_triple: "aarch64-unknown-linux-gnu".to_string(),
            sha256: HASH.to_string(),
            size_bytes: 3,
            computer_id,
            holder_name: holder.to_string(),
            relative_path: "ff-6dc4086b-aarch64/artifact/ff".to_string(),
        }
    }

    fn release_batch(computer_id: Uuid, holder: &str) -> ReleaseArtifactBatchAssertion {
        let mut manifest = release_assertion(computer_id, holder);
        manifest.artifact_name = "llama-runtime-manifest".to_string();
        manifest.relative_path = "logan-runtime/runtime-manifest.json".to_string();
        let mut library = release_assertion(computer_id, holder);
        library.artifact_name = "libllama".to_string();
        library.relative_path = "logan-runtime/libllama.so".to_string();
        library.sha256 = "0".repeat(64);
        let mut server = release_assertion(computer_id, holder);
        server.artifact_name = "llama-server".to_string();
        server.relative_path = "logan-runtime/llama-server".to_string();
        server.sha256 = "1".repeat(64);
        ReleaseArtifactBatchAssertion {
            manifest_artifact_name: manifest.artifact_name.clone(),
            artifacts: vec![manifest, library, server],
        }
    }

    fn artifact_test_db_url() -> Option<String> {
        env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| env::var("FORGEFLEET_DATABASE_URL"))
            .ok()
    }

    async fn create_fresh_artifact_temp_db() -> Option<(PgPool, PgPool, String)> {
        let base_url = artifact_test_db_url()?;
        let (prefix, _) = base_url.rsplit_once('/')?;
        let db_name = format!("ff_artifact_v291_{}", Uuid::new_v4().simple());
        let admin_url = format!("{prefix}/postgres");
        let database_url = format!("{prefix}/{db_name}");

        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&admin_url)
            .await
            .ok()?;
        if sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
            .execute(&admin)
            .await
            .is_err()
        {
            admin.close().await;
            return None;
        }
        let pool = match PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url)
            .await
        {
            Ok(pool) => pool,
            Err(_) => {
                let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db_name}\""))
                    .execute(&admin)
                    .await;
                admin.close().await;
                return None;
            }
        };
        Some((admin, pool, db_name))
    }

    async fn drop_artifact_temp_db(admin: PgPool, pool: PgPool, db_name: &str) {
        pool.close().await;
        sqlx::query(
            "SELECT pg_terminate_backend(pid)
               FROM pg_stat_activity
              WHERE datname = $1
                AND pid <> pg_backend_pid()",
        )
        .bind(db_name)
        .execute(&admin)
        .await
        .expect("terminate artifact temp database sessions");
        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db_name}\""))
            .execute(&admin)
            .await
            .expect("drop artifact temp database");
        admin.close().await;
    }

    #[test]
    fn null_initializes_but_non_null_never_rewrites() {
        assert_eq!(decide_hash_write(None, HASH), HashWriteDecision::Initialize);
        assert_eq!(
            decide_hash_write(Some(HASH), HASH),
            HashWriteDecision::Match
        );
        assert_eq!(
            decide_hash_write(Some(&"0".repeat(64)), HASH),
            HashWriteDecision::Mismatch
        );
    }

    #[test]
    fn cas_sql_is_fenced_and_null_only() {
        for guard in [
            "WHERE id = $1",
            "worker_name = $2",
            "file_path = $3",
            "size_bytes = $4",
            "downloaded_at = $5",
            "sha256 IS NULL",
        ] {
            assert!(
                MODEL_LIBRARY_SHA256_CAS_SQL.contains(guard),
                "missing {guard}"
            );
        }
        assert!(!MODEL_LIBRARY_SHA256_CAS_SQL.contains("COALESCE"));
    }

    #[test]
    fn comparison_query_locks_the_row_for_transactional_outcomes() {
        assert!(MODEL_LIBRARY_HASH_SELECT_FOR_UPDATE_SQL.contains("WHERE id = $1"));
        assert!(MODEL_LIBRARY_HASH_SELECT_FOR_UPDATE_SQL.contains("FOR UPDATE"));
        assert!(!MODEL_LIBRARY_HASH_SELECT_FOR_UPDATE_SQL.contains("SKIP LOCKED"));
    }

    #[test]
    fn directory_digest_persistence_fails_closed_without_schema_evidence() {
        let directory = assertion(ModelArtifactKind::Directory, "ff-dir-v1+sha256");
        assert!(validate_assertion(&directory).is_err());
        assert!(validate_assertion(&assertion(ModelArtifactKind::File, "sha256")).is_ok());
        assert!(
            validate_assertion(&assertion(ModelArtifactKind::File, "ff-dir-v1+sha256")).is_err()
        );
    }

    #[test]
    fn vinny_exclusion_precedes_repository_access() {
        assert!(!model_integrity_worker_allowed("vinny"));
        assert!(!model_integrity_worker_allowed("VINNY"));
    }

    #[test]
    fn zero_row_cas_is_a_lost_race_not_a_success() {
        assert_eq!(
            classify_cas_rows(0).unwrap(),
            ModelLibraryHashCasOutcome::LostRace
        );
        assert_eq!(
            classify_cas_rows(1).unwrap(),
            ModelLibraryHashCasOutcome::Initialized
        );
        assert!(classify_cas_rows(2).is_err());
    }

    #[test]
    fn release_assertion_validation_fails_closed() {
        let valid = release_assertion(Uuid::new_v4(), "thalia");
        validate_release_artifact_assertion(&valid).unwrap();

        let mut invalid = valid.clone();
        invalid.sha256 = "A".repeat(64);
        assert!(validate_release_artifact_assertion(&invalid).is_err());
        let mut invalid = valid.clone();
        invalid.source_commit = "6dc4086b".to_string();
        assert!(validate_release_artifact_assertion(&invalid).is_err());
        let mut invalid = valid.clone();
        invalid.target_triple = "linux".to_string();
        assert!(validate_release_artifact_assertion(&invalid).is_err());
        let mut invalid = valid.clone();
        invalid.size_bytes = 0;
        assert!(validate_release_artifact_assertion(&invalid).is_err());
        let mut invalid = valid.clone();
        invalid.holder_name = "VINNY".to_string();
        assert!(validate_release_artifact_assertion(&invalid).is_err());
        for path in [
            "/absolute/ff",
            "../ff",
            "build/../ff",
            "build\\ff",
            "build//ff",
        ] {
            let mut invalid = valid.clone();
            invalid.relative_path = path.to_string();
            assert!(
                validate_release_artifact_assertion(&invalid).is_err(),
                "accepted spoofable path {path}"
            );
        }
    }

    #[test]
    fn release_content_and_custody_are_compare_only() {
        let assertion = release_assertion(Uuid::new_v4(), "thalia");
        let artifact = ReleaseArtifactRow {
            id: Uuid::new_v4(),
            artifact_name: assertion.artifact_name.clone(),
            artifact_version: assertion.artifact_version.clone(),
            source_commit: assertion.source_commit.clone(),
            target_triple: assertion.target_triple.clone(),
            sha256: assertion.sha256.clone(),
            size_bytes: assertion.size_bytes,
            created_at: DateTime::<Utc>::UNIX_EPOCH,
        };
        assert!(release_content_matches(&artifact, &assertion));
        let mut changed = assertion.clone();
        changed.sha256 = "0".repeat(64);
        assert!(!release_content_matches(&artifact, &changed));
        let mut changed = assertion.clone();
        changed.size_bytes += 1;
        assert!(!release_content_matches(&artifact, &changed));

        let custody = ReleaseArtifactCustodyRow {
            artifact_id: artifact.id,
            computer_id: assertion.computer_id,
            holder_name_at_registration: assertion.holder_name.clone(),
            relative_path: assertion.relative_path.clone(),
            first_verified_at: DateTime::<Utc>::UNIX_EPOCH,
            last_verified_at: DateTime::<Utc>::UNIX_EPOCH,
        };
        assert!(release_custody_matches(&custody, &assertion));
        let mut changed = assertion;
        changed.relative_path = "different/artifact/ff".to_string();
        assert!(!release_custody_matches(&custody, &changed));
    }

    #[test]
    fn release_batch_requires_one_complete_shared_identity() {
        let computer_id = Uuid::new_v4();
        let valid = release_batch(computer_id, "logan");
        let sorted = validate_release_artifact_batch(&valid).unwrap();
        assert_eq!(sorted.len(), 3);
        assert_eq!(sorted[0].artifact_name, "libllama");

        let mut invalid = valid.clone();
        invalid.artifacts.clear();
        assert!(validate_release_artifact_batch(&invalid).is_err());

        let mut invalid = valid.clone();
        invalid.manifest_artifact_name = "missing-manifest".to_string();
        assert!(validate_release_artifact_batch(&invalid).is_err());

        let mut invalid = valid.clone();
        invalid.artifacts[2].artifact_name = invalid.artifacts[1].artifact_name.clone();
        assert!(validate_release_artifact_batch(&invalid).is_err());

        let mut invalid = valid.clone();
        invalid.artifacts[2].relative_path = invalid.artifacts[1].relative_path.clone();
        assert!(validate_release_artifact_batch(&invalid).is_err());

        let mut invalid = valid;
        invalid.artifacts[2].source_commit = "2".repeat(40);
        assert!(validate_release_artifact_batch(&invalid).is_err());
    }

    #[test]
    fn release_sql_has_no_content_rewrite_or_delete_surface() {
        assert!(RELEASE_ARTIFACT_INSERT_SQL.contains("ON CONFLICT"));
        assert!(RELEASE_ARTIFACT_INSERT_SQL.contains("DO NOTHING"));
        assert!(!RELEASE_ARTIFACT_INSERT_SQL.contains("DO UPDATE"));
        assert!(!RELEASE_ARTIFACT_INSERT_SQL.contains("DELETE"));
        assert!(RELEASE_ARTIFACT_SELECT_FOR_UPDATE_SQL.contains("FOR UPDATE"));
        assert!(RELEASE_CUSTODY_INSERT_SQL.contains("DO NOTHING"));
        assert!(!RELEASE_CUSTODY_INSERT_SQL.contains("DO UPDATE"));
        assert!(RELEASE_CUSTODY_SELECT_FOR_UPDATE_SQL.contains("FOR UPDATE"));
        assert!(RELEASE_HOLDER_SELECT_FOR_UPDATE_SQL.contains("WHERE id = $1"));
        assert!(RELEASE_HOLDER_SELECT_FOR_UPDATE_SQL.contains("FOR UPDATE"));
        assert!(!RELEASE_HOLDER_SELECT_FOR_UPDATE_SQL.contains("KEY SHARE"));
    }

    /// Integration test against a freshly-created disposable database. The
    /// configured fleet URL is used only to reach the server's `postgres`
    /// administration database; authority tables and rows are never created in
    /// the configured database itself.
    #[tokio::test]
    async fn release_registration_converges_and_rejects_mismatch() {
        let Some((admin, pool, db_name)) = create_fresh_artifact_temp_db().await else {
            eprintln!(
                "skipping release artifact integration test: no usable \
                 FORGEFLEET_POSTGRES_URL/FORGEFLEET_DATABASE_URL temp-db authority"
            );
            return;
        };
        eprintln!("created disposable artifact test database {db_name}");
        sqlx::raw_sql(
            "CREATE TABLE computers (
                id UUID PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                primary_ip TEXT NOT NULL,
                all_ips JSONB NOT NULL DEFAULT '[]',
                os_family TEXT NOT NULL,
                ssh_user TEXT NOT NULL
             );",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(crate::schema::SCHEMA_V291_RELEASE_ARTIFACT_CUSTODY)
            .execute(&pool)
            .await
            .unwrap();

        let suffix = Uuid::new_v4().simple().to_string();
        let holder_a = format!("a{}", &suffix[..10]);
        let holder_b = format!("b{}", &suffix[..10]);
        let holder_c = format!("c{}", &suffix[..10]);
        let computer_a = Uuid::new_v4();
        let computer_b = Uuid::new_v4();
        let computer_c = Uuid::new_v4();
        for (id, name, ip) in [
            (computer_a, holder_a.as_str(), "192.0.2.10"),
            (computer_b, holder_b.as_str(), "192.0.2.11"),
            (computer_c, holder_c.as_str(), "192.0.2.12"),
        ] {
            sqlx::query(
                "INSERT INTO computers (id, name, primary_ip, all_ips, os_family, ssh_user)
                 VALUES ($1, $2, $3, '[]'::jsonb, 'linux-ubuntu', 'test')",
            )
            .bind(id)
            .bind(name)
            .bind(ip)
            .execute(&pool)
            .await
            .unwrap();
        }

        let mut assertion_a = release_assertion(computer_a, &holder_a);
        assertion_a.artifact_version = suffix.clone();
        let mut assertion_b = release_assertion(computer_b, &holder_b);
        assertion_b.artifact_version = suffix.clone();
        let (registered_a, registered_b) = tokio::join!(
            pg_register_release_artifact(&pool, &assertion_a),
            pg_register_release_artifact(&pool, &assertion_b),
        );
        let registered_a = registered_a.unwrap();
        let registered_b = registered_b.unwrap();
        assert_eq!(registered_a.artifact.id, registered_b.artifact.id);
        assert_eq!(
            pg_list_release_artifact_custody(&pool, registered_a.artifact.id)
                .await
                .unwrap()
                .len(),
            2
        );

        let verified = pg_register_release_artifact(&pool, &assertion_a)
            .await
            .unwrap();
        assert_eq!(
            verified.outcome,
            ReleaseArtifactRegistrationOutcome::Verified
        );
        assert!(verified.custody.last_verified_at >= verified.custody.first_verified_at);

        let mut wrong_digest = assertion_a.clone();
        wrong_digest.sha256 = "0".repeat(64);
        assert!(
            pg_register_release_artifact(&pool, &wrong_digest)
                .await
                .is_err()
        );
        let mut wrong_path = assertion_a.clone();
        wrong_path.relative_path = "different/artifact/ff".to_string();
        assert!(
            pg_register_release_artifact(&pool, &wrong_path)
                .await
                .is_err()
        );
        let stored = pg_get_release_artifact(
            &pool,
            &assertion_a.artifact_name,
            &assertion_a.artifact_version,
            &assertion_a.source_commit,
            &assertion_a.target_triple,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(stored.sha256, HASH);
        assert_eq!(stored.size_bytes, 3);
        let listed = pg_list_release_artifacts(&pool, 10).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0], stored);

        assert!(
            sqlx::query("UPDATE release_artifacts SET sha256 = $2 WHERE id = $1")
                .bind(stored.id)
                .bind("0".repeat(64))
                .execute(&pool)
                .await
                .is_err(),
            "database trigger must reject direct content mutation"
        );
        assert!(
            sqlx::query("DELETE FROM release_artifact_custody WHERE artifact_id = $1")
                .bind(stored.id)
                .execute(&pool)
                .await
                .is_err(),
            "database trigger must reject custody deletion"
        );

        let set_batch_version = |batch: &mut ReleaseArtifactBatchAssertion, version: &str| {
            for artifact in &mut batch.artifacts {
                artifact.artifact_version = version.to_string();
            }
        };

        let mut bundle_a = release_batch(computer_a, &holder_a);
        let bundle_version = format!("{suffix}_bundle");
        set_batch_version(&mut bundle_a, &bundle_version);
        let registered = pg_register_release_artifact_batch(&pool, &bundle_a)
            .await
            .unwrap();
        assert_eq!(
            registered.outcome,
            ReleaseArtifactRegistrationOutcome::Registered
        );
        assert_eq!(registered.origin_computer_id, computer_a);
        assert_eq!(registered.origin_holder, holder_a);
        assert_eq!(registered.registrations.len(), 3);

        let replay = pg_register_release_artifact_batch(&pool, &bundle_a)
            .await
            .unwrap();
        assert_eq!(replay.outcome, ReleaseArtifactRegistrationOutcome::Verified);
        assert_eq!(replay.origin_computer_id, computer_a);

        let mut bundle_b = release_batch(computer_b, &holder_b);
        set_batch_version(&mut bundle_b, &bundle_version);
        let replicated = pg_register_release_artifact_batch(&pool, &bundle_b)
            .await
            .unwrap();
        assert_eq!(
            replicated.outcome,
            ReleaseArtifactRegistrationOutcome::Registered
        );
        assert_eq!(replicated.origin_computer_id, computer_a);
        assert_eq!(replicated.origin_holder, holder_a);

        let mut drifted = bundle_a.clone();
        drifted.artifacts[1].sha256 = "f".repeat(64);
        assert!(
            pg_register_release_artifact_batch(&pool, &drifted)
                .await
                .is_err()
        );
        let mut drifted = bundle_a.clone();
        drifted.artifacts[1].relative_path = "logan-runtime/other.so".to_string();
        assert!(
            pg_register_release_artifact_batch(&pool, &drifted)
                .await
                .is_err()
        );

        let mut partial_identity = release_batch(computer_a, &holder_a);
        let partial_identity_version = format!("{suffix}_partial_identity");
        set_batch_version(&mut partial_identity, &partial_identity_version);
        pg_register_release_artifact(&pool, &partial_identity.artifacts[0])
            .await
            .unwrap();
        assert!(
            pg_register_release_artifact_batch(&pool, &partial_identity)
                .await
                .is_err()
        );
        let partial_identity_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM release_artifacts WHERE artifact_version = $1",
        )
        .bind(&partial_identity_version)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(partial_identity_count, 1);

        let mut complete_a = release_batch(computer_a, &holder_a);
        let partial_custody_version = format!("{suffix}_partial_custody");
        set_batch_version(&mut complete_a, &partial_custody_version);
        pg_register_release_artifact_batch(&pool, &complete_a)
            .await
            .unwrap();
        let mut partial_b = release_batch(computer_b, &holder_b);
        set_batch_version(&mut partial_b, &partial_custody_version);
        pg_register_release_artifact(&pool, &partial_b.artifacts[0])
            .await
            .unwrap();
        assert!(
            pg_register_release_artifact_batch(&pool, &partial_b)
                .await
                .is_err()
        );

        let mixed_origin_version = format!("{suffix}_mixed_origin");
        let mut origin_a = release_batch(computer_a, &holder_a);
        let mut origin_b = release_batch(computer_b, &holder_b);
        set_batch_version(&mut origin_a, &mixed_origin_version);
        set_batch_version(&mut origin_b, &mixed_origin_version);
        for (index, assertion) in origin_a.artifacts.iter().enumerate() {
            let first = if index == 1 {
                &origin_b.artifacts[index]
            } else {
                assertion
            };
            pg_register_release_artifact(&pool, first).await.unwrap();
        }
        for assertion in &origin_a.artifacts {
            if assertion.artifact_name == origin_a.artifacts[1].artifact_name {
                pg_register_release_artifact(&pool, assertion)
                    .await
                    .unwrap();
            }
        }
        for (index, assertion) in origin_b.artifacts.iter().enumerate() {
            if index != 1 {
                pg_register_release_artifact(&pool, assertion)
                    .await
                    .unwrap();
            }
        }
        assert!(
            pg_register_release_artifact_batch(&pool, &origin_a)
                .await
                .is_err()
        );

        let mut atomic = release_batch(computer_c, &holder_c);
        let atomic_version = format!("{suffix}_atomic");
        set_batch_version(&mut atomic, &atomic_version);
        for artifact in &mut atomic.artifacts {
            artifact.relative_path = format!("atomic/{}", artifact.artifact_name);
        }
        let collision_path = atomic
            .artifacts
            .iter()
            .find(|artifact| artifact.artifact_name == "llama-server")
            .unwrap()
            .relative_path
            .clone();
        let mut collision = release_assertion(computer_c, &holder_c);
        collision.artifact_name = "unrelated-collision".to_string();
        collision.artifact_version = format!("{suffix}_unrelated");
        collision.relative_path = collision_path;
        pg_register_release_artifact(&pool, &collision)
            .await
            .unwrap();
        assert!(
            pg_register_release_artifact_batch(&pool, &atomic)
                .await
                .is_err()
        );
        let atomic_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM release_artifacts WHERE artifact_version = $1",
        )
        .bind(&atomic_version)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            atomic_count, 0,
            "late failure must roll back the whole batch"
        );

        drop_artifact_temp_db(admin, pool, &db_name).await;
        eprintln!("dropped disposable artifact test database {db_name}");
    }
}
