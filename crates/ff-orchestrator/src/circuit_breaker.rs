//! Circuit breaker state management for orchestration backends.
//!
//! The state machine lives in `ff-core` so every ForgeFleet component uses the
//! same transition rules. This wrapper exposes that behavior from the
//! orchestrator without maintaining a second, divergent implementation.

pub use ff_core::{CircuitBreakerConfig, CircuitBreakerSnapshot, CircuitState};

/// Tracks consecutive backend failures and circuit state transitions.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    inner: ff_core::CircuitBreaker,
}

impl CircuitBreaker {
    /// Create a closed circuit breaker with the supplied configuration.
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            inner: ff_core::CircuitBreaker::new(config),
        }
    }

    /// Return the current circuit state.
    pub fn state(&self) -> CircuitState {
        self.inner.state()
    }

    /// Return whether the next request may proceed.
    ///
    /// Once the recovery timeout elapses, this transitions an open circuit to
    /// half-open and permits a recovery probe.
    pub fn is_allowed(&mut self) -> bool {
        self.inner.is_allowed()
    }

    /// Record a successful backend request.
    pub fn record_success(&mut self) {
        self.inner.record_success();
    }

    /// Record a failed backend request.
    pub fn record_failure(&mut self) {
        self.inner.record_failure();
    }

    /// Manually return the circuit to its closed state.
    pub fn reset(&mut self) {
        self.inner.reset();
    }

    /// Return a snapshot of the current state and lifetime counters.
    pub fn snapshot(&self) -> CircuitBreakerSnapshot {
        self.inner.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn fast_config() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            failure_threshold: 2,
            recovery_timeout: Duration::from_millis(10),
            success_threshold_for_close: 1,
        }
    }

    #[test]
    fn opens_at_failure_threshold() {
        let mut breaker = CircuitBreaker::new(fast_config());

        breaker.record_failure();
        assert_eq!(breaker.state(), CircuitState::Closed);

        breaker.record_failure();
        assert_eq!(breaker.state(), CircuitState::Open);
        assert!(!breaker.is_allowed());
    }

    #[tokio::test]
    async fn recovers_after_timeout_and_success() {
        let mut breaker = CircuitBreaker::new(fast_config());
        breaker.record_failure();
        breaker.record_failure();

        tokio::time::sleep(Duration::from_millis(15)).await;
        assert!(breaker.is_allowed());
        assert_eq!(breaker.state(), CircuitState::HalfOpen);

        breaker.record_success();
        assert_eq!(breaker.state(), CircuitState::Closed);
    }
}
