//! Launch recipe for the canonical qwen3-coder-480B llama.cpp RPC ring.

use serde::{Deserialize, Serialize};

pub const DEFAULT_480B_MODEL: &str = "qwen3-coder-480b";
pub const DEFAULT_480B_RPC_SHARD_COUNT: u32 = 3;
pub const DEFAULT_480B_RPC_RING_TOPOLOGY: &str = "adele,rihanna,beyonce";
pub const DEFAULT_480B_ENDPOINT_URL: &str = "http://127.0.0.1:51001";
pub const DEFAULT_480B_PORT: u16 = 51001;
pub const DEFAULT_480B_CTX_SIZE: u32 = 65_536;
pub const DEFAULT_480B_PARALLEL: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rpc480bRingConfig {
    pub model: String,
    pub model_path: String,
    pub endpoint_url: String,
    pub binary: String,
    pub host: String,
    pub port: u16,
    pub ctx_size: u32,
    pub parallel: u32,
    pub rpc_shard_count: u32,
    pub rpc_ring_topology: String,
}

impl Default for Rpc480bRingConfig {
    fn default() -> Self {
        Self {
            model: DEFAULT_480B_MODEL.to_string(),
            model_path: format!("/models/{DEFAULT_480B_MODEL}/model.gguf"),
            endpoint_url: DEFAULT_480B_ENDPOINT_URL.to_string(),
            binary: "llama-server".to_string(),
            host: "0.0.0.0".to_string(),
            port: DEFAULT_480B_PORT,
            ctx_size: DEFAULT_480B_CTX_SIZE,
            parallel: DEFAULT_480B_PARALLEL,
            rpc_shard_count: DEFAULT_480B_RPC_SHARD_COUNT,
            rpc_ring_topology: DEFAULT_480B_RPC_RING_TOPOLOGY.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rpc480bShardRecipe {
    pub worker_name: String,
    pub shard_id: u32,
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rpc480bRingRecipe {
    pub model: String,
    pub endpoint_url: String,
    pub rpc_shard_count: u32,
    pub rpc_ring_topology: String,
    pub shards: Vec<Rpc480bShardRecipe>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Rpc480bRecipeError {
    #[error("480B RPC shard count must be greater than zero")]
    EmptyShardCount,
    #[error("480B RPC ring topology must name at least one worker")]
    EmptyTopology,
    #[error("480B RPC topology has {topology_len} worker(s), expected {shard_count}")]
    TopologyShardCountMismatch {
        topology_len: usize,
        shard_count: u32,
    },
}

/// Build the orchestrator recipe for the 480B llama.cpp RPC ring.
///
/// The recipe is intentionally data-only: callers can render, approve, or
/// dispatch the returned per-shard commands through the fleet scheduler.
pub fn orchestrator_480b_ring_rpc(
    config: Rpc480bRingConfig,
) -> Result<Rpc480bRingRecipe, Rpc480bRecipeError> {
    let topology = parse_topology(&config)?;
    let shards = topology
        .into_iter()
        .enumerate()
        .map(|(idx, worker_name)| Rpc480bShardRecipe {
            worker_name,
            shard_id: idx as u32,
            program: config.binary.clone(),
            args: llama_server_args(&config, idx as u32),
        })
        .collect();

    Ok(Rpc480bRingRecipe {
        model: config.model,
        endpoint_url: config.endpoint_url,
        rpc_shard_count: config.rpc_shard_count,
        rpc_ring_topology: config.rpc_ring_topology,
        shards,
    })
}

fn parse_topology(config: &Rpc480bRingConfig) -> Result<Vec<String>, Rpc480bRecipeError> {
    if config.rpc_shard_count == 0 {
        return Err(Rpc480bRecipeError::EmptyShardCount);
    }
    let topology: Vec<String> = config
        .rpc_ring_topology
        .split(',')
        .map(str::trim)
        .filter(|worker| !worker.is_empty())
        .map(str::to_string)
        .collect();
    if topology.is_empty() {
        return Err(Rpc480bRecipeError::EmptyTopology);
    }
    if topology.len() != config.rpc_shard_count as usize {
        return Err(Rpc480bRecipeError::TopologyShardCountMismatch {
            topology_len: topology.len(),
            shard_count: config.rpc_shard_count,
        });
    }
    Ok(topology)
}

fn llama_server_args(config: &Rpc480bRingConfig, shard_id: u32) -> Vec<String> {
    vec![
        "--model".to_string(),
        config.model_path.clone(),
        "--host".to_string(),
        config.host.clone(),
        "--port".to_string(),
        config.port.to_string(),
        "--ctx-size".to_string(),
        config.ctx_size.to_string(),
        "--parallel".to_string(),
        config.parallel.to_string(),
        "--mlock".to_string(),
        "--metrics".to_string(),
        "-lv".to_string(),
        "2".to_string(),
        "--rpc-shard-id".to_string(),
        shard_id.to_string(),
        "--rpc-shard-count".to_string(),
        config.rpc_shard_count.to_string(),
        "--rpc-ring-topology".to_string(),
        config.rpc_ring_topology.clone(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_recipe_matches_canonical_480b_ring() {
        let recipe = orchestrator_480b_ring_rpc(Rpc480bRingConfig::default()).unwrap();

        assert_eq!(recipe.model, DEFAULT_480B_MODEL);
        assert_eq!(recipe.endpoint_url, DEFAULT_480B_ENDPOINT_URL);
        assert_eq!(recipe.rpc_shard_count, 3);
        assert_eq!(recipe.rpc_ring_topology, "adele,rihanna,beyonce");
        assert_eq!(
            recipe
                .shards
                .iter()
                .map(|shard| (shard.worker_name.as_str(), shard.shard_id))
                .collect::<Vec<_>>(),
            vec![("adele", 0), ("rihanna", 1), ("beyonce", 2)]
        );
    }

    #[test]
    fn shard_commands_include_rpc_ring_flags() {
        let recipe = orchestrator_480b_ring_rpc(Rpc480bRingConfig {
            model_path: "/srv/models/qwen3-coder-480b.gguf".to_string(),
            rpc_shard_count: 2,
            rpc_ring_topology: "a,b".to_string(),
            ..Rpc480bRingConfig::default()
        })
        .unwrap();

        let second = &recipe.shards[1];
        assert_eq!(second.worker_name, "b");
        assert_eq!(second.program, "llama-server");
        assert!(second.args.windows(2).any(|w| w == ["--rpc-shard-id", "1"]));
        assert!(
            second
                .args
                .windows(2)
                .any(|w| w == ["--rpc-shard-count", "2"])
        );
        assert!(
            second
                .args
                .windows(2)
                .any(|w| w == ["--rpc-ring-topology", "a,b"])
        );
        assert!(
            second
                .args
                .windows(2)
                .any(|w| w == ["--model", "/srv/models/qwen3-coder-480b.gguf"])
        );
    }

    #[test]
    fn topology_must_match_shard_count() {
        let err = orchestrator_480b_ring_rpc(Rpc480bRingConfig {
            rpc_shard_count: 3,
            rpc_ring_topology: "a,b".to_string(),
            ..Rpc480bRingConfig::default()
        })
        .unwrap_err();

        assert_eq!(
            err,
            Rpc480bRecipeError::TopologyShardCountMismatch {
                topology_len: 2,
                shard_count: 3,
            }
        );
    }
}
