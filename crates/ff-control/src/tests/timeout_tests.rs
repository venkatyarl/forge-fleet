//! Timeout / requeue tests for the ff-control scheduler path.
//!
//! These tests use an in-memory [`CronEngine`] with a mocked [`LocalExecutor`]
//! that simulates a hanging command. They verify that:
//!
//! 1. A hanging job is recorded as `Failed`.
//! 2. The engine requeues the job for a retry (advancing `next_run_at`).
//! 3. After `retry.max_attempts` is exhausted the job returns to the regular
//!    cron schedule instead of retrying forever.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use ff_cron::dispatcher::LocalExecutor;
use ff_cron::policy::BackoffPolicy;
use ff_cron::{
    CronEngine, Dispatcher, JobDefinition, JobPriority, JobRun, JobTask, RunStatus,
    SchedulingPolicy,
};
use ff_discovery::{HealthMonitor, NodeRegistry, ScannerConfig};
use ff_orchestrator::TaskRouter;
use ff_runtime::EngineConfig;
use tokio::time::sleep;

use crate::bootstrap::{BootstrapPlan, BootstrapValidation, StartupSubsystem};
use crate::control_plane::{
    ControlPlane, ControlPlaneHandles, DeploySubsystemHandle, DiscoverySubsystemHandle,
    OrchestratorSubsystemHandle, RuntimeSubsystemHandle, SchedulerSubsystemHandle,
};

/// Build a minimal control plane around a pre-built (mocked) scheduler engine.
fn control_plane_with_engine(engine: Arc<CronEngine>) -> ControlPlane {
    let config = Arc::new(ff_core::config::FleetConfig::default());

    let handles = ControlPlaneHandles {
        discovery: DiscoverySubsystemHandle {
            registry: Arc::new(NodeRegistry::new()),
            scanner_config: ScannerConfig::default(),
            health_monitor: HealthMonitor::default(),
            last_scan_at: None,
        },
        runtime: RuntimeSubsystemHandle {
            desired_engine: EngineConfig::default(),
            last_status: None,
        },
        orchestrator: OrchestratorSubsystemHandle {
            router: Arc::new(RwLock::new(TaskRouter::new(vec![], vec![], HashMap::new()))),
        },
        scheduler: SchedulerSubsystemHandle { engine },
        deploy: DeploySubsystemHandle::default(),
    };

    let startup_plan = BootstrapPlan {
        order: StartupSubsystem::default_order(),
        validation: BootstrapValidation::default(),
        planned_at: Utc::now(),
    };

    ControlPlane {
        config,
        handles,
        startup_plan,
        startup_events: Vec::new(),
    }
}

/// Scheduling policy with a short, predictable retry backoff.
fn timeout_policy() -> SchedulingPolicy {
    SchedulingPolicy {
        quiet_hours: None,
        min_priority_during_quiet: JobPriority::Critical,
        backoff: BackoffPolicy {
            initial_delay_secs: 1,
            max_delay_secs: 5,
            multiplier: 2.0,
        },
    }
}

/// A mock executor that pretends to hang and then fails with a timeout error.
///
/// The returned closure is suitable for [`Dispatcher::with_local_executor`].
fn hanging_executor(calls: Arc<AtomicUsize>) -> LocalExecutor {
    Arc::new(move |_command: String, _timeout_secs: Option<u64>| {
        let calls = Arc::clone(&calls);
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            // Simulate a short hang so the test exercises the async timeout path
            // without slowing CI down.
            sleep(Duration::from_millis(5)).await;
            Err(io::Error::new(io::ErrorKind::TimedOut, "simulated command timed out").into())
        })
    })
}

/// Test harness that wires a hanging executor into a full [`ControlPlane`].
struct MockHarness {
    calls: Arc<AtomicUsize>,
    control_plane: ControlPlane,
}

impl MockHarness {
    fn new() -> Self {
        let calls = Arc::new(AtomicUsize::new(0));
        let dispatcher =
            Arc::new(Dispatcher::new().with_local_executor(hanging_executor(Arc::clone(&calls))));
        let engine = Arc::new(CronEngine::new(dispatcher, None, timeout_policy()));

        Self {
            calls,
            control_plane: control_plane_with_engine(engine),
        }
    }

    async fn add_job(&self, job: JobDefinition) {
        self.control_plane
            .handles
            .scheduler
            .engine
            .add_job(job)
            .await
            .expect("add_job should succeed in memory");
    }

    async fn run_due(&self, now: DateTime<Utc>) -> Vec<JobRun> {
        self.control_plane
            .handles
            .scheduler
            .engine
            .execute_due_jobs(now)
            .await
            .expect("execute_due_jobs should succeed")
    }

    fn get_job(&self, job_id: uuid::Uuid) -> Option<JobDefinition> {
        self.control_plane.handles.scheduler.engine.get_job(job_id)
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

fn hanging_job(name: &str, max_attempts: u32, now: DateTime<Utc>) -> JobDefinition {
    let mut job = JobDefinition::new(
        name,
        "* * * * *",
        JobTask::LocalCommand {
            command: "sleep 999".into(),
            timeout_secs: Some(1),
        },
        JobPriority::Normal,
    )
    .expect("'* * * * *' is a valid cron expression");

    job.retry.max_attempts = max_attempts;
    job.next_run_at = Some(now);
    job
}

#[tokio::test]
async fn hanging_job_times_out_and_requeues_until_retries_exhausted() {
    let harness = MockHarness::new();
    let now = Utc::now();

    let job = hanging_job("hang-test", 2, now);
    let job_id = job.id;
    harness.add_job(job).await;

    // First attempt: should fail and be requeued for retry.
    let runs = harness.run_due(now).await;
    assert_eq!(runs.len(), 1, "one run should be produced");
    assert_eq!(runs[0].status, RunStatus::Failed);
    assert!(
        runs[0].error.as_ref().unwrap().contains("timed out"),
        "error should indicate a timeout: {:?}",
        runs[0].error
    );

    let job_after = harness.get_job(job_id).expect("job should still exist");
    assert_eq!(job_after.consecutive_failures, 1);
    let retry_at = job_after
        .next_run_at
        .expect("job should be requeued after failure");
    assert!(retry_at > now, "retry time should be in the future");

    // Second attempt at the retry time: also fails, but retries are now exhausted.
    let runs2 = harness.run_due(retry_at).await;
    assert_eq!(runs2.len(), 1, "one retry run should be produced");
    assert_eq!(runs2[0].status, RunStatus::Failed);

    let job_after2 = harness.get_job(job_id).expect("job should still exist");
    // The last failure does not increment the counter because no retry is queued.
    assert_eq!(job_after2.consecutive_failures, 1);

    // After retries are exhausted the job should return to the normal cron
    // schedule instead of staying at the old retry time.
    let next_cron = job_after2
        .next_run_at
        .expect("job should have a next-run time");
    assert!(
        next_cron > retry_at,
        "next_run_at should advance to the next cron recurrence"
    );

    // Running again at the old retry time must not produce another attempt.
    let runs3 = harness.run_due(retry_at).await;
    assert!(
        runs3.is_empty(),
        "no runs should be produced once retries are exhausted"
    );

    // The executor should have been invoked exactly max_attempts times.
    assert_eq!(
        harness.call_count(),
        2,
        "executor should be called twice total"
    );
}
