//! llama.cpp RPC distributed inference — protocol integration for tensor parallelism.
//!
//! llama.cpp supports distributed inference via its RPC system:
//! - One controller node handles tokenization and scheduling
//! - Worker nodes run RPC servers exposing GPU memory and compute
//! - Model layers are split across nodes proportional to available memory
//!
//! This module manages the RPC topology and integrates with ff-mesh for node selection.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};
use tracing::info;

/// RPC worker node in the distributed inference cluster.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcWorker {
    pub name: String,
    pub address: SocketAddr,
    pub available_memory_gb: u32,
    pub gpu_type: String,
    pub status: RpcWorkerStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RpcWorkerStatus {
    Available,
    Busy,
    Offline,
}

/// Configuration for a distributed inference cluster.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcClusterConfig {
    /// Controller node (runs the main llama-server with --rpc).
    pub controller: RpcControllerConfig,
    /// Worker nodes (run rpc-server binary).
    pub workers: Vec<RpcWorkerConfig>,
    /// Model to load (GGUF path or URL).
    pub model: String,
    /// Tensor split ratios (proportional to memory, auto-computed if empty).
    pub tensor_split: Vec<f32>,
    /// Context size.
    pub context_size: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcControllerConfig {
    pub worker_name: String,
    pub bind_port: u16,
    /// GPU layers on controller (0 = CPU only on controller).
    pub gpu_layers: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcWorkerConfig {
    pub worker_name: String,
    pub address: String,
    pub port: u16,
    pub memory_gb: u32,
    /// Zero-based tensor shard exposed by this RPC worker.
    pub rpc_shard_id: u32,
    /// Total number of tensor shards in the RPC cluster.
    pub rpc_shard_count: u32,
    /// QSFP ring topology passed through to llama.cpp.
    pub rpc_ring_topology: String,
}

/// Manage a distributed inference cluster using llama.cpp RPC.
pub struct RpcClusterManager {
    config: Option<RpcClusterConfig>,
    #[allow(dead_code)]
    workers: HashMap<String, RpcWorker>,
}

impl RpcClusterManager {
    pub fn new() -> Self {
        Self {
            config: None,
            workers: HashMap::new(),
        }
    }

    /// Configure and validate a cluster.
    pub fn configure(&mut self, config: RpcClusterConfig) -> anyhow::Result<()> {
        // Validate: need at least one worker
        if config.workers.is_empty() {
            anyhow::bail!("RPC cluster needs at least one worker node");
        }

        // Compute tensor split if not provided
        let total_memory: u32 = config.workers.iter().map(|w| w.memory_gb).sum();
        if total_memory == 0 {
            anyhow::bail!("Total worker memory is 0");
        }

        let expected_shard_count = config.workers.len() as u32;
        let expected_topology = &config.workers[0].rpc_ring_topology;
        if expected_topology.trim().is_empty() {
            anyhow::bail!("RPC ring topology must not be empty");
        }

        let mut shard_ids = HashSet::with_capacity(config.workers.len());
        for worker in &config.workers {
            if worker.rpc_shard_count != expected_shard_count {
                anyhow::bail!(
                    "RPC worker '{}' shard count {} does not match worker count {}",
                    worker.worker_name,
                    worker.rpc_shard_count,
                    expected_shard_count
                );
            }
            if worker.rpc_shard_id >= worker.rpc_shard_count {
                anyhow::bail!(
                    "RPC worker '{}' shard id {} is out of range for {} shards",
                    worker.worker_name,
                    worker.rpc_shard_id,
                    worker.rpc_shard_count
                );
            }
            if !shard_ids.insert(worker.rpc_shard_id) {
                anyhow::bail!("Duplicate RPC shard id {}", worker.rpc_shard_id);
            }
            if worker.rpc_ring_topology != *expected_topology {
                anyhow::bail!("All RPC workers must use the same ring topology");
            }
        }

        info!(
            controller = %config.controller.worker_name,
            workers = config.workers.len(),
            total_memory_gb = total_memory,
            model = %config.model,
            "RPC cluster configured"
        );

        self.config = Some(config);
        Ok(())
    }

    /// Generate the llama-server command line for the controller.
    pub fn controller_command(&self) -> anyhow::Result<Vec<String>> {
        let config = self
            .config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Cluster not configured"))?;

        let mut args = vec![
            "llama-server".to_string(),
            "--model".to_string(),
            config.model.clone(),
            "--ctx-size".to_string(),
            config.context_size.to_string(),
            "--host".to_string(),
            "0.0.0.0".to_string(),
            "--port".to_string(),
            config.controller.bind_port.to_string(),
        ];

        // Add RPC servers
        let rpc_addrs: Vec<String> = config
            .workers
            .iter()
            .map(|w| format!("{}:{}", w.address, w.port))
            .collect();
        args.push("--rpc".to_string());
        args.push(rpc_addrs.join(","));

        // Add tensor split if specified
        if !config.tensor_split.is_empty() {
            args.push("--tensor-split".to_string());
            let split_str: Vec<String> = config
                .tensor_split
                .iter()
                .map(|s| format!("{s:.2}"))
                .collect();
            args.push(split_str.join(","));
        }

        // GPU layers on controller
        if config.controller.gpu_layers > 0 {
            args.push("--n-gpu-layers".to_string());
            args.push(config.controller.gpu_layers.to_string());
        }

        Ok(args)
    }

    /// Generate the rpc-server command line for a worker.
    pub fn worker_command(&self, worker_name: &str) -> anyhow::Result<Vec<String>> {
        let config = self
            .config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Cluster not configured"))?;

        let worker = config
            .workers
            .iter()
            .find(|w| w.worker_name == worker_name)
            .ok_or_else(|| anyhow::anyhow!("Worker '{worker_name}' not found in cluster config"))?;

        Ok(vec![
            "rpc-server".to_string(),
            "--host".to_string(),
            "0.0.0.0".to_string(),
            "--port".to_string(),
            worker.port.to_string(),
            "--rpc-shard-id".to_string(),
            worker.rpc_shard_id.to_string(),
            "--rpc-shard-count".to_string(),
            worker.rpc_shard_count.to_string(),
            "--rpc-ring-topology".to_string(),
            worker.rpc_ring_topology.clone(),
        ])
    }

    /// Auto-compute tensor split ratios based on worker memory.
    pub fn auto_tensor_split(workers: &[RpcWorkerConfig]) -> Vec<f32> {
        let total: f32 = workers.iter().map(|w| w.memory_gb as f32).sum();
        if total == 0.0 {
            return vec![1.0 / workers.len() as f32; workers.len()];
        }
        workers.iter().map(|w| w.memory_gb as f32 / total).collect()
    }

    /// Example: configure a 4-node DGX Spark cluster for a 405B model.
    pub fn example_dgx_spark_cluster() -> RpcClusterConfig {
        RpcClusterConfig {
            controller: RpcControllerConfig {
                worker_name: "sia".into(),
                bind_port: 51000,
                gpu_layers: 999, // offload everything to GPU
            },
            workers: vec![
                RpcWorkerConfig {
                    worker_name: "adele".into(),
                    address: "192.168.5.110".into(),
                    port: 50052,
                    memory_gb: 128,
                    rpc_shard_id: 0,
                    rpc_shard_count: 3,
                    rpc_ring_topology: "adele,rihanna,beyonce".into(),
                },
                RpcWorkerConfig {
                    worker_name: "rihanna".into(),
                    address: "192.168.5.112".into(),
                    port: 50052,
                    memory_gb: 128,
                    rpc_shard_id: 1,
                    rpc_shard_count: 3,
                    rpc_ring_topology: "adele,rihanna,beyonce".into(),
                },
                RpcWorkerConfig {
                    worker_name: "beyonce".into(),
                    address: "192.168.5.114".into(),
                    port: 50052,
                    memory_gb: 128,
                    rpc_shard_id: 2,
                    rpc_shard_count: 3,
                    rpc_ring_topology: "adele,rihanna,beyonce".into(),
                },
            ],
            model: "/models/Llama-3.1-405B-Instruct-Q4_K_M.gguf".into(),
            tensor_split: vec![], // auto-compute
            context_size: 32768,
        }
    }

    /// Example: configure a 4-node EVO-X2 cluster.
    pub fn example_evo_x2_cluster() -> RpcClusterConfig {
        RpcClusterConfig {
            controller: RpcControllerConfig {
                worker_name: "evo1".into(),
                bind_port: 51000,
                gpu_layers: 999,
            },
            workers: vec![
                RpcWorkerConfig {
                    worker_name: "evo2".into(),
                    address: "192.168.5.120".into(),
                    port: 50052,
                    memory_gb: 128,
                    rpc_shard_id: 0,
                    rpc_shard_count: 3,
                    rpc_ring_topology: "evo2,evo3,evo4".into(),
                },
                RpcWorkerConfig {
                    worker_name: "evo3".into(),
                    address: "192.168.5.122".into(),
                    port: 50052,
                    memory_gb: 128,
                    rpc_shard_id: 1,
                    rpc_shard_count: 3,
                    rpc_ring_topology: "evo2,evo3,evo4".into(),
                },
                RpcWorkerConfig {
                    worker_name: "evo4".into(),
                    address: "192.168.5.124".into(),
                    port: 50052,
                    memory_gb: 128,
                    rpc_shard_id: 2,
                    rpc_shard_count: 3,
                    rpc_ring_topology: "evo2,evo3,evo4".into(),
                },
            ],
            model: "/models/Qwen3-235B-A22B-Q4_K_M.gguf".into(),
            tensor_split: vec![],
            context_size: 32768,
        }
    }
}

impl Default for RpcClusterManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> RpcClusterConfig {
        RpcClusterConfig {
            controller: RpcControllerConfig {
                worker_name: "controller".into(),
                bind_port: 51000,
                gpu_layers: 0,
            },
            workers: (0..3)
                .map(|rpc_shard_id| RpcWorkerConfig {
                    worker_name: format!("worker-{rpc_shard_id}"),
                    address: format!("10.0.0.{}", rpc_shard_id + 1),
                    port: 50052,
                    memory_gb: 128,
                    rpc_shard_id,
                    rpc_shard_count: 3,
                    rpc_ring_topology: "worker-0,worker-1,worker-2".into(),
                })
                .collect(),
            model: "/models/model.gguf".into(),
            tensor_split: vec![],
            context_size: 32768,
        }
    }

    #[test]
    fn worker_command_exposes_tensor_shard_flags() {
        let mut manager = RpcClusterManager::new();
        manager.configure(test_config()).unwrap();

        assert_eq!(
            manager.worker_command("worker-1").unwrap(),
            vec![
                "rpc-server",
                "--host",
                "0.0.0.0",
                "--port",
                "50052",
                "--rpc-shard-id",
                "1",
                "--rpc-shard-count",
                "3",
                "--rpc-ring-topology",
                "worker-0,worker-1,worker-2",
            ]
        );
    }

    #[test]
    fn configure_rejects_invalid_shard_topology() {
        let mut duplicate = test_config();
        duplicate.workers[1].rpc_shard_id = 0;
        assert!(
            RpcClusterManager::new()
                .configure(duplicate)
                .unwrap_err()
                .to_string()
                .contains("Duplicate RPC shard id")
        );

        let mut wrong_count = test_config();
        wrong_count.workers[0].rpc_shard_count = 4;
        assert!(
            RpcClusterManager::new()
                .configure(wrong_count)
                .unwrap_err()
                .to_string()
                .contains("does not match worker count")
        );

        let mut mismatched_ring = test_config();
        mismatched_ring.workers[2].rpc_ring_topology = "another-ring".into();
        assert!(
            RpcClusterManager::new()
                .configure(mismatched_ring)
                .unwrap_err()
                .to_string()
                .contains("same ring topology")
        );
    }
}
