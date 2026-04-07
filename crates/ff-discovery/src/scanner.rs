//! Network and fleet-node scanning for ForgeFleet.
//!
//! Two scanning modes:
//! - **Subnet scan** (`scan_subnet`) — probes a /24 subnet for open ports (existing)
//! - **Fleet node scan** (`NodeScanner`) — HTTP health-checks configured fleet nodes

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::{Duration, timeout};
use tracing::{debug, trace, warn};

// ─── Subnet Scanner (existing) ───────────────────────────────────────────────

/// Configuration for subnet scanning.
#[derive(Debug, Clone)]
pub struct ScannerConfig {
    /// Subnet in CIDR notation. Currently /24 is supported (e.g. 192.168.5.0/24).
    pub subnet_cidr: String,
    /// First host octet to scan (inclusive).
    pub start_host: u8,
    /// Last host octet to scan (inclusive).
    pub end_host: u8,
    /// Ports to probe via TCP connect.
    pub known_ports: Vec<u16>,
    /// TCP connect timeout for a single probe.
    pub connect_timeout: Duration,
    /// Max number of concurrent host scans.
    pub max_concurrency: usize,
}

impl Default for ScannerConfig {
    fn default() -> Self {
        Self {
            subnet_cidr: "192.168.5.0/24".to_string(),
            start_host: 1,
            end_host: 254,
            known_ports: crate::ports::known_service_ports(),
            connect_timeout: Duration::from_millis(350),
            max_concurrency: 64,
        }
    }
}

/// Node discovered from a subnet scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredNode {
    pub ip: IpAddr,
    pub open_ports: Vec<u16>,
    pub discovered_at: DateTime<Utc>,
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("invalid subnet cidr: {0}")]
    InvalidCidr(String),
    #[error("unsupported subnet mask: {0} (only /24 is currently supported)")]
    UnsupportedMask(String),
}

/// Scan a subnet for nodes by probing known ports.
pub async fn scan_subnet(config: &ScannerConfig) -> Result<Vec<DiscoveredNode>, DiscoveryError> {
    let prefix = parse_cidr_prefix(&config.subnet_cidr)?;
    let semaphore = Arc::new(Semaphore::new(config.max_concurrency.max(1)));
    let mut tasks = JoinSet::new();

    debug!(
        subnet = %config.subnet_cidr,
        start = config.start_host,
        end = config.end_host,
        ports = ?config.known_ports,
        "starting subnet scan"
    );

    for host in config.start_host..=config.end_host {
        let permit = semaphore.clone().acquire_owned().await;
        let ports = config.known_ports.clone();
        let timeout = config.connect_timeout;

        if let Ok(permit) = permit {
            tasks.spawn(async move {
                let _permit = permit;
                let ip = IpAddr::V4(Ipv4Addr::new(prefix[0], prefix[1], prefix[2], host));
                scan_host(ip, ports, timeout).await
            });
        }
    }

    let mut discovered = Vec::new();

    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(Some(node)) => discovered.push(node),
            Ok(None) => {}
            Err(join_err) => warn!(error = %join_err, "host scan task failed"),
        }
    }

    discovered.sort_by_key(|n| n.ip);
    debug!(count = discovered.len(), "subnet scan completed");

    Ok(discovered)
}

async fn scan_host(ip: IpAddr, ports: Vec<u16>, timeout_dur: Duration) -> Option<DiscoveredNode> {
    let mut open_ports = Vec::new();

    for port in ports {
        if tcp_probe(ip, port, timeout_dur).await {
            trace!(%ip, port, "open port detected");
            open_ports.push(port);
        }
    }

    if open_ports.is_empty() {
        None
    } else {
        Some(DiscoveredNode {
            ip,
            open_ports,
            discovered_at: Utc::now(),
        })
    }
}

async fn tcp_probe(ip: IpAddr, port: u16, timeout_dur: Duration) -> bool {
    let addr = SocketAddr::new(ip, port);
    matches!(
        timeout(timeout_dur, TcpStream::connect(addr)).await,
        Ok(Ok(_))
    )
}

fn parse_cidr_prefix(cidr: &str) -> Result<[u8; 3], DiscoveryError> {
    let (ip_str, mask_str) = cidr
        .split_once('/')
        .ok_or_else(|| DiscoveryError::InvalidCidr(cidr.to_string()))?;

    if mask_str != "24" {
        return Err(DiscoveryError::UnsupportedMask(mask_str.to_string()));
    }

    let ip: Ipv4Addr = ip_str
        .parse()
        .map_err(|_| DiscoveryError::InvalidCidr(cidr.to_string()))?;
    let [a, b, c, _] = ip.octets();

    Ok([a, b, c])
}

// ─── Fleet Node Scanner ──────────────────────────────────────────────────────

/// A target node to scan, derived from fleet.toml node configuration.
#[derive(Debug, Clone)]
pub struct ScanTarget {
    /// Node name from fleet.toml (e.g. "taylor", "james").
    pub name: String,
    /// IP address or hostname.
    pub host: String,
    /// API port to health-check.
    pub port: u16,
    /// Election priority (lower = more preferred).
    pub priority: u32,
}

/// Status of a scanned fleet node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeScanStatus {
    /// Node is responding normally (HTTP 2xx, latency < degraded threshold).
    Online,
    /// Node is responding but degraded (slow response or HTTP 5xx).
    Degraded,
    /// Node is not responding (connection refused, timeout, etc.).
    Offline,
}

impl std::fmt::Display for NodeScanStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Online => write!(f, "Online"),
            Self::Degraded => write!(f, "Degraded"),
            Self::Offline => write!(f, "Offline"),
        }
    }
}

/// Result of scanning a single fleet node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeScanResult {
    /// Node name from fleet.toml.
    pub name: String,
    /// IP address or hostname.
    pub host: String,
    /// Port that was checked.
    pub port: u16,
    /// Resolved status.
    pub status: NodeScanStatus,
    /// Response latency in milliseconds.
    pub latency_ms: u128,
    /// When the scan was performed.
    pub scanned_at: DateTime<Utc>,
    /// HTTP status code if a response was received.
    pub http_status: Option<u16>,
    /// Error message if the check failed.
    pub error: Option<String>,
}

/// Latency threshold (ms) above which a node is considered degraded.
const DEFAULT_DEGRADED_THRESHOLD_MS: u128 = 1500;

/// Default HTTP timeout for health checks.
const DEFAULT_HEALTH_TIMEOUT: Duration = Duration::from_secs(3);

/// Scans configured fleet nodes via HTTP GET to `/health`.
///
/// Use [`NodeScanner::new`] with targets derived from fleet.toml,
/// then call [`NodeScanner::scan_once`] or [`NodeScanner::run_periodic`].
pub struct NodeScanner {
    targets: Vec<ScanTarget>,
    client: reqwest::Client,
    degraded_threshold_ms: u128,
}

impl NodeScanner {
    /// Create a new scanner for the given targets.
    pub fn new(targets: Vec<ScanTarget>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(DEFAULT_HEALTH_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            targets,
            client,
            degraded_threshold_ms: DEFAULT_DEGRADED_THRESHOLD_MS,
        }
    }

    /// Create a scanner with a custom HTTP timeout.
    pub fn with_timeout(targets: Vec<ScanTarget>, health_timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(health_timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            targets,
            client,
            degraded_threshold_ms: DEFAULT_DEGRADED_THRESHOLD_MS,
        }
    }

    /// Set the latency threshold (ms) above which nodes are flagged as degraded.
    pub fn set_degraded_threshold(&mut self, threshold_ms: u128) {
        self.degraded_threshold_ms = threshold_ms;
    }

    /// Get the current scan targets.
    pub fn targets(&self) -> &[ScanTarget] {
        &self.targets
    }

    /// Replace the scan target list (e.g. after a config reload).
    pub fn set_targets(&mut self, targets: Vec<ScanTarget>) {
        self.targets = targets;
    }

    /// Scan all configured targets concurrently.
    ///
    /// Returns one [`NodeScanResult`] per target, sorted by name.
    pub async fn scan_once(&self) -> Vec<NodeScanResult> {
        let mut tasks = JoinSet::new();

        for target in &self.targets {
            let client = self.client.clone();
            let target = target.clone();
            let degraded_threshold = self.degraded_threshold_ms;

            tasks
                .spawn(async move { check_fleet_node(&client, &target, degraded_threshold).await });
        }

        let mut results = Vec::with_capacity(self.targets.len());
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(scan_result) => results.push(scan_result),
                Err(err) => warn!(error = %err, "node scan task panicked"),
            }
        }

        results.sort_by(|a, b| a.name.cmp(&b.name));
        results
    }

    /// Run periodic scans forever, calling `on_results` after each round.
    pub async fn run_periodic<F, Fut>(&self, interval: Duration, mut on_results: F)
    where
        F: FnMut(Vec<NodeScanResult>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        loop {
            let results = self.scan_once().await;
            on_results(results).await;
            tokio::time::sleep(interval).await;
        }
    }
}

/// Perform an HTTP GET to `http://{host}:{port}/health` and classify the result.
async fn check_fleet_node(
    client: &reqwest::Client,
    target: &ScanTarget,
    degraded_threshold_ms: u128,
) -> NodeScanResult {
    let url = format!(
        "http://{}:{}{}",
        target.host,
        target.port,
        crate::ports::HEALTH_PATH
    );
    let started = Instant::now();
    let scanned_at = Utc::now();

    match client.get(&url).send().await {
        Ok(response) => {
            let latency_ms = started.elapsed().as_millis();
            let http_status = response.status().as_u16();

            let status = if response.status().is_success() {
                if latency_ms > degraded_threshold_ms {
                    NodeScanStatus::Degraded
                } else {
                    NodeScanStatus::Online
                }
            } else if response.status().is_server_error() {
                NodeScanStatus::Degraded
            } else {
                // 4xx → treat as offline (not a real fleet node)
                NodeScanStatus::Offline
            };

            debug!(
                node = %target.name,
                latency_ms,
                http_status,
                %status,
                "fleet node scanned"
            );

            NodeScanResult {
                name: target.name.clone(),
                host: target.host.clone(),
                port: target.port,
                status,
                latency_ms,
                scanned_at,
                http_status: Some(http_status),
                error: None,
            }
        }
        Err(err) => {
            let latency_ms = started.elapsed().as_millis();
            debug!(
                node = %target.name,
                latency_ms,
                error = %err,
                "fleet node unreachable"
            );

            NodeScanResult {
                name: target.name.clone(),
                host: target.host.clone(),
                port: target.port,
                status: NodeScanStatus::Offline,
                latency_ms,
                scanned_at,
                http_status: None,
                error: Some(err.to_string()),
            }
        }
    }
}

/// Build [`ScanTarget`]s from fleet config node entries.
///
/// Accepts an iterator of `(name, ip, port, priority)` tuples — the caller
/// extracts these from [`ff_core::config::FleetConfig`] so this crate stays
/// independent of `ff-core`.
pub fn build_scan_targets<I, S>(nodes: I, default_port: u16) -> Vec<ScanTarget>
where
    I: IntoIterator<Item = (S, S, Option<u16>, u32)>,
    S: Into<String>,
{
    nodes
        .into_iter()
        .map(|(name, host, port, priority)| ScanTarget {
            name: name.into(),
            host: host.into(),
            port: port.unwrap_or(default_port),
            priority,
        })
        .collect()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    /// Start a minimal HTTP server that returns a fixed status code.
    async fn mock_http_server(status_code: u16, delay_ms: u64) -> (u16, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let delay = delay_ms;
                let status = status_code;
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf).await;
                    if delay > 0 {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }
                    let reason = match status {
                        200 => "OK",
                        500 => "Internal Server Error",
                        _ => "Unknown",
                    };
                    let body = r#"{"status":"ok"}"#;
                    let response = format!(
                        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\n\r\n{}",
                        status,
                        reason,
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });

        // Give the listener a moment to start.
        tokio::time::sleep(Duration::from_millis(20)).await;
        (port, handle)
    }

    #[tokio::test]
    async fn test_scan_online_node() {
        let (port, _server) = mock_http_server(200, 0).await;

        let scanner = NodeScanner::new(vec![ScanTarget {
            name: "test-node".into(),
            host: "127.0.0.1".into(),
            port,
            priority: 1,
        }]);

        let results = scanner.scan_once().await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "test-node");
        assert_eq!(results[0].status, NodeScanStatus::Online);
        assert_eq!(results[0].http_status, Some(200));
        assert!(results[0].error.is_none());
    }

    #[tokio::test]
    async fn test_scan_offline_node() {
        // Use a port nothing is listening on.
        let scanner = NodeScanner::with_timeout(
            vec![ScanTarget {
                name: "dead-node".into(),
                host: "127.0.0.1".into(),
                port: 59999,
                priority: 50,
            }],
            Duration::from_secs(1),
        );

        let results = scanner.scan_once().await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, NodeScanStatus::Offline);
        assert!(results[0].error.is_some());
    }

    #[tokio::test]
    async fn test_scan_degraded_node_by_status() {
        let (port, _server) = mock_http_server(500, 0).await;

        let scanner = NodeScanner::new(vec![ScanTarget {
            name: "sick-node".into(),
            host: "127.0.0.1".into(),
            port,
            priority: 10,
        }]);

        let results = scanner.scan_once().await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, NodeScanStatus::Degraded);
        assert_eq!(results[0].http_status, Some(500));
    }

    #[tokio::test]
    async fn test_scan_degraded_node_by_latency() {
        // Server responds after 200ms, but we set degraded threshold to 50ms.
        let (port, _server) = mock_http_server(200, 200).await;

        let mut scanner = NodeScanner::new(vec![ScanTarget {
            name: "slow-node".into(),
            host: "127.0.0.1".into(),
            port,
            priority: 10,
        }]);
        scanner.set_degraded_threshold(50); // 50ms threshold

        let results = scanner.scan_once().await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, NodeScanStatus::Degraded);
        assert!(results[0].latency_ms >= 100); // at least 100ms
    }

    #[tokio::test]
    async fn test_scan_multiple_nodes() {
        let (port1, _s1) = mock_http_server(200, 0).await;
        let (port2, _s2) = mock_http_server(200, 0).await;

        let scanner = NodeScanner::new(vec![
            ScanTarget {
                name: "alpha".into(),
                host: "127.0.0.1".into(),
                port: port1,
                priority: 1,
            },
            ScanTarget {
                name: "beta".into(),
                host: "127.0.0.1".into(),
                port: port2,
                priority: 2,
            },
        ]);

        let results = scanner.scan_once().await;
        assert_eq!(results.len(), 2);
        // Sorted by name
        assert_eq!(results[0].name, "alpha");
        assert_eq!(results[1].name, "beta");
        assert!(results.iter().all(|r| r.status == NodeScanStatus::Online));
    }

    #[tokio::test]
    async fn test_build_scan_targets() {
        let targets = build_scan_targets(
            vec![
                ("taylor", "192.168.5.100", Some(51800u16), 1u32),
                ("james", "192.168.5.101", None, 2),
            ],
            51800,
        );

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].name, "taylor");
        assert_eq!(targets[0].port, 51800);
        assert_eq!(targets[1].name, "james");
        assert_eq!(targets[1].port, 51800); // used default
    }

    #[test]
    fn test_node_scan_status_display() {
        assert_eq!(NodeScanStatus::Online.to_string(), "Online");
        assert_eq!(NodeScanStatus::Degraded.to_string(), "Degraded");
        assert_eq!(NodeScanStatus::Offline.to_string(), "Offline");
    }

    #[tokio::test]
    async fn test_scan_timeout_returns_offline() {
        // Server delays 2s, but scanner times out at 500ms.
        let (port, _server) = mock_http_server(200, 2000).await;

        let scanner = NodeScanner::with_timeout(
            vec![ScanTarget {
                name: "timeout-node".into(),
                host: "127.0.0.1".into(),
                port,
                priority: 5,
            }],
            Duration::from_millis(500),
        );

        let results = scanner.scan_once().await;
        assert_eq!(results.len(), 1);
        // Should be Offline due to timeout
        assert_eq!(results[0].status, NodeScanStatus::Offline);
        assert!(results[0].error.is_some());
    }

    #[tokio::test]
    async fn test_set_targets() {
        let mut scanner = NodeScanner::new(vec![]);
        assert!(scanner.targets().is_empty());

        scanner.set_targets(vec![ScanTarget {
            name: "new-node".into(),
            host: "127.0.0.1".into(),
            port: 8080,
            priority: 1,
        }]);
        assert_eq!(scanner.targets().len(), 1);
        assert_eq!(scanner.targets()[0].name, "new-node");
    }
}
