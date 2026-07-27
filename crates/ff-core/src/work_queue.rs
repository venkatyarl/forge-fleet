//! In-memory work queue with Eisenhower scoring and multi-project fairness.

use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap},
};

use chrono::{DateTime, Utc};

const AGE_POINTS_PER_HOUR: f64 = 10.0;
const BLOCKER_PENALTY: f64 = 50.0;

/// Eisenhower quadrant: urgency × importance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quadrant {
    /// Urgent and important.
    Q1,
    /// Important but not urgent.
    Q2,
    /// Urgent but not important.
    Q3,
    /// Neither urgent nor important.
    Q4,
}

impl Quadrant {
    /// Base scheduling score for the quadrant.
    pub fn base_score(self) -> f64 {
        match self {
            Self::Q1 => 1000.0,
            Self::Q2 => 750.0,
            Self::Q3 => 500.0,
            Self::Q4 => 250.0,
        }
    }
}

/// Priority band from 1 (highest) to 5 (lowest).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Priority(pub i32);

impl Priority {
    /// Validate and wrap a priority value.
    pub fn new(value: i32) -> Option<Self> {
        (1..=5).contains(&value).then_some(Self(value))
    }

    /// Score contribution: lower numeric priority means a higher score.
    pub fn score(self) -> f64 {
        ((6 - self.0) * 100) as f64
    }
}

impl Default for Priority {
    fn default() -> Self {
        Self(3)
    }
}

/// A unit of work waiting in the queue.
#[derive(Debug, Clone)]
pub struct WorkQueueItem {
    pub id: String,
    /// Project or tenant identifier used for fairness accounting.
    pub project_id: String,
    pub quadrant: Quadrant,
    pub priority: Priority,
    pub created_at: DateTime<Utc>,
    pub blockers: usize,
}

impl WorkQueueItem {
    /// Compute the scheduling score relative to `now`.
    pub fn score(&self, now: DateTime<Utc>) -> f64 {
        let age_hours = now.signed_duration_since(self.created_at).num_seconds() as f64 / 3600.0;
        self.quadrant.base_score() + self.priority.score() + age_hours * AGE_POINTS_PER_HOUR
            - self.blockers as f64 * BLOCKER_PENALTY
    }
}

#[derive(Debug, Clone)]
struct ScoredItem {
    priority_score: f64,
    sequence: u64,
    item: WorkQueueItem,
}

impl PartialEq for ScoredItem {
    fn eq(&self, other: &Self) -> bool {
        self.priority_score.total_cmp(&other.priority_score) == Ordering::Equal
            && self.sequence == other.sequence
    }
}

impl Eq for ScoredItem {}

impl PartialOrd for ScoredItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoredItem {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority_score
            .total_cmp(&other.priority_score)
            // BinaryHeap is a max heap; earlier inserts win score ties.
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

/// Eisenhower-ordered queue with quota-weighted project fairness.
///
/// Each project owns a max heap, so insertion and removal within a project are
/// `O(log n)`. A pop compares only project heap heads instead of scanning every
/// queued item.
#[derive(Debug, Default, Clone)]
pub struct WorkQueue {
    projects: HashMap<String, BinaryHeap<ScoredItem>>,
    project_quotas: HashMap<String, f64>,
    popped_counts: HashMap<String, usize>,
    len: usize,
    next_sequence: u64,
}

impl WorkQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a project's relative share. Non-positive or non-finite values are
    /// ignored and the project retains the default share of `1.0`.
    pub fn set_project_quota(&mut self, project_id: &str, quota: f64) {
        if quota.is_finite() && quota > 0.0 {
            self.project_quotas.insert(project_id.to_string(), quota);
        } else {
            self.project_quotas.remove(project_id);
        }
    }

    pub fn remove_project_quota(&mut self, project_id: &str) {
        self.project_quotas.remove(project_id);
    }

    /// Compute `score()` once on insert and add the item to its project heap.
    pub fn push(&mut self, item: WorkQueueItem) {
        let scored = ScoredItem {
            priority_score: item.score(Utc::now()),
            sequence: self.next_sequence,
            item,
        };
        self.next_sequence += 1;
        self.len += 1;
        self.projects
            .entry(scored.item.project_id.clone())
            .or_default()
            .push(scored);
    }

    /// Remove the highest quota-weighted project head.
    ///
    /// Fairness policy: quotas are relative target shares. Each project head's
    /// Eisenhower score is multiplied by `target_share / actual_share`, clamped
    /// to `[0.25, 4.0]`; an unserved project receives weight `2.0`. This promotes
    /// under-served projects without allowing fairness to erase large urgency
    /// differences. Before any item is served, quota share is a small tie-break
    /// boost. Projects without an explicit quota receive the default share 1.
    pub fn pop(&mut self) -> Option<WorkQueueItem> {
        let total_popped: usize = self.popped_counts.values().sum();
        let active_quotas: HashMap<&str, f64> = self
            .projects
            .keys()
            .map(|project_id| {
                (
                    project_id.as_str(),
                    self.project_quotas.get(project_id).copied().unwrap_or(1.0),
                )
            })
            .collect();
        let total_quota: f64 = active_quotas.values().sum();

        let project_id = self
            .projects
            .iter()
            .filter_map(|(project_id, heap)| {
                let head = heap.peek()?;
                let target_share = active_quotas[project_id.as_str()] / total_quota;
                let weight = if total_popped == 0 {
                    1.0 + target_share
                } else {
                    let served = self.popped_counts.get(project_id).copied().unwrap_or(0);
                    if served == 0 {
                        2.0
                    } else {
                        (target_share / (served as f64 / total_popped as f64)).clamp(0.25, 4.0)
                    }
                };
                Some((project_id, head.priority_score * weight, head.sequence))
            })
            .max_by(|left, right| {
                left.1
                    .total_cmp(&right.1)
                    .then_with(|| right.2.cmp(&left.2))
            })
            .map(|(project_id, _, _)| project_id.clone())?;

        let heap = self.projects.get_mut(&project_id)?;
        let scored = heap.pop()?;
        if heap.is_empty() {
            self.projects.remove(&project_id);
        }
        self.len -= 1;
        *self.popped_counts.entry(project_id).or_default() += 1;
        Some(scored.item)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn reset_fairness_counts(&mut self) {
        self.popped_counts.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn item(id: &str, project: &str, quadrant: Quadrant, priority: i32) -> WorkQueueItem {
        WorkQueueItem {
            id: id.into(),
            project_id: project.into(),
            quadrant,
            priority: Priority::new(priority).unwrap(),
            created_at: Utc::now(),
            blockers: 0,
        }
    }

    #[test]
    fn push_computes_score_and_pop_uses_descending_order() {
        let mut queue = WorkQueue::new();
        queue.push(item("low", "one", Quadrant::Q4, 5));
        queue.push(item("high", "one", Quadrant::Q1, 1));
        assert_eq!(queue.pop().unwrap().id, "high");
        assert_eq!(queue.pop().unwrap().id, "low");
    }

    #[test]
    fn score_accounts_for_age_and_blockers() {
        let now = Utc::now();
        let mut old = item("old", "one", Quadrant::Q2, 3);
        old.created_at = now - Duration::hours(5);
        let mut blocked = old.clone();
        blocked.blockers = 2;
        assert_eq!(old.score(now) - blocked.score(now), 100.0);
    }

    #[test]
    fn fairness_gives_an_unserved_project_a_turn() {
        let mut queue = WorkQueue::new();
        queue.set_project_quota("one", 1.0);
        queue.set_project_quota("two", 1.0);
        queue.push(item("one-a", "one", Quadrant::Q1, 1));
        queue.push(item("one-b", "one", Quadrant::Q1, 1));
        queue.push(item("two", "two", Quadrant::Q2, 2));
        assert_eq!(queue.pop().unwrap().project_id, "one");
        assert_eq!(queue.pop().unwrap().project_id, "two");
    }

    #[test]
    fn fairness_respects_quota_ratio() {
        let mut queue = WorkQueue::new();
        queue.set_project_quota("one", 2.0);
        queue.set_project_quota("two", 1.0);
        for n in 0..3 {
            queue.push(item(&format!("one-{n}"), "one", Quadrant::Q2, 3));
            queue.push(item(&format!("two-{n}"), "two", Quadrant::Q2, 3));
        }
        let projects: Vec<_> = (0..3).map(|_| queue.pop().unwrap().project_id).collect();
        assert_eq!(projects.iter().filter(|id| *id == "one").count(), 2);
        assert_eq!(projects.iter().filter(|id| *id == "two").count(), 1);
    }
}
