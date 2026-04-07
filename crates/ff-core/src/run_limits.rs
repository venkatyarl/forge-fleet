use serde::{Deserialize, Serialize};

/// Runtime limits for autonomous execution.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RunLimits {
    pub max_retries: u32,
    pub max_duration_secs: u64,
    pub confidence_threshold: f32,
    pub escalation_threshold: f32,
}

impl Default for RunLimits {
    fn default() -> Self {
        Self {
            max_retries: 3,
            max_duration_secs: 300,
            confidence_threshold: 0.65,
            escalation_threshold: 0.45,
        }
    }
}

impl RunLimits {
    pub fn should_escalate(&self, retry_count: u32, elapsed_secs: u64, confidence: f32) -> bool {
        if retry_count >= self.max_retries {
            return true;
        }

        if elapsed_secs >= self.max_duration_secs {
            return true;
        }

        if confidence <= self.escalation_threshold {
            return true;
        }

        retry_count > 0 && confidence < self.confidence_threshold
    }
}

/// Default run-limit escalation evaluator.
pub fn should_escalate(retry_count: u32, elapsed_secs: u64, confidence: f32) -> bool {
    RunLimits::default().should_escalate(retry_count, elapsed_secs, confidence)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escalates_when_retry_limit_reached() {
        let limits = RunLimits::default();
        assert!(limits.should_escalate(limits.max_retries, 1, 0.95));
    }

    #[test]
    fn escalates_when_duration_limit_reached() {
        let limits = RunLimits::default();
        assert!(limits.should_escalate(0, limits.max_duration_secs, 0.95));
    }

    #[test]
    fn escalates_when_confidence_drops_below_escalation_threshold() {
        let limits = RunLimits::default();
        assert!(limits.should_escalate(0, 1, limits.escalation_threshold - 0.01));
    }

    #[test]
    fn does_not_escalate_for_first_try_with_high_confidence() {
        let limits = RunLimits::default();
        assert!(!limits.should_escalate(0, 30, 0.90));
    }

    #[test]
    fn free_function_uses_default_limits() {
        assert!(should_escalate(10, 1, 0.9));
    }
}
