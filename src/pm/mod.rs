//! Project-management scheduler tick.
//!
//! This module implements a lightweight, deterministic scheduler that matches
//! ready work items to capable slots.  It is intentionally pure logic so it can
//! be unit-tested without a database; a caller that wants DB-driven ticks can
//! load `WorkItem` / `Slot` rows and pass them to [`scheduler_tick`].

use chrono::{DateTime, Utc};
use std::collections::HashSet;

// ─── Types ───────────────────────────────────────────────────────────────────

/// Eisenhower-style quadrant used for coarse scheduling priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// Base score contribution; higher is picked sooner.
    pub fn base_score(&self) -> f64 {
        match self {
            Self::Q1 => 1000.0,
            Self::Q2 => 750.0,
            Self::Q3 => 500.0,
            Self::Q4 => 250.0,
        }
    }
}

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

// ─── Tick ────────────────────────────────────────────────────────────────────

/// One scheduler pass.
///
/// 1. Filters to [`Status::Ready`] items.
/// 2. Computes a `pick_score` for each using [`compute_pick_score`].
/// 3. Sorts by score descending (ties broken by item id for determinism).
/// 4. Greedily assigns each ready item to the first capable, unused slot.
///
/// Returns the list of assignments made this tick.  Items that cannot be
/// matched to a capable slot, or that are not `Ready`, are left unassigned.
pub fn scheduler_tick(items: &[WorkItem], slots: &[Slot], now: DateTime<Utc>) -> Vec<Assignment> {
    let mut ready: Vec<(f64, &WorkItem)> = items
        .iter()
        .filter(|i| i.status == Status::Ready)
        .map(|i| (compute_pick_score(i, now), i))
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
            .position(|slot| item.required_capabilities.is_subset(&slot.capabilities))
        {
            let slot = available.swap_remove(idx);
            assignments.push(Assignment {
                item_id: item.id.clone(),
                slot_id: slot.id.clone(),
                pick_score: compute_pick_score(item, now),
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
}
