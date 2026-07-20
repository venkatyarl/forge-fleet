//! Recovery of work owned by a node when its agent restarts.

use sqlx::PgPool;

/// Why an agent process is restarting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartReason {
    /// An expected restart performed while deploying a new ForgeFleet build.
    Deploy,
    /// An unexpected restart (crash, reboot, or failed health check).
    Failure,
}

impl RestartReason {
    /// Classify the restart reason recorded by the deploy/restart path.
    pub fn detect(reason: Option<&str>) -> Self {
        match reason.map(str::trim) {
            Some(reason) if reason.eq_ignore_ascii_case("deploy") => Self::Deploy,
            _ => Self::Failure,
        }
    }
}

/// Requeue work that was interrupted when `node` restarted.
///
/// Deploys are expected interruptions, so they must not consume a build
/// attempt. Unexpected restarts retain the existing retry accounting.
pub async fn requeue_building_items(
    pool: &PgPool,
    node: &str,
    reason: RestartReason,
) -> Result<u64, sqlx::Error> {
    let increment_attempts = attempts_increment(reason);
    let result = sqlx::query(
        "UPDATE work_items \
         SET status = 'ready', \
             attempts = COALESCE(attempts, 0) + $2, \
             assigned_computer = NULL, \
             started_at = NULL \
         WHERE status = 'building' \
           AND assigned_computer = $1",
    )
    .bind(node)
    .bind(increment_attempts)
    .execute(pool)
    .await?;

    Ok(result.rows_affected())
}

fn attempts_increment(reason: RestartReason) -> i32 {
    match reason {
        RestartReason::Deploy => 0,
        RestartReason::Failure => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deploy_restart_does_not_consume_an_attempt() {
        assert_eq!(attempts_increment(RestartReason::Deploy), 0);
    }

    #[test]
    fn unexpected_restart_consumes_an_attempt() {
        assert_eq!(attempts_increment(RestartReason::Failure), 1);
    }

    #[test]
    fn detects_deploy_restart_reason() {
        assert_eq!(RestartReason::detect(Some("deploy")), RestartReason::Deploy);
        assert_eq!(
            RestartReason::detect(Some(" DEPLOY ")),
            RestartReason::Deploy
        );
        assert_eq!(RestartReason::detect(Some("crash")), RestartReason::Failure);
        assert_eq!(RestartReason::detect(None), RestartReason::Failure);
    }
}
