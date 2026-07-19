//! Priority / pick-score tests for the PM scheduler.
//!
//! These tests live next to the crate so they exercise the public API without
//! needing a running database.

use chrono::{DateTime, Duration, Utc};
use forge_fleet::pm::{Priority, Quadrant, Status, WorkItem, compute_pick_score};
use std::collections::HashSet;

fn item_with(
    quadrant: Quadrant,
    priority: i32,
    created_at: DateTime<Utc>,
    blockers: usize,
) -> WorkItem {
    WorkItem {
        id: "test".to_string(),
        status: Status::Ready,
        quadrant,
        priority: Priority::new(priority).unwrap(),
        created_at,
        blocked_by_count: blockers,
        required_capabilities: HashSet::new(),
        project_status: None,
        improvement_count: 0,
        performance_score: None,
    }
}

#[test]
fn priority_new_accepts_valid_range() {
    for v in 1..=5 {
        assert!(Priority::new(v).is_some(), "priority {v} should be valid");
    }
}

#[test]
fn priority_new_rejects_out_of_range() {
    assert!(Priority::new(0).is_none());
    assert!(Priority::new(6).is_none());
    assert!(Priority::new(-1).is_none());
    assert!(Priority::new(100).is_none());
}

#[test]
fn priority_score_is_inverse() {
    // Lower numeric priority value => higher scheduling score.
    assert_eq!(Priority::new(1).unwrap().score(), 500.0);
    assert_eq!(Priority::new(2).unwrap().score(), 400.0);
    assert_eq!(Priority::new(3).unwrap().score(), 300.0);
    assert_eq!(Priority::new(4).unwrap().score(), 200.0);
    assert_eq!(Priority::new(5).unwrap().score(), 100.0);
}

#[test]
fn quadrant_base_scores_are_ordered() {
    assert!(Quadrant::Q1.base_score() > Quadrant::Q2.base_score());
    assert!(Quadrant::Q2.base_score() > Quadrant::Q3.base_score());
    assert!(Quadrant::Q3.base_score() > Quadrant::Q4.base_score());
    assert_eq!(Quadrant::Q1.base_score(), 1000.0);
    assert_eq!(Quadrant::Q4.base_score(), 250.0);
}

#[test]
fn pick_score_exact_for_fresh_unblocked_item() {
    let now = Utc::now();
    let item = item_with(Quadrant::Q1, 1, now, 0);
    let score = compute_pick_score(&item, now);
    // Q1 base (1000) + P1 score (500) + zero age + zero blocker penalty.
    assert!((score - 1500.0).abs() < f64::EPSILON);
}

#[test]
fn pick_score_increases_with_age() {
    let now = Utc::now();
    let base = item_with(Quadrant::Q2, 3, now, 0);
    let aged = item_with(Quadrant::Q2, 3, now - Duration::hours(5), 0);

    let base_score = compute_pick_score(&base, now);
    let aged_score = compute_pick_score(&aged, now);

    // 5 hours * 10 points/hour = 50 extra points.
    assert!((aged_score - base_score - 50.0).abs() < 0.001);
    assert!(aged_score > base_score);
}

#[test]
fn pick_score_decreases_with_blockers() {
    let now = Utc::now();
    let free = item_with(Quadrant::Q2, 3, now, 0);
    let blocked = item_with(Quadrant::Q2, 3, now, 4);

    let free_score = compute_pick_score(&free, now);
    let blocked_score = compute_pick_score(&blocked, now);

    // 4 blockers * 50 penalty = 200 point reduction.
    assert!((free_score - blocked_score - 200.0).abs() < f64::EPSILON);
    assert!(blocked_score < free_score);
}

#[test]
fn pick_score_with_fractional_hours() {
    let now = Utc::now();
    let created = now - Duration::minutes(30);
    let item = item_with(Quadrant::Q3, 3, created, 0);

    let score = compute_pick_score(&item, now);
    // Q3 (500) + P3 (300) + 0.5h * 10 = 805.
    assert!((score - 805.0).abs() < 0.001);
}

#[test]
fn pick_score_with_negative_age_is_lower() {
    // created_at in the future relative to `now` (edge case: clock skew).
    let now = Utc::now();
    let created = now + Duration::hours(2);
    let item = item_with(Quadrant::Q2, 3, created, 0);

    let score = compute_pick_score(&item, now);
    let expected = Quadrant::Q2.base_score() + Priority::new(3).unwrap().score() - 20.0;
    assert!((score - expected).abs() < 0.001);
}

#[test]
fn pick_score_orders_q1_priority_one_above_q4_priority_five() {
    let now = Utc::now();
    let high = item_with(Quadrant::Q1, 1, now, 0);
    let low = item_with(Quadrant::Q4, 5, now, 0);

    assert!(compute_pick_score(&high, now) > compute_pick_score(&low, now));
}

#[test]
fn pick_score_tie_breaker_depends_on_age_when_quadrant_priority_equal() {
    let now = Utc::now();
    let older = item_with(Quadrant::Q2, 3, now - Duration::hours(3), 0);
    let newer = item_with(Quadrant::Q2, 3, now - Duration::hours(1), 0);

    assert!(compute_pick_score(&older, now) > compute_pick_score(&newer, now));
}

#[test]
fn pick_score_blockers_can_outweigh_priority_advantage() {
    let now = Utc::now();
    // A lower base+priority item with no blockers...
    let clean = item_with(Quadrant::Q3, 3, now, 0); // 500 + 300 = 800
    // ...beats a higher base+priority item weighed down by blockers.
    let blocked = item_with(Quadrant::Q2, 2, now, 8); // 750 + 400 - 400 = 750

    let clean_score = compute_pick_score(&clean, now);
    let blocked_score = compute_pick_score(&blocked, now);

    assert!(clean_score > blocked_score);
    assert!((clean_score - blocked_score - 50.0).abs() < f64::EPSILON);
}

#[test]
fn priority_default_is_three() {
    let default = Priority::default();
    assert_eq!(default.score(), 300.0);
}
