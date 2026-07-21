//! Process-wide concurrency limits shared by dispatch paths.

use tokio::sync::{Semaphore, SemaphorePermit};

/// The 480B deployment is configured for at most two parallel requests.
pub const MAX_480B_CONCURRENCY: usize = 2;

static LANE_480B_SEMAPHORE: Semaphore = Semaphore::const_new(MAX_480B_CONCURRENCY);

/// Acquire capacity on the shared 480B lane.
pub async fn acquire_480b_permit() -> Result<SemaphorePermit<'static>, tokio::sync::AcquireError> {
    LANE_480B_SEMAPHORE.acquire().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shared_480b_lane_is_capped_at_two_requests() {
        let first = acquire_480b_permit().await.expect("first permit");
        let second = acquire_480b_permit().await.expect("second permit");
        assert!(LANE_480B_SEMAPHORE.try_acquire().is_err());
        drop((first, second));
    }
}
