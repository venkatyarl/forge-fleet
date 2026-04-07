//! Health monitoring and alert condition generation for ForgeFleet.
//!
//! This module provides stateful monitors for:
//! - Node health endpoints
//! - Model (llama-server) endpoint health
//! - Disk usage thresholds
//!
//! Alerts are emitted only on state transitions to avoid noisy repeated polling alerts.

use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::config::FleetConfig;
use crate::error::{ForgeFleetError, Result};
use crate::notifications::{NotificationLevel, NotificationSender};

fn default_check_interval() -> Duration {
    Duration::from_secs(60)
}

#[derive(Debug, Clone)]
pub struct MonitorSettings {
    pub check_interval: Duration,
    pub request_timeout: Duration,
    pub node_health_path: String,
}

impl Default for MonitorSettings {
    fn default() -> Self {
        Self {
            check_interval: default_check_interval(),
            request_timeout: Duration::from_secs(5),
            node_health_path: "/health".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    Online,
    Offline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelState {
    Healthy,
    Degraded,
    Offline,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DiskState {
    Healthy,
    Warning,
    Critical,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AlertCondition {
    NodeWentOffline {
        node: String,
    },
    NodeCameBackOnline {
        node: String,
    },
    ModelEndpointStoppedResponding {
        node: String,
        model: String,
        endpoint: String,
    },
    ModelEndpointDegraded {
        node: String,
        model: String,
        endpoint: String,
    },
    LeaderFailoverOccurred {
        from: String,
        to: String,
    },
    SelfUpdateFailed {
        node: String,
        reason: String,
    },
    DiskUsageCritical {
        node: String,
        mount: String,
        percent_used: f64,
    },
}

#[derive(Debug, Clone)]
pub struct MonitorAlert {
    pub condition: AlertCondition,
    pub level: NotificationLevel,
    pub title: String,
    pub body: String,
    pub event_key: String,
}

impl MonitorAlert {
    pub fn to_notification_parts(&self) -> (NotificationLevel, &str, &str) {
        (self.level, &self.title, &self.body)
    }
}

#[derive(Clone)]
pub struct NodeMonitor {
    client: reqwest::Client,
    settings: MonitorSettings,
    state: Arc<Mutex<HashMap<String, NodeState>>>,
}

#[derive(Clone)]
pub struct ModelMonitor {
    client: reqwest::Client,
    settings: MonitorSettings,
    state: Arc<Mutex<HashMap<String, ModelState>>>,
}

#[derive(Clone)]
pub struct DiskMonitor {
    settings: MonitorSettings,
    warn_threshold: f64,
    critical_threshold: f64,
    state: Arc<Mutex<HashMap<String, DiskState>>>,
}

#[derive(Debug, Clone)]
pub struct DiskUsageSample {
    pub mount: String,
    pub percent_used: f64,
}

#[derive(Clone)]
pub struct FleetMonitor<S: NotificationSender> {
    sender: Arc<S>,
    pub node_monitor: NodeMonitor,
    pub model_monitor: ModelMonitor,
    pub disk_monitor: DiskMonitor,
}

impl NodeMonitor {
    pub fn new(settings: MonitorSettings) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(settings.request_timeout)
            .build()
            .map_err(|error| {
                ForgeFleetError::Runtime(format!("failed to build reqwest client: {error}"))
            })?;

        Ok(Self {
            client,
            settings,
            state: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn check_interval(&self) -> Duration {
        self.settings.check_interval
    }

    /// Poll all configured nodes and return transition alerts.
    pub async fn poll(&self, config: &FleetConfig) -> Vec<MonitorAlert> {
        let mut alerts = Vec::new();

        for (node_name, node) in &config.nodes {
            let port = node.port.unwrap_or(config.fleet.api_port);
            let url = format!(
                "http://{}:{}{}",
                node.ip,
                port,
                normalize_path(&self.settings.node_health_path)
            );

            let is_up = probe_online(&self.client, &url).await;
            let current = if is_up {
                NodeState::Online
            } else {
                NodeState::Offline
            };

            let previous = {
                let mut state = self.state.lock().await;
                state.insert(node_name.clone(), current)
            };

            if let Some(prev) = previous
                && prev != current
            {
                match current {
                    NodeState::Offline => alerts.push(MonitorAlert {
                        condition: AlertCondition::NodeWentOffline {
                            node: node_name.clone(),
                        },
                        level: NotificationLevel::Critical,
                        title: format!("Node offline: {node_name}"),
                        body: format!("{node_name} at {} stopped responding at {url}", node.ip),
                        event_key: format!("node_offline:{node_name}"),
                    }),
                    NodeState::Online => alerts.push(MonitorAlert {
                        condition: AlertCondition::NodeCameBackOnline {
                            node: node_name.clone(),
                        },
                        level: NotificationLevel::Info,
                        title: format!("Node recovered: {node_name}"),
                        body: format!("{node_name} is responding again at {url}"),
                        event_key: format!("node_online:{node_name}"),
                    }),
                }
            }
        }

        alerts
    }
}

impl ModelMonitor {
    pub fn new(settings: MonitorSettings) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(settings.request_timeout)
            .build()
            .map_err(|error| {
                ForgeFleetError::Runtime(format!("failed to build reqwest client: {error}"))
            })?;

        Ok(Self {
            client,
            settings,
            state: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn check_interval(&self) -> Duration {
        self.settings.check_interval
    }

    /// Poll all configured model endpoints and emit transition alerts.
    pub async fn poll(&self, config: &FleetConfig) -> Vec<MonitorAlert> {
        let mut alerts = Vec::new();

        for (node_name, node) in &config.nodes {
            for (model_slug, model) in &node.models {
                let Some(port) = model.port else {
                    continue;
                };

                let endpoint = format!("http://{}:{port}/health", node.ip);
                let current = probe_endpoint_state(&self.client, &endpoint).await;
                let key = format!("{node_name}:{model_slug}:{port}");

                let previous = {
                    let mut state = self.state.lock().await;
                    state.insert(key.clone(), current)
                };

                if let Some(prev) = previous
                    && prev != current
                {
                    match current {
                        ModelState::Offline => alerts.push(MonitorAlert {
                            condition: AlertCondition::ModelEndpointStoppedResponding {
                                node: node_name.clone(),
                                model: model_slug.clone(),
                                endpoint: endpoint.clone(),
                            },
                            level: NotificationLevel::Critical,
                            title: format!("Model endpoint offline: {node_name}/{model_slug}"),
                            body: format!(
                                "Model '{}' on node '{}' is unreachable at {endpoint}",
                                model.name, node_name
                            ),
                            event_key: format!("model_offline:{node_name}:{model_slug}"),
                        }),
                        ModelState::Degraded => alerts.push(MonitorAlert {
                            condition: AlertCondition::ModelEndpointDegraded {
                                node: node_name.clone(),
                                model: model_slug.clone(),
                                endpoint: endpoint.clone(),
                            },
                            level: NotificationLevel::Warning,
                            title: format!("Model degraded: {node_name}/{model_slug}"),
                            body: format!(
                                "Model '{}' on node '{}' returned unhealthy status at {endpoint}",
                                model.name, node_name
                            ),
                            event_key: format!("model_degraded:{node_name}:{model_slug}"),
                        }),
                        ModelState::Healthy => alerts.push(MonitorAlert {
                            condition: AlertCondition::ModelEndpointDegraded {
                                node: node_name.clone(),
                                model: model_slug.clone(),
                                endpoint: endpoint.clone(),
                            },
                            level: NotificationLevel::Info,
                            title: format!("Model recovered: {node_name}/{model_slug}"),
                            body: format!(
                                "Model '{}' on node '{}' is healthy again at {endpoint}",
                                model.name, node_name
                            ),
                            event_key: format!("model_healthy:{node_name}:{model_slug}"),
                        }),
                    }
                }
            }
        }

        alerts
    }
}

impl DiskMonitor {
    pub fn new(settings: MonitorSettings) -> Self {
        Self {
            settings,
            warn_threshold: 80.0,
            critical_threshold: 90.0,
            state: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_thresholds(mut self, warn_threshold: f64, critical_threshold: f64) -> Self {
        self.warn_threshold = warn_threshold;
        self.critical_threshold = critical_threshold;
        self
    }

    pub fn check_interval(&self) -> Duration {
        self.settings.check_interval
    }

    /// Poll local disk usage and emit alerts on state transitions only.
    pub async fn poll_local(&self, node_name: &str) -> Result<Vec<MonitorAlert>> {
        let samples = read_local_disk_usage()?;
        Ok(self.evaluate_samples(node_name, &samples).await)
    }

    async fn evaluate_samples(
        &self,
        node_name: &str,
        samples: &[DiskUsageSample],
    ) -> Vec<MonitorAlert> {
        let mut alerts = Vec::new();

        for sample in samples {
            let current = if sample.percent_used >= self.critical_threshold {
                DiskState::Critical
            } else if sample.percent_used >= self.warn_threshold {
                DiskState::Warning
            } else {
                DiskState::Healthy
            };

            let key = format!("{node_name}:{}", sample.mount);
            let previous = {
                let mut state = self.state.lock().await;
                state.insert(key, current)
            };

            if let Some(prev) = previous
                && prev != current
                && current == DiskState::Critical
            {
                alerts.push(MonitorAlert {
                    condition: AlertCondition::DiskUsageCritical {
                        node: node_name.to_string(),
                        mount: sample.mount.clone(),
                        percent_used: sample.percent_used,
                    },
                    level: NotificationLevel::Critical,
                    title: format!("Disk critical on {node_name}"),
                    body: format!(
                        "Mount {} reached {:.1}% utilization on {node_name}",
                        sample.mount, sample.percent_used
                    ),
                    event_key: format!("disk_critical:{node_name}:{}", sample.mount),
                });
            }
        }

        alerts
    }
}

impl<S: NotificationSender> FleetMonitor<S> {
    pub fn new(sender: Arc<S>, settings: MonitorSettings) -> Result<Self> {
        Ok(Self {
            sender,
            node_monitor: NodeMonitor::new(settings.clone())?,
            model_monitor: ModelMonitor::new(settings.clone())?,
            disk_monitor: DiskMonitor::new(settings),
        })
    }

    /// Run one full monitoring cycle and dispatch generated alerts.
    pub async fn poll_and_notify(
        &self,
        config: &FleetConfig,
        local_node_name: &str,
    ) -> Result<Vec<MonitorAlert>> {
        let mut alerts = Vec::new();
        alerts.extend(self.node_monitor.poll(config).await);
        alerts.extend(self.model_monitor.poll(config).await);
        alerts.extend(self.disk_monitor.poll_local(local_node_name).await?);

        for alert in &alerts {
            let (level, title, body) = alert.to_notification_parts();
            self.sender.send(level, title, body).await?;
        }

        Ok(alerts)
    }

    /// Helper to report leader failover condition.
    pub async fn notify_leader_failover(&self, from: &str, to: &str) -> Result<()> {
        let title = format!("Leader failover: {from} → {to}");
        let body =
            format!("ForgeFleet elected a new leader due to state transition ({from} to {to}).");
        self.sender
            .send(NotificationLevel::Warning, &title, &body)
            .await
    }

    /// Helper to report self-update failures.
    pub async fn notify_self_update_failed(&self, node: &str, reason: &str) -> Result<()> {
        let title = format!("Self-update failed on {node}");
        let body = format!("Automatic update failed on node '{node}': {reason}");
        self.sender
            .send(NotificationLevel::Critical, &title, &body)
            .await
    }
}

async fn probe_online(client: &reqwest::Client, url: &str) -> bool {
    match client.get(url).send().await {
        Ok(response) => response.status().is_success() || response.status().is_server_error(),
        Err(_) => false,
    }
}

async fn probe_endpoint_state(client: &reqwest::Client, url: &str) -> ModelState {
    match client.get(url).send().await {
        Ok(response) if response.status().is_success() => ModelState::Healthy,
        Ok(_) => ModelState::Degraded,
        Err(_) => ModelState::Offline,
    }
}

fn normalize_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

fn read_local_disk_usage() -> Result<Vec<DiskUsageSample>> {
    let output = Command::new("df")
        .args(["-P", "-k"])
        .output()
        .map_err(ForgeFleetError::Io)?;

    if !output.status.success() {
        return Err(ForgeFleetError::Runtime(format!(
            "df command failed with status {}",
            output.status
        )));
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|error| ForgeFleetError::Runtime(format!("df output is not utf-8: {error}")))?;

    let mut samples = Vec::new();

    for line in stdout.lines().skip(1) {
        let columns: Vec<&str> = line.split_whitespace().collect();
        if columns.len() < 6 {
            continue;
        }

        let Some(raw_percent) = columns.get(4) else {
            continue;
        };
        let percent = raw_percent
            .trim_end_matches('%')
            .parse::<f64>()
            .unwrap_or(0.0);
        let mount = columns[5].to_string();

        samples.push(DiskUsageSample {
            mount,
            percent_used: percent,
        });
    }

    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn config_with_node_and_model(port: u16) -> FleetConfig {
        toml::from_str(&format!(
            r#"
[general]
name = "TestFleet"
api_port = 51800

[nodes.taylor]
ip = "127.0.0.1"
role = "gateway"
port = {port}

[nodes.taylor.models.qwen35_35b]
name = "Qwen3.5-35B"
port = {port}
tier = 2
"#,
        ))
        .unwrap()
    }

    async fn spawn_status_server(
        initial_status: u16,
    ) -> (
        u16,
        tokio::task::JoinHandle<()>,
        tokio::sync::watch::Sender<u16>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (status_tx, mut status_rx) = tokio::sync::watch::channel(initial_status);

        let handle = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buffer = [0u8; 1024];
                let _ = stream.read(&mut buffer).await;
                let status = *status_rx.borrow_and_update();
                let body = if status == 200 { "ok" } else { "bad" };
                let response = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        (port, handle, status_tx)
    }

    #[tokio::test]
    async fn test_node_monitor_alerts_only_on_state_change() {
        let (port, handle, _status_tx) = spawn_status_server(200).await;
        let monitor = NodeMonitor::new(MonitorSettings::default()).unwrap();
        let config = config_with_node_and_model(port);

        // First observation establishes baseline.
        assert!(monitor.poll(&config).await.is_empty());

        // Bring endpoint down.
        handle.abort();
        tokio::time::sleep(Duration::from_millis(80)).await;

        let alerts = monitor.poll(&config).await;
        assert_eq!(alerts.len(), 1);
        assert!(matches!(
            alerts[0].condition,
            AlertCondition::NodeWentOffline { .. }
        ));

        // Same state again should not re-alert.
        assert!(monitor.poll(&config).await.is_empty());
    }

    #[tokio::test]
    async fn test_model_monitor_detects_healthy_to_degraded_transition() {
        let (port, _handle, status_tx) = spawn_status_server(200).await;
        let monitor = ModelMonitor::new(MonitorSettings::default()).unwrap();
        let config = config_with_node_and_model(port);

        // Baseline healthy.
        assert!(monitor.poll(&config).await.is_empty());

        // Flip to degraded (503).
        status_tx.send(503).unwrap();
        let alerts = monitor.poll(&config).await;

        assert_eq!(alerts.len(), 1);
        assert!(matches!(
            alerts[0].condition,
            AlertCondition::ModelEndpointDegraded { .. }
        ));
        assert_eq!(alerts[0].level, NotificationLevel::Warning);
    }

    #[tokio::test]
    async fn test_disk_monitor_critical_on_transition_only() {
        let monitor = DiskMonitor::new(MonitorSettings::default()).with_thresholds(80.0, 90.0);

        // Inject sample transitions directly through evaluator.
        let first = vec![DiskUsageSample {
            mount: "/".to_string(),
            percent_used: 75.0,
        }];
        assert!(monitor.evaluate_samples("taylor", &first).await.is_empty());

        let second = vec![DiskUsageSample {
            mount: "/".to_string(),
            percent_used: 92.0,
        }];
        let alerts = monitor.evaluate_samples("taylor", &second).await;
        assert_eq!(alerts.len(), 1);
        assert!(matches!(
            alerts[0].condition,
            AlertCondition::DiskUsageCritical { .. }
        ));

        // Staying critical should not spam.
        assert!(monitor.evaluate_samples("taylor", &second).await.is_empty());
    }

    #[tokio::test]
    async fn test_status_server_changes_status_codes() {
        let (port, _handle, status_tx) = spawn_status_server(200).await;
        let url = format!("http://127.0.0.1:{port}/health");
        let client = reqwest::Client::new();

        let first = client.get(&url).send().await.unwrap();
        assert_eq!(first.status().as_u16(), 200);

        status_tx.send(503).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let second = client.get(&url).send().await.unwrap();
        assert_eq!(second.status().as_u16(), 503);
    }
}
