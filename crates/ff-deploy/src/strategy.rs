//! Rollout strategy definitions and validation.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Deployment rollout strategy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RolloutStrategy {
    /// Start with a small canary, then gradually increase traffic.
    Canary(CanaryStrategy),
    /// Roll out in explicit staged percentages.
    Staged(StagedStrategy),
    /// Deploy to all targets in a single wave.
    Full(FullStrategy),
}

impl RolloutStrategy {
    /// Human-friendly strategy name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Canary(_) => "canary",
            Self::Staged(_) => "staged",
            Self::Full(_) => "full",
        }
    }

    /// Validate strategy configuration.
    pub fn validate(&self) -> Result<(), StrategyError> {
        match self {
            Self::Canary(cfg) => cfg.validate(),
            Self::Staged(cfg) => cfg.validate(),
            Self::Full(cfg) => cfg.validate(),
        }
    }

    /// Return cumulative rollout percentages in execution order.
    pub fn progression(&self) -> Vec<u8> {
        match self {
            Self::Canary(cfg) => cfg.progression(),
            Self::Staged(cfg) => cfg.progression.clone(),
            Self::Full(_) => vec![100],
        }
    }

    /// Pause duration in seconds between steps.
    pub fn pause_secs(&self) -> u64 {
        match self {
            Self::Canary(cfg) => cfg.pause_secs,
            Self::Staged(cfg) => cfg.pause_secs,
            Self::Full(cfg) => cfg.pause_secs,
        }
    }

    /// Max tolerated error rate (0.0 to 1.0).
    pub fn max_error_rate(&self) -> f64 {
        match self {
            Self::Canary(cfg) => cfg.max_error_rate,
            Self::Staged(cfg) => cfg.max_error_rate,
            Self::Full(cfg) => cfg.max_error_rate,
        }
    }
}

/// Canary rollout configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanaryStrategy {
    /// Initial canary percentage (1..=99).
    pub initial_percent: u8,
    /// Increment for each subsequent canary wave (1..=99).
    pub step_percent: u8,
    /// Wait time after each intermediate wave.
    pub pause_secs: u64,
    /// Maximum tolerated error rate (0.0..=1.0).
    pub max_error_rate: f64,
}

impl Default for CanaryStrategy {
    fn default() -> Self {
        Self {
            initial_percent: 10,
            step_percent: 20,
            pause_secs: 60,
            max_error_rate: 0.02,
        }
    }
}

impl CanaryStrategy {
    /// Validate canary strategy values.
    pub fn validate(&self) -> Result<(), StrategyError> {
        if self.initial_percent == 0 || self.initial_percent >= 100 {
            return Err(StrategyError::InvalidPercent(
                "canary.initial_percent",
                self.initial_percent,
            ));
        }
        if self.step_percent == 0 || self.step_percent >= 100 {
            return Err(StrategyError::InvalidPercent(
                "canary.step_percent",
                self.step_percent,
            ));
        }
        if !(0.0..=1.0).contains(&self.max_error_rate) {
            return Err(StrategyError::InvalidThreshold(
                "canary.max_error_rate",
                self.max_error_rate,
            ));
        }
        Ok(())
    }

    /// Generate cumulative progression ending in 100%.
    pub fn progression(&self) -> Vec<u8> {
        let mut pct = self.initial_percent;
        let mut progression = vec![pct];

        while pct < 100 {
            let next = pct.saturating_add(self.step_percent);
            pct = next.min(100);
            if progression.last().copied() != Some(pct) {
                progression.push(pct);
            } else {
                break;
            }
        }

        if progression.last().copied() != Some(100) {
            progression.push(100);
        }

        progression
    }
}

/// Staged rollout configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StagedStrategy {
    /// Cumulative percentages (must be strictly increasing, ending in 100).
    pub progression: Vec<u8>,
    /// Wait time after each intermediate stage.
    pub pause_secs: u64,
    /// Maximum tolerated error rate (0.0..=1.0).
    pub max_error_rate: f64,
}

impl Default for StagedStrategy {
    fn default() -> Self {
        Self {
            progression: vec![25, 50, 100],
            pause_secs: 120,
            max_error_rate: 0.03,
        }
    }
}

impl StagedStrategy {
    /// Validate staged progression.
    pub fn validate(&self) -> Result<(), StrategyError> {
        if self.progression.is_empty() {
            return Err(StrategyError::EmptyProgression);
        }

        let mut prev = 0u8;
        for value in &self.progression {
            if *value == 0 || *value > 100 {
                return Err(StrategyError::InvalidPercent("staged.progression", *value));
            }
            if *value <= prev {
                return Err(StrategyError::NonMonotonicProgression);
            }
            prev = *value;
        }

        if self.progression.last().copied() != Some(100) {
            return Err(StrategyError::MustEndAtOneHundred);
        }

        if !(0.0..=1.0).contains(&self.max_error_rate) {
            return Err(StrategyError::InvalidThreshold(
                "staged.max_error_rate",
                self.max_error_rate,
            ));
        }

        Ok(())
    }
}

/// Full rollout configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FullStrategy {
    /// Optional stabilization pause after the full rollout step.
    pub pause_secs: u64,
    /// Maximum tolerated error rate (0.0..=1.0).
    pub max_error_rate: f64,
}

impl Default for FullStrategy {
    fn default() -> Self {
        Self {
            pause_secs: 0,
            max_error_rate: 0.05,
        }
    }
}

impl FullStrategy {
    /// Validate full strategy values.
    pub fn validate(&self) -> Result<(), StrategyError> {
        if !(0.0..=1.0).contains(&self.max_error_rate) {
            return Err(StrategyError::InvalidThreshold(
                "full.max_error_rate",
                self.max_error_rate,
            ));
        }
        Ok(())
    }
}

/// Strategy validation error.
#[derive(Debug, Error)]
pub enum StrategyError {
    /// A percentage field contains an invalid value.
    #[error("invalid percentage for {0}: {1} (must be 1..=100 and non-zero where required)")]
    InvalidPercent(&'static str, u8),
    /// A threshold field is out of [0.0, 1.0].
    #[error("invalid threshold for {0}: {1} (must be within 0.0..=1.0)")]
    InvalidThreshold(&'static str, f64),
    /// Staged progression cannot be empty.
    #[error("staged progression cannot be empty")]
    EmptyProgression,
    /// Staged progression must be strictly increasing.
    #[error("staged progression must be strictly increasing")]
    NonMonotonicProgression,
    /// Staged progression must end at 100.
    #[error("staged progression must end at 100")]
    MustEndAtOneHundred,
}
