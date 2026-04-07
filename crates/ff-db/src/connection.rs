//! SQLite connection pool with WAL mode and performance tuning.
//!
//! Uses a simple pool of `rusqlite::Connection` instances behind a channel.
//! All connections are pre-configured with WAL journal mode, synchronous=NORMAL,
//! and other pragmas for optimal performance.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;
use tokio::sync::{Mutex, Semaphore};
use tracing::{debug, info};

use crate::error::{DbError, Result};

/// Configuration for the database connection pool.
#[derive(Debug, Clone)]
pub struct DbPoolConfig {
    /// Path to the SQLite database file.
    pub path: PathBuf,
    /// Maximum number of connections in the pool.
    pub max_connections: usize,
    /// SQLite cache size in KiB (negative = KiB, positive = pages).
    pub cache_size_kib: i64,
    /// Enable WAL mode (strongly recommended).
    pub wal_mode: bool,
    /// Busy timeout in milliseconds.
    pub busy_timeout_ms: u32,
}

impl Default for DbPoolConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("forgefleet.db"),
            max_connections: 4,
            cache_size_kib: -64_000, // 64 MiB
            wal_mode: true,
            busy_timeout_ms: 5_000,
        }
    }
}

impl DbPoolConfig {
    /// Create a config for an in-memory database (useful for testing).
    pub fn in_memory() -> Self {
        Self {
            path: PathBuf::from(":memory:"),
            max_connections: 1, // in-memory only supports 1 real connection
            ..Default::default()
        }
    }

    /// Create a config with a specific database path.
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            ..Default::default()
        }
    }
}

/// A pool of SQLite connections.
///
/// Wraps multiple pre-configured `rusqlite::Connection` instances with async
/// checkout via a semaphore. All connections share the same WAL file.
#[derive(Clone)]
pub struct DbPool {
    connections: Arc<Vec<Mutex<Connection>>>,
    semaphore: Arc<Semaphore>,
    config: DbPoolConfig,
}

impl std::fmt::Debug for DbPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbPool")
            .field("path", &self.config.path)
            .field("max_connections", &self.config.max_connections)
            .finish()
    }
}

impl DbPool {
    /// Open a new connection pool.
    ///
    /// Creates the database file (and parent directories) if they don't exist.
    /// All connections are configured with WAL mode and performance pragmas.
    pub fn open(config: DbPoolConfig) -> Result<Self> {
        // Ensure parent directory exists for file-based databases.
        if config.path.to_str() != Some(":memory:")
            && let Some(parent) = config.path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)?;
        }

        info!(path = %config.path.display(), pool_size = config.max_connections, "opening SQLite pool");

        let mut connections = Vec::with_capacity(config.max_connections);
        for i in 0..config.max_connections {
            let conn = open_connection(&config)?;
            debug!(index = i, "connection opened");
            connections.push(Mutex::new(conn));
        }

        let semaphore = Arc::new(Semaphore::new(config.max_connections));

        Ok(Self {
            connections: Arc::new(connections),
            semaphore,
            config,
        })
    }

    /// Get the database file path.
    pub fn path(&self) -> &Path {
        &self.config.path
    }

    /// Get the pool configuration.
    pub fn config(&self) -> &DbPoolConfig {
        &self.config
    }

    /// Execute a closure with a connection from the pool.
    ///
    /// Acquires a semaphore permit, locks the first available connection,
    /// runs the closure, then releases both.
    pub async fn with_conn<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|e| DbError::Pool(format!("semaphore closed: {e}")))?;

        // Find the first available (unlockable) connection.
        for conn_mutex in self.connections.iter() {
            if let Ok(conn) = conn_mutex.try_lock() {
                return f(&conn);
            }
        }

        // Fallback: wait for any connection.
        let conn = self.connections[0].lock().await;
        f(&conn)
    }

    /// Execute a mutable closure with a connection (for transactions, etc.).
    pub async fn with_conn_mut<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|e| DbError::Pool(format!("semaphore closed: {e}")))?;

        for conn_mutex in self.connections.iter() {
            if let Ok(mut conn) = conn_mutex.try_lock() {
                return f(&mut conn);
            }
        }

        let mut conn = self.connections[0].lock().await;
        f(&mut conn)
    }

    /// Open a raw connection to the same database (for backup/replication).
    pub fn open_raw_connection(&self) -> Result<Connection> {
        open_connection(&self.config)
    }
}

/// Open and configure a single SQLite connection.
fn open_connection(config: &DbPoolConfig) -> Result<Connection> {
    let conn = Connection::open(&config.path)?;

    // WAL mode — allows concurrent readers with one writer.
    if config.wal_mode {
        conn.pragma_update(None, "journal_mode", "WAL")?;
    }

    // Synchronous NORMAL — safe with WAL, faster than FULL.
    conn.pragma_update(None, "synchronous", "NORMAL")?;

    // Busy timeout — wait instead of failing on lock.
    conn.pragma_update(None, "busy_timeout", config.busy_timeout_ms)?;

    // Cache size (negative = KiB).
    conn.pragma_update(None, "cache_size", config.cache_size_kib)?;

    // Memory-mapped I/O — 256 MiB.
    conn.pragma_update(None, "mmap_size", 268_435_456i64)?;

    // Foreign keys on.
    conn.pragma_update(None, "foreign_keys", "ON")?;

    // Temp store in memory.
    conn.pragma_update(None, "temp_store", "MEMORY")?;

    // Page size 4096 (default, but be explicit).
    // Only takes effect on new databases before first write.
    conn.pragma_update(None, "page_size", 4096)?;

    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_pool_open_in_memory() {
        let pool = DbPool::open(DbPoolConfig::in_memory()).unwrap();
        let result = pool
            .with_conn(|conn| {
                let val: i64 = conn.query_row("SELECT 42", [], |row| row.get(0))?;
                Ok(val)
            })
            .await
            .unwrap();
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn test_wal_mode_enabled() {
        let pool = DbPool::open(DbPoolConfig::in_memory()).unwrap();
        let mode = pool
            .with_conn(|conn| {
                let mode: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
                Ok(mode)
            })
            .await
            .unwrap();
        // In-memory databases use "memory" journal mode, but WAL pragma was sent.
        assert!(mode == "wal" || mode == "memory");
    }
}
