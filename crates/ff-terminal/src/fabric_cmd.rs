//! `ff fabric pair <a> <b> --kind cx7` — record that computers A and B are
//! linked by a private fabric (CX-7 / InfiniBand / RoCE). Does NOT assign
//! IPs; that's still a manual nmcli step. Inserts a `fabric_pairs` row
//! with NULL IPs so the materializer can fill them once both daemons
//! start emitting cx7-fabric Ip entries with `paired_with`.

use anyhow::{bail, Context, Result};
use sqlx::{PgPool, Row};
use uuid::Uuid;

pub async fn handle_fabric_pair(
    pg: &PgPool,
    a: &str,
    b: &str,
    kind: &str,
) -> Result<()> {
    if a == b {
        bail!("cannot pair a computer with itself");
    }
    let (a_name, b_name) = if a < b { (a, b) } else { (b, a) };
    let pair_name = format!("{}-{}", a_name, b_name);

    let row_a = sqlx::query("SELECT id FROM computers WHERE name = $1")
        .bind(a_name).fetch_optional(pg).await?
        .with_context(|| format!("computer '{}' not found", a_name))?;
    let row_b = sqlx::query("SELECT id FROM computers WHERE name = $1")
        .bind(b_name).fetch_optional(pg).await?
        .with_context(|| format!("computer '{}' not found", b_name))?;
    let a_id: Uuid = row_a.try_get("id")?;
    let b_id: Uuid = row_b.try_get("id")?;

    sqlx::query(
        "INSERT INTO fabric_pairs \
            (pair_name, fabric_kind, computer_a_id, computer_b_id, \
             a_iface, b_iface, a_ip, b_ip) \
         VALUES ($1, $2, $3, $4, '', '', '', '') \
         ON CONFLICT (pair_name) DO UPDATE SET fabric_kind = EXCLUDED.fabric_kind"
    )
    .bind(&pair_name).bind(kind).bind(a_id).bind(b_id)
    .execute(pg).await?;

    println!("Paired: {} (kind={})", pair_name, kind);
    println!("Next: configure IPs via nmcli on both hosts, then beats will auto-populate iface/ip.");
    Ok(())
}
