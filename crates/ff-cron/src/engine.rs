use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use thiserror::Error;
use tokio::sync::watch;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::dispatcher::{DispatchError, DispatchOutcome, DispatchRequest, Dispatcher};
use crate::job::{JobDefinition, JobRun, RunStatus};
use crate::persistence::{CronPersistence, PersistenceError};
use crate::policy::SchedulingPolicy;
use crate::schedule::ScheduleError;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("job not found: {0}")]
    JobNotFound(Uuid),

    #[error("schedule error: {0}")]
    Schedule(#[from] ScheduleError),

    #[error("dispatch error: {0}")]
    Dispatch(#[from] DispatchError),

    #[error("persistence error: {0}")]
    Persistence(#[from] PersistenceError),
}

/// In-memory scheduler + execution engine.
#[derive(Clone)]
pub struct CronEngine {
    jobs: Arc<DashMap<Uuid, JobDefinition>>,
    runs: Arc<DashMap<Uuid, JobRun>>,
    dispatcher: Arc<Dispatcher>,
    persistence: Option<Arc<CronPersistence>>,
    policy: SchedulingPolicy,
    poll_interval: Duration,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
}

impl CronEngine {
    pub fn new(
        dispatcher: Arc<Dispatcher>,
        persistence: Option<Arc<CronPersistence>>,
        policy: SchedulingPolicy,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        Self {
            jobs: Arc::new(DashMap::new()),
            runs: Arc::new(DashMap::new()),
            dispatcher,
            persistence,
            policy,
            poll_interval: Duration::from_secs(5),
            shutdown_tx,
            shutdown_rx,
        }
    }

    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    pub async fn load_from_persistence(&self) -> Result<usize, EngineError> {
        let Some(persistence) = &self.persistence else {
            return Ok(0);
        };

        let jobs = persistence.list_jobs(true).await?;
        let count = jobs.len();
        for job in jobs {
            self.jobs.insert(job.id, job);
        }

        Ok(count)
    }

    pub async fn add_job(&self, mut job: JobDefinition) -> Result<Uuid, EngineError> {
        if job.next_run_at.is_none() {
            job.recompute_next_run(Utc::now())?;
        }

        let id = job.id;
        self.jobs.insert(id, job.clone());

        if let Some(persistence) = &self.persistence {
            persistence.upsert_job(&job).await?;
        }

        Ok(id)
    }

    pub async fn remove_job(&self, job_id: Uuid) -> Result<bool, EngineError> {
        let removed = self.jobs.remove(&job_id).is_some();
        if removed && let Some(persistence) = &self.persistence {
            let _ = persistence.delete_job(job_id).await?;
        }

        Ok(removed)
    }

    pub fn get_job(&self, job_id: Uuid) -> Option<JobDefinition> {
        self.jobs.get(&job_id).map(|entry| entry.value().clone())
    }

    pub fn list_jobs(&self) -> Vec<JobDefinition> {
        self.jobs
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    pub fn list_runs(&self) -> Vec<JobRun> {
        self.runs
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Execute all jobs due at the provided timestamp.
    pub async fn execute_due_jobs(&self, now: DateTime<Utc>) -> Result<Vec<JobRun>, EngineError> {
        let due_job_ids: Vec<Uuid> = self
            .jobs
            .iter()
            .filter_map(|entry| {
                let job = entry.value();
                if job.should_run_at(now) {
                    Some(*entry.key())
                } else {
                    None
                }
            })
            .collect();

        let mut executed_runs = Vec::new();

        for job_id in due_job_ids {
            if let Some(run) = self.execute_job(job_id, now).await? {
                executed_runs.push(run);
            }
        }

        Ok(executed_runs)
    }

    async fn execute_job(
        &self,
        job_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Option<JobRun>, EngineError> {
        let Some(mut job) = self.get_job(job_id) else {
            return Err(EngineError::JobNotFound(job_id));
        };

        if !self.policy.should_run(job.priority, now) {
            if let Some(deferred_until) = self.policy.defer_until(job.priority, now) {
                job.next_run_at = Some(deferred_until);
                job.updated_at = Utc::now();
                self.persist_and_replace_job(job).await?;
                debug!(job_id = %job_id, deferred_until = %deferred_until, "job deferred due to quiet-hours policy");
            }
            return Ok(None);
        }

        let scheduled_for = job.next_run_at.unwrap_or(now);
        let attempt = job.consecutive_failures.saturating_add(1);
        let mut run = JobRun::pending(job.id, scheduled_for, attempt);
        run.mark_running(job.ownership.owner_node.clone());

        self.persist_run(&run).await?;

        let request = DispatchRequest {
            job_id: job.id,
            run_id: run.id,
            task: job.task.clone(),
            attempt,
        };

        match self.dispatcher.dispatch(request).await {
            Ok(DispatchOutcome::LocalCompleted {
                output,
                duration_ms,
            }) => {
                run.mark_success(output);
                if let Some(existing) = run.output.clone() {
                    run.output = Some(format!("{}\n(duration_ms={})", existing, duration_ms));
                }

                job.note_success(now)?;
                info!(job_id = %job.id, run_id = %run.id, "cron job completed locally");
            }
            Ok(DispatchOutcome::RemoteQueued {
                task_id,
                worker_hint,
            }) => {
                run.mark_dispatched(
                    worker_hint,
                    format!("queued to ff-mesh task_id={}", task_id),
                );

                // Treat dispatch as accepted; next recurrence follows cron schedule.
                job.note_success(now)?;
                info!(job_id = %job.id, run_id = %run.id, task_id = %task_id, "cron job queued for remote execution");
            }
            Err(err) => {
                warn!(job_id = %job.id, run_id = %run.id, error = %err, "cron job dispatch failed");
                run.mark_failure(err.to_string());

                if job.can_retry() {
                    let retry_attempt = job.consecutive_failures.saturating_add(1);
                    let delay = self.policy.retry_delay(retry_attempt);
                    let retry_at = now + chrono::Duration::from_std(delay).unwrap_or_default();
                    job.note_failure_with_retry(now, retry_at);
                } else {
                    // Retries exhausted: keep the cron schedule moving.
                    job.recompute_next_run(now)?;
                }
            }
        }

        self.persist_and_replace_job(job).await?;
        self.persist_run(&run).await?;
        self.runs.insert(run.id, run.clone());

        Ok(Some(run))
    }

    async fn persist_and_replace_job(&self, job: JobDefinition) -> Result<(), EngineError> {
        let id = job.id;
        self.jobs.insert(id, job.clone());

        if let Some(persistence) = &self.persistence {
            persistence.upsert_job(&job).await?;
        }

        Ok(())
    }

    async fn persist_run(&self, run: &JobRun) -> Result<(), EngineError> {
        if let Some(persistence) = &self.persistence {
            persistence.upsert_run(run).await?;
        }
        Ok(())
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    pub fn start(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(err) = self.load_from_persistence().await {
                error!(error = %err, "failed to load cron jobs from persistence at startup");
            }

            let mut ticker = tokio::time::interval(self.poll_interval);
            let mut shutdown_rx = self.shutdown_rx.clone();

            info!(
                interval_secs = self.poll_interval.as_secs(),
                "cron engine loop started"
            );

            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        let now = Utc::now();
                        if let Err(err) = self.execute_due_jobs(now).await {
                            error!(error = %err, "cron engine tick failed");
                        }
                    }
                    _ = shutdown_rx.changed() => {
                        info!("cron engine loop shutting down");
                        break;
                    }
                }
            }
        })
    }

    pub fn cleanup_in_memory_runs(&self, keep_last: usize) -> usize {
        let mut runs: Vec<JobRun> = self
            .runs
            .iter()
            .map(|entry| entry.value().clone())
            .collect();

        runs.sort_by_key(|run| run.created_at);

        if runs.len() <= keep_last {
            return 0;
        }

        let remove_count = runs.len() - keep_last;
        for run in runs.into_iter().take(remove_count) {
            self.runs.remove(&run.id);
        }

        remove_count
    }

    pub fn mark_remote_result(
        &self,
        task_id: Uuid,
        status: RunStatus,
        output: Option<String>,
        error: Option<String>,
    ) {
        if let Some(record) = self.dispatcher.complete_remote_dispatch(task_id)
            && let Some(mut entry) = self.runs.get_mut(&record.run_id)
        {
            let run = entry.value_mut();
            run.status = status;
            run.finished_at = Some(Utc::now());
            run.output = output;
            run.error = error;

            if let Some(persistence) = self.persistence.clone() {
                let run_to_save = run.clone();
                tokio::spawn(async move {
                    if let Err(err) = persistence.upsert_run(&run_to_save).await {
                        error!(error = %err, run_id = %run_to_save.id, "failed to persist remote cron run result");
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::JobTask;
    use crate::policy::JobPriority;

    #[tokio::test]
    async fn engine_executes_due_local_job() {
        let dispatcher = Arc::new(Dispatcher::new());
        let engine = CronEngine::new(dispatcher, None, SchedulingPolicy::default());

        let mut job = JobDefinition::new(
            "test",
            "* * * * *",
            JobTask::LocalCommand {
                command: "echo hi".into(),
                timeout_secs: Some(5),
            },
            JobPriority::Normal,
        )
        .unwrap();

        let now = Utc::now();
        job.next_run_at = Some(now);

        engine.add_job(job).await.unwrap();

        let runs = engine.execute_due_jobs(now).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RunStatus::Succeeded);
    }
}
