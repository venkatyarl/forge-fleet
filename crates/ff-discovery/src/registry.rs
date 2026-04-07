//! Thread-safe node registry for ForgeFleet discovery.
//!
//! Provides a `DashMap`-backed registry that tracks all known fleet nodes
//! with multiple indices (ID, IP, config name) and supports:
//! - CRUD operations (add / update / remove / get / list)
//! - Config-sourced nodes (from fleet.toml)
//! - Discovery-sourced nodes (from subnet scans)
//! - Health-check integration (apply scan results)
//! - Stale-node detection (mark offline after 90s)
//! - Election data extraction (health tuples for leader election)

use crate::hardware::HardwareProfile;
use crate::health::{HealthCheckResult, HealthStatus};
use crate::models::ModelCard;
use crate::scanner::{DiscoveredNode, NodeScanResult, NodeScanStatus};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::sync::RwLock;
use tracing::{debug, info, warn};
use uuid::Uuid;

// ─── FleetNode ───────────────────────────────────────────────────────────────

/// A tracked node in the fleet registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetNode {
    /// Unique node ID (auto-generated).
    pub id: Uuid,
    /// IP address.
    pub ip: IpAddr,
    /// Resolved hostname (if available).
    pub hostname: Option<String>,
    /// Fleet config name (e.g. "taylor", "james"). Set for config-sourced nodes.
    pub config_name: Option<String>,
    /// Election priority from config (lower = more preferred).
    pub election_priority: Option<u32>,
    /// Primary API port for this node.
    pub api_port: Option<u16>,
    /// Open ports discovered via subnet scan.
    pub open_ports: Vec<u16>,
    /// When this node was first discovered/registered.
    pub discovered_at: DateTime<Utc>,
    /// Last time this node was seen (scan, heartbeat, or registration).
    pub last_seen: DateTime<Utc>,
    /// Detected hardware profile.
    pub hardware: Option<HardwareProfile>,
    /// Latest health check result.
    pub health: Option<HealthCheckResult>,
    /// Models served on this node.
    pub models: Vec<ModelCard>,
}

impl FleetNode {
    /// Create a FleetNode from a subnet-discovered node.
    fn from_discovery(id: Uuid, discovered: DiscoveredNode) -> Self {
        Self {
            id,
            ip: discovered.ip,
            hostname: None,
            config_name: None,
            election_priority: None,
            api_port: None,
            open_ports: discovered.open_ports,
            discovered_at: discovered.discovered_at,
            last_seen: discovered.discovered_at,
            hardware: None,
            health: None,
            models: vec![],
        }
    }

    /// Create a FleetNode from fleet.toml configuration.
    pub fn from_config(name: &str, ip: IpAddr, port: u16, priority: u32) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            ip,
            hostname: None,
            config_name: Some(name.to_string()),
            election_priority: Some(priority),
            api_port: Some(port),
            open_ports: vec![port],
            discovered_at: now,
            last_seen: now,
            hardware: None,
            health: None,
            models: vec![],
        }
    }

    /// Returns `true` if the node is considered healthy (Healthy or Degraded).
    pub fn is_healthy(&self) -> bool {
        self.health.as_ref().map_or(false, |h| {
            matches!(h.status, HealthStatus::Healthy | HealthStatus::Degraded)
        })
    }

    /// Returns `true` if the node is online (Healthy status only).
    pub fn is_online(&self) -> bool {
        self.health
            .as_ref()
            .map_or(false, |h| matches!(h.status, HealthStatus::Healthy))
    }

    /// Returns `true` if last_seen is older than `stale_after_secs`.
    pub fn is_stale(&self, stale_after_secs: i64) -> bool {
        Utc::now()
            .signed_duration_since(self.last_seen)
            .num_seconds()
            > stale_after_secs
    }
}

// ─── NodeRegistry ────────────────────────────────────────────────────────────

/// Thread-safe registry of all known fleet nodes.
///
/// Uses `DashMap` for lock-free concurrent read/write access with
/// multiple indices: by UUID, by IP address, and by config name.
#[derive(Debug)]
pub struct NodeRegistry {
    /// Primary store: node ID → FleetNode.
    nodes: DashMap<Uuid, FleetNode>,
    /// IP address → node ID index.
    ip_index: DashMap<IpAddr, Uuid>,
    /// Config name → node ID index (e.g. "taylor" → uuid).
    name_index: DashMap<String, Uuid>,
    /// Current leader name, updated via election or announcement.
    current_leader: RwLock<Option<String>>,
}

impl Default for NodeRegistry {
    fn default() -> Self {
        Self {
            nodes: DashMap::new(),
            ip_index: DashMap::new(),
            name_index: DashMap::new(),
            current_leader: RwLock::new(None),
        }
    }
}

impl NodeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    // ── Basic queries ────────────────────────────────────────────────────────

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// List all nodes, sorted by IP address.
    pub fn list_nodes(&self) -> Vec<FleetNode> {
        let mut nodes: Vec<FleetNode> = self.nodes.iter().map(|n| n.value().clone()).collect();
        nodes.sort_by_key(|node| node.ip);
        nodes
    }

    /// Get a node by its UUID.
    pub fn get_node(&self, id: Uuid) -> Option<FleetNode> {
        self.nodes.get(&id).map(|n| n.value().clone())
    }

    /// Get a node by IP address.
    pub fn get_node_by_ip(&self, ip: IpAddr) -> Option<FleetNode> {
        let id = self.ip_index.get(&ip).map(|v| *v.value())?;
        self.get_node(id)
    }

    /// Get a node by config name (e.g. "taylor").
    pub fn get_node_by_name(&self, name: &str) -> Option<FleetNode> {
        let id = self.name_index.get(name).map(|v| *v.value())?;
        self.get_node(id)
    }

    // ── Config-sourced upsert ────────────────────────────────────────────────

    /// Insert or update a node from fleet.toml config.
    ///
    /// If a node with the same IP or name already exists, it is updated
    /// with the config fields. Otherwise a new node is created.
    pub fn upsert_config_node(&self, name: &str, ip: IpAddr, port: u16, priority: u32) -> Uuid {
        // Check by IP first.
        if let Some(existing_id) = self.ip_index.get(&ip).map(|v| *v.value()) {
            if let Some(mut node) = self.nodes.get_mut(&existing_id) {
                node.config_name = Some(name.to_string());
                node.election_priority = Some(priority);
                node.api_port = Some(port);
                if !node.open_ports.contains(&port) {
                    node.open_ports.push(port);
                }
            }
            self.name_index.insert(name.to_string(), existing_id);
            return existing_id;
        }

        // Check by name.
        if let Some(existing_id) = self.name_index.get(name).map(|v| *v.value()) {
            if let Some(mut node) = self.nodes.get_mut(&existing_id) {
                let old_ip = node.ip;
                node.ip = ip;
                node.election_priority = Some(priority);
                node.api_port = Some(port);
                if !node.open_ports.contains(&port) {
                    node.open_ports.push(port);
                }

                // Update IP index if IP changed.
                if old_ip != ip {
                    self.ip_index.remove(&old_ip);
                    self.ip_index.insert(ip, existing_id);
                }
            }
            return existing_id;
        }

        // New node from config.
        let node = FleetNode::from_config(name, ip, port, priority);
        let id = node.id;

        self.ip_index.insert(ip, id);
        self.name_index.insert(name.to_string(), id);
        self.nodes.insert(id, node);

        debug!(name, %ip, port, priority, "registered config node");
        id
    }

    // ── Discovery-sourced upsert ─────────────────────────────────────────────

    /// Insert or update a node from a subnet scan discovery.
    pub fn upsert_discovered_node(&self, discovered: DiscoveredNode) -> Uuid {
        if let Some(existing_id) = self.ip_index.get(&discovered.ip).map(|v| *v.value()) {
            if let Some(mut node) = self.nodes.get_mut(&existing_id) {
                node.open_ports = discovered.open_ports;
                node.last_seen = discovered.discovered_at;
            }
            return existing_id;
        }

        let id = Uuid::new_v4();
        let ip = discovered.ip;
        let node = FleetNode::from_discovery(id, discovered);

        self.nodes.insert(id, node);
        self.ip_index.insert(ip, id);

        id
    }

    /// Batch upsert from subnet scan results.
    pub fn upsert_many_discovered(&self, discovered_nodes: Vec<DiscoveredNode>) -> Vec<Uuid> {
        discovered_nodes
            .into_iter()
            .map(|node| self.upsert_discovered_node(node))
            .collect()
    }

    // ── Remove ───────────────────────────────────────────────────────────────

    /// Remove a node by ID. Returns the removed node if it existed.
    pub fn remove_node(&self, id: Uuid) -> Option<FleetNode> {
        let removed = self.nodes.remove(&id).map(|(_, node)| node);
        if let Some(node) = &removed {
            self.ip_index.remove(&node.ip);
            if let Some(ref name) = node.config_name {
                self.name_index.remove(name);
            }
        }
        removed
    }

    // ── Field updates ────────────────────────────────────────────────────────

    /// Update last_seen timestamp for a node (heartbeat touch).
    pub fn touch(&self, id: Uuid) {
        if let Some(mut node) = self.nodes.get_mut(&id) {
            node.last_seen = Utc::now();
        }
    }

    /// Update last_seen by name.
    pub fn touch_by_name(&self, name: &str) {
        if let Some(id) = self.name_index.get(name).map(|v| *v.value()) {
            self.touch(id);
        }
    }

    pub fn set_hostname(&self, id: Uuid, hostname: String) -> bool {
        if let Some(mut node) = self.nodes.get_mut(&id) {
            node.hostname = Some(hostname);
            return true;
        }
        false
    }

    pub fn update_hardware(&self, id: Uuid, hardware: HardwareProfile) -> bool {
        if let Some(mut node) = self.nodes.get_mut(&id) {
            node.hardware = Some(hardware);
            node.last_seen = Utc::now();
            return true;
        }
        false
    }

    pub fn update_health_by_ip(&self, ip: IpAddr, health: HealthCheckResult) -> bool {
        let Some(id) = self.ip_index.get(&ip).map(|v| *v.value()) else {
            return false;
        };

        if let Some(mut node) = self.nodes.get_mut(&id) {
            node.health = Some(health);
            node.last_seen = Utc::now();
            return true;
        }

        false
    }

    pub fn update_models_by_ip(&self, ip: IpAddr, models: Vec<ModelCard>) -> bool {
        let Some(id) = self.ip_index.get(&ip).map(|v| *v.value()) else {
            return false;
        };

        if let Some(mut node) = self.nodes.get_mut(&id) {
            node.models = models;
            node.last_seen = Utc::now();
            return true;
        }

        false
    }

    // ── Scan result integration ──────────────────────────────────────────────

    /// Apply a single [`NodeScanResult`] to the registry.
    ///
    /// Looks up the node by name, then updates its health and last_seen.
    pub fn apply_scan_result(&self, result: &NodeScanResult) {
        let id = self.name_index.get(&result.name).map(|v| *v.value());

        // Fall back to IP-based lookup.
        let id = id.or_else(|| {
            result
                .host
                .parse::<IpAddr>()
                .ok()
                .and_then(|ip| self.ip_index.get(&ip).map(|v| *v.value()))
        });

        let Some(id) = id else {
            debug!(
                node = %result.name,
                "scan result for unknown node — skipping"
            );
            return;
        };

        if let Some(mut node) = self.nodes.get_mut(&id) {
            let health_status = match result.status {
                NodeScanStatus::Online => HealthStatus::Healthy,
                NodeScanStatus::Degraded => HealthStatus::Degraded,
                NodeScanStatus::Offline => HealthStatus::Unreachable,
            };

            node.health = Some(HealthCheckResult {
                name: result.name.clone(),
                host: result.host.clone(),
                port: result.port,
                checked_at: result.scanned_at,
                latency_ms: result.latency_ms,
                tcp_ok: result.status != NodeScanStatus::Offline,
                http_ok: result.http_status.map(|s| s < 400),
                http_status: result.http_status,
                status: health_status,
                error: result.error.clone(),
            });

            node.last_seen = result.scanned_at;
        }
    }

    /// Apply all scan results from a fleet node scan round.
    pub fn apply_scan_results(&self, results: &[NodeScanResult]) {
        for result in results {
            self.apply_scan_result(result);
        }
    }

    // ── Stale detection ──────────────────────────────────────────────────────

    /// Mark nodes as unreachable if they haven't been seen in `stale_after_secs`.
    ///
    /// Returns the config names of nodes that were newly marked stale.
    pub fn mark_stale_nodes(&self, stale_after_secs: i64) -> Vec<String> {
        let now = Utc::now();
        let mut stale_names = Vec::new();

        for mut entry in self.nodes.iter_mut() {
            let node = entry.value_mut();
            let elapsed = now.signed_duration_since(node.last_seen).num_seconds();

            if elapsed > stale_after_secs {
                let was_healthy = node.is_healthy();

                // Build or update the health record to Unreachable.
                if let Some(ref mut health) = node.health {
                    if !matches!(health.status, HealthStatus::Unreachable) {
                        health.status = HealthStatus::Unreachable;
                    }
                } else {
                    // No health record yet — create one as unreachable.
                    node.health = Some(HealthCheckResult {
                        name: node
                            .config_name
                            .clone()
                            .unwrap_or_else(|| node.ip.to_string()),
                        host: node.ip.to_string(),
                        port: node.api_port.unwrap_or(0),
                        checked_at: now,
                        latency_ms: 0,
                        tcp_ok: false,
                        http_ok: None,
                        http_status: None,
                        status: HealthStatus::Unreachable,
                        error: Some(format!("stale: no heartbeat for {}s", elapsed)),
                    });
                }

                // Only report if the node was previously healthy.
                if was_healthy {
                    if let Some(ref name) = node.config_name {
                        stale_names.push(name.clone());
                    }
                }
            }
        }

        if !stale_names.is_empty() {
            warn!(nodes = ?stale_names, stale_after_secs, "nodes marked stale");
        }

        stale_names
    }

    // ── Election data ────────────────────────────────────────────────────────

    /// Extract health tuples for leader election.
    ///
    /// Returns `Vec<(name, is_healthy, is_yielding)>` for all config-sourced nodes.
    /// `is_yielding` is always `false` here — yielding is tracked by the activity
    /// subsystem and must be overlaid by the caller if needed.
    pub fn node_health_for_election(&self) -> Vec<(String, bool, bool)> {
        self.nodes
            .iter()
            .filter_map(|entry| {
                let node = entry.value();
                let name = node.config_name.as_ref()?;
                Some((name.clone(), node.is_healthy(), false))
            })
            .collect()
    }

    /// Get names of all online config-sourced nodes.
    pub fn online_node_names(&self) -> Vec<String> {
        self.nodes
            .iter()
            .filter_map(|entry| {
                let node = entry.value();
                if node.is_healthy() {
                    node.config_name.clone()
                } else {
                    None
                }
            })
            .collect()
    }

    // ── Leader tracking ──────────────────────────────────────────────────────

    /// Record the current leader name (from election result or announcement).
    pub fn set_leader(&self, name: String) {
        info!(leader = %name, "recording current leader");
        if let Ok(mut leader) = self.current_leader.write() {
            *leader = Some(name);
        }
    }

    /// Get the current leader name.
    pub fn current_leader(&self) -> Option<String> {
        self.current_leader.read().ok().and_then(|l| l.clone())
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn ip(last_octet: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 168, 5, last_octet))
    }

    #[test]
    fn test_new_registry_is_empty() {
        let reg = NodeRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert!(reg.list_nodes().is_empty());
    }

    #[test]
    fn test_upsert_config_node() {
        let reg = NodeRegistry::new();
        let id = reg.upsert_config_node("taylor", ip(100), 51800, 1);

        assert_eq!(reg.len(), 1);

        let node = reg.get_node(id).unwrap();
        assert_eq!(node.config_name.as_deref(), Some("taylor"));
        assert_eq!(node.election_priority, Some(1));
        assert_eq!(node.api_port, Some(51800));
        assert_eq!(node.ip, ip(100));
    }

    #[test]
    fn test_get_node_by_name() {
        let reg = NodeRegistry::new();
        reg.upsert_config_node("taylor", ip(100), 51800, 1);
        reg.upsert_config_node("james", ip(101), 51800, 2);

        let taylor = reg.get_node_by_name("taylor").unwrap();
        assert_eq!(taylor.ip, ip(100));

        let james = reg.get_node_by_name("james").unwrap();
        assert_eq!(james.ip, ip(101));

        assert!(reg.get_node_by_name("nonexistent").is_none());
    }

    #[test]
    fn test_get_node_by_ip() {
        let reg = NodeRegistry::new();
        reg.upsert_config_node("taylor", ip(100), 51800, 1);

        let node = reg.get_node_by_ip(ip(100)).unwrap();
        assert_eq!(node.config_name.as_deref(), Some("taylor"));

        assert!(reg.get_node_by_ip(ip(200)).is_none());
    }

    #[test]
    fn test_upsert_config_node_updates_existing_by_ip() {
        let reg = NodeRegistry::new();
        let id1 = reg.upsert_config_node("taylor", ip(100), 51800, 1);
        let id2 = reg.upsert_config_node("taylor", ip(100), 51801, 1);

        // Same node — should return the same ID.
        assert_eq!(id1, id2);
        assert_eq!(reg.len(), 1);

        let node = reg.get_node(id1).unwrap();
        assert_eq!(node.api_port, Some(51801));
    }

    #[test]
    fn test_upsert_config_node_updates_existing_by_name() {
        let reg = NodeRegistry::new();
        let id1 = reg.upsert_config_node("taylor", ip(100), 51800, 1);

        // Same name, different IP → should update the IP.
        let id2 = reg.upsert_config_node("taylor", ip(200), 51800, 1);

        assert_eq!(id1, id2);
        assert_eq!(reg.len(), 1);

        let node = reg.get_node(id1).unwrap();
        assert_eq!(node.ip, ip(200));

        // Old IP should no longer resolve.
        assert!(reg.get_node_by_ip(ip(100)).is_none());
        // New IP should resolve.
        assert!(reg.get_node_by_ip(ip(200)).is_some());
    }

    #[test]
    fn test_remove_node() {
        let reg = NodeRegistry::new();
        let id = reg.upsert_config_node("taylor", ip(100), 51800, 1);

        let removed = reg.remove_node(id);
        assert!(removed.is_some());
        assert_eq!(reg.len(), 0);
        assert!(reg.get_node_by_name("taylor").is_none());
        assert!(reg.get_node_by_ip(ip(100)).is_none());
    }

    #[test]
    fn test_upsert_discovered_node() {
        let reg = NodeRegistry::new();
        let discovered = DiscoveredNode {
            ip: ip(150),
            open_ports: vec![51800, 51801],
            discovered_at: Utc::now(),
        };

        let id = reg.upsert_discovered_node(discovered);
        assert_eq!(reg.len(), 1);

        let node = reg.get_node(id).unwrap();
        assert!(node.config_name.is_none()); // Discovery has no name.
        assert_eq!(node.open_ports.len(), 2);
    }

    #[test]
    fn test_apply_scan_result_updates_health() {
        let reg = NodeRegistry::new();
        reg.upsert_config_node("taylor", ip(100), 51800, 1);

        let result = NodeScanResult {
            name: "taylor".into(),
            host: "192.168.5.100".into(),
            port: 51800,
            status: NodeScanStatus::Online,
            latency_ms: 42,
            scanned_at: Utc::now(),
            http_status: Some(200),
            error: None,
        };

        reg.apply_scan_result(&result);

        let node = reg.get_node_by_name("taylor").unwrap();
        assert!(node.is_healthy());
        assert!(node.is_online());
        assert_eq!(node.health.as_ref().unwrap().status, HealthStatus::Healthy);
    }

    #[test]
    fn test_apply_scan_result_offline() {
        let reg = NodeRegistry::new();
        reg.upsert_config_node("james", ip(101), 51800, 2);

        let result = NodeScanResult {
            name: "james".into(),
            host: "192.168.5.101".into(),
            port: 51800,
            status: NodeScanStatus::Offline,
            latency_ms: 3000,
            scanned_at: Utc::now(),
            http_status: None,
            error: Some("connection refused".into()),
        };

        reg.apply_scan_result(&result);

        let node = reg.get_node_by_name("james").unwrap();
        assert!(!node.is_healthy());
        assert!(!node.is_online());
    }

    #[test]
    fn test_apply_scan_result_degraded() {
        let reg = NodeRegistry::new();
        reg.upsert_config_node("marcus", ip(102), 51800, 50);

        let result = NodeScanResult {
            name: "marcus".into(),
            host: "192.168.5.102".into(),
            port: 51800,
            status: NodeScanStatus::Degraded,
            latency_ms: 2000,
            scanned_at: Utc::now(),
            http_status: Some(500),
            error: None,
        };

        reg.apply_scan_result(&result);

        let node = reg.get_node_by_name("marcus").unwrap();
        // Degraded is still "healthy" for election purposes.
        assert!(node.is_healthy());
        // But not "online" (strict check).
        assert!(!node.is_online());
    }

    #[test]
    fn test_mark_stale_nodes() {
        let reg = NodeRegistry::new();
        let id = reg.upsert_config_node("taylor", ip(100), 51800, 1);

        // Mark as healthy first.
        let result = NodeScanResult {
            name: "taylor".into(),
            host: "192.168.5.100".into(),
            port: 51800,
            status: NodeScanStatus::Online,
            latency_ms: 10,
            scanned_at: Utc::now(),
            http_status: Some(200),
            error: None,
        };
        reg.apply_scan_result(&result);
        assert!(reg.get_node(id).unwrap().is_healthy());

        // Manually backdate last_seen to simulate staleness.
        if let Some(mut node) = reg.nodes.get_mut(&id) {
            node.last_seen = Utc::now() - chrono::Duration::seconds(100);
        }

        let stale = reg.mark_stale_nodes(90);
        assert_eq!(stale, vec!["taylor".to_string()]);

        // Node should now be unreachable.
        let node = reg.get_node(id).unwrap();
        assert!(!node.is_healthy());
    }

    #[test]
    fn test_mark_stale_nodes_no_false_positives() {
        let reg = NodeRegistry::new();
        reg.upsert_config_node("taylor", ip(100), 51800, 1);

        // Node was just registered (last_seen = now), should not be stale.
        let stale = reg.mark_stale_nodes(90);
        assert!(stale.is_empty());
    }

    #[test]
    fn test_node_health_for_election() {
        let reg = NodeRegistry::new();
        reg.upsert_config_node("taylor", ip(100), 51800, 1);
        reg.upsert_config_node("james", ip(101), 51800, 2);

        // Mark taylor online, james offline.
        reg.apply_scan_result(&NodeScanResult {
            name: "taylor".into(),
            host: "192.168.5.100".into(),
            port: 51800,
            status: NodeScanStatus::Online,
            latency_ms: 10,
            scanned_at: Utc::now(),
            http_status: Some(200),
            error: None,
        });
        reg.apply_scan_result(&NodeScanResult {
            name: "james".into(),
            host: "192.168.5.101".into(),
            port: 51800,
            status: NodeScanStatus::Offline,
            latency_ms: 3000,
            scanned_at: Utc::now(),
            http_status: None,
            error: Some("timeout".into()),
        });

        let health = reg.node_health_for_election();
        assert_eq!(health.len(), 2);

        let taylor_health = health.iter().find(|(n, _, _)| n == "taylor").unwrap();
        assert!(taylor_health.1); // is_healthy

        let james_health = health.iter().find(|(n, _, _)| n == "james").unwrap();
        assert!(!james_health.1); // not healthy
    }

    #[test]
    fn test_online_node_names() {
        let reg = NodeRegistry::new();
        reg.upsert_config_node("taylor", ip(100), 51800, 1);
        reg.upsert_config_node("james", ip(101), 51800, 2);

        // Both start without health data → neither is online.
        assert!(reg.online_node_names().is_empty());

        // Mark taylor online.
        reg.apply_scan_result(&NodeScanResult {
            name: "taylor".into(),
            host: "192.168.5.100".into(),
            port: 51800,
            status: NodeScanStatus::Online,
            latency_ms: 10,
            scanned_at: Utc::now(),
            http_status: Some(200),
            error: None,
        });

        let online = reg.online_node_names();
        assert_eq!(online, vec!["taylor".to_string()]);
    }

    #[test]
    fn test_leader_tracking() {
        let reg = NodeRegistry::new();
        assert!(reg.current_leader().is_none());

        reg.set_leader("taylor".into());
        assert_eq!(reg.current_leader(), Some("taylor".to_string()));

        reg.set_leader("james".into());
        assert_eq!(reg.current_leader(), Some("james".to_string()));
    }

    #[test]
    fn test_touch_updates_last_seen() {
        let reg = NodeRegistry::new();
        let id = reg.upsert_config_node("taylor", ip(100), 51800, 1);

        let before = reg.get_node(id).unwrap().last_seen;
        std::thread::sleep(std::time::Duration::from_millis(10));
        reg.touch(id);
        let after = reg.get_node(id).unwrap().last_seen;

        assert!(after > before);
    }

    #[test]
    fn test_touch_by_name() {
        let reg = NodeRegistry::new();
        reg.upsert_config_node("taylor", ip(100), 51800, 1);

        let before = reg.get_node_by_name("taylor").unwrap().last_seen;
        std::thread::sleep(std::time::Duration::from_millis(10));
        reg.touch_by_name("taylor");
        let after = reg.get_node_by_name("taylor").unwrap().last_seen;

        assert!(after > before);
    }

    #[test]
    fn test_discovered_node_merges_with_config_node() {
        let reg = NodeRegistry::new();

        // First: register from config.
        let config_id = reg.upsert_config_node("taylor", ip(100), 51800, 1);

        // Then: discover the same IP via subnet scan.
        let discovered = DiscoveredNode {
            ip: ip(100),
            open_ports: vec![51800, 51801, 51802],
            discovered_at: Utc::now(),
        };
        let disc_id = reg.upsert_discovered_node(discovered);

        // Should be the same node.
        assert_eq!(config_id, disc_id);
        assert_eq!(reg.len(), 1);

        // Config name should be preserved.
        let node = reg.get_node(config_id).unwrap();
        assert_eq!(node.config_name.as_deref(), Some("taylor"));
        // Ports should be updated from discovery.
        assert_eq!(node.open_ports, vec![51800, 51801, 51802]);
    }
}
