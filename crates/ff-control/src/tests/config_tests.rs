//! Tests for the 480B RPC ring deployment configuration.

use crate::config::{Endpoint480bConfig, EscalationConfig};

#[test]
fn legacy_endpoint_config_uses_canonical_rpc_ring_defaults() {
    let config: Endpoint480bConfig = serde_json::from_str(
        r#"{
            "url": "http://127.0.0.1:51001",
            "model": "qwen3-coder-480b",
            "timeout_secs": 600
        }"#,
    )
    .expect("legacy endpoint configuration should remain valid");

    assert_eq!(config.rpc_shard_id, 0);
    assert_eq!(config.rpc_shard_count, 3);
    assert_eq!(config.rpc_ring_topology, "adele,rihanna,beyonce");
}

#[test]
fn endpoint_config_accepts_rpc_ring_overrides() {
    let config: Endpoint480bConfig = serde_json::from_str(
        r#"{
            "url": "http://127.0.0.1:51001",
            "model": "qwen3-coder-480b",
            "timeout_secs": 900,
            "rpc_shard_id": 2,
            "rpc_shard_count": 4,
            "rpc_ring_topology": "adele,rihanna,beyonce,vinny"
        }"#,
    )
    .expect("RPC ring overrides should deserialize");

    assert_eq!(config.rpc_shard_id, 2);
    assert_eq!(config.rpc_shard_count, 4);
    assert_eq!(config.rpc_ring_topology, "adele,rihanna,beyonce,vinny");
}

#[test]
fn escalation_config_round_trips_rpc_ring_configuration() {
    let config: EscalationConfig = serde_json::from_str(
        r#"{
            "enabled": true,
            "failure_threshold": 3,
            "complexity_threshold": 0.9,
            "endpoint": {
                "url": "http://127.0.0.1:51001",
                "model": "qwen3-coder-480b",
                "timeout_secs": 900,
                "rpc_shard_id": 1,
                "rpc_shard_count": 3,
                "rpc_ring_topology": "adele,rihanna,beyonce"
            }
        }"#,
    )
    .expect("escalation configuration should deserialize");

    let serialized = serde_json::to_string(&config).expect("configuration should serialize");
    let round_tripped: EscalationConfig =
        serde_json::from_str(&serialized).expect("serialized configuration should deserialize");

    assert!(round_tripped.enabled);
    assert_eq!(round_tripped.endpoint.rpc_shard_id, 1);
    assert_eq!(round_tripped.endpoint.rpc_shard_count, 3);
    assert_eq!(
        round_tripped.endpoint.rpc_ring_topology,
        "adele,rihanna,beyonce"
    );
}
