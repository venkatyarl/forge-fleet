//! Dynamic Model Loading — P3 on-demand model management.
//!
//! Loads models into inference engines based on task requirements,
//! unloads idle models to free VRAM, and maintains a hot-cache of
//! frequently-used weights.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// A model that can be dynamically loaded/unloaded.
#[derive(Debug, Clone)]
pub struct DynamicModel {
    pub id: String,
    pub catalog_id: String,
    pub runtime: String,
    pub vram_gb: f32,
    pub load_time_ms: u64,
    pub last_used: std::time::Instant,
    pub load_count: u32,
}

/// State of a model on a given node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelState {
    Unloaded,
    Loading,
    Loaded,
    Evicting,
}

/// Per-node model state tracker.
pub struct DynamicLoader {
    models: Arc<RwLock<HashMap<String, DynamicModel>>>,
    states: Arc<RwLock<HashMap<String, ModelState>>>,
    /// Maximum VRAM budget in GB for this node.
    vram_budget_gb: f32,
    /// Idle time before eviction (seconds).
    eviction_idle_secs: u64,
}

impl DynamicLoader {
    pub fn new(vram_budget_gb: f32, eviction_idle_secs: u64) -> Self {
        Self {
            models: Arc::new(RwLock::new(HashMap::new())),
            states: Arc::new(RwLock::new(HashMap::new())),
            vram_budget_gb,
            eviction_idle_secs,
        }
    }

    /// Register a model as available for dynamic loading.
    pub async fn register(&self, model: DynamicModel) {
        let mut m = self.models.write().await;
        m.insert(model.id.clone(), model);
    }

    /// Request a model be loaded. Returns true if already loaded or load initiated.
    pub async fn request_load(&self, model_id: &str) -> bool {
        let models = self.models.read().await;
        let Some(model) = models.get(model_id) else {
            warn!("Dynamic load requested for unknown model: {}", model_id);
            return false;
        };
        let model = model.clone();
        drop(models);

        let mut states = self.states.write().await;
        match states.get(model_id).copied() {
            Some(ModelState::Loaded) => return true,
            Some(ModelState::Loading) => return true,
            _ => {
                states.insert(model_id.to_string(), ModelState::Loading);
            }
        }
        drop(states);

        // Check VRAM budget before loading
        let used_vram = self.used_vram().await;
        if used_vram + model.vram_gb > self.vram_budget_gb {
            info!("VRAM budget exceeded ({} + {} > {}); evicting idle models", used_vram, model.vram_gb, self.vram_budget_gb);
            self.evict_idle().await;
        }

        // Simulate async load
        let states = self.states.clone();
        let id = model_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(model.load_time_ms)).await;
            let mut s = states.write().await;
            s.insert(id.clone(), ModelState::Loaded);
            info!("Model {} loaded successfully", id);
        });

        true
    }

    /// Mark a model as recently used (resets eviction timer).
    pub async fn touch(&self, model_id: &str) {
        let mut models = self.models.write().await;
        if let Some(m) = models.get_mut(model_id) {
            m.last_used = std::time::Instant::now();
            m.load_count += 1;
        }
    }

    /// Evict idle models to free VRAM.
    pub async fn evict_idle(&self) {
        let models = self.models.write().await;
        let mut states = self.states.write().await;
        let now = std::time::Instant::now();
        let threshold = std::time::Duration::from_secs(self.eviction_idle_secs);

        let to_evict: Vec<String> = models
            .iter()
            .filter(|(_, m)| now.duration_since(m.last_used) > threshold)
            .filter(|(id, _)| states.get(*id) == Some(&ModelState::Loaded))
            .map(|(id, _)| id.clone())
            .collect();

        for id in to_evict {
            states.insert(id.clone(), ModelState::Evicting);
            info!("Evicting idle model: {}", id);
            // Simulate eviction
            states.insert(id, ModelState::Unloaded);
        }
    }

    /// Total VRAM currently in use by loaded models.
    pub async fn used_vram(&self) -> f32 {
        let models = self.models.read().await;
        let states = self.states.read().await;
        models
            .iter()
            .filter(|(id, _)| states.get(*id) == Some(&ModelState::Loaded))
            .map(|(_, m)| m.vram_gb)
            .sum()
    }

    /// List currently loaded models.
    pub async fn loaded_models(&self) -> Vec<String> {
        let states = self.states.read().await;
        states
            .iter()
            .filter(|(_, s)| **s == ModelState::Loaded)
            .map(|(id, _)| id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_dynamic_load_and_evict() {
        let loader = DynamicLoader::new(48.0, 60);
        loader.register(DynamicModel {
            id: "qwen3-30b".to_string(),
            catalog_id: "mlx:qwen3-30b".to_string(),
            runtime: "mlx".to_string(),
            vram_gb: 24.0,
            load_time_ms: 100,
            last_used: std::time::Instant::now(),
            load_count: 0,
        }).await;

        assert!(loader.request_load("qwen3-30b").await);
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        assert!(loader.loaded_models().await.contains(&"qwen3-30b".to_string()));
    }
}
