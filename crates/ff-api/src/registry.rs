use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendEndpoint {
    pub id: String,
    pub node: String,
    pub host: String,
    pub port: u16,
    pub model: String,
    pub tier: u8,
    #[serde(default = "default_true")]
    pub healthy: bool,
    #[serde(default)]
    pub busy: bool,
    #[serde(default = "default_http")]
    pub scheme: String,
}

impl BackendEndpoint {
    pub fn base_url(&self) -> String {
        format!("{}://{}:{}", self.scheme, self.host, self.port)
    }
}

fn default_true() -> bool {
    true
}

fn default_http() -> String {
    "http".to_string()
}

#[derive(Debug)]
pub struct BackendRegistry {
    endpoints: RwLock<Vec<BackendEndpoint>>,
}

impl BackendRegistry {
    pub fn new(endpoints: Vec<BackendEndpoint>) -> Self {
        Self {
            endpoints: RwLock::new(endpoints),
        }
    }

    pub async fn add_endpoint(&self, endpoint: BackendEndpoint) {
        self.endpoints.write().await.push(endpoint);
    }

    pub async fn all_endpoints(&self) -> Vec<BackendEndpoint> {
        self.endpoints.read().await.clone()
    }

    pub async fn healthy_endpoints(&self) -> Vec<BackendEndpoint> {
        self.endpoints
            .read()
            .await
            .iter()
            .filter(|endpoint| endpoint.healthy)
            .cloned()
            .collect()
    }

    pub async fn healthy_by_model(&self, model: &str) -> Vec<BackendEndpoint> {
        self.endpoints
            .read()
            .await
            .iter()
            .filter(|endpoint| endpoint.healthy && endpoint.model == model)
            .cloned()
            .collect()
    }

    pub async fn healthy_by_tier(&self, tier: u8) -> Vec<BackendEndpoint> {
        self.endpoints
            .read()
            .await
            .iter()
            .filter(|endpoint| endpoint.healthy && endpoint.tier == tier)
            .cloned()
            .collect()
    }

    pub async fn model_tier(&self, model: &str) -> Option<u8> {
        self.endpoints
            .read()
            .await
            .iter()
            .filter(|endpoint| endpoint.model == model)
            .map(|endpoint| endpoint.tier)
            .min()
    }

    pub async fn set_health(&self, id: &str, healthy: bool) {
        if let Some(endpoint) = self
            .endpoints
            .write()
            .await
            .iter_mut()
            .find(|endpoint| endpoint.id == id)
        {
            endpoint.healthy = healthy;
        }
    }

    pub async fn set_busy(&self, id: &str, busy: bool) {
        if let Some(endpoint) = self
            .endpoints
            .write()
            .await
            .iter_mut()
            .find(|endpoint| endpoint.id == id)
        {
            endpoint.busy = busy;
        }
    }

    pub async fn available_models(&self) -> Vec<(String, u8)> {
        let mut model_tiers = HashMap::<String, u8>::new();

        for endpoint in self
            .endpoints
            .read()
            .await
            .iter()
            .filter(|endpoint| endpoint.healthy)
        {
            model_tiers
                .entry(endpoint.model.clone())
                .and_modify(|tier| *tier = (*tier).min(endpoint.tier))
                .or_insert(endpoint.tier);
        }

        let mut models = model_tiers.into_iter().collect::<Vec<_>>();
        models.sort_by(|left, right| left.0.cmp(&right.0));
        models
    }

    pub async fn stats(&self) -> RegistryStats {
        let endpoints = self.endpoints.read().await;
        let total = endpoints.len();
        let healthy = endpoints.iter().filter(|endpoint| endpoint.healthy).count();
        let busy = endpoints.iter().filter(|endpoint| endpoint.busy).count();

        RegistryStats {
            total,
            healthy,
            busy,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RegistryStats {
    pub total: usize,
    pub healthy: usize,
    pub busy: usize,
}
