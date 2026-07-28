//! Fairness-aware slot dispatch ordered by Eisenhower priority score.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use ff_api::routes::work_queue::{WorkItemResponse, WorkQueue};
use ff_core::schema::work_items::Quadrant;

const AGE_POINTS_PER_HOUR: f64 = 10.0;

pub fn eisenhower_quadrant(priority: i32) -> Quadrant {
    match priority {
        i if i <= 1 => Quadrant::Q1,
        2 => Quadrant::Q2,
        3 => Quadrant::Q3,
        _ => Quadrant::Q4,
    }
}

pub fn priority_score(item: &WorkItemResponse, now: DateTime<Utc>) -> f64 {
    let age_hours = (now.timestamp() - item.created_at) as f64 / 3600.0;
    eisenhower_quadrant(item.priority).base_score() + age_hours.max(0.0) * AGE_POINTS_PER_HOUR
}

#[derive(Debug, Clone)]
pub struct SlotAssignment {
    pub work_item_id: String,
    pub quadrant: Quadrant,
    pub score: f64,
}

#[derive(Debug, Clone)]
pub struct DispatchedTask {
    pub slot_id: String,
    pub work_item_id: String,
    pub kind: String,
    pub payload: serde_json::Value,
    pub priority: i32,
    pub quadrant: Quadrant,
    pub score: f64,
}

#[derive(Debug)]
struct Slot {
    id: String,
    assignment: Option<SlotAssignment>,
}

pub struct SlotPool {
    queue: Arc<WorkQueue>,
    slots: Mutex<Vec<Slot>>,
}

impl SlotPool {
    pub fn new(queue: Arc<WorkQueue>, slot_ids: impl IntoIterator<Item = String>) -> Self {
        Self {
            queue,
            slots: Mutex::new(
                slot_ids
                    .into_iter()
                    .map(|id| Slot {
                        id,
                        assignment: None,
                    })
                    .collect(),
            ),
        }
    }

    fn slots(&self) -> std::sync::MutexGuard<'_, Vec<Slot>> {
        self.slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn assignment_for(&self, slot_id: &str) -> Option<SlotAssignment> {
        self.slots()
            .iter()
            .find(|slot| slot.id == slot_id)
            .and_then(|slot| slot.assignment.clone())
    }

    pub fn dispatch_next(&self, now: DateTime<Utc>) -> Option<DispatchedTask> {
        let mut slots = self.slots();
        let slot = slots.iter_mut().find(|slot| slot.assignment.is_none())?;
        let (item, score) = self
            .queue
            .claim_pending_by_score(|item| priority_score(item, now))?;
        let quadrant = eisenhower_quadrant(item.priority);
        slot.assignment = Some(SlotAssignment {
            work_item_id: item.id.clone(),
            quadrant,
            score,
        });
        Some(DispatchedTask {
            slot_id: slot.id.clone(),
            work_item_id: item.id,
            kind: item.kind,
            payload: item.payload,
            priority: item.priority,
            quadrant,
            score,
        })
    }

    pub fn release(&self, slot_id: &str) -> bool {
        match self.slots().iter_mut().find(|slot| slot.id == slot_id) {
            Some(slot) if slot.assignment.is_some() => {
                slot.assignment = None;
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ff_api::routes::work_queue::SubmitWorkItemRequest;

    #[test]
    fn slots_respect_score_propagate_metadata_and_preserve_lifecycle() {
        let queue = Arc::new(WorkQueue::new());
        for (kind, priority) in [("low", 5), ("critical", 1)] {
            queue.submit(SubmitWorkItemRequest {
                kind: kind.into(),
                payload: serde_json::Value::Null,
                priority,
            });
        }
        let pool = SlotPool::new(queue, vec!["s1".into()]);
        let task = pool.dispatch_next(Utc::now()).unwrap();
        assert_eq!(task.kind, "critical");
        assert_eq!(task.quadrant, Quadrant::Q1);
        let assignment = pool.assignment_for("s1").unwrap();
        assert_eq!(assignment.score, task.score);
        assert!(pool.dispatch_next(Utc::now()).is_none());
        assert!(pool.release("s1"));
        assert!(pool.assignment_for("s1").is_none());
    }

    #[test]
    fn age_increases_score() {
        let item = WorkItemResponse {
            id: "x".into(),
            kind: "test".into(),
            payload: serde_json::Value::Null,
            status: "pending".into(),
            priority: 3,
            created_at: 0,
            updated_at: 0,
        };
        assert!(
            priority_score(&item, DateTime::from_timestamp(3600, 0).unwrap())
                > priority_score(&item, DateTime::from_timestamp(0, 0).unwrap())
        );
    }
}
