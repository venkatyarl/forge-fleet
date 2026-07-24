#[allow(dead_code)]
#[path = "../../../src/pm/mod.rs"]
mod scheduler;

use chrono::{DateTime, Duration, TimeZone, Utc};
use ff_db::models::slots::Slot as DbSlot;
use ff_db::models::work_item::WorkItem as DbWorkItem;
use scheduler::{
    Priority, Quadrant, Slot, Status, WorkItem, compute_pick_score, scheduler_tick,
    slot_can_handle, wsjf_match_score,
};
use serde_json::Value;
use std::collections::HashSet;
use uuid::Uuid;

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

    assert!(slot_can_handle(&no_requirements, &slot("empty", &[])));
    assert!(slot_can_handle(
        &gpu_linux,
        &slot("superset", &["linux", "gpu", "docker"])
    ));
    assert!(!slot_can_handle(
        &gpu_linux,
        &slot("missing-gpu", &["linux", "docker"])
    ));
    assert!(!slot_can_handle(
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
    let items = [
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

    let assignments = scheduler_tick(&items, &slots, now);
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
    // `Assignment.pick_score` is the raw `compute_pick_score` (Q1/P1 = 1500,
    // Q2/P3 = 1050), not the WSJF-adjusted score used only to order picks —
    // "second" requires 2 capabilities so its WSJF score (750) is lower than
    // "first"'s, but its own compute_pick_score is still 1500 (same quadrant
    // and priority as "first").
    assert_eq!(
        assignments
            .iter()
            .map(|assignment| assignment.pick_score)
            .collect::<Vec<_>>(),
        vec![1500.0, 1050.0, 1500.0]
    );
}

// ─── `Slot::is_capable_of` (ff-db persistence models) ───────────────────────
//
// These exercise the DB-backed `Slot`/`WorkItem` models (distinct from the
// pure `scheduler::Slot` above): `is_capable_of` matches a required tag
// against the union of a slot's `kind`, `skill`, `model_preference`, the repo
// name derived from `workspace_dir`, and its explicit `capabilities`, with
// `"ram:<n>"` tags checked numerically against `ram_gb` instead.

fn db_slot(
    kind: &str,
    model_preference: Option<&str>,
    workspace_dir: &str,
    capabilities: Value,
    skill: Value,
    ram_gb: Option<i32>,
) -> DbSlot {
    DbSlot {
        id: Uuid::nil(),
        computer_id: Uuid::nil(),
        slot: 0,
        status: "idle".to_string(),
        current_work_item_id: None,
        started_at: None,
        workspace_dir: workspace_dir.to_string(),
        model_preference: model_preference.map(str::to_string),
        last_heartbeat_at: None,
        metadata: serde_json::json!({}),
        kind: kind.to_string(),
        capabilities,
        skill,
        ram_gb,
    }
}

fn db_item_requiring(required_capabilities: Value) -> DbWorkItem {
    DbWorkItem {
        id: Uuid::nil(),
        project_id: "p".to_string(),
        milestone_id: None,
        parent_id: None,
        kind: "task".to_string(),
        title: "t".to_string(),
        description: None,
        labels: serde_json::json!([]),
        status: "ready".to_string(),
        priority: "normal".to_string(),
        assigned_to: None,
        assigned_computer: None,
        branch_name: None,
        pr_url: None,
        brain_node_ids: serde_json::json!([]),
        created_at: Utc::now(),
        created_by: "test".to_string(),
        started_at: None,
        completed_at: None,
        due_date: None,
        estimated_hours: None,
        metadata: serde_json::json!({}),
        required_capabilities,
        complexity: "low".to_string(),
        predicted_paths: serde_json::json!([]),
        touched_paths: serde_json::json!([]),
        base_branch: None,
        base_sha: None,
        integration_branch: None,
        merge_rank: None,
        risk_score: 0.0,
        reviewer_required: false,
        attempts: 0,
        last_error: None,
        repo_id: None,
        repo_url: None,
        repo_path: None,
        context: serde_json::json!({}),
        parked: false,
        pre_work: serde_json::json!({}),
        work: serde_json::json!({}),
        post_work: serde_json::json!({}),
        cleanup_complete: false,
        original_signal: serde_json::json!({}),
        signal_cleared: None,
        signal_verified_at: None,
        refiled_from: None,
        cortex_subgraph_id: None,
    }
}

#[test]
fn is_capable_of_matches_no_required_capabilities_regardless_of_slot() {
    let bare = db_slot(
        "sub_agent",
        None,
        "/tmp/slot",
        serde_json::json!([]),
        serde_json::json!([]),
        None,
    );
    assert!(bare.is_capable_of(&db_item_requiring(serde_json::json!([]))));
}

#[test]
fn is_capable_of_matches_explicit_capabilities_tag() {
    let gpu_slot = db_slot(
        "sub_agent",
        None,
        "/tmp/slot",
        serde_json::json!(["gpu", "rust"]),
        serde_json::json!([]),
        None,
    );
    assert!(gpu_slot.is_capable_of(&db_item_requiring(serde_json::json!(["gpu"]))));
    assert!(!gpu_slot.is_capable_of(&db_item_requiring(serde_json::json!(["macos"]))));
}

#[test]
fn is_capable_of_matches_kind_and_skill_tags() {
    let reviewer = db_slot(
        "reviewer",
        None,
        "/tmp/slot",
        serde_json::json!([]),
        serde_json::json!(["frontend"]),
        None,
    );
    assert!(reviewer.is_capable_of(&db_item_requiring(serde_json::json!(["reviewer"]))));
    assert!(reviewer.is_capable_of(&db_item_requiring(serde_json::json!(["frontend"]))));
    assert!(!reviewer.is_capable_of(&db_item_requiring(serde_json::json!(["backend"]))));
}

#[test]
fn is_capable_of_matches_model_preference_and_repo_derived_tags() {
    let worker = db_slot(
        "sub_agent",
        Some("qwen3-coder-30b"),
        "/home/lily/.forgefleet/sub-agents/sub-agent-0/forge-fleet",
        serde_json::json!([]),
        serde_json::json!([]),
        None,
    );
    assert!(worker.is_capable_of(&db_item_requiring(serde_json::json!(["qwen3-coder-30b"]))));
    assert!(!worker.is_capable_of(&db_item_requiring(serde_json::json!(["llama-70b"]))));
    assert!(worker.is_capable_of(&db_item_requiring(serde_json::json!(["forge-fleet"]))));
    assert!(!worker.is_capable_of(&db_item_requiring(serde_json::json!(["other-repo"]))));
}

#[test]
fn is_capable_of_checks_ram_tags_numerically_against_ram_gb() {
    let big_ram = db_slot(
        "sub_agent",
        None,
        "/tmp/slot",
        serde_json::json!([]),
        serde_json::json!([]),
        Some(64),
    );
    let small_ram = db_slot(
        "sub_agent",
        None,
        "/tmp/slot",
        serde_json::json!([]),
        serde_json::json!([]),
        Some(16),
    );
    let unknown_ram = db_slot(
        "sub_agent",
        None,
        "/tmp/slot",
        serde_json::json!([]),
        serde_json::json!([]),
        None,
    );

    assert!(big_ram.is_capable_of(&db_item_requiring(serde_json::json!(["ram:32"]))));
    assert!(big_ram.is_capable_of(&db_item_requiring(serde_json::json!(["ram:64"]))));
    assert!(!small_ram.is_capable_of(&db_item_requiring(serde_json::json!(["ram:32"]))));
    assert!(!unknown_ram.is_capable_of(&db_item_requiring(serde_json::json!(["ram:32"]))));
}

#[test]
fn is_capable_of_requires_every_tag_across_every_dimension() {
    let fully_capable = db_slot(
        "sub_agent",
        Some("qwen3-30b"),
        "/tmp/forge-fleet",
        serde_json::json!(["gpu"]),
        serde_json::json!(["rust"]),
        Some(64),
    );

    assert!(
        fully_capable.is_capable_of(&db_item_requiring(serde_json::json!([
            "gpu",
            "rust",
            "qwen3-30b",
            "forge-fleet",
            "ram:32"
        ])))
    );
    assert!(!fully_capable.is_capable_of(&db_item_requiring(serde_json::json!(["gpu", "macos"]))));
    assert!(
        !fully_capable.is_capable_of(&db_item_requiring(serde_json::json!(["gpu", "ram:128"])))
    );
}
