#[allow(dead_code)]
#[path = "../../../src/pm/mod.rs"]
mod scheduler;

use chrono::{DateTime, Duration, TimeZone, Utc};
use scheduler::{
    Priority, Quadrant, Slot, Status, WorkItem, compute_pick_score, is_capable_of, scheduler_tick,
    wsjf_match_score,
};
use std::collections::HashSet;

fn item(
    id: &str,
    quadrant: Quadrant,
    priority: i32,
    created_at: DateTime<Utc>,
    blockers: usize,
    capabilities: &[&str],
) -> WorkItem {
    WorkItem {
        id: id.to_owned(),
        status: Status::Ready,
        quadrant,
        priority: Priority::new(priority).unwrap(),
        eisenhower_quadrant: None,
        numeric_priority: None,
        pick_score: None,
        capability_tags: Vec::new(),
        created_at,
        blocked_by_count: blockers,
        required_capabilities: capabilities
            .iter()
            .map(|capability| (*capability).to_owned())
            .collect(),
        project_status: None,
        improvement_count: 0,
        performance_score: None,
    }
}

fn slot(id: &str, capabilities: &[&str]) -> Slot {
    Slot {
        id: id.to_owned(),
        capabilities: capabilities
            .iter()
            .map(|capability| (*capability).to_owned())
            .collect::<HashSet<_>>(),
    }
}

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 21, 12, 0, 0).unwrap()
}

#[test]
fn compute_pick_score_covers_quadrants_age_and_blockers() {
    let now = now();
    let cases = [
        (Quadrant::Q1, 0, 0, 1500.0),
        (Quadrant::Q2, 2, 0, 1270.0),
        (Quadrant::Q3, 12, 1, 1070.0),
        (Quadrant::Q4, 24, 4, 790.0),
    ];

    for (quadrant, age_hours, blockers, expected) in cases {
        let work_item = item(
            "score",
            quadrant,
            1,
            now - Duration::hours(age_hours),
            blockers,
            &[],
        );
        assert_eq!(compute_pick_score(&work_item, now), expected);
    }
}

#[test]
fn capability_match_requires_every_tag_and_accepts_slot_supersets() {
    let now = now();
    let no_requirements = item("any", Quadrant::Q2, 3, now, 0, &[]);
    let gpu_linux = item("gpu-linux", Quadrant::Q2, 3, now, 0, &["gpu", "linux"]);

    assert!(is_capable_of(&no_requirements, &slot("empty", &[])));
    assert!(is_capable_of(
        &gpu_linux,
        &slot("superset", &["linux", "gpu", "docker"])
    ));
    assert!(!is_capable_of(
        &gpu_linux,
        &slot("missing-gpu", &["linux", "docker"])
    ));
    assert!(!is_capable_of(
        &gpu_linux,
        &slot("case-sensitive", &["GPU", "linux"])
    ));
}

#[test]
fn wsjf_orders_equal_delay_by_smallest_capability_set() {
    let now = now();
    let worker = slot("worker", &["cpu", "gpu", "linux"]);
    let small = item("small", Quadrant::Q1, 1, now, 0, &["cpu"]);
    let large = item("large", Quadrant::Q1, 1, now, 0, &["cpu", "gpu", "linux"]);

    let small_score = wsjf_match_score(&small, &worker, now).unwrap();
    let large_score = wsjf_match_score(&large, &worker, now).unwrap();
    assert_eq!(small_score, 1500.0);
    assert_eq!(large_score, 500.0);
    assert!(small_score > large_score);
    assert!(wsjf_match_score(&large, &slot("cpu-only", &["cpu"]), now).is_none());
}

#[test]
fn scheduler_picks_in_wsjf_order_and_uses_each_slot_once() {
    let now = now();
    let mut items = [
        item("third", Quadrant::Q2, 3, now, 0, &["linux"]),
        item("first", Quadrant::Q1, 1, now, 0, &["gpu"]),
        item("second", Quadrant::Q1, 1, now, 0, &["linux", "gpu"]),
        item("unmatched", Quadrant::Q1, 1, now, 0, &["tpu"]),
    ];
    let slots = [
        slot("gpu", &["gpu"]),
        slot("gpu-linux", &["gpu", "linux"]),
        slot("linux", &["linux"]),
    ];

    let assignments = scheduler_tick(&mut items, &slots, now);
    let sequence: Vec<_> = assignments
        .iter()
        .map(|assignment| (assignment.item_id.as_str(), assignment.slot_id.as_str()))
        .collect();

    assert_eq!(
        sequence,
        vec![
            ("first", "gpu"),
            ("third", "linux"),
            ("second", "gpu-linux"),
        ]
    );
    assert_eq!(
        assignments
            .iter()
            .map(|assignment| assignment.pick_score)
            .collect::<Vec<_>>(),
        vec![1500.0, 1050.0, 750.0]
    );
}
