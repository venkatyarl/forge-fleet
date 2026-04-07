use thiserror::Error;

/// Error type for ff-control facade operations.
#[derive(Debug, Error)]
pub enum ControlError {
    #[error("bootstrap validation failed: {0}")]
    BootstrapValidation(String),

    #[error("invalid startup order: {0}")]
    InvalidStartupOrder(String),

    #[error("missing subsystem handle: {0}")]
    MissingSubsystem(&'static str),

    #[error("node not configured: {0}")]
    UnknownNode(String),

    #[error("schedule expression invalid: {0}")]
    InvalidSchedule(String),

    #[error("core error: {0}")]
    Core(#[from] ff_core::ForgeFleetError),

    #[error("discovery error: {0}")]
    Discovery(#[from] ff_discovery::DiscoveryError),

    #[error("runtime error: {0}")]
    Runtime(#[from] ff_runtime::RuntimeError),

    #[error("cron engine error: {0}")]
    CronEngine(#[from] ff_cron::EngineError),

    #[error("cron dispatch error: {0}")]
    CronDispatch(#[from] ff_cron::DispatchError),

    #[error("cron schedule error: {0}")]
    CronSchedule(#[from] ff_cron::ScheduleError),

    #[error("cron persistence error: {0}")]
    CronPersistence(#[from] ff_cron::PersistenceError),
}

pub type Result<T> = std::result::Result<T, ControlError>;
