//! Leader→follower replication via SQLite backup API.
//!
//! Strategy:
//! - **Initial sync**: Full database snapshot via `sqlite3_backup_init` / `backup_step` / `backup_finish`.
//! - **Incremental**: Leader periodically serializes its database to a snapshot file,
//!   followers fetch and apply it. This is simpler than WAL shipping for SQLite
//!   (which doesn't expose WAL frames via public API) and is transactionally consistent.
//!
//! In production, the leader exposes an HTTP endpoint that serves the snapshot;
//! followers poll it on a schedule. This module provides the serialization/deserialization
//! primitives — the HTTP transport lives in ff-mesh.

use std::path::{Path, PathBuf};
use std::time::Instant;

use rusqlite::Connection;
use tracing::{debug, info};

use crate::error::{DbError, Result};

/// Metadata about a replication snapshot.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SnapshotMeta {
    /// When the snapshot was created (ISO 8601).
    pub created_at: String,
    /// Leader node name.
    pub leader: String,
    /// Schema version at snapshot time.
    pub schema_version: u32,
    /// Snapshot file size in bytes.
    pub size_bytes: u64,
    /// Monotonically increasing sequence number for ordering.
    pub sequence: u64,
}

// ─── Leader Side ───────────────────────────────────────────────────────────

/// Create a full snapshot of the source database to a destination file.
///
/// Uses SQLite's online backup API, which is safe to call while the source
/// database is actively being written to (it handles WAL correctly).
pub fn create_snapshot(
    source: &Connection,
    dest_path: &Path,
    leader_name: &str,
    sequence: u64,
) -> Result<SnapshotMeta> {
    let start = Instant::now();

    // Remove existing snapshot file if present.
    if dest_path.exists() {
        std::fs::remove_file(dest_path)?;
    }

    // Ensure parent directory exists.
    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Open destination database.
    let mut dest_conn = Connection::open(dest_path)
        .map_err(|e| DbError::Replication(format!("failed to open snapshot destination: {e}")))?;

    // Use the backup API to copy source → dest.
    let backup = rusqlite::backup::Backup::new(source, &mut dest_conn)
        .map_err(|e| DbError::Replication(format!("backup init failed: {e}")))?;

    // Step through the entire database. -1 means "copy all remaining pages".
    backup
        .step(-1)
        .map_err(|e| DbError::Replication(format!("backup step failed: {e}")))?;

    // Get file size.
    let size_bytes = std::fs::metadata(dest_path).map(|m| m.len()).unwrap_or(0);

    // Get schema version.
    let schema_version: u32 = source
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM _migrations",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let elapsed = start.elapsed();
    let meta = SnapshotMeta {
        created_at: chrono::Utc::now().to_rfc3339(),
        leader: leader_name.to_string(),
        schema_version,
        size_bytes,
        sequence,
    };

    info!(
        leader = leader_name,
        sequence,
        size_kb = size_bytes / 1024,
        elapsed_ms = elapsed.as_millis(),
        "snapshot created"
    );

    Ok(meta)
}

/// Serialize the snapshot metadata to JSON (for HTTP response headers or sidecar file).
pub fn serialize_snapshot_meta(meta: &SnapshotMeta) -> Result<String> {
    serde_json::to_string(meta).map_err(Into::into)
}

// ─── Follower Side ─────────────────────────────────────────────────────────

/// Apply a snapshot file to the local database.
///
/// This performs a full replacement: the snapshot becomes the new database.
/// The existing database is backed up first (moved to `.pre-sync`).
///
/// **Must be called when no other connections are active on the target database.**
pub fn apply_snapshot(target_db_path: &Path, snapshot_path: &Path) -> Result<()> {
    let start = Instant::now();

    if !snapshot_path.exists() {
        return Err(DbError::Replication(format!(
            "snapshot file not found: {}",
            snapshot_path.display()
        )));
    }

    // Back up existing database.
    let pre_sync_path = target_db_path.with_extension("pre-sync");
    if target_db_path.exists() {
        debug!(
            from = %target_db_path.display(),
            to = %pre_sync_path.display(),
            "backing up existing database before sync"
        );
        std::fs::copy(target_db_path, &pre_sync_path)?;
    }

    // Open the snapshot as source and target database as destination.
    let source_conn = Connection::open(snapshot_path)
        .map_err(|e| DbError::Replication(format!("failed to open snapshot: {e}")))?;

    // Remove WAL/SHM files from target so we start clean.
    let wal_path = PathBuf::from(format!("{}-wal", target_db_path.display()));
    let shm_path = PathBuf::from(format!("{}-shm", target_db_path.display()));
    let _ = std::fs::remove_file(&wal_path);
    let _ = std::fs::remove_file(&shm_path);

    let mut dest_conn = Connection::open(target_db_path)
        .map_err(|e| DbError::Replication(format!("failed to open target database: {e}")))?;

    // Backup from snapshot → target.
    let backup = rusqlite::backup::Backup::new(&source_conn, &mut dest_conn)
        .map_err(|e| DbError::Replication(format!("apply backup init failed: {e}")))?;

    backup
        .step(-1)
        .map_err(|e| DbError::Replication(format!("apply backup step failed: {e}")))?;

    let elapsed = start.elapsed();
    info!(
        target = %target_db_path.display(),
        elapsed_ms = elapsed.as_millis(),
        "snapshot applied to local database"
    );

    Ok(())
}

/// Check if a snapshot is newer than our local database.
pub fn should_apply_snapshot(local_sequence: u64, remote_meta: &SnapshotMeta) -> bool {
    remote_meta.sequence > local_sequence
}

/// Read the last applied snapshot sequence from config_kv.
pub fn get_local_sequence(conn: &Connection) -> u64 {
    conn.query_row(
        "SELECT value FROM config_kv WHERE key = 'replication.sequence'",
        [],
        |row| {
            let val: String = row.get(0)?;
            Ok(val.parse::<u64>().unwrap_or(0))
        },
    )
    .unwrap_or(0)
}

/// Store the applied snapshot sequence in config_kv.
pub fn set_local_sequence(conn: &Connection, sequence: u64) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO config_kv (key, value, updated_at) VALUES ('replication.sequence', ?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        rusqlite::params![sequence.to_string(), now],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::run_migrations;
    use tempfile::TempDir;

    fn setup_db(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        run_migrations(&conn).unwrap();
        conn
    }

    #[test]
    fn test_snapshot_create_and_apply() {
        let dir = TempDir::new().unwrap();
        let source_path = dir.path().join("source.db");
        let snapshot_path = dir.path().join("snapshot.db");
        let target_path = dir.path().join("target.db");

        // Create source with some data.
        let source = setup_db(&source_path);
        source
            .execute(
                "INSERT INTO config_kv (key, value) VALUES ('test.key', 'hello_from_leader')",
                [],
            )
            .unwrap();

        // Create snapshot.
        let meta = create_snapshot(&source, &snapshot_path, "taylor", 1).unwrap();
        assert_eq!(meta.leader, "taylor");
        assert_eq!(meta.sequence, 1);
        assert!(meta.size_bytes > 0);

        // Apply to target.
        apply_snapshot(&target_path, &snapshot_path).unwrap();

        // Verify data arrived.
        let target = Connection::open(&target_path).unwrap();
        let val: String = target
            .query_row(
                "SELECT value FROM config_kv WHERE key = 'test.key'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(val, "hello_from_leader");
    }

    #[test]
    fn test_should_apply_snapshot() {
        let meta = SnapshotMeta {
            created_at: "2026-04-04T00:00:00Z".into(),
            leader: "taylor".into(),
            schema_version: 1,
            size_bytes: 4096,
            sequence: 5,
        };

        assert!(should_apply_snapshot(4, &meta));
        assert!(!should_apply_snapshot(5, &meta));
        assert!(!should_apply_snapshot(10, &meta));
    }

    #[test]
    fn test_local_sequence_tracking() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let conn = setup_db(&db_path);

        assert_eq!(get_local_sequence(&conn), 0);
        set_local_sequence(&conn, 42).unwrap();
        assert_eq!(get_local_sequence(&conn), 42);
    }

    #[test]
    fn test_serialize_snapshot_meta() {
        let meta = SnapshotMeta {
            created_at: "2026-04-04T00:00:00Z".into(),
            leader: "taylor".into(),
            schema_version: 1,
            size_bytes: 8192,
            sequence: 3,
        };
        let json = serialize_snapshot_meta(&meta).unwrap();
        let parsed: SnapshotMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.leader, "taylor");
        assert_eq!(parsed.sequence, 3);
    }
}
