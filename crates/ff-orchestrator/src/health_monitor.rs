//! In-memory health tracking for LLM backends.

use std::collections::BTreeMap;
use std::time::Duration;

/// Aggregate health information for one LLM backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmHealth {
    pub successes: u64,
    pub failures: u64,
    pub consecutive_failures: u32,
    pub total_latency: Duration,
}

impl LlmHealth {
    /// Whether the backend is below the monitor's failure threshold.
    pub fn is_healthy(&self, failure_threshold: u32) -> bool {
        self.consecutive_failures < failure_threshold
    }

    /// Mean latency across successful requests.
    pub fn average_latency(&self) -> Option<Duration> {
        (self.successes != 0).then(|| self.total_latency / self.successes as u32)
    }
}

/// Minimal, process-local health monitor for LLM backends.
#[derive(Debug, Clone)]
pub struct LlmHealthMonitor {
    failure_threshold: u32,
    backends: BTreeMap<String, LlmHealth>,
}

impl LlmHealthMonitor {
    /// Create a monitor. A backend becomes unhealthy after this many
    /// consecutive failures.
    pub fn new(failure_threshold: u32) -> Self {
        assert!(failure_threshold > 0, "failure threshold must be non-zero");
        Self {
            failure_threshold,
            backends: BTreeMap::new(),
        }
    }

    /// Record a successful request and reset its consecutive-failure count.
    pub fn record_success(&mut self, backend: impl Into<String>, latency: Duration) {
        let health = self.entry(backend);
        health.successes = health.successes.saturating_add(1);
        health.consecutive_failures = 0;
        health.total_latency = health.total_latency.saturating_add(latency);
    }

    /// Record a failed request.
    pub fn record_failure(&mut self, backend: impl Into<String>) {
        let health = self.entry(backend);
        health.failures = health.failures.saturating_add(1);
        health.consecutive_failures = health.consecutive_failures.saturating_add(1);
    }

    /// Return the current health counters for a backend.
    pub fn health(&self, backend: &str) -> Option<&LlmHealth> {
        self.backends.get(backend)
    }

    /// Return whether a known backend is healthy.
    pub fn is_healthy(&self, backend: &str) -> Option<bool> {
        self.health(backend)
            .map(|health| health.is_healthy(self.failure_threshold))
    }

    /// Iterate over known backends in stable lexical order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &LlmHealth)> {
        self.backends
            .iter()
            .map(|(backend, health)| (backend.as_str(), health))
    }

    fn entry(&mut self, backend: impl Into<String>) -> &mut LlmHealth {
        self.backends.entry(backend.into()).or_insert(LlmHealth {
            successes: 0,
            failures: 0,
            consecutive_failures: 0,
            total_latency: Duration::ZERO,
        })
    }
}

impl Default for LlmHealthMonitor {
    fn default() -> Self {
        Self::new(3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_cross_threshold_and_success_recovers() {
        let mut monitor = LlmHealthMonitor::new(2);

        assert_eq!(monitor.is_healthy("local"), None);
        monitor.record_failure("local");
        assert_eq!(monitor.is_healthy("local"), Some(true));
        monitor.record_failure("local");
        assert_eq!(monitor.is_healthy("local"), Some(false));

        monitor.record_success("local", Duration::from_millis(40));
        assert_eq!(monitor.is_healthy("local"), Some(true));
        assert_eq!(
            monitor.health("local"),
            Some(&LlmHealth {
                successes: 1,
                failures: 2,
                consecutive_failures: 0,
                total_latency: Duration::from_millis(40),
            })
        );
    }

    #[test]
    fn latency_and_iteration_are_deterministic() {
        let mut monitor = LlmHealthMonitor::default();
        monitor.record_success("zeta", Duration::from_millis(10));
        monitor.record_success("alpha", Duration::from_millis(20));
        monitor.record_success("alpha", Duration::from_millis(40));

        assert_eq!(
            monitor.health("alpha").and_then(LlmHealth::average_latency),
            Some(Duration::from_millis(30))
        );
        assert_eq!(
            monitor
                .iter()
                .map(|(backend, _)| backend)
                .collect::<Vec<_>>(),
            ["alpha", "zeta"]
        );
    }
}
