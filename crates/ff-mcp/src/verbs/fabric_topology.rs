//! Text view of the configured private-fabric ring.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::handlers::HandlerResult;

#[derive(Debug, FromRow)]
struct FabricEdge {
    pair_name: String,
    fabric_kind: String,
    computer_a_id: Uuid,
    computer_a_name: Option<String>,
    computer_b_id: Uuid,
    computer_b_name: Option<String>,
    a_iface: String,
    b_iface: String,
    a_ip: String,
    b_ip: String,
    verification_status: Option<String>,
    verified: Option<bool>,
}

pub async fn fabric_topology(_params: Option<Value>) -> HandlerResult {
    let pool = crate::pool::shared_pg_pool().await?;
    let edges = load_edges(&pool).await?;
    Ok(json!(format_topology(&edges)))
}

async fn load_edges(pool: &PgPool) -> Result<Vec<FabricEdge>, String> {
    let has_verification: bool = sqlx::query_scalar(
        "SELECT COUNT(*) = 2 FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = 'fabric_pairs' \
         AND column_name IN ('status', 'verified')",
    )
    .fetch_one(pool)
    .await
    .map_err(|error| format!("failed to inspect fabric_pairs schema: {error}"))?;

    let sql = if has_verification {
        "SELECT fp.pair_name, fp.fabric_kind, fp.computer_a_id, ca.name AS computer_a_name, \
                fp.computer_b_id, cb.name AS computer_b_name, fp.a_iface, fp.b_iface, \
                fp.a_ip, fp.b_ip, fp.status AS verification_status, fp.verified \
         FROM fabric_pairs fp \
         LEFT JOIN computers ca ON ca.id = fp.computer_a_id \
         LEFT JOIN computers cb ON cb.id = fp.computer_b_id \
         ORDER BY fp.pair_name, fp.id"
    } else {
        "SELECT fp.pair_name, fp.fabric_kind, fp.computer_a_id, ca.name AS computer_a_name, \
                fp.computer_b_id, cb.name AS computer_b_name, fp.a_iface, fp.b_iface, \
                fp.a_ip, fp.b_ip, NULL::text AS verification_status, NULL::boolean AS verified \
         FROM fabric_pairs fp \
         LEFT JOIN computers ca ON ca.id = fp.computer_a_id \
         LEFT JOIN computers cb ON cb.id = fp.computer_b_id \
         ORDER BY fp.pair_name, fp.id"
    };

    sqlx::query_as(sql)
        .fetch_all(pool)
        .await
        .map_err(|error| format!("failed to query fabric topology: {error}"))
}

fn format_topology(edges: &[FabricEdge]) -> String {
    let mut nodes = BTreeMap::new();
    for edge in edges {
        nodes.insert(edge.computer_a_id, edge.computer_a_name.as_deref());
        nodes.insert(edge.computer_b_id, edge.computer_b_name.as_deref());
    }

    let mut output = format!(
        "Fabric ring: {} node(s), {} edge(s)\nNodes:\n",
        nodes.len(),
        edges.len()
    );
    for (id, name) in nodes {
        output.push_str(&format!("- {} ({id})\n", name.unwrap_or("unknown")));
    }
    output.push_str("Edges:\n");
    for edge in edges {
        let verification = match (edge.verified, edge.verification_status.as_deref()) {
            (Some(true), _) => "verified",
            (Some(false), Some(status)) => status,
            (Some(false), None) => "unverified",
            (None, Some(status)) => status,
            (None, None) => "unknown",
        };
        output.push_str(&format!(
            "- {} [{}]: {} {} ({}) <-> {} {} ({}); verification={}\n",
            edge.pair_name,
            edge.fabric_kind,
            edge.computer_a_name.as_deref().unwrap_or("unknown"),
            edge.a_ip,
            edge.a_iface,
            edge.computer_b_name.as_deref().unwrap_or("unknown"),
            edge.b_ip,
            edge.b_iface,
            verification,
        ));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_nodes_edges_and_verification() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let output = format_topology(&[FabricEdge {
            pair_name: "alpha-beta".into(),
            fabric_kind: "roce".into(),
            computer_a_id: a,
            computer_a_name: Some("alpha".into()),
            computer_b_id: b,
            computer_b_name: Some("beta".into()),
            a_iface: "eth1".into(),
            b_iface: "eth2".into(),
            a_ip: "10.0.0.1".into(),
            b_ip: "10.0.0.2".into(),
            verification_status: Some("ready".into()),
            verified: Some(true),
        }]);

        assert!(output.contains("Fabric ring: 2 node(s), 1 edge(s)"));
        assert!(output.contains("alpha-beta [roce]"));
        assert!(output.contains("verification=verified"));
    }

    #[test]
    fn legacy_rows_have_unknown_verification() {
        let output = format_topology(&[FabricEdge {
            pair_name: "legacy".into(),
            fabric_kind: "infiniband".into(),
            computer_a_id: Uuid::from_u128(1),
            computer_a_name: None,
            computer_b_id: Uuid::from_u128(2),
            computer_b_name: None,
            a_iface: "ib0".into(),
            b_iface: "ib0".into(),
            a_ip: "10.1.0.1".into(),
            b_ip: "10.1.0.2".into(),
            verification_status: None,
            verified: None,
        }]);

        assert!(output.contains("verification=unknown"));
    }
}
