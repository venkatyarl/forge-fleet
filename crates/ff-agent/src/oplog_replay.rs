use std::cmp::Ordering;

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct ReplayController {
    pool: PgPool,
    batch_size: i64,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ReplayReport {
    pub node_id: String,
    pub applied: usize,
    pub skipped: usize,
    pub last_sequence: i64,
    pub state_version: i64,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum MergeStrategy {
    Lww,
    Union,
}

#[derive(Debug, Clone)]
struct OpLogEntry {
    node_id: String,
    sequence: i64,
    operation_id: Uuid,
    entity_type: String,
    entity_id: String,
    field_name: String,
    merge_strategy: MergeStrategy,
    value: Value,
    observed_at: DateTime<Utc>,
    writer_id: String,
}

impl ReplayController {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            batch_size: 100,
        }
    }

    pub fn with_batch_size(mut self, batch_size: i64) -> Self {
        self.batch_size = batch_size.max(1);
        self
    }

    pub async fn replay_node(&self, node_id: &str) -> Result<ReplayReport> {
        if node_id.trim().is_empty() {
            bail!("node_id is required for OpLog replay");
        }

        let mut tx = self.pool.begin().await?;
        let (last_sequence, state_version) = lock_checkpoint(&mut tx, node_id).await?;
        mark_checkpoint_replaying(&mut tx, node_id, last_sequence, state_version).await?;

        let entries = load_batch(&mut tx, node_id, last_sequence, self.batch_size).await?;
        if entries.is_empty() {
            mark_checkpoint_idle(&mut tx, node_id, last_sequence, state_version).await?;
            tx.commit().await?;
            return Ok(ReplayReport {
                node_id: node_id.to_owned(),
                applied: 0,
                skipped: 0,
                last_sequence,
                state_version,
            });
        }

        if entries[0].sequence != last_sequence + 1 {
            let error = format!(
                "oplog gap for node {node_id}: expected sequence {}, found {}",
                last_sequence + 1,
                entries[0].sequence
            );
            mark_checkpoint_failed(&mut tx, node_id, last_sequence, state_version, &error).await?;
            tx.commit().await?;
            bail!(error);
        }

        let mut expected = last_sequence + 1;
        let mut applied = 0;
        let mut skipped = 0;
        for entry in &entries {
            if entry.sequence != expected {
                let error = format!(
                    "oplog gap for node {node_id}: expected sequence {expected}, found {}",
                    entry.sequence
                );
                mark_checkpoint_failed(&mut tx, node_id, last_sequence, state_version, &error)
                    .await?;
                tx.commit().await?;
                bail!(error);
            }

            if operation_already_applied(&mut tx, entry).await? {
                skipped += 1;
            } else {
                apply_entry(&mut tx, entry).await?;
                record_applied(&mut tx, entry).await?;
                applied += 1;
            }
            expected += 1;
        }

        let new_last_sequence = entries
            .last()
            .map(|entry| entry.sequence)
            .ok_or_else(|| anyhow!("non-empty batch had no last entry"))?;
        let new_state_version = state_version + 1;
        mark_checkpoint_idle(&mut tx, node_id, new_last_sequence, new_state_version).await?;
        tx.commit().await?;

        Ok(ReplayReport {
            node_id: node_id.to_owned(),
            applied,
            skipped,
            last_sequence: new_last_sequence,
            state_version: new_state_version,
        })
    }
}

async fn lock_checkpoint(tx: &mut Transaction<'_, Postgres>, node_id: &str) -> Result<(i64, i64)> {
    sqlx::query(
        "INSERT INTO oplog_replay_checkpoints (node_id)
         VALUES ($1)
         ON CONFLICT (node_id) DO NOTHING",
    )
    .bind(node_id)
    .execute(&mut **tx)
    .await?;

    let row = sqlx::query(
        "SELECT last_sequence, state_version
           FROM oplog_replay_checkpoints
          WHERE node_id = $1
          FOR UPDATE",
    )
    .bind(node_id)
    .fetch_one(&mut **tx)
    .await?;

    Ok((row.try_get("last_sequence")?, row.try_get("state_version")?))
}

async fn mark_checkpoint_replaying(
    tx: &mut Transaction<'_, Postgres>,
    node_id: &str,
    last_sequence: i64,
    state_version: i64,
) -> Result<()> {
    update_checkpoint(tx, node_id, last_sequence, state_version, "replaying", None).await
}

async fn mark_checkpoint_idle(
    tx: &mut Transaction<'_, Postgres>,
    node_id: &str,
    last_sequence: i64,
    state_version: i64,
) -> Result<()> {
    update_checkpoint(tx, node_id, last_sequence, state_version, "idle", None).await
}

async fn mark_checkpoint_failed(
    tx: &mut Transaction<'_, Postgres>,
    node_id: &str,
    last_sequence: i64,
    state_version: i64,
    error: &str,
) -> Result<()> {
    update_checkpoint(
        tx,
        node_id,
        last_sequence,
        state_version + 1,
        "failed",
        Some(error),
    )
    .await
}

async fn update_checkpoint(
    tx: &mut Transaction<'_, Postgres>,
    node_id: &str,
    last_sequence: i64,
    state_version: i64,
    state: &str,
    last_error: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE oplog_replay_checkpoints
            SET last_sequence = $2,
                state = $3,
                state_version = $4,
                last_error = $5,
                updated_at = NOW()
          WHERE node_id = $1",
    )
    .bind(node_id)
    .bind(last_sequence)
    .bind(state)
    .bind(state_version)
    .bind(last_error)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn load_batch(
    tx: &mut Transaction<'_, Postgres>,
    node_id: &str,
    after_sequence: i64,
    batch_size: i64,
) -> Result<Vec<OpLogEntry>> {
    let rows = sqlx::query(
        "SELECT node_id, sequence, operation_id, entity_type, entity_id, field_name,
                merge_strategy, value, observed_at, writer_id
           FROM isolated_node_oplog
          WHERE node_id = $1 AND sequence > $2
          ORDER BY sequence
          LIMIT $3",
    )
    .bind(node_id)
    .bind(after_sequence)
    .bind(batch_size)
    .fetch_all(&mut **tx)
    .await?;

    rows.into_iter()
        .map(|row| {
            let merge_strategy: String = row.try_get("merge_strategy")?;
            Ok(OpLogEntry {
                node_id: row.try_get("node_id")?,
                sequence: row.try_get("sequence")?,
                operation_id: row.try_get("operation_id")?,
                entity_type: row.try_get("entity_type")?,
                entity_id: row.try_get("entity_id")?,
                field_name: row.try_get("field_name")?,
                merge_strategy: parse_strategy(&merge_strategy)?,
                value: row.try_get("value")?,
                observed_at: row.try_get("observed_at")?,
                writer_id: row.try_get("writer_id")?,
            })
        })
        .collect()
}

fn parse_strategy(strategy: &str) -> Result<MergeStrategy> {
    match strategy {
        "LWW" => Ok(MergeStrategy::Lww),
        "UNION" => Ok(MergeStrategy::Union),
        other => bail!("unsupported OpLog merge strategy: {other}"),
    }
}

async fn operation_already_applied(
    tx: &mut Transaction<'_, Postgres>,
    entry: &OpLogEntry,
) -> Result<bool> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(
             SELECT 1 FROM oplog_replay_applied
              WHERE node_id = $1 AND sequence = $2
         )",
    )
    .bind(&entry.node_id)
    .bind(entry.sequence)
    .fetch_one(&mut **tx)
    .await?;
    Ok(exists)
}

async fn apply_entry(tx: &mut Transaction<'_, Postgres>, entry: &OpLogEntry) -> Result<()> {
    lock_state_field(tx, entry).await?;
    match entry.merge_strategy {
        MergeStrategy::Lww => apply_lww(tx, entry).await,
        MergeStrategy::Union => apply_union(tx, entry).await,
    }
}

async fn lock_state_field(tx: &mut Transaction<'_, Postgres>, entry: &OpLogEntry) -> Result<()> {
    let lock_key = format!(
        "{}\u{1f}{}\u{1f}{}",
        entry.entity_type, entry.entity_id, entry.field_name
    );
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 282))")
        .bind(lock_key)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn apply_lww(tx: &mut Transaction<'_, Postgres>, entry: &OpLogEntry) -> Result<()> {
    let current = sqlx::query(
        "SELECT lww_observed_at, lww_writer_id, lww_sequence
           FROM oplog_shared_state
          WHERE entity_type = $1 AND entity_id = $2 AND field_name = $3
          FOR UPDATE",
    )
    .bind(&entry.entity_type)
    .bind(&entry.entity_id)
    .bind(&entry.field_name)
    .fetch_optional(&mut **tx)
    .await?;

    let incoming_wins = match current {
        Some(row) => {
            let current_at: Option<DateTime<Utc>> = row.try_get("lww_observed_at")?;
            let current_writer: Option<String> = row.try_get("lww_writer_id")?;
            let current_sequence: Option<i64> = row.try_get("lww_sequence")?;
            compare_lww(
                entry.observed_at,
                &entry.writer_id,
                entry.sequence,
                current_at,
                current_writer.as_deref(),
                current_sequence,
            ) == Ordering::Greater
        }
        None => true,
    };

    if !incoming_wins {
        return Ok(());
    }

    sqlx::query(
        "INSERT INTO oplog_shared_state
            (entity_type, entity_id, field_name, value, merge_strategy,
             lww_observed_at, lww_writer_id, lww_sequence, version)
         VALUES ($1, $2, $3, $4, 'LWW', $5, $6, $7, 1)
         ON CONFLICT (entity_type, entity_id, field_name) DO UPDATE
            SET value = EXCLUDED.value,
                merge_strategy = EXCLUDED.merge_strategy,
                lww_observed_at = EXCLUDED.lww_observed_at,
                lww_writer_id = EXCLUDED.lww_writer_id,
                lww_sequence = EXCLUDED.lww_sequence,
                version = oplog_shared_state.version + 1,
                updated_at = NOW()",
    )
    .bind(&entry.entity_type)
    .bind(&entry.entity_id)
    .bind(&entry.field_name)
    .bind(&entry.value)
    .bind(entry.observed_at)
    .bind(&entry.writer_id)
    .bind(entry.sequence)
    .execute(&mut **tx)
    .await?;

    Ok(())
}

fn compare_lww(
    incoming_at: DateTime<Utc>,
    incoming_writer: &str,
    incoming_sequence: i64,
    current_at: Option<DateTime<Utc>>,
    current_writer: Option<&str>,
    current_sequence: Option<i64>,
) -> Ordering {
    (
        Some(incoming_at),
        Some(incoming_writer),
        Some(incoming_sequence),
    )
        .cmp(&(current_at, current_writer, current_sequence))
}

async fn apply_union(tx: &mut Transaction<'_, Postgres>, entry: &OpLogEntry) -> Result<()> {
    let current = sqlx::query(
        "SELECT value
           FROM oplog_shared_state
          WHERE entity_type = $1 AND entity_id = $2 AND field_name = $3
          FOR UPDATE",
    )
    .bind(&entry.entity_type)
    .bind(&entry.entity_id)
    .bind(&entry.field_name)
    .fetch_optional(&mut **tx)
    .await?;

    let current_value = current
        .map(|row| row.try_get::<Value, _>("value"))
        .transpose()?
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let merged = canonical_union(current_value, entry.value.clone());

    sqlx::query(
        "INSERT INTO oplog_shared_state
            (entity_type, entity_id, field_name, value, merge_strategy, version)
         VALUES ($1, $2, $3, $4, 'UNION', 1)
         ON CONFLICT (entity_type, entity_id, field_name) DO UPDATE
            SET value = EXCLUDED.value,
                merge_strategy = EXCLUDED.merge_strategy,
                version = CASE
                    WHEN oplog_shared_state.value = EXCLUDED.value
                    THEN oplog_shared_state.version
                    ELSE oplog_shared_state.version + 1
                END,
                updated_at = CASE
                    WHEN oplog_shared_state.value = EXCLUDED.value
                    THEN oplog_shared_state.updated_at
                    ELSE NOW()
                END",
    )
    .bind(&entry.entity_type)
    .bind(&entry.entity_id)
    .bind(&entry.field_name)
    .bind(merged)
    .execute(&mut **tx)
    .await?;

    Ok(())
}

fn canonical_union(left: Value, right: Value) -> Value {
    let mut values = Vec::new();
    push_union_values(&mut values, left);
    push_union_values(&mut values, right);
    values.sort_by_key(|value| value.to_string());
    values.dedup();
    Value::Array(values)
}

fn push_union_values(values: &mut Vec<Value>, value: Value) {
    match value {
        Value::Array(items) => values.extend(items),
        Value::Null => {}
        scalar => values.push(scalar),
    }
}

async fn record_applied(tx: &mut Transaction<'_, Postgres>, entry: &OpLogEntry) -> Result<()> {
    sqlx::query(
        "INSERT INTO oplog_replay_applied (node_id, sequence, operation_id)
         VALUES ($1, $2, $3)
         ON CONFLICT DO NOTHING",
    )
    .bind(&entry.node_id)
    .bind(entry.sequence)
    .bind(entry.operation_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_union_sorts_and_deduplicates_values() {
        let merged = canonical_union(
            serde_json::json!(["b", "a", "a"]),
            serde_json::json!(["c", "b"]),
        );
        assert_eq!(merged, serde_json::json!(["a", "b", "c"]));
    }

    #[test]
    fn lww_tie_breaks_by_writer_then_sequence() {
        let at = Utc::now();
        assert_eq!(
            compare_lww(at, "node-b", 1, Some(at), Some("node-a"), Some(99)),
            Ordering::Greater
        );
        assert_eq!(
            compare_lww(at, "node-a", 100, Some(at), Some("node-a"), Some(99)),
            Ordering::Greater
        );
    }
}
