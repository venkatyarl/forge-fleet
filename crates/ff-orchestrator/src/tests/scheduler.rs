use std::time::Duration;

use crate::{
    PlacementPolicy, PriorityQueue, QueuedTask, ResourceRequirements, ScheduleDecision,
    ScheduledTask, Scheduler, TaskPriority,
};

fn requirements(cpu_cores: u32, memory_gib: u64) -> ResourceRequirements {
    ResourceRequirements {
        cpu_cores,
        memory_gib,
        gpu_required: false,
        estimated_duration: Duration::from_secs(60),
    }
}

#[test]
fn queued_tasks_are_dequeued_by_priority_and_scheduled() {
    let queue = PriorityQueue::with_default_timeout();
    let normal = QueuedTask::new("normal", requirements(1, 1), TaskPriority::Normal);
    let critical = QueuedTask::new("critical", requirements(2, 4), TaskPriority::Critical);

    queue.enqueue(normal, TaskPriority::Normal);
    queue.enqueue(critical.clone(), TaskPriority::Critical);

    let next = queue.dequeue().expect("critical task should be queued");
    assert_eq!(next.id, critical.id);

    let mut scheduler = Scheduler::new(PlacementPolicy::BinPack);
    scheduler.add_node(crate::NodeCapacity::from_config(
        "worker-1".to_string(),
        4,
        8,
        false,
    ));
    let task = ScheduledTask {
        id: next.id,
        description: next.description,
        project: next.project,
        requirements: next.requirements,
        priority: next.effective_priority,
        submitted_at: next.enqueued_at,
        preferred_nodes: Vec::new(),
        workload_type: next.workload_type,
    };

    assert!(matches!(
        scheduler.schedule_task(&task),
        ScheduleDecision::Assign { worker_name, .. } if worker_name == "worker-1"
    ));
    assert_eq!(queue.len(), 1);
}

#[test]
fn task_stays_queued_when_scheduler_has_no_capacity() {
    let queue = PriorityQueue::with_default_timeout();
    let queued = QueuedTask::new("large build", requirements(8, 16), TaskPriority::High);
    queue.enqueue(queued.clone(), TaskPriority::High);

    let mut scheduler = Scheduler::new(PlacementPolicy::BinPack);
    scheduler.add_node(crate::NodeCapacity::from_config(
        "worker-1".to_string(),
        2,
        4,
        false,
    ));
    let task = ScheduledTask {
        id: queued.id,
        description: queued.description.clone(),
        project: queued.project.clone(),
        requirements: queued.requirements.clone(),
        priority: queued.effective_priority,
        submitted_at: queued.enqueued_at,
        preferred_nodes: Vec::new(),
        workload_type: queued.workload_type.clone(),
    };

    assert!(matches!(
        scheduler.schedule_task(&task),
        ScheduleDecision::Queue { .. }
    ));
    assert_eq!(queue.peek().map(|task| task.id), Some(queued.id));
}
