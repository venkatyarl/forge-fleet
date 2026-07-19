//! Runtime configuration for build supervision.
//!
//! Provides [`BuildConfig`], a small config struct that controls how long
//! build jobs are allowed to run before the timeout watcher terminates them.
//! Settings can be loaded from environment variables or from a TOML/JSON
//! config file.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Default maximum build duration in seconds.
pub const DEFAULT_MAX_BUILD_DURATION_SECS: u64 = 1800;

/// Environment variable used to override [`BuildConfig::max_build_duration`].
pub const MAX_BUILD_DURATION_ENV: &str = "FORGEFLEET_MAX_BUILD_DURATION_SECS";

/// Runtime build configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildConfig {
    /// Maximum wall-clock time a build is allowed to run before it is killed.
    #[serde(default = "default_max_build_duration", with = "serde_duration_secs")]
    pub max_build_duration: Duration,
}

impl BuildConfig {
    /// Load configuration from environment variables, falling back to defaults.
    ///
    /// Recognized variables:
    /// - `FORGEFLEET_MAX_BUILD_DURATION_SECS` — override max build duration.
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Ok(raw) = std::env::var(MAX_BUILD_DURATION_ENV) {
            if let Ok(secs) = raw.parse::<u64>() {
                config.max_build_duration = Duration::from_secs(secs);
            }
        }

        config
    }

    /// Load configuration from a TOML or JSON file, then apply environment
    /// variable overrides.
    ///
    /// The file format is inferred from the extension: `.toml` is parsed as
    /// TOML, otherwise it is parsed as JSON.
    pub fn from_file<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)?;

        let mut config: Self = if path.extension().is_some_and(|ext| ext == "toml") {
            toml::from_str(&raw)?
        } else {
            serde_json::from_str(&raw)?
        };

        // Environment variables take precedence over the file.
        if let Ok(raw) = std::env::var(MAX_BUILD_DURATION_ENV) {
            if let Ok(secs) = raw.parse::<u64>() {
                config.max_build_duration = Duration::from_secs(secs);
            }
        }

        Ok(config)
    }

    /// Maximum allowed build duration as a [`Duration`] accessor.
    pub fn max_build_duration(&self) -> Duration {
        self.max_build_duration
    }
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            max_build_duration: default_max_build_duration(),
        }
    }
}

fn default_max_build_duration() -> Duration {
    Duration::from_secs(DEFAULT_MAX_BUILD_DURATION_SECS)
}

mod serde_duration_secs {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        duration.as_secs().serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        let secs = u64::deserialize(deserializer)?;
        Ok(Duration::from_secs(secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_config_defaults_to_thirty_minutes() {
        let cfg = BuildConfig::default();
        assert_eq!(cfg.max_build_duration, Duration::from_secs(1800));
        assert_eq!(cfg.max_build_duration(), Duration::from_secs(1800));
    }

    #[test]
    fn build_config_from_env_overrides_default() {
        // Set env var, run test, then restore previous value.
        let prev = std::env::var(MAX_BUILD_DURATION_ENV).ok();
        unsafe {
            std::env::set_var(MAX_BUILD_DURATION_ENV, "3600");
        }

        let cfg = BuildConfig::from_env();
        assert_eq!(cfg.max_build_duration, Duration::from_secs(3600));

        unsafe {
            match prev {
                Some(value) => std::env::set_var(MAX_BUILD_DURATION_ENV, value),
                None => std::env::remove_var(MAX_BUILD_DURATION_ENV),
            }
        }
    }

    #[test]
    fn build_config_from_toml_file() {
        let tmp = tempfile::NamedTempFile::with_suffix(".toml").unwrap();
        std::fs::write(tmp.path(), "max_build_duration = 7200\n").unwrap();

        let cfg = BuildConfig::from_file(tmp.path()).unwrap();
        assert_eq!(cfg.max_build_duration, Duration::from_secs(7200));
    }

    #[test]
    fn build_config_from_json_file() {
        let tmp = tempfile::NamedTempFile::with_suffix(".json").unwrap();
        std::fs::write(tmp.path(), "{\"max_build_duration\": 900}").unwrap();

        let cfg = BuildConfig::from_file(tmp.path()).unwrap();
        assert_eq!(cfg.max_build_duration, Duration::from_secs(900));
    }
}
