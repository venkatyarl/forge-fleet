//! Scheduled backup **restore-drill** — verifies that the most recent Postgres
//! backup is actually *restorable*, not merely present on disk.
//!
//! Motivation: on 2026-04-18 a docker-compose consolidation wiped the fleet
//! metadata DB. Backups existed in principle, but nothing ever proved they
//! could be decrypted, extracted, and loaded — a "backup" that has never been
//! test-restored is a liability, not a safety net. The
//! [`crate::ha::backup::BackupOrchestrator`] produces `pg_basebackup -Ft -z`
//! archives every 4h; this tick (daily, leader-gated) takes the newest one all
//! the way through the restore path and records the outcome in `backup_drills`.
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
//!      cryptographic per-file validation — a bonus, not a requirement.)
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

use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlx::PgPool;
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

/// Refuse to extract an archive larger than this (decrypted size is bounded by
/// the encrypted size we check first). Fleet metadata is <100 MB; a multi-GB
/// archive means something is very wrong and we must not fill the leader's disk
/// during an unattended drill.
const MAX_ENCRYPTED_BYTES: i64 = 5 * 1024 * 1024 * 1024;

/// The alert policy name seeded by migration V130.
const POLICY_NAME: &str = "backup_restore_drill_failed";

/// Outcome of a single restore-drill pass. Persisted verbatim to
/// `backup_drills` and used to decide whether to alert.
#[derive(Debug, Clone)]
pub struct DrillOutcome {
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
    fn failed(backup_id: Option<uuid::Uuid>, file: &str, stage: &str, detail: String) -> Self {
        Self {
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

    /// Run one full drill against the newest Postgres backup. Never panics;
    /// always returns an outcome (the temp dir is cleaned on every path).
    pub async fn run_drill_once(&self) -> DrillOutcome {
        let started = std::time::Instant::now();
        let mut outcome = self.drill_inner().await;
        outcome.duration_ms = started.elapsed().as_millis() as i64;
        outcome
    }

    async fn drill_inner(&self) -> DrillOutcome {
        // 1) select — newest postgres backup row.
        let rows: Vec<(uuid::Uuid, String, i64, String)> = match sqlx::query_as(
            "SELECT id, file_name, size_bytes, checksum_sha256 \
               FROM backups WHERE database_kind = 'postgres' \
              ORDER BY created_at DESC LIMIT 20",
        )
        .fetch_all(&self.pg)
        .await
        {
            Ok(r) => r,
            Err(e) => {
                return DrillOutcome::failed(
                    None,
                    "",
                    "select",
                    format!("query backups failed: {e}"),
                );
            }
        };
        if rows.is_empty() {
            return DrillOutcome::failed(
                None,
                "",
                "select",
                "no postgres backup rows exist — the orchestrator has never \
                 produced a backup, or every row was pruned (data-loss risk)"
                    .into(),
            );
        }

        // 2) locate — pick the NEWEST backup whose ciphertext is present on THIS
        //    node. The leader holds every snapshot so it always drills the
        //    globally-newest; a peer (a cross-node `--on` drill) may lag the very
        //    newest by an rsync cycle, so it drills the newest copy it actually
        //    holds rather than false-failing — rsync lag is not un-restorability.
        let newest_name = rows[0].1.clone();
        let mut found: Option<(uuid::Uuid, String, String, std::path::PathBuf, i64)> = None;
        for (id, file_name, _recorded_bytes, checksum) in &rows {
            let path = self.backup_dir.join("postgres").join(file_name);
            if let Ok(m) = tokio::fs::metadata(&path).await {
                found = Some((
                    *id,
                    file_name.clone(),
                    checksum.clone(),
                    path,
                    m.len() as i64,
                ));
                break;
            }
        }
        let Some((id, file_name, checksum, path, disk_bytes)) = found else {
            return DrillOutcome::failed(
                Some(rows[0].0),
                &newest_name,
                "locate",
                format!(
                    "none of the {} newest postgres backups are on this node \
                     (newest={newest_name}) — rsync may not have landed, or \
                     backups live only on a peer",
                    rows.len()
                ),
            );
        };
        if disk_bytes == 0 {
            return DrillOutcome::failed(
                Some(id),
                &file_name,
                "locate",
                "backup file is 0 bytes — producer never wrote ciphertext \
                 (likely `age` CLI missing at backup time)"
                    .into(),
            );
        }
        if disk_bytes > MAX_ENCRYPTED_BYTES {
            return DrillOutcome::failed(
                Some(id),
                &file_name,
                "locate",
                format!(
                    "backup file is {disk_bytes} bytes (> {MAX_ENCRYPTED_BYTES} cap) — \
                     refusing to extract during an unattended drill"
                ),
            );
        }

        // 3) checksum — guards against bit-rot / truncated rsync.
        match crate::ha::backup::file_metadata(&path).await {
            Ok((_, actual)) if actual == checksum => {}
            Ok((_, actual)) => {
                return DrillOutcome::failed(
                    Some(id),
                    &file_name,
                    "checksum",
                    format!("sha256 mismatch: on-disk {actual} != recorded {checksum}"),
                );
            }
            Err(e) => {
                return DrillOutcome::failed(
                    Some(id),
                    &file_name,
                    "checksum",
                    format!("checksum read failed: {e}"),
                );
            }
        }

        // Everything below materializes plaintext / extracts the cluster, so do
        // it under a unique temp dir we always remove.
        let work = std::env::temp_dir().join(format!("ff-drill-{}", id.simple()));
        let _ = tokio::fs::remove_dir_all(&work).await; // stale from a killed run
        if let Err(e) = tokio::fs::create_dir_all(&work).await {
            return DrillOutcome::failed(
                Some(id),
                &file_name,
                "decrypt",
                format!("create work dir failed: {e}"),
            );
        }
        let outcome = self
            .drill_decrypt_extract(id, &file_name, &path, &work)
            .await;
        let _ = tokio::fs::remove_dir_all(&work).await;
        outcome
    }

    /// Stages 4–6 (decrypt → extract → validate), all under `work`.
    async fn drill_decrypt_extract(
        &self,
        id: uuid::Uuid,
        file_name: &str,
        enc_path: &Path,
        work: &Path,
    ) -> DrillOutcome {
        // 4) decrypt → `<work>/<file without .age>`.
        let plain_name = file_name.strip_suffix(".age").unwrap_or(file_name);
        let plain_path = work.join(plain_name);
        if let Err(e) =
            crate::ha::backup::decrypt_backup_file(&self.pg, enc_path, &plain_path).await
        {
            return DrillOutcome::failed(
                Some(id),
                file_name,
                "decrypt",
                format!("age decrypt failed: {e}"),
            );
        }

        // 5) extract — `tar -xzf` into `<work>/pgdata`.
        let pgdata = work.join("pgdata");
        if let Err(e) = tokio::fs::create_dir_all(&pgdata).await {
            return DrillOutcome::failed(
                Some(id),
                file_name,
                "extract",
                format!("create pgdata dir failed: {e}"),
            );
        }
        let tar_out = tokio::process::Command::new("tar")
            .arg("-xzf")
            .arg(&plain_path)
            .arg("-C")
            .arg(&pgdata)
            .output()
            .await;
        match tar_out {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                return DrillOutcome::failed(
                    Some(id),
                    file_name,
                    "extract",
                    format!(
                        "tar -xzf failed ({}): {}",
                        o.status,
                        String::from_utf8_lossy(&o.stderr).trim()
                    ),
                );
            }
            Err(e) => {
                return DrillOutcome::failed(
                    Some(id),
                    file_name,
                    "extract",
                    format!("tar spawn failed: {e}"),
                );
            }
        }

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
                Some(id),
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

        let (file_count, extracted_bytes) = dir_size_and_count(&pgdata).await;

        // Bonus: cryptographic per-file validation if both the manifest and the
        // tool are available. Absence is NOT a failure.
        let verifybackup = self.maybe_pg_verifybackup(&pgdata).await;

        let detail = match verifybackup {
            Some(true) => "restore drill passed; pg_verifybackup OK".to_string(),
            Some(false) => {
                // verifybackup running but failing is a real integrity problem.
                return DrillOutcome {
                    backup_id: Some(id),
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
            None => "restore drill passed (structural PGDATA validation; \
                     pg_verifybackup not run)"
                .to_string(),
        };

        DrillOutcome {
            backup_id: Some(id),
            backup_file: file_name.to_string(),
            success: true,
            stage: "done".to_string(),
            detail,
            extracted_bytes: Some(extracted_bytes),
            file_count: Some(file_count),
            pg_version,
            verifybackup,
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
        let which = tokio::process::Command::new("pg_verifybackup")
            .arg("--version")
            .output()
            .await;
        if which.map(|o| !o.status.success()).unwrap_or(true) {
            return None;
        }
        let out = tokio::process::Command::new("pg_verifybackup")
            .arg("-n") // skip WAL verification (WAL replay isn't part of a -X fetch base)
            .arg(pgdata)
            .output()
            .await;
        match out {
            Ok(o) => Some(o.status.success()),
            Err(_) => None,
        }
    }

    /// Persist a drill outcome to `backup_drills`.
    pub async fn record_drill(&self, o: &DrillOutcome) {
        if let Err(e) = sqlx::query(
            r#"
            INSERT INTO backup_drills
                (backup_id, backup_file, database_kind, success, stage, detail,
                 extracted_bytes, file_count, pg_version, verifybackup,
                 duration_ms, drill_node, finished_at)
            VALUES ($1, $2, 'postgres', $3, $4, $5, $6, $7, $8, $9, $10, $11, NOW())
            "#,
        )
        .bind(o.backup_id)
        .bind(&o.backup_file)
        .bind(o.success)
        .bind(&o.stage)
        .bind(&o.detail)
        .bind(o.extracted_bytes)
        .bind(o.file_count)
        .bind(&o.pg_version)
        .bind(o.verifybackup)
        .bind(o.duration_ms)
        .bind(&self.my_name)
        .execute(&self.pg)
        .await
        {
            tracing::error!(error = %e, "restore-drill: failed to record backup_drills row");
        }
    }

    /// Days since the newest *successful* drill, or `None` if there has never
    /// been one.
    async fn days_since_last_success(&self) -> Option<f64> {
        let secs: Option<f64> = sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM (NOW() - MAX(started_at)))::DOUBLE PRECISION \
               FROM backup_drills WHERE success = true",
        )
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

    /// Run a drill, record it, and alert if warranted. Public for the CLI to
    /// share the exact same path.
    pub async fn run_record_and_alert(&self) -> DrillOutcome {
        let outcome = self.run_drill_once().await;
        self.record_drill(&outcome).await;

        let days = self.days_since_last_success().await;
        if should_alert(outcome.success, days, STALE_DRILL_DAYS) {
            let staleness = match days {
                Some(d) => format!("{d:.1}d since last success"),
                None => "no drill has EVER succeeded".to_string(),
            };
            let msg = format!(
                "Backup restore-drill on leader '{}': backup={} stage={} — {}. \
                 ({}). A backup that cannot be restored is a silent data-loss \
                 risk (cf. the 2026-04-18 wipe).",
                self.my_name, outcome.backup_file, outcome.stage, outcome.detail, staleness
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
                "restore-drill: PASS — newest postgres backup is restorable"
            );
        }
        outcome
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
                        self.run_record_and_alert().await;
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

/// Recursively sum file count + bytes under `root`. Best-effort; unreadable
/// entries are skipped (a drill metric, not a correctness gate).
async fn dir_size_and_count(root: &Path) -> (i64, i64) {
    let mut count: i64 = 0;
    let mut bytes: i64 = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            match entry.file_type().await {
                Ok(ft) if ft.is_dir() => stack.push(entry.path()),
                Ok(ft) if ft.is_file() => {
                    count += 1;
                    if let Ok(m) = entry.metadata().await {
                        bytes += m.len() as i64;
                    }
                }
                _ => {}
            }
        }
    }
    (count, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let o = DrillOutcome::failed(None, "pg-x.tar.gz.age", "decrypt", "boom".into());
        assert!(!o.success);
        assert_eq!(o.stage, "decrypt");
        assert!(o.extracted_bytes.is_none());
        assert!(o.verifybackup.is_none());
    }
}
