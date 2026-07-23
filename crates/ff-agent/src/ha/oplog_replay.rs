//! OpLog replay engine: merges an isolated node's recorded operations back
//! into the shared DB once it reconnects.
//!
//! An isolated node keeps writing locally while partitioned from the fleet.
//! Its operations are shipped into `oplog_entries` on reconnect; this module
//! reads them back out in per-node sequence order, resolves each entry
//! against the shared `oplog_entity_state` using the merge strategy implied
//! by its `OpType` (last-write-wins for `Upsert`/`Delete`, set-union for
//! `Union`), and advances `oplog_replay_state.last_applied_sequence` one
//! entry at a time so a failure partway through a batch leaves already-merged
//! entries committed instead of rolling the whole batch back.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{PgPool, Row};

/// How an isolated node's operation should be merged into shared state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpType {
    /// Replace the entity's data outright. Resolved via last-write-wins:
    /// applied only if `op_version` is newer than the entity's current
    /// version.
    Upsert,
    /// Remove the entity. Also last-write-wins: applied only if `op_version`
    /// is newer than the entity's current version. Implemented as a
    /// tombstone (see [`oplog_entity_state`](self) docs) rather than a
    /// physical row delete, so the version survives for future comparisons.
    Delete,
    /// Merge `payload` (a JSON array) into the entity's existing array,
    /// de-duplicating. Commutative, so it applies regardless of version
    /// ordering; the entity's version is bumped to the max of the two.
    Union,
}

impl OpType {
    fn as_str(self) -> &'static str {
        match self {
            OpType::Upsert => "upsert",
            OpType::Delete => "delete",
            OpType::Union => "union",
        }
    }

    fn parse(raw: &str) -> Result<Self, ReplayApplyError> {
        match raw {
            "upsert" => Ok(OpType::Upsert),
            "delete" => Ok(OpType::Delete),
            "union" => Ok(OpType::Union),
            other => Err(ReplayApplyError::UnknownOpType(other.to_string())),
        }
    }
}

/// A single operation recorded by an isolated node, staged in `oplog_entries`
/// for replay.
#[derive(Debug, Clone)]
pub struct OpLogEntry {
    pub id: uuid::Uuid,
    pub node_id: String,
    pub sequence: i64,
    pub entity_type: String,
    pub entity_id: String,
    pub op_type: String,
    pub op_version: i64,
    pub payload: Value,
    pub recorded_at: DateTime<Utc>,
}

/// Records an operation from an isolated node into the replay staging table.
/// Idempotent on `(node_id, sequence)`: re-ingesting the same entry is a
/// no-op rather than a duplicate.
pub async fn record_op(
    pool: &PgPool,
    node_id: &str,
    sequence: i64,
    entity_type: &str,
    entity_id: &str,
    op_type: OpType,
    op_version: i64,
    payload: Value,
    recorded_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO oplog_entries
            (node_id, sequence, entity_type, entity_id, op_type, op_version, payload, recorded_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (node_id, sequence) DO NOTHING",
    )
    .bind(node_id)
    .bind(sequence)
    .bind(entity_type)
    .bind(entity_id)
    .bind(op_type.as_str())
    .bind(op_version)
    .bind(payload)
    .bind(recorded_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// A per-entry, data-level failure encountered while applying a replay
/// batch (as opposed to a systemic DB error) — bad input the isolated node
/// shipped, not an infrastructure problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayApplyError {
    /// `op_type` on the entry isn't one this engine understands.
    UnknownOpType(String),
    /// A `Union` entry's payload wasn't a JSON array.
    UnionPayloadNotArray,
}

impl std::fmt::Display for ReplayApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplayApplyError::UnknownOpType(raw) => write!(f, "unknown op_type '{raw}'"),
            ReplayApplyError::UnionPayloadNotArray => {
                write!(f, "union op payload must be a JSON array")
            }
        }
    }
}

/// Internal distinction between a data-level failure — recorded to
/// `oplog_replay_failures` so replay can stop gracefully and resume later —
/// and a systemic DB error, which is propagated as a hard error from
/// [`replay_node`] since it likely means every subsequent entry would fail
/// too (and writing the failure audit row would itself fail).
enum ApplyError {
    Data(ReplayApplyError),
    Db(sqlx::Error),
}

impl From<sqlx::Error> for ApplyError {
    fn from(err: sqlx::Error) -> Self {
        ApplyError::Db(err)
    }
}

impl From<ReplayApplyError> for ApplyError {
    fn from(err: ReplayApplyError) -> Self {
        ApplyError::Data(err)
    }
}

/// Outcome of a [`replay_node`] call. A non-empty `failure` means the replay
/// stopped early — entries up to (and including) `applied` sequences were
/// still committed, and a later call to [`replay_node`] will resume from
/// there once the offending entry is fixed or skipped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplaySummary {
    /// Number of entries successfully applied in this call.
    pub applied: u64,
    /// Sequence of the last successfully applied entry, if any.
    pub last_applied_sequence: Option<i64>,
    /// Set when replay stopped early due to a per-entry error.
    pub failure: Option<ReplayFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayFailure {
    pub sequence: i64,
    pub error: String,
}

/// Reads an isolated node's staged OpLog entries in sequence order (starting
/// just after the node's last successfully applied sequence) and merges each
/// one into `oplog_entity_state`.
///
/// Each entry is applied in its own transaction, so a failure partway through
/// leaves prior entries committed. On failure the offending entry is recorded
/// in `oplog_replay_failures`, the node's replay state is marked `failed`,
/// and replay stops — the returned [`ReplaySummary`] reflects the partial
/// progress rather than the whole call failing.
pub async fn replay_node(pool: &PgPool, node_id: &str) -> Result<ReplaySummary, sqlx::Error> {
    let last_applied: i64 = sqlx::query_scalar(
        "INSERT INTO oplog_replay_state (node_id, last_applied_sequence, status)
         VALUES ($1, 0, 'idle')
         ON CONFLICT (node_id) DO UPDATE SET node_id = EXCLUDED.node_id
         RETURNING last_applied_sequence",
    )
    .bind(node_id)
    .fetch_one(pool)
    .await?;

    let entries = sqlx::query(
        "SELECT id, node_id, sequence, entity_type, entity_id, op_type, op_version, payload, recorded_at
           FROM oplog_entries
          WHERE node_id = $1 AND sequence > $2
          ORDER BY sequence ASC",
    )
    .bind(node_id)
    .bind(last_applied)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| OpLogEntry {
        id: row.get("id"),
        node_id: row.get("node_id"),
        sequence: row.get("sequence"),
        entity_type: row.get("entity_type"),
        entity_id: row.get("entity_id"),
        op_type: row.get("op_type"),
        op_version: row.get("op_version"),
        payload: row.get("payload"),
        recorded_at: row.get("recorded_at"),
    })
    .collect::<Vec<_>>();

    let mut summary = ReplaySummary::default();

    for entry in &entries {
        let mut tx = pool.begin().await?;
        match apply_entry(&mut tx, entry).await {
            Ok(()) => {
                sqlx::query(
                    "UPDATE oplog_replay_state
                        SET last_applied_sequence = $2, status = 'idle', last_error = NULL, updated_at = NOW()
                      WHERE node_id = $1",
                )
                .bind(node_id)
                .bind(entry.sequence)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                summary.applied += 1;
                summary.last_applied_sequence = Some(entry.sequence);
            }
            Err(ApplyError::Db(db_err)) => {
                // Systemic failure (e.g. connection loss): roll back this
                // entry's transaction and let it bubble as a hard error
                // rather than recording it as a data-level replay failure —
                // the DB itself, not this entry's payload, is the problem.
                tx.rollback().await?;
                return Err(db_err);
            }
            Err(ApplyError::Data(app_err)) => {
                tx.rollback().await?;
                let message = app_err.to_string();
                sqlx::query(
                    "INSERT INTO oplog_replay_failures (entry_id, node_id, sequence, error)
                     VALUES ($1, $2, $3, $4)",
                )
                .bind(entry.id)
                .bind(node_id)
                .bind(entry.sequence)
                .bind(&message)
                .execute(pool)
                .await?;
                sqlx::query(
                    "UPDATE oplog_replay_state
                        SET status = 'failed', last_error = $2, updated_at = NOW()
                      WHERE node_id = $1",
                )
                .bind(node_id)
                .bind(&message)
                .execute(pool)
                .await?;
                summary.failure = Some(ReplayFailure {
                    sequence: entry.sequence,
                    error: message,
                });
                break;
            }
        }
    }

    Ok(summary)
}

/// Current merged state of an entity, as read from `oplog_entity_state`
/// before applying a new entry against it. The version alone drives the LWW
/// comparison — including for a tombstoned (deleted) row, which is why
/// `Delete` never removes the row: a missing row and a tombstoned row must
/// be distinguishable so a stale operation can't resurrect deleted state.
struct CurrentState {
    version: i64,
    data: Option<Value>,
}

async fn apply_entry(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    entry: &OpLogEntry,
) -> Result<(), ApplyError> {
    let op_type = OpType::parse(&entry.op_type)?;

    let current = sqlx::query(
        "SELECT version, data FROM oplog_entity_state
          WHERE entity_type = $1 AND entity_id = $2
          FOR UPDATE",
    )
    .bind(&entry.entity_type)
    .bind(&entry.entity_id)
    .fetch_optional(&mut **tx)
    .await?
    .map(|row| CurrentState {
        version: row.get("version"),
        data: row.get("data"),
    });

    match op_type {
        OpType::Upsert => {
            let is_newer = current
                .as_ref()
                .map(|state| entry.op_version > state.version)
                .unwrap_or(true);
            if is_newer {
                upsert_entity_state(
                    tx,
                    entry,
                    entry.op_version,
                    Some(entry.payload.clone()),
                    false,
                )
                .await?;
            }
        }
        OpType::Delete => {
            // Last-write-wins, same as Upsert — but instead of physically
            // removing the row, write a tombstone that keeps the row's
            // version. A hard DELETE would erase that version entirely: a
            // later replay of an older, stale entry (op_version <= this
            // delete's) would then see no row at all, treat itself as the
            // first write ever seen for the entity, and resurrect it.
            let is_newer = current
                .as_ref()
                .map(|state| entry.op_version > state.version)
                .unwrap_or(true);
            if is_newer {
                upsert_entity_state(tx, entry, entry.op_version, None, true).await?;
            }
        }
        OpType::Union => {
            let incoming = entry
                .payload
                .as_array()
                .ok_or(ReplayApplyError::UnionPayloadNotArray)?;
            let existing_version = current.as_ref().map(|state| state.version).unwrap_or(0);
            let existing_items = current
                .as_ref()
                .and_then(|state| state.data.as_ref())
                .and_then(|data| data.as_array())
                .cloned()
                .unwrap_or_default();
            let merged = union_json_arrays(&existing_items, incoming);
            let new_version = existing_version.max(entry.op_version);
            upsert_entity_state(tx, entry, new_version, Some(Value::Array(merged)), false).await?;
        }
    }

    Ok(())
}

async fn upsert_entity_state(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    entry: &OpLogEntry,
    version: i64,
    data: Option<Value>,
    deleted: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO oplog_entity_state (entity_type, entity_id, version, data, deleted, updated_at)
         VALUES ($1, $2, $3, $4, $5, NOW())
         ON CONFLICT (entity_type, entity_id)
         DO UPDATE SET version = EXCLUDED.version,
                        data = EXCLUDED.data,
                        deleted = EXCLUDED.deleted,
                        updated_at = NOW()",
    )
    .bind(&entry.entity_type)
    .bind(&entry.entity_id)
    .bind(version)
    .bind(data)
    .bind(deleted)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Union-merges two JSON arrays, de-duplicating while preserving `existing`'s
/// order followed by any genuinely new items from `incoming`.
fn union_json_arrays(existing: &[Value], incoming: &[Value]) -> Vec<Value> {
    let mut merged = existing.to_vec();
    for item in incoming {
        if !merged.contains(item) {
            merged.push(item.clone());
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_type_round_trips_through_text() {
        for op in [OpType::Upsert, OpType::Delete, OpType::Union] {
            assert_eq!(OpType::parse(op.as_str()).unwrap(), op);
        }
    }

    #[test]
    fn op_type_parse_rejects_unknown_values() {
        assert_eq!(
            OpType::parse("frobnicate"),
            Err(ReplayApplyError::UnknownOpType("frobnicate".to_string()))
        );
    }

    #[test]
    fn union_json_arrays_deduplicates_while_preserving_order() {
        let existing = vec![Value::from("a"), Value::from("b")];
        let incoming = vec![Value::from("b"), Value::from("c")];
        let merged = union_json_arrays(&existing, &incoming);
        assert_eq!(
            merged,
            vec![Value::from("a"), Value::from("b"), Value::from("c")]
        );
    }

    #[test]
    fn union_json_arrays_is_commutative_as_a_set() {
        let a = vec![Value::from("x"), Value::from("y")];
        let b = vec![Value::from("y"), Value::from("z")];

        let mut merged_ab: Vec<Value> = union_json_arrays(&a, &b);
        let mut merged_ba: Vec<Value> = union_json_arrays(&b, &a);
        merged_ab.sort_by_key(|v| v.to_string());
        merged_ba.sort_by_key(|v| v.to_string());
        assert_eq!(merged_ab, merged_ba);
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use std::env;

    fn temp_db_urls() -> Option<(String, String, String)> {
        let base_url = env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| env::var("FORGEFLEET_DATABASE_URL"))
            .ok()?;
        let (prefix, _) = base_url.rsplit_once('/')?;
        let db_name = format!("ff_oplog_replay_int_{}", uuid::Uuid::new_v4().simple());
        Some((
            format!("{prefix}/postgres"),
            format!("{prefix}/{db_name}"),
            db_name,
        ))
    }

    async fn create_temp_db() -> Option<(PgPool, PgPool, String)> {
        let (admin_url, db_url, db_name) = temp_db_urls()?;
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&admin_url)
            .await
            .expect("connect admin db");
        sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
            .execute(&admin)
            .await
            .expect("create temp db");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect(&db_url)
            .await
            .expect("connect temp db");
        sqlx::raw_sql("CREATE EXTENSION IF NOT EXISTS pgcrypto;")
            .execute(&pool)
            .await
            .expect("enable pgcrypto");
        sqlx::raw_sql(ff_db::schema::SCHEMA_V246_OPLOG_REPLAY)
            .execute(&pool)
            .await
            .expect("create oplog replay schema");
        Some((admin, pool, db_name))
    }

    async fn drop_temp_db(admin: PgPool, pool: PgPool, db_name: &str) {
        pool.close().await;
        sqlx::query(
            "SELECT pg_terminate_backend(pid)
               FROM pg_stat_activity
              WHERE datname = $1
                AND pid <> pg_backend_pid()",
        )
        .bind(db_name)
        .execute(&admin)
        .await
        .expect("terminate temp db sessions");
        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db_name}\""))
            .execute(&admin)
            .await
            .expect("drop temp db");
        admin.close().await;
    }

    #[tokio::test]
    async fn lww_upsert_applies_only_newer_versions() {
        let Some((admin, pool, db_name)) = create_temp_db().await else {
            eprintln!(
                "skipping lww_upsert_applies_only_newer_versions: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        };

        let node = "node-a";
        let now = Utc::now();
        record_op(
            &pool,
            node,
            1,
            "widget",
            "w1",
            OpType::Upsert,
            5,
            serde_json::json!({"name": "first"}),
            now,
        )
        .await
        .unwrap();
        // A later sequence but an OLDER logical version (e.g. the isolated
        // node re-shipped a stale write) must not clobber the newer one.
        record_op(
            &pool,
            node,
            2,
            "widget",
            "w1",
            OpType::Upsert,
            3,
            serde_json::json!({"name": "stale"}),
            now,
        )
        .await
        .unwrap();
        record_op(
            &pool,
            node,
            3,
            "widget",
            "w1",
            OpType::Upsert,
            9,
            serde_json::json!({"name": "latest"}),
            now,
        )
        .await
        .unwrap();

        let summary = replay_node(&pool, node).await.unwrap();
        assert_eq!(summary.applied, 3);
        assert_eq!(summary.last_applied_sequence, Some(3));
        assert!(summary.failure.is_none());

        let row = sqlx::query("SELECT version, data FROM oplog_entity_state WHERE entity_type = 'widget' AND entity_id = 'w1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let version: i64 = row.get("version");
        let data: Value = row.get("data");
        assert_eq!(version, 9);
        assert_eq!(data, serde_json::json!({"name": "latest"}));

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn union_merges_across_entries_regardless_of_order() {
        let Some((admin, pool, db_name)) = create_temp_db().await else {
            eprintln!(
                "skipping union_merges_across_entries_regardless_of_order: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        };

        let node = "node-b";
        let now = Utc::now();
        record_op(
            &pool,
            node,
            1,
            "tag_set",
            "project-1",
            OpType::Union,
            1,
            serde_json::json!(["alpha", "beta"]),
            now,
        )
        .await
        .unwrap();
        record_op(
            &pool,
            node,
            2,
            "tag_set",
            "project-1",
            OpType::Union,
            2,
            serde_json::json!(["beta", "gamma"]),
            now,
        )
        .await
        .unwrap();

        let summary = replay_node(&pool, node).await.unwrap();
        assert_eq!(summary.applied, 2);
        assert!(summary.failure.is_none());

        let row = sqlx::query("SELECT data FROM oplog_entity_state WHERE entity_type = 'tag_set' AND entity_id = 'project-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let data: Value = row.get("data");
        assert_eq!(data, serde_json::json!(["alpha", "beta", "gamma"]));

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn delete_writes_a_tombstone_that_keeps_the_version() {
        let Some((admin, pool, db_name)) = create_temp_db().await else {
            eprintln!(
                "skipping delete_writes_a_tombstone_that_keeps_the_version: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        };

        let node = "node-c";
        let now = Utc::now();
        record_op(
            &pool,
            node,
            1,
            "widget",
            "w2",
            OpType::Upsert,
            1,
            serde_json::json!({"name": "keep-me"}),
            now,
        )
        .await
        .unwrap();
        // A stale delete (lower version than the current row) must be a no-op.
        record_op(
            &pool,
            node,
            2,
            "widget",
            "w2",
            OpType::Delete,
            0,
            Value::Null,
            now,
        )
        .await
        .unwrap();
        record_op(
            &pool,
            node,
            3,
            "widget",
            "w2",
            OpType::Delete,
            2,
            Value::Null,
            now,
        )
        .await
        .unwrap();

        let summary = replay_node(&pool, node).await.unwrap();
        assert_eq!(summary.applied, 3);

        // The row survives as a tombstone (not physically deleted) so the
        // delete's version stays visible to later comparisons.
        let row = sqlx::query(
            "SELECT version, data, deleted FROM oplog_entity_state WHERE entity_type = 'widget' AND entity_id = 'w2'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let version: i64 = row.get("version");
        let data: Option<Value> = row.get("data");
        let deleted: bool = row.get("deleted");
        assert_eq!(version, 2);
        assert_eq!(data, None);
        assert!(deleted);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn stale_upsert_after_delete_does_not_resurrect_the_entity() {
        let Some((admin, pool, db_name)) = create_temp_db().await else {
            eprintln!(
                "skipping stale_upsert_after_delete_does_not_resurrect_the_entity: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        };

        // Two isolated nodes both touched the same entity. node-e deleted it
        // at version 5; node-f — reconnecting later — replays a write it made
        // at version 3, before it ever saw the delete. Because the delete's
        // version survives as a tombstone, the stale write must lose.
        let node_e = "node-e";
        let node_f = "node-f";
        let now = Utc::now();
        record_op(
            &pool,
            node_e,
            1,
            "widget",
            "w4",
            OpType::Delete,
            5,
            Value::Null,
            now,
        )
        .await
        .unwrap();
        record_op(
            &pool,
            node_f,
            1,
            "widget",
            "w4",
            OpType::Upsert,
            3,
            serde_json::json!({"name": "stale-write"}),
            now,
        )
        .await
        .unwrap();

        replay_node(&pool, node_e).await.unwrap();
        replay_node(&pool, node_f).await.unwrap();

        let row = sqlx::query(
            "SELECT version, data, deleted FROM oplog_entity_state WHERE entity_type = 'widget' AND entity_id = 'w4'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let version: i64 = row.get("version");
        let data: Option<Value> = row.get("data");
        let deleted: bool = row.get("deleted");
        assert_eq!(version, 5);
        assert_eq!(data, None);
        assert!(
            deleted,
            "stale upsert must not resurrect a tombstoned entity"
        );

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn newer_upsert_after_delete_supersedes_the_tombstone() {
        let Some((admin, pool, db_name)) = create_temp_db().await else {
            eprintln!(
                "skipping newer_upsert_after_delete_supersedes_the_tombstone: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        };

        let node = "node-g";
        let now = Utc::now();
        record_op(
            &pool,
            node,
            1,
            "widget",
            "w5",
            OpType::Delete,
            5,
            Value::Null,
            now,
        )
        .await
        .unwrap();
        record_op(
            &pool,
            node,
            2,
            "widget",
            "w5",
            OpType::Upsert,
            6,
            serde_json::json!({"name": "recreated"}),
            now,
        )
        .await
        .unwrap();

        replay_node(&pool, node).await.unwrap();

        let row = sqlx::query(
            "SELECT version, data, deleted FROM oplog_entity_state WHERE entity_type = 'widget' AND entity_id = 'w5'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let version: i64 = row.get("version");
        let data: Option<Value> = row.get("data");
        let deleted: bool = row.get("deleted");
        assert_eq!(version, 6);
        assert_eq!(data, Some(serde_json::json!({"name": "recreated"})));
        assert!(!deleted);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn partial_replay_stops_at_bad_entry_and_resumes_after_it_is_skipped() {
        let Some((admin, pool, db_name)) = create_temp_db().await else {
            eprintln!(
                "skipping partial_replay_stops_at_bad_entry_and_resumes_after_it_is_skipped: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        };

        let node = "node-d";
        let now = Utc::now();
        record_op(
            &pool,
            node,
            1,
            "widget",
            "w3",
            OpType::Upsert,
            1,
            serde_json::json!({"name": "ok"}),
            now,
        )
        .await
        .unwrap();
        // A union op whose payload isn't an array — malformed, should fail
        // to apply without poisoning the earlier successful entry.
        record_op(
            &pool,
            node,
            2,
            "tag_set",
            "project-2",
            OpType::Union,
            1,
            serde_json::json!({"not": "an array"}),
            now,
        )
        .await
        .unwrap();
        record_op(
            &pool,
            node,
            3,
            "widget",
            "w3",
            OpType::Upsert,
            2,
            serde_json::json!({"name": "never-applied-yet"}),
            now,
        )
        .await
        .unwrap();

        let summary = replay_node(&pool, node).await.unwrap();
        assert_eq!(summary.applied, 1);
        assert_eq!(summary.last_applied_sequence, Some(1));
        let failure = summary.failure.expect("expected a recorded failure");
        assert_eq!(failure.sequence, 2);

        // Entry 1 is durably committed even though replay stopped at entry 2.
        let row = sqlx::query(
            "SELECT version FROM oplog_entity_state WHERE entity_type = 'widget' AND entity_id = 'w3'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let version: i64 = row.get("version");
        assert_eq!(version, 1);

        // Entry 3 was never attempted — sequence order is preserved.
        let untouched = sqlx::query(
            "SELECT COUNT(*) AS n FROM oplog_entity_state WHERE entity_type = 'widget' AND entity_id = 'w3' AND version = 2",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let n: i64 = untouched.get("n");
        assert_eq!(n, 0);

        // The replay state and failure audit trail both reflect the stall.
        let state = sqlx::query(
            "SELECT last_applied_sequence, status FROM oplog_replay_state WHERE node_id = $1",
        )
        .bind(node)
        .fetch_one(&pool)
        .await
        .unwrap();
        let last_applied_sequence: i64 = state.get("last_applied_sequence");
        let status: String = state.get("status");
        assert_eq!(last_applied_sequence, 1);
        assert_eq!(status, "failed");

        let failures = sqlx::query(
            "SELECT COUNT(*) AS n FROM oplog_replay_failures WHERE node_id = $1 AND sequence = 2",
        )
        .bind(node)
        .fetch_one(&pool)
        .await
        .unwrap();
        let n: i64 = failures.get("n");
        assert_eq!(n, 1);

        // Re-running replay without fixing the bad entry stops at the same
        // place again rather than silently skipping ahead to entry 3.
        let retry = replay_node(&pool, node).await.unwrap();
        assert_eq!(retry.applied, 0);
        assert_eq!(retry.failure.map(|f| f.sequence), Some(2));

        drop_temp_db(admin, pool, &db_name).await;
    }
}
