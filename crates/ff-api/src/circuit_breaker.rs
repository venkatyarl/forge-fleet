//! Circuit breaker for resilient upstream routing.
//!
//! Prevents cascading failures by temporarily blocking requests to a node
//! that has exceeded the failure threshold.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Circuit breaker states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation — requests are allowed.
    Closed,
    /// Failure threshold exceeded — requests are blocked.
    Open,
    /// Testing whether the node has recovered.
    HalfOpen,
}

/// Configuration for a circuit breaker.
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Number of consecutive failures before opening the circuit.
    pub failure_threshold: u32,
    /// Duration to wait before allowing a test request (HalfOpen).
    pub recovery_timeout: Duration,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            recovery_timeout: Duration::from_secs(60),
        }
    }
}

/// Thread-safe circuit breaker.
pub struct CircuitBreaker {
    state: Mutex<CircuitState>,
    failure_count: Mutex<u32>,
    last_failure_time: Mutex<Option<Instant>>,
    config: CircuitBreakerConfig,
}

impl CircuitBreaker {
    /// Create a new circuit breaker with the given configuration.
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            state: Mutex::new(CircuitState::Closed),
            failure_count: Mutex::new(0),
            last_failure_time: Mutex::new(None),
            config,
        }
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new(CircuitBreakerConfig::default())
    }
}

impl CircuitBreaker {
    /// Record a successful request.
    /// Resets failure count and closes the circuit if it was half-open.
    pub fn record_success(&self) {
        let mut failures = self.failure_count.lock().unwrap();
        *failures = 0;
        drop(failures);

        let mut state = self.state.lock().unwrap();
        *state = CircuitState::Closed;
        drop(state);

        let mut last = self.last_failure_time.lock().unwrap();
        *last = None;
    }

    /// Record a failed request.
    /// Increments failure count and may open the circuit.
    pub fn record_failure(&self) {
        let mut failures = self.failure_count.lock().unwrap();
        *failures += 1;
        let count = *failures;
        drop(failures);

        let mut last = self.last_failure_time.lock().unwrap();
        *last = Some(Instant::now());
        drop(last);

        if count >= self.config.failure_threshold {
            let mut state = self.state.lock().unwrap();
            *state = CircuitState::Open;
        }
    }

    /// Determine whether a request should be allowed.
    pub fn allow_request(&self) -> bool {
        let state = *self.state.lock().unwrap();
        match state {
            CircuitState::Closed => true,
            CircuitState::Open => {
                // Check if recovery timeout has elapsed
                let last = *self.last_failure_time.lock().unwrap();
                if let Some(t) = last {
                    if t.elapsed() >= self.config.recovery_timeout {
                        let mut state = self.state.lock().unwrap();
                        *state = CircuitState::HalfOpen;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            CircuitState::HalfOpen => true,
        }
    }

    /// Get the current state.
    pub fn state(&self) -> CircuitState {
        *self.state.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_circuit_breaker_defaults() {
        let cb = CircuitBreaker::default();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(cb.allow_request());
    }

    #[test]
    fn test_opens_after_threshold() {
        let config = CircuitBreakerConfig {
            failure_threshold: 3,
            recovery_timeout: Duration::from_secs(60),
        };
        let cb = CircuitBreaker::new(config);
        cb.record_failure();
        cb.record_failure();
        assert!(cb.allow_request());
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);
        assert!(!cb.allow_request());
    }

    #[test]
    fn test_success_resets() {
        let cb = CircuitBreaker::default();
        cb.record_failure();
        cb.record_failure();
        cb.record_success();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(cb.allow_request());
    }

    #[test]
    fn test_half_open_after_timeout() {
        let config = CircuitBreakerConfig {
            failure_threshold: 1,
            recovery_timeout: Duration::from_millis(10),
        };
        let cb = CircuitBreaker::new(config);
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);
        std::thread::sleep(Duration::from_millis(20));
        assert!(cb.allow_request());
        assert_eq!(cb.state(), CircuitState::HalfOpen);
    }
}
