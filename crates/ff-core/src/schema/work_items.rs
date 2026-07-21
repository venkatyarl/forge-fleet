//! Shared work-item schema values.

use serde::{Deserialize, Serialize};

/// Pick-score weight for urgent and important work.
pub const Q1: f64 = 1000.0;
/// Pick-score weight for important but not urgent work.
pub const Q2: f64 = 750.0;
/// Pick-score weight for urgent but not important work.
pub const Q3: f64 = 500.0;
/// Pick-score weight for neither urgent nor important work.
pub const Q4: f64 = 250.0;

/// Eisenhower-style quadrant used for coarse scheduling priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quadrant {
    /// Urgent + important: do first.
    Q1,
    /// Important but not urgent: plan.
    Q2,
    /// Urgent but not important: delegate if possible.
    Q3,
    /// Neither urgent nor important: defer.
    Q4,
}

impl Quadrant {
    /// Stable numeric value used when computing a work item's pick score.
    pub const fn value(self) -> f64 {
        match self {
            Self::Q1 => Q1,
            Self::Q2 => Q2,
            Self::Q3 => Q3,
            Self::Q4 => Q4,
        }
    }

    /// Base score contribution; higher is picked sooner.
    pub const fn base_score(self) -> f64 {
        self.value()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quadrant_values_use_shared_constants() {
        assert_eq!(Quadrant::Q1.value(), Q1);
        assert_eq!(Quadrant::Q2.value(), Q2);
        assert_eq!(Quadrant::Q3.value(), Q3);
        assert_eq!(Quadrant::Q4.value(), Q4);
    }
}
