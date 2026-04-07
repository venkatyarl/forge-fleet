//! Shared application state for ForgeFleet.
//!
//! [`AppState`] is the central struct passed to all axum handlers and subsystems.
//! It holds the current config, database handle, broadcaster, and node identity.
//!
//! Designed to be `Clone`-friendly (all `Arc`/`Clone` types) so axum can
//! share it across handler tasks without explicit wrapping.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::info;

use crate::config::FleetConfig;
use crate::hot_reload::{ConfigBroadcaster, ConfigReceiver};
use crate::types::Role;

// ─── AppState ────────────────────────────────────────────────────────────────

/// Shared application state for the ForgeFleet daemon.
///
/// Every field is `Clone`-friendly. Axum handlers receive this via
/// `axum::extract::State<AppState>`.
///
/// # Config Access
///
/// Use [`config()`](AppState::config) to get a snapshot of the current config.
/// The snapshot is an `Arc<FleetConfig>` — cheap to clone, never stale for
/// the duration of a single request.
///
/// For long-running tasks that need to react to config changes,
/// call [`config_receiver()`](AppState::config_receiver) and await changes.
#[derive(Clone)]
pub struct AppState {
    /// Current config — updated on hot-reload via watch channel.
    config_rx: ConfigReceiver,

    /// Broadcaster for sending config updates (wrapped in Arc<Mutex> so
    /// AppState stays Clone even though watch::Sender isn't Clone).
    config_tx: Arc<Mutex<ConfigBroadcaster>>,

    /// Path to fleet.toml on disk.
    config_path: PathBuf,

    /// This node's name (e.g. "taylor", "marcus").
    node_name: Arc<String>,

    /// This node's role.
    role: Role,

    /// Type-erased database pool handle.
    ///
    /// We use `Arc<dyn Any + Send + Sync>` so ff-core doesn't need to depend
    /// on ff-db. Callers downcast to the concrete pool type.
    db_pool: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

impl AppState {
    /// Create AppState from pre-built broadcast channel components.
    ///
    /// This is the preferred constructor. Create the broadcast channel first
    /// via [`new_broadcast`](crate::hot_reload::new_broadcast), then pass
    /// the receiver here. The sender goes to the watcher AND to this struct.
    ///
    /// # Example
    /// ```ignore
    /// let config = load_config(&path)?;
    /// let (tx, rx) = hot_reload::new_broadcast(config);
    /// let state = AppState::new(rx, tx, path, "taylor", Role::Gateway);
    /// // Pass state.take_broadcaster() to the watcher.
    /// ```
    pub fn new(
        config_rx: ConfigReceiver,
        config_tx: ConfigBroadcaster,
        config_path: PathBuf,
        node_name: impl Into<String>,
        role: Role,
    ) -> Self {
        let node_name = node_name.into();
        info!(
            node = %node_name,
            role = %role,
            config = %config_path.display(),
            "AppState initialized"
        );
        Self {
            config_rx,
            config_tx: Arc::new(Mutex::new(config_tx)),
            config_path,
            node_name: Arc::new(node_name),
            role,
            db_pool: None,
        }
    }

    /// Attach a database pool (type-erased).
    ///
    /// # Example
    /// ```ignore
    /// let pool = ff_db::DbPool::open(config)?;
    /// let state = state.with_db(pool);
    /// ```
    pub fn with_db<T: Send + Sync + 'static>(mut self, pool: T) -> Self {
        self.db_pool = Some(Arc::new(pool));
        self
    }

    // ── Accessors ────────────────────────────────────────────────────────────

    /// Get a snapshot of the current config.
    ///
    /// This is cheap (`Arc::clone`) and safe to call from any handler.
    pub fn config(&self) -> Arc<FleetConfig> {
        self.config_rx.borrow().clone()
    }

    /// Get a new receiver for config changes.
    ///
    /// Use this in long-running tasks:
    /// ```ignore
    /// let mut rx = state.config_receiver();
    /// loop {
    ///     rx.changed().await.unwrap();
    ///     let cfg = rx.borrow().clone();
    ///     // react to new config
    /// }
    /// ```
    pub fn config_receiver(&self) -> ConfigReceiver {
        self.config_rx.clone()
    }

    /// Access the config broadcaster (for save endpoint / direct broadcast).
    ///
    /// The Mutex is held briefly — just long enough to call `send()`.
    pub async fn broadcast_config(&self, config: FleetConfig) {
        let tx = self.config_tx.lock().await;
        let _ = tx.send(Arc::new(config));
    }

    /// Synchronously broadcast a config (for non-async contexts).
    /// Returns false if the lock couldn't be acquired immediately.
    pub fn try_broadcast_config(&self, config: FleetConfig) -> bool {
        match self.config_tx.try_lock() {
            Ok(tx) => {
                let _ = tx.send(Arc::new(config));
                true
            }
            Err(_) => false,
        }
    }

    /// Path to fleet.toml on disk.
    pub fn config_path(&self) -> &PathBuf {
        &self.config_path
    }

    /// This node's name.
    pub fn node_name(&self) -> &str {
        &self.node_name
    }

    /// This node's role.
    pub fn role(&self) -> Role {
        self.role
    }

    /// Try to get the database pool as a specific type.
    ///
    /// Returns `None` if no pool is attached or if the type doesn't match.
    pub fn db<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.db_pool.as_ref()?.downcast_ref::<T>()
    }

    /// Check if a database pool is attached.
    pub fn has_db(&self) -> bool {
        self.db_pool.is_some()
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("node_name", &self.node_name)
            .field("role", &self.role)
            .field("config_path", &self.config_path)
            .field("has_db", &self.db_pool.is_some())
            .finish()
    }
}

// ─── Builder ─────────────────────────────────────────────────────────────────

/// Builder for [`AppState`]. Preferred for complex construction.
///
/// # Example
/// ```ignore
/// let (tx, rx) = hot_reload::new_broadcast(config);
/// let state = AppStateBuilder::new(rx, tx)
///     .config_path(path)
///     .node_name("taylor")
///     .role(Role::Gateway)
///     .build();
/// ```
pub struct AppStateBuilder {
    config_rx: ConfigReceiver,
    config_tx: ConfigBroadcaster,
    config_path: PathBuf,
    node_name: String,
    role: Role,
    db_pool: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

impl AppStateBuilder {
    pub fn new(config_rx: ConfigReceiver, config_tx: ConfigBroadcaster) -> Self {
        Self {
            config_rx,
            config_tx,
            config_path: PathBuf::from("fleet.toml"),
            node_name: "unknown".into(),
            role: Role::Worker,
            db_pool: None,
        }
    }

    pub fn config_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_path = path.into();
        self
    }

    pub fn node_name(mut self, name: impl Into<String>) -> Self {
        self.node_name = name.into();
        self
    }

    pub fn role(mut self, role: Role) -> Self {
        self.role = role;
        self
    }

    pub fn db<T: Send + Sync + 'static>(mut self, pool: T) -> Self {
        self.db_pool = Some(Arc::new(pool));
        self
    }

    pub fn build(self) -> AppState {
        info!(
            node = %self.node_name,
            role = %self.role,
            config = %self.config_path.display(),
            "AppState built via builder"
        );
        AppState {
            config_rx: self.config_rx,
            config_tx: Arc::new(Mutex::new(self.config_tx)),
            config_path: self.config_path,
            node_name: Arc::new(self.node_name),
            role: self.role,
            db_pool: self.db_pool,
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hot_reload::new_broadcast;

    fn test_config() -> FleetConfig {
        toml::from_str(
            r#"
[general]
name = "TestFleet"
api_port = 51800
heartbeat_interval_secs = 15
"#,
        )
        .unwrap()
    }

    #[test]
    fn test_appstate_new() {
        let config = test_config();
        let (tx, rx) = new_broadcast(config);

        let state = AppState::new(
            rx,
            tx,
            PathBuf::from("/tmp/fleet.toml"),
            "taylor",
            Role::Gateway,
        );

        assert_eq!(state.node_name(), "taylor");
        assert_eq!(state.role(), Role::Gateway);
        assert_eq!(state.config().fleet.name, "TestFleet");
        assert!(!state.has_db());
    }

    #[test]
    fn test_appstate_builder() {
        let config = test_config();
        let (tx, rx) = new_broadcast(config);

        let state = AppStateBuilder::new(rx, tx)
            .config_path("/tmp/fleet.toml")
            .node_name("marcus")
            .role(Role::Builder)
            .build();

        assert_eq!(state.node_name(), "marcus");
        assert_eq!(state.role(), Role::Builder);
        assert_eq!(state.config().fleet.name, "TestFleet");
    }

    #[test]
    fn test_appstate_clone() {
        let config = test_config();
        let (tx, rx) = new_broadcast(config);

        let state = AppState::new(
            rx,
            tx,
            PathBuf::from("/tmp/fleet.toml"),
            "taylor",
            Role::Gateway,
        );

        let cloned = state.clone();
        assert_eq!(cloned.node_name(), "taylor");
        assert_eq!(cloned.config().fleet.name, "TestFleet");
    }

    #[test]
    fn test_appstate_with_db() {
        let config = test_config();
        let (tx, rx) = new_broadcast(config);

        #[derive(Debug)]
        struct FakePool {
            name: String,
        }

        let state = AppState::new(
            rx,
            tx,
            PathBuf::from("/tmp/fleet.toml"),
            "taylor",
            Role::Gateway,
        )
        .with_db(FakePool {
            name: "test".into(),
        });

        assert!(state.has_db());
        let pool: &FakePool = state.db::<FakePool>().unwrap();
        assert_eq!(pool.name, "test");

        // Wrong type returns None.
        assert!(state.db::<String>().is_none());
    }

    #[tokio::test]
    async fn test_appstate_broadcast_config() {
        let config = test_config();
        let (tx, rx) = new_broadcast(config);

        let state = AppState::new(
            rx,
            tx,
            PathBuf::from("/tmp/fleet.toml"),
            "taylor",
            Role::Gateway,
        );

        // Broadcast a modified config.
        let mut new_cfg = test_config();
        new_cfg.fleet.name = "UpdatedFleet".into();
        state.broadcast_config(new_cfg).await;

        // State should reflect the new config.
        assert_eq!(state.config().fleet.name, "UpdatedFleet");
    }

    #[test]
    fn test_appstate_try_broadcast() {
        let config = test_config();
        let (tx, rx) = new_broadcast(config);

        let state = AppState::new(
            rx,
            tx,
            PathBuf::from("/tmp/fleet.toml"),
            "taylor",
            Role::Gateway,
        );

        let mut new_cfg = test_config();
        new_cfg.fleet.name = "SyncUpdate".into();
        assert!(state.try_broadcast_config(new_cfg));
        assert_eq!(state.config().fleet.name, "SyncUpdate");
    }

    #[test]
    fn test_appstate_config_receiver() {
        let config = test_config();
        let (tx, rx) = new_broadcast(config);

        let state = AppState::new(
            rx,
            tx,
            PathBuf::from("/tmp/fleet.toml"),
            "taylor",
            Role::Gateway,
        );

        let mut rx = state.config_receiver();
        assert_eq!(rx.borrow().fleet.name, "TestFleet");

        // Broadcast new config.
        let mut new_cfg = test_config();
        new_cfg.fleet.name = "NewFleet".into();
        assert!(state.try_broadcast_config(new_cfg));

        assert!(rx.has_changed().unwrap());
        assert_eq!(rx.borrow_and_update().fleet.name, "NewFleet");
    }

    #[test]
    fn test_appstate_debug() {
        let config = test_config();
        let (tx, rx) = new_broadcast(config);

        let state = AppState::new(
            rx,
            tx,
            PathBuf::from("/tmp/fleet.toml"),
            "taylor",
            Role::Gateway,
        );

        let debug = format!("{state:?}");
        assert!(debug.contains("taylor"));
        assert!(debug.contains("Gateway"));
    }

    #[test]
    fn test_builder_with_db() {
        let config = test_config();
        let (tx, rx) = new_broadcast(config);

        let state = AppStateBuilder::new(rx, tx)
            .config_path("/tmp/fleet.toml")
            .node_name("taylor")
            .role(Role::Gateway)
            .db(42u64)
            .build();

        assert!(state.has_db());
        assert_eq!(*state.db::<u64>().unwrap(), 42);
    }
}
