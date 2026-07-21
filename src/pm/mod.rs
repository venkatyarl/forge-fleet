//! Project-management scheduler tick.
//!
//! This module implements a lightweight, deterministic scheduler that matches
//! ready work items to capable slots.  It is intentionally pure logic so it can
//! be unit-tested without a database; a caller that wants DB-driven ticks can
//! load `WorkItem` / `Slot` rows and pass them to [`scheduler_tick`].

use chrono::{DateTime, Utc};
pub use ff_core::schema::work_items::Quadrant;
use std::collections::HashSet;

// ─── Types ───────────────────────────────────────────────────────────────────

/// Priority level 1 (critical) through 5 (minimal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Priority(pub i32);

impl Priority {
    /// Validate and wrap a priority value.
    pub fn new(v: i32) -> Option<Self> {
        if (1..=5).contains(&v) {
            Some(Self(v))
        } else {
            None
        }
    }

    /// Score contribution: lower numeric value -> higher score.
    pub fn score(&self) -> f64 {
        ((6 - self.0) * 100) as f64
    }
}

impl Default for Priority {
    fn default() -> Self {
        Self(3)
    }
}

/// Lifecycle state of a work item.  Only [`Status::Ready`] items are eligible
/// for scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Idea,
    Ready,
    InProgress,
    Blocked,
    Done,
}

/// A unit of work waiting to be assigned to a slot.
#[derive(Debug, Clone)]
pub struct WorkItem {
    pub id: String,
    pub status: Status,
    pub quadrant: Quadrant,
    pub priority: Priority,
    pub created_at: DateTime<Utc>,
    pub blocked_by_count: usize,
    pub required_capabilities: HashSet<String>,
    pub eisenhower_quadrant: Quadrant,
    pub numeric_priority: i32,
    pub pick_score: f64,
    pub capability_tags: HashSet<String>,
    /// Self-improvement tracking: coarse project health/status bucket the item
    /// belongs to (e.g. "green", "yellow", "red"). Optional so legacy items
    /// without a status still deserialize.
    pub project_status: Option<String>,
    /// Self-improvement tracking: number of prior improvement iterations this
    /// work item has undergone.
    pub improvement_count: usize,
    /// Self-improvement tracking: latest performance metric score associated
    /// with the item (e.g. throughput, success rate). Higher is better.
    pub performance_score: Option<f64>,
}

/// An available worker slot with a set of capabilities.
#[derive(Debug, Clone)]
pub struct Slot {
    pub id: String,
    pub capabilities: HashSet<String>,
}

/// The result of assigning one work item to one slot.
#[derive(Debug, Clone)]
pub struct Assignment {
    pub item_id: String,
    pub slot_id: String,
    pub pick_score: f64,
}

// ─── Scoring ─────────────────────────────────────────────────────────────────

/// Points awarded per hour of age.
const AGE_POINTS_PER_HOUR: f64 = 10.0;

/// Penalty per blocking dependency.
const BLOCKER_PENALTY: f64 = 50.0;

/// Compute the scheduling score for a ready item relative to `now`.
///
/// Higher scores are scheduled first.  The score blends quadrant, priority,
/// age, and blocker count.
pub fn compute_pick_score(item: &WorkItem, now: DateTime<Utc>) -> f64 {
    let age = now.signed_duration_since(item.created_at);
    let age_hours = age.num_seconds() as f64 / 3600.0;

    item.quadrant.base_score() + item.priority.score() + (age_hours * AGE_POINTS_PER_HOUR)
        - (item.blocked_by_count as f64 * BLOCKER_PENALTY)
}

// ─── Capability matching ─────────────────────────────────────────────────────

/// Returns `true` when `slot` provides every capability required by `item`.
///
/// A work item with no required capabilities matches any slot; a slot matches
/// only when its capability set is a superset of the item's required tags.
pub fn slot_can_handle(item: &WorkItem, slot: &Slot) -> bool {
    item.required_capabilities.is_subset(&slot.capabilities)
}

/// Estimated job size for the WSJF denominator.
///
/// Sized by the number of distinct capabilities the item requires.  Items with
/// no required capabilities default to a size of 1.0 so the WSJF ratio is
/// well-defined and capability-agnostic work is not infinitely preferred.
const MIN_JOB_SIZE: f64 = 1.0;

pub fn job_size(item: &WorkItem) -> f64 {
    (item.required_capabilities.len() as f64).max(MIN_JOB_SIZE)
}

/// Cost of Delay for the WSJF numerator.
///
/// Reuses the existing pick score, which already weights urgency (quadrant +
/// priority), ageing, and blocker penalties.
pub fn cost_of_delay(item: &WorkItem, now: DateTime<Utc>) -> f64 {
    compute_pick_score(item, now)
}

/// WSJF capability-match score for assigning `item` to `slot`.
///
/// Returns `Some(cost_of_delay / job_size)` when the slot can handle the item,
/// otherwise `None`.  Higher scores schedule first.
pub fn wsjf_match_score(item: &WorkItem, slot: &Slot, now: DateTime<Utc>) -> Option<f64> {
    if !slot_can_handle(item, slot) {
        return None;
    }
    Some(cost_of_delay(item, now) / job_size(item))
}

// ─── Tick ────────────────────────────────────────────────────────────────────

/// One scheduler pass.
///
/// 1. Filters to [`Status::Ready`] items.
/// 2. Computes a WSJF match score for each using [`wsjf_match_score`].
/// 3. Sorts by score descending (ties broken by item id for determinism).
/// 4. Greedily assigns each ready item to the first capable, unused slot.
///
/// Returns the list of assignments made this tick.  Items that cannot be
/// matched to a capable slot, or that are not `Ready`, are left unassigned.
pub fn scheduler_tick(items: &[WorkItem], slots: &[Slot], now: DateTime<Utc>) -> Vec<Assignment> {
    let mut ready: Vec<(f64, &WorkItem)> = items
        .iter()
        .filter(|i| i.status == Status::Ready)
        .map(|i| {
            // Score each item by its best WSJF match across all slots; items
            // that no slot can handle receive a score of 0.0 and are skipped.
            let best_score = slots
                .iter()
                .filter_map(|slot| wsjf_match_score(i, slot, now))
                .fold(0.0, f64::max);
            (best_score, i)
        })
        .collect();

    // Higher score first; tie-break by id so the output is deterministic.
    ready.sort_by(|(a_score, a_item), (b_score, b_item)| {
        b_score
            .partial_cmp(a_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a_item.id.cmp(&b_item.id))
    });

    let mut available: Vec<&Slot> = slots.iter().collect();
    let mut assignments = Vec::new();

    for (_, item) in ready {
        if let Some(idx) = available
            .iter()
            .position(|slot| slot_can_handle(item, slot))
        {
            let slot = available.swap_remove(idx);
            assignments.push(Assignment {
                item_id: item.id.clone(),
                slot_id: slot.id.clone(),
                pick_score: wsjf_match_score(item, slot, now).unwrap_or(0.0),
            });
        }
    }

    assignments
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn item(
        id: &str,
        quadrant: Quadrant,
        priority: i32,
        age_hours: i64,
        blockers: usize,
        caps: &[&str],
    ) -> WorkItem {
        WorkItem {
            id: id.to_string(),
            status: Status::Ready,
            quadrant,
            priority: Priority::new(priority).unwrap(),
            created_at: Utc::now() - Duration::hours(age_hours),
            blocked_by_count: blockers,
            required_capabilities: caps.iter().map(|s| s.to_string()).collect(),
            eisenhower_quadrant: quadrant,
            numeric_priority: priority,
            pick_score: 0.0,
            capability_tags: caps.iter().map(|s| s.to_string()).collect(),
            project_status: None,
            improvement_count: 0,
            performance_score: None,
        }
    }

    fn slot(id: &str, caps: &[&str]) -> Slot {
        Slot {
            id: id.to_string(),
            capabilities: caps.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn q1_priority_one_beats_q4_priority_five() {
        let now = Utc::now();
        let q1p1 = item("a", Quadrant::Q1, 1, 0, 0, &[]);
        let q4p5 = item("b", Quadrant::Q4, 5, 0, 0, &[]);
        assert!(compute_pick_score(&q1p1, now) > compute_pick_score(&q4p5, now));
    }

    #[test]
    fn age_increases_score() {
        let now = Utc::now();
        let fresh = item("a", Quadrant::Q2, 3, 1, 0, &[]);
        let old = item("b", Quadrant::Q2, 3, 10, 0, &[]);
        assert!(compute_pick_score(&old, now) > compute_pick_score(&fresh, now));
    }

    #[test]
    fn blockers_decrease_score() {
        let now = Utc::now();
        let free = item("a", Quadrant::Q2, 3, 0, 0, &[]);
        let blocked = item("b", Quadrant::Q2, 3, 0, 3, &[]);
        assert!(compute_pick_score(&free, now) > compute_pick_score(&blocked, now));
    }

    #[test]
    fn only_ready_items_are_assigned() {
        let now = Utc::now();
        let mut not_ready = item("nr", Quadrant::Q1, 1, 0, 0, &[]);
        not_ready.status = Status::Blocked;
        let ready = item("r", Quadrant::Q1, 1, 0, 0, &[]);

        let slots = [slot("s1", &[])];
        let assignments = scheduler_tick(&[not_ready, ready], &slots, now);
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].item_id, "r");
    }

    #[test]
    fn capability_mismatch_prevents_assignment() {
        let now = Utc::now();
        let items = [item("a", Quadrant::Q1, 1, 0, 0, &["gpu"])];
        let slots = [slot("s1", &["cpu"])];
        let assignments = scheduler_tick(&items, &slots, now);
        assert!(assignments.is_empty());
    }

    #[test]
    fn higher_score_item_wins_limited_slot() {
        let now = Utc::now();
        let high = item("high", Quadrant::Q1, 1, 10, 0, &[]);
        let low = item("low", Quadrant::Q4, 5, 0, 0, &[]);
        let slots = [slot("only", &[])];

        let assignments = scheduler_tick(&[low.clone(), high.clone()], &slots, now);
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].item_id, "high");
    }

    #[test]
    fn slots_are_not_reused() {
        let now = Utc::now();
        let items = [
            item("a", Quadrant::Q1, 1, 0, 0, &[]),
            item("b", Quadrant::Q1, 1, 0, 0, &[]),
        ];
        let slots = [slot("s1", &[])];
        let assignments = scheduler_tick(&items, &slots, now);
        assert_eq!(assignments.len(), 1);
    }

    #[test]
    fn slot_can_handle_allows_superset() {
        let item = item("a", Quadrant::Q2, 3, 0, 0, &["cpu"]);
        let slot = slot("s1", &["cpu", "gpu"]);
        assert!(slot_can_handle(&item, &slot));
    }

    #[test]
    fn slot_can_handle_rejects_missing_capability() {
        let item = item("a", Quadrant::Q2, 3, 0, 0, &["gpu"]);
        let slot = slot("s1", &["cpu"]);
        assert!(!slot_can_handle(&item, &slot));
    }

    #[test]
    fn wsjf_match_score_none_when_slot_cannot_handle() {
        let now = Utc::now();
        let item = item("a", Quadrant::Q1, 1, 0, 0, &["gpu"]);
        let slot = slot("s1", &["cpu"]);
        assert!(wsjf_match_score(&item, &slot, now).is_none());
    }

    #[test]
    fn wsjf_prefers_shorter_job_at_same_cost_of_delay() {
        let now = Utc::now();
        let simple = item("simple", Quadrant::Q1, 1, 0, 0, &["cpu"]);
        let complex = item("complex", Quadrant::Q1, 1, 0, 0, &["cpu", "gpu", "ram"]);
        let slot = slot("s1", &["cpu", "gpu", "ram", "disk"]);

        let simple_score = wsjf_match_score(&simple, &slot, now).unwrap();
        let complex_score = wsjf_match_score(&complex, &slot, now).unwrap();

        // Same quadrant, priority, age and blockers => same cost of delay.
        // The item requiring fewer capabilities has a smaller job size, so its
        // WSJF score is higher.
        assert!(simple_score > complex_score);
    }
}
