//! Periodic backup and restore for the local SQLite database.
//!
//! Separate from replication — this is for disaster recovery.
//! Creates timestamped backup files and supports restore from any backup.

use std::path::{Path, PathBuf};
use std::time::Instant;

use rusqlite::Connection;
use tracing::{debug, info};

use crate::error::{DbError, Result};

/// Configuration for backup operations.
#[derive(Debug, Clone)]
pub struct BackupConfig {
    /// Directory to store backup files.
    pub backup_dir: PathBuf,
    /// Maximum number of backup files to retain.
    pub max_backups: usize,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            backup_dir: PathBuf::from("backups"),
            max_backups: 10,
        }
    }
}

/// Create a backup of the database.
///
/// Uses the SQLite backup API for a consistent snapshot, even while
/// the database is being written to. The backup file is named with
/// a timestamp: `forgefleet_YYYYMMDD_HHMMSS.db`
pub fn create_backup(source: &Connection, config: &BackupConfig) -> Result<PathBuf> {
    let start = Instant::now();

    // Ensure backup directory exists.
    std::fs::create_dir_all(&config.backup_dir)?;

    // Generate timestamped filename.
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let filename = format!("forgefleet_{timestamp}.db");
    let backup_path = config.backup_dir.join(&filename);

    // Open destination and perform backup.
    let mut dest = Connection::open(&backup_path)
        .map_err(|e| DbError::Backup(format!("failed to open backup destination: {e}")))?;

    let backup = rusqlite::backup::Backup::new(source, &mut dest)
        .map_err(|e| DbError::Backup(format!("backup init failed: {e}")))?;

    backup
        .step(-1)
        .map_err(|e| DbError::Backup(format!("backup step failed: {e}")))?;

    // Explicitly drop to close the connection and flush.
    drop(backup);
    drop(dest);

    let size = std::fs::metadata(&backup_path)
        .map(|m| m.len())
        .unwrap_or(0);

    let elapsed = start.elapsed();
    info!(
        path = %backup_path.display(),
        size_kb = size / 1024,
        elapsed_ms = elapsed.as_millis(),
        "backup created"
    );

    // Prune old backups.
    prune_old_backups(config)?;

    Ok(backup_path)
}

/// Restore the database from a backup file.
///
/// **Warning:** This replaces the current database entirely.
/// The caller must ensure no other connections are active.
pub fn restore_from_backup(backup_path: &Path, target_db_path: &Path) -> Result<()> {
    let start = Instant::now();

    if !backup_path.exists() {
        return Err(DbError::Backup(format!(
            "backup file not found: {}",
            backup_path.display()
        )));
    }

    // Safety: move current database aside.
    let aside_path = target_db_path.with_extension("pre-restore");
    if target_db_path.exists() {
        debug!(
            from = %target_db_path.display(),
            to = %aside_path.display(),
            "moving current database aside"
        );
        std::fs::rename(target_db_path, &aside_path)?;
    }

    // Clean up WAL/SHM from old database.
    let wal_path = PathBuf::from(format!("{}-wal", target_db_path.display()));
    let shm_path = PathBuf::from(format!("{}-shm", target_db_path.display()));
    let _ = std::fs::remove_file(&wal_path);
    let _ = std::fs::remove_file(&shm_path);

    // Copy backup to target location using backup API for consistency.
    let source = Connection::open(backup_path)
        .map_err(|e| DbError::Backup(format!("failed to open backup file: {e}")))?;

    let mut dest = Connection::open(target_db_path)
        .map_err(|e| DbError::Backup(format!("failed to open target for restore: {e}")))?;

    let backup = rusqlite::backup::Backup::new(&source, &mut dest)
        .map_err(|e| DbError::Backup(format!("restore backup init failed: {e}")))?;

    backup
        .step(-1)
        .map_err(|e| DbError::Backup(format!("restore backup step failed: {e}")))?;

    let elapsed = start.elapsed();
    info!(
        from = %backup_path.display(),
        to = %target_db_path.display(),
        elapsed_ms = elapsed.as_millis(),
        "database restored from backup"
    );

    Ok(())
}

/// List available backup files, newest first.
pub fn list_backups(config: &BackupConfig) -> Result<Vec<PathBuf>> {
    if !config.backup_dir.exists() {
        return Ok(Vec::new());
    }

    let mut backups: Vec<PathBuf> = std::fs::read_dir(&config.backup_dir)?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension().map(|e| e == "db").unwrap_or(false)
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("forgefleet_"))
                    .unwrap_or(false)
            {
                Some(path)
            } else {
                None
            }
        })
        .collect();

    // Sort by name descending (timestamp in name = chronological order).
    backups.sort_by(|a, b| b.cmp(a));
    Ok(backups)
}

/// Remove old backup files, keeping only `max_backups` most recent.
fn prune_old_backups(config: &BackupConfig) -> Result<()> {
    let backups = list_backups(config)?;
    if backups.len() <= config.max_backups {
        return Ok(());
    }

    let to_remove = &backups[config.max_backups..];
    for path in to_remove {
        debug!(path = %path.display(), "removing old backup");
        std::fs::remove_file(path)?;
    }

    info!(
        removed = to_remove.len(),
        retained = config.max_backups,
        "pruned old backups"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::run_migrations;
    use tempfile::TempDir;

    fn setup_source(dir: &Path) -> Connection {
        let db_path = dir.join("source.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        run_migrations(&conn).unwrap();

        // Insert test data.
        conn.execute(
            "INSERT INTO config_kv (key, value) VALUES ('backup.test', 'it_works')",
            [],
        )
        .unwrap();

        conn
    }

    #[test]
    fn test_backup_create_and_restore() {
        let dir = TempDir::new().unwrap();
        let source = setup_source(dir.path());
        let config = BackupConfig {
            backup_dir: dir.path().join("backups"),
            max_backups: 5,
        };

        // Create backup.
        let backup_path = create_backup(&source, &config).unwrap();
        assert!(backup_path.exists());

        // Restore to a new location.
        let target_path = dir.path().join("restored.db");
        restore_from_backup(&backup_path, &target_path).unwrap();

        // Verify data.
        let restored = Connection::open(&target_path).unwrap();
        let val: String = restored
            .query_row(
                "SELECT value FROM config_kv WHERE key = 'backup.test'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(val, "it_works");
    }

    #[test]
    fn test_list_backups() {
        let dir = TempDir::new().unwrap();
        let source = setup_source(dir.path());
        let config = BackupConfig {
            backup_dir: dir.path().join("backups"),
            max_backups: 5,
        };

        create_backup(&source, &config).unwrap();
        let backups = list_backups(&config).unwrap();
        assert_eq!(backups.len(), 1);
    }

    #[test]
    fn test_prune_old_backups() {
        let dir = TempDir::new().unwrap();
        let backup_dir = dir.path().join("backups");
        std::fs::create_dir_all(&backup_dir).unwrap();

        // Create dummy backup files.
        for i in 1..=5 {
            let name = format!("forgefleet_20260404_00000{i}.db");
            std::fs::write(backup_dir.join(name), b"dummy").unwrap();
        }

        let config = BackupConfig {
            backup_dir: backup_dir.clone(),
            max_backups: 3,
        };

        prune_old_backups(&config).unwrap();

        let remaining = list_backups(&config).unwrap();
        assert_eq!(remaining.len(), 3);
    }
}
