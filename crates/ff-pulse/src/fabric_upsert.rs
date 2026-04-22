//! V43 materializer helpers: fabric pairs + ray cluster memberships.
//!
//! Called by `Materializer::materialize_beat` (in materializer.rs) once
//! per beat, AFTER the computers row is upserted. Keeps `fabric_pairs`,
//! `llm_clusters` in sync with what the beat reports.

use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::beat_v2::PulseBeatV2;

/// For every fabric-kind Ip in the beat with a `paired_with` hint,
/// upsert a row in `fabric_pairs` keyed by a lexicographic pair_name
/// (so "sia-adele" == "adele-sia"). Both sides' beats converge on the
/// same row.
pub async fn upsert_fabric_pairs(
    pg: &PgPool,
    beat: &PulseBeatV2,
    computer_id: Uuid,
) -> Result<(), sqlx::Error> {
    for ip_entry in &beat.network.all_ips {
        if !ip_entry.kind.ends_with("-fabric") {
            continue;
        }
        let peer_name = match ip_entry.paired_with.as_deref() {
            Some(p) => p,
            None => continue,
        };

        let peer_row = sqlx::query("SELECT id FROM computers WHERE name = $1 LIMIT 1")
            .bind(peer_name)
            .fetch_optional(pg)
            .await?;
        let peer_id: Uuid = match peer_row {
            Some(r) => r.try_get("id")?,
            None => continue,
        };

        let my_name = beat.computer_name.as_str();
        let (a_name, a_id, b_name, b_id) = if my_name < peer_name {
            (my_name, computer_id, peer_name, peer_id)
        } else {
            (peer_name, peer_id, my_name, computer_id)
        };
        let pair_name = format!("{}-{}", a_name, b_name);

        let (a_iface, a_ip, b_iface, b_ip) = if my_name == a_name {
            (ip_entry.iface.as_str(), ip_entry.ip.as_str(), "", "")
        } else {
            ("", "", ip_entry.iface.as_str(), ip_entry.ip.as_str())
        };

        sqlx::query(
            "INSERT INTO fabric_pairs \
                (pair_name, fabric_kind, computer_a_id, computer_b_id, \
                 a_iface, b_iface, a_ip, b_ip) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (pair_name) DO UPDATE SET \
                fabric_kind = EXCLUDED.fabric_kind, \
                a_iface = CASE WHEN EXCLUDED.a_iface <> '' THEN EXCLUDED.a_iface ELSE fabric_pairs.a_iface END, \
                a_ip    = CASE WHEN EXCLUDED.a_ip    <> '' THEN EXCLUDED.a_ip    ELSE fabric_pairs.a_ip    END, \
                b_iface = CASE WHEN EXCLUDED.b_iface <> '' THEN EXCLUDED.b_iface ELSE fabric_pairs.b_iface END, \
                b_ip    = CASE WHEN EXCLUDED.b_ip    <> '' THEN EXCLUDED.b_ip    ELSE fabric_pairs.b_ip    END",
        )
        .bind(&pair_name)
        .bind(&ip_entry.kind)
        .bind(a_id)
        .bind(b_id)
        .bind(a_iface)
        .bind(b_iface)
        .bind(a_ip)
        .bind(b_ip)
        .execute(pg)
        .await?;
    }
    Ok(())
}

/// Upsert ray cluster memberships into `llm_clusters`. The beat reports
/// which clusters this daemon participates in (head or worker). Heads
/// are authoritative for creating the row; workers merely add themselves
/// to the worker_computer_ids set.
pub async fn upsert_ray_memberships(
    pg: &PgPool,
    beat: &PulseBeatV2,
    computer_id: Uuid,
) -> Result<(), sqlx::Error> {
    let mhp = match &beat.multi_host_participation {
        Some(m) => m,
        None => return Ok(()),
    };
    for rc in &mhp.ray_clusters {
        if rc.role == "head" {
            sqlx::query(
                "INSERT INTO llm_clusters \
                    (id, model_id, runtime, topology, head_computer_id, \
                     worker_computer_ids, ray_head_endpoint, api_endpoint, \
                     status, last_health_at) \
                 VALUES ($1, 'unknown', 'vllm', 'tp', $2, '[]', $3, '', 'healthy', NOW()) \
                 ON CONFLICT (id) DO UPDATE SET \
                    head_computer_id = EXCLUDED.head_computer_id, \
                    ray_head_endpoint = EXCLUDED.ray_head_endpoint, \
                    last_health_at = NOW()",
            )
            .bind(&rc.cluster_id)
            .bind(computer_id)
            .bind(&rc.head_endpoint)
            .execute(pg)
            .await?;
        } else {
            let id_str = computer_id.to_string();
            sqlx::query(
                "UPDATE llm_clusters \
                    SET worker_computer_ids = \
                        CASE WHEN worker_computer_ids @> to_jsonb($2::text) \
                             THEN worker_computer_ids \
                             ELSE worker_computer_ids || to_jsonb($2::text) END, \
                        last_health_at = NOW() \
                  WHERE id = $1",
            )
            .bind(&rc.cluster_id)
            .bind(&id_str)
            .execute(pg)
            .await?;
        }
    }
    Ok(())
}
