//! File-based logging with daily rotation and retention limits.
//!
//! Uses [`tracing_appender`] for non-blocking rolling file output.
//! The formatting layer (JSON/text) is configured in `telemetry.rs`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_appender::rolling;

/// Configuration for file-based logging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileLogConfig {
    /// Whether file logging is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Directory to write log files into.
    /// Defaults to `~/.forgefleet/logs/`.
    #[serde(default = "default_log_dir")]
    pub log_dir: PathBuf,

    /// Filename prefix for log files.
    #[serde(default = "default_file_prefix")]
    pub file_prefix: String,

    /// Maximum number of rotated log files to keep.
    /// Older files are deleted automatically by the appender.
    #[serde(default = "default_max_files")]
    pub max_files: usize,

    /// Whether to emit structured JSON to file logs.
    #[serde(default = "default_true")]
    pub json: bool,

    /// Include source file location in file logs.
    #[serde(default)]
    pub include_location: bool,

    /// Include span fields in file logs.
    #[serde(default = "default_true")]
    pub include_spans: bool,
}

fn default_true() -> bool {
    true
}

fn default_log_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".forgefleet")
        .join("logs")
}

fn default_file_prefix() -> String {
    "forgefleetd".to_string()
}

fn default_max_files() -> usize {
    30
}

impl Default for FileLogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            log_dir: default_log_dir(),
            file_prefix: default_file_prefix(),
            max_files: default_max_files(),
            json: true,
            include_location: false,
            include_spans: true,
        }
    }
}

impl FileLogConfig {
    /// Recommended defaults for daemon runtime retention (7 days).
    pub fn daemon_defaults() -> Self {
        Self {
            max_files: 7,
            ..Self::default()
        }
    }
}

/// Build a non-blocking writer backed by a daily-rotating file appender.
///
/// The returned [`WorkerGuard`] must be kept alive for process lifetime so
/// logs flush correctly on shutdown.
pub fn create_non_blocking_writer(
    config: &FileLogConfig,
) -> anyhow::Result<(NonBlocking, WorkerGuard)> {
    std::fs::create_dir_all(&config.log_dir)?;

    let appender = rolling::Builder::new()
        .rotation(rolling::Rotation::DAILY)
        .filename_prefix(&config.file_prefix)
        .filename_suffix("log")
        .max_log_files(config.max_files)
        .build(&config.log_dir)?;

    let (writer, guard) = tracing_appender::non_blocking(appender);
    Ok((writer, guard))
}

/// Manual cleanup helper for environments where extra pruning is needed.
pub fn cleanup_old_logs(log_dir: &Path, prefix: &str, max_files: usize) -> std::io::Result<u32> {
    let mut log_files: Vec<_> = std::fs::read_dir(log_dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
        .collect();

    log_files.sort_by(|a, b| {
        let a_time = a
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let b_time = b
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        b_time.cmp(&a_time)
    });

    let mut removed = 0;
    for old_file in log_files.into_iter().skip(max_files) {
        std::fs::remove_file(old_file.path())?;
        removed += 1;
    }

    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = FileLogConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.max_files, 30);
        assert!(cfg.json);
    }

    #[test]
    fn daemon_defaults_use_7_days() {
        let cfg = FileLogConfig::daemon_defaults();
        assert_eq!(cfg.max_files, 7);
    }
}
