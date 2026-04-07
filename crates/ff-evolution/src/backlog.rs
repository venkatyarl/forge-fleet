//! Durable backlog generation from recurring failures.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::analyzer::{AnalysisReport, RootCause, RootCauseCategory};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BacklogPriority {
    P0,
    P1,
    P2,
    P3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BacklogStatus {
    Draft,
    Open,
    InProgress,
    Resolved,
}

/// A durable engineering work item created from recurring root causes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacklogItem {
    pub id: Uuid,
    pub fingerprint: String,
    pub title: String,
    pub cause_category: RootCauseCategory,
    pub occurrences: u32,
    pub priority: BacklogPriority,
    pub status: BacklogStatus,
    pub durable: bool,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub recommended_next_step: String,
}

/// Converts repeated issues into backlog items once recurrence threshold is met.
#[derive(Clone)]
pub struct BacklogService {
    recurrence_threshold: u32,
    items: Arc<DashMap<String, BacklogItem>>,
}

impl std::fmt::Debug for BacklogService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BacklogService")
            .field("recurrence_threshold", &self.recurrence_threshold)
            .field("items", &self.items.len())
            .finish()
    }
}

impl Default for BacklogService {
    fn default() -> Self {
        Self::new(3)
    }
}

impl BacklogService {
    pub fn new(recurrence_threshold: u32) -> Self {
        Self {
            recurrence_threshold: recurrence_threshold.max(1),
            items: Arc::new(DashMap::new()),
        }
    }

    /// Ingest an analysis report and update recurring issue counters.
    ///
    /// Returns items promoted to durable/open state by this ingestion.
    pub fn ingest_report(&self, report: &AnalysisReport) -> Vec<BacklogItem> {
        let mut promoted = Vec::new();
        for cause in &report.causes {
            if let Some(item) = self.upsert_cause(cause) {
                promoted.push(item);
            }
        }
        promoted
    }

    pub fn get(&self, fingerprint: &str) -> Option<BacklogItem> {
        self.items.get(fingerprint).map(|entry| entry.clone())
    }

    pub fn durable_items(&self) -> Vec<BacklogItem> {
        self.items
            .iter()
            .filter(|entry| entry.durable)
            .map(|entry| entry.clone())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn upsert_cause(&self, cause: &RootCause) -> Option<BacklogItem> {
        let now = Utc::now();
        let mut promoted: Option<BacklogItem> = None;

        self.items
            .entry(cause.fingerprint.clone())
            .and_modify(|item| {
                item.occurrences += 1;
                item.last_seen = now;
                item.priority = classify_priority(item.cause_category, item.occurrences);

                if !item.durable && item.occurrences >= self.recurrence_threshold {
                    item.durable = true;
                    item.status = BacklogStatus::Open;
                    promoted = Some(item.clone());
                }
            })
            .or_insert_with(|| {
                let occurrences = 1;
                BacklogItem {
                    id: Uuid::new_v4(),
                    fingerprint: cause.fingerprint.clone(),
                    title: format!("Recurring {:?} issue", cause.category),
                    cause_category: cause.category,
                    occurrences,
                    priority: classify_priority(cause.category, occurrences),
                    status: BacklogStatus::Draft,
                    durable: occurrences >= self.recurrence_threshold,
                    first_seen: now,
                    last_seen: now,
                    recommended_next_step: default_next_step(cause.category),
                }
            });

        // Handle threshold=1 case and first insert promotion.
        if promoted.is_none()
            && let Some(item) = self.items.get(&cause.fingerprint)
            && item.durable
            && item.occurrences == 1
        {
            promoted = Some(item.clone());
        }

        promoted
    }
}

fn classify_priority(category: RootCauseCategory, occurrences: u32) -> BacklogPriority {
    match (category, occurrences) {
        (RootCauseCategory::ResourceExhaustion, n) if n >= 3 => BacklogPriority::P0,
        (RootCauseCategory::ApiContractMismatch, n) if n >= 3 => BacklogPriority::P1,
        (RootCauseCategory::CompilationError, n) if n >= 4 => BacklogPriority::P1,
        (_, n) if n >= 5 => BacklogPriority::P1,
        (_, n) if n >= 3 => BacklogPriority::P2,
        _ => BacklogPriority::P3,
    }
}

fn default_next_step(category: RootCauseCategory) -> String {
    match category {
        RootCauseCategory::CompilationError => {
            "Add compile-time guardrails and stronger lint gates".to_string()
        }
        RootCauseCategory::DependencyResolution => {
            "Introduce dependency update policy and lockfile review automation".to_string()
        }
        RootCauseCategory::MissingConfiguration => {
            "Create validated config schema and startup validation checks".to_string()
        }
        RootCauseCategory::ApiContractMismatch => {
            "Add contract tests for producer/consumer compatibility".to_string()
        }
        RootCauseCategory::NetworkInstability => {
            "Implement resilience policy (timeouts, retries, circuit breakers)".to_string()
        }
        RootCauseCategory::ResourceExhaustion => {
            "Define autoscaling / resource budget thresholds".to_string()
        }
        RootCauseCategory::TestRegression => {
            "Improve regression test coverage around changed surfaces".to_string()
        }
        RootCauseCategory::FlakyBehavior => {
            "Stabilize flaky tests and isolate timing-dependent code paths".to_string()
        }
        RootCauseCategory::ToolingFailure => {
            "Harden CI runner/toolchain reproducibility".to_string()
        }
        RootCauseCategory::Unknown => "Collect richer telemetry and triage manually".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::{FailureCategory, RootCause};

    fn cause(fingerprint: &str, category: RootCauseCategory) -> RootCause {
        RootCause {
            id: Uuid::new_v4(),
            category,
            failure_category: FailureCategory::Build,
            summary: "summary".to_string(),
            evidence: vec!["error:".to_string()],
            confidence: 0.8,
            fingerprint: fingerprint.to_string(),
            created_at: Utc::now(),
        }
    }

    fn report_from(cause: RootCause) -> AnalysisReport {
        AnalysisReport {
            id: Uuid::new_v4(),
            observation_id: Uuid::new_v4(),
            failure_category: FailureCategory::Build,
            primary: Some(cause.clone()),
            causes: vec![cause],
            analyzed_at: Utc::now(),
            classifier_version: "test".to_string(),
        }
    }

    #[test]
    fn promotes_recurring_issue_to_durable_backlog() {
        let backlog = BacklogService::new(2);
        let fp = "compile:missing-type";

        let promoted1 =
            backlog.ingest_report(&report_from(cause(fp, RootCauseCategory::CompilationError)));
        assert!(promoted1.is_empty());

        let promoted2 =
            backlog.ingest_report(&report_from(cause(fp, RootCauseCategory::CompilationError)));
        assert_eq!(promoted2.len(), 1);
        assert!(promoted2[0].durable);
        assert_eq!(promoted2[0].status, BacklogStatus::Open);
    }
}
