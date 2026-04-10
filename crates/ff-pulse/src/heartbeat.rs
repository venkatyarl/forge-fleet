//! HeartbeatPublisher — background loop that collects local system metrics
//! and publishes them to Redis on a fixed interval.

use std::time::Duration;

use chrono::Utc;
use sysinfo::{Disks, System};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, error, info};

use crate::client::PulseClient;
use crate::metrics::{LoadedModel, NodeMetrics};

/// Publishes periodic heartbeats containing system metrics to Redis.
pub struct HeartbeatPublisher {
    client: PulseClient,
    node_name: String,
    interval: Duration,
}

impl HeartbeatPublisher {
    /// Create a new heartbeat publisher.
    pub fn new(client: PulseClient, node_name: String, interval: Duration) -> Self {
        Self {
            client,
            node_name,
            interval,
        }
    }

    /// Create with the default 15-second interval.
    pub fn with_defaults(client: PulseClient, node_name: String) -> Self {
        Self::new(client, node_name, Duration::from_secs(15))
    }

    /// Spawn the heartbeat loop as a background tokio task.
    ///
    /// The loop runs until `shutdown` receives `true`, collecting local
    /// metrics and publishing them to Redis each interval.
    pub fn start(mut self, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            info!(
                "Heartbeat publisher started for '{}' (interval: {:?})",
                self.node_name, self.interval
            );

            loop {
                tokio::select! {
                    _ = tokio::time::sleep(self.interval) => {
                        let metrics = self.collect_local_metrics().await;
                        if let Err(e) = self.client.publish_metrics(&self.node_name, &metrics).await {
                            error!("Failed to publish heartbeat: {e}");
                        } else {
                            debug!("Heartbeat published for '{}'", self.node_name);
                        }
                    }
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            info!("Heartbeat publisher for '{}' shutting down", self.node_name);
                            break;
                        }
                    }
                }
            }
        })
    }

    /// Collect system metrics from the local machine.
    async fn collect_local_metrics(&self) -> NodeMetrics {
        let mut sys = System::new_all();
        sys.refresh_all();

        let cpu_percent = sys.global_cpu_usage() as f64;

        let ram_total_gb = sys.total_memory() as f64 / 1_073_741_824.0;
        let ram_used_gb = sys.used_memory() as f64 / 1_073_741_824.0;

        let disks = Disks::new_with_refreshed_list();
        let (disk_total, disk_used) =
            disks
                .iter()
                .fold((0u64, 0u64), |(total, used), disk| {
                    (
                        total + disk.total_space(),
                        used + (disk.total_space() - disk.available_space()),
                    )
                });
        let disk_total_gb = disk_total as f64 / 1_073_741_824.0;
        let disk_used_gb = disk_used as f64 / 1_073_741_824.0;

        let uptime_secs = System::uptime();

        // Auto-detect running LLM servers on ports 55000-55010
        let loaded_models = Self::scan_local_models().await;

        NodeMetrics {
            node_name: self.node_name.clone(),
            timestamp: Utc::now(),
            cpu_percent,
            ram_used_gb,
            ram_total_gb,
            disk_used_gb,
            disk_total_gb,
            loaded_models,
            active_tasks: 0,
            queue_depth: 0,
            tokens_per_sec: 0.0,
            temperature_c: None,
            uptime_secs,
        }
    }

    /// Scan localhost ports 55000-55010 for running LLM servers.
    ///
    /// Hits /health and /v1/models on each port to detect what's loaded.
    async fn scan_local_models() -> Vec<LoadedModel> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap_or_default();

        let mut models = Vec::new();

        for port in 55000..=55010 {
            // Check health first
            let health_url = format!("http://127.0.0.1:{port}/health");
            let health_ok = match client.get(&health_url).send().await {
                Ok(r) => r.status().is_success(),
                Err(_) => continue, // port not listening, skip
            };

            let status = if health_ok { "healthy" } else { "loading" };

            // Try to get model name from /v1/models
            let models_url = format!("http://127.0.0.1:{port}/v1/models");
            let model_id = match client.get(&models_url).send().await {
                Ok(r) => {
                    if let Ok(body) = r.json::<serde_json::Value>().await {
                        // llama.cpp returns {"data": [{"id": "model-name"}]}
                        // mlx_lm returns {"data": [{"id": "/path/to/model"}, ...]}
                        body.get("data")
                            .and_then(|d| d.as_array())
                            .and_then(|arr| arr.last()) // last = most recently loaded
                            .and_then(|m| m.get("id"))
                            .and_then(|id| id.as_str())
                            .map(|s| {
                                // Clean up path-based model IDs
                                s.rsplit('/').next().unwrap_or(s).to_string()
                            })
                            .unwrap_or_else(|| "unknown".to_string())
                    } else {
                        "unknown".to_string()
                    }
                }
                Err(_) => "unknown".to_string(),
            };

            debug!(port, model = %model_id, status, "detected local LLM server");
            models.push(LoadedModel {
                port,
                model_id,
                status: status.to_string(),
            });
        }

        models
    }
}
