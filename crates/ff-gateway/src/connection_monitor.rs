//! Gateway health-endpoint connection monitoring.

use std::time::Duration;

use tokio::{sync::watch, task::JoinHandle};
use tokio_util::sync::CancellationToken;

/// Current reachability of the monitored endpoint.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ConnectionState {
    /// No health probe has completed yet.
    #[default]
    Unknown,
    /// The most recent health probe returned a successful HTTP status.
    Connected,
    /// The most recent health probe failed or returned a non-success status.
    Disconnected,
}

/// Periodically probes a health endpoint and publishes state transitions.
pub struct ConnectionMonitor {
    client: reqwest::Client,
    health_url: String,
    interval: Duration,
    state_tx: watch::Sender<ConnectionState>,
}

impl ConnectionMonitor {
    /// Create a monitor and a receiver for its state changes.
    pub fn new(
        health_url: impl Into<String>,
        interval: Duration,
        request_timeout: Duration,
    ) -> Result<(Self, watch::Receiver<ConnectionState>), reqwest::Error> {
        let client = reqwest::Client::builder()
            .timeout(request_timeout)
            .build()?;
        let (state_tx, state_rx) = watch::channel(ConnectionState::Unknown);

        Ok((
            Self {
                client,
                health_url: health_url.into(),
                interval,
                state_tx,
            },
            state_rx,
        ))
    }

    /// Subscribe to future state changes while retaining the latest state.
    pub fn subscribe(&self) -> watch::Receiver<ConnectionState> {
        self.state_tx.subscribe()
    }

    /// Spawn the monitor on the current Tokio runtime.
    ///
    /// The first probe runs immediately. The task exits when `cancel` is cancelled.
    pub fn spawn(self, cancel: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(self.run(cancel))
    }

    async fn run(self, cancel: CancellationToken) {
        let mut ticker = tokio::time::interval(self.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                _ = ticker.tick() => {
                    let state = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => break,
                        result = self.client.get(&self.health_url).send() => {
                            match result {
                                Ok(response) if response.status().is_success() => ConnectionState::Connected,
                                _ => ConnectionState::Disconnected,
                            }
                        }
                    };

                    self.state_tx.send_if_modified(|current| {
                        if *current == state {
                            false
                        } else {
                            *current = state;
                            true
                        }
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, http::StatusCode, routing::get};

    async fn health_server(status: StatusCode) -> String {
        let app = Router::new().route("/health", get(move || async move { status }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}/health")
    }

    #[tokio::test]
    async fn publishes_connected_after_immediate_successful_probe() {
        let url = health_server(StatusCode::NO_CONTENT).await;
        let (monitor, mut state) =
            ConnectionMonitor::new(url, Duration::from_secs(60), Duration::from_secs(1)).unwrap();
        let cancel = CancellationToken::new();
        let task = monitor.spawn(cancel.clone());

        state.changed().await.unwrap();
        assert_eq!(*state.borrow(), ConnectionState::Connected);

        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn publishes_disconnected_for_unsuccessful_response() {
        let url = health_server(StatusCode::SERVICE_UNAVAILABLE).await;
        let (monitor, mut state) =
            ConnectionMonitor::new(url, Duration::from_secs(60), Duration::from_secs(1)).unwrap();
        let cancel = CancellationToken::new();
        let task = monitor.spawn(cancel.clone());

        state.changed().await.unwrap();
        assert_eq!(*state.borrow(), ConnectionState::Disconnected);

        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_stops_an_in_flight_probe() {
        let (monitor, _state) = ConnectionMonitor::new(
            "http://192.0.2.1/health",
            Duration::from_secs(60),
            Duration::from_secs(60),
        )
        .unwrap();
        let cancel = CancellationToken::new();
        let task = monitor.spawn(cancel.clone());

        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("monitor should stop promptly")
            .unwrap();
    }
}
