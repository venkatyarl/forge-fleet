//! Synthetic health checks for ForgeFleet.
//!
//! Provides a [`SyntheticProbe`] trait and built-in implementations that
//! actively verify fleet subsystems rather than waiting for failures.
//!
//! # Built-in probes
//!
//! | Probe | What it checks |
//! |---|---|
//! | [`HttpHealthProbe`] | GET /health on each node |
//! | [`LlmSmokeProbe`] | Send "say hello" to /v1/chat/completions |
//! | [`DbWriteReadProbe`] | Write/read/delete a test key in config_kv |
//! | [`ReplicationLagProbe`] | Leader sequence vs follower sequence |
//! | [`DiskSpaceProbe`] | Disk usage via `std::fs` metadata |
//! | [`BackupFreshnessProbe`] | Last backup age, fail if >48h |

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::config::FleetConfig;

// ─── Probe Result ────────────────────────────────────────────────────────────

/// Outcome of a single probe execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProbeStatus {
    /// Probe passed — subsystem is healthy.
    Pass,
    /// Probe detected degradation but not full failure.
    Degraded { reason: String },
    /// Probe failed — subsystem is down or broken.
    Fail { reason: String },
}

impl ProbeStatus {
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass)
    }

    pub fn is_fail(&self) -> bool {
        matches!(self, Self::Fail { .. })
    }

    pub fn is_degraded(&self) -> bool {
        matches!(self, Self::Degraded { .. })
    }

    /// Numeric score: Pass=1.0, Degraded=0.5, Fail=0.0
    pub fn score(&self) -> f64 {
        match self {
            Self::Pass => 1.0,
            Self::Degraded { .. } => 0.5,
            Self::Fail { .. } => 0.0,
        }
    }
}

/// Full result of a probe execution, including timing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeResult {
    /// Which probe produced this result.
    pub probe_name: String,
    /// Pass / Degraded / Fail.
    pub status: ProbeStatus,
    /// How long the probe took to execute.
    pub latency: Duration,
    /// When the probe ran.
    pub timestamp: DateTime<Utc>,
    /// Optional node name (for per-node probes).
    pub node: Option<String>,
    /// Optional extra metadata.
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

impl ProbeResult {
    /// Create a new result with timing already measured.
    pub fn new(
        probe_name: impl Into<String>,
        status: ProbeStatus,
        latency: Duration,
        node: Option<String>,
    ) -> Self {
        Self {
            probe_name: probe_name.into(),
            status,
            latency,
            timestamp: Utc::now(),
            node,
            metadata: HashMap::new(),
        }
    }

    /// Attach a metadata key-value pair.
    pub fn with_meta(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }
}

// ─── Probe Trait ─────────────────────────────────────────────────────────────

/// A synthetic health probe that actively verifies a subsystem.
#[async_trait]
pub trait SyntheticProbe: Send + Sync {
    /// Human-readable probe name (e.g. "http_health", "llm_smoke").
    fn name(&self) -> &str;

    /// How often this probe should run.
    fn interval(&self) -> Duration;

    /// Which health category this probe belongs to.
    fn category(&self) -> ProbeCategory;

    /// Execute the probe and return results (one per node or one overall).
    async fn execute(&self, config: &FleetConfig) -> Vec<ProbeResult>;
}

/// Health category for scorecard aggregation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeCategory {
    /// API endpoint health (weight 30).
    Api,
    /// Model / LLM inference health (weight 25).
    Models,
    /// Storage / database health (weight 20).
    Storage,
    /// Fleet coordination — replication, leader election (weight 15).
    Fleet,
    /// Infrastructure — disk, backups (weight 10).
    Infra,
}

impl ProbeCategory {
    /// Scorecard weight for this category (out of 100).
    pub fn weight(&self) -> u32 {
        match self {
            Self::Api => 30,
            Self::Models => 25,
            Self::Storage => 20,
            Self::Fleet => 15,
            Self::Infra => 10,
        }
    }

    /// All categories in order.
    pub fn all() -> &'static [ProbeCategory] {
        &[
            Self::Api,
            Self::Models,
            Self::Storage,
            Self::Fleet,
            Self::Infra,
        ]
    }
}

impl std::fmt::Display for ProbeCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Api => write!(f, "API"),
            Self::Models => write!(f, "Models"),
            Self::Storage => write!(f, "Storage"),
            Self::Fleet => write!(f, "Fleet"),
            Self::Infra => write!(f, "Infra"),
        }
    }
}

// ─── Built-in Probes ─────────────────────────────────────────────────────────

/// GET /health on every node's API port.
pub struct HttpHealthProbe {
    client: reqwest::Client,
    interval: Duration,
}

impl HttpHealthProbe {
    pub fn new(timeout: Duration) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("failed to build reqwest client"),
            interval: Duration::from_secs(30),
        }
    }

    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }
}

#[async_trait]
impl SyntheticProbe for HttpHealthProbe {
    fn name(&self) -> &str {
        "http_health"
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    fn category(&self) -> ProbeCategory {
        ProbeCategory::Api
    }

    async fn execute(&self, config: &FleetConfig) -> Vec<ProbeResult> {
        let mut results = Vec::new();

        for (node_name, node) in &config.nodes {
            let port = node.port.unwrap_or(config.fleet.api_port);
            let url = format!("http://{}:{}/health", node.ip, port);

            let start = Instant::now();
            let status = match self.client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => ProbeStatus::Pass,
                Ok(resp) => ProbeStatus::Degraded {
                    reason: format!("HTTP {}", resp.status()),
                },
                Err(e) => ProbeStatus::Fail {
                    reason: format!("unreachable: {e}"),
                },
            };
            let latency = start.elapsed();

            debug!(probe = "http_health", node = %node_name, ?status, ?latency);
            results.push(
                ProbeResult::new("http_health", status, latency, Some(node_name.clone()))
                    .with_meta("url", &url),
            );
        }

        results
    }
}

// ─── LLM Smoke Probe ────────────────────────────────────────────────────────

/// Send "say hello" to each model's /v1/chat/completions and verify a response.
pub struct LlmSmokeProbe {
    client: reqwest::Client,
    interval: Duration,
}

impl LlmSmokeProbe {
    pub fn new(timeout: Duration) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("failed to build reqwest client"),
            interval: Duration::from_secs(120),
        }
    }

    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }
}

#[async_trait]
impl SyntheticProbe for LlmSmokeProbe {
    fn name(&self) -> &str {
        "llm_smoke"
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    fn category(&self) -> ProbeCategory {
        ProbeCategory::Models
    }

    async fn execute(&self, config: &FleetConfig) -> Vec<ProbeResult> {
        let mut results = Vec::new();

        for (node_name, node) in &config.nodes {
            for (model_slug, model) in &node.models {
                let Some(port) = model.port else {
                    continue;
                };

                let url = format!("http://{}:{}/v1/chat/completions", node.ip, port);
                let body = serde_json::json!({
                    "model": &model.name,
                    "messages": [{"role": "user", "content": "say hello"}],
                    "max_tokens": 16,
                    "temperature": 0.0,
                });

                let start = Instant::now();
                let status = match self.client.post(&url).json(&body).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        // Verify we got some content back
                        match resp.json::<serde_json::Value>().await {
                            Ok(json) => {
                                let has_content = json
                                    .pointer("/choices/0/message/content")
                                    .and_then(|v| v.as_str())
                                    .is_some_and(|s| !s.is_empty());
                                if has_content {
                                    ProbeStatus::Pass
                                } else {
                                    ProbeStatus::Degraded {
                                        reason: "response missing content".into(),
                                    }
                                }
                            }
                            Err(e) => ProbeStatus::Degraded {
                                reason: format!("invalid JSON response: {e}"),
                            },
                        }
                    }
                    Ok(resp) => ProbeStatus::Fail {
                        reason: format!("HTTP {}", resp.status()),
                    },
                    Err(e) => ProbeStatus::Fail {
                        reason: format!("unreachable: {e}"),
                    },
                };
                let latency = start.elapsed();

                debug!(
                    probe = "llm_smoke",
                    node = %node_name,
                    model = %model_slug,
                    ?status,
                    ?latency,
                );
                results.push(
                    ProbeResult::new("llm_smoke", status, latency, Some(node_name.clone()))
                        .with_meta("model", model_slug)
                        .with_meta("url", &url),
                );
            }
        }

        results
    }
}

// ─── DB Write-Read Probe ─────────────────────────────────────────────────────

/// Write a test key to config_kv, read it back, then delete it.
///
/// This probe requires a SQLite connection path. It verifies the database
/// is writable, readable, and responsive.
pub struct DbWriteReadProbe {
    /// Path to the SQLite database (used for local checks or API routing).
    #[allow(dead_code)]
    db_path: String,
    interval: Duration,
}

impl DbWriteReadProbe {
    pub fn new(db_path: impl Into<String>) -> Self {
        Self {
            db_path: db_path.into(),
            interval: Duration::from_secs(60),
        }
    }

    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Perform the write-read-delete cycle using raw SQL on the node's API.
    /// In practice this would hit the local SQLite or the node's /api/db/query endpoint.
    /// For now we verify via the fleet API.
    async fn probe_via_api(&self, client: &reqwest::Client, base_url: &str) -> ProbeStatus {
        let test_key = format!("synthetic_probe.{}", uuid::Uuid::new_v4());
        let test_value = "probe_ok";

        // Write
        let write_url = format!("{base_url}/api/config/kv");
        let write_body = serde_json::json!({
            "key": &test_key,
            "value": test_value,
        });
        match client.put(&write_url).json(&write_body).send().await {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => {
                return ProbeStatus::Fail {
                    reason: format!("write failed: HTTP {}", r.status()),
                };
            }
            Err(e) => {
                return ProbeStatus::Fail {
                    reason: format!("write failed: {e}"),
                };
            }
        }

        // Read
        let read_url = format!("{base_url}/api/config/kv/{test_key}");
        match client.get(&read_url).send().await {
            Ok(r) if r.status().is_success() => match r.json::<serde_json::Value>().await {
                Ok(json) => {
                    let val = json.get("value").and_then(|v| v.as_str());
                    if val != Some(test_value) {
                        return ProbeStatus::Fail {
                            reason: format!(
                                "read mismatch: expected '{test_value}', got {:?}",
                                val
                            ),
                        };
                    }
                }
                Err(e) => {
                    return ProbeStatus::Degraded {
                        reason: format!("read parse error: {e}"),
                    };
                }
            },
            Ok(r) => {
                return ProbeStatus::Fail {
                    reason: format!("read failed: HTTP {}", r.status()),
                };
            }
            Err(e) => {
                return ProbeStatus::Fail {
                    reason: format!("read failed: {e}"),
                };
            }
        }

        // Delete (cleanup)
        let delete_url = format!("{base_url}/api/config/kv/{test_key}");
        if let Err(e) = client.delete(&delete_url).send().await {
            warn!(probe = "db_write_read", "cleanup delete failed: {e}");
        }

        ProbeStatus::Pass
    }
}

#[async_trait]
impl SyntheticProbe for DbWriteReadProbe {
    fn name(&self) -> &str {
        "db_write_read"
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    fn category(&self) -> ProbeCategory {
        ProbeCategory::Storage
    }

    async fn execute(&self, config: &FleetConfig) -> Vec<ProbeResult> {
        // Probe the leader node's database via its API
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build reqwest client");

        // Find the leader/gateway node
        let leader = config
            .nodes
            .iter()
            .find(|(_, n)| n.role.is_leader_like())
            .or_else(|| config.nodes.iter().next());

        let Some((node_name, node)) = leader else {
            return vec![ProbeResult::new(
                "db_write_read",
                ProbeStatus::Fail {
                    reason: "no nodes configured".into(),
                },
                Duration::ZERO,
                None,
            )];
        };

        let port = node.port.unwrap_or(config.fleet.api_port);
        let base_url = format!("http://{}:{}", node.ip, port);

        let start = Instant::now();
        let status = self.probe_via_api(&client, &base_url).await;
        let latency = start.elapsed();

        debug!(probe = "db_write_read", node = %node_name, ?status, ?latency);

        vec![ProbeResult::new(
            "db_write_read",
            status,
            latency,
            Some(node_name.clone()),
        )]
    }
}

// ─── Replication Lag Probe ───────────────────────────────────────────────────

/// Compare leader sequence vs follower sequence to detect replication lag.
pub struct ReplicationLagProbe {
    client: reqwest::Client,
    /// Max acceptable lag in sequence numbers before degraded.
    max_lag_degraded: u64,
    /// Max acceptable lag before failure.
    max_lag_fail: u64,
    interval: Duration,
}

impl ReplicationLagProbe {
    pub fn new(timeout: Duration) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("failed to build reqwest client"),
            max_lag_degraded: 10,
            max_lag_fail: 100,
            interval: Duration::from_secs(60),
        }
    }

    pub fn with_thresholds(mut self, degraded: u64, fail: u64) -> Self {
        self.max_lag_degraded = degraded;
        self.max_lag_fail = fail;
        self
    }

    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    async fn get_sequence(&self, base_url: &str) -> Option<u64> {
        let url = format!("{base_url}/api/replication/sequence");
        self.client
            .get(&url)
            .send()
            .await
            .ok()?
            .json::<serde_json::Value>()
            .await
            .ok()?
            .get("sequence")
            .and_then(|v| v.as_u64())
    }
}

#[async_trait]
impl SyntheticProbe for ReplicationLagProbe {
    fn name(&self) -> &str {
        "replication_lag"
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    fn category(&self) -> ProbeCategory {
        ProbeCategory::Fleet
    }

    async fn execute(&self, config: &FleetConfig) -> Vec<ProbeResult> {
        let mut results = Vec::new();

        // Find leader
        let leader = config.nodes.iter().find(|(_, n)| n.role.is_leader_like());

        let Some((leader_name, leader_node)) = leader else {
            return vec![ProbeResult::new(
                "replication_lag",
                ProbeStatus::Fail {
                    reason: "no leader node found".into(),
                },
                Duration::ZERO,
                None,
            )];
        };

        let leader_port = leader_node.port.unwrap_or(config.fleet.api_port);
        let leader_url = format!("http://{}:{}", leader_node.ip, leader_port);

        let start = Instant::now();
        let leader_seq = match self.get_sequence(&leader_url).await {
            Some(s) => s,
            None => {
                return vec![ProbeResult::new(
                    "replication_lag",
                    ProbeStatus::Fail {
                        reason: "cannot read leader sequence".into(),
                    },
                    start.elapsed(),
                    Some(leader_name.clone()),
                )];
            }
        };

        // Check each follower
        for (node_name, node) in &config.nodes {
            if node.role.is_leader_like() {
                continue;
            }

            let port = node.port.unwrap_or(config.fleet.api_port);
            let follower_url = format!("http://{}:{}", node.ip, port);

            let probe_start = Instant::now();
            let status = match self.get_sequence(&follower_url).await {
                Some(follower_seq) => {
                    let lag = leader_seq.saturating_sub(follower_seq);
                    if lag >= self.max_lag_fail {
                        ProbeStatus::Fail {
                            reason: format!(
                                "replication lag {lag} exceeds threshold {}",
                                self.max_lag_fail
                            ),
                        }
                    } else if lag >= self.max_lag_degraded {
                        ProbeStatus::Degraded {
                            reason: format!(
                                "replication lag {lag} above warning threshold {}",
                                self.max_lag_degraded
                            ),
                        }
                    } else {
                        ProbeStatus::Pass
                    }
                }
                None => ProbeStatus::Fail {
                    reason: "cannot read follower sequence".into(),
                },
            };
            let latency = probe_start.elapsed();

            debug!(
                probe = "replication_lag",
                node = %node_name,
                leader = %leader_name,
                ?status,
                ?latency,
            );
            results.push(
                ProbeResult::new("replication_lag", status, latency, Some(node_name.clone()))
                    .with_meta("leader", leader_name)
                    .with_meta("leader_seq", &leader_seq.to_string()),
            );
        }

        if results.is_empty() {
            // Single-node fleet — no followers to check
            results.push(ProbeResult::new(
                "replication_lag",
                ProbeStatus::Pass,
                start.elapsed(),
                Some(leader_name.clone()),
            ));
        }

        results
    }
}

// ─── Disk Space Probe ────────────────────────────────────────────────────────

/// Check disk usage on the local machine via `std::fs` metadata.
///
/// For remote nodes, this hits `/api/system/disk` on each node.
pub struct DiskSpaceProbe {
    client: reqwest::Client,
    /// Percent used before degraded warning.
    warn_threshold: f64,
    /// Percent used before failure.
    critical_threshold: f64,
    interval: Duration,
}

impl DiskSpaceProbe {
    pub fn new(timeout: Duration) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("failed to build reqwest client"),
            warn_threshold: 80.0,
            critical_threshold: 95.0,
            interval: Duration::from_secs(300),
        }
    }

    pub fn with_thresholds(mut self, warn: f64, critical: f64) -> Self {
        self.warn_threshold = warn;
        self.critical_threshold = critical;
        self
    }

    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Check local disk usage using platform-appropriate methods.
    #[cfg(unix)]
    fn check_local_disk() -> Option<f64> {
        use std::ffi::CString;
        use std::mem::MaybeUninit;

        let path = CString::new("/").ok()?;
        let mut stat = MaybeUninit::<libc::statvfs>::uninit();
        let ret = unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) };
        if ret != 0 {
            return None;
        }
        let stat = unsafe { stat.assume_init() };
        let total = stat.f_blocks as f64 * stat.f_frsize as f64;
        let avail = stat.f_bavail as f64 * stat.f_frsize as f64;
        if total == 0.0 {
            return None;
        }
        Some(((total - avail) / total) * 100.0)
    }

    #[cfg(not(unix))]
    fn check_local_disk() -> Option<f64> {
        None
    }
}

#[async_trait]
impl SyntheticProbe for DiskSpaceProbe {
    fn name(&self) -> &str {
        "disk_space"
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    fn category(&self) -> ProbeCategory {
        ProbeCategory::Infra
    }

    async fn execute(&self, config: &FleetConfig) -> Vec<ProbeResult> {
        let mut results = Vec::new();

        for (node_name, node) in &config.nodes {
            let port = node.port.unwrap_or(config.fleet.api_port);
            let url = format!("http://{}:{}/api/system/disk", node.ip, port);

            let start = Instant::now();
            let status = match self.client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    match resp.json::<serde_json::Value>().await {
                        Ok(json) => {
                            let pct = json
                                .get("percent_used")
                                .and_then(|v| v.as_f64())
                                .unwrap_or(0.0);
                            if pct >= self.critical_threshold {
                                ProbeStatus::Fail {
                                    reason: format!(
                                        "disk {pct:.1}% used (critical ≥{}%)",
                                        self.critical_threshold
                                    ),
                                }
                            } else if pct >= self.warn_threshold {
                                ProbeStatus::Degraded {
                                    reason: format!(
                                        "disk {pct:.1}% used (warning ≥{}%)",
                                        self.warn_threshold
                                    ),
                                }
                            } else {
                                ProbeStatus::Pass
                            }
                        }
                        Err(e) => ProbeStatus::Degraded {
                            reason: format!("invalid disk response: {e}"),
                        },
                    }
                }
                Ok(resp) => ProbeStatus::Degraded {
                    reason: format!("disk endpoint HTTP {}", resp.status()),
                },
                Err(_) => {
                    // Try local check if this might be the local node
                    match Self::check_local_disk() {
                        Some(pct) if pct >= self.critical_threshold => ProbeStatus::Fail {
                            reason: format!("local disk {pct:.1}% used (critical)"),
                        },
                        Some(pct) if pct >= self.warn_threshold => ProbeStatus::Degraded {
                            reason: format!("local disk {pct:.1}% used (warning)"),
                        },
                        Some(_) => ProbeStatus::Pass,
                        None => ProbeStatus::Degraded {
                            reason: "disk endpoint unreachable, local check unavailable".into(),
                        },
                    }
                }
            };
            let latency = start.elapsed();

            debug!(probe = "disk_space", node = %node_name, ?status, ?latency);
            results.push(
                ProbeResult::new("disk_space", status, latency, Some(node_name.clone()))
                    .with_meta("url", &url),
            );
        }

        results
    }
}

// ─── Backup Freshness Probe ──────────────────────────────────────────────────

/// Check last backup age. Fail if older than the configured threshold (default 48h).
pub struct BackupFreshnessProbe {
    client: reqwest::Client,
    /// Max backup age before failure.
    max_age: Duration,
    /// Max backup age before degraded warning (default: max_age / 2).
    warn_age: Duration,
    interval: Duration,
}

impl BackupFreshnessProbe {
    pub fn new(timeout: Duration) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("failed to build reqwest client"),
            max_age: Duration::from_secs(48 * 3600),
            warn_age: Duration::from_secs(24 * 3600),
            interval: Duration::from_secs(600),
        }
    }

    pub fn with_max_age(mut self, max_age: Duration) -> Self {
        self.max_age = max_age;
        self.warn_age = Duration::from_secs(max_age.as_secs() / 2);
        self
    }

    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }
}

#[async_trait]
impl SyntheticProbe for BackupFreshnessProbe {
    fn name(&self) -> &str {
        "backup_freshness"
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    fn category(&self) -> ProbeCategory {
        ProbeCategory::Infra
    }

    async fn execute(&self, config: &FleetConfig) -> Vec<ProbeResult> {
        // Check backup status on the leader node
        let leader = config
            .nodes
            .iter()
            .find(|(_, n)| n.role.is_leader_like())
            .or_else(|| config.nodes.iter().next());

        let Some((node_name, node)) = leader else {
            return vec![ProbeResult::new(
                "backup_freshness",
                ProbeStatus::Fail {
                    reason: "no nodes configured".into(),
                },
                Duration::ZERO,
                None,
            )];
        };

        let port = node.port.unwrap_or(config.fleet.api_port);
        let url = format!("http://{}:{}/api/backup/status", node.ip, port);

        let start = Instant::now();
        let status = match self.client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<serde_json::Value>().await {
                    Ok(json) => {
                        let last_backup = json
                            .get("last_backup_at")
                            .and_then(|v| v.as_str())
                            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                            .map(|dt| dt.with_timezone(&Utc));

                        match last_backup {
                            Some(ts) => {
                                let age = Utc::now()
                                    .signed_duration_since(ts)
                                    .to_std()
                                    .unwrap_or(Duration::MAX);
                                if age > self.max_age {
                                    ProbeStatus::Fail {
                                        reason: format!(
                                            "last backup {:.1}h ago (max {}h)",
                                            age.as_secs_f64() / 3600.0,
                                            self.max_age.as_secs() / 3600,
                                        ),
                                    }
                                } else if age > self.warn_age {
                                    ProbeStatus::Degraded {
                                        reason: format!(
                                            "last backup {:.1}h ago (warning at {}h)",
                                            age.as_secs_f64() / 3600.0,
                                            self.warn_age.as_secs() / 3600,
                                        ),
                                    }
                                } else {
                                    ProbeStatus::Pass
                                }
                            }
                            None => ProbeStatus::Fail {
                                reason: "no backup timestamp found".into(),
                            },
                        }
                    }
                    Err(e) => ProbeStatus::Fail {
                        reason: format!("invalid backup response: {e}"),
                    },
                }
            }
            Ok(resp) => ProbeStatus::Fail {
                reason: format!("backup endpoint HTTP {}", resp.status()),
            },
            Err(e) => ProbeStatus::Fail {
                reason: format!("backup endpoint unreachable: {e}"),
            },
        };
        let latency = start.elapsed();

        debug!(probe = "backup_freshness", node = %node_name, ?status, ?latency);

        vec![ProbeResult::new(
            "backup_freshness",
            status,
            latency,
            Some(node_name.clone()),
        )]
    }
}

// ─── Probe Registry ──────────────────────────────────────────────────────────

/// Collection of probes with convenience methods for running them all.
pub struct ProbeRegistry {
    probes: Vec<Box<dyn SyntheticProbe>>,
}

impl ProbeRegistry {
    pub fn new() -> Self {
        Self { probes: Vec::new() }
    }

    /// Create a registry with all built-in probes using default settings.
    pub fn with_defaults(db_path: impl Into<String>) -> Self {
        let timeout = Duration::from_secs(10);
        let mut registry = Self::new();
        registry.register(Box::new(HttpHealthProbe::new(timeout)));
        registry.register(Box::new(LlmSmokeProbe::new(Duration::from_secs(30))));
        registry.register(Box::new(DbWriteReadProbe::new(db_path)));
        registry.register(Box::new(ReplicationLagProbe::new(timeout)));
        registry.register(Box::new(DiskSpaceProbe::new(timeout)));
        registry.register(Box::new(BackupFreshnessProbe::new(timeout)));
        registry
    }

    /// Register a custom probe.
    pub fn register(&mut self, probe: Box<dyn SyntheticProbe>) {
        self.probes.push(probe);
    }

    /// Run all probes and collect results.
    pub async fn run_all(&self, config: &FleetConfig) -> Vec<ProbeResult> {
        let mut all_results = Vec::new();
        for probe in &self.probes {
            let results = probe.execute(config).await;
            all_results.extend(results);
        }
        all_results
    }

    /// Run only probes in a specific category.
    pub async fn run_category(
        &self,
        category: ProbeCategory,
        config: &FleetConfig,
    ) -> Vec<ProbeResult> {
        let mut results = Vec::new();
        for probe in &self.probes {
            if probe.category() == category {
                results.extend(probe.execute(config).await);
            }
        }
        results
    }

    /// Get all registered probe names.
    pub fn probe_names(&self) -> Vec<&str> {
        self.probes.iter().map(|p| p.name()).collect()
    }

    /// Number of registered probes.
    pub fn len(&self) -> usize {
        self.probes.len()
    }

    /// Whether the registry has no probes.
    pub fn is_empty(&self) -> bool {
        self.probes.is_empty()
    }
}

impl Default for ProbeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_probe_status_score() {
        assert_eq!(ProbeStatus::Pass.score(), 1.0);
        assert_eq!(
            ProbeStatus::Degraded {
                reason: "test".into()
            }
            .score(),
            0.5
        );
        assert_eq!(
            ProbeStatus::Fail {
                reason: "test".into()
            }
            .score(),
            0.0
        );
    }

    #[test]
    fn test_probe_status_predicates() {
        assert!(ProbeStatus::Pass.is_pass());
        assert!(!ProbeStatus::Pass.is_fail());
        assert!(!ProbeStatus::Pass.is_degraded());

        let degraded = ProbeStatus::Degraded {
            reason: "slow".into(),
        };
        assert!(degraded.is_degraded());
        assert!(!degraded.is_pass());
    }

    #[test]
    fn test_probe_category_weights_sum_to_100() {
        let total: u32 = ProbeCategory::all().iter().map(|c| c.weight()).sum();
        assert_eq!(total, 100);
    }

    #[test]
    fn test_probe_result_metadata() {
        let result = ProbeResult::new("test", ProbeStatus::Pass, Duration::from_millis(42), None)
            .with_meta("key", "value")
            .with_meta("node", "taylor");

        assert_eq!(result.metadata.get("key").unwrap(), "value");
        assert_eq!(result.metadata.get("node").unwrap(), "taylor");
        assert_eq!(result.probe_name, "test");
    }

    #[test]
    fn test_probe_result_serialization() {
        let result = ProbeResult::new(
            "http_health",
            ProbeStatus::Degraded {
                reason: "HTTP 503".into(),
            },
            Duration::from_millis(150),
            Some("taylor".into()),
        );
        let json = serde_json::to_string(&result).unwrap();
        let parsed: ProbeResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.probe_name, "http_health");
        assert!(parsed.status.is_degraded());
    }

    #[test]
    fn test_probe_registry_empty() {
        let registry = ProbeRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn test_category_display() {
        assert_eq!(format!("{}", ProbeCategory::Api), "API");
        assert_eq!(format!("{}", ProbeCategory::Models), "Models");
        assert_eq!(format!("{}", ProbeCategory::Storage), "Storage");
        assert_eq!(format!("{}", ProbeCategory::Fleet), "Fleet");
        assert_eq!(format!("{}", ProbeCategory::Infra), "Infra");
    }

    #[cfg(unix)]
    #[test]
    fn test_local_disk_check() {
        // Should return Some on Unix systems
        let pct = DiskSpaceProbe::check_local_disk();
        assert!(pct.is_some());
        let pct = pct.unwrap();
        assert!(pct >= 0.0 && pct <= 100.0);
    }
}
