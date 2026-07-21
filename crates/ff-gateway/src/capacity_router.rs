//! Shadow consumer for the `model_capacity` routing view.

use std::{collections::HashMap, sync::Mutex};

use chrono::{DateTime, Utc};
use sqlx::PgPool;

const FRESHNESS_SECONDS: i64 = 90;
const SATURATED_LOAD: i32 = 4;
const SWITCH_MARGIN: i32 = 2;
const SWITCH_CONFIRMATIONS: u8 = 2;

#[derive(Debug, Clone, sqlx::FromRow)]
struct CapacityRow {
    computer: String,
    catalog_id: String,
    queue_depth: Option<i32>,
    active_requests: Option<i32>,
    last_scraped_at: Option<DateTime<Utc>>,
    status: String,
}

impl CapacityRow {
    fn load(&self) -> i32 {
        self.queue_depth.unwrap_or(0) + self.active_requests.unwrap_or(0)
    }

    fn is_fresh(&self, now: DateTime<Utc>) -> bool {
        self.last_scraped_at.is_some_and(|scraped| {
            let age = now.signed_duration_since(scraped).num_seconds();
            (0..=FRESHNESS_SECONDS).contains(&age)
        })
    }

    fn has_headroom(&self, now: DateTime<Utc>) -> bool {
        self.is_fresh(now)
            && matches!(self.status.as_str(), "healthy" | "active" | "ready")
            && self.load() < SATURATED_LOAD
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapacityReason {
    Headroom,
    Unknown,
    AtCapacity,
}

/// Observational capacity decision. The gateway logs it but never applies it.
#[derive(Debug, Clone)]
pub struct CapacityDecision {
    pub recommended_computer: Option<String>,
    pub actual_computer: String,
    pub reason: CapacityReason,
}

impl CapacityDecision {
    pub fn agrees_with_actual(&self) -> bool {
        self.recommended_computer.as_deref() == Some(self.actual_computer.as_str())
    }
}

#[derive(Debug, Default)]
struct HysteresisState {
    selected: Option<String>,
    challenger: Option<String>,
    confirmations: u8,
}

/// Stateful shadow evaluator. State is process-local and affects only logs.
#[derive(Debug, Default)]
pub struct CapacityRouter {
    states: Mutex<HashMap<String, HysteresisState>>,
}

impl CapacityRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Evaluate the current `model_capacity` view without changing the real route.
    pub async fn evaluate_capacity(
        &self,
        pool: &PgPool,
        model_id: &str,
        actual_computer: &str,
    ) -> Result<CapacityDecision, sqlx::Error> {
        let rows = sqlx::query_as::<_, CapacityRow>(
            "SELECT computer, catalog_id, queue_depth, active_requests, \
                    last_scraped_at, status FROM model_capacity",
        )
        .fetch_all(pool)
        .await?;
        Ok(self.evaluate_rows(rows, model_id, actual_computer, Utc::now()))
    }

    fn evaluate_rows(
        &self,
        rows: Vec<CapacityRow>,
        model_id: &str,
        actual_computer: &str,
        now: DateTime<Utc>,
    ) -> CapacityDecision {
        let model = normalize_model_id(model_id);
        let matching: Vec<_> = rows
            .into_iter()
            .filter(|row| normalize_model_id(&row.catalog_id) == model)
            .collect();
        let any_fresh = matching.iter().any(|row| row.is_fresh(now));
        let mut candidates: Vec<_> = matching
            .iter()
            .filter(|row| row.has_headroom(now))
            .collect();
        candidates.sort_by_key(|row| row.load());

        let reason = if candidates.is_empty() {
            if any_fresh {
                CapacityReason::AtCapacity
            } else {
                CapacityReason::Unknown
            }
        } else {
            CapacityReason::Headroom
        };

        let recommended_computer = if candidates.is_empty() {
            None
        } else {
            let best = candidates[0];
            let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
            let state = states.entry(model).or_default();
            let incumbent = state
                .selected
                .as_ref()
                .and_then(|selected| candidates.iter().find(|row| &row.computer == selected));

            match incumbent {
                None => {
                    state.selected = Some(best.computer.clone());
                    state.challenger = None;
                    state.confirmations = 0;
                }
                Some(current)
                    if current.computer != best.computer
                        && best.load() + SWITCH_MARGIN <= current.load() =>
                {
                    if state.challenger.as_deref() == Some(best.computer.as_str()) {
                        state.confirmations = state.confirmations.saturating_add(1);
                    } else {
                        state.challenger = Some(best.computer.clone());
                        state.confirmations = 1;
                    }
                    if state.confirmations >= SWITCH_CONFIRMATIONS {
                        state.selected = Some(best.computer.clone());
                        state.challenger = None;
                        state.confirmations = 0;
                    }
                }
                _ => {
                    state.challenger = None;
                    state.confirmations = 0;
                }
            }
            state.selected.clone()
        };

        CapacityDecision {
            recommended_computer,
            actual_computer: actual_computer.to_string(),
            reason,
        }
    }

    /// Compare the shadow recommendation with the actual request outcome.
    pub fn record_outcome(&self, decision: &CapacityDecision, success: bool) {
        tracing::info!(
            actual_computer = %decision.actual_computer,
            shadow_computer = decision.recommended_computer.as_deref().unwrap_or("unknown"),
            agrees = decision.agrees_with_actual(),
            success,
            reason = ?decision.reason,
            "capacity shadow routing outcome"
        );
    }
}

/// Integration-friendly wrapper used by the routing loop.
pub async fn evaluate_capacity(
    router: &CapacityRouter,
    pool: &PgPool,
    model_id: &str,
    actual_computer: &str,
) -> Result<CapacityDecision, sqlx::Error> {
    router
        .evaluate_capacity(pool, model_id, actual_computer)
        .await
}

fn normalize_model_id(id: &str) -> String {
    id.to_ascii_lowercase()
        .trim_end_matches(".gguf")
        .split(':')
        .next()
        .unwrap_or_default()
        .replace('_', "-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn row(computer: &str, load: i32, age_secs: i64) -> CapacityRow {
        CapacityRow {
            computer: computer.into(),
            catalog_id: "test-model".into(),
            queue_depth: Some(load),
            active_requests: Some(0),
            last_scraped_at: Some(Utc::now() - Duration::seconds(age_secs)),
            status: "healthy".into(),
        }
    }

    #[test]
    fn stale_capacity_is_unknown() {
        let router = CapacityRouter::new();
        let decision = router.evaluate_rows(
            vec![row("node-a", 0, 91)],
            "test-model",
            "node-a",
            Utc::now(),
        );
        assert_eq!(decision.reason, CapacityReason::Unknown);
        assert!(decision.recommended_computer.is_none());
    }

    #[test]
    fn hysteresis_requires_repeated_material_improvement() {
        let router = CapacityRouter::new();
        let first = router.evaluate_rows(
            vec![row("node-a", 2, 0), row("node-b", 3, 0)],
            "test-model",
            "node-a",
            Utc::now(),
        );
        assert_eq!(first.recommended_computer.as_deref(), Some("node-a"));

        let borderline = router.evaluate_rows(
            vec![row("node-a", 2, 0), row("node-b", 1, 0)],
            "test-model",
            "node-a",
            Utc::now(),
        );
        assert_eq!(borderline.recommended_computer.as_deref(), Some("node-a"));

        let pending = router.evaluate_rows(
            vec![row("node-a", 3, 0), row("node-b", 1, 0)],
            "test-model",
            "node-a",
            Utc::now(),
        );
        assert_eq!(pending.recommended_computer.as_deref(), Some("node-a"));
        let switched = router.evaluate_rows(
            vec![row("node-a", 3, 0), row("node-b", 1, 0)],
            "test-model",
            "node-a",
            Utc::now(),
        );
        assert_eq!(switched.recommended_computer.as_deref(), Some("node-b"));
    }
}
