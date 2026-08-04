//! Compare-or-CAS persistence for verified model-library hashes.
//!
//! Existing non-NULL hashes are comparison-only. A NULL hash is initialized
//! with one CAS constrained by row id, owner, path, size, and `downloaded_at`.
//! The assertion must come from a freshly completed filesystem verifier final
//! identity pass and be submitted without caching/replay. This transaction
//! fences database identity, but no userspace API can make the preceding
//! filesystem scan and PostgreSQL commit one atomic snapshot. Directory
//! manifests fail closed until the live schema records algorithm and kind.

use chrono::{DateTime, Utc};
use ff_core::model_integrity::{
    ModelArtifactKind, constant_time_sha256_eq, model_integrity_worker_allowed, parse_sha256_hex,
};
use sqlx::Row;
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
    use super::*;

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
}
