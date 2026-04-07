//! Circuit breaker pattern for ForgeFleet backends.
//!
//! Prevents cascading failures by tracking per-backend error rates and
//! short-circuiting requests to unhealthy endpoints.
//!
//! Three states:
//! - **Closed** — normal operation, requests flow through
//! - **Open** — backend is failing, requests are rejected immediately
//! - **HalfOpen** — recovery probe in progress, limited requests allowed
//!
//! State transitions:
//! ```text
//! Closed ──(failures ≥ threshold)──► Open
//! Open ──(recovery_timeout elapsed)──► HalfOpen
//! HalfOpen ──(success_threshold met)──► Closed
//! HalfOpen ──(any failure)──► Open
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info};

// ─── Backend Identifier ──────────────────────────────────────────────────────

/// Opaque backend identifier — typically `"node:port"` or a model endpoint slug.
pub type BackendId = String;

// ─── Configuration ───────────────────────────────────────────────────────────

/// Tunable knobs for a circuit breaker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerConfig {
    /// Number of consecutive failures before the breaker opens.
    pub failure_threshold: u32,
    /// How long to stay open before probing recovery.
    pub recovery_timeout: Duration,
    /// Consecutive successes in half-open state required to close again.
    pub success_threshold_for_close: u32,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            recovery_timeout: Duration::from_secs(60),
            success_threshold_for_close: 3,
        }
    }
}

// ─── State ───────────────────────────────────────────────────────────────────

/// The three canonical circuit breaker states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CircuitState {
    /// Normal — requests pass through.
    Closed,
    /// Failing — requests are rejected immediately.
    Open,
    /// Probing — a limited number of requests are allowed to test recovery.
    HalfOpen,
}

impl std::fmt::Display for CircuitState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(f, "Closed"),
            Self::Open => write!(f, "Open"),
            Self::HalfOpen => write!(f, "HalfOpen"),
        }
    }
}

// ─── Single Breaker ──────────────────────────────────────────────────────────

/// Per-backend circuit breaker.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    state: CircuitState,
    /// Consecutive failure count (reset on success).
    consecutive_failures: u32,
    /// Consecutive successes in half-open state.
    half_open_successes: u32,
    /// When the breaker last transitioned to Open.
    opened_at: Option<DateTime<Utc>>,
    /// When the last state transition occurred.
    last_transition: DateTime<Utc>,
    /// Total lifetime failure count (never reset — useful for metrics).
    total_failures: u64,
    /// Total lifetime success count.
    total_successes: u64,
}

/// Snapshot for external inspection / serialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerSnapshot {
    pub state: CircuitState,
    pub consecutive_failures: u32,
    pub half_open_successes: u32,
    pub opened_at: Option<DateTime<Utc>>,
    pub last_transition: DateTime<Utc>,
    pub total_failures: u64,
    pub total_successes: u64,
}

impl CircuitBreaker {
    /// Create a new breaker in the Closed state.
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            state: CircuitState::Closed,
            consecutive_failures: 0,
            half_open_successes: 0,
            opened_at: None,
            last_transition: Utc::now(),
            total_failures: 0,
            total_successes: 0,
        }
    }

    /// Current state (after checking timeouts).
    pub fn state(&self) -> CircuitState {
        self.state
    }

    /// Check whether a request should be allowed through.
    ///
    /// - **Closed** → always allowed
    /// - **Open** → allowed only if recovery timeout has elapsed (transitions to HalfOpen)
    /// - **HalfOpen** → allowed (probing)
    pub fn is_allowed(&mut self) -> bool {
        match self.state {
            CircuitState::Closed => true,
            CircuitState::Open => {
                if self.recovery_timeout_elapsed() {
                    self.transition(CircuitState::HalfOpen);
                    true
                } else {
                    false
                }
            }
            CircuitState::HalfOpen => true,
        }
    }

    /// Record a successful request.
    pub fn record_success(&mut self) {
        self.total_successes += 1;
        match self.state {
            CircuitState::HalfOpen => {
                self.half_open_successes += 1;
                if self.half_open_successes >= self.config.success_threshold_for_close {
                    self.transition(CircuitState::Closed);
                    self.consecutive_failures = 0;
                    self.half_open_successes = 0;
                }
            }
            CircuitState::Closed => {
                // Reset failure counter on any success.
                self.consecutive_failures = 0;
            }
            CircuitState::Open => {
                // Shouldn't happen (requests blocked), but be safe.
                debug!("success recorded while breaker is open — ignoring");
            }
        }
    }

    /// Record a failed request.
    pub fn record_failure(&mut self) {
        self.total_failures += 1;
        self.consecutive_failures += 1;

        match self.state {
            CircuitState::Closed => {
                if self.consecutive_failures >= self.config.failure_threshold {
                    self.transition(CircuitState::Open);
                    self.opened_at = Some(Utc::now());
                }
            }
            CircuitState::HalfOpen => {
                // Any failure in half-open goes straight back to open.
                self.transition(CircuitState::Open);
                self.opened_at = Some(Utc::now());
                self.half_open_successes = 0;
            }
            CircuitState::Open => {
                // Already open — just count.
            }
        }
    }

    /// Force-reset to Closed (manual override).
    pub fn reset(&mut self) {
        self.transition(CircuitState::Closed);
        self.consecutive_failures = 0;
        self.half_open_successes = 0;
        self.opened_at = None;
    }

    /// Produce a serializable snapshot.
    pub fn snapshot(&self) -> CircuitBreakerSnapshot {
        CircuitBreakerSnapshot {
            state: self.state,
            consecutive_failures: self.consecutive_failures,
            half_open_successes: self.half_open_successes,
            opened_at: self.opened_at,
            last_transition: self.last_transition,
            total_failures: self.total_failures,
            total_successes: self.total_successes,
        }
    }

    // ── Internals ────────────────────────────────────────────────────────

    fn recovery_timeout_elapsed(&self) -> bool {
        match self.opened_at {
            Some(opened) => {
                let elapsed = Utc::now().signed_duration_since(opened);
                elapsed
                    >= chrono::Duration::from_std(self.config.recovery_timeout)
                        .unwrap_or(chrono::Duration::seconds(60))
            }
            None => true,
        }
    }

    fn transition(&mut self, to: CircuitState) {
        let from = self.state;
        if from != to {
            info!(from = %from, to = %to, "circuit breaker state transition");
            self.state = to;
            self.last_transition = Utc::now();
        }
    }
}

// ─── Registry ────────────────────────────────────────────────────────────────

/// Thread-safe registry of per-backend circuit breakers.
///
/// Automatically creates breakers on first access using the default config.
#[derive(Clone)]
pub struct CircuitBreakerRegistry {
    default_config: CircuitBreakerConfig,
    breakers: Arc<RwLock<HashMap<BackendId, CircuitBreaker>>>,
}

impl CircuitBreakerRegistry {
    /// Create a registry with the given default config.
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            default_config: config,
            breakers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Check if a request to `backend` should proceed.
    pub async fn is_allowed(&self, backend: &BackendId) -> bool {
        let mut map = self.breakers.write().await;
        let breaker = map
            .entry(backend.clone())
            .or_insert_with(|| CircuitBreaker::new(self.default_config.clone()));
        breaker.is_allowed()
    }

    /// Record a success for `backend`.
    pub async fn record_success(&self, backend: &BackendId) {
        let mut map = self.breakers.write().await;
        if let Some(breaker) = map.get_mut(backend) {
            breaker.record_success();
        }
    }

    /// Record a failure for `backend`.
    pub async fn record_failure(&self, backend: &BackendId) {
        let mut map = self.breakers.write().await;
        let breaker = map
            .entry(backend.clone())
            .or_insert_with(|| CircuitBreaker::new(self.default_config.clone()));
        breaker.record_failure();
    }

    /// Force-reset a specific backend breaker.
    pub async fn reset(&self, backend: &BackendId) {
        let mut map = self.breakers.write().await;
        if let Some(breaker) = map.get_mut(backend) {
            info!(backend = %backend, "manually resetting circuit breaker");
            breaker.reset();
        }
    }

    /// Snapshot all breakers for dashboards / API.
    pub async fn snapshot_all(&self) -> HashMap<BackendId, CircuitBreakerSnapshot> {
        let map = self.breakers.read().await;
        map.iter()
            .map(|(id, cb)| (id.clone(), cb.snapshot()))
            .collect()
    }

    /// Get snapshot for a single backend.
    pub async fn snapshot(&self, backend: &BackendId) -> Option<CircuitBreakerSnapshot> {
        let map = self.breakers.read().await;
        map.get(backend).map(|cb| cb.snapshot())
    }

    /// Remove a backend entirely (e.g. node decommissioned).
    pub async fn remove(&self, backend: &BackendId) {
        let mut map = self.breakers.write().await;
        map.remove(backend);
    }

    /// List all backends currently tracked.
    pub async fn tracked_backends(&self) -> Vec<BackendId> {
        let map = self.breakers.read().await;
        map.keys().cloned().collect()
    }

    /// List backends currently in Open state.
    pub async fn open_backends(&self) -> Vec<BackendId> {
        let map = self.breakers.read().await;
        map.iter()
            .filter(|(_, cb)| cb.state() == CircuitState::Open)
            .map(|(id, _)| id.clone())
            .collect()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fast_config() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            failure_threshold: 3,
            recovery_timeout: Duration::from_millis(50),
            success_threshold_for_close: 2,
        }
    }

    #[test]
    fn test_starts_closed() {
        let cb = CircuitBreaker::new(fast_config());
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn test_opens_after_threshold() {
        let mut cb = CircuitBreaker::new(fast_config());
        assert!(cb.is_allowed());

        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Closed);

        cb.record_failure(); // 3rd failure → open
        assert_eq!(cb.state(), CircuitState::Open);
        assert!(!cb.is_allowed());
    }

    #[test]
    fn test_success_resets_failure_count() {
        let mut cb = CircuitBreaker::new(fast_config());
        cb.record_failure();
        cb.record_failure();
        cb.record_success(); // reset consecutive failures
        cb.record_failure();
        cb.record_failure();
        // Still only 2 consecutive → stays closed
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[tokio::test]
    async fn test_transitions_to_half_open_after_timeout() {
        let mut cb = CircuitBreaker::new(fast_config());

        // Trip the breaker
        for _ in 0..3 {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Open);
        assert!(!cb.is_allowed());

        // Wait for recovery timeout
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Should transition to HalfOpen on next is_allowed()
        assert!(cb.is_allowed());
        assert_eq!(cb.state(), CircuitState::HalfOpen);
    }

    #[tokio::test]
    async fn test_half_open_closes_on_enough_successes() {
        let mut cb = CircuitBreaker::new(fast_config());

        // Trip it
        for _ in 0..3 {
            cb.record_failure();
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
        cb.is_allowed(); // → HalfOpen

        cb.record_success();
        assert_eq!(cb.state(), CircuitState::HalfOpen);
        cb.record_success(); // 2nd success → close
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[tokio::test]
    async fn test_half_open_reopens_on_failure() {
        let mut cb = CircuitBreaker::new(fast_config());

        for _ in 0..3 {
            cb.record_failure();
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
        cb.is_allowed(); // → HalfOpen

        cb.record_failure(); // any failure → back to Open
        assert_eq!(cb.state(), CircuitState::Open);
    }

    #[test]
    fn test_manual_reset() {
        let mut cb = CircuitBreaker::new(fast_config());
        for _ in 0..3 {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Open);

        cb.reset();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(cb.is_allowed());
    }

    #[test]
    fn test_snapshot_captures_state() {
        let mut cb = CircuitBreaker::new(fast_config());
        cb.record_failure();
        cb.record_success();
        cb.record_failure();
        cb.record_failure();
        cb.record_failure(); // → open

        let snap = cb.snapshot();
        assert_eq!(snap.state, CircuitState::Open);
        assert_eq!(snap.total_failures, 4);
        assert_eq!(snap.total_successes, 1);
        assert!(snap.opened_at.is_some());
    }

    #[tokio::test]
    async fn test_registry_creates_breakers_on_demand() {
        let reg = CircuitBreakerRegistry::new(fast_config());
        let backend: BackendId = "taylor:51800".into();

        // First access creates the breaker
        assert!(reg.is_allowed(&backend).await);

        let tracked = reg.tracked_backends().await;
        assert_eq!(tracked.len(), 1);
        assert!(tracked.contains(&backend));
    }

    #[tokio::test]
    async fn test_registry_tracks_failures() {
        let reg = CircuitBreakerRegistry::new(fast_config());
        let backend: BackendId = "james:51801".into();

        // Need to call is_allowed first to create the breaker
        reg.is_allowed(&backend).await;

        for _ in 0..3 {
            reg.record_failure(&backend).await;
        }

        let snap = reg.snapshot(&backend).await.unwrap();
        assert_eq!(snap.state, CircuitState::Open);
        assert!(!reg.is_allowed(&backend).await);
    }

    #[tokio::test]
    async fn test_registry_open_backends() {
        let reg = CircuitBreakerRegistry::new(fast_config());
        let b1: BackendId = "taylor:51800".into();
        let b2: BackendId = "james:51801".into();

        reg.is_allowed(&b1).await;
        reg.is_allowed(&b2).await;

        // Trip b1 only
        for _ in 0..3 {
            reg.record_failure(&b1).await;
        }

        let open = reg.open_backends().await;
        assert_eq!(open.len(), 1);
        assert!(open.contains(&b1));
    }

    #[tokio::test]
    async fn test_registry_remove() {
        let reg = CircuitBreakerRegistry::new(fast_config());
        let backend: BackendId = "taylor:51800".into();
        reg.is_allowed(&backend).await;

        assert_eq!(reg.tracked_backends().await.len(), 1);
        reg.remove(&backend).await;
        assert_eq!(reg.tracked_backends().await.len(), 0);
    }

    #[tokio::test]
    async fn test_registry_reset() {
        let reg = CircuitBreakerRegistry::new(fast_config());
        let backend: BackendId = "taylor:51800".into();
        reg.is_allowed(&backend).await;

        for _ in 0..3 {
            reg.record_failure(&backend).await;
        }
        assert!(!reg.is_allowed(&backend).await);

        reg.reset(&backend).await;
        assert!(reg.is_allowed(&backend).await);
    }
}
