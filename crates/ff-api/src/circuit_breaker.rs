//! Circuit breaker for resilient upstream routing.
//!
//! Prevents cascading failures by temporarily blocking requests to a node
//! that has exceeded the failure threshold.
//!
//! All mutable state is stored in atomics so the breaker never blocks an
//! async task (no `std::sync::Mutex` in hot paths).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Circuit breaker states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation — requests are allowed.
    Closed = 0,
    /// Failure threshold exceeded — requests are blocked.
    Open = 1,
    /// Testing whether the node has recovered.
    HalfOpen = 2,
}

impl CircuitState {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => CircuitState::Open,
            2 => CircuitState::HalfOpen,
            _ => CircuitState::Closed,
        }
    }
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

/// Thread-safe circuit breaker backed by atomics (no mutex in hot paths).
pub struct CircuitBreaker {
    state: AtomicU32,
    failure_count: AtomicU32,
    /// u64::MAX = None, otherwise millis since `base_instant`.
    last_failure_time_ms: AtomicU64,
    base_instant: Instant,
    config: CircuitBreakerConfig,
}

impl CircuitBreaker {
    /// Create a new circuit breaker with the given configuration.
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            state: AtomicU32::new(CircuitState::Closed as u32),
            failure_count: AtomicU32::new(0),
            last_failure_time_ms: AtomicU64::new(u64::MAX),
            base_instant: Instant::now(),
            config,
        }
    }

    fn store_last_failure(&self, instant: Instant) {
        let ms = instant.duration_since(self.base_instant).as_millis() as u64;
        self.last_failure_time_ms.store(ms, Ordering::SeqCst);
    }

    fn load_last_failure(&self) -> Option<Instant> {
        let ms = self.last_failure_time_ms.load(Ordering::SeqCst);
        if ms == u64::MAX {
            None
        } else {
            Some(self.base_instant + Duration::from_millis(ms))
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
        self.failure_count.store(0, Ordering::SeqCst);
        self.state.store(CircuitState::Closed as u32, Ordering::SeqCst);
        self.last_failure_time_ms.store(u64::MAX, Ordering::SeqCst);
    }

    /// Record a failed request.
    /// Increments failure count and may open the circuit.
    pub fn record_failure(&self) {
        let count = self.failure_count.fetch_add(1, Ordering::SeqCst) + 1;
        self.store_last_failure(Instant::now());

        if count >= self.config.failure_threshold {
            self.state.store(CircuitState::Open as u32, Ordering::SeqCst);
        }
    }

    /// Determine whether a request should be allowed.
    pub fn allow_request(&self) -> bool {
        let state = CircuitState::from_u8(self.state.load(Ordering::SeqCst) as u8);
        match state {
            CircuitState::Closed => true,
            CircuitState::Open => {
                // Check if recovery timeout has elapsed
                if let Some(t) = self.load_last_failure() {
                    if t.elapsed() >= self.config.recovery_timeout {
                        self.state.store(CircuitState::HalfOpen as u32, Ordering::SeqCst);
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
        CircuitState::from_u8(self.state.load(Ordering::SeqCst) as u8)
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
        assert!(!cb.allow_request());

        std::thread::sleep(Duration::from_millis(15));
        assert!(cb.allow_request());
        assert_eq!(cb.state(), CircuitState::HalfOpen);
    }
}
