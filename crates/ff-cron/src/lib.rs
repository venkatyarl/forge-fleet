//! `ff-cron` — ForgeFleet scheduler and automation layer.
//!
//! This crate provides:
//! - **schedule** — Cron expression parsing + next-run calculation
//! - **job** — Job definitions, metadata, retries, ownership, run records
//! - **policy** — Quiet hours, priority policy, backoff policy
//! - **dispatcher** — Local execution or remote dispatch to ff-mesh workers
//! - **persistence** — OperationalStore-backed storage for jobs/runs (SQLite or Postgres via ff-db)
//! - **engine** — Scheduler loop that executes due jobs
//! - **heartbeat** — Periodic maintenance task runner

pub mod dispatcher;
pub mod engine;
pub mod heartbeat;
pub mod job;
pub mod persistence;
pub mod policy;
pub mod schedule;

pub use dispatcher::{
    DispatchError, DispatchOutcome, DispatchRequest, Dispatcher, RemoteDispatchRecord,
};
pub use engine::{CronEngine, EngineError};
pub use heartbeat::{HeartbeatRunner, HeartbeatTask};
pub use job::{JobDefinition, JobMetadata, JobOwnership, JobRun, JobTask, RetryPolicy, RunStatus};
pub use persistence::{CronPersistence, PersistenceError};
pub use policy::{BackoffPolicy, JobPriority, QuietHoursPolicy, SchedulingPolicy};
pub use schedule::{CronSchedule, ScheduleError};
