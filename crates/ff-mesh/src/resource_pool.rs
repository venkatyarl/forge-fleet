//! Aggregate fleet resources — total memory, total compute, available capacity.
//! Track per-node resource usage.

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};
use uuid::Uuid;

// ─── Per-Node Resources ──────────────────────────────────────────────────────

/// Resource tracking for a single node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeResources {
    /// Node ID.
    pub node_id: Uuid,
    /// Total memory (GiB).
    pub total_memory_gib: u64,
    /// Total CPU cores.
    pub total_cpu_cores: u32,
    /// Whether the node has a GPU.
    pub has_gpu: bool,
    /// Memory usage percent (0.0–100.0).
    pub memory_usage_pct: f32,
    /// CPU usage percent (0.0–100.0).
    pub cpu_usage_pct: f32,
    /// GPU usage percent (0.0–100.0).
    pub gpu_usage_pct: f32,
    /// Whether the node is online and contributing.
    pub online: bool,
}

/// Fleet-wide resource summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetResourceSummary {
    /// Total nodes in pool.
    pub total_nodes: usize,
    /// Online nodes.
    pub online_nodes: usize,
    /// Total memory across all online nodes (GiB).
    pub total_memory_gib: u64,
    /// Available memory across all online nodes (GiB, estimated).
    pub available_memory_gib: u64,
    /// Total CPU cores across all online nodes.
    pub total_cpu_cores: u32,
    /// Available CPU cores (estimated from usage).
    pub available_cpu_cores: u32,
    /// Number of GPU-capable online nodes.
    pub gpu_nodes: usize,
    /// Average CPU usage across fleet (0.0–100.0).
    pub avg_cpu_usage: f32,
    /// Average memory usage across fleet (0.0–100.0).
    pub avg_memory_usage: f32,
}

// ─── Resource Pool ───────────────────────────────────────────────────────────

/// Fleet-wide resource pool — aggregates and tracks resources across all nodes.
pub struct ResourcePool {
    /// Per-node resource tracking.
    nodes: DashMap<Uuid, NodeResources>,
}

impl ResourcePool {
    /// Create an empty resource pool.
    pub fn new() -> Self {
        Self {
            nodes: DashMap::new(),
        }
    }

    /// Add a new node to the resource pool.
    pub fn add_node(
        &self,
        node_id: Uuid,
        total_memory_gib: u64,
        total_cpu_cores: u32,
        has_gpu: bool,
    ) {
        let resources = NodeResources {
            node_id,
            total_memory_gib,
            total_cpu_cores,
            has_gpu,
            memory_usage_pct: 0.0,
            cpu_usage_pct: 0.0,
            gpu_usage_pct: 0.0,
            online: true,
        };

        self.nodes.insert(node_id, resources);
        info!(
            node_id = %node_id,
            memory_gib = total_memory_gib,
            cpu_cores = total_cpu_cores,
            has_gpu,
            "node added to resource pool"
        );
    }

    /// Remove a node from the resource pool.
    pub fn remove_node(&self, node_id: Uuid) {
        self.nodes.remove(&node_id);
        info!(node_id = %node_id, "node removed from resource pool");
    }

    /// Update resource usage for a node.
    pub fn update_usage(
        &self,
        node_id: Uuid,
        memory_usage_pct: f32,
        cpu_usage_pct: f32,
        gpu_usage_pct: f32,
    ) {
        if let Some(mut entry) = self.nodes.get_mut(&node_id) {
            entry.memory_usage_pct = memory_usage_pct;
            entry.cpu_usage_pct = cpu_usage_pct;
            entry.gpu_usage_pct = gpu_usage_pct;
            debug!(
                node_id = %node_id,
                cpu = cpu_usage_pct,
                mem = memory_usage_pct,
                gpu = gpu_usage_pct,
                "resource usage updated"
            );
        }
    }

    /// Mark a node as offline.
    pub fn mark_offline(&self, node_id: Uuid) {
        if let Some(mut entry) = self.nodes.get_mut(&node_id) {
            entry.online = false;
            info!(node_id = %node_id, "node marked offline in resource pool");
        }
    }

    /// Mark a node as online.
    pub fn mark_online(&self, node_id: Uuid) {
        if let Some(mut entry) = self.nodes.get_mut(&node_id) {
            entry.online = true;
            info!(node_id = %node_id, "node marked online in resource pool");
        }
    }

    /// Get resources for a specific node.
    pub fn get_node(&self, node_id: &Uuid) -> Option<NodeResources> {
        self.nodes.get(node_id).map(|entry| entry.clone())
    }

    /// Get all node resources.
    pub fn all_nodes(&self) -> Vec<NodeResources> {
        self.nodes
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Compute fleet-wide resource summary.
    pub fn summary(&self) -> FleetResourceSummary {
        let total_nodes = self.nodes.len();
        let mut online_nodes = 0usize;
        let mut total_memory: u64 = 0;
        let mut available_memory: u64 = 0;
        let mut total_cores: u32 = 0;
        let mut available_cores: u32 = 0;
        let mut gpu_nodes = 0usize;
        let mut total_cpu_pct: f32 = 0.0;
        let mut total_mem_pct: f32 = 0.0;

        for entry in self.nodes.iter() {
            let node = entry.value();
            if !node.online {
                continue;
            }

            online_nodes += 1;
            total_memory += node.total_memory_gib;
            total_cores += node.total_cpu_cores;

            // Estimate available resources.
            let used_mem = (node.total_memory_gib as f32 * node.memory_usage_pct / 100.0) as u64;
            available_memory += node.total_memory_gib.saturating_sub(used_mem);

            let used_cores = (node.total_cpu_cores as f32 * node.cpu_usage_pct / 100.0) as u32;
            available_cores += node.total_cpu_cores.saturating_sub(used_cores);

            if node.has_gpu {
                gpu_nodes += 1;
            }

            total_cpu_pct += node.cpu_usage_pct;
            total_mem_pct += node.memory_usage_pct;
        }

        let count = online_nodes.max(1) as f32;

        FleetResourceSummary {
            total_nodes,
            online_nodes,
            total_memory_gib: total_memory,
            available_memory_gib: available_memory,
            total_cpu_cores: total_cores,
            available_cpu_cores: available_cores,
            gpu_nodes,
            avg_cpu_usage: total_cpu_pct / count,
            avg_memory_usage: total_mem_pct / count,
        }
    }

    /// Check if the fleet has enough resources for a given requirement.
    pub fn has_capacity(&self, required_memory_gib: u64, requires_gpu: bool) -> bool {
        let summary = self.summary();

        if summary.available_memory_gib < required_memory_gib {
            return false;
        }

        if requires_gpu && summary.gpu_nodes == 0 {
            return false;
        }

        true
    }

    /// Find nodes with the most available resources.
    /// Returns node IDs sorted by available capacity (most available first).
    pub fn most_available(&self) -> Vec<Uuid> {
        let mut nodes: Vec<(Uuid, f64)> = self
            .nodes
            .iter()
            .filter(|entry| entry.value().online)
            .map(|entry| {
                let node = entry.value();
                // Score: lower usage = higher availability.
                let availability = 300.0
                    - node.cpu_usage_pct as f64
                    - node.memory_usage_pct as f64
                    - node.gpu_usage_pct as f64;
                (node.node_id, availability)
            })
            .collect();

        nodes.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        nodes.into_iter().map(|(id, _)| id).collect()
    }
}

impl Default for ResourcePool {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_and_get_node() {
        let pool = ResourcePool::new();
        let id = Uuid::new_v4();

        pool.add_node(id, 128, 16, true);

        let node = pool.get_node(&id).unwrap();
        assert_eq!(node.total_memory_gib, 128);
        assert_eq!(node.total_cpu_cores, 16);
        assert!(node.has_gpu);
        assert!(node.online);
    }

    #[test]
    fn test_update_usage() {
        let pool = ResourcePool::new();
        let id = Uuid::new_v4();

        pool.add_node(id, 64, 8, false);
        pool.update_usage(id, 45.0, 30.0, 0.0);

        let node = pool.get_node(&id).unwrap();
        assert_eq!(node.memory_usage_pct, 45.0);
        assert_eq!(node.cpu_usage_pct, 30.0);
    }

    #[test]
    fn test_mark_offline() {
        let pool = ResourcePool::new();
        let id = Uuid::new_v4();

        pool.add_node(id, 64, 8, false);
        assert!(pool.get_node(&id).unwrap().online);

        pool.mark_offline(id);
        assert!(!pool.get_node(&id).unwrap().online);
    }

    #[test]
    fn test_remove_node() {
        let pool = ResourcePool::new();
        let id = Uuid::new_v4();

        pool.add_node(id, 64, 8, false);
        assert!(pool.get_node(&id).is_some());

        pool.remove_node(id);
        assert!(pool.get_node(&id).is_none());
    }

    #[test]
    fn test_fleet_summary() {
        let pool = ResourcePool::new();

        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let id3 = Uuid::new_v4();

        pool.add_node(id1, 128, 16, true); // Taylor
        pool.add_node(id2, 64, 8, false); // James
        pool.add_node(id3, 32, 8, false); // Marcus

        pool.update_usage(id1, 30.0, 20.0, 10.0);
        pool.update_usage(id2, 50.0, 40.0, 0.0);
        pool.update_usage(id3, 20.0, 10.0, 0.0);

        let summary = pool.summary();
        assert_eq!(summary.total_nodes, 3);
        assert_eq!(summary.online_nodes, 3);
        assert_eq!(summary.total_memory_gib, 224); // 128 + 64 + 32
        assert_eq!(summary.total_cpu_cores, 32); // 16 + 8 + 8
        assert_eq!(summary.gpu_nodes, 1);
    }

    #[test]
    fn test_has_capacity() {
        let pool = ResourcePool::new();
        let id = Uuid::new_v4();

        pool.add_node(id, 128, 16, true);
        pool.update_usage(id, 50.0, 30.0, 10.0);

        assert!(pool.has_capacity(60, true)); // 64 GiB available, has GPU.
        assert!(!pool.has_capacity(200, false)); // Not enough memory.
    }

    #[test]
    fn test_offline_nodes_excluded_from_summary() {
        let pool = ResourcePool::new();

        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        pool.add_node(id1, 128, 16, true);
        pool.add_node(id2, 64, 8, false);

        pool.mark_offline(id2);

        let summary = pool.summary();
        assert_eq!(summary.total_nodes, 2);
        assert_eq!(summary.online_nodes, 1);
        assert_eq!(summary.total_memory_gib, 128); // Only online node.
    }

    #[test]
    fn test_most_available() {
        let pool = ResourcePool::new();

        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        pool.add_node(id1, 128, 16, true);
        pool.add_node(id2, 64, 8, false);

        pool.update_usage(id1, 80.0, 70.0, 60.0); // Heavily loaded.
        pool.update_usage(id2, 10.0, 5.0, 0.0); // Mostly idle.

        let ranked = pool.most_available();
        assert_eq!(ranked[0], id2); // Marcus first — more available.
        assert_eq!(ranked[1], id1);
    }
}
