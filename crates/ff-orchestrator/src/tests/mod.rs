//! Integration tests driving [`crate::queue::PriorityQueue`] and
//! [`crate::scheduler::Scheduler`] together, exercising the full
//! enqueue → dequeue → schedule → release → reschedule lifecycle.
//!
//! The per-module unit tests in `scheduler.rs`/`queue.rs` cover each type in
//! isolation; these tests cover the handoff between them, which is how the
//! leader's tick loop actually drives scheduling.

use crate::placement::PlacementPolicy;
use crate::queue::{PriorityQueue, QueuedTask};
use crate::scheduler::{
    NodeCapacity, ResourceRequirements, ScheduleDecision, ScheduledTask, Scheduler, TaskPriority,
};

fn small_requirements() -> ResourceRequirements {
    ResourceRequirements {
        cpu_cores: 2,
        memory_gib: 4,
        gpu_required: false,
        estimated_duration: std::time::Duration::from_secs(60),
    }
}

#[test]
fn dequeue_then_schedule_assigns_to_a_node() {
    let queue = PriorityQueue::with_default_timeout();
    let mut scheduler = Scheduler::new(PlacementPolicy::BinPack);
    scheduler.add_node(NodeCapacity::from_config(
        "node-a".to_string(),
        8,
        16,
        false,
    ));

    queue.enqueue(
        QueuedTask::new("build project", small_requirements(), TaskPriority::Normal),
        TaskPriority::Normal,
    );

    let queued = queue.dequeue().expect("task should be queued");
    let scheduled = ScheduledTask::new(&queued.description)
        .with_priority(queued.effective_priority)
        .with_requirements(queued.requirements);

    let decision = scheduler.schedule_task(&scheduled);
    assert!(decision.is_assigned());
    assert_eq!(decision.target_node(), Some("node-a"));
    assert!(queue.is_empty());
}

#[test]
fn no_node_fits_task_stays_available_to_requeue() {
    let queue = PriorityQueue::with_default_timeout();
    let mut scheduler = Scheduler::new(PlacementPolicy::BinPack);
    // Node too small for the request.
    scheduler.add_node(NodeCapacity::from_config("node-a".to_string(), 1, 1, false));

    let task = QueuedTask::new("heavy job", small_requirements(), TaskPriority::High);
    let id = task.id;
    queue.enqueue(task, TaskPriority::High);

    let queued = queue.peek().expect("task should still be queued");
    let scheduled = ScheduledTask::new(&queued.description)
        .with_priority(queued.effective_priority)
        .with_requirements(queued.requirements.clone());

    let decision = scheduler.schedule_task(&scheduled);
    assert!(!decision.is_assigned());
    assert!(matches!(decision, ScheduleDecision::Queue { .. }));

    // Task remains in the queue for a later scheduling attempt.
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.peek().unwrap().id, id);
}

#[test]
fn release_frees_capacity_for_the_next_queued_task() {
    let queue = PriorityQueue::with_default_timeout();
    let mut scheduler = Scheduler::new(PlacementPolicy::BinPack);
    scheduler.add_node(NodeCapacity::from_config("node-a".to_string(), 2, 4, false));

    queue.enqueue(
        QueuedTask::new("first", small_requirements(), TaskPriority::Normal),
        TaskPriority::Normal,
    );
    queue.enqueue(
        QueuedTask::new("second", small_requirements(), TaskPriority::Normal),
        TaskPriority::Normal,
    );

    let first = queue.dequeue().unwrap();
    let first_scheduled = ScheduledTask::new(&first.description)
        .with_priority(first.effective_priority)
        .with_requirements(first.requirements);
    let first_decision = scheduler.schedule_task(&first_scheduled);
    assert!(first_decision.is_assigned(), "node has capacity for one");

    // Node is now full — second task can't be placed yet.
    let second = queue.dequeue().unwrap();
    let second_scheduled = ScheduledTask::new(&second.description)
        .with_priority(second.effective_priority)
        .with_requirements(second.requirements.clone());
    let second_decision = scheduler.schedule_task(&second_scheduled);
    assert!(!second_decision.is_assigned());

    // Release the first task's allocation, then reschedule the second.
    scheduler.release_task("node-a", first_scheduled.id);
    let retried = scheduler.schedule_task(&second_scheduled);
    assert!(retried.is_assigned(), "capacity freed up after release");
    assert_eq!(retried.target_node(), Some("node-a"));
}
