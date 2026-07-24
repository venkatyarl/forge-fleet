use std::net::TcpListener;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

fn config(batch_size: usize) -> BatchUpdateConfig {
    BatchUpdateConfig {
        batch_size,
        health_gate: HealthGateConfig::default(),
    }
}

fn healthy_snapshot() -> HealthSnapshot {
    HealthSnapshot::new(1.0, 0.0, 10, 1.0, 1_000)
}

fn unhealthy_snapshot() -> HealthSnapshot {
    HealthSnapshot::new(0.0, 1.0, 10_000, 0.0, 1_000)
}

/// Regression test for the bug that sank the previous attempt at this
/// module: `run_batched_update` processed every node strictly one at a time,
/// so `batch_size` never bounded anything observable. Here `batch_size`
/// nodes must be in flight (health-checked/updated/restarted) *at the same
/// time* within a batch, capped at exactly `batch_size` — not 1 (fully
/// sequential) and not the whole node count (unbounded concurrency).
#[tokio::test]
async fn batch_size_caps_concurrent_node_processing() {
    let nodes: Vec<String> = (0..6).map(|i| format!("n{i}")).collect();
    let in_flight = std::sync::Arc::new(AtomicUsize::new(0));
    let max_in_flight = std::sync::Arc::new(AtomicUsize::new(0));

    let in_flight_probe = in_flight.clone();
    let max_probe = max_in_flight.clone();

    let report = run_batched_update(
        &nodes,
        &config(3),
        move |_node| {
            let in_flight = in_flight_probe.clone();
            let max_in_flight = max_probe.clone();
            async move {
                let current = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                max_in_flight.fetch_max(current, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                healthy_snapshot()
            }
        },
        |_node| async { Ok(()) },
        |_node| async { Ok(()) },
    )
    .await
    .unwrap();

    assert!(report.all_succeeded());
    assert_eq!(max_in_flight.load(Ordering::SeqCst), 3);
}

/// Same probe as above but with `batch_size` 1: this must observe no
/// overlap at all, proving the concurrency cap tracks `batch_size` in both
/// directions rather than being a fixed accident of the implementation.
#[tokio::test]
async fn batch_size_one_processes_nodes_with_no_overlap() {
    let nodes: Vec<String> = (0..4).map(|i| format!("n{i}")).collect();
    let in_flight = std::sync::Arc::new(AtomicUsize::new(0));
    let max_in_flight = std::sync::Arc::new(AtomicUsize::new(0));

    let in_flight_probe = in_flight.clone();
    let max_probe = max_in_flight.clone();

    let report = run_batched_update(
        &nodes,
        &config(1),
        move |_node| {
            let in_flight = in_flight_probe.clone();
            let max_in_flight = max_probe.clone();
            async move {
                let current = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                max_in_flight.fetch_max(current, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                healthy_snapshot()
            }
        },
        |_node| async { Ok(()) },
        |_node| async { Ok(()) },
    )
    .await
    .unwrap();

    assert!(report.all_succeeded());
    assert_eq!(max_in_flight.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn updates_and_restarts_all_healthy_nodes_across_batches() {
    let nodes = vec!["n1", "n2", "n3", "n4", "n5"];
    let updated = Mutex::new(Vec::new());
    let restarted = Mutex::new(Vec::new());

    let report = run_batched_update(
        &nodes,
        &config(2),
        |_node| async { healthy_snapshot() },
        |node| {
            updated.lock().unwrap().push(node.to_string());
            async { Ok(()) }
        },
        |node| {
            restarted.lock().unwrap().push(node.to_string());
            async { Ok(()) }
        },
    )
    .await
    .unwrap();

    assert!(report.all_succeeded());
    assert_eq!(report.outcomes.len(), 5);

    let mut updated = updated.into_inner().unwrap();
    updated.sort();
    let mut restarted = restarted.into_inner().unwrap();
    restarted.sort();
    let mut expected: Vec<String> = nodes.iter().map(|n| n.to_string()).collect();
    expected.sort();
    assert_eq!(updated, expected);
    assert_eq!(restarted, expected);
}

#[tokio::test]
async fn skips_unhealthy_node_and_does_not_update_or_restart_it() {
    let nodes = vec!["good", "bad"];
    let updated = Mutex::new(Vec::new());

    let report = run_batched_update(
        &nodes,
        &config(2),
        |node| {
            let snapshot = if node == "bad" {
                unhealthy_snapshot()
            } else {
                healthy_snapshot()
            };
            async move { snapshot }
        },
        |node| {
            updated.lock().unwrap().push(node.to_string());
            async { Ok(()) }
        },
        |_node| async { Ok(()) },
    )
    .await
    .unwrap();

    assert_eq!(*updated.lock().unwrap(), vec!["good".to_string()]);
    let bad_outcome = report
        .outcomes
        .iter()
        .find(|o| o.node == "bad")
        .expect("bad node outcome recorded");
    assert_eq!(bad_outcome.result, NodeUpdateResult::SkippedUnhealthy);
    assert!(!bad_outcome.health.passed());
    assert_eq!(report.aborted_after_batch, Some(0));
}

#[tokio::test]
async fn does_not_start_next_batch_after_a_failure() {
    let nodes = vec!["b1a", "b1b", "b2a", "b2b"];
    let touched = Mutex::new(Vec::new());

    let report = run_batched_update(
        &nodes,
        &config(2),
        |node| {
            let snapshot = if node == "b1b" {
                unhealthy_snapshot()
            } else {
                healthy_snapshot()
            };
            async move { snapshot }
        },
        |node| {
            touched.lock().unwrap().push(node.to_string());
            async { Ok(()) }
        },
        |_node| async { Ok(()) },
    )
    .await
    .unwrap();

    // Second batch (b2a, b2b) must never be touched.
    assert_eq!(*touched.lock().unwrap(), vec!["b1a".to_string()]);
    assert_eq!(report.outcomes.len(), 2);
    assert_eq!(report.aborted_after_batch, Some(0));
    assert!(!report.all_succeeded());
}

#[tokio::test]
async fn update_failure_aborts_remaining_batches() {
    let nodes = vec!["ok", "fails-update", "never-reached"];

    let report = run_batched_update(
        &nodes,
        &config(1),
        |_node| async { healthy_snapshot() },
        |node| {
            let node = node.to_string();
            async move {
                if node == "fails-update" {
                    anyhow::bail!("update failed")
                } else {
                    Ok(())
                }
            }
        },
        |_node| async { Ok(()) },
    )
    .await
    .unwrap();

    assert_eq!(report.outcomes.len(), 2);
    assert_eq!(
        report.outcomes[1].result,
        NodeUpdateResult::UpdateFailed("update failed".to_string())
    );
    assert_eq!(report.aborted_after_batch, Some(1));
}

#[tokio::test]
async fn restart_failure_is_recorded_and_aborts_remaining_batches() {
    let nodes = vec!["ok", "fails-restart", "never-reached"];

    let report = run_batched_update(
        &nodes,
        &config(1),
        |_node| async { healthy_snapshot() },
        |_node| async { Ok(()) },
        |node| {
            let node = node.to_string();
            async move {
                if node == "fails-restart" {
                    anyhow::bail!("restart failed")
                } else {
                    Ok(())
                }
            }
        },
    )
    .await
    .unwrap();

    assert_eq!(report.outcomes.len(), 2);
    assert_eq!(
        report.outcomes[1].result,
        NodeUpdateResult::RestartFailed("restart failed".to_string())
    );
    assert_eq!(report.aborted_after_batch, Some(1));
}

#[tokio::test]
async fn rejects_zero_batch_size() {
    let nodes = vec!["n1"];
    let err = run_batched_update(
        &nodes,
        &config(0),
        |_node| async { healthy_snapshot() },
        |_node| async { Ok(()) },
        |_node| async { Ok(()) },
    )
    .await
    .unwrap_err();

    assert!(err.to_string().contains("batch_size"));
}

#[tokio::test]
async fn probe_reports_healthy_snapshot_for_reachable_address() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // Accept in the background so `connect` completes instead of queuing on backlog.
    std::thread::spawn(move || {
        let _ = listener.accept();
    });

    let snapshot = probe_forgefleetd_health(&addr.to_string(), Duration::from_millis(500)).await;

    assert_eq!(snapshot.success_rate, 1.0);
    assert_eq!(snapshot.error_rate, 0.0);
    assert_eq!(snapshot.availability, 1.0);
}

#[tokio::test]
async fn probe_reports_unhealthy_snapshot_for_unreachable_address() {
    // Port 0 with an already-bound-then-closed listener is flaky across
    // platforms; instead use a documented TEST-NET address that will not
    // route, so the connect attempt reliably times out.
    let snapshot = probe_forgefleetd_health("192.0.2.1:51000", Duration::from_millis(200)).await;

    assert_eq!(snapshot.success_rate, 0.0);
    assert_eq!(snapshot.error_rate, 1.0);
    assert_eq!(snapshot.availability, 0.0);
}

#[tokio::test]
async fn update_node_checkout_resets_to_remote_ref() {
    use std::process::Command;

    fn run_git(repo: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .expect("failed to spawn git");
        assert!(status.success(), "git {} failed", args.join(" "));
    }

    let origin = tempfile::tempdir().unwrap();
    run_git(origin.path(), &["init", "-q"]);
    run_git(origin.path(), &["config", "user.email", "test@example.com"]);
    run_git(origin.path(), &["config", "user.name", "Test"]);
    std::fs::write(origin.path().join("file.txt"), "origin").unwrap();
    run_git(origin.path(), &["add", "file.txt"]);
    run_git(origin.path(), &["commit", "-m", "init", "-q"]);
    run_git(origin.path(), &["branch", "-M", "main"]);

    let local = tempfile::tempdir().unwrap();
    run_git(
        local.path(),
        &["clone", "-q", origin.path().to_str().unwrap(), "."],
    );

    update_node_checkout(local.path(), "origin/main")
        .await
        .unwrap();

    let content = std::fs::read_to_string(local.path().join("file.txt")).unwrap();
    assert_eq!(content, "origin");
}
