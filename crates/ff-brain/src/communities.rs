//! Leiden community detection on the vault graph.
//!
//! For now, uses a simple union-find connected-components algorithm as a
//! placeholder. Real Leiden clustering will replace this when we have
//! enough nodes to warrant it.

use sqlx::PgPool;
use std::collections::HashMap;

/// Summary of community detection results.
pub struct CommunitySummary {
    pub communities_found: usize,
    pub largest_community: usize,
}

/// Run community detection on the vault graph.
///
/// Current implementation: union-find connected components.
/// Assigns each component a community_id and writes it back to brain_vault_nodes.
pub async fn detect_communities(pool: &PgPool) -> Result<CommunitySummary, String> {
    // Fetch all active node paths
    let node_rows: Vec<(String,)> =
        sqlx::query_as("SELECT path FROM brain_vault_nodes WHERE valid_until IS NULL")
            .fetch_all(pool)
            .await
            .map_err(|e| format!("DB error fetching nodes: {e}"))?;

    if node_rows.is_empty() {
        return Ok(CommunitySummary {
            communities_found: 0,
            largest_community: 0,
        });
    }

    // Build path -> index mapping
    let mut path_to_idx: HashMap<String, usize> = HashMap::new();
    for (i, (path,)) in node_rows.iter().enumerate() {
        path_to_idx.insert(path.clone(), i);
    }
    let n = node_rows.len();

    // Union-Find
    let mut parent: Vec<usize> = (0..n).collect();
    let mut rank: Vec<usize> = vec![0; n];

    let find = |parent: &mut Vec<usize>, mut x: usize| -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]]; // path compression
            x = parent[x];
        }
        x
    };

    let union = |parent: &mut Vec<usize>, rank: &mut Vec<usize>, a: usize, b: usize| {
        let ra = {
            let mut x = a;
            while parent[x] != x {
                parent[x] = parent[parent[x]];
                x = parent[x];
            }
            x
        };
        let rb = {
            let mut x = b;
            while parent[x] != x {
                parent[x] = parent[parent[x]];
                x = parent[x];
            }
            x
        };
        if ra == rb {
            return;
        }
        if rank[ra] < rank[rb] {
            parent[ra] = rb;
        } else if rank[ra] > rank[rb] {
            parent[rb] = ra;
        } else {
            parent[rb] = ra;
            rank[ra] += 1;
        }
    };

    // Fetch edges and union
    let edge_rows: Vec<(String, String)> =
        sqlx::query_as("SELECT source_path, target_path FROM brain_vault_edges")
            .fetch_all(pool)
            .await
            .map_err(|e| format!("DB error fetching edges: {e}"))?;

    for (src, tgt) in &edge_rows {
        if let (Some(&si), Some(&ti)) = (path_to_idx.get(src), path_to_idx.get(tgt)) {
            union(&mut parent, &mut rank, si, ti);
        }
    }

    // Assign community IDs
    let mut root_to_community: HashMap<usize, i32> = HashMap::new();
    let mut next_community: i32 = 0;
    let mut community_sizes: HashMap<i32, usize> = HashMap::new();

    for i in 0..n {
        let root = find(&mut parent, i);
        let cid = *root_to_community.entry(root).or_insert_with(|| {
            let c = next_community;
            next_community += 1;
            c
        });
        *community_sizes.entry(cid).or_insert(0) += 1;
    }

    // Write community_id back to nodes
    for (i, (path,)) in node_rows.iter().enumerate() {
        let root = find(&mut parent, i);
        let cid = root_to_community[&root];
        sqlx::query(
            "UPDATE brain_vault_nodes SET community_id = $1 WHERE path = $2 AND valid_until IS NULL",
        )
        .bind(cid)
        .bind(path)
        .execute(pool)
        .await
        .map_err(|e| format!("DB error updating community_id: {e}"))?;
    }

    let communities_found = root_to_community.len();
    let largest_community = community_sizes.values().max().copied().unwrap_or(0);

    Ok(CommunitySummary {
        communities_found,
        largest_community,
    })
}
