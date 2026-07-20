use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;
use crate::config::DeployConfig;
use crate::daemon::ActiveLease;

fn config(drain_timeout: Duration) -> DeployConfig {
    DeployConfig { drain_timeout }
}

#[tokio::test]
async fn restarts_after_clean_drain() {
    let restarts = Arc::new(AtomicUsize::new(0));

    let r = restarts.clone();
    let report = restart_forgefleetd_with_drain(
        &config(Duration::from_secs(1)),
        || async { Ok::<_, anyhow::Error>(vec![]) },
        |_leases| async { Ok(()) },
        move || async move {
            r.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    )
    .await
    .unwrap();

    assert!(report.drained);
    assert!(report.requeued_item_ids.is_empty());
    assert_eq!(restarts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn requeues_then_restarts_when_drain_times_out() {
    let restarts = Arc::new(AtomicUsize::new(0));
    let requeues = Arc::new(AtomicUsize::new(0));

    let rs = restarts.clone();
    let rq = requeues.clone();
    let report = restart_forgefleetd_with_drain(
        &config(Duration::from_millis(50)),
        || async {
            Ok(vec![ActiveLease {
                lease_id: "slot-1".into(),
                work_item_ids: vec!["wi-1".into()],
            }])
        },
        move |_leases| {
            let rq = rq.clone();
            async move {
                rq.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        },
        move || async move {
            rs.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    )
    .await
    .unwrap();

    assert!(!report.drained);
    assert_eq!(report.requeued_item_ids, &["wi-1"]);
    assert_eq!(requeues.load(Ordering::SeqCst), 1);
    assert_eq!(restarts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn does_not_restart_when_drain_errors() {
    let restarts = Arc::new(AtomicUsize::new(0));

    let r = restarts.clone();
    let result = restart_forgefleetd_with_drain(
        &config(Duration::from_secs(1)),
        || async { Err::<Vec<ActiveLease>, _>(anyhow::anyhow!("lease query down")) },
        |_leases| async { Ok(()) },
        move || async move {
            r.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    )
    .await;

    assert!(result.is_err());
    assert_eq!(restarts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn drains_after_active_leases_release() {
    let restarts = Arc::new(AtomicUsize::new(0));
    let requeues = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));

    let rs = restarts.clone();
    let rq = requeues.clone();
    let cc = calls.clone();
    let report = restart_forgefleetd_with_drain(
        &config(Duration::from_secs(5)),
        move || {
            let cc = cc.clone();
            async move {
                if cc.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(vec![ActiveLease {
                        lease_id: "slot-1".into(),
                        work_item_ids: vec!["wi-1".into()],
                    }])
                } else {
                    Ok(vec![])
                }
            }
        },
        move |_leases| {
            let rq = rq.clone();
            async move {
                rq.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        },
        move || async move {
            rs.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    )
    .await
    .unwrap();

    assert!(report.drained);
    assert!(report.requeued_item_ids.is_empty());
    assert_eq!(requeues.load(Ordering::SeqCst), 0);
    assert_eq!(restarts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn requeues_all_items_without_incrementing_attempts() {
    #[derive(Debug)]
    struct ItemState {
        id: String,
        attempts: u32,
        queued: bool,
    }

    let states = Arc::new(Mutex::new(vec![
        ItemState {
            id: "wi-1".into(),
            attempts: 0,
            queued: false,
        },
        ItemState {
            id: "wi-2".into(),
            attempts: 0,
            queued: false,
        },
        ItemState {
            id: "wi-3".into(),
            attempts: 0,
            queued: false,
        },
    ]));

    let restarts = Arc::new(AtomicUsize::new(0));
    let states_for_requeue = states.clone();

    let report = restart_forgefleetd_with_drain(
        &config(Duration::from_millis(50)),
        || async {
            Ok(vec![
                ActiveLease {
                    lease_id: "slot-1".into(),
                    work_item_ids: vec!["wi-1".into(), "wi-2".into()],
                },
                ActiveLease {
                    lease_id: "slot-2".into(),
                    work_item_ids: vec!["wi-3".into()],
                },
            ])
        },
        move |leases| {
            let states = states_for_requeue.clone();
            async move {
                let mut guard = states.lock().unwrap();
                for lease in leases {
                    for id in lease.work_item_ids {
                        let item = guard.iter_mut().find(|i| i.id == id).unwrap();
                        item.queued = true;
                        // Intentionally do NOT increment item.attempts — the
                        // restart drain must be attempt-neutral.
                    }
                }
                Ok(())
            }
        },
        move || async move {
            restarts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    )
    .await
    .unwrap();

    assert!(!report.drained);
    assert_eq!(report.requeued_item_ids, &["wi-1", "wi-2", "wi-3"]);

    let guard = states.lock().unwrap();
    assert!(guard.iter().all(|i| i.queued));
    assert!(guard.iter().all(|i| i.attempts == 0));
}

#[tokio::test]
async fn does_not_restart_when_requeue_errors() {
    let restarts = Arc::new(AtomicUsize::new(0));

    let r = restarts.clone();
    let result = restart_forgefleetd_with_drain(
        &config(Duration::from_millis(50)),
        || async {
            Ok(vec![ActiveLease {
                lease_id: "slot-1".into(),
                work_item_ids: vec!["wi-1".into()],
            }])
        },
        |_leases| async { Err::<(), _>(anyhow::anyhow!("requeue down")) },
        move || async move {
            r.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    )
    .await;

    assert!(result.is_err());
    assert_eq!(restarts.load(Ordering::SeqCst), 0);
}

#[test]
fn restart_command_uses_launchctl_on_macos() {
    let cmd = forgefleetd_restart_command("macos");
    assert!(cmd.contains("launchctl kickstart -k"));
    assert!(cmd.contains("com.forgefleet.forgefleetd"));
}

#[test]
fn restart_command_uses_detached_nonblocking_systemctl_on_linux() {
    let cmd = forgefleetd_restart_command("linux");
    assert!(cmd.contains("systemctl --user restart --no-block forgefleetd.service"));
    assert!(cmd.contains("setsid"));
    assert!(cmd.contains("XDG_RUNTIME_DIR"));
}
