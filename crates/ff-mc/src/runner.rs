//! Sub-agent task run limits.
//!
//! Mission Control dispatches work items to sub-agents, which execute them in
//! iterative passes. This module enforces an iteration cap on those runs —
//! mirroring `ff_core::run_limits` — so a stuck or looping sub-agent task
//! stops instead of spinning forever.

use serde::{Deserialize, Serialize};

/// Default maximum iterations for a sub-agent task run.
pub const DEFAULT_MAX_ITERATIONS: u32 = 50;

/// Iteration limits for sub-agent task runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IterationLimits {
    pub max_iterations: u32,
}

impl Default for IterationLimits {
    fn default() -> Self {
        Self {
            max_iterations: DEFAULT_MAX_ITERATIONS,
        }
    }
}

impl IterationLimits {
    pub fn new(max_iterations: u32) -> Self {
        Self { max_iterations }
    }

    /// Whether a run that has completed `iterations` iterations has hit the cap.
    pub fn is_reached(&self, iterations: u32) -> bool {
        iterations >= self.max_iterations
    }
}

/// Tracks iterations of one sub-agent task run against an [`IterationLimits`].
#[derive(Debug, Clone, Copy)]
pub struct IterationGuard {
    limits: IterationLimits,
    iterations: u32,
}

impl IterationGuard {
    pub fn new(limits: IterationLimits) -> Self {
        Self {
            limits,
            iterations: 0,
        }
    }

    /// Record one iteration. Returns `false` (and logs a warning) once the
    /// cap is reached — the caller should stop the run.
    pub fn advance(&mut self) -> bool {
        self.iterations = self.iterations.saturating_add(1);
        if self.iterations > self.limits.max_iterations {
            tracing::warn!(
                max_iterations = self.limits.max_iterations,
                "sub-agent task iteration cap reached; stopping run"
            );
            return false;
        }
        true
    }

    /// Iterations recorded so far.
    pub fn iterations(&self) -> u32 {
        self.iterations
    }
}

impl Default for IterationGuard {
    fn default() -> Self {
        Self::new(IterationLimits::default())
    }
}

/// Default iteration-cap evaluator.
pub fn iteration_cap_reached(iterations: u32) -> bool {
    IterationLimits::default().is_reached(iterations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_reached_at_limit() {
        let limits = IterationLimits::default();
        assert!(limits.is_reached(limits.max_iterations));
    }

    #[test]
    fn cap_not_reached_below_limit() {
        let limits = IterationLimits::default();
        assert!(!limits.is_reached(limits.max_iterations - 1));
    }

    #[test]
    fn guard_allows_up_to_the_cap_then_stops() {
        let mut guard = IterationGuard::new(IterationLimits::new(3));
        assert!(guard.advance());
        assert!(guard.advance());
        assert!(guard.advance());
        assert!(!guard.advance());
        assert_eq!(guard.iterations(), 4);
    }

    #[test]
    fn zero_limit_stops_immediately() {
        let mut guard = IterationGuard::new(IterationLimits::new(0));
        assert!(!guard.advance());
    }

    #[test]
    fn free_function_uses_default_limits() {
        assert!(!iteration_cap_reached(0));
        assert!(iteration_cap_reached(DEFAULT_MAX_ITERATIONS));
    }
}
