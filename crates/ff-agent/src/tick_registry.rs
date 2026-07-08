use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Duration,
};

use rand::Rng;

/// Configuration for a registered daemon tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TickConfig {
    /// Base interval between tick invocations.
    pub interval: Duration,
    /// Jitter magnitude as a fraction of `interval` (e.g. `0.1` for +/-10%).
    /// Clamped to the range `[0.0, 1.0]`.
    pub jitter_pct: f64,
}

impl TickConfig {
    /// Create a simple config with no jitter.
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            jitter_pct: 0.0,
        }
    }

    /// Create a config with the given base interval and jitter percentage.
    pub fn with_jitter(interval: Duration, jitter_pct: f64) -> Self {
        Self {
            interval,
            jitter_pct: jitter_pct.clamp(0.0, 1.0),
        }
    }
}

/// In-memory registry of named tick configurations.
///
/// Mirrors the registry pattern used by other agent subsystems: a shared
/// `Arc<RwLock<HashMap<...>>>` allows cheap reads and serialized writes.
#[derive(Clone, Debug, Default)]
pub struct TickRegistry {
    ticks: Arc<RwLock<HashMap<String, TickConfig>>>,
}

impl TickRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            ticks: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register or overwrite a tick configuration under `name`.
    pub fn register_tick(&self, name: impl Into<String>, config: TickConfig) {
        let name = name.into();
        let mut ticks = self
            .ticks
            .write()
            .expect("tick registry lock should not be poisoned");
        ticks.insert(name, config);
    }

    /// Return the configured base interval for `name`, if registered.
    pub fn get_tick_interval(&self, name: &str) -> Option<Duration> {
        let ticks = self
            .ticks
            .read()
            .expect("tick registry lock should not be poisoned");
        ticks.get(name).map(|config| config.interval)
    }

    /// Look up `name` and return its jittered interval.
    ///
    /// Returns `None` if the tick is not registered.
    pub fn get_jittered_interval(&self, name: &str) -> Option<Duration> {
        let ticks = self
            .ticks
            .read()
            .expect("tick registry lock should not be poisoned");
        ticks
            .get(name)
            .map(|config| Self::calculate_jittered_interval(config.interval, config.jitter_pct))
    }

    /// Apply random +/- jitter to `interval`.
    ///
    /// `jitter_pct` is clamped to `[0.0, 1.0]`. The result is always at least
    /// one millisecond so callers never receive a zero-length sleep.
    pub fn calculate_jittered_interval(interval: Duration, jitter_pct: f64) -> Duration {
        let jitter_pct = jitter_pct.clamp(0.0, 1.0);
        if jitter_pct == 0.0 || interval.is_zero() {
            return interval.max(Duration::from_millis(1));
        }

        let base_secs = interval.as_secs_f64();
        let jitter_factor = {
            let mut rng = rand::thread_rng();
            let max_delta = jitter_pct;
            let delta = rng.gen_range(-max_delta..=max_delta);
            1.0 + delta
        };

        let jittered_millis = (base_secs * jitter_factor * 1000.0).round().max(1.0) as u64;
        Duration::from_millis(jittered_millis)
    }
}

impl Default for TickConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(60),
            jitter_pct: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_lookup_round_trip() {
        let registry = TickRegistry::new();
        let config = TickConfig::with_jitter(Duration::from_secs(30), 0.1);
        registry.register_tick("test_tick", config);

        assert_eq!(
            registry.get_tick_interval("test_tick"),
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn unknown_tick_returns_none() {
        let registry = TickRegistry::new();
        assert!(registry.get_tick_interval("missing").is_none());
        assert!(registry.get_jittered_interval("missing").is_none());
    }

    #[test]
    fn jittered_interval_stays_within_bounds() {
        let interval = Duration::from_secs(60);
        let jitter_pct = 0.2;

        for _ in 0..100 {
            let jittered = TickRegistry::calculate_jittered_interval(interval, jitter_pct);
            let lower = Duration::from_secs_f64(interval.as_secs_f64() * 0.8);
            let upper = Duration::from_secs_f64(interval.as_secs_f64() * 1.2);
            assert!(jittered >= lower && jittered <= upper);
        }
    }

    #[test]
    fn zero_jitter_returns_base_interval() {
        let interval = Duration::from_secs(42);
        assert_eq!(
            TickRegistry::calculate_jittered_interval(interval, 0.0),
            interval
        );
    }
}
