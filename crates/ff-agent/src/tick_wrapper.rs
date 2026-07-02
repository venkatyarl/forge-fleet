//! Panic-isolating wrapper for periodic daemon ticks.
//!
//! A panic in one background tick should be reported as a failed tick, not take
//! down the whole daemon. This wrapper captures both ordinary `Err` returns and
//! panics as data, while preserving the elapsed time for operational logs/tests.

use std::any::Any;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

/// Error-side outcome from a tick wrapped by [`PanicIsolatingWrapper`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TickOutcome {
    /// The tick returned an ordinary error.
    #[error("tick failed after {duration:?}: {error}")]
    Failed {
        /// Wall-clock duration until the error was observed.
        duration: Duration,
        /// Stringified error returned by the tick body.
        error: String,
    },
    /// The tick panicked while constructing or polling its future.
    #[error("tick panicked after {duration:?}: {panic}")]
    Panicked {
        /// Wall-clock duration until the panic was observed.
        duration: Duration,
        /// Best-effort panic payload rendered as a string.
        panic: String,
    },
}

impl TickOutcome {
    /// Duration captured for this failed tick.
    pub fn duration(&self) -> Duration {
        match self {
            Self::Failed { duration, .. } | Self::Panicked { duration, .. } => *duration,
        }
    }

    /// Human-readable failure message.
    pub fn message(&self) -> &str {
        match self {
            Self::Failed { error, .. } => error,
            Self::Panicked { panic, .. } => panic,
        }
    }
}

/// Wraps async tick closures so ordinary errors and panics are both returned as
/// [`TickOutcome`] instead of escaping the tick loop.
#[derive(Debug, Default, Clone, Copy)]
pub struct PanicIsolatingWrapper;

impl PanicIsolatingWrapper {
    /// Construct a new wrapper.
    pub const fn new() -> Self {
        Self
    }

    /// Run an async tick closure with panic isolation and duration capture.
    ///
    /// Panics are caught both while invoking the closure and while polling the
    /// returned future. This matters for `async` closures because most body
    /// code runs during polling, not at construction time.
    pub async fn wrap<F, Fut, E>(&self, tick: F) -> Result<(), TickOutcome>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(), E>>,
        E: std::fmt::Display,
    {
        let started = Instant::now();

        let future = match catch_unwind(AssertUnwindSafe(|| tick())) {
            Ok(future) => future,
            Err(payload) => {
                return Err(TickOutcome::Panicked {
                    duration: started.elapsed(),
                    panic: panic_payload_to_string(payload),
                });
            }
        };

        match CatchUnwindFuture::new(future).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(TickOutcome::Failed {
                duration: started.elapsed(),
                error: error.to_string(),
            }),
            Err(payload) => Err(TickOutcome::Panicked {
                duration: started.elapsed(),
                panic: panic_payload_to_string(payload),
            }),
        }
    }

    /// Alias for [`Self::wrap`].
    pub async fn run<F, Fut, E>(&self, tick: F) -> Result<(), TickOutcome>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(), E>>,
        E: std::fmt::Display,
    {
        self.wrap(tick).await
    }
}

struct CatchUnwindFuture<F> {
    inner: F,
}

impl<F> CatchUnwindFuture<F> {
    fn new(inner: F) -> Self {
        Self { inner }
    }
}

impl<F> Future for CatchUnwindFuture<F>
where
    F: Future,
{
    type Output = std::thread::Result<F::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: `inner` is pinned in place as part of `self`, and we never move
        // it out through this projection.
        let inner = unsafe { self.map_unchecked_mut(|this| &mut this.inner) };
        match catch_unwind(AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(payload) => Poll::Ready(Err(payload)),
        }
    }
}

fn panic_payload_to_string(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}
