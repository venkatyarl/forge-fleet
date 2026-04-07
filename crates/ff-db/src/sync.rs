//! Replication coordinator for leader→follower database synchronization.
//!
//! Provides high-level coordination on top of the low-level primitives in
//! `replication.rs` and `backup.rs`:
//!
//! - **[`LeaderSync`]**: Periodically creates snapshots, serves them to followers.
//! - **[`FollowerSync`]**: Periodically checks the leader's sequence, pulls if behind.
//! - **[`BackupScheduler`]**: Daily automatic backups with retention policy.
//!
//! # Replication model
//!
//! Single-writer: the leader is the only node that writes to its database.
//! Followers receive full snapshots and apply them locally.  Conflict resolution
//! is trivial — the leader always wins.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ff_core::config::DatabaseMode;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::backup::{BackupConfig, create_backup};
use crate::connection::DbPool;
use crate::error::{DbError, Result};
use crate::replication::{
    SnapshotMeta, apply_snapshot, create_snapshot, get_local_sequence, set_local_sequence,
};

// ─── Configuration ───────────────────────────────────────────────────────────

/// Availability of ff-db's built-in snapshot replication + backup helpers.
///
/// These helpers rely on SQLite's online backup API and are intentionally scoped
/// to `embedded_sqlite` mode. Postgres modes should use Postgres-native backup
/// and replication tooling instead of sqlite snapshot files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicationBackupHelperAvailability {
    EnabledEmbeddedSqlite,
    DisabledPostgresBacked,
}

impl ReplicationBackupHelperAvailability {
    pub fn for_database_mode(mode: &DatabaseMode) -> Self {
        match mode {
            DatabaseMode::EmbeddedSqlite => Self::EnabledEmbeddedSqlite,
            DatabaseMode::PostgresRuntime | DatabaseMode::PostgresFull => {
                Self::DisabledPostgresBacked
            }
        }
    }

    pub fn is_enabled(self) -> bool {
        matches!(self, Self::EnabledEmbeddedSqlite)
    }

    pub fn summary(self) -> &'static str {
        match self {
            Self::EnabledEmbeddedSqlite => {
                "ff-db snapshot replication/backup helpers enabled (embedded_sqlite mode)"
            }
            Self::DisabledPostgresBacked => {
                "ff-db snapshot replication/backup helpers disabled for Postgres-backed modes; use Postgres-native backup/replication controls"
            }
        }
    }
}

/// Role of this node in the replication topology.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SyncRole {
    Leader,
    Follower,
}

/// Configuration for the replication coordinator.
#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// This node's role.
    pub role: SyncRole,
    /// Human-readable node name (used in snapshot metadata).
    pub node_name: String,
    /// Base URL of the leader node (required for followers).
    /// Example: `http://192.168.5.100:8787`
    pub leader_url: Option<String>,
    /// How often to sync (leader: snapshot creation, follower: poll interval).
    pub sync_interval: Duration,
    /// Directory for snapshot files.
    pub snapshot_dir: PathBuf,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            role: SyncRole::Follower,
            node_name: "unknown".into(),
            leader_url: None,
            sync_interval: Duration::from_secs(30),
            snapshot_dir: PathBuf::from("snapshots"),
        }
    }
}

// ─── Leader Sync ─────────────────────────────────────────────────────────────

/// Cached snapshot data for serving to followers.
#[derive(Debug, Clone)]
struct CachedSnapshot {
    meta: SnapshotMeta,
    path: PathBuf,
}

/// Leader-side replication coordinator.
///
/// Maintains the authoritative sequence number and periodically creates
/// snapshots that followers can pull.  The latest snapshot is cached in
/// memory so follower requests are served from disk without re-creating.
pub struct LeaderSync {
    pool: DbPool,
    config: SyncConfig,
    /// Monotonically increasing sequence counter.
    sequence: Arc<AtomicU64>,
    /// Most recent snapshot ready for serving.
    latest_snapshot: Arc<RwLock<Option<CachedSnapshot>>>,
}

impl LeaderSync {
    /// Create a new leader sync coordinator.
    ///
    /// Reads the current sequence from the database so we resume correctly
    /// after a restart.
    pub fn new(pool: DbPool, config: SyncConfig) -> Result<Self> {
        std::fs::create_dir_all(&config.snapshot_dir)?;

        let current_seq = pool
            .open_raw_connection()
            .map(|conn| get_local_sequence(&conn))
            .unwrap_or(0);

        info!(
            node = %config.node_name,
            sequence = current_seq,
            "leader sync initialized"
        );

        Ok(Self {
            pool,
            config,
            sequence: Arc::new(AtomicU64::new(current_seq)),
            latest_snapshot: Arc::new(RwLock::new(None)),
        })
    }

    /// Current authoritative sequence number.
    pub fn current_sequence(&self) -> u64 {
        self.sequence.load(Ordering::SeqCst)
    }

    /// Create a fresh snapshot and cache it for serving.
    ///
    /// Bumps the sequence number, creates a snapshot via the SQLite backup
    /// API, persists the new sequence, and removes the previous snapshot file.
    pub async fn create_fresh_snapshot(&self) -> Result<SnapshotMeta> {
        let new_seq = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let snapshot_path = self
            .config
            .snapshot_dir
            .join(format!("snapshot_{new_seq}.db"));
        let node_name = self.config.node_name.clone();
        let sp = snapshot_path.clone();

        // Create the snapshot inside a pool connection.
        let meta = self
            .pool
            .with_conn(move |conn| create_snapshot(conn, &sp, &node_name, new_seq))
            .await?;

        // Persist the new sequence number.
        let seq = new_seq;
        self.pool
            .with_conn(move |conn| set_local_sequence(conn, seq))
            .await?;

        // Swap the cached snapshot (remove old file).
        {
            let mut cached = self.latest_snapshot.write().await;
            if let Some(old) = cached.as_ref() {
                if old.path != snapshot_path {
                    let _ = std::fs::remove_file(&old.path);
                }
            }
            *cached = Some(CachedSnapshot {
                meta: meta.clone(),
                path: snapshot_path,
            });
        }

        info!(sequence = new_seq, "leader created fresh snapshot");
        Ok(meta)
    }

    /// Return the latest cached snapshot metadata and file path.
    ///
    /// Returns `None` if no snapshot has been created yet.
    pub async fn get_latest_snapshot(&self) -> Option<(SnapshotMeta, PathBuf)> {
        let cached = self.latest_snapshot.read().await;
        cached.as_ref().map(|c| (c.meta.clone(), c.path.clone()))
    }

    /// Handle a pull request from a follower.
    ///
    /// If the follower's sequence is behind the leader's, returns the latest
    /// snapshot.  Otherwise returns `None` (follower is up to date).
    pub async fn handle_pull(
        &self,
        follower_sequence: u64,
    ) -> Result<Option<(SnapshotMeta, PathBuf)>> {
        let current = self.current_sequence();
        if follower_sequence >= current {
            return Ok(None);
        }

        // Ensure we have a cached snapshot.
        let snapshot = self.get_latest_snapshot().await;
        if snapshot.is_none() {
            // No cached snapshot — create one on the fly.
            let _meta = self.create_fresh_snapshot().await?;
            let cached = self.get_latest_snapshot().await;
            return Ok(cached);
        }

        Ok(snapshot)
    }

    /// Start the periodic snapshot-creation loop.
    ///
    /// Returns a `JoinHandle` that runs until dropped or the task is aborted.
    /// Creates an initial snapshot immediately, then repeats on the configured
    /// interval.
    pub fn start_periodic(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::clone(self);
        let interval = this.config.sync_interval;

        tokio::spawn(async move {
            // Create an initial snapshot right away.
            match this.create_fresh_snapshot().await {
                Ok(meta) => info!(sequence = meta.sequence, "initial leader snapshot created"),
                Err(e) => error!(error = %e, "failed to create initial leader snapshot"),
            }

            let mut ticker = tokio::time::interval(interval);
            // The first tick fires immediately — skip it since we just created one.
            ticker.tick().await;

            loop {
                ticker.tick().await;
                match this.create_fresh_snapshot().await {
                    Ok(meta) => {
                        debug!(sequence = meta.sequence, "periodic snapshot created");
                    }
                    Err(e) => warn!(error = %e, "periodic snapshot creation failed"),
                }
            }
        })
    }
}

// ─── Follower Sync ───────────────────────────────────────────────────────────

/// Follower-side replication coordinator.
///
/// Periodically polls the leader's sequence number.  When the follower is
/// behind, it pulls a full snapshot and applies it locally.
pub struct FollowerSync {
    pool: DbPool,
    config: SyncConfig,
    http: reqwest::Client,
}

/// Response from the leader's sequence endpoint.
#[derive(Debug, serde::Deserialize)]
struct SequenceResponse {
    sequence: u64,
}

impl FollowerSync {
    /// Create a new follower sync coordinator.
    pub fn new(pool: DbPool, config: SyncConfig) -> Result<Self> {
        if config.leader_url.is_none() {
            return Err(DbError::Replication(
                "follower requires leader_url in SyncConfig".into(),
            ));
        }

        std::fs::create_dir_all(&config.snapshot_dir)?;

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| DbError::Replication(format!("failed to build HTTP client: {e}")))?;

        info!(
            node = %config.node_name,
            leader = config.leader_url.as_deref().unwrap_or("?"),
            "follower sync initialized"
        );

        Ok(Self { pool, config, http })
    }

    /// URL helper.
    fn leader_url(&self, path: &str) -> String {
        format!(
            "{}{}",
            self.config.leader_url.as_deref().unwrap_or(""),
            path
        )
    }

    /// Fetch the leader's current sequence number.
    pub async fn fetch_leader_sequence(&self) -> Result<u64> {
        let url = self.leader_url("/api/fleet/replicate/sequence");
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| DbError::Replication(format!("fetch sequence failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(DbError::Replication(format!(
                "leader returned {} for sequence request",
                resp.status()
            )));
        }

        let body: SequenceResponse = resp
            .json()
            .await
            .map_err(|e| DbError::Replication(format!("parse sequence response: {e}")))?;

        Ok(body.sequence)
    }

    /// Get the local sequence number.
    pub async fn local_sequence(&self) -> u64 {
        self.pool
            .with_conn(|conn| Ok(get_local_sequence(conn)))
            .await
            .unwrap_or(0)
    }

    /// Pull a full snapshot from the leader.
    ///
    /// Sends the follower's current sequence; the leader returns the snapshot
    /// bytes if the follower is behind.
    pub async fn pull_snapshot(&self, local_seq: u64) -> Result<Option<(SnapshotMeta, Vec<u8>)>> {
        let url = self.leader_url("/api/fleet/replicate/pull");
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({"since_sequence": local_seq}))
            .send()
            .await
            .map_err(|e| DbError::Replication(format!("pull snapshot request failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(DbError::Replication(format!(
                "leader returned {} for pull request",
                resp.status()
            )));
        }

        // Check if the response is a status JSON (up to date) or binary snapshot.
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if content_type.contains("application/json") {
            // Follower is up to date.
            debug!("follower is up to date with leader");
            return Ok(None);
        }

        // Binary snapshot — extract metadata from header.
        let meta_header = resp
            .headers()
            .get("x-snapshot-meta")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("{}");

        let meta: SnapshotMeta = serde_json::from_str(meta_header)
            .map_err(|e| DbError::Replication(format!("parse snapshot meta header: {e}")))?;

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| DbError::Replication(format!("read snapshot bytes: {e}")))?;

        Ok(Some((meta, bytes.to_vec())))
    }

    /// Check the leader and sync if we are behind.
    ///
    /// Returns `true` if a sync was performed, `false` if already up to date.
    pub async fn check_and_sync(&self) -> Result<bool> {
        let local_seq = self.local_sequence().await;
        let leader_seq = self.fetch_leader_sequence().await?;

        if local_seq >= leader_seq {
            debug!(
                local = local_seq,
                leader = leader_seq,
                "follower is up to date"
            );
            return Ok(false);
        }

        info!(
            local = local_seq,
            leader = leader_seq,
            "follower is behind — pulling snapshot"
        );

        match self.pull_snapshot(local_seq).await? {
            Some((meta, bytes)) => {
                self.apply_pulled_snapshot(&meta, &bytes).await?;
                Ok(true)
            }
            None => {
                debug!("pull returned up-to-date (race condition, harmless)");
                Ok(false)
            }
        }
    }

    /// Write snapshot bytes to disk and apply them.
    async fn apply_pulled_snapshot(&self, meta: &SnapshotMeta, bytes: &[u8]) -> Result<()> {
        let snapshot_path = self
            .config
            .snapshot_dir
            .join(format!("received_snapshot_{}.db", meta.sequence));

        // Write snapshot to disk.
        std::fs::write(&snapshot_path, bytes)?;

        let db_path = self.pool.path().to_path_buf();

        // Apply the snapshot.
        // NOTE: This uses the SQLite backup API which handles concurrent readers,
        // but for maximum safety the pool should be quiesced before applying.
        apply_snapshot(&db_path, &snapshot_path)?;

        // Update local sequence.
        let seq = meta.sequence;
        self.pool
            .with_conn(move |conn| set_local_sequence(conn, seq))
            .await?;

        // Clean up downloaded snapshot.
        let _ = std::fs::remove_file(&snapshot_path);

        info!(
            sequence = meta.sequence,
            leader = %meta.leader,
            size_kb = meta.size_bytes / 1024,
            "follower applied snapshot from leader"
        );

        Ok(())
    }

    /// Perform a full initial sync (download complete snapshot regardless of sequence).
    pub async fn full_sync(&self) -> Result<()> {
        info!("performing full initial sync from leader");
        // Pull with sequence 0 to force a full download.
        match self.pull_snapshot(0).await? {
            Some((meta, bytes)) => {
                self.apply_pulled_snapshot(&meta, &bytes).await?;
                info!(sequence = meta.sequence, "full initial sync complete");
                Ok(())
            }
            None => {
                warn!("leader returned no snapshot for full sync — leader may be empty");
                Ok(())
            }
        }
    }

    /// Start the periodic sync loop.
    ///
    /// Performs a full sync on first run (if local sequence is 0), then
    /// polls incrementally on the configured interval.
    pub fn start_periodic(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        let interval = self.config.sync_interval;

        tokio::spawn(async move {
            // Initial sync if this is a fresh follower.
            let local_seq = self.local_sequence().await;
            if local_seq == 0 {
                match self.full_sync().await {
                    Ok(()) => info!("initial full sync completed"),
                    Err(e) => error!(error = %e, "initial full sync failed — will retry"),
                }
            }

            let mut ticker = tokio::time::interval(interval);
            // Skip the first immediate tick.
            ticker.tick().await;

            loop {
                ticker.tick().await;
                match self.check_and_sync().await {
                    Ok(true) => debug!("follower sync: applied new snapshot"),
                    Ok(false) => {}
                    Err(e) => warn!(error = %e, "follower sync check failed"),
                }
            }
        })
    }
}

// ─── Backup Scheduler ────────────────────────────────────────────────────────

/// Backup trigger reasons (for logging/auditing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupReason {
    /// Scheduled daily backup.
    Daily,
    /// Pre-update safety backup.
    BeforeUpdate,
    /// Pre-failover safety backup.
    BeforeFailover,
    /// Manual trigger.
    Manual,
}

impl std::fmt::Display for BackupReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Daily => write!(f, "daily"),
            Self::BeforeUpdate => write!(f, "before_update"),
            Self::BeforeFailover => write!(f, "before_failover"),
            Self::Manual => write!(f, "manual"),
        }
    }
}

/// Scheduled backup coordinator.
///
/// Manages automatic daily backups and on-demand safety backups before
/// potentially destructive operations (updates, failovers).
pub struct BackupScheduler {
    pool: DbPool,
    config: BackupConfig,
    /// Timestamp of last backup (for daily scheduling).
    last_backup: Arc<RwLock<Option<chrono::DateTime<chrono::Utc>>>>,
}

impl BackupScheduler {
    /// Create a new backup scheduler.
    ///
    /// Uses `max_backups = 7` by default (keep last 7 days of backups).
    pub fn new(pool: DbPool, backup_dir: PathBuf) -> Self {
        Self {
            pool,
            config: BackupConfig {
                backup_dir,
                max_backups: 7,
            },
            last_backup: Arc::new(RwLock::new(None)),
        }
    }

    /// Create a new backup scheduler with custom config.
    pub fn with_config(pool: DbPool, config: BackupConfig) -> Self {
        Self {
            pool,
            config,
            last_backup: Arc::new(RwLock::new(None)),
        }
    }

    /// Trigger a backup for the given reason.
    pub async fn trigger_backup(&self, reason: BackupReason) -> Result<PathBuf> {
        info!(reason = %reason, "triggering backup");

        let config = self.config.clone();
        let path = self
            .pool
            .with_conn(move |conn| create_backup(conn, &config))
            .await?;

        // Update last-backup timestamp.
        {
            let mut last = self.last_backup.write().await;
            *last = Some(chrono::Utc::now());
        }

        info!(
            reason = %reason,
            path = %path.display(),
            "backup completed"
        );

        Ok(path)
    }

    /// Backup before a self-update operation.
    pub async fn backup_before_update(&self) -> Result<PathBuf> {
        self.trigger_backup(BackupReason::BeforeUpdate).await
    }

    /// Backup before a leader failover.
    pub async fn backup_before_failover(&self) -> Result<PathBuf> {
        self.trigger_backup(BackupReason::BeforeFailover).await
    }

    /// Check if a daily backup is needed and create one if so.
    ///
    /// A daily backup is needed if more than 24 hours have passed since the
    /// last backup (or no backup has ever been created).
    pub async fn daily_check(&self) -> Result<bool> {
        let needs_backup = {
            let last = self.last_backup.read().await;
            match *last {
                Some(ts) => {
                    let elapsed = chrono::Utc::now() - ts;
                    elapsed.num_hours() >= 24
                }
                None => true,
            }
        };

        if needs_backup {
            self.trigger_backup(BackupReason::Daily).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Start the daily backup check loop.
    ///
    /// Checks every hour whether a daily backup is due.
    pub fn start_daily(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::clone(self);

        tokio::spawn(async move {
            // Initial check on startup.
            match this.daily_check().await {
                Ok(true) => info!("startup daily backup created"),
                Ok(false) => debug!("no daily backup needed at startup"),
                Err(e) => warn!(error = %e, "startup daily backup check failed"),
            }

            // Check every hour.
            let mut ticker = tokio::time::interval(Duration::from_secs(3600));
            ticker.tick().await; // skip first immediate tick

            loop {
                ticker.tick().await;
                match this.daily_check().await {
                    Ok(true) => info!("daily backup created"),
                    Ok(false) => {}
                    Err(e) => warn!(error = %e, "daily backup check failed"),
                }
            }
        })
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::DbPoolConfig;
    use crate::migrations::run_migrations;
    use ff_core::config::DatabaseMode;
    use std::path::Path;
    use tempfile::TempDir;

    /// Helper: create a DbPool with migrations applied.
    fn setup_pool(dir: &Path) -> DbPool {
        let db_path = dir.join("test.db");
        let pool = DbPool::open(DbPoolConfig::with_path(&db_path)).unwrap();

        // Run migrations synchronously via a raw connection.
        let conn = pool.open_raw_connection().unwrap();
        run_migrations(&conn).unwrap();

        // Insert some test data.
        conn.execute(
            "INSERT INTO config_kv (key, value) VALUES ('sync.test', 'leader_data')",
            [],
        )
        .unwrap();

        pool
    }

    fn leader_config(dir: &Path) -> SyncConfig {
        SyncConfig {
            role: SyncRole::Leader,
            node_name: "test-leader".into(),
            leader_url: None,
            sync_interval: Duration::from_secs(5),
            snapshot_dir: dir.join("snapshots"),
        }
    }

    // ── Leader tests ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_leader_sync_init() {
        let dir = TempDir::new().unwrap();
        let pool = setup_pool(dir.path());
        let config = leader_config(dir.path());

        let leader = LeaderSync::new(pool, config).unwrap();
        assert_eq!(leader.current_sequence(), 0);
    }

    #[tokio::test]
    async fn test_leader_create_snapshot() {
        let dir = TempDir::new().unwrap();
        let pool = setup_pool(dir.path());
        let config = leader_config(dir.path());

        let leader = LeaderSync::new(pool, config).unwrap();
        let meta = leader.create_fresh_snapshot().await.unwrap();

        assert_eq!(meta.sequence, 1);
        assert_eq!(meta.leader, "test-leader");
        assert!(meta.size_bytes > 0);
        assert_eq!(leader.current_sequence(), 1);

        // Should have a cached snapshot.
        let (cached_meta, cached_path) = leader.get_latest_snapshot().await.unwrap();
        assert_eq!(cached_meta.sequence, 1);
        assert!(cached_path.exists());
    }

    #[tokio::test]
    async fn test_leader_sequence_increments() {
        let dir = TempDir::new().unwrap();
        let pool = setup_pool(dir.path());
        let config = leader_config(dir.path());

        let leader = LeaderSync::new(pool, config).unwrap();

        let meta1 = leader.create_fresh_snapshot().await.unwrap();
        let meta2 = leader.create_fresh_snapshot().await.unwrap();
        let meta3 = leader.create_fresh_snapshot().await.unwrap();

        assert_eq!(meta1.sequence, 1);
        assert_eq!(meta2.sequence, 2);
        assert_eq!(meta3.sequence, 3);
        assert_eq!(leader.current_sequence(), 3);
    }

    #[tokio::test]
    async fn test_leader_old_snapshot_cleaned() {
        let dir = TempDir::new().unwrap();
        let pool = setup_pool(dir.path());
        let config = leader_config(dir.path());

        let leader = LeaderSync::new(pool, config).unwrap();

        leader.create_fresh_snapshot().await.unwrap();
        let (_, path1) = leader.get_latest_snapshot().await.unwrap();
        assert!(path1.exists());

        leader.create_fresh_snapshot().await.unwrap();
        let (_, path2) = leader.get_latest_snapshot().await.unwrap();
        assert!(path2.exists());

        // Old snapshot file should be removed.
        assert!(!path1.exists(), "old snapshot should be cleaned up");
    }

    #[tokio::test]
    async fn test_leader_handle_pull_up_to_date() {
        let dir = TempDir::new().unwrap();
        let pool = setup_pool(dir.path());
        let config = leader_config(dir.path());

        let leader = LeaderSync::new(pool, config).unwrap();
        leader.create_fresh_snapshot().await.unwrap();

        // Follower at same sequence — should get None.
        let result = leader.handle_pull(1).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_leader_handle_pull_behind() {
        let dir = TempDir::new().unwrap();
        let pool = setup_pool(dir.path());
        let config = leader_config(dir.path());

        let leader = LeaderSync::new(pool, config).unwrap();
        leader.create_fresh_snapshot().await.unwrap();

        // Follower at sequence 0 — should get snapshot.
        let result = leader.handle_pull(0).await.unwrap();
        assert!(result.is_some());
        let (meta, path) = result.unwrap();
        assert_eq!(meta.sequence, 1);
        assert!(path.exists());
    }

    // ── Backup scheduler tests ────────────────────────────────────────────

    #[tokio::test]
    async fn test_backup_scheduler_trigger() {
        let dir = TempDir::new().unwrap();
        let pool = setup_pool(dir.path());
        let backup_dir = dir.path().join("backups");

        let scheduler = BackupScheduler::new(pool, backup_dir.clone());
        let path = scheduler
            .trigger_backup(BackupReason::Manual)
            .await
            .unwrap();

        assert!(path.exists());
        assert!(path.to_str().unwrap().contains("forgefleet_"));
    }

    #[tokio::test]
    async fn test_backup_scheduler_daily_check() {
        let dir = TempDir::new().unwrap();
        let pool = setup_pool(dir.path());
        let backup_dir = dir.path().join("backups");

        let scheduler = BackupScheduler::new(pool, backup_dir);

        // First check should create a backup (no previous backup).
        assert!(scheduler.daily_check().await.unwrap());

        // Immediate second check should skip (< 24h).
        assert!(!scheduler.daily_check().await.unwrap());
    }

    #[tokio::test]
    async fn test_backup_scheduler_before_update() {
        let dir = TempDir::new().unwrap();
        let pool = setup_pool(dir.path());
        let backup_dir = dir.path().join("backups");

        let scheduler = BackupScheduler::new(pool, backup_dir);
        let path = scheduler.backup_before_update().await.unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn test_backup_scheduler_before_failover() {
        let dir = TempDir::new().unwrap();
        let pool = setup_pool(dir.path());
        let backup_dir = dir.path().join("backups");

        let scheduler = BackupScheduler::new(pool, backup_dir);
        let path = scheduler.backup_before_failover().await.unwrap();
        assert!(path.exists());
    }

    // ── Follower tests (unit-level, no HTTP) ──────────────────────────────

    #[test]
    fn test_follower_requires_leader_url() {
        let dir = TempDir::new().unwrap();
        let pool = setup_pool(dir.path());
        let config = SyncConfig {
            role: SyncRole::Follower,
            node_name: "test-follower".into(),
            leader_url: None,
            sync_interval: Duration::from_secs(5),
            snapshot_dir: dir.path().join("snapshots"),
        };

        let result = FollowerSync::new(pool, config);
        assert!(result.is_err());
    }

    #[test]
    fn test_follower_init_with_leader_url() {
        let dir = TempDir::new().unwrap();
        let pool = setup_pool(dir.path());
        let config = SyncConfig {
            role: SyncRole::Follower,
            node_name: "test-follower".into(),
            leader_url: Some("http://localhost:8787".into()),
            sync_interval: Duration::from_secs(5),
            snapshot_dir: dir.path().join("snapshots"),
        };

        let result = FollowerSync::new(pool, config);
        assert!(result.is_ok());
    }

    // ── SyncConfig tests ──────────────────────────────────────────────────

    #[test]
    fn test_sync_role_serde() {
        let leader = SyncRole::Leader;
        let json = serde_json::to_string(&leader).unwrap();
        assert_eq!(json, "\"Leader\"");

        let parsed: SyncRole = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, SyncRole::Leader);
    }

    #[test]
    fn test_sync_config_default() {
        let config = SyncConfig::default();
        assert_eq!(config.role, SyncRole::Follower);
        assert_eq!(config.sync_interval, Duration::from_secs(30));
        assert!(config.leader_url.is_none());
    }

    #[test]
    fn test_backup_reason_display() {
        assert_eq!(BackupReason::Daily.to_string(), "daily");
        assert_eq!(BackupReason::BeforeUpdate.to_string(), "before_update");
        assert_eq!(BackupReason::BeforeFailover.to_string(), "before_failover");
        assert_eq!(BackupReason::Manual.to_string(), "manual");
    }

    #[test]
    fn test_replication_backup_helper_availability_by_mode() {
        assert_eq!(
            ReplicationBackupHelperAvailability::for_database_mode(&DatabaseMode::EmbeddedSqlite),
            ReplicationBackupHelperAvailability::EnabledEmbeddedSqlite
        );
        assert_eq!(
            ReplicationBackupHelperAvailability::for_database_mode(&DatabaseMode::PostgresRuntime),
            ReplicationBackupHelperAvailability::DisabledPostgresBacked
        );
        assert_eq!(
            ReplicationBackupHelperAvailability::for_database_mode(&DatabaseMode::PostgresFull),
            ReplicationBackupHelperAvailability::DisabledPostgresBacked
        );
    }

    #[test]
    fn test_replication_backup_helper_summary_is_explicit() {
        let sqlite = ReplicationBackupHelperAvailability::EnabledEmbeddedSqlite;
        assert!(sqlite.is_enabled());
        assert!(sqlite.summary().contains("embedded_sqlite"));

        let postgres = ReplicationBackupHelperAvailability::DisabledPostgresBacked;
        assert!(!postgres.is_enabled());
        assert!(postgres.summary().contains("disabled"));
        assert!(postgres.summary().contains("Postgres-native"));
    }
}
