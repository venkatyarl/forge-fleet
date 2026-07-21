//! Batched writes for normalized model-server metrics.
//!
//! The implementation avoids session state and disables SQLx's prepared
//! statement cache, so it is safe behind PgCat transaction pooling.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, QueryBuilder};

/// One row normalized to the supply-side `model_metrics` schema.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedMetricRow {
    pub recorded_at: DateTime<Utc>,
    pub node: String,
    pub port: i32,
    pub model: String,
    pub boot_id: Option<String>,
    pub batch_occupancy: Option<f64>,
    pub kv_cache_util: Option<f64>,
    pub queue_depth: Option<i64>,
    pub prompt_tokens_total: Option<i64>,
    pub output_tokens_total: Option<i64>,
}

/// Insert a batch of normalized metrics in one PgCat transaction-mode-safe
/// transaction.
///
/// When `stale_record` is `Some(true)`, each input identifies a missed sample
/// (for example, after a boot-ID reset). The identifying fields are retained,
/// but every measurement is written as `NULL`; stale values are never carried
/// into the new boot.
pub async fn write_metrics(
    pool: &PgPool,
    rows: &[NormalizedMetricRow],
    stale_record: Option<bool>,
) -> Result<u64, sqlx::Error> {
    if rows.is_empty() {
        return Ok(0);
    }

    let is_stale = stale_record.unwrap_or(false);
    let mut transaction = pool.begin().await?;
    let mut written = 0;

    // Eleven binds per row. Staying below Postgres's 65,535-parameter limit
    // also prevents a large collector flush from becoming one giant query.
    for chunk in rows.chunks(5_000) {
        let mut query = QueryBuilder::<Postgres>::new(
            "INSERT INTO model_metrics (recorded_at, node, port, model, boot_id, \
             batch_occupancy, kv_cache_util, queue_depth, prompt_tokens_total, \
             output_tokens_total, is_stale) ",
        );
        query.push_values(chunk, |mut values, row| {
            values
                .push_bind(row.recorded_at)
                .push_bind(&row.node)
                .push_bind(row.port)
                .push_bind(&row.model)
                .push_bind(&row.boot_id)
                .push_bind((!is_stale).then_some(row.batch_occupancy).flatten())
                .push_bind((!is_stale).then_some(row.kv_cache_util).flatten())
                .push_bind((!is_stale).then_some(row.queue_depth).flatten())
                .push_bind((!is_stale).then_some(row.prompt_tokens_total).flatten())
                .push_bind((!is_stale).then_some(row.output_tokens_total).flatten())
                .push_bind(is_stale);
        });

        written += query
            .build()
            .persistent(false)
            .execute(&mut *transaction)
            .await?
            .rows_affected();
    }

    transaction.commit().await?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_batch_does_not_need_a_database_connection() {
        let pool = PgPool::connect_lazy("postgres://unused:unused@127.0.0.1/unused").unwrap();
        assert_eq!(write_metrics(&pool, &[], Some(true)).await.unwrap(), 0);
    }
}
