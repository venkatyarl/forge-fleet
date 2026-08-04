//! Scheduled backup **restore-drill** — verifies that one exact Postgres or
//! FalkorDB backup is actually *restorable*, not merely present on disk.
//!
//! Motivation: on 2026-04-18 a docker-compose consolidation wiped the fleet
//! metadata DB. Backups existed in principle, but nothing ever proved they
//! could be decrypted, extracted, and loaded — a "backup" that has never been
//! test-restored is a liability, not a safety net. The
//! [`crate::ha::backup::BackupOrchestrator`] produces `pg_basebackup -Ft -z`
//! archives every 4h; this tick (daily, leader-gated) freezes the newest backup
//! ID once, then takes only that exact artifact through the restore path and
//! records the outcome in the preallocated `backup_drills` run row.
//!
//! What "restorable" means here, for a `pg_basebackup -Ft -z` archive (a
//! *physical* cluster snapshot — NOT a logical `pg_dump`, so it can't be
//! `pg_restore`'d into a scratch DB):
//!   1. the `.age` file exists on disk and is non-zero,
//!   2. its SHA-256 matches the `backups.checksum_sha256` recorded at write
//!      time (no bit-rot / truncated rsync),
//!   3. it decrypts with the fleet's `backup_encryption_privkey`,
//!   4. the plaintext `tar.gz` extracts cleanly, and
//!   5. the extracted tree is a *structurally complete* `PGDATA` — it contains
//!      `PG_VERSION` and `global/pg_control`. (If a `backup_manifest` and the
//!      `pg_verifybackup` tool are present, we additionally run it for
//!      cryptographic per-file validation), and
//!   6. that PGDATA starts as PostgreSQL 16 in a resource-bounded Docker
//!      container with no network or host ports, reaches readiness, and serves
//!      an application read from `fleet_nodes`.
//!
//! This is a genuine restore verification: it would have caught a 0-byte stub,
//! a missing decryption key, a corrupt/truncated archive, or a malformed
//! cluster snapshot — every failure mode that turns a "backup" into nothing.
//!
//! Design notes (mirrors [`crate::db_integrity::AmcheckTick`]):
//!   - **Leader-gated on every fire** via
//!     [`ff_db::leader_state::pg_get_current_leader`]; safe to spawn on every
//!     daemon (no-ops on followers).
//!   - **Alert-only.** A failed drill (or "no successful drill in
//!     [`STALE_DRILL_DAYS`]") fires the `backup_restore_drill_failed` policy
//!     seeded in migration V130, dispatched immediately (never `pending`).
//!   - **Self-cleaning.** Decrypt + extract happen under a unique temp dir that
//!     is removed on every exit path, success or failure.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use sqlx::{PgPool, Postgres, pool::PoolConnection};
use sysinfo::Disks;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// How often the drill runs. Backups land every 4h; a daily restore proof is
/// plenty and keeps the (small) extract cost off the hot path.
pub const DRILL_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Leader heartbeat freshness window — matches `db_integrity` and the
/// `leader_heartbeat_stale` policy.
const LEADER_FRESH_SECS: i64 = 60;

/// Alert if the newest *successful* drill is older than this many days (or none
/// has ever succeeded). Daily cadence ⇒ 2 days tolerates one missed/failed run
/// before escalating.
const STALE_DRILL_DAYS: f64 = 2.0;

const GIB: u64 = 1024 * 1024 * 1024;
const DEFAULT_MAX_ENCRYPTED_BYTES: u64 = 64 * GIB;
const DEFAULT_MAX_EXTRACTED_BYTES: u64 = 256 * GIB;
const DEFAULT_MAX_FILES: u64 = 5_000_000;
const DEFAULT_EXPANSION_RATIO: u64 = 8;
const DEFAULT_RESERVE_BYTES: u64 = 20 * GIB;
const POSTGRES_PROOF_IMAGE: &str = "pgvector/pgvector:pg16";
/// Exact image used to interpret FalkorDB RDB/AOF bytes. Never use a mutable
/// tag for a restore proof: a tag move would make an old receipt irreproducible.
const FALKORDB_PROOF_IMAGE: &str =
    "falkordb/falkordb@sha256:9042fdc4e53f5390ca5a3993aa71506523970efb40ffb9a98e6a4b1a9a4f8862";
const POSTGRES_PROOF_USER: &str = "forgefleet";
const POSTGRES_PROOF_DATABASE: &str = "forgefleet";
const POSTGRES_PROOF_QUERY: &str = "SELECT current_setting('server_version_num')::int / 10000 = 16 AND (SELECT count(*) > 0 FROM public.computers);";
const DOCKER_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const POSTGRES_READY_TIMEOUT: Duration = Duration::from_secs(180);
const FALKORDB_READY_TIMEOUT: Duration = Duration::from_secs(120);
const FALKORDB_GRAPH_QUERY_TIMEOUT: Duration = Duration::from_secs(10);
const FALKORDB_ALL_GRAPHS_TIMEOUT: Duration = Duration::from_secs(120);
const PG_VERIFYBACKUP_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const DRILL_RESOURCE_TTL_SECS: i64 = 60 * 60;
const TAR_ENTRY_OVERHEAD_BYTES: u64 = 1024;
const FALKORDB_EXPECTED_MIN_KEYS: u64 = 1;
const FALKORDB_EXPECTED_MIN_GRAPH_NODES: u64 = 1;
const FALKORDB_MAX_GRAPHS: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreKind {
    Postgres,
    FalkorDb,
}

impl RestoreKind {
    fn from_database_kind(value: &str) -> Result<Self, String> {
        match value {
            "postgres" => Ok(Self::Postgres),
            "falkordb" => Ok(Self::FalkorDb),
            other => Err(format!(
                "database kind {other:?} has no exact restore-drill implementation"
            )),
        }
    }

    fn backup_subdir(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::FalkorDb => "FalkorDB",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FalkorProofEvidence {
    image_id: String,
    key_count: u64,
    graph_count: u64,
    node_count: u64,
}

fn falkordb_receipt(evidence: &FalkorProofEvidence, input_checksum: &str) -> serde_json::Value {
    serde_json::json!({
        "proof": "falkordb_exact_restore_v1",
        "input_checksum_sha256": input_checksum,
        "image_reference": FALKORDB_PROOF_IMAGE,
        "image_id": evidence.image_id,
        "network": "none",
        "query_mode": "GRAPH.RO_QUERY",
        "expected_min_keys": FALKORDB_EXPECTED_MIN_KEYS,
        "observed_keys": evidence.key_count,
        "expected_min_graph_nodes": FALKORDB_EXPECTED_MIN_GRAPH_NODES,
        "observed_graphs": evidence.graph_count,
        "observed_graph_nodes": evidence.node_count,
    })
}

#[derive(Debug, Clone, Copy)]
struct DrillPolicy {
    max_encrypted_bytes: u64,
    max_extracted_bytes: u64,
    max_files: u64,
    expansion_ratio: u64,
    reserve_bytes: u64,
}

impl DrillPolicy {
    fn from_env() -> Result<Self, String> {
        Ok(Self {
            max_encrypted_bytes: env_u64(
                "FF_RESTORE_DRILL_MAX_ENCRYPTED_BYTES",
                DEFAULT_MAX_ENCRYPTED_BYTES,
            )?,
            max_extracted_bytes: env_u64(
                "FF_RESTORE_DRILL_MAX_EXTRACTED_BYTES",
                DEFAULT_MAX_EXTRACTED_BYTES,
            )?,
            max_files: env_u64("FF_RESTORE_DRILL_MAX_FILES", DEFAULT_MAX_FILES)?,
            expansion_ratio: env_u64(
                "FF_RESTORE_DRILL_MAX_EXPANSION_RATIO",
                DEFAULT_EXPANSION_RATIO,
            )?,
            reserve_bytes: env_u64("FF_RESTORE_DRILL_RESERVE_BYTES", DEFAULT_RESERVE_BYTES)?,
        })
    }

    fn extracted_limit(&self, encrypted_bytes: u64) -> u64 {
        self.max_extracted_bytes
            .min(encrypted_bytes.saturating_mul(self.expansion_ratio))
    }

    fn preflight(&self, encrypted_bytes: u64, available_bytes: u64) -> Result<u64, String> {
        if encrypted_bytes > self.max_encrypted_bytes {
            return Err(format!(
                "ciphertext {encrypted_bytes} bytes exceeds policy ceiling {} bytes",
                self.max_encrypted_bytes
            ));
        }
        let extracted = self.extracted_limit(encrypted_bytes);
        let required = encrypted_bytes
            .saturating_add(extracted)
            .saturating_add(self.reserve_bytes);
        if available_bytes < required {
            return Err(format!(
                "insufficient scratch space: required={required} available={available_bytes} bytes; policy encrypted_max={} extracted_max={} effective_extracted={} files_max={} expansion_ratio={} reserve={}",
                self.max_encrypted_bytes,
                self.max_extracted_bytes,
                extracted,
                self.max_files,
                self.expansion_ratio,
                self.reserve_bytes
            ));
        }
        Ok(required)
    }

    fn summary(&self, encrypted_bytes: u64, required: u64, available: u64) -> String {
        format!(
            "capacity required={required} available={available} bytes; policy encrypted_max={} extracted_max={} effective_extracted={} files_max={} expansion_ratio={} reserve={}",
            self.max_encrypted_bytes,
            self.max_extracted_bytes,
            self.extracted_limit(encrypted_bytes),
            self.max_files,
            self.expansion_ratio,
            self.reserve_bytes
        )
    }
}

fn env_u64(name: &str, default: u64) -> Result<u64, String> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| format!("{name} must be a positive integer byte/count value")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(format!("cannot read {name}: {error}")),
    }
}

/// The alert policy name seeded by migration V130.
const POLICY_NAME: &str = "backup_restore_drill_failed";

/// Outcome of a single restore-drill pass. Persisted verbatim to
/// `backup_drills` and used to decide whether to alert.
#[derive(Debug, Clone)]
pub struct DrillOutcome {
    /// Primary key of the preallocated `backup_drills` row. This is the
    /// coordinator/remote-worker fencing token.
    pub run_id: uuid::Uuid,
    pub backup_id: Option<uuid::Uuid>,
    pub backup_file: String,
    pub success: bool,
    /// How far the drill got (or where it failed): `select` → `locate` →
    /// `checksum` → `decrypt` → `extract` → `validate` → `done`.
    pub stage: String,
    pub detail: String,
    pub extracted_bytes: Option<i64>,
    pub file_count: Option<i64>,
    pub pg_version: Option<String>,
    /// `Some(true/false)` if `pg_verifybackup` ran; `None` if skipped (tool or
    /// manifest absent) — skipping is not a failure.
    pub verifybackup: Option<bool>,
    pub duration_ms: i64,
}

impl DrillOutcome {
    fn failed(
        run_id: uuid::Uuid,
        backup_id: Option<uuid::Uuid>,
        file: &str,
        stage: &str,
        detail: String,
    ) -> Self {
        Self {
            run_id,
            backup_id,
            backup_file: file.to_string(),
            success: false,
            stage: stage.to_string(),
            detail,
            extracted_bytes: None,
            file_count: None,
            pg_version: None,
            verifybackup: None,
            duration_ms: 0,
        }
    }
}

/// Preallocate one immutable restore-drill run. The caller-chosen UUID is both
/// the `backup_drills.id` primary key and the token carried through a remote
/// deferred payload. Reusing a run ID is accepted only when backup and node
/// match exactly; a collision can never retarget an existing run.
pub async fn reserve_drill_run(
    pool: &PgPool,
    run_id: uuid::Uuid,
    backup_id: uuid::Uuid,
    drill_node: &str,
) -> anyhow::Result<()> {
    let drill_node = drill_node.trim();
    if drill_node.is_empty() {
        anyhow::bail!("restore-drill node must not be empty");
    }

    sqlx::query(
        r#"
        INSERT INTO backup_drills
            (id, backup_id, backup_file, database_kind, success, stage, detail,
             drill_node)
        SELECT $1, b.id, b.file_name, b.database_kind, false, 'reserved',
               'exact restore drill reserved', $3
          FROM backups b
         WHERE b.id = $2 AND b.database_kind IN ('postgres', 'falkordb')
        ON CONFLICT (id) DO NOTHING
        "#,
    )
    .bind(run_id)
    .bind(backup_id)
    .bind(drill_node)
    .execute(pool)
    .await?;

    let fence: Option<(Option<uuid::Uuid>, String, String)> = sqlx::query_as(
        "SELECT backup_id, drill_node, database_kind FROM backup_drills WHERE id=$1",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await?;
    match fence {
        Some((Some(actual_backup), actual_node, kind))
            if actual_backup == backup_id
                && actual_node.eq_ignore_ascii_case(drill_node)
                && RestoreKind::from_database_kind(&kind).is_ok() =>
        {
            Ok(())
        }
        Some((actual_backup, actual_node, kind)) => anyhow::bail!(
            "restore-drill run-id collision: run={run_id} requested backup={backup_id} node={drill_node}, existing backup={actual_backup:?} node={actual_node} kind={kind}"
        ),
        None => anyhow::bail!(
            "no supported postgres/falkordb backup row with id {backup_id}; exact restore drill refuses fallback"
        ),
    }
}

/// Pure alert decision, isolated for unit testing: alert when the just-run
/// drill failed, OR when the newest successful drill is too old (or there has
/// never been one).
fn should_alert(success: bool, days_since_success: Option<f64>, stale_days: f64) -> bool {
    if !success {
        return true;
    }
    match days_since_success {
        None => true,
        Some(d) => d > stale_days,
    }
}

/// The restore-drill tick. Spawn on every daemon; gated to the live leader
/// inside the loop.
pub struct RestoreDrillTick {
    pg: PgPool,
    my_name: String,
    /// Root of the backup tree (`<dir>/postgres/<file>`); defaults to
    /// `~/.forgefleet/backups`, the same default as `BackupOrchestrator`.
    backup_dir: PathBuf,
}

impl RestoreDrillTick {
    pub fn new(pg: PgPool, my_name: String) -> Self {
        let backup_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".forgefleet/backups");
        Self {
            pg,
            my_name,
            backup_dir,
        }
    }

    /// Are we the live leader right now? (Identical gate to `db_integrity`.)
    async fn is_live_leader(&self) -> bool {
        match ff_db::leader_state::pg_get_current_leader(&self.pg).await {
            Ok(Some(leader)) => {
                let fresh = chrono::Utc::now()
                    .signed_duration_since(leader.heartbeat_at)
                    .num_seconds()
                    < LEADER_FRESH_SECS;
                leader.member_name == self.my_name && fresh
            }
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(error = %e, "restore-drill: failed to read leader state");
                false
            }
        }
    }

    /// Run one full drill against one immutable Postgres or FalkorDB backup
    /// ID. Never selects an older/local fallback when the requested artifact
    /// is absent.
    pub async fn run_drill_once(&self, backup_id: uuid::Uuid, run_id: uuid::Uuid) -> DrillOutcome {
        let started = std::time::Instant::now();
        let mut outcome = self.drill_inner(backup_id, run_id).await;
        outcome.duration_ms = started.elapsed().as_millis() as i64;
        outcome
    }

    async fn drill_inner(&self, backup_id: uuid::Uuid, run_id: uuid::Uuid) -> DrillOutcome {
        // 1) select exactly the caller-fenced supported backup row.
        let row: Option<(String, i64, String, String)> = match sqlx::query_as(
            "SELECT file_name, size_bytes, checksum_sha256, database_kind \
               FROM backups \
              WHERE id=$1 AND database_kind IN ('postgres', 'falkordb')",
        )
        .bind(backup_id)
        .fetch_optional(&self.pg)
        .await
        {
            Ok(r) => r,
            Err(e) => {
                return DrillOutcome::failed(
                    run_id,
                    Some(backup_id),
                    "",
                    "select",
                    format!("query backups failed: {e}"),
                );
            }
        };
        let Some((file_name, recorded_bytes, checksum, database_kind)) = row else {
            return DrillOutcome::failed(
                run_id,
                Some(backup_id),
                "",
                "select",
                format!(
                    "no supported postgres/falkordb backup row with id {backup_id}; exact restore drill refuses fallback"
                ),
            );
        };
        let kind = match RestoreKind::from_database_kind(&database_kind) {
            Ok(kind) => kind,
            Err(error) => {
                return DrillOutcome::failed(run_id, Some(backup_id), &file_name, "select", error);
            }
        };
        if !safe_backup_file_name(&file_name) {
            return DrillOutcome::failed(
                run_id,
                Some(backup_id),
                &file_name,
                "locate",
                "catalog file_name is not one bounded path component".into(),
            );
        }
        if !file_name.ends_with(".age") {
            return DrillOutcome::failed(
                run_id,
                Some(backup_id),
                &file_name,
                "locate",
                "exact restore drill requires an age-encrypted artifact".into(),
            );
        }
        if kind == RestoreKind::FalkorDb && !file_name.ends_with(".tar.zst.age") {
            return DrillOutcome::failed(
                run_id,
                Some(backup_id),
                &file_name,
                "locate",
                "FalkorDB restore drill requires a .tar.zst.age artifact".into(),
            );
        }

        // 2) locate only that exact artifact on this node. Symlinks and
        // non-files fail closed before checksum/decryption.
        let path = self.backup_dir.join(kind.backup_subdir()).join(&file_name);
        let metadata = match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) if metadata.file_type().is_file() => metadata,
            Ok(_) => {
                return DrillOutcome::failed(
                    run_id,
                    Some(backup_id),
                    &file_name,
                    "locate",
                    "exact backup artifact is not a regular non-symlink file".into(),
                );
            }
            Err(error) => {
                return DrillOutcome::failed(
                    run_id,
                    Some(backup_id),
                    &file_name,
                    "locate",
                    format!(
                        "exact backup artifact is missing on node '{}': {error}; no local fallback",
                        self.my_name
                    ),
                );
            }
        };
        let disk_bytes = metadata.len();
        if disk_bytes == 0 {
            return DrillOutcome::failed(
                run_id,
                Some(backup_id),
                &file_name,
                "locate",
                "backup file is 0 bytes — producer never wrote ciphertext \
                 (likely `age` CLI missing at backup time)"
                    .into(),
            );
        }
        let policy = match DrillPolicy::from_env() {
            Ok(policy) => policy,
            Err(error) => {
                return DrillOutcome::failed(
                    run_id,
                    Some(backup_id),
                    &file_name,
                    "preflight",
                    error,
                );
            }
        };
        let available_bytes = match available_space(std::env::temp_dir().as_path()) {
            Ok(bytes) => bytes,
            Err(error) => {
                return DrillOutcome::failed(
                    run_id,
                    Some(backup_id),
                    &file_name,
                    "preflight",
                    error,
                );
            }
        };
        let required_bytes = match policy.preflight(disk_bytes, available_bytes) {
            Ok(required) => required,
            Err(error) => {
                return DrillOutcome::failed(
                    run_id,
                    Some(backup_id),
                    &file_name,
                    "preflight",
                    error,
                );
            }
        };
        let capacity = policy.summary(disk_bytes, required_bytes, available_bytes);
        if recorded_bytes < 0 || recorded_bytes as u64 != disk_bytes {
            return DrillOutcome::failed(
                run_id,
                Some(backup_id),
                &file_name,
                "locate",
                format!(
                    "exact artifact size mismatch: on-disk {disk_bytes} != catalog {recorded_bytes}"
                ),
            );
        }

        // 3) checksum — guards against bit-rot / truncated rsync and binds the
        // proof receipt to the catalogued input bytes.
        if !valid_sha256_checksum(&checksum) {
            return DrillOutcome::failed(
                run_id,
                Some(backup_id),
                &file_name,
                "checksum",
                "catalog checksum_sha256 is not exactly 64 hexadecimal characters".into(),
            );
        }
        match crate::ha::backup::file_metadata(&path).await {
            Ok((_, actual)) if actual == checksum => {}
            Ok((_, actual)) => {
                return DrillOutcome::failed(
                    run_id,
                    Some(backup_id),
                    &file_name,
                    "checksum",
                    format!("sha256 mismatch: on-disk {actual} != recorded {checksum}"),
                );
            }
            Err(e) => {
                return DrillOutcome::failed(
                    run_id,
                    Some(backup_id),
                    &file_name,
                    "checksum",
                    format!("checksum read failed: {e}"),
                );
            }
        }

        // Everything below materializes plaintext / extracts the cluster, so do
        // it under a unique temp dir we always remove.
        if let Err(error) =
            scavenge_stale_drill_dirs(std::env::temp_dir().as_path(), std::time::SystemTime::now())
        {
            return DrillOutcome::failed(run_id, Some(backup_id), &file_name, "cleanup", error);
        }
        let work = match tempfile::Builder::new().prefix("ff-drill-").tempdir() {
            Ok(work) => work,
            Err(error) => {
                return DrillOutcome::failed(
                    run_id,
                    Some(backup_id),
                    &file_name,
                    "decrypt",
                    format!("create unique work dir failed: {error}"),
                );
            }
        };
        match kind {
            RestoreKind::Postgres => {
                self.drill_decrypt_extract(
                    run_id,
                    backup_id,
                    &file_name,
                    &path,
                    work.path(),
                    policy,
                    &capacity,
                )
                .await
            }
            RestoreKind::FalkorDb => {
                self.drill_falkordb_decrypt_extract(
                    run_id,
                    backup_id,
                    &file_name,
                    &path,
                    &checksum,
                    work.path(),
                    policy,
                    &capacity,
                )
                .await
            }
        }
    }

    /// Stages 4–6 (decrypt → extract → validate), all under `work`.
    async fn drill_decrypt_extract(
        &self,
        run_id: uuid::Uuid,
        backup_id: uuid::Uuid,
        file_name: &str,
        enc_path: &Path,
        work: &Path,
        policy: DrillPolicy,
        capacity: &str,
    ) -> DrillOutcome {
        // 4) decrypt → `<work>/<file without .age>`.
        let plain_name = file_name.strip_suffix(".age").unwrap_or(file_name);
        let plain_path = work.join(plain_name);
        let max_plaintext_bytes = std::fs::metadata(enc_path)
            .map(|metadata| metadata.len())
            .unwrap_or(policy.max_encrypted_bytes);
        if let Err(e) =
            decrypt_backup_bounded(&self.pg, enc_path, &plain_path, max_plaintext_bytes).await
        {
            return DrillOutcome::failed(
                run_id,
                Some(backup_id),
                file_name,
                "decrypt",
                format!("age decrypt failed: {e}"),
            );
        }

        // 5) extract with entry-by-entry type, path, byte, and file bounds.
        let pgdata = work.join("pgdata");
        if let Err(e) = tokio::fs::create_dir_all(&pgdata).await {
            return DrillOutcome::failed(
                run_id,
                Some(backup_id),
                file_name,
                "extract",
                format!("create pgdata dir failed: {e}"),
            );
        }
        let extract_path = plain_path.clone();
        let extract_root = pgdata.clone();
        let extraction = tokio::task::spawn_blocking(move || {
            extract_archive_bounded(&extract_path, &extract_root, policy)
        })
        .await;
        let (file_count, extracted_bytes) = match extraction {
            Ok(Ok(metrics)) => metrics,
            Ok(Err(error)) => {
                return DrillOutcome::failed(run_id, Some(backup_id), file_name, "extract", error);
            }
            Err(error) => {
                return DrillOutcome::failed(
                    run_id,
                    Some(backup_id),
                    file_name,
                    "extract",
                    format!("extract worker failed: {error}"),
                );
            }
        };

        // 6) validate — structurally complete PGDATA?
        let pg_version_path = pgdata.join("PG_VERSION");
        let pg_control_path = pgdata.join("global").join("pg_control");
        let has_version = tokio::fs::try_exists(&pg_version_path)
            .await
            .unwrap_or(false);
        let has_control = tokio::fs::try_exists(&pg_control_path)
            .await
            .unwrap_or(false);
        if !has_version || !has_control {
            return DrillOutcome::failed(
                run_id,
                Some(backup_id),
                file_name,
                "validate",
                format!(
                    "extracted tree is not a complete PGDATA (PG_VERSION={has_version}, \
                     global/pg_control={has_control}) — archive is corrupt or truncated"
                ),
            );
        }
        let pg_version = tokio::fs::read_to_string(&pg_version_path)
            .await
            .ok()
            .map(|s| s.trim().to_string());
        if pg_version.as_deref() != Some("16") {
            return DrillOutcome {
                run_id,
                backup_id: Some(backup_id),
                backup_file: file_name.to_string(),
                success: false,
                stage: "validate".to_string(),
                detail: format!(
                    "physical restore proof requires PostgreSQL 16 PGDATA; found {:?}",
                    pg_version.as_deref().unwrap_or("missing")
                ),
                extracted_bytes: Some(extracted_bytes),
                file_count: Some(file_count),
                pg_version,
                verifybackup: None,
                duration_ms: 0,
            };
        }

        // Cryptographic per-file validation remains additive. A reported
        // mismatch is fatal; absence alone is not proof and therefore cannot
        // produce success without the isolated PostgreSQL startup below.
        let verifybackup = self.maybe_pg_verifybackup(&pgdata).await;

        let verify_detail = match verifybackup {
            Some(true) => "pg_verifybackup OK",
            Some(false) => {
                return DrillOutcome {
                    run_id,
                    backup_id: Some(backup_id),
                    backup_file: file_name.to_string(),
                    success: false,
                    stage: "validate".to_string(),
                    detail: "pg_verifybackup reported manifest/file mismatch".to_string(),
                    extracted_bytes: Some(extracted_bytes),
                    file_count: Some(file_count),
                    pg_version,
                    verifybackup,
                    duration_ms: 0,
                };
            }
            None => "pg_verifybackup unavailable (not accepted as proof by itself)",
        };

        // 7) The success boundary: start this extracted PGDATA in an isolated,
        // bounded PostgreSQL 16 container and execute an application read.
        // Structural checks alone never reach `success=true`.
        let restore_detail = match prove_postgres_restore(&pgdata, run_id).await {
            Ok(detail) => detail,
            Err(error) => {
                return DrillOutcome {
                    run_id,
                    backup_id: Some(backup_id),
                    backup_file: file_name.to_string(),
                    success: false,
                    stage: "restore".to_string(),
                    detail: error,
                    extracted_bytes: Some(extracted_bytes),
                    file_count: Some(file_count),
                    pg_version,
                    verifybackup,
                    duration_ms: 0,
                };
            }
        };

        DrillOutcome {
            run_id,
            backup_id: Some(backup_id),
            backup_file: file_name.to_string(),
            success: true,
            stage: "done".to_string(),
            detail: format!("restore drill passed; {restore_detail}; {verify_detail}; {capacity}"),
            extracted_bytes: Some(extracted_bytes),
            file_count: Some(file_count),
            pg_version,
            verifybackup,
            duration_ms: 0,
        }
    }

    /// Restore one encrypted FalkorDB tar.zst into an isolated, digest-pinned
    /// container and prove it with read-only Redis/FalkorDB queries.
    #[allow(clippy::too_many_arguments)]
    async fn drill_falkordb_decrypt_extract(
        &self,
        run_id: uuid::Uuid,
        backup_id: uuid::Uuid,
        file_name: &str,
        enc_path: &Path,
        input_checksum: &str,
        work: &Path,
        policy: DrillPolicy,
        capacity: &str,
    ) -> DrillOutcome {
        let plain_name = file_name.strip_suffix(".age").unwrap_or(file_name);
        let plain_path = work.join(plain_name);
        let max_plaintext_bytes = std::fs::metadata(enc_path)
            .map(|metadata| metadata.len())
            .unwrap_or(policy.max_encrypted_bytes);
        if let Err(error) =
            decrypt_backup_bounded(&self.pg, enc_path, &plain_path, max_plaintext_bytes).await
        {
            return DrillOutcome::failed(
                run_id,
                Some(backup_id),
                file_name,
                "decrypt",
                format!("age decrypt failed: {error}"),
            );
        }

        let data_dir = work.join("falkordb-data");
        if let Err(error) = tokio::fs::create_dir_all(&data_dir).await {
            return DrillOutcome::failed(
                run_id,
                Some(backup_id),
                file_name,
                "extract",
                format!("create FalkorDB data dir failed: {error}"),
            );
        }
        let extract_path = plain_path.clone();
        let extract_root = data_dir.clone();
        let extraction = tokio::task::spawn_blocking(move || {
            extract_falkordb_archive_bounded(&extract_path, &extract_root, policy)
        })
        .await;
        let (file_count, extracted_bytes) = match extraction {
            Ok(Ok(metrics)) => metrics,
            Ok(Err(error)) => {
                return DrillOutcome::failed(run_id, Some(backup_id), file_name, "extract", error);
            }
            Err(error) => {
                return DrillOutcome::failed(
                    run_id,
                    Some(backup_id),
                    file_name,
                    "extract",
                    format!("extract worker failed: {error}"),
                );
            }
        };

        let dump_path = data_dir.join("dump.rdb");
        match std::fs::symlink_metadata(&dump_path) {
            Ok(metadata) if metadata.file_type().is_file() && metadata.len() > 0 => {}
            Ok(_) => {
                return DrillOutcome::failed(
                    run_id,
                    Some(backup_id),
                    file_name,
                    "validate",
                    "FalkorDB archive dump.rdb is not a non-empty regular file".into(),
                );
            }
            Err(error) => {
                return DrillOutcome::failed(
                    run_id,
                    Some(backup_id),
                    file_name,
                    "validate",
                    format!("FalkorDB archive is missing dump.rdb: {error}"),
                );
            }
        }

        let evidence = match prove_falkordb_restore(&data_dir, run_id).await {
            Ok(evidence) => evidence,
            Err(error) => {
                return DrillOutcome {
                    run_id,
                    backup_id: Some(backup_id),
                    backup_file: file_name.to_string(),
                    success: false,
                    stage: "restore".to_string(),
                    detail: error,
                    extracted_bytes: Some(extracted_bytes),
                    file_count: Some(file_count),
                    pg_version: None,
                    verifybackup: None,
                    duration_ms: 0,
                };
            }
        };

        let receipt = falkordb_receipt(&evidence, input_checksum);
        DrillOutcome {
            run_id,
            backup_id: Some(backup_id),
            backup_file: file_name.to_string(),
            success: true,
            stage: "done".to_string(),
            detail: format!("restore drill passed; receipt={receipt}; {capacity}"),
            extracted_bytes: Some(extracted_bytes),
            file_count: Some(file_count),
            pg_version: None,
            verifybackup: None,
            duration_ms: 0,
        }
    }

    /// Run `pg_verifybackup` iff the tool is on PATH and a `backup_manifest`
    /// exists in the extracted tree. Returns `None` when skipped.
    async fn maybe_pg_verifybackup(&self, pgdata: &Path) -> Option<bool> {
        let manifest = pgdata.join("backup_manifest");
        if !tokio::fs::try_exists(&manifest).await.unwrap_or(false) {
            return None;
        }
        // `pg_verifybackup` lives in the host's postgres client tools; on the
        // leader it may not be installed. Probe before invoking.
        let mut version_command = tokio::process::Command::new("pg_verifybackup");
        version_command.arg("--version").kill_on_drop(true);
        let which = tokio::time::timeout(DOCKER_COMMAND_TIMEOUT, version_command.output()).await;
        if which
            .ok()
            .and_then(Result::ok)
            .map(|output| !output.status.success())
            .unwrap_or(true)
        {
            return None;
        }
        let mut verify_command = tokio::process::Command::new("pg_verifybackup");
        verify_command
            .arg("-n") // skip WAL verification (WAL replay isn't part of a -X fetch base)
            .arg(pgdata)
            .kill_on_drop(true);
        let out = tokio::time::timeout(PG_VERIFYBACKUP_TIMEOUT, verify_command.output()).await;
        match out {
            Ok(Ok(output)) => Some(output.status.success()),
            Ok(Err(_)) | Err(_) => Some(false),
        }
    }

    async fn claim_reserved_run(
        &self,
        run_id: uuid::Uuid,
        backup_id: uuid::Uuid,
    ) -> anyhow::Result<String> {
        sqlx::query_scalar(
            "UPDATE backup_drills
                SET stage='running', detail='exact restore drill running',
                    started_at=NOW(), finished_at=NULL
              WHERE id=$1 AND backup_id=$2
                AND LOWER(drill_node)=LOWER($3)
                AND stage='reserved' AND finished_at IS NULL
              RETURNING backup_file",
        )
        .bind(run_id)
        .bind(backup_id)
        .bind(&self.my_name)
        .fetch_optional(&self.pg)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "restore-drill fence rejected claim run={run_id} backup={backup_id} node={}",
                self.my_name
            )
        })
    }

    /// Hold one session-level advisory lock for the exact backup during the
    /// expensive restore. `close_on_drop` is mandatory: cancellation closes
    /// the DB session instead of returning a still-locked connection to the
    /// pool.
    async fn acquire_backup_lock(
        &self,
        backup_id: uuid::Uuid,
    ) -> Result<PoolConnection<Postgres>, String> {
        let mut connection = self
            .pg
            .acquire()
            .await
            .map_err(|error| format!("acquire backup drill lock connection: {error}"))?;
        connection.close_on_drop();
        let acquired: bool =
            sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtextextended($1, 0))")
                .bind(backup_id.to_string())
                .fetch_one(&mut *connection)
                .await
                .map_err(|error| format!("acquire backup drill advisory lock: {error}"))?;
        if !acquired {
            return Err(format!(
                "another exact restore drill already owns backup {backup_id}"
            ));
        }
        Ok(connection)
    }

    async fn release_backup_lock(
        &self,
        connection: &mut PoolConnection<Postgres>,
        backup_id: uuid::Uuid,
    ) {
        let released =
            sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
                .bind(backup_id.to_string())
                .fetch_one(&mut **connection)
                .await;
        if !matches!(released, Ok(true)) {
            tracing::warn!(%backup_id, result=?released, "restore-drill advisory unlock failed; connection is close-on-drop");
        }
    }

    /// Complete exactly the preallocated run row. A successful full startup +
    /// application-read proof also promotes that exact backup to
    /// restore-verified evidence in the same transaction. Zero-row fences and
    /// missing/wrong-kind backups roll the transaction back.
    pub async fn record_drill(&self, o: &DrillOutcome) -> anyhow::Result<()> {
        let backup_id = o
            .backup_id
            .ok_or_else(|| anyhow::anyhow!("restore-drill outcome is missing backup_id"))?;
        let result: Result<(), sqlx::Error> = async {
            let mut tx = self.pg.begin().await?;
            sqlx::query_scalar::<_, uuid::Uuid>(
                r#"
                UPDATE backup_drills
                   SET success=$4, stage=$5, detail=$6, extracted_bytes=$7,
                       file_count=$8, pg_version=$9, verifybackup=$10,
                       duration_ms=$11, finished_at=NOW()
                 WHERE id=$1 AND backup_id=$2
                   AND LOWER(drill_node)=LOWER($3)
                   AND stage='running' AND finished_at IS NULL
                 RETURNING id
                "#,
            )
            .bind(o.run_id)
            .bind(backup_id)
            .bind(&self.my_name)
            .bind(o.success)
            .bind(&o.stage)
            .bind(&o.detail)
            .bind(o.extracted_bytes)
            .bind(o.file_count)
            .bind(&o.pg_version)
            .bind(o.verifybackup)
            .bind(o.duration_ms)
            .fetch_one(&mut *tx)
            .await?;

            if o.success {
                sqlx::query_scalar::<_, uuid::Uuid>(
                    "UPDATE backups SET verified_restorable_at=NOW()
                      WHERE id=$1 AND database_kind IN ('postgres', 'falkordb') RETURNING id",
                )
                .bind(backup_id)
                .fetch_one(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            Ok(())
        }
        .await;
        result.map_err(|error| anyhow::anyhow!("record exact restore drill: {error}"))
    }

    /// Days since the newest *successful* drill, or `None` if there has never
    /// been one.
    async fn days_since_last_success(&self, backup_id: Option<uuid::Uuid>) -> Option<f64> {
        let secs: Option<f64> = sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM (NOW() - MAX(started_at)))::DOUBLE PRECISION \
               FROM backup_drills \
              WHERE success = true \
                AND database_kind = COALESCE( \
                    (SELECT database_kind FROM backups WHERE id=$1), 'postgres')",
        )
        .bind(backup_id)
        .fetch_one(&self.pg)
        .await
        .ok()
        .flatten();
        secs.map(|s| s / 86_400.0)
    }

    /// Fire the `backup_restore_drill_failed` alert (mirrors
    /// `db_integrity::fire_corruption_alert`).
    async fn fire_alert(&self, message: &str) {
        let policy: Option<(uuid::Uuid, String, String)> = match sqlx::query_as(
            "SELECT id, severity, channel FROM alert_policies WHERE name = $1 AND enabled = true",
        )
        .bind(POLICY_NAME)
        .fetch_optional(&self.pg)
        .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, "restore-drill: failed to load alert policy");
                None
            }
        };
        let Some((policy_id, severity, channel)) = policy else {
            tracing::error!(
                "restore-drill: ALERT-WORTHY ({message}) but policy '{POLICY_NAME}' \
                 missing/disabled — NOT alerting"
            );
            return;
        };

        let channel_result =
            crate::alert_evaluator::dispatch_alert(&self.pg, &channel, &severity, message).await;
        if let Err(e) = sqlx::query(
            r#"
            INSERT INTO alert_events
                (policy_id, computer_id, value, value_text, message, channel_result)
            VALUES ($1, NULL, 1, NULL, $2, $3)
            "#,
        )
        .bind(policy_id)
        .bind(message)
        .bind(&channel_result)
        .execute(&self.pg)
        .await
        {
            tracing::error!(error = %e, "restore-drill: failed to record alert_event");
        }
        tracing::error!(channel = %channel, "restore-drill: alert fired — {message}");
    }

    async fn alert_and_log(&self, outcome: &DrillOutcome) {
        let days = self.days_since_last_success(outcome.backup_id).await;
        if should_alert(outcome.success, days, STALE_DRILL_DAYS) {
            let staleness = match days {
                Some(d) => format!("{d:.1}d since last success"),
                None => "no drill has EVER succeeded".to_string(),
            };
            let msg = format!(
                "Backup restore-drill on '{}': run={} backup={} stage={} — {}. \
                 ({}). A backup that cannot be restored is a silent data-loss \
                 risk (cf. the 2026-04-18 wipe).",
                self.my_name,
                outcome.run_id,
                outcome.backup_file,
                outcome.stage,
                outcome.detail,
                staleness
            );
            self.fire_alert(&msg).await;
        } else {
            tracing::info!(
                backup = %outcome.backup_file,
                bytes = outcome.extracted_bytes.unwrap_or(0),
                files = outcome.file_count.unwrap_or(0),
                pg_version = outcome.pg_version.as_deref().unwrap_or("?"),
                verifybackup = ?outcome.verifybackup,
                ms = outcome.duration_ms,
                run_id = %outcome.run_id,
                backup_id = ?outcome.backup_id,
                "restore-drill: PASS — exact backup is restorable"
            );
        }
    }

    /// Run, record, and alert for one exact `(run, backup, node)` fence.
    pub async fn run_record_and_alert_exact(
        &self,
        backup_id: uuid::Uuid,
        run_id: uuid::Uuid,
    ) -> DrillOutcome {
        if let Err(error) = reserve_drill_run(&self.pg, run_id, backup_id, &self.my_name).await {
            let outcome =
                DrillOutcome::failed(run_id, Some(backup_id), "", "fence", error.to_string());
            self.alert_and_log(&outcome).await;
            return outcome;
        }
        let backup_file = match self.claim_reserved_run(run_id, backup_id).await {
            Ok(file) => file,
            Err(error) => {
                let outcome =
                    DrillOutcome::failed(run_id, Some(backup_id), "", "fence", error.to_string());
                self.alert_and_log(&outcome).await;
                return outcome;
            }
        };
        let mut lock = match self.acquire_backup_lock(backup_id).await {
            Ok(lock) => lock,
            Err(error) => {
                let outcome =
                    DrillOutcome::failed(run_id, Some(backup_id), &backup_file, "fence", error);
                if let Err(record_error) = self.record_drill(&outcome).await {
                    tracing::error!(error=%record_error, "restore-drill: failed to record lock refusal");
                }
                self.alert_and_log(&outcome).await;
                return outcome;
            }
        };

        let mut outcome = self.run_drill_once(backup_id, run_id).await;
        let record_result = self.record_drill(&outcome).await;
        self.release_backup_lock(&mut lock, backup_id).await;
        if let Err(error) = record_result {
            tracing::error!(error=%error, %run_id, %backup_id, "restore-drill: exact completion fence failed");
            outcome = DrillOutcome::failed(
                run_id,
                Some(backup_id),
                &outcome.backup_file,
                "record",
                error.to_string(),
            );
        }
        self.alert_and_log(&outcome).await;
        outcome
    }

    /// The scheduled tick freezes one newest backup ID, generates a run ID,
    /// and then uses the same exact path as the CLI. It never falls back if
    /// that artifact is absent locally.
    async fn run_scheduled_record_and_alert(&self) -> DrillOutcome {
        let run_id = uuid::Uuid::new_v4();
        let backup_id: Option<uuid::Uuid> = sqlx::query_scalar(
            "SELECT id FROM backups WHERE database_kind='postgres' ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_optional(&self.pg)
        .await
        .unwrap_or(None);
        let Some(backup_id) = backup_id else {
            let outcome = DrillOutcome::failed(
                run_id,
                None,
                "",
                "select",
                "no postgres backup row exists for scheduled exact restore drill".into(),
            );
            self.alert_and_log(&outcome).await;
            return outcome;
        };
        self.run_record_and_alert_exact(backup_id, run_id).await
    }

    /// Spawn the daily loop. Leader-gated per fire; safe on every daemon.
    pub fn spawn(self, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            // Let startup settle before the first (potentially I/O-heavy) drill,
            // then run immediately so a deploy gets a fresh proof without waiting
            // a full day.
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(120)) => {}
                _ = shutdown.changed() => return,
            }
            let mut ticker = tokio::time::interval(DRILL_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        if !self.is_live_leader().await {
                            continue;
                        }
                        self.run_scheduled_record_and_alert().await;
                    }
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            break;
                        }
                    }
                }
            }
            tracing::info!("restore-drill tick loop stopped");
        })
    }
}

/// Stream an age ciphertext into the scratch archive with a hard plaintext
/// ceiling. The general backup helper materializes both ciphertext and
/// plaintext in RAM; a multi-gigabyte restore drill must not do that.
async fn decrypt_backup_bounded(
    pool: &PgPool,
    encrypted: &Path,
    destination: &Path,
    max_plaintext_bytes: u64,
) -> Result<(), String> {
    use std::str::FromStr;

    let private_key = ff_db::pg_get_secret(pool, crate::ha::backup::BACKUP_ENC_PRIVKEY)
        .await
        .map_err(|error| format!("read backup decryption key: {error}"))?
        .ok_or_else(|| "fleet_secrets.backup_encryption_privkey is not set".to_string())?;
    let identity = age::x25519::Identity::from_str(private_key.trim())
        .map_err(|error| format!("parse age identity: {error}"))?;
    let encrypted = encrypted.to_path_buf();
    let destination = destination.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let ciphertext = std::fs::File::open(&encrypted)
            .map_err(|error| format!("open age ciphertext failed: {error}"))?;
        let decryptor = age::Decryptor::new(ciphertext)
            .map_err(|error| format!("create age decryptor failed: {error}"))?;
        let plaintext = decryptor
            .decrypt(std::iter::once(&identity as &dyn age::Identity))
            .map_err(|error| format!("decrypt age ciphertext failed: {error}"))?;
        let mut plaintext = ByteLimitReader::new(
            plaintext,
            max_plaintext_bytes,
            "decrypted archive byte limit exceeded",
        );
        let mut output = std::fs::File::create(&destination)
            .map_err(|error| format!("create decrypted archive failed: {error}"))?;
        std::io::copy(&mut plaintext, &mut output)
            .map_err(|error| format!("stream decrypted archive failed: {error}"))?;
        output
            .sync_all()
            .map_err(|error| format!("sync decrypted archive failed: {error}"))?;
        Ok::<(), String>(())
    })
    .await
    .map_err(|error| format!("decrypt worker failed: {error}"))?
}

fn available_space(path: &Path) -> Result<u64, String> {
    let path = path.canonicalize().map_err(|error| {
        format!(
            "cannot resolve scratch directory '{}': {error}",
            path.display()
        )
    })?;
    let disks = Disks::new_with_refreshed_list();
    disks
        .iter()
        .filter(|disk| path.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().components().count())
        .map(|disk| disk.available_space())
        .ok_or_else(|| {
            format!(
                "cannot determine available space for '{}'; failing closed",
                path.display()
            )
        })
}

fn extract_archive_bounded(
    archive_path: &Path,
    root: &Path,
    policy: DrillPolicy,
) -> Result<(i64, i64), String> {
    let file = std::fs::File::open(archive_path)
        .map_err(|error| format!("open decrypted archive failed: {error}"))?;
    let compressed_bytes = file
        .metadata()
        .map_err(|error| format!("stat decrypted archive failed: {error}"))?
        .len();
    let decoder = flate2::read::GzDecoder::new(file);
    extract_tar_bounded(decoder, compressed_bytes, root, policy)
}

fn extract_falkordb_archive_bounded(
    archive_path: &Path,
    root: &Path,
    policy: DrillPolicy,
) -> Result<(i64, i64), String> {
    let file = std::fs::File::open(archive_path)
        .map_err(|error| format!("open decrypted FalkorDB archive failed: {error}"))?;
    let compressed_bytes = file
        .metadata()
        .map_err(|error| format!("stat decrypted FalkorDB archive failed: {error}"))?
        .len();
    let decoder = zstd::stream::read::Decoder::new(file)
        .map_err(|error| format!("open zstd archive failed: {error}"))?;
    extract_tar_bounded(decoder, compressed_bytes, root, policy)
}

fn extract_tar_bounded<R: std::io::Read>(
    decoder: R,
    compressed_bytes: u64,
    root: &Path,
    policy: DrillPolicy,
) -> Result<(i64, i64), String> {
    let mut paths = HashSet::new();
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    let byte_limit = policy.extracted_limit(compressed_bytes);
    // Count the entire decoded tar stream, not just entry payloads. This also
    // bounds headers, padding, and any malicious gzip expansion outside the
    // advertised entry sizes. A valid tar consumes at most two 512-byte
    // blocks of overhead per entry plus its payload and final marker.
    let decoded_limit = byte_limit
        .saturating_add(policy.max_files.saturating_mul(TAR_ENTRY_OVERHEAD_BYTES))
        .saturating_add(TAR_ENTRY_OVERHEAD_BYTES);
    let mut archive = tar::Archive::new(ByteLimitReader::new(
        decoder,
        decoded_limit,
        "decoded tar byte limit exceeded",
    ));

    let entries = archive
        .entries()
        .map_err(|error| format!("read tar archive failed: {error}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|error| format!("read tar entry failed: {error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("invalid tar path: {error}"))?
            .into_owned();
        if !safe_archive_path(&path) {
            return Err(format!("unsafe tar path rejected: '{}'", path.display()));
        }
        if !paths.insert(path.clone()) {
            return Err(format!("duplicate tar path rejected: '{}'", path.display()));
        }

        files = files.saturating_add(1);
        if files > policy.max_files {
            return Err(format!(
                "file-count limit exceeded: {files} > {}",
                policy.max_files
            ));
        }
        let kind = entry.header().entry_type();
        if kind.is_file() {
            bytes = bytes.saturating_add(
                entry
                    .header()
                    .size()
                    .map_err(|error| format!("invalid size for '{}': {error}", path.display()))?,
            );
            if bytes > byte_limit {
                return Err(format!(
                    "extracted-byte limit exceeded: {bytes} > {byte_limit}"
                ));
            }
        } else if !kind.is_dir() {
            return Err(format!(
                "unsafe tar entry type rejected for '{}' (links, sparse files, and devices are forbidden)",
                path.display()
            ));
        }

        entry
            .unpack_in(root)
            .map_err(|error| format!("extract '{}' failed: {error}", path.display()))?
            .then_some(())
            .ok_or_else(|| format!("tar entry escaped extraction root: '{}'", path.display()))?;
    }

    let files =
        i64::try_from(files).map_err(|_| "file count exceeds database metric range".to_string())?;
    let bytes = i64::try_from(bytes)
        .map_err(|_| "extracted bytes exceed database metric range".to_string())?;
    Ok((files, bytes))
}

fn safe_archive_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

fn safe_backup_file_name(file_name: &str) -> bool {
    !file_name.is_empty()
        && file_name.len() <= 255
        && !file_name.contains('\0')
        && Path::new(file_name).components().count() == 1
        && matches!(
            Path::new(file_name).components().next(),
            Some(Component::Normal(_))
        )
}

/// Reader that fails closed once the total uncompressed tar stream crosses
/// its policy limit. `Read::take` would silently present EOF at the boundary,
/// which could make a malicious archive look like a cleanly terminated tar.
struct ByteLimitReader<R> {
    inner: R,
    remaining: u64,
    message: &'static str,
}

impl<R> ByteLimitReader<R> {
    fn new(inner: R, limit: u64, message: &'static str) -> Self {
        Self {
            inner,
            remaining: limit,
            message,
        }
    }
}

impl<R: std::io::Read> std::io::Read for ByteLimitReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            let mut probe = [0_u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => Err(std::io::Error::other(self.message)),
            };
        }
        let remaining = usize::try_from(self.remaining).unwrap_or(usize::MAX);
        let requested = buffer.len().min(remaining);
        let read = self.inner.read(&mut buffer[..requested])?;
        self.remaining = self.remaining.saturating_sub(read as u64);
        Ok(read)
    }
}

/// Remove only stale ForgeFleet restore work directories from the selected
/// scratch root. This is the SIGKILL recovery path for `TempDir`, whose Drop
/// cleanup cannot run after an uncatchable process death.
fn scavenge_stale_drill_dirs(
    scratch_root: &Path,
    now: std::time::SystemTime,
) -> Result<(), String> {
    let canonical_root = scratch_root.canonicalize().map_err(|error| {
        format!(
            "cannot resolve restore-drill scratch root '{}': {error}",
            scratch_root.display()
        )
    })?;
    let entries = std::fs::read_dir(&canonical_root).map_err(|error| {
        format!(
            "cannot scan restore-drill scratch root '{}': {error}",
            canonical_root.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("scan restore-drill scratch entry: {error}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("ff-drill-") {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path())
            .map_err(|error| format!("stat stale restore-drill candidate: {error}"))?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let modified = metadata
            .modified()
            .map_err(|error| format!("read stale restore-drill candidate timestamp: {error}"))?;
        let stale = now
            .duration_since(modified)
            .map(|age| age.as_secs() >= DRILL_RESOURCE_TTL_SECS as u64)
            .unwrap_or(false);
        if !stale {
            continue;
        }
        let candidate = entry
            .path()
            .canonicalize()
            .map_err(|error| format!("resolve stale restore-drill candidate failed: {error}"))?;
        if candidate.parent() != Some(canonical_root.as_path()) {
            return Err(format!(
                "stale restore-drill candidate escaped scratch root: '{}'",
                candidate.display()
            ));
        }
        std::fs::remove_dir_all(&candidate).map_err(|error| {
            format!(
                "remove stale restore-drill directory '{}': {error}",
                candidate.display()
            )
        })?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct RestoreResourceNames {
    container: String,
    copy_container: String,
    volume: String,
    expires_at: i64,
}

impl RestoreResourceNames {
    fn for_run(run_id: uuid::Uuid) -> Self {
        let suffix = run_id.simple();
        Self {
            container: format!("ff-restore-drill-{suffix}"),
            copy_container: format!("ff-restore-drill-{suffix}-copy"),
            volume: format!("ff-restore-drill-{suffix}-data"),
            expires_at: chrono::Utc::now().timestamp() + DRILL_RESOURCE_TTL_SECS,
        }
    }

    fn labels(&self, run_id: uuid::Uuid) -> Vec<String> {
        vec![
            "--label".into(),
            "forgefleet.restore-drill=true".into(),
            "--label".into(),
            format!("forgefleet.restore-drill.run-id={run_id}"),
            "--label".into(),
            format!("forgefleet.restore-drill.expires-at={}", self.expires_at),
        ]
    }
}

#[async_trait::async_trait]
trait PostgresProofRuntime: Sync {
    async fn scavenge(&self) -> Result<(), String>;
    async fn cleanup(&self, resources: &RestoreResourceNames) -> Result<(), String>;
    async fn resolve_pg16_image(&self) -> Result<String, String>;
    async fn prepare(
        &self,
        pgdata: &Path,
        run_id: uuid::Uuid,
        resources: &RestoreResourceNames,
        image_id: &str,
    ) -> Result<(), String>;
    async fn start(
        &self,
        run_id: uuid::Uuid,
        resources: &RestoreResourceNames,
        image_id: &str,
    ) -> Result<(), String>;
    async fn wait_ready(&self, resources: &RestoreResourceNames) -> Result<(), String>;
    async fn application_read(&self, resources: &RestoreResourceNames) -> Result<(), String>;
}

struct DockerPostgresProof;

#[async_trait::async_trait]
impl PostgresProofRuntime for DockerPostgresProof {
    async fn scavenge(&self) -> Result<(), String> {
        scavenge_stale_docker_resources().await
    }

    async fn cleanup(&self, resources: &RestoreResourceNames) -> Result<(), String> {
        let mut errors = Vec::new();
        for name in [&resources.copy_container, &resources.container] {
            if let Err(error) = docker_allow_absent(&["rm", "-f", name]).await {
                errors.push(error);
            }
        }
        if let Err(error) = docker_allow_absent(&["volume", "rm", "-f", &resources.volume]).await {
            errors.push(error);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    async fn resolve_pg16_image(&self) -> Result<String, String> {
        let image_id = docker_output(
            &[
                "image",
                "inspect",
                "--format",
                "{{.Id}}",
                POSTGRES_PROOF_IMAGE,
            ],
            DOCKER_COMMAND_TIMEOUT,
        )
        .await?
        .trim()
        .to_string();
        if !valid_sha256_id(&image_id) {
            return Err(format!(
                "PostgreSQL proof image '{}' did not resolve to an immutable sha256 ID",
                POSTGRES_PROOF_IMAGE
            ));
        }
        let version = docker_output(
            &[
                "run",
                "--rm",
                "--network",
                "none",
                "--pull",
                "never",
                "--entrypoint",
                "postgres",
                &image_id,
                "--version",
            ],
            DOCKER_COMMAND_TIMEOUT,
        )
        .await?;
        if !version
            .split_whitespace()
            .any(|part| part == "16" || part.starts_with("16."))
        {
            return Err(format!(
                "resolved proof image is not PostgreSQL 16: {}",
                version.trim()
            ));
        }
        Ok(image_id)
    }

    async fn prepare(
        &self,
        pgdata: &Path,
        run_id: uuid::Uuid,
        resources: &RestoreResourceNames,
        image_id: &str,
    ) -> Result<(), String> {
        let pgdata = pgdata
            .canonicalize()
            .map_err(|error| format!("resolve extracted PGDATA: {error}"))?;
        let pgdata = pgdata
            .to_str()
            .ok_or_else(|| "extracted PGDATA path is not valid UTF-8".to_string())?;

        let mut volume_args = vec!["volume".into(), "create".into()];
        volume_args.extend(resources.labels(run_id));
        volume_args.push(resources.volume.clone());
        docker_output_owned(&volume_args, DOCKER_COMMAND_TIMEOUT).await?;

        let mut args = vec![
            "run".into(),
            "--rm".into(),
            "--name".into(),
            resources.copy_container.clone(),
            "--network".into(),
            "none".into(),
            "--pull".into(),
            "never".into(),
            "--cpus".into(),
            "2".into(),
            "--memory".into(),
            "4g".into(),
            "--pids-limit".into(),
            "512".into(),
            "--cap-drop".into(),
            "ALL".into(),
            "--cap-add".into(),
            "CHOWN".into(),
            "--cap-add".into(),
            "DAC_OVERRIDE".into(),
            "--cap-add".into(),
            "FOWNER".into(),
            "--security-opt".into(),
            "no-new-privileges".into(),
        ];
        args.extend(resources.labels(run_id));
        args.extend([
            "-v".into(),
            format!("{pgdata}:/source:ro"),
            "-v".into(),
            format!("{}:/target", resources.volume),
            "--entrypoint".into(),
            "/bin/sh".into(),
            image_id.into(),
            "-ceu".into(),
            "cp -a /source/. /target/; chown -R postgres:postgres /target; chmod 0700 /target; printf 'local all all trust\\n' > /target/ff_restore_drill_hba.conf; chown postgres:postgres /target/ff_restore_drill_hba.conf; chmod 0600 /target/ff_restore_drill_hba.conf".into(),
        ]);
        docker_output_owned(&args, DOCKER_COMMAND_TIMEOUT).await?;
        Ok(())
    }

    async fn start(
        &self,
        run_id: uuid::Uuid,
        resources: &RestoreResourceNames,
        image_id: &str,
    ) -> Result<(), String> {
        let mut args = vec![
            "run".into(),
            "-d".into(),
            "--rm".into(),
            "--name".into(),
            resources.container.clone(),
            "--network".into(),
            "none".into(),
            "--pull".into(),
            "never".into(),
            "--cpus".into(),
            "2".into(),
            "--memory".into(),
            "4g".into(),
            "--pids-limit".into(),
            "512".into(),
            "--read-only".into(),
            "--tmpfs".into(),
            "/tmp:rw,noexec,nosuid,size=64m,mode=1777".into(),
            "--cap-drop".into(),
            "ALL".into(),
            "--security-opt".into(),
            "no-new-privileges".into(),
            "--user".into(),
            "postgres".into(),
        ];
        args.extend(resources.labels(run_id));
        args.extend([
            "-v".into(),
            format!("{}:/var/lib/postgresql/data", resources.volume),
            "--entrypoint".into(),
            "postgres".into(),
            image_id.into(),
            "-D".into(),
            "/var/lib/postgresql/data".into(),
        ]);
        for setting in [
            "listen_addresses=",
            "unix_socket_directories=/tmp",
            "archive_mode=off",
            "archive_command=",
            "restore_command=",
            "shared_preload_libraries=",
            "local_preload_libraries=",
            "session_preload_libraries=",
            "ssl=off",
            "fsync=off",
            "synchronous_commit=off",
            "max_connections=20",
            "hba_file=/var/lib/postgresql/data/ff_restore_drill_hba.conf",
        ] {
            args.push("-c".into());
            args.push(setting.into());
        }
        docker_output_owned(&args, DOCKER_COMMAND_TIMEOUT).await?;
        Ok(())
    }

    async fn wait_ready(&self, resources: &RestoreResourceNames) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + POSTGRES_READY_TIMEOUT;
        let mut last_error = String::new();
        while tokio::time::Instant::now() < deadline {
            match docker_output_owned(
                &postgres_ready_probe_args(&resources.container),
                Duration::from_secs(5),
            )
            .await
            {
                Ok(_) => return Ok(()),
                Err(error) => last_error = error,
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        Err(format!(
            "isolated PostgreSQL did not become ready within {}s: {last_error}",
            POSTGRES_READY_TIMEOUT.as_secs()
        ))
    }

    async fn application_read(&self, resources: &RestoreResourceNames) -> Result<(), String> {
        let output = docker_output_owned(
            &postgres_application_read_args(&resources.container),
            DOCKER_COMMAND_TIMEOUT,
        )
        .await?;
        if output.trim() != "t" {
            return Err(format!(
                "isolated PostgreSQL 16 application read returned unexpected output: {:?}",
                output.trim()
            ));
        }
        Ok(())
    }
}

fn postgres_ready_probe_args(container: &str) -> Vec<String> {
    [
        "exec",
        container,
        "pg_isready",
        "-h",
        "/tmp",
        "-U",
        POSTGRES_PROOF_USER,
        "-d",
        POSTGRES_PROOF_DATABASE,
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn postgres_application_read_args(container: &str) -> Vec<String> {
    [
        "exec",
        container,
        "psql",
        "-h",
        "/tmp",
        "-U",
        POSTGRES_PROOF_USER,
        "-d",
        POSTGRES_PROOF_DATABASE,
        "-XAt",
        "-v",
        "ON_ERROR_STOP=1",
        "-c",
        POSTGRES_PROOF_QUERY,
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

async fn prove_postgres_restore(pgdata: &Path, run_id: uuid::Uuid) -> Result<String, String> {
    prove_postgres_restore_with(&DockerPostgresProof, pgdata, run_id).await
}

async fn prove_postgres_restore_with<R: PostgresProofRuntime>(
    runtime: &R,
    pgdata: &Path,
    run_id: uuid::Uuid,
) -> Result<String, String> {
    let resources = RestoreResourceNames::for_run(run_id);
    let proof = async {
        runtime.scavenge().await?;
        runtime.cleanup(&resources).await?;
        let image_id = runtime.resolve_pg16_image().await?;
        runtime
            .prepare(pgdata, run_id, &resources, &image_id)
            .await?;
        runtime.start(run_id, &resources, &image_id).await?;
        runtime.wait_ready(&resources).await?;
        runtime.application_read(&resources).await?;
        Ok::<_, String>(format!(
            "isolated PostgreSQL 16 startup/readiness/application SELECT passed (image={image_id})"
        ))
    }
    .await;
    let cleanup = runtime.cleanup(&resources).await;
    match (proof, cleanup) {
        (Ok(detail), Ok(())) => Ok(detail),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(format!(
            "restore proof passed but bounded cleanup failed: {cleanup_error}"
        )),
        (Err(error), Err(cleanup_error)) => Err(format!(
            "{error}; bounded cleanup also failed: {cleanup_error}"
        )),
    }
}

#[async_trait::async_trait]
trait FalkorProofRuntime: Sync {
    async fn scavenge(&self) -> Result<(), String>;
    async fn cleanup(&self, resources: &RestoreResourceNames) -> Result<(), String>;
    async fn resolve_image(&self) -> Result<String, String>;
    async fn prepare(
        &self,
        data_dir: &Path,
        run_id: uuid::Uuid,
        resources: &RestoreResourceNames,
        image_id: &str,
    ) -> Result<(), String>;
    async fn start(
        &self,
        run_id: uuid::Uuid,
        resources: &RestoreResourceNames,
        image_id: &str,
    ) -> Result<(), String>;
    async fn wait_ready(&self, resources: &RestoreResourceNames) -> Result<(), String>;
    async fn read_counts(
        &self,
        resources: &RestoreResourceNames,
    ) -> Result<(u64, u64, u64), String>;
}

struct DockerFalkorProof;

#[async_trait::async_trait]
impl FalkorProofRuntime for DockerFalkorProof {
    async fn scavenge(&self) -> Result<(), String> {
        scavenge_stale_docker_resources().await
    }

    async fn cleanup(&self, resources: &RestoreResourceNames) -> Result<(), String> {
        let mut errors = Vec::new();
        for name in [&resources.copy_container, &resources.container] {
            if let Err(error) = docker_allow_absent(&["rm", "-f", name]).await {
                errors.push(error);
            }
        }
        if let Err(error) = docker_allow_absent(&["volume", "rm", "-f", &resources.volume]).await {
            errors.push(error);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    async fn resolve_image(&self) -> Result<String, String> {
        if !valid_pinned_image_reference(FALKORDB_PROOF_IMAGE) {
            return Err("FalkorDB proof image is not pinned by sha256 digest".into());
        }
        let image_id = docker_output(
            &[
                "image",
                "inspect",
                "--format",
                "{{.Id}}",
                FALKORDB_PROOF_IMAGE,
            ],
            DOCKER_COMMAND_TIMEOUT,
        )
        .await?
        .trim()
        .to_string();
        if !valid_sha256_id(&image_id) {
            return Err(format!(
                "FalkorDB proof image did not resolve to an immutable sha256 ID: {image_id:?}"
            ));
        }
        Ok(image_id)
    }

    async fn prepare(
        &self,
        data_dir: &Path,
        run_id: uuid::Uuid,
        resources: &RestoreResourceNames,
        image_id: &str,
    ) -> Result<(), String> {
        let data_dir = data_dir
            .canonicalize()
            .map_err(|error| format!("resolve extracted FalkorDB data directory: {error}"))?;
        let data_dir = data_dir
            .to_str()
            .ok_or_else(|| "extracted FalkorDB data path is not valid UTF-8".to_string())?;

        let mut volume_args = vec!["volume".into(), "create".into()];
        volume_args.extend(resources.labels(run_id));
        volume_args.push(resources.volume.clone());
        docker_output_owned(&volume_args, DOCKER_COMMAND_TIMEOUT).await?;

        let mut args = vec![
            "run".into(),
            "--rm".into(),
            "--name".into(),
            resources.copy_container.clone(),
            "--network".into(),
            "none".into(),
            "--pull".into(),
            "never".into(),
            "--cpus".into(),
            "1".into(),
            "--memory".into(),
            "1g".into(),
            "--pids-limit".into(),
            "128".into(),
            "--cap-drop".into(),
            "ALL".into(),
            "--cap-add".into(),
            "DAC_OVERRIDE".into(),
            "--cap-add".into(),
            "FOWNER".into(),
            "--security-opt".into(),
            "no-new-privileges".into(),
        ];
        args.extend(resources.labels(run_id));
        args.extend([
            "-v".into(),
            format!("{data_dir}:/source:ro"),
            "-v".into(),
            format!("{}:/target", resources.volume),
            "--entrypoint".into(),
            "/bin/sh".into(),
            image_id.into(),
            "-ceu".into(),
            "cp -R /source/. /target/".into(),
        ]);
        docker_output_owned(&args, DOCKER_COMMAND_TIMEOUT).await?;
        Ok(())
    }

    async fn start(
        &self,
        run_id: uuid::Uuid,
        resources: &RestoreResourceNames,
        image_id: &str,
    ) -> Result<(), String> {
        let args = falkordb_start_args(run_id, resources, image_id);
        docker_output_owned(&args, DOCKER_COMMAND_TIMEOUT).await?;
        Ok(())
    }

    async fn wait_ready(&self, resources: &RestoreResourceNames) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + FALKORDB_READY_TIMEOUT;
        let mut last_error = String::new();
        while tokio::time::Instant::now() < deadline {
            match docker_output(
                &["exec", &resources.container, "redis-cli", "--raw", "PING"],
                Duration::from_secs(5),
            )
            .await
            {
                Ok(output) if output.trim() == "PONG" => return Ok(()),
                Ok(output) => last_error = format!("unexpected PING response: {output:?}"),
                Err(error) => last_error = error,
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        Err(format!(
            "isolated FalkorDB did not become ready within {}s: {last_error}",
            FALKORDB_READY_TIMEOUT.as_secs()
        ))
    }

    async fn read_counts(
        &self,
        resources: &RestoreResourceNames,
    ) -> Result<(u64, u64, u64), String> {
        let key_output = docker_output(
            &[
                "exec",
                &resources.container,
                "redis-cli",
                "--json",
                "DBSIZE",
            ],
            DOCKER_COMMAND_TIMEOUT,
        )
        .await?;
        let key_count = parse_json_u64(&key_output, "DBSIZE")?;

        let graph_output = docker_output(
            &[
                "exec",
                &resources.container,
                "redis-cli",
                "--json",
                "GRAPH.LIST",
            ],
            DOCKER_COMMAND_TIMEOUT,
        )
        .await?;
        let graphs = parse_graph_list(&graph_output)?;
        let mut node_count = 0_u64;
        let deadline = tokio::time::Instant::now() + FALKORDB_ALL_GRAPHS_TIMEOUT;
        for graph in &graphs {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "read-only graph proof exceeded total {}s timeout",
                    FALKORDB_ALL_GRAPHS_TIMEOUT.as_secs()
                ));
            }
            let args = falkordb_graph_query_args(&resources.container, graph);
            let output =
                docker_output_owned(&args, FALKORDB_GRAPH_QUERY_TIMEOUT.min(remaining)).await?;
            node_count = node_count
                .checked_add(parse_graph_node_count(&output)?)
                .ok_or_else(|| "FalkorDB graph-node count overflow".to_string())?;
        }
        Ok((key_count, graphs.len() as u64, node_count))
    }
}

fn falkordb_start_args(
    run_id: uuid::Uuid,
    resources: &RestoreResourceNames,
    image_id: &str,
) -> Vec<String> {
    let mut args = vec![
        "run".into(),
        "-d".into(),
        "--rm".into(),
        "--name".into(),
        resources.container.clone(),
        "--network".into(),
        "none".into(),
        "--pull".into(),
        "never".into(),
        "--cpus".into(),
        "2".into(),
        "--memory".into(),
        "4g".into(),
        "--pids-limit".into(),
        "512".into(),
        "--read-only".into(),
        "--tmpfs".into(),
        "/tmp:rw,noexec,nosuid,size=64m,mode=1777".into(),
        "--cap-drop".into(),
        "ALL".into(),
        "--security-opt".into(),
        "no-new-privileges".into(),
        "-e".into(),
        "BROWSER=0".into(),
        "-e".into(),
        "REDIS_ARGS=--appendonly yes --appendfsync no".into(),
    ];
    args.extend(resources.labels(run_id));
    args.extend([
        "-v".into(),
        format!("{}:/var/lib/falkordb/data", resources.volume),
        image_id.into(),
    ]);
    args
}

fn falkordb_graph_query_args(container: &str, graph: &str) -> Vec<String> {
    [
        "exec",
        container,
        "redis-cli",
        "--json",
        "GRAPH.RO_QUERY",
        graph,
        "MATCH (n) RETURN count(n) AS node_count",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

async fn prove_falkordb_restore(
    data_dir: &Path,
    run_id: uuid::Uuid,
) -> Result<FalkorProofEvidence, String> {
    prove_falkordb_restore_with(&DockerFalkorProof, data_dir, run_id).await
}

async fn prove_falkordb_restore_with<R: FalkorProofRuntime>(
    runtime: &R,
    data_dir: &Path,
    run_id: uuid::Uuid,
) -> Result<FalkorProofEvidence, String> {
    let resources = RestoreResourceNames::for_run(run_id);
    let proof = async {
        runtime.scavenge().await?;
        runtime.cleanup(&resources).await?;
        let image_id = runtime.resolve_image().await?;
        runtime
            .prepare(data_dir, run_id, &resources, &image_id)
            .await?;
        runtime.start(run_id, &resources, &image_id).await?;
        runtime.wait_ready(&resources).await?;
        let (key_count, graph_count, node_count) = runtime.read_counts(&resources).await?;
        if key_count < FALKORDB_EXPECTED_MIN_KEYS {
            return Err(format!(
                "restored FalkorDB key count {key_count} is below expected minimum {FALKORDB_EXPECTED_MIN_KEYS}"
            ));
        }
        if node_count < FALKORDB_EXPECTED_MIN_GRAPH_NODES {
            return Err(format!(
                "restored FalkorDB graph-node count {node_count} is below expected minimum {FALKORDB_EXPECTED_MIN_GRAPH_NODES}"
            ));
        }
        Ok(FalkorProofEvidence {
            image_id,
            key_count,
            graph_count,
            node_count,
        })
    }
    .await;
    let cleanup = runtime.cleanup(&resources).await;
    match (proof, cleanup) {
        (Ok(evidence), Ok(())) => Ok(evidence),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(format!(
            "FalkorDB restore proof passed but bounded cleanup failed: {cleanup_error}"
        )),
        (Err(error), Err(cleanup_error)) => Err(format!(
            "{error}; bounded cleanup also failed: {cleanup_error}"
        )),
    }
}

fn parse_json_u64(output: &str, label: &str) -> Result<u64, String> {
    serde_json::from_str::<serde_json::Value>(output.trim())
        .map_err(|error| format!("parse {label} JSON failed: {error}"))?
        .as_u64()
        .ok_or_else(|| format!("{label} JSON is not an unsigned integer"))
}

fn parse_graph_list(output: &str) -> Result<Vec<String>, String> {
    let graphs: Vec<String> = serde_json::from_str(output.trim())
        .map_err(|error| format!("parse GRAPH.LIST JSON failed: {error}"))?;
    if graphs.len() > FALKORDB_MAX_GRAPHS {
        return Err(format!(
            "GRAPH.LIST returned {} graphs, above safety limit {FALKORDB_MAX_GRAPHS}",
            graphs.len()
        ));
    }
    if graphs
        .iter()
        .any(|graph| graph.is_empty() || graph.contains('\0'))
    {
        return Err("GRAPH.LIST returned an empty or NUL-containing graph name".into());
    }
    Ok(graphs)
}

fn parse_graph_node_count(output: &str) -> Result<u64, String> {
    let value: serde_json::Value = serde_json::from_str(output.trim())
        .map_err(|error| format!("parse GRAPH.RO_QUERY JSON failed: {error}"))?;
    value
        .get(1)
        .and_then(serde_json::Value::as_array)
        .and_then(|rows| rows.first())
        .and_then(serde_json::Value::as_array)
        .and_then(|row| row.first())
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "GRAPH.RO_QUERY JSON did not contain one unsigned node_count".into())
}

fn valid_pinned_image_reference(value: &str) -> bool {
    value
        .rsplit_once("@sha256:")
        .map(|(repository, digest)| {
            !repository.is_empty()
                && digest.len() == 64
                && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        .unwrap_or(false)
}

fn valid_sha256_checksum(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_sha256_id(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

async fn docker_output(args: &[&str], timeout: Duration) -> Result<String, String> {
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
    docker_output_owned(&args, timeout).await
}

async fn docker_output_owned(args: &[String], timeout: Duration) -> Result<String, String> {
    let mut command = tokio::process::Command::new("docker");
    command.args(args).kill_on_drop(true);
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| format!("docker command timed out after {}s", timeout.as_secs()))?
        .map_err(|error| format!("launch docker command failed: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "docker command failed ({}): {}",
            output.status,
            truncate_detail(stderr.trim(), 1000)
        ));
    }
    String::from_utf8(output.stdout).map_err(|_| "docker output was not UTF-8".to_string())
}

async fn docker_allow_absent(args: &[&str]) -> Result<(), String> {
    match docker_output(args, DOCKER_COMMAND_TIMEOUT).await {
        Ok(_) => Ok(()),
        Err(error)
            if error.contains("No such container")
                || error.contains("No such volume")
                || error.contains("no such container")
                || error.contains("no such volume") =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn truncate_detail(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

async fn scavenge_stale_docker_resources() -> Result<(), String> {
    for kind in ["container", "volume"] {
        let list_args = if kind == "container" {
            vec![
                "ps",
                "-aq",
                "--filter",
                "label=forgefleet.restore-drill=true",
            ]
        } else {
            vec![
                "volume",
                "ls",
                "-q",
                "--filter",
                "label=forgefleet.restore-drill=true",
            ]
        };
        let objects = docker_output(&list_args, DOCKER_COMMAND_TIMEOUT).await?;
        for object in objects
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            if !object
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            {
                return Err(format!("unsafe Docker restore resource ID: {object:?}"));
            }
            let inspect_args = if kind == "container" {
                vec![
                    "inspect",
                    "--format",
                    "{{index .Config.Labels \"forgefleet.restore-drill.expires-at\"}}",
                    object,
                ]
            } else {
                vec![
                    "volume",
                    "inspect",
                    "--format",
                    "{{index .Labels \"forgefleet.restore-drill.expires-at\"}}",
                    object,
                ]
            };
            let expires = docker_output(&inspect_args, DOCKER_COMMAND_TIMEOUT).await?;
            let expires = expires
                .trim()
                .parse::<i64>()
                .map_err(|_| format!("restore resource {object} has invalid expires-at label"))?;
            if expires > chrono::Utc::now().timestamp() {
                continue;
            }
            if kind == "container" {
                docker_allow_absent(&["rm", "-f", object]).await?;
            } else {
                docker_allow_absent(&["volume", "rm", "-f", object]).await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_policy() -> DrillPolicy {
        DrillPolicy {
            max_encrypted_bytes: 64 * GIB,
            max_extracted_bytes: 256 * GIB,
            max_files: 5_000_000,
            expansion_ratio: 8,
            reserve_bytes: 20 * GIB,
        }
    }

    #[test]
    fn alerts_when_drill_failed_regardless_of_history() {
        assert!(should_alert(false, Some(0.0), STALE_DRILL_DAYS));
        assert!(should_alert(false, None, STALE_DRILL_DAYS));
    }

    #[test]
    fn alerts_when_no_success_ever() {
        assert!(should_alert(true, None, STALE_DRILL_DAYS));
    }

    #[test]
    fn alerts_when_last_success_is_stale() {
        assert!(should_alert(true, Some(3.0), STALE_DRILL_DAYS));
    }

    #[test]
    fn quiet_when_drill_passed_and_recent() {
        assert!(!should_alert(true, Some(0.0), STALE_DRILL_DAYS));
        assert!(!should_alert(true, Some(1.9), STALE_DRILL_DAYS));
    }

    #[test]
    fn failed_outcome_has_no_metrics() {
        let run_id = uuid::Uuid::new_v4();
        let o = DrillOutcome::failed(run_id, None, "pg-x.tar.gz.age", "decrypt", "boom".into());
        assert_eq!(o.run_id, run_id);
        assert!(!o.success);
        assert_eq!(o.stage, "decrypt");
        assert!(o.extracted_bytes.is_none());
        assert!(o.verifybackup.is_none());
    }

    #[test]
    fn exact_backup_name_rejects_paths_and_accepts_one_component() {
        assert!(safe_backup_file_name("pg-20260804T120000Z.tar.gz.age"));
        for unsafe_name in [
            "",
            "../backup.age",
            "postgres/backup.age",
            "/tmp/backup.age",
            ".",
            "..",
        ] {
            assert!(!safe_backup_file_name(unsafe_name), "{unsafe_name:?}");
        }
    }

    #[test]
    fn decoded_tar_reader_errors_instead_of_silent_truncation() {
        use std::io::Read;

        let input = std::io::Cursor::new(b"four".to_vec());
        let mut limited = ByteLimitReader::new(input, 2, "decoded tar byte limit exceeded");
        let mut decoded = Vec::new();
        let error = limited.read_to_end(&mut decoded).unwrap_err();
        assert_eq!(decoded, b"fo");
        assert!(error.to_string().contains("decoded tar byte limit"));
    }

    #[test]
    fn stale_drill_directory_scavenger_is_prefix_and_age_bounded() {
        let scratch = tempfile::tempdir().unwrap();
        let stale = scratch.path().join("ff-drill-stale");
        let unrelated = scratch.path().join("operator-data");
        std::fs::create_dir(&stale).unwrap();
        std::fs::create_dir(&unrelated).unwrap();
        std::fs::write(stale.join("plaintext"), b"secret").unwrap();

        let future =
            std::time::SystemTime::now() + Duration::from_secs(DRILL_RESOURCE_TTL_SECS as u64 + 1);
        scavenge_stale_drill_dirs(scratch.path(), future).unwrap();
        assert!(!stale.exists());
        assert!(unrelated.exists());
    }

    #[derive(Default)]
    struct MockPostgresProof {
        calls: std::sync::Mutex<Vec<&'static str>>,
        fail_at: Option<&'static str>,
    }

    impl MockPostgresProof {
        fn record(&self, stage: &'static str) -> Result<(), String> {
            self.calls.lock().unwrap().push(stage);
            if self.fail_at == Some(stage) {
                Err(format!("mock {stage} failure"))
            } else {
                Ok(())
            }
        }
    }

    #[async_trait::async_trait]
    impl PostgresProofRuntime for MockPostgresProof {
        async fn scavenge(&self) -> Result<(), String> {
            self.record("scavenge")
        }

        async fn cleanup(&self, _resources: &RestoreResourceNames) -> Result<(), String> {
            self.record("cleanup")
        }

        async fn resolve_pg16_image(&self) -> Result<String, String> {
            self.record("resolve")?;
            Ok(format!("sha256:{}", "a".repeat(64)))
        }

        async fn prepare(
            &self,
            _pgdata: &Path,
            _run_id: uuid::Uuid,
            _resources: &RestoreResourceNames,
            _image_id: &str,
        ) -> Result<(), String> {
            self.record("prepare")
        }

        async fn start(
            &self,
            _run_id: uuid::Uuid,
            _resources: &RestoreResourceNames,
            _image_id: &str,
        ) -> Result<(), String> {
            self.record("start")
        }

        async fn wait_ready(&self, _resources: &RestoreResourceNames) -> Result<(), String> {
            self.record("ready")
        }

        async fn application_read(&self, _resources: &RestoreResourceNames) -> Result<(), String> {
            self.record("select")
        }
    }

    #[tokio::test]
    async fn full_postgres_proof_requires_start_ready_and_select_then_cleans() {
        let runtime = MockPostgresProof::default();
        let pgdata = tempfile::tempdir().unwrap();
        let result = prove_postgres_restore_with(&runtime, pgdata.path(), uuid::Uuid::new_v4())
            .await
            .unwrap();
        assert!(result.contains("application SELECT passed"));
        assert_eq!(
            *runtime.calls.lock().unwrap(),
            [
                "scavenge", "cleanup", "resolve", "prepare", "start", "ready", "select", "cleanup"
            ]
        );
    }

    #[tokio::test]
    async fn full_postgres_proof_cleans_after_start_failure() {
        let runtime = MockPostgresProof {
            fail_at: Some("start"),
            ..Default::default()
        };
        let pgdata = tempfile::tempdir().unwrap();
        let error = prove_postgres_restore_with(&runtime, pgdata.path(), uuid::Uuid::new_v4())
            .await
            .unwrap_err();
        assert!(error.contains("mock start failure"));
        assert_eq!(runtime.calls.lock().unwrap().last(), Some(&"cleanup"));
    }

    struct MockFalkorProof {
        calls: std::sync::Mutex<Vec<&'static str>>,
        fail_at: Option<&'static str>,
        cleanup_calls: std::sync::atomic::AtomicUsize,
        fail_final_cleanup: bool,
        counts: (u64, u64, u64),
    }

    impl MockFalkorProof {
        fn healthy() -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                fail_at: None,
                cleanup_calls: std::sync::atomic::AtomicUsize::new(0),
                fail_final_cleanup: false,
                counts: (3, 2, 17),
            }
        }

        fn record(&self, stage: &'static str) -> Result<(), String> {
            self.calls.lock().unwrap().push(stage);
            if self.fail_at == Some(stage) {
                Err(format!("mock {stage} failure"))
            } else {
                Ok(())
            }
        }
    }

    #[async_trait::async_trait]
    impl FalkorProofRuntime for MockFalkorProof {
        async fn scavenge(&self) -> Result<(), String> {
            self.record("scavenge")
        }

        async fn cleanup(&self, _resources: &RestoreResourceNames) -> Result<(), String> {
            self.record("cleanup")?;
            let call = self
                .cleanup_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            if self.fail_final_cleanup && call == 2 {
                Err("mock final cleanup failure".into())
            } else {
                Ok(())
            }
        }

        async fn resolve_image(&self) -> Result<String, String> {
            self.record("resolve")?;
            Ok(format!("sha256:{}", "b".repeat(64)))
        }

        async fn prepare(
            &self,
            _data_dir: &Path,
            _run_id: uuid::Uuid,
            _resources: &RestoreResourceNames,
            _image_id: &str,
        ) -> Result<(), String> {
            self.record("prepare")
        }

        async fn start(
            &self,
            _run_id: uuid::Uuid,
            _resources: &RestoreResourceNames,
            _image_id: &str,
        ) -> Result<(), String> {
            self.record("start")
        }

        async fn wait_ready(&self, _resources: &RestoreResourceNames) -> Result<(), String> {
            self.record("ready")
        }

        async fn read_counts(
            &self,
            _resources: &RestoreResourceNames,
        ) -> Result<(u64, u64, u64), String> {
            self.record("read")?;
            Ok(self.counts)
        }
    }

    #[tokio::test]
    async fn full_falkordb_proof_requires_nonempty_counts_then_cleans() {
        let runtime = MockFalkorProof::healthy();
        let data_dir = tempfile::tempdir().unwrap();
        let evidence = prove_falkordb_restore_with(&runtime, data_dir.path(), uuid::Uuid::new_v4())
            .await
            .unwrap();
        assert_eq!(evidence.key_count, 3);
        assert_eq!(evidence.graph_count, 2);
        assert_eq!(evidence.node_count, 17);
        assert_eq!(
            *runtime.calls.lock().unwrap(),
            [
                "scavenge", "cleanup", "resolve", "prepare", "start", "ready", "read", "cleanup"
            ]
        );
    }

    #[tokio::test]
    async fn falkordb_zero_graph_nodes_fails_and_cleans() {
        let runtime = MockFalkorProof {
            counts: (3, 2, 0),
            ..MockFalkorProof::healthy()
        };
        let data_dir = tempfile::tempdir().unwrap();
        let error = prove_falkordb_restore_with(&runtime, data_dir.path(), uuid::Uuid::new_v4())
            .await
            .unwrap_err();
        assert!(error.contains("graph-node count 0"));
        assert_eq!(runtime.calls.lock().unwrap().last(), Some(&"cleanup"));
    }

    #[tokio::test]
    async fn falkordb_cleanup_failure_cannot_produce_success() {
        let runtime = MockFalkorProof {
            fail_final_cleanup: true,
            ..MockFalkorProof::healthy()
        };
        let data_dir = tempfile::tempdir().unwrap();
        let error = prove_falkordb_restore_with(&runtime, data_dir.path(), uuid::Uuid::new_v4())
            .await
            .unwrap_err();
        assert!(error.contains("proof passed but bounded cleanup failed"));
    }

    #[test]
    fn falkordb_runtime_is_digest_pinned_networkless_and_read_only() {
        assert!(valid_pinned_image_reference(FALKORDB_PROOF_IMAGE));
        assert!(!valid_pinned_image_reference("falkordb/falkordb:latest"));
        assert!(FALKORDB_GRAPH_QUERY_TIMEOUT < FALKORDB_ALL_GRAPHS_TIMEOUT);
        let run_id = uuid::Uuid::new_v4();
        let resources = RestoreResourceNames::for_run(run_id);
        let image_id = format!("sha256:{}", "c".repeat(64));
        let args = falkordb_start_args(run_id, &resources, &image_id);
        assert!(args.windows(2).any(|pair| pair == ["--network", "none"]));
        assert!(args.windows(2).any(|pair| pair == ["--pull", "never"]));
        assert!(args.iter().any(|arg| arg == "--read-only"));
        assert_eq!(args.last(), Some(&image_id));

        let query = falkordb_graph_query_args("drill", "cortex");
        assert!(query.iter().any(|arg| arg == "GRAPH.RO_QUERY"));
        assert!(!query.iter().any(|arg| arg == "GRAPH.QUERY"));
    }

    #[test]
    fn falkordb_json_counts_are_strict() {
        assert_eq!(parse_json_u64("6\n", "DBSIZE").unwrap(), 6);
        assert_eq!(
            parse_graph_list("[\"social-demo\",\"cortex\"]\n").unwrap(),
            ["social-demo", "cortex"]
        );
        assert_eq!(
            parse_graph_node_count(
                r#"[["node_count"],[[42]],["Query internal execution time: 0.1 milliseconds"]]"#
            )
            .unwrap(),
            42
        );
        assert!(parse_graph_node_count(r#"[["node_count"],[],[]]"#).is_err());
    }

    #[test]
    fn falkordb_receipt_binds_checksum_image_and_observed_counts() {
        let checksum = "d".repeat(64);
        let evidence = FalkorProofEvidence {
            image_id: format!("sha256:{}", "e".repeat(64)),
            key_count: 3,
            graph_count: 2,
            node_count: 17,
        };
        let receipt = falkordb_receipt(&evidence, &checksum);
        assert_eq!(receipt["input_checksum_sha256"], checksum);
        assert_eq!(receipt["image_reference"], FALKORDB_PROOF_IMAGE);
        assert_eq!(receipt["image_id"], evidence.image_id);
        assert_eq!(receipt["network"], "none");
        assert_eq!(receipt["query_mode"], "GRAPH.RO_QUERY");
        assert_eq!(receipt["observed_keys"], 3);
        assert_eq!(receipt["observed_graph_nodes"], 17);
    }

    #[test]
    fn restored_cluster_probes_use_the_forgefleet_bootstrap_identity() {
        for args in [
            postgres_ready_probe_args("drill-container"),
            postgres_application_read_args("drill-container"),
        ] {
            let user_index = args.iter().position(|arg| arg == "-U").unwrap();
            let database_index = args.iter().position(|arg| arg == "-d").unwrap();
            assert_eq!(args[user_index + 1], POSTGRES_PROOF_USER);
            assert_eq!(args[database_index + 1], POSTGRES_PROOF_DATABASE);
            assert!(!args.iter().any(|arg| arg == "postgres"));
            if let Some(query_index) = args.iter().position(|arg| arg == "-c") {
                assert_eq!(args[query_index + 1], POSTGRES_PROOF_QUERY);
                assert!(POSTGRES_PROOF_QUERY.contains("public.computers"));
                assert!(!POSTGRES_PROOF_QUERY.contains("fleet_nodes"));
            }
        }
    }

    #[test]
    fn live_sized_backup_fits_large_peer() {
        let encrypted = 6_236_903_728;
        let available = 577 * GIB;
        let required = test_policy().preflight(encrypted, available).unwrap();
        assert!(required < available);
        assert!(required > encrypted);
    }

    #[test]
    fn live_sized_backup_refuses_low_space_peer() {
        let encrypted = 6_236_903_728;
        let error = test_policy().preflight(encrypted, 30 * GIB).unwrap_err();
        assert!(error.contains("required="));
        assert!(error.contains("available="));
    }

    #[test]
    fn corrupt_archive_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("corrupt.tar.gz");
        std::fs::write(&archive, b"not a gzip archive").unwrap();
        let error = extract_archive_bounded(&archive, dir.path(), test_policy()).unwrap_err();
        assert!(error.contains("tar") || error.contains("archive"));
    }

    #[test]
    fn excessive_expansion_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("large.tar.gz");
        let archive_file = std::fs::File::create(&archive_path).unwrap();
        let encoder = flate2::write::GzEncoder::new(archive_file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let data = vec![0_u8; 4096];
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o600);
        header.set_cksum();
        builder
            .append_data(&mut header, "large", data.as_slice())
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();

        let mut policy = test_policy();
        policy.max_extracted_bytes = 1024;
        let error = extract_archive_bounded(&archive_path, dir.path(), policy).unwrap_err();
        assert!(error.contains("extracted-byte limit exceeded"));
    }

    #[test]
    fn pg_basebackup_shape_extracts_with_bounded_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("pg.tar.gz");
        let archive_file = std::fs::File::create(&archive_path).unwrap();
        let encoder = flate2::write::GzEncoder::new(archive_file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);

        for (path, data) in [
            ("PG_VERSION", b"16\n".as_slice()),
            ("global/pg_control", b"control".as_slice()),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o600);
            header.set_cksum();
            builder.append_data(&mut header, path, data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();

        let extraction = dir.path().join("extracted");
        std::fs::create_dir(&extraction).unwrap();
        let (files, bytes) =
            extract_archive_bounded(&archive_path, &extraction, test_policy()).unwrap();
        assert_eq!(files, 2);
        assert_eq!(bytes, 10);
        assert_eq!(
            std::fs::read_to_string(extraction.join("PG_VERSION")).unwrap(),
            "16\n"
        );
        assert_eq!(
            std::fs::read(extraction.join("global/pg_control")).unwrap(),
            b"control"
        );
    }

    #[test]
    fn falkordb_zstd_shape_extracts_with_bounded_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("falkordb.tar.zst");
        let archive_file = std::fs::File::create(&archive_path).unwrap();
        let encoder = zstd::stream::write::Encoder::new(archive_file, 1).unwrap();
        let mut builder = tar::Builder::new(encoder);
        for (path, data) in [
            ("dump.rdb", b"rdb-data".as_slice()),
            (
                "appendonlydir/appendonly.aof.1.incr.aof",
                b"aof-data".as_slice(),
            ),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o600);
            header.set_cksum();
            builder.append_data(&mut header, path, data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();

        let extraction = dir.path().join("falkordb-data");
        std::fs::create_dir(&extraction).unwrap();
        let (files, bytes) =
            extract_falkordb_archive_bounded(&archive_path, &extraction, test_policy()).unwrap();
        assert_eq!(files, 2);
        assert_eq!(bytes, 16);
        assert_eq!(
            std::fs::read(extraction.join("dump.rdb")).unwrap(),
            b"rdb-data"
        );
        assert_eq!(
            std::fs::read(extraction.join("appendonlydir/appendonly.aof.1.incr.aof")).unwrap(),
            b"aof-data"
        );
    }

    #[test]
    fn duplicate_paths_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("duplicate.tar.gz");
        let archive_file = std::fs::File::create(&archive_path).unwrap();
        let encoder = flate2::write::GzEncoder::new(archive_file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for data in [b"first".as_slice(), b"second".as_slice()] {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o600);
            header.set_cksum();
            builder.append_data(&mut header, "same", data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();

        let error = extract_archive_bounded(&archive_path, dir.path(), test_policy()).unwrap_err();
        assert!(error.contains("duplicate tar path rejected"));
    }

    #[test]
    fn link_entries_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("link.tar.gz");
        let archive_file = std::fs::File::create(&archive_path).unwrap();
        let encoder = flate2::write::GzEncoder::new(archive_file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_link_name("../escape").unwrap();
        header.set_cksum();
        builder
            .append_data(&mut header, "unsafe-link", std::io::empty())
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();

        let error = extract_archive_bounded(&archive_path, dir.path(), test_policy()).unwrap_err();
        assert!(error.contains("unsafe tar entry type rejected"));
    }

    #[test]
    fn traversal_paths_are_rejected() {
        assert!(!safe_archive_path(Path::new("../escape")));
        assert!(!safe_archive_path(Path::new("/absolute")));
        assert!(safe_archive_path(Path::new("global/pg_control")));
    }

    #[test]
    fn unique_work_directory_cleans_up_on_error_path() {
        let path = {
            let work = tempfile::Builder::new()
                .prefix("ff-drill-test-")
                .tempdir()
                .unwrap();
            let path = work.path().to_path_buf();
            std::fs::write(path.join("plaintext"), b"sensitive").unwrap();
            path
        };
        assert!(!path.exists());
    }
}
