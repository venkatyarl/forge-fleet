//! Gateway API client — the TUI's single source of truth.
//!
//! All dashboard data is fetched from `http://localhost:51002` (the gateway).
//! In dev mode (no `FF_JWT_SECRET`) read-only GET calls succeed without auth.

use serde::Deserialize;

/// Default gateway base URL.
pub const GATEWAY_BASE: &str = "http://localhost:51002";

/// Typed gateway client.
#[derive(Debug, Clone)]
pub struct GatewayClient {
    pub base_url: String,
    client: reqwest::Client,
}

impl Default for GatewayClient {
    fn default() -> Self {
        Self::new(GATEWAY_BASE)
    }
}

impl GatewayClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            client: reqwest::Client::new(),
        }
    }

    // ── helpers ────────────────────────────────────────────────────────────

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, String> {
        let url = format!("{}{}", self.base_url, path);
        let res = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        if !res.status().is_success() {
            return Err(format!("{} {}", res.status().as_u16(), res.status().canonical_reason().unwrap_or("")));
        }
        res.json::<T>()
            .await
            .map_err(|e| format!("invalid JSON: {e}"))
    }

    async fn get_text(&self, path: &str) -> Result<String, String> {
        let url = format!("{}{}", self.base_url, path);
        let res = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        if !res.status().is_success() {
            return Err(format!("{} {}", res.status().as_u16(), res.status().canonical_reason().unwrap_or("")));
        }
        res.text().await.map_err(|e| format!("body read failed: {e}"))
    }

    // ── fleet ──────────────────────────────────────────────────────────────

    pub async fn get_fleet_status(&self) -> Result<FleetStatusResponse, String> {
        self.get_json("/api/fleet/status").await
    }

    pub async fn get_fleet_computers(&self) -> Result<Vec<FleetComputer>, String> {
        #[derive(Deserialize)]
        struct Resp {
            computers: Vec<FleetComputer>,
        }
        self.get_json::<Resp>("/api/fleet/computers")
            .await
            .map(|r| r.computers)
    }

    // ── models ─────────────────────────────────────────────────────────────

    pub async fn get_llm_servers(&self) -> Result<Vec<LlmServer>, String> {
        #[derive(Deserialize)]
        struct Resp {
            servers: Vec<LlmServer>,
        }
        self.get_json::<Resp>("/api/llm/servers")
            .await
            .map(|r| r.servers)
    }

    // ── tools ──────────────────────────────────────────────────────────────

    pub async fn get_tools(&self) -> Result<Vec<LiveTool>, String> {
        #[derive(Deserialize)]
        struct Resp {
            tools: Vec<LiveTool>,
        }
        self.get_json::<Resp>("/api/tools")
            .await
            .map(|r| r.tools)
    }

    // ── interactions ───────────────────────────────────────────────────────

    pub async fn get_interactions(&self, limit: usize) -> Result<Vec<Interaction>, String> {
        #[derive(Deserialize)]
        struct Resp {
            rows: Vec<Interaction>,
            #[serde(default)]
            error: Option<String>,
        }
        let path = format!("/api/interactions?limit={}", limit.min(500));
        let r = self.get_json::<Resp>(&path).await?;
        if let Some(e) = r.error {
            return Err(e);
        }
        Ok(r.rows)
    }

    // ── alerts ─────────────────────────────────────────────────────────────

    pub async fn get_alert_events(&self) -> Result<Vec<AlertEvent>, String> {
        #[derive(Deserialize)]
        struct Resp {
            events: Vec<AlertEvent>,
        }
        self.get_json::<Resp>("/api/alerts/events")
            .await
            .map(|r| r.events)
    }

    // ── ledger ─────────────────────────────────────────────────────────────

    pub async fn get_ledger_summary(&self) -> Result<FleetSummary, String> {
        self.get_json("/api/ledger/summary").await
    }

    // ── config / settings ──────────────────────────────────────────────────

    pub async fn get_config_text(&self) -> Result<String, String> {
        // Prefer the JSON wrapper; fall back to raw TOML if needed.
        match self.get_json::<ConfigResponse>("/api/config").await {
            Ok(cfg) => Ok(cfg.content),
            Err(_) => self.get_text("/api/config").await,
        }
    }

    pub async fn get_settings_runtime(&self) -> Result<SettingsResponse, String> {
        self.get_json("/api/settings/runtime").await
    }

    // ── skills ─────────────────────────────────────────────────────────────

    pub async fn get_skills(&self) -> Result<Vec<Skill>, String> {
        #[derive(Deserialize)]
        struct Resp {
            skills: Vec<Skill>,
        }
        self.get_json::<Resp>("/api/skills")
            .await
            .map(|r| r.skills)
    }

    // ── brain threads ──────────────────────────────────────────────────────

    pub async fn get_brain_threads(&self) -> Result<Vec<BrainThread>, String> {
        #[derive(Deserialize)]
        struct Resp {
            threads: Vec<BrainThread>,
        }
        self.get_json::<Resp>("/api/brain/threads")
            .await
            .map(|r| r.threads)
    }
}

// ── Response types ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
pub struct FleetStatusResponse {
    pub status: Option<String>,
    #[serde(default)]
    pub total_nodes: Option<usize>,
    #[serde(default)]
    pub summary: Option<FleetStatusSummary>,
    #[serde(default)]
    pub nodes: Vec<FleetComputer>,
    #[serde(default)]
    pub models: Vec<FleetModel>,
    #[serde(default)]
    pub scanned_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct FleetStatusSummary {
    #[serde(default)]
    pub total_nodes: Option<usize>,
    #[serde(default)]
    pub connected_nodes: Option<usize>,
    #[serde(default)]
    pub unhealthy_nodes: Option<usize>,
    #[serde(default)]
    pub enrolled_nodes: Option<usize>,
    #[serde(default)]
    pub seed_nodes: Option<usize>,
    #[serde(default)]
    pub model_count: Option<usize>,
    #[serde(default)]
    pub leader: Option<String>,
    #[serde(default)]
    pub gateway_version: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct FleetComputer {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub health: Option<String>,
    #[serde(default)]
    pub leader_state: Option<String>,
    #[serde(default)]
    pub is_leader: Option<bool>,
    #[serde(default)]
    pub models_loaded: Option<Vec<String>>,
    #[serde(default)]
    pub last_heartbeat: Option<String>,
    #[serde(default)]
    pub current_workload: Option<FleetWorkload>,
    #[serde(default)]
    pub models: Option<Vec<FleetModel>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct FleetWorkload {
    pub status: String,
    #[serde(default)]
    pub active_tasks: Option<i32>,
    #[serde(default)]
    pub gpu_util: Option<f64>,
    #[serde(default)]
    pub vram_used_gb: Option<f64>,
    #[serde(default)]
    pub vram_total_gb: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct FleetModel {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub tier: Option<i32>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub context_window: Option<i32>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub node: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LlmServer {
    pub computer: String,
    pub endpoint: String,
    pub runtime: String,
    pub model: String,
    pub queue_depth: i32,
    #[serde(default)]
    pub active_requests: Option<i32>,
    pub tokens_per_sec: f64,
    #[serde(default)]
    pub gpu_pct: Option<f64>,
    #[serde(default)]
    pub load_score: Option<f64>,
    pub healthy: bool,
    pub status: String,
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LiveTool {
    pub tool_name: String,
    pub worker_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub health_checked_at: String,
    #[serde(default)]
    pub call_count: i64,
    #[serde(default)]
    pub avg_latency_ms: Option<f64>,
    #[serde(default)]
    pub healthy: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Interaction {
    #[serde(default)]
    pub id: Option<String>,
    pub channel: String,
    #[serde(default)]
    pub engine: Option<String>,
    pub request_text: String,
    #[serde(default)]
    pub response_text: Option<String>,
    #[serde(default)]
    pub error_text: Option<String>,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub latency_ms: Option<i64>,
    #[serde(default)]
    pub tokens_in: Option<i64>,
    #[serde(default)]
    pub tokens_out: Option<i64>,
    #[serde(default)]
    pub ts: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AlertEvent {
    pub id: String,
    pub policy_id: String,
    pub policy_name: String,
    pub severity: String,
    pub metric: String,
    #[serde(default)]
    pub computer_id: Option<String>,
    #[serde(default)]
    pub computer_name: Option<String>,
    pub fired_at: String,
    #[serde(default)]
    pub resolved_at: Option<String>,
    #[serde(default)]
    pub value: Option<f64>,
    #[serde(default)]
    pub value_text: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct FleetSummary {
    #[serde(default)]
    pub total_requests: i64,
    #[serde(default)]
    pub total_tokens: i64,
    #[serde(default)]
    pub total_cost_usd: f64,
    #[serde(default)]
    pub local_requests: i64,
    #[serde(default)]
    pub cloud_requests: i64,
    #[serde(default)]
    pub cloud_cost_usd: f64,
    #[serde(default)]
    pub savings_vs_cloud_only_usd: f64,
    #[serde(default)]
    pub models: Vec<ModelStats>,
    #[serde(default)]
    pub daily_cost_usd: f64,
    #[serde(default)]
    pub daily_budget_usd: f64,
    #[serde(default)]
    pub budget_remaining_usd: f64,
    #[serde(default)]
    pub budget_percent_used: f64,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ModelStats {
    pub model: String,
    #[serde(default)]
    pub request_count: i64,
    #[serde(default)]
    pub total_tokens: i64,
    #[serde(default)]
    pub total_cost_usd: f64,
    #[serde(default)]
    pub cloud_cost_usd: f64,
    #[serde(default)]
    pub avg_latency_ms: f64,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigResponse {
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SettingsResponse {
    #[serde(default)]
    pub runtime_config: Option<RuntimeConfig>,
    #[serde(default)]
    pub enrollment: Option<EnrollmentSettings>,
    #[serde(default)]
    pub telegram: Option<TelegramSettings>,
    #[serde(default)]
    pub database: Option<DatabaseSettings>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RuntimeConfig {
    #[serde(default)]
    pub loaded: bool,
    #[serde(default)]
    pub config_path: Option<String>,
    #[serde(default)]
    pub fleet_name: Option<String>,
    #[serde(default)]
    pub api_port: Option<i32>,
    #[serde(default)]
    pub nodes_configured: Option<i32>,
    #[serde(default)]
    pub models_configured: Option<i32>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct EnrollmentSettings {
    #[serde(default)]
    pub default_role: Option<String>,
    #[serde(default)]
    pub allowed_roles: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TelegramSettings {
    #[serde(default)]
    pub configured: bool,
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct DatabaseSettings {
    #[serde(default)]
    pub active_mode: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Skill {
    pub id: String,
    pub scope: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub family: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct BrainThread {
    #[serde(default)]
    pub id: String,
    pub slug: String,
    pub title: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub last_message_at: Option<String>,
    #[serde(default)]
    pub message_count: Option<i64>,
}
