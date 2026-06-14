//! Leiden community detection on the vault graph.
//!
//! For now, uses a simple union-find connected-components algorithm as a
//! placeholder. Real Leiden clustering will replace this when we have
//! enough nodes to warrant it.

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::collections::HashMap;

/// Summary of community detection results.
pub struct CommunitySummary {
    pub communities_found: usize,
    pub largest_community: usize,
    /// Rows reconciled into the `brain_communities` registry this run.
    pub communities_persisted: usize,
}

/// Stable identity for a community = SHA-256 of its member node paths, sorted so
/// the order union-find happens to visit them in never matters. Two detection
/// runs over the same connected component produce the same hash, so the
/// `brain_communities` row (and any LLM summary attached to it) survives a
/// re-detection — only a community whose membership actually changed gets a new
/// hash. Pure + unit-tested.
pub fn community_member_hash(member_paths: &[String]) -> String {
    let mut sorted: Vec<&str> = member_paths.iter().map(|s| s.as_str()).collect();
    sorted.sort_unstable();
    let mut h = Sha256::new();
    for p in sorted {
        h.update(p.as_bytes());
        h.update(b"\n");
    }
    format!("{:x}", h.finalize())
}

/// Run community detection on the vault graph.
///
/// Current implementation: union-find connected components.
/// Assigns each component a community_id and writes it back to brain_vault_nodes.
pub async fn detect_communities(pool: &PgPool) -> Result<CommunitySummary, String> {
    // Fetch all active node (id, path) pairs.
    let node_rows: Vec<(uuid::Uuid, String)> =
        sqlx::query_as("SELECT id, path FROM brain_vault_nodes WHERE valid_until IS NULL")
            .fetch_all(pool)
            .await
            .map_err(|e| format!("DB error fetching nodes: {e}"))?;

    if node_rows.is_empty() {
        return Ok(CommunitySummary {
            communities_found: 0,
            largest_community: 0,
            communities_persisted: 0,
        });
    }

    // Build path→index and id→index mappings.
    let mut path_to_idx: HashMap<String, usize> = HashMap::new();
    let mut id_to_idx: HashMap<uuid::Uuid, usize> = HashMap::new();
    for (i, (id, path)) in node_rows.iter().enumerate() {
        path_to_idx.insert(path.clone(), i);
        id_to_idx.insert(*id, i);
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

    // Fetch edges and union — schema uses src_id/dst_id (UUIDs).
    let edge_rows: Vec<(uuid::Uuid, uuid::Uuid)> =
        sqlx::query_as("SELECT src_id, dst_id FROM brain_vault_edges")
            .fetch_all(pool)
            .await
            .map_err(|e| format!("DB error fetching edges: {e}"))?;

    // Node degree (appearances as either endpoint) — used to pick each
    // community's representative "god node".
    let mut degree: Vec<usize> = vec![0; n];
    for (src_id, dst_id) in &edge_rows {
        if let (Some(&si), Some(&ti)) = (id_to_idx.get(src_id), id_to_idx.get(dst_id)) {
            union(&mut parent, &mut rank, si, ti);
            degree[si] += 1;
            degree[ti] += 1;
        }
    }

    // Assign community IDs and group member indices per community.
    let mut root_to_community: HashMap<usize, i32> = HashMap::new();
    let mut next_community: i32 = 0;
    let mut members: HashMap<i32, Vec<usize>> = HashMap::new();

    for i in 0..n {
        let root = find(&mut parent, i);
        let cid = *root_to_community.entry(root).or_insert_with(|| {
            let c = next_community;
            next_community += 1;
            c
        });
        members.entry(cid).or_default().push(i);
    }

    // Batch-write community_id back to nodes in ONE statement (was one UPDATE per
    // node — tens of thousands of round-trips on a real corpus).
    let (paths, cids): (Vec<String>, Vec<i32>) = (0..n)
        .map(|i| {
            let root = find(&mut parent, i);
            (node_rows[i].1.clone(), root_to_community[&root])
        })
        .unzip();
    sqlx::query(
        "UPDATE brain_vault_nodes AS bn SET community_id = t.cid
         FROM UNNEST($1::text[], $2::int[]) AS t(path, cid)
         WHERE bn.path = t.path AND bn.valid_until IS NULL",
    )
    .bind(&paths)
    .bind(&cids)
    .execute(pool)
    .await
    .map_err(|e| format!("DB error batch-updating community_id: {e}"))?;

    let communities_found = root_to_community.len();
    let largest_community = members.values().map(|m| m.len()).max().unwrap_or(0);

    // Reconcile the brain_communities registry. Each community is keyed by a
    // STABLE member-set hash so an unchanged community keeps its row (and its
    // summary); the god node is the highest-degree member (ties broken by path
    // for determinism).
    let mut hashes: Vec<String> = Vec::with_capacity(members.len());
    let mut god_ids: Vec<uuid::Uuid> = Vec::with_capacity(members.len());
    let mut counts: Vec<i32> = Vec::with_capacity(members.len());
    for idxs in members.values() {
        let member_paths: Vec<String> = idxs.iter().map(|&i| node_rows[i].1.clone()).collect();
        let hash = community_member_hash(&member_paths);
        // Representative node: max degree, then smallest path.
        let god = idxs
            .iter()
            .copied()
            .max_by(|&a, &b| {
                degree[a]
                    .cmp(&degree[b])
                    .then_with(|| node_rows[b].1.cmp(&node_rows[a].1))
            })
            .expect("every community has at least one member");
        hashes.push(hash);
        god_ids.push(node_rows[god].0);
        counts.push(idxs.len() as i32);
    }

    // GC communities (and any legacy NULL-hash rows) no longer present, then
    // upsert the current set — preserving `summary*` on unchanged rows.
    sqlx::query(
        "DELETE FROM brain_communities WHERE member_hash IS NULL OR member_hash <> ALL($1)",
    )
    .bind(&hashes)
    .execute(pool)
    .await
    .map_err(|e| format!("DB error pruning stale communities: {e}"))?;

    sqlx::query(
        "INSERT INTO brain_communities (member_hash, god_node_id, member_count, updated_at)
         SELECT h, g, c, NOW()
         FROM UNNEST($1::text[], $2::uuid[], $3::int[]) AS t(h, g, c)
         ON CONFLICT (member_hash) DO UPDATE
           SET member_count = EXCLUDED.member_count,
               god_node_id  = EXCLUDED.god_node_id,
               updated_at   = NOW()",
    )
    .bind(&hashes)
    .bind(&god_ids)
    .bind(&counts)
    .execute(pool)
    .await
    .map_err(|e| format!("DB error upserting communities: {e}"))?;

    Ok(CommunitySummary {
        communities_found,
        largest_community,
        communities_persisted: hashes.len(),
    })
}

// ─── Cortex code-community detection (`calls`-subgraph label propagation) ────
//
// `detect_communities` above is union-find connected components over ALL vault
// edges — correct for the brain KG, but `contains`/`imports` bridge the whole
// code graph into one ~45k-node mega-community whose summary is garbage (cortex
// roadmap #4 blocker). This is the cortex-specific replacement: cluster ONLY the
// `calls` subgraph among non-extern `code:*` nodes, using **label propagation**
// — an algorithm that *subdivides* a connected graph (connected-components
// fundamentally cannot). Output lands in the parallel `code_community_id` column
// + `brain_code_communities` registry (V127), leaving the brain KG's
// `community_id`/`brain_communities` untouched.

/// Max local-moving passes. Single-level Louvain on a sparse call graph
/// converges in a handful of passes; the cap guarantees termination.
const CLUSTER_MAX_PASSES: usize = 30;

/// Pure single-level Louvain (modularity local-moving) over an undirected
/// adjacency list. Returns one community label per node.
///
/// `order` is the deterministic node-visit order; `adj[i]` lists `i`'s neighbour
/// indices, with multiplicity (a node called N times is N neighbour entries, so
/// the move weights by call frequency). Each node is greedily moved into the
/// neighbouring community that yields the largest modularity gain, repeated until
/// a pass makes no move. Unlike label propagation (which collapses weakly-linked
/// cliques across a single bridge edge under any deterministic tie-break), the
/// modularity objective keeps two cliques joined by one edge as two communities —
/// exactly the structure a call graph has, and exactly what connected-components
/// (the brain-KG clusterer) gets wrong.
///
/// Determinism: gains are compared with an epsilon and ties broken by the
/// SMALLEST community id, so the partition is reproducible run-to-run regardless
/// of `HashMap` iteration order — important so `member_hash` is stable and
/// summaries survive re-detection. Isolated nodes (`adj[i]` empty) stay in their
/// own singleton community.
pub fn cluster_calls_graph(n: usize, adj: &[Vec<usize>], order: &[usize]) -> Vec<usize> {
    const EPS: f64 = 1e-12;
    let deg: Vec<f64> = adj.iter().map(|a| a.len() as f64).collect();
    let two_m: f64 = deg.iter().sum();
    if two_m == 0.0 {
        return (0..n).collect(); // no edges → all singletons
    }

    let mut comm: Vec<usize> = (0..n).collect();
    // sum_tot[c] = Σ degree of nodes currently in community c.
    let mut sum_tot: Vec<f64> = deg.clone();

    for _ in 0..CLUSTER_MAX_PASSES {
        let mut improved = false;
        for &i in order {
            if adj[i].is_empty() {
                continue;
            }
            let ci = comm[i];
            // Detach i from its community before scoring (so "stay" is scored on
            // the community minus i).
            sum_tot[ci] -= deg[i];

            // Edge weight from i to each neighbouring community.
            let mut w_to: HashMap<usize, f64> = HashMap::new();
            for &nb in &adj[i] {
                if nb == i {
                    continue;
                }
                *w_to.entry(comm[nb]).or_insert(0.0) += 1.0;
            }

            // Modularity gain of placing i in community c ∝ w_to[c] - sum_tot[c]*deg[i]/2m.
            // Baseline = staying in (the now-i-less) ci.
            let mut best_c = ci;
            let mut best_gain =
                w_to.get(&ci).copied().unwrap_or(0.0) - sum_tot[ci] * deg[i] / two_m;
            for (&c, &w) in &w_to {
                let gain = w - sum_tot[c] * deg[i] / two_m;
                if gain > best_gain + EPS || ((gain - best_gain).abs() <= EPS && c < best_c) {
                    best_gain = gain;
                    best_c = c;
                }
            }

            // Re-attach i to the chosen community.
            sum_tot[best_c] += deg[i];
            if best_c != ci {
                comm[i] = best_c;
                improved = true;
            }
        }
        if !improved {
            break;
        }
    }
    comm
}

/// Run cortex code-community detection: label propagation over the `calls`
/// subgraph among non-extern `code:*` nodes. Writes each node's
/// `code_community_id` and reconciles the `brain_code_communities` registry
/// (keyed by the stable [`community_member_hash`], god node = highest call
/// fan-in). Only multi-member communities (≥2) get a registry row — singletons
/// (uncalled entry points, leaf helpers) would otherwise flood it with noise and
/// aren't worth an LLM summary. Leaves `community_id`/`brain_communities`
/// (the brain KG view) completely untouched.
pub async fn detect_code_communities(pool: &PgPool) -> Result<CommunitySummary, String> {
    // Non-extern code nodes only — the universe we cluster.
    let node_rows: Vec<(uuid::Uuid, String)> = sqlx::query_as(
        "SELECT id, path FROM brain_vault_nodes
         WHERE valid_until IS NULL
           AND node_type LIKE 'code:%'
           AND node_type <> 'code:extern'",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("DB error fetching code nodes: {e}"))?;

    if node_rows.is_empty() {
        // Clear any stale registry so callers see an honest empty state.
        sqlx::query("DELETE FROM brain_code_communities")
            .execute(pool)
            .await
            .map_err(|e| format!("DB error clearing code communities: {e}"))?;
        return Ok(CommunitySummary {
            communities_found: 0,
            largest_community: 0,
            communities_persisted: 0,
        });
    }

    let n = node_rows.len();
    let mut id_to_idx: HashMap<uuid::Uuid, usize> = HashMap::with_capacity(n);
    for (i, (id, _)) in node_rows.iter().enumerate() {
        id_to_idx.insert(*id, i);
    }

    // `calls` edges among code nodes → undirected adjacency + directed in-degree
    // (callers) for the god-node pick.
    let edge_rows: Vec<(uuid::Uuid, uuid::Uuid)> =
        sqlx::query_as("SELECT src_id, dst_id FROM brain_vault_edges WHERE edge_type = 'calls'")
            .fetch_all(pool)
            .await
            .map_err(|e| format!("DB error fetching call edges: {e}"))?;

    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut in_degree: Vec<usize> = vec![0; n];
    for (src_id, dst_id) in &edge_rows {
        if let (Some(&si), Some(&di)) = (id_to_idx.get(src_id), id_to_idx.get(dst_id)) {
            if si == di {
                continue; // self-recursion adds no community signal
            }
            adj[si].push(di);
            adj[di].push(si);
            in_degree[di] += 1;
        }
    }

    // Deterministic visit order (by path) so the clustering is reproducible.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| node_rows[a].1.cmp(&node_rows[b].1));

    let label = cluster_calls_graph(n, &adj, &order);

    // Relabel raw labels → dense community ids; group members.
    let mut label_to_cid: HashMap<usize, i32> = HashMap::new();
    let mut next_cid: i32 = 0;
    let mut members: HashMap<i32, Vec<usize>> = HashMap::new();
    let mut cid_of: Vec<i32> = vec![0; n];
    for i in 0..n {
        let cid = *label_to_cid.entry(label[i]).or_insert_with(|| {
            let c = next_cid;
            next_cid += 1;
            c
        });
        cid_of[i] = cid;
        members.entry(cid).or_default().push(i);
    }

    // Batch-write code_community_id by node id (code paths are unique, but id is
    // the safe key). Clear it first on any code node not in this run is implicit:
    // every current code node is in node_rows and gets a value here.
    let ids: Vec<uuid::Uuid> = node_rows.iter().map(|(id, _)| *id).collect();
    sqlx::query(
        "UPDATE brain_vault_nodes AS bn SET code_community_id = t.cid
         FROM UNNEST($1::uuid[], $2::int[]) AS t(id, cid)
         WHERE bn.id = t.id",
    )
    .bind(&ids)
    .bind(&cid_of)
    .execute(pool)
    .await
    .map_err(|e| format!("DB error batch-updating code_community_id: {e}"))?;

    let communities_found = members.len();
    let largest_community = members.values().map(|m| m.len()).max().unwrap_or(0);

    // Reconcile the registry — only multi-member communities (singletons are
    // noise + not worth a summary). Stable member_hash so summaries survive.
    let mut hashes: Vec<String> = Vec::new();
    let mut god_ids: Vec<uuid::Uuid> = Vec::new();
    let mut counts: Vec<i32> = Vec::new();
    for idxs in members.values() {
        if idxs.len() < 2 {
            continue;
        }
        let member_paths: Vec<String> = idxs.iter().map(|&i| node_rows[i].1.clone()).collect();
        let hash = community_member_hash(&member_paths);
        // God node: highest call fan-in (most-called member), ties → smallest path.
        let god = idxs
            .iter()
            .copied()
            .max_by(|&a, &b| {
                in_degree[a]
                    .cmp(&in_degree[b])
                    .then_with(|| node_rows[b].1.cmp(&node_rows[a].1))
            })
            .expect("multi-member community has at least one member");
        hashes.push(hash);
        god_ids.push(node_rows[god].0);
        counts.push(idxs.len() as i32);
    }

    // GC stale rows (and legacy NULL-hash), then upsert — preserving summaries on
    // unchanged communities.
    sqlx::query(
        "DELETE FROM brain_code_communities WHERE member_hash IS NULL OR member_hash <> ALL($1)",
    )
    .bind(&hashes)
    .execute(pool)
    .await
    .map_err(|e| format!("DB error pruning stale code communities: {e}"))?;

    sqlx::query(
        "INSERT INTO brain_code_communities (member_hash, god_node_id, member_count, updated_at)
         SELECT h, g, c, NOW()
         FROM UNNEST($1::text[], $2::uuid[], $3::int[]) AS t(h, g, c)
         ON CONFLICT (member_hash) DO UPDATE
           SET member_count = EXCLUDED.member_count,
               god_node_id  = EXCLUDED.god_node_id,
               updated_at   = NOW()",
    )
    .bind(&hashes)
    .bind(&god_ids)
    .bind(&counts)
    .execute(pool)
    .await
    .map_err(|e| format!("DB error upserting code communities: {e}"))?;

    Ok(CommunitySummary {
        communities_found,
        largest_community,
        communities_persisted: hashes.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::{cluster_calls_graph, community_member_hash};

    // Two triangles {0,1,2} and {3,4,5} joined by a single 2-3 bridge edge.
    fn two_cliques() -> Vec<Vec<usize>> {
        vec![
            vec![1, 2],    // 0
            vec![0, 2],    // 1
            vec![0, 1, 3], // 2 — bridge
            vec![2, 4, 5], // 3 — bridge
            vec![3, 5],    // 4
            vec![3, 4],    // 5
        ]
    }

    #[test]
    fn cluster_splits_two_cliques_joined_by_a_bridge() {
        // The case connected-components (the brain KG clusterer) wrongly merges:
        // modularity must keep these as TWO communities.
        let adj = two_cliques();
        let order: Vec<usize> = (0..6).collect();
        let label = cluster_calls_graph(6, &adj, &order);
        assert_eq!(label[0], label[1]);
        assert_eq!(label[1], label[2]);
        assert_eq!(label[3], label[4]);
        assert_eq!(label[4], label[5]);
        assert_ne!(
            label[0], label[3],
            "the two cliques must NOT collapse into one community"
        );
    }

    #[test]
    fn cluster_isolated_nodes_stay_singletons() {
        // Nodes 0,1 connected; 2 isolated.
        let adj = vec![vec![1], vec![0], vec![]];
        let label = cluster_calls_graph(3, &adj, &[0, 1, 2]);
        assert_eq!(label[0], label[1]);
        assert_ne!(label[2], label[0], "isolated node keeps its own community");
        assert_eq!(label[2], 2, "isolated node keeps its INITIAL community");
    }

    #[test]
    fn cluster_is_deterministic_across_visit_orders() {
        // Same graph, different visit orders → same community partition (the
        // smallest-id tie-break makes this reproducible, so member_hash is stable).
        let adj = two_cliques();
        let a = cluster_calls_graph(6, &adj, &[0, 1, 2, 3, 4, 5]);
        let b = cluster_calls_graph(6, &adj, &[5, 4, 3, 2, 1, 0]);
        let same_partition = |x: &[usize], y: &[usize]| {
            (0..x.len()).all(|i| (0..x.len()).all(|j| (x[i] == x[j]) == (y[i] == y[j])))
        };
        assert!(
            same_partition(&a, &b),
            "partition must not depend on visit order"
        );
    }

    #[test]
    fn cluster_no_edges_is_all_singletons() {
        let adj = vec![vec![], vec![], vec![]];
        let label = cluster_calls_graph(3, &adj, &[0, 1, 2]);
        assert_eq!(label, vec![0, 1, 2]);
    }

    #[test]
    fn member_hash_is_order_independent() {
        let a = community_member_hash(&["code://x".into(), "code://y".into(), "code://z".into()]);
        let b = community_member_hash(&["code://z".into(), "code://x".into(), "code://y".into()]);
        assert_eq!(a, b, "hash must not depend on member visit order");
    }

    #[test]
    fn member_hash_changes_with_membership() {
        let base = community_member_hash(&["code://x".into(), "code://y".into()]);
        let grown =
            community_member_hash(&["code://x".into(), "code://y".into(), "code://added".into()]);
        let shrunk = community_member_hash(&["code://x".into()]);
        assert_ne!(base, grown, "adding a member must change the hash");
        assert_ne!(base, shrunk, "removing a member must change the hash");
    }

    #[test]
    fn member_hash_is_sha256_hex() {
        let h = community_member_hash(&["code://x".into()]);
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
