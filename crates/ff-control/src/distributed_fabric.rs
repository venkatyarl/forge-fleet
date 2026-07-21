//! Deterministic launcher topology selection from verified fabric links.

use std::collections::{BTreeMap, BTreeSet};

use ff_db::models::FabricPair;
use serde::{Deserialize, Serialize};

/// Hub-and-spoke topology consumed by the distributed launcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DistributedTopologyPlan {
    pub hub: String,
    pub workers: Vec<String>,
}

/// Select the best connected node as the launcher hub and its verified peers as workers.
///
/// Fabric links are undirected. Duplicate rows and self-links do not increase a node's
/// degree. Re-running the selection after another pair is verified automatically adds
/// that peer to the plan (including the third edge in a growing fabric).
pub fn select_hub_and_workers(pairs: &[FabricPair]) -> Option<DistributedTopologyPlan> {
    let mut edges = BTreeSet::new();
    for pair in pairs.iter().filter(|pair| pair.verified) {
        if pair.source_node == pair.target_node {
            continue;
        }

        let edge = if pair.source_node < pair.target_node {
            (pair.source_node.clone(), pair.target_node.clone())
        } else {
            (pair.target_node.clone(), pair.source_node.clone())
        };
        edges.insert(edge);
    }

    let mut neighbors: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (source, target) in edges {
        neighbors
            .entry(source.clone())
            .or_default()
            .insert(target.clone());
        neighbors.entry(target).or_default().insert(source);
    }

    let hub = neighbors
        .iter()
        .max_by(|(name_a, edges_a), (name_b, edges_b)| {
            edges_a
                .len()
                .cmp(&edges_b.len())
                .then_with(|| name_b.cmp(name_a))
        })?
        .0
        .clone();
    let workers = neighbors.remove(&hub)?.into_iter().collect();

    Some(DistributedTopologyPlan { hub, workers })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn pair(source: &str, target: &str, verified: bool) -> FabricPair {
        FabricPair {
            id: Uuid::new_v4(),
            source_node: source.into(),
            target_node: target.into(),
            cidr: "192.0.2.0/30".into(),
            status: if verified { "verified" } else { "pending" }.into(),
            verified,
        }
    }

    #[test]
    fn chooses_most_connected_hub_and_only_verified_workers() {
        let plan = select_hub_and_workers(&[
            pair("rihanna", "lily", true),
            pair("logan", "rihanna", true),
            pair("rihanna", "adele", false),
            pair("lily", "adele", true),
        ])
        .unwrap();

        assert_eq!(plan.hub, "lily");
        assert_eq!(plan.workers, ["adele", "rihanna"]);
    }

    #[test]
    fn adds_third_worker_once_edge_is_verified() {
        let mut pairs = vec![
            pair("rihanna", "adele", true),
            pair("rihanna", "lily", true),
            pair("rihanna", "logan", false),
        ];

        let initial = select_hub_and_workers(&pairs).unwrap();
        assert_eq!(initial.workers, ["adele", "lily"]);

        pairs[2].verified = true;
        let upgraded = select_hub_and_workers(&pairs).unwrap();
        assert_eq!(upgraded.workers, ["adele", "lily", "logan"]);
    }

    #[test]
    fn deduplicates_edges_and_breaks_hub_ties_by_name() {
        let plan = select_hub_and_workers(&[
            pair("beta", "alpha", true),
            pair("alpha", "beta", true),
            pair("alpha", "alpha", true),
        ])
        .unwrap();

        assert_eq!(plan.hub, "alpha");
        assert_eq!(plan.workers, ["beta"]);
    }

    #[test]
    fn returns_none_without_verified_edges() {
        assert_eq!(select_hub_and_workers(&[pair("a", "b", false)]), None);
    }
}
