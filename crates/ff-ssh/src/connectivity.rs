use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::SshNodeConfig;
use crate::connection::{SshConnection, SshConnectionError, SshConnectionOptions};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectivityStatus {
    Success,
    AuthDenied,
    Timeout,
    Refused,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConnectivityResult {
    pub node: String,
    pub host: String,
    pub checked_at: DateTime<Utc>,
    pub latency_ms: Option<u128>,
    pub status: ConnectivityStatus,
    pub details: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectivityMatrix {
    pub checked_at: DateTime<Utc>,
    pub results: Vec<NodeConnectivityResult>,
}

impl ConnectivityMatrix {
    pub fn success_count(&self) -> usize {
        self.results
            .iter()
            .filter(|r| matches!(r.status, ConnectivityStatus::Success))
            .count()
    }

    pub fn failure_count(&self) -> usize {
        self.results.len().saturating_sub(self.success_count())
    }
}

/// Fleet connectivity checker.
#[derive(Debug, Clone)]
pub struct ConnectivityChecker {
    probe_command: String,
    timeout_secs: u64,
}

impl Default for ConnectivityChecker {
    fn default() -> Self {
        Self {
            probe_command: "echo forgefleet_ssh_ok".to_string(),
            timeout_secs: 8,
        }
    }
}

impl ConnectivityChecker {
    pub fn new(probe_command: impl Into<String>, timeout_secs: u64) -> Self {
        Self {
            probe_command: probe_command.into(),
            timeout_secs,
        }
    }

    /// Check connectivity for all nodes in parallel.
    pub async fn check_all(&self, nodes: &[SshNodeConfig]) -> ConnectivityMatrix {
        let checked_at = Utc::now();
        let mut handles = Vec::with_capacity(nodes.len());

        for node in nodes {
            let checker = self.clone();
            let node = node.clone();
            handles.push(tokio::spawn(async move { checker.check_node(&node).await }));
        }

        let mut results = Vec::with_capacity(nodes.len());
        for handle in handles {
            if let Ok(result) = handle.await {
                results.push(result);
            }
        }

        ConnectivityMatrix {
            checked_at,
            results,
        }
    }

    /// Check one node, trying primary host and then alternates.
    pub async fn check_node(&self, node: &SshNodeConfig) -> NodeConnectivityResult {
        let mut last_failure = None;

        for candidate_host in node.candidate_hosts() {
            let mut candidate = node.clone();
            candidate.host = candidate_host.to_string();

            let now = Utc::now();
            let started = Instant::now();

            match self.probe_candidate(&candidate) {
                Ok(()) => {
                    return NodeConnectivityResult {
                        node: node.name.clone(),
                        host: candidate.host,
                        checked_at: now,
                        latency_ms: Some(started.elapsed().as_millis()),
                        status: ConnectivityStatus::Success,
                        details: None,
                    };
                }
                Err((status, details)) => {
                    last_failure = Some(NodeConnectivityResult {
                        node: node.name.clone(),
                        host: candidate.host,
                        checked_at: now,
                        latency_ms: Some(started.elapsed().as_millis()),
                        status,
                        details: Some(details),
                    });
                }
            }
        }

        last_failure.unwrap_or(NodeConnectivityResult {
            node: node.name.clone(),
            host: node.host.clone(),
            checked_at: Utc::now(),
            latency_ms: None,
            status: ConnectivityStatus::Unknown,
            details: Some("no candidates to probe".to_string()),
        })
    }

    fn probe_candidate(&self, node: &SshNodeConfig) -> Result<(), (ConnectivityStatus, String)> {
        let mut options = SshConnectionOptions::from_node(node);
        options.command_timeout_secs = Some(self.timeout_secs);
        options.connect_timeout_secs = Some(self.timeout_secs);

        let connection = SshConnection::new(options);
        match connection.execute(&self.probe_command) {
            Ok(output) if output.success => Ok(()),
            Ok(output) => {
                let status = classify_failure(&output.stderr);
                Err((status, output.stderr))
            }
            Err(err) => Err(classify_transport_error(err)),
        }
    }
}

fn classify_transport_error(err: SshConnectionError) -> (ConnectivityStatus, String) {
    match err {
        SshConnectionError::TimedOut { .. } => (ConnectivityStatus::Timeout, err.to_string()),
        other => {
            let status = classify_failure(&other.to_string());
            (status, other.to_string())
        }
    }
}

fn classify_failure(stderr: &str) -> ConnectivityStatus {
    let s = stderr.to_ascii_lowercase();

    if s.contains("permission denied") || s.contains("authentication failed") {
        ConnectivityStatus::AuthDenied
    } else if s.contains("connection timed out") || s.contains("operation timed out") {
        ConnectivityStatus::Timeout
    } else if s.contains("connection refused") {
        ConnectivityStatus::Refused
    } else {
        ConnectivityStatus::Unknown
    }
}
