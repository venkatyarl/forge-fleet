//! Partition maintenance for the tiered fleet metrics tables (V177).
//!
//! The V177 migration creates only the partitioned PARENTS
//! (`fleet_metrics_raw` / `fleet_metrics_1min` / `fleet_metrics_hourly`); a
//! parent with no children rejects inserts, so this module owns the child
//! lifecycle:
//!
//! - [`pg_ensure_metrics_partitions`] pre-creates dated children (daily for
//!   raw + 1min, monthly for hourly) covering a small window around "now".
//! - [`pg_drop_expired_metrics_partitions`] drops raw children older than
//!   7 days and 1min children older than 30 days. Hourly children are kept
//!   forever and never dropped.
//!
//! Both are idempotent and safe to run on every tick — forgefleetd drives
//! them leader-gated from `ff_agent::metrics_partition_maintenance`.
//!
//! Child names embed their bounds (`fleet_metrics_raw_p20260719` covers
//! `[2026-07-19, 2026-07-20)`; `fleet_metrics_hourly_p202607` covers July
//! 2026), so the drop pass can decide expiry from `pg_inherits` names alone.
//! Children whose names don't parse are left untouched.

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use sqlx::PgPool;

use crate::error::Result;

/// Raw samples are kept 7 days.
pub const RAW_RETENTION_DAYS: i64 = 7;
/// 1-minute rollups are kept 30 days.
pub const ROLLUP_1MIN_RETENTION_DAYS: i64 = 30;
/// Daily partitions are pre-created this many days ahead so a missed tick
/// (leader down over a weekend) never leaves writers without a partition.
const DAILY_LOOKAHEAD_DAYS: i64 = 3;

const RAW_PARENT: &str = "fleet_metrics_raw";
const ROLLUP_1MIN_PARENT: &str = "fleet_metrics_1min";
const HOURLY_PARENT: &str = "fleet_metrics_hourly";

/// Name of the daily child covering `[day, day+1)`.
fn daily_partition_name(parent: &str, day: NaiveDate) -> String {
    format!("{parent}_p{}", day.format("%Y%m%d"))
}

/// Name of the monthly child covering `day`'s calendar month.
fn monthly_partition_name(parent: &str, day: NaiveDate) -> String {
    format!("{parent}_p{}", day.format("%Y%m"))
}

/// Parse the covered day back out of a daily child name (`{parent}_pYYYYMMDD`).
fn parse_daily_partition(name: &str, parent: &str) -> Option<NaiveDate> {
    let suffix = name.strip_prefix(parent)?.strip_prefix("_p")?;
    NaiveDate::parse_from_str(suffix, "%Y%m%d").ok()
}

/// First day of the month containing `day`.
fn month_start(day: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(day.year(), day.month(), 1).expect("first of month is always valid")
}

/// First day of the month after the one containing `day`.
fn next_month_start(day: NaiveDate) -> NaiveDate {
    if day.month() == 12 {
        NaiveDate::from_ymd_opt(day.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(day.year(), day.month() + 1, 1)
    }
    .expect("first of month is always valid")
}

/// Children of `parent` (name-parseable daily ones only) whose whole range is
/// older than `retention_days` before `today`. A child covering `[d, d+1)` is
/// expired when `d + 1 <= today - retention_days`.
fn expired_daily_partitions(
    children: &[String],
    parent: &str,
    today: NaiveDate,
    retention_days: i64,
) -> Vec<String> {
    let cutoff = today - Duration::days(retention_days);
    children
        .iter()
        .filter(|name| {
            parse_daily_partition(name, parent).is_some_and(|day| day + Duration::days(1) <= cutoff)
        })
        .cloned()
        .collect()
}

/// `CREATE TABLE IF NOT EXISTS` one range child. Identifiers and bounds are
/// derived from compile-time parent names and formatted dates — nothing
/// user-controlled reaches the interpolated DDL.
async fn create_range_partition(
    pool: &PgPool,
    parent: &str,
    child: &str,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<()> {
    sqlx::raw_sql(&format!(
        "CREATE TABLE IF NOT EXISTS \"{child}\" PARTITION OF \"{parent}\"
             FOR VALUES FROM ('{from}T00:00:00Z') TO ('{to}T00:00:00Z')"
    ))
    .execute(pool)
    .await?;
    Ok(())
}

/// List existing children of `parent` in the public schema.
async fn list_partitions(pool: &PgPool, parent: &str) -> Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT c.relname
           FROM pg_inherits i
           JOIN pg_class c ON c.oid = i.inhrelid
           JOIN pg_class p ON p.oid = i.inhparent
           JOIN pg_namespace n ON n.oid = p.relnamespace
          WHERE p.relname = $1 AND n.nspname = 'public'",
    )
    .bind(parent)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(name,)| name).collect())
}

/// Pre-create the child partitions writers need around `now`: daily children
/// for raw + 1min covering yesterday through `DAILY_LOOKAHEAD_DAYS` ahead, and
/// monthly children for hourly covering this month + next. Idempotent.
pub async fn pg_ensure_metrics_partitions(pool: &PgPool, now: DateTime<Utc>) -> Result<()> {
    let today = now.date_naive();
    for offset in -1..=DAILY_LOOKAHEAD_DAYS {
        let day = today + Duration::days(offset);
        for parent in [RAW_PARENT, ROLLUP_1MIN_PARENT] {
            let child = daily_partition_name(parent, day);
            create_range_partition(pool, parent, &child, day, day + Duration::days(1)).await?;
        }
    }
    for month in [month_start(today), next_month_start(today)] {
        let child = monthly_partition_name(HOURLY_PARENT, month);
        create_range_partition(pool, HOURLY_PARENT, &child, month, next_month_start(month)).await?;
    }
    Ok(())
}

/// Drop expired raw (7d) and 1min-rollup (30d) child partitions. Hourly
/// children are retained forever. Returns the dropped child names.
pub async fn pg_drop_expired_metrics_partitions(
    pool: &PgPool,
    now: DateTime<Utc>,
) -> Result<Vec<String>> {
    let today = now.date_naive();
    let mut dropped = Vec::new();
    for (parent, retention_days) in [
        (RAW_PARENT, RAW_RETENTION_DAYS),
        (ROLLUP_1MIN_PARENT, ROLLUP_1MIN_RETENTION_DAYS),
    ] {
        let children = list_partitions(pool, parent).await?;
        for child in expired_daily_partitions(&children, parent, today, retention_days) {
            sqlx::raw_sql(&format!("DROP TABLE IF EXISTS \"{child}\""))
                .execute(pool)
                .await?;
            dropped.push(child);
        }
    }
    Ok(dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn partition_names_round_trip() {
        let day = d("2026-07-19");
        let name = daily_partition_name(RAW_PARENT, day);
        assert_eq!(name, "fleet_metrics_raw_p20260719");
        assert_eq!(parse_daily_partition(&name, RAW_PARENT), Some(day));
        assert_eq!(
            monthly_partition_name(HOURLY_PARENT, day),
            "fleet_metrics_hourly_p202607"
        );
        // Foreign / malformed names never parse (and thus never get dropped).
        assert_eq!(
            parse_daily_partition("fleet_metrics_raw_default", RAW_PARENT),
            None
        );
        assert_eq!(
            parse_daily_partition("fleet_metrics_1min_p20260719", RAW_PARENT),
            None
        );
    }

    #[test]
    fn month_boundaries_handle_december() {
        assert_eq!(month_start(d("2026-12-31")), d("2026-12-01"));
        assert_eq!(next_month_start(d("2026-12-31")), d("2027-01-01"));
        assert_eq!(next_month_start(d("2026-07-19")), d("2026-08-01"));
    }

    #[test]
    fn expiry_keeps_partitions_touching_the_window() {
        let today = d("2026-07-19");
        let children = vec![
            "fleet_metrics_raw_p20260710".to_string(), // ends 07-11 < cutoff 07-12 → drop
            "fleet_metrics_raw_p20260711".to_string(), // ends exactly at cutoff → drop
            "fleet_metrics_raw_p20260712".to_string(), // inside the 7d window → keep
            "fleet_metrics_raw_p20260719".to_string(), // today → keep
            "fleet_metrics_raw_default".to_string(),   // unparseable → keep
        ];
        assert_eq!(
            expired_daily_partitions(&children, RAW_PARENT, today, RAW_RETENTION_DAYS),
            vec![
                "fleet_metrics_raw_p20260710".to_string(),
                "fleet_metrics_raw_p20260711".to_string(),
            ]
        );
    }

    /// End-to-end against a throwaway database: migrate, ensure partitions,
    /// insert into every tier, back-date a raw child, drop expired.
    ///
    /// Needs Postgres — early-returns (never panics) when neither
    /// FORGEFLEET_POSTGRES_URL nor FORGEFLEET_DATABASE_URL is set, so CI's
    /// DB-less `cargo test --lib` skips it.
    #[tokio::test]
    async fn partition_lifecycle_against_live_postgres() {
        let Ok(base_url) = std::env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
        else {
            return;
        };
        let Some((prefix, _)) = base_url.rsplit_once('/') else {
            return;
        };
        let db_name = format!("ff_metrics_part_{}", uuid::Uuid::new_v4().simple());
        let Ok(admin) = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&format!("{prefix}/postgres"))
            .await
        else {
            return;
        };
        // The bootstrap baseline needs pgcrypto/pgvector/amcheck; skip when
        // the server can't provide them.
        let extensions_ready: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'pgcrypto')
                AND EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'vector')
                AND EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'amcheck')",
        )
        .fetch_one(&admin)
        .await
        .unwrap_or(false);
        if !extensions_ready {
            admin.close().await;
            return;
        }
        if sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
            .execute(&admin)
            .await
            .is_err()
        {
            admin.close().await;
            return;
        }
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&format!("{prefix}/{db_name}"))
            .await
            .expect("connect temp db");

        crate::migrations::run_postgres_migrations(&pool)
            .await
            .expect("migrations apply");

        let now = Utc::now();
        pg_ensure_metrics_partitions(&pool, now)
            .await
            .expect("ensure partitions");
        // Second run must be a no-op, not an error.
        pg_ensure_metrics_partitions(&pool, now)
            .await
            .expect("ensure partitions is idempotent");

        // Every tier accepts a row for "now" once children exist.
        sqlx::query(
            "INSERT INTO fleet_metrics_raw (worker_name, metric, value) VALUES ('t', 'cpu', 1.0)",
        )
        .execute(&pool)
        .await
        .expect("raw insert routes to a partition");
        for table in ["fleet_metrics_1min", "fleet_metrics_hourly"] {
            sqlx::query(&format!(
                "INSERT INTO {table}
                     (worker_name, metric, bucket_start, sample_count,
                      value_min, value_max, value_avg, value_last)
                 VALUES ('t', 'cpu', date_trunc('hour', NOW()), 1, 1, 1, 1, 1)"
            ))
            .execute(&pool)
            .await
            .expect("rollup insert routes to a partition");
        }

        // A raw child fully past the 7d window gets dropped; live ones stay.
        let old_day = now.date_naive() - Duration::days(RAW_RETENTION_DAYS + 2);
        let old_child = daily_partition_name(RAW_PARENT, old_day);
        create_range_partition(
            &pool,
            RAW_PARENT,
            &old_child,
            old_day,
            old_day + Duration::days(1),
        )
        .await
        .expect("create back-dated partition");
        let dropped = pg_drop_expired_metrics_partitions(&pool, now)
            .await
            .expect("drop expired");
        assert_eq!(dropped, vec![old_child]);
        let remaining = list_partitions(&pool, RAW_PARENT).await.expect("list");
        assert!(remaining.contains(&daily_partition_name(RAW_PARENT, now.date_naive())));

        pool.close().await;
        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db_name}\""))
            .execute(&admin)
            .await
            .ok();
        admin.close().await;
    }
}
