//! Leiden community detection on the vault graph.
//!
//! For now, uses a simple union-find connected-components algorithm as a
//! placeholder. Real Leiden clustering will replace this when we have
//! enough nodes to warrant it.

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};

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

/// Full multi-level (hierarchical) Louvain. Repeatedly run single-level
/// local-moving ([`cluster_calls_graph`]) then AGGREGATE the graph — each
/// community becomes a super-node and inter-community edges (plus intra-community
/// self-loops, which preserve degree so modularity stays correct) become the
/// next level's edges — until a pass merges nothing.
///
/// Returns one community labeling **over the original `n` nodes per level**:
/// `result[0]` is the finest partition (identical to [`cluster_calls_graph`]'s,
/// dense-relabelled), each subsequent level is strictly coarser, and the last
/// level is the coarsest stable partition. Always returns ≥1 level (even a
/// no-edge graph yields one all-singletons level). This is the GraphRAG
/// community hierarchy: fine levels = tight call clusters, coarse levels =
/// subsystems.
pub fn cluster_calls_graph_levels(
    n: usize,
    adj: &[Vec<usize>],
    order: &[usize],
) -> Vec<Vec<usize>> {
    let mut levels: Vec<Vec<usize>> = Vec::new();

    // Mapping original node → its node id in the CURRENT (possibly condensed)
    // graph. Starts as identity; after each aggregation it points at the
    // super-node a node now lives in.
    let mut orig_to_cur: Vec<usize> = (0..n).collect();
    let mut cur_n = n;
    let mut cur_adj: Vec<Vec<usize>> = adj.to_vec();
    let mut cur_order: Vec<usize> = order.to_vec();

    loop {
        // Local-move on the current graph, then dense-relabel to 0..k.
        let raw = cluster_calls_graph(cur_n, &cur_adj, &cur_order);
        let mut relabel: HashMap<usize, usize> = HashMap::new();
        let mut k = 0usize;
        let mut comm_of_cur: Vec<usize> = vec![0; cur_n];
        for (i, c) in comm_of_cur.iter_mut().enumerate() {
            *c = *relabel.entry(raw[i]).or_insert_with(|| {
                let x = k;
                k += 1;
                x
            });
        }

        // Project onto the original nodes and record this level.
        let level_labels: Vec<usize> = (0..n).map(|o| comm_of_cur[orig_to_cur[o]]).collect();
        levels.push(level_labels);

        // No community absorbed another → coarsest stable partition reached.
        if k == cur_n {
            break;
        }

        // Aggregate into the condensed graph: every current edge i→j becomes
        // comm(i)→comm(j). Intra-community edges become self-loops, which keeps
        // each super-node's degree (= sum of its members' degrees) intact so the
        // next level's modularity is computed on the right `2m`.
        let mut new_adj: Vec<Vec<usize>> = vec![Vec::new(); k];
        for (i, nbrs) in cur_adj.iter().enumerate() {
            let ci = comm_of_cur[i];
            for &j in nbrs {
                new_adj[ci].push(comm_of_cur[j]);
            }
        }

        orig_to_cur = (0..n).map(|o| comm_of_cur[orig_to_cur[o]]).collect();
        cur_n = k;
        cur_adj = new_adj;
        // Deterministic visit order on the condensed graph: ascending super-node id.
        cur_order = (0..k).collect();
    }

    levels
}

/// Given a multi-level community labeling (`levels[L][i]` = node `i`'s community
/// at level `L`, as returned by [`cluster_calls_graph_levels`]), compute each
/// multi-member community's PARENT in the hierarchy: the first STRICTLY-larger
/// enclosing community at a higher level (the immediate super-community in the
/// tree of distinct groupings). Returns a map `(level, label) -> parent` where
/// `parent` is `Some((level', label'))` or `None` for top-level communities.
///
/// Because coarsening is monotonic, all members of a community share the same
/// label at every higher level, so any representative member resolves the
/// parent. Singletons are omitted (they aren't stored). Pure; unit-tested.
pub fn community_parents(levels: &[Vec<usize>]) -> HashMap<(usize, usize), Option<(usize, usize)>> {
    let mut out: HashMap<(usize, usize), Option<(usize, usize)>> = HashMap::new();
    if levels.is_empty() {
        return out;
    }
    let n = levels[0].len();

    // Per-level community sizes (label -> member count).
    let sizes: Vec<HashMap<usize, usize>> = levels
        .iter()
        .map(|labels| {
            let mut s: HashMap<usize, usize> = HashMap::new();
            for &l in labels {
                *s.entry(l).or_insert(0) += 1;
            }
            s
        })
        .collect();

    for (lvl, labels) in levels.iter().enumerate() {
        let mut seen: HashSet<usize> = HashSet::new();
        for (i, &c) in labels.iter().enumerate().take(n) {
            if !seen.insert(c) {
                continue; // already handled this community via its first member
            }
            let size_c = sizes[lvl][&c];
            if size_c < 2 {
                continue; // singletons aren't stored
            }
            // Walk up: first higher level where this member's enclosing community
            // is strictly larger is the immediate parent.
            let mut parent: Option<(usize, usize)> = None;
            for (l2, sizes_l2) in sizes.iter().enumerate().skip(lvl + 1) {
                let pc = levels[l2][i];
                if sizes_l2[&pc] > size_c {
                    parent = Some((l2, pc));
                    break;
                }
            }
            out.insert((lvl, c), parent);
        }
    }
    out
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

    // Full multi-level Louvain: levels[0] is the finest partition (drives
    // code_community_id, as before); coarser levels feed the hierarchy.
    let levels = cluster_calls_graph_levels(n, &adj, &order);
    let label = &levels[0];

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

    // Reconcile the registry across ALL hierarchy levels — only multi-member
    // communities (singletons are noise + not worth a summary). Each distinct
    // grouping (member_hash) is recorded ONCE, at the FINEST level it appears:
    // iterating levels[0] (finest) → coarsest and skipping already-seen hashes
    // means a community that never merges stays at level 0 and only genuinely
    // coarser super-communities get a higher level. Stable member_hash so
    // summaries survive across runs.
    // Group nodes by (level, label) once, and hash every multi-member group so
    // both a community's own hash AND its parent's hash come from one map.
    let mut level_groups: Vec<HashMap<usize, Vec<usize>>> = Vec::with_capacity(levels.len());
    for labels in &levels {
        let mut by_label: HashMap<usize, Vec<usize>> = HashMap::new();
        for (i, &l) in labels.iter().enumerate() {
            by_label.entry(l).or_default().push(i);
        }
        level_groups.push(by_label);
    }
    let mut group_hash: HashMap<(usize, usize), String> = HashMap::new();
    for (lvl, groups) in level_groups.iter().enumerate() {
        for (&label, idxs) in groups {
            if idxs.len() < 2 {
                continue;
            }
            let paths: Vec<String> = idxs.iter().map(|&i| node_rows[i].1.clone()).collect();
            group_hash.insert((lvl, label), community_member_hash(&paths));
        }
    }
    let parents = community_parents(&levels);

    let mut hashes: Vec<String> = Vec::new();
    let mut god_ids: Vec<uuid::Uuid> = Vec::new();
    let mut counts: Vec<i32> = Vec::new();
    let mut row_levels: Vec<i32> = Vec::new();
    let mut parent_hashes: Vec<Option<String>> = Vec::new();
    let mut seen_hashes: HashSet<String> = HashSet::new();
    for (lvl, groups) in level_groups.iter().enumerate() {
        for (&label, idxs) in groups {
            if idxs.len() < 2 {
                continue;
            }
            let hash = group_hash[&(lvl, label)].clone();
            if !seen_hashes.insert(hash.clone()) {
                continue; // identical grouping already recorded at a finer level
            }
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
            // Parent = the immediate strictly-larger enclosing community (a tree
            // edge up the hierarchy), keyed by its member_hash. NULL = top.
            let parent_hash = parents
                .get(&(lvl, label))
                .and_then(|p| p.as_ref())
                .and_then(|(l2, pc)| group_hash.get(&(*l2, *pc)).cloned());
            hashes.push(hash);
            god_ids.push(node_rows[god].0);
            counts.push(idxs.len() as i32);
            row_levels.push(lvl as i32);
            parent_hashes.push(parent_hash);
        }
    }

    // GC stale rows (and legacy NULL-hash), then upsert — preserving summaries on
    // unchanged (member_hash, level) rows.
    sqlx::query(
        "DELETE FROM brain_code_communities
          WHERE member_hash IS NULL
             OR (member_hash, level) NOT IN (
                 SELECT h, l FROM UNNEST($1::text[], $2::int[]) AS t(h, l)
             )",
    )
    .bind(&hashes)
    .bind(&row_levels)
    .execute(pool)
    .await
    .map_err(|e| format!("DB error pruning stale code communities: {e}"))?;

    sqlx::query(
        "INSERT INTO brain_code_communities
             (member_hash, god_node_id, member_count, level, parent_member_hash, updated_at)
         SELECT h, g, c, l, p, NOW()
         FROM UNNEST($1::text[], $2::uuid[], $3::int[], $4::int[], $5::text[]) AS t(h, g, c, l, p)
         ON CONFLICT (member_hash, level) DO UPDATE
           SET member_count       = EXCLUDED.member_count,
               god_node_id        = EXCLUDED.god_node_id,
               parent_member_hash = EXCLUDED.parent_member_hash,
               updated_at         = NOW()",
    )
    .bind(&hashes)
    .bind(&god_ids)
    .bind(&counts)
    .bind(&row_levels)
    .bind(&parent_hashes)
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
    use super::{
        cluster_calls_graph, cluster_calls_graph_levels, community_member_hash, community_parents,
    };

    #[test]
    fn community_parents_walks_to_first_strictly_larger() {
        // 3 pairs at level 0; pairs {0,1} and {2,3} merge at level 1; everything
        // merges at level 2.
        let levels = vec![
            vec![0, 0, 1, 1, 2, 2], // L0: three 2-member communities
            vec![0, 0, 0, 0, 1, 1], // L1: {0,1,2,3} (size 4) and {4,5} (size 2)
            vec![0, 0, 0, 0, 0, 0], // L2: everything (size 6)
        ];
        let p = community_parents(&levels);
        // Finest pairs: {0,1} and {2,3} enclose into the size-4 L1 community.
        assert_eq!(p[&(0, 0)], Some((1, 0)));
        assert_eq!(p[&(0, 1)], Some((1, 0)));
        // {4,5} stays size 2 at L1 (not strictly larger), so its parent is L2.
        assert_eq!(p[&(0, 2)], Some((2, 0)));
        // The L1 size-4 community and the L1 size-2 community both roll up to L2.
        assert_eq!(p[&(1, 0)], Some((2, 0)));
        assert_eq!(p[&(1, 1)], Some((2, 0)));
        // The top community has no parent.
        assert_eq!(p[&(2, 0)], None);
    }

    /// True if `a` and `b` induce the SAME partition (group nodes identically),
    /// ignoring the actual label values.
    fn same_partition(a: &[usize], b: &[usize]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        let n = a.len();
        for i in 0..n {
            for j in (i + 1)..n {
                if (a[i] == a[j]) != (b[i] == b[j]) {
                    return false;
                }
            }
        }
        true
    }

    /// `coarse` is a coarsening of `fine` iff every pair grouped in `fine` is
    /// still grouped in `coarse` (merges only, never splits).
    fn is_coarsening(fine: &[usize], coarse: &[usize]) -> bool {
        let n = fine.len();
        for i in 0..n {
            for j in (i + 1)..n {
                if fine[i] == fine[j] && coarse[i] != coarse[j] {
                    return false;
                }
            }
        }
        true
    }

    #[test]
    fn levels_level0_matches_single_level() {
        let adj = two_cliques();
        let order: Vec<usize> = (0..6).collect();
        let levels = cluster_calls_graph_levels(6, &adj, &order);
        assert!(!levels.is_empty(), "always at least one level");
        let flat = cluster_calls_graph(6, &adj, &order);
        assert!(
            same_partition(&levels[0], &flat),
            "finest level must match single-level Louvain's partition"
        );
    }

    #[test]
    fn levels_no_edges_all_singletons() {
        let adj: Vec<Vec<usize>> = vec![vec![]; 5];
        let order: Vec<usize> = (0..5).collect();
        let levels = cluster_calls_graph_levels(5, &adj, &order);
        assert_eq!(levels.len(), 1, "no edges → no aggregation → one level");
        // Every node its own singleton.
        let l = &levels[0];
        for i in 0..5 {
            for j in (i + 1)..5 {
                assert_ne!(l[i], l[j], "isolated nodes are singletons");
            }
        }
    }

    #[test]
    fn levels_are_monotonically_coarsening() {
        // A denser, multi-cluster graph: three triangles fully bridged to each
        // other — exercises aggregation without asserting an exact level count
        // (that depends on modularity thresholds; coarsening is the invariant).
        let adj = vec![
            vec![1, 2],       // 0  ┐ T1
            vec![0, 2],       // 1  │
            vec![0, 1, 3, 6], // 2  ┘ bridges → T2, T3
            vec![4, 5, 2],    // 3  ┐ T2
            vec![3, 5],       // 4  │
            vec![3, 4, 6],    // 5  ┘ bridge → T3
            vec![7, 8, 2, 5], // 6  ┐ T3 bridges → T1, T2
            vec![6, 8],       // 7  │
            vec![6, 7],       // 8  ┘
        ];
        let order: Vec<usize> = (0..9).collect();
        let levels = cluster_calls_graph_levels(9, &adj, &order);
        assert!(!levels.is_empty());
        for w in levels.windows(2) {
            assert!(
                is_coarsening(&w[0], &w[1]),
                "each level must only merge communities, never split: {:?} -> {:?}",
                w[0],
                w[1]
            );
        }
    }

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
