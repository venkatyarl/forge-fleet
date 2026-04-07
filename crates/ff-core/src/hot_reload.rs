//! Hot-reload system for ForgeFleet configuration.
//!
//! Provides SHA256-based file watching and atomic config updates:
//! - [`ConfigWatcher`] — polls `fleet.toml` every 2 seconds, detects changes via SHA256
//! - [`ConfigBroadcaster`] — `tokio::watch` channel for broadcasting config changes
//! - [`atomic_write`] — write-to-tmp + rename for crash-safe config updates
//! - [`validate_config`] — pre-flight validation before applying changes

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use crate::config::{FleetConfig, apply_env_overrides};
use crate::error::{ForgeFleetError, Result};

// ─── Types ───────────────────────────────────────────────────────────────────

/// Sender side of the config broadcast channel.
pub type ConfigBroadcaster = watch::Sender<Arc<FleetConfig>>;

/// Receiver side — clone this for each subsystem that needs config updates.
pub type ConfigReceiver = watch::Receiver<Arc<FleetConfig>>;

/// Create a new broadcaster/receiver pair from an initial config.
pub fn new_broadcast(config: FleetConfig) -> (ConfigBroadcaster, ConfigReceiver) {
    watch::channel(Arc::new(config))
}

// ─── SHA256 hashing ──────────────────────────────────────────────────────────

/// Compute SHA256 hash of file contents. Returns hex string.
pub fn sha256_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(sha256_bytes(&bytes))
}

/// Compute SHA256 hash of a byte slice. Returns hex string.
pub fn sha256_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    hex_encode(&result)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ─── Validation ──────────────────────────────────────────────────────────────

/// Validate a config before applying it.
///
/// Checks:
/// - Fleet name is not empty
/// - API port is non-zero
/// - All node IPs are non-empty
/// - Heartbeat interval > 0
///
/// Returns `Ok(())` if valid, or a descriptive error.
pub fn validate_config(config: &FleetConfig) -> Result<()> {
    if config.fleet.name.trim().is_empty() {
        return Err(ForgeFleetError::Config(
            "fleet name cannot be empty".into(),
        ));
    }

    if config.fleet.api_port == 0 {
        return Err(ForgeFleetError::Config(
            "api_port cannot be zero".into(),
        ));
    }

    if config.fleet.heartbeat_interval_secs == 0 {
        return Err(ForgeFleetError::Config(
            "heartbeat_interval_secs must be > 0".into(),
        ));
    }

    for (name, node) in &config.nodes {
        if node.ip.trim().is_empty() {
            return Err(ForgeFleetError::Config(format!(
                "node '{name}' has an empty IP address"
            )));
        }
    }

    Ok(())
}

// ─── Atomic write ────────────────────────────────────────────────────────────

/// Write config to a file atomically (write to .tmp, then rename).
///
/// This ensures readers never see a partially-written file.
pub fn atomic_write(path: &Path, config: &FleetConfig) -> Result<()> {
    let serialized = toml::to_string_pretty(config)
        .map_err(ForgeFleetError::TomlSerialize)?;

    let tmp_path = path.with_extension("toml.tmp");

    // Write to temp file.
    std::fs::write(&tmp_path, serialized.as_bytes())?;

    // Atomic rename.
    std::fs::rename(&tmp_path, path)?;

    info!(path = %path.display(), "config written atomically");
    Ok(())
}

// ─── ConfigWatcher ───────────────────────────────────────────────────────────

/// Watches a config file for changes using SHA256 polling.
///
/// When a change is detected, the new config is parsed, validated,
/// and broadcast to all subscribers via the `ConfigBroadcaster`.
pub struct ConfigWatcher {
    path: PathBuf,
    tx: ConfigBroadcaster,
    poll_interval: std::time::Duration,
    last_hash: Option<String>,
}

impl ConfigWatcher {
    /// Create a new watcher for the given config path and broadcaster.
    pub fn new(path: PathBuf, tx: ConfigBroadcaster) -> Self {
        // Compute initial hash so we only broadcast on actual changes.
        let last_hash = sha256_file(&path).ok();
        Self {
            path,
            tx,
            poll_interval: std::time::Duration::from_secs(2),
            last_hash,
        }
    }

    /// Override the poll interval (useful for tests).
    pub fn with_poll_interval(mut self, interval: std::time::Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Run the watcher loop. This blocks the current task forever
    /// (or until the broadcaster is dropped, which closes receivers).
    ///
    /// Call this via `tokio::spawn(watcher.run())`.
    pub async fn run(mut self) {
        info!(
            path = %self.path.display(),
            poll_ms = self.poll_interval.as_millis(),
            "config watcher started"
        );

        loop {
            tokio::time::sleep(self.poll_interval).await;

            match self.check_and_reload() {
                Ok(true) => {
                    debug!("config change detected and broadcast");
                }
                Ok(false) => {
                    // No change — nothing to do.
                }
                Err(e) => {
                    warn!(error = %e, "config reload failed — keeping previous config");
                }
            }
        }
    }

    /// Check for file changes and reload if needed.
    ///
    /// Returns `Ok(true)` if config was reloaded, `Ok(false)` if unchanged.
    pub fn check_and_reload(&mut self) -> Result<bool> {
        let current_hash = match sha256_file(&self.path) {
            Ok(h) => h,
            Err(e) => {
                // File might be temporarily missing during atomic write.
                debug!(error = %e, "could not hash config file — skipping cycle");
                return Ok(false);
            }
        };

        if self.last_hash.as_deref() == Some(&current_hash) {
            return Ok(false);
        }

        info!(
            path = %self.path.display(),
            old_hash = self.last_hash.as_deref().unwrap_or("none"),
            new_hash = %current_hash,
            "config file changed — reloading"
        );

        // Parse new config.
        let mut config = {
            let raw = std::fs::read_to_string(&self.path)?;
            let cfg: FleetConfig = toml::from_str(&raw)?;
            cfg
        };

        // Apply env overrides.
        apply_env_overrides(&mut config);

        // Validate before broadcasting.
        validate_config(&config)?;

        // Broadcast to all subscribers.
        let arc = Arc::new(config);
        if self.tx.send(arc).is_err() {
            error!("all config receivers dropped — watcher has no subscribers");
        }

        self.last_hash = Some(current_hash);
        Ok(true)
    }
}

/// Spawn the config watcher as a background tokio task.
///
/// Returns the `JoinHandle` so callers can abort it on shutdown.
pub fn spawn_config_watcher(
    path: PathBuf,
    tx: ConfigBroadcaster,
) -> tokio::task::JoinHandle<()> {
    let watcher = ConfigWatcher::new(path, tx);
    tokio::spawn(watcher.run())
}

// ─── Save + validate helper ─────────────────────────────────────────────────

/// Validate and atomically save a new config.
///
/// This is the function the `POST /api/config` endpoint should call.
/// The ConfigWatcher will detect the change and broadcast it automatically.
pub fn save_config(path: &Path, config: &FleetConfig) -> Result<()> {
    validate_config(config)?;
    atomic_write(path, config)?;
    Ok(())
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_config;

    /// Minimal valid TOML for testing.
    fn minimal_toml() -> &'static str {
        r#"
[general]
name = "TestFleet"
api_port = 51800
heartbeat_interval_secs = 15
"#
    }

    fn modified_toml() -> &'static str {
        r#"
[general]
name = "ModifiedFleet"
api_port = 51800
heartbeat_interval_secs = 15
"#
    }

    #[test]
    fn test_sha256_deterministic() {
        let a = sha256_bytes(b"hello world");
        let b = sha256_bytes(b"hello world");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64); // 256 bits = 64 hex chars
    }

    #[test]
    fn test_sha256_different_input() {
        let a = sha256_bytes(b"hello");
        let b = sha256_bytes(b"world");
        assert_ne!(a, b);
    }

    #[test]
    fn test_validate_config_ok() {
        let config: FleetConfig = toml::from_str(minimal_toml()).unwrap();
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn test_validate_empty_name() {
        let mut config: FleetConfig = toml::from_str(minimal_toml()).unwrap();
        config.fleet.name = "".into();
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_zero_port() {
        let mut config: FleetConfig = toml::from_str(minimal_toml()).unwrap();
        config.fleet.api_port = 0;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_zero_heartbeat() {
        let mut config: FleetConfig = toml::from_str(minimal_toml()).unwrap();
        config.fleet.heartbeat_interval_secs = 0;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_empty_node_ip() {
        let mut config: FleetConfig = toml::from_str(minimal_toml()).unwrap();
        let mut node = crate::config::NodeConfig::default();
        node.ip = "".into();
        config.nodes.insert("bad".into(), node);
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_atomic_write_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.toml");

        let config: FleetConfig = toml::from_str(minimal_toml()).unwrap();
        atomic_write(&path, &config).unwrap();

        // Read back and verify.
        let reloaded = load_config(&path).unwrap();
        assert_eq!(reloaded.fleet.name, "TestFleet");

        // Temp file should NOT exist.
        let tmp = path.with_extension("toml.tmp");
        assert!(!tmp.exists());
    }

    #[test]
    fn test_atomic_write_replaces_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.toml");

        // Write initial.
        let config1: FleetConfig = toml::from_str(minimal_toml()).unwrap();
        atomic_write(&path, &config1).unwrap();

        // Write modified.
        let config2: FleetConfig = toml::from_str(modified_toml()).unwrap();
        atomic_write(&path, &config2).unwrap();

        let reloaded = load_config(&path).unwrap();
        assert_eq!(reloaded.fleet.name, "ModifiedFleet");
    }

    #[test]
    fn test_sha256_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, b"hello world").unwrap();

        let hash = sha256_file(&path).unwrap();
        assert_eq!(hash.len(), 64);

        // Same content = same hash.
        let hash2 = sha256_file(&path).unwrap();
        assert_eq!(hash, hash2);

        // Different content = different hash.
        std::fs::write(&path, b"goodbye world").unwrap();
        let hash3 = sha256_file(&path).unwrap();
        assert_ne!(hash, hash3);
    }

    #[test]
    fn test_watcher_detects_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.toml");

        // Write initial config.
        let config: FleetConfig = toml::from_str(minimal_toml()).unwrap();
        atomic_write(&path, &config).unwrap();

        // Create broadcaster.
        let (tx, mut rx) = new_broadcast(config.clone());

        // Create watcher.
        let mut watcher = ConfigWatcher::new(path.clone(), tx);

        // No change yet.
        assert_eq!(watcher.check_and_reload().unwrap(), false);

        // Write modified config.
        let config2: FleetConfig = toml::from_str(modified_toml()).unwrap();
        atomic_write(&path, &config2).unwrap();

        // Now should detect change.
        assert_eq!(watcher.check_and_reload().unwrap(), true);

        // Receiver should have the new config.
        assert!(rx.has_changed().unwrap());
        let new_cfg = rx.borrow_and_update();
        assert_eq!(new_cfg.fleet.name, "ModifiedFleet");
    }

    #[test]
    fn test_watcher_no_false_positive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.toml");

        let config: FleetConfig = toml::from_str(minimal_toml()).unwrap();
        atomic_write(&path, &config).unwrap();

        let (tx, _rx) = new_broadcast(config.clone());
        let mut watcher = ConfigWatcher::new(path.clone(), tx);

        // Multiple checks with no change.
        assert_eq!(watcher.check_and_reload().unwrap(), false);
        assert_eq!(watcher.check_and_reload().unwrap(), false);
        assert_eq!(watcher.check_and_reload().unwrap(), false);
    }

    #[test]
    fn test_watcher_rejects_invalid_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.toml");

        let config: FleetConfig = toml::from_str(minimal_toml()).unwrap();
        atomic_write(&path, &config).unwrap();

        let (tx, _rx) = new_broadcast(config);
        let mut watcher = ConfigWatcher::new(path.clone(), tx);

        // Write an invalid config (empty fleet name).
        let bad_toml = r#"
[general]
name = ""
api_port = 51800
heartbeat_interval_secs = 15
"#;
        std::fs::write(&path, bad_toml).unwrap();

        // Should detect change but reject it.
        let result = watcher.check_and_reload();
        assert!(result.is_err());
    }

    #[test]
    fn test_save_config_validates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.toml");

        // Valid save should work.
        let config: FleetConfig = toml::from_str(minimal_toml()).unwrap();
        assert!(save_config(&path, &config).is_ok());

        // Invalid save should fail without writing.
        let mut bad = config.clone();
        bad.fleet.name = "".into();
        assert!(save_config(&path, &bad).is_err());

        // Original config should still be on disk.
        let on_disk = load_config(&path).unwrap();
        assert_eq!(on_disk.fleet.name, "TestFleet");
    }

    #[tokio::test]
    async fn test_watcher_async_detects_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.toml");

        let config: FleetConfig = toml::from_str(minimal_toml()).unwrap();
        atomic_write(&path, &config).unwrap();

        let (tx, mut rx) = new_broadcast(config);

        // Spawn watcher with fast polling for tests.
        let watcher = ConfigWatcher::new(path.clone(), tx)
            .with_poll_interval(std::time::Duration::from_millis(50));
        let handle = tokio::spawn(watcher.run());

        // Give the watcher a moment to start.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Write modified config.
        let config2: FleetConfig = toml::from_str(modified_toml()).unwrap();
        atomic_write(&path, &config2).unwrap();

        // Wait for the change to propagate.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        assert!(rx.has_changed().unwrap());
        let new_cfg = rx.borrow_and_update();
        assert_eq!(new_cfg.fleet.name, "ModifiedFleet");

        handle.abort();
    }

    #[test]
    fn test_new_broadcast() {
        let config: FleetConfig = toml::from_str(minimal_toml()).unwrap();
        let (tx, rx) = new_broadcast(config);
        assert_eq!(rx.borrow().fleet.name, "TestFleet");
        drop(tx);
    }
}
