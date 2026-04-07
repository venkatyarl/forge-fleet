//! Learning and suppression subsystem.
//!
//! Persists outcomes of attempted repair strategies and suppresses repeated
//! failed approaches for the same root-cause fingerprint.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::repair::RepairStrategy;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningOutcome {
    Success,
    Failure,
    RolledBack,
    Suppressed,
}

/// A durable lesson from a specific strategy attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningRecord {
    pub id: Uuid,
    pub cause_fingerprint: String,
    pub strategy: RepairStrategy,
    pub outcome: LearningOutcome,
    pub confidence_delta: f32,
    pub notes: String,
    pub recorded_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SuppressionPolicy {
    /// Number of failed attempts before strategy suppression.
    pub max_failures: u32,
    /// Suppression cooldown period in minutes.
    pub cooldown_minutes: i64,
    /// Lookback window for counting failures.
    pub failure_lookback_minutes: i64,
}

impl Default for SuppressionPolicy {
    fn default() -> Self {
        Self {
            max_failures: 3,
            cooldown_minutes: 180,
            failure_lookback_minutes: 1_440,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StrategyKey {
    cause_fingerprint: String,
    strategy: RepairStrategy,
}

/// In-memory learning store with suppression logic.
#[derive(Clone)]
pub struct LearningStore {
    policy: SuppressionPolicy,
    records: Arc<DashMap<StrategyKey, Vec<LearningRecord>>>,
    suppressed_until: Arc<DashMap<StrategyKey, DateTime<Utc>>>,
}

impl std::fmt::Debug for LearningStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LearningStore")
            .field("policy", &self.policy)
            .field("keys", &self.records.len())
            .field("suppressed", &self.suppressed_until.len())
            .finish()
    }
}

impl Default for LearningStore {
    fn default() -> Self {
        Self::new(SuppressionPolicy::default())
    }
}

impl LearningStore {
    pub fn new(policy: SuppressionPolicy) -> Self {
        Self {
            policy,
            records: Arc::new(DashMap::new()),
            suppressed_until: Arc::new(DashMap::new()),
        }
    }

    pub fn policy(&self) -> SuppressionPolicy {
        self.policy
    }

    pub fn is_suppressed(
        &self,
        cause_fingerprint: &str,
        strategy: RepairStrategy,
        at: DateTime<Utc>,
    ) -> bool {
        let key = Self::key(cause_fingerprint, strategy);
        self.suppressed_until
            .get(&key)
            .map(|until| *until > at)
            .unwrap_or(false)
    }

    pub fn suppression_until(
        &self,
        cause_fingerprint: &str,
        strategy: RepairStrategy,
    ) -> Option<DateTime<Utc>> {
        let key = Self::key(cause_fingerprint, strategy);
        self.suppressed_until.get(&key).map(|v| *v)
    }

    pub fn record(
        &self,
        cause_fingerprint: impl Into<String>,
        strategy: RepairStrategy,
        outcome: LearningOutcome,
        notes: impl Into<String>,
        confidence_delta: f32,
    ) -> LearningRecord {
        self.record_at(
            cause_fingerprint,
            strategy,
            outcome,
            notes,
            confidence_delta,
            Utc::now(),
        )
    }

    pub fn record_at(
        &self,
        cause_fingerprint: impl Into<String>,
        strategy: RepairStrategy,
        outcome: LearningOutcome,
        notes: impl Into<String>,
        confidence_delta: f32,
        timestamp: DateTime<Utc>,
    ) -> LearningRecord {
        let fingerprint = cause_fingerprint.into();
        let key = Self::key(&fingerprint, strategy);

        let record = LearningRecord {
            id: Uuid::new_v4(),
            cause_fingerprint: fingerprint,
            strategy,
            outcome,
            confidence_delta,
            notes: notes.into(),
            recorded_at: timestamp,
        };

        self.records
            .entry(key.clone())
            .and_modify(|records| records.push(record.clone()))
            .or_insert_with(|| vec![record.clone()]);

        self.recompute_suppression(&key, timestamp);
        record
    }

    pub fn failure_count_recent(
        &self,
        cause_fingerprint: &str,
        strategy: RepairStrategy,
        at: DateTime<Utc>,
    ) -> usize {
        let key = Self::key(cause_fingerprint, strategy);
        self.failure_count_recent_for_key(&key, at)
    }

    pub fn records_for(
        &self,
        cause_fingerprint: &str,
        strategy: RepairStrategy,
    ) -> Vec<LearningRecord> {
        let key = Self::key(cause_fingerprint, strategy);
        self.records
            .get(&key)
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    pub fn should_suppress(
        &self,
        cause_fingerprint: &str,
        strategy: RepairStrategy,
        at: DateTime<Utc>,
    ) -> bool {
        self.is_suppressed(cause_fingerprint, strategy, at)
            || self.failure_count_recent(cause_fingerprint, strategy, at)
                >= self.policy.max_failures as usize
    }

    fn recompute_suppression(&self, key: &StrategyKey, at: DateTime<Utc>) {
        let failures = self.failure_count_recent_for_key(key, at);
        if failures >= self.policy.max_failures as usize {
            let until = at + Duration::minutes(self.policy.cooldown_minutes);
            self.suppressed_until.insert(key.clone(), until);
        }
    }

    fn failure_count_recent_for_key(&self, key: &StrategyKey, at: DateTime<Utc>) -> usize {
        let lookback_start = at - Duration::minutes(self.policy.failure_lookback_minutes);

        self.records
            .get(key)
            .map(|records| {
                records
                    .iter()
                    .filter(|record| {
                        matches!(
                            record.outcome,
                            LearningOutcome::Failure | LearningOutcome::RolledBack
                        ) && record.recorded_at >= lookback_start
                            && record.recorded_at <= at
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    fn key(cause_fingerprint: &str, strategy: RepairStrategy) -> StrategyKey {
        StrategyKey {
            cause_fingerprint: cause_fingerprint.to_string(),
            strategy,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suppresses_repeated_failed_strategy() {
        let policy = SuppressionPolicy {
            max_failures: 2,
            cooldown_minutes: 60,
            failure_lookback_minutes: 240,
        };
        let store = LearningStore::new(policy);

        let fingerprint = "root:compile:error";
        let strategy = RepairStrategy::FixCompilation;
        let t0 = Utc::now();

        store.record_at(
            fingerprint,
            strategy,
            LearningOutcome::Failure,
            "first attempt failed",
            -0.2,
            t0,
        );
        assert!(!store.is_suppressed(fingerprint, strategy, t0 + Duration::minutes(1)));

        store.record_at(
            fingerprint,
            strategy,
            LearningOutcome::Failure,
            "second attempt failed",
            -0.3,
            t0 + Duration::minutes(2),
        );

        assert!(store.is_suppressed(fingerprint, strategy, t0 + Duration::minutes(3)));

        // Cooldown expires.
        assert!(!store.is_suppressed(fingerprint, strategy, t0 + Duration::minutes(63)));
    }

    #[test]
    fn success_does_not_increase_failure_counter() {
        let store = LearningStore::default();
        let fingerprint = "root:test:regression";
        let strategy = RepairStrategy::StabilizeTest;
        let now = Utc::now();

        store.record_at(
            fingerprint,
            strategy,
            LearningOutcome::Success,
            "fixed",
            0.4,
            now,
        );

        assert_eq!(store.failure_count_recent(fingerprint, strategy, now), 0);
        assert!(!store.should_suppress(fingerprint, strategy, now));
    }
}
