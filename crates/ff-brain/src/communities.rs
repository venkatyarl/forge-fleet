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

#[cfg(test)]
mod tests {
    use super::community_member_hash;

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
