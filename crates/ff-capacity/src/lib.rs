//! ff-capacity — cached fleet capacity snapshot.
//!
//! Exposes a cheap, read-only view of `v_fleet_capacity` for router hot paths.
//! The snapshot is refreshed every 30 seconds from Postgres and stored in an
//! [`ArcSwap`] so consumers never hit the database on the request path.
//!
//! This crate is intentionally shadow-only: no callers are wired yet.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};

mod error;
pub use error::{CapacityError, Result};

/// A single row from `v_fleet_capacity`.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct CapacityRow {
    pub kind: String,
    pub catalog_family: Option<String>,
    pub computer: Option<String>,
    pub provider: Option<String>,
    pub pool: Option<String>,
    pub free_slots: Option<i32>,
    pub bucket: Option<String>,
    pub label: Option<String>,
}

#[derive(Debug, Clone)]
struct CapacityData {
    rows: Vec<CapacityRow>,
    loaded_at: DateTime<Utc>,
}

/// Cached fleet capacity snapshot with a background 30-second refresh loop.
#[derive(Debug)]
pub struct CapacitySnapshot {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    data: Arc<ArcSwap<CapacityData>>,
    pool: PgPool,
    refresh: JoinHandle<()>,
}

impl CapacitySnapshot {
    /// Load the snapshot from `v_fleet_capacity` and start the 30-second refresh loop.
    pub async fn load(pool: &PgPool) -> Result<Self> {
        let data = Self::fetch(pool).await?;
        let data = Arc::new(ArcSwap::from_pointee(data));
        let inner = Arc::new(Inner {
            data: data.clone(),
            pool: pool.clone(),
            refresh: spawn_refresh(pool.clone(), data),
        });
        Ok(Self { inner })
    }

    /// Force an immediate refresh of the cached snapshot.
    pub async fn refresh(&self) -> Result<()> {
        let data = Self::fetch(&self.inner.pool).await?;
        self.inner.data.store(Arc::new(data));
        Ok(())
    }

    /// Return the `loaded_at` timestamp of the currently cached snapshot.
    pub fn loaded_at(&self) -> DateTime<Utc> {
        self.inner.data.load().loaded_at
    }

    /// Inference pool names for a given catalog family.
    pub fn inference_pools(&self, catalog_family: &str) -> Vec<String> {
        self.inner
            .data
            .load()
            .rows
            .iter()
            .filter(|r| r.kind == "inference_pool")
            .filter(|r| r.catalog_family.as_deref() == Some(catalog_family))
            .filter_map(|r| r.pool.clone())
            .collect()
    }

    /// Total free build slots for a given computer.
    pub fn free_build_slots(&self, computer: &str) -> i32 {
        self.inner
            .data
            .load()
            .rows
            .iter()
            .filter(|r| r.kind == "free_build_slot")
            .filter(|r| r.computer.as_deref() == Some(computer))
            .filter_map(|r| r.free_slots)
            .sum()
    }

    /// Cloud bucket name for a given provider, if known.
    pub fn cloud_bucket(&self, provider: &str) -> Option<String> {
        self.inner
            .data
            .load()
            .rows
            .iter()
            .find(|r| r.kind == "cloud_bucket" && r.provider.as_deref() == Some(provider))
            .and_then(|r| r.bucket.clone())
    }

    async fn fetch(pool: &PgPool) -> Result<CapacityData> {
        let rows = sqlx::query_as::<_, CapacityRow>(
            r#"
            SELECT
                kind,
                catalog_family,
                computer,
                provider,
                pool,
                free_slots,
                bucket,
                label
            FROM v_fleet_capacity
            ORDER BY kind, label
            "#,
        )
        .fetch_all(pool)
        .await?;

        Ok(CapacityData {
            rows,
            loaded_at: Utc::now(),
        })
    }
}

impl Drop for CapacitySnapshot {
    fn drop(&mut self) {
        self.inner.refresh.abort();
    }
}

fn spawn_refresh(pool: PgPool, data: Arc<ArcSwap<CapacityData>>) -> JoinHandle<()> {
    let mut interval = interval(Duration::from_secs(30));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    tokio::spawn(async move {
        loop {
            interval.tick().await;
            match CapacitySnapshot::fetch(&pool).await {
                Ok(snapshot) => data.store(Arc::new(snapshot)),
                Err(e) => tracing::warn!(error = %e, "capacity snapshot refresh failed"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::env;

    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    use super::*;

    /// Create a throwaway Postgres database for a test.
    /// Returns `None` (causing the test to skip) when no DB URL is configured.
    async fn setup_pool() -> Option<PgPool> {
        let Ok(base_url) =
            env::var("FORGEFLEET_POSTGRES_URL").or_else(|_| env::var("FORGEFLEET_DATABASE_URL"))
        else {
            return None;
        };
        let Some((prefix, _)) = base_url.rsplit_once('/') else {
            return None;
        };

        let db_name = format!("ff_capacity_{}", Uuid::new_v4().simple());
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&format!("{prefix}/postgres"))
            .await
            .ok()?;

        let create_sql = format!("CREATE DATABASE \"{db_name}\"");
        if sqlx::query(&create_sql).execute(&admin).await.is_err() {
            admin.close().await;
            return None;
        }

        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&format!("{prefix}/{db_name}"))
            .await
            .ok()?;

        // Step 1a owns the real `v_fleet_capacity` view. For unit tests we
        // stand in a simple table with the same column shape so fixture rows
        // can be inserted directly. Drop any view left by step 1a first so
        // tests stay hermetic on a freshly migrated throwaway database.
        let _ = sqlx::query("DROP VIEW IF EXISTS v_fleet_capacity")
            .execute(&pool)
            .await;
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS v_fleet_capacity (
                kind           TEXT NOT NULL,
                catalog_family TEXT,
                computer       TEXT,
                provider       TEXT,
                pool           TEXT,
                free_slots     INTEGER,
                bucket         TEXT,
                label          TEXT
            )
            "#,
        )
        .execute(&pool)
        .await
        .ok()?;

        Some(pool)
    }

    async fn insert_fixtures(pool: &PgPool) {
        sqlx::query(
            r#"
            INSERT INTO v_fleet_capacity
                (kind, catalog_family, computer, provider, pool, free_slots, bucket, label)
            VALUES
                ('inference_pool', 'qwen',  NULL,      NULL,  'coder', NULL, NULL,            'qwen coder pool'),
                ('inference_pool', 'qwen',  NULL,      NULL,  'chat',  NULL, NULL,            'qwen chat pool'),
                ('inference_pool', 'gemma', NULL,      NULL,  'judge', NULL, NULL,            'gemma judge pool'),
                ('free_build_slot', NULL,   'builder-1', NULL, NULL,   4,    NULL,            'builder-1 slots'),
                ('free_build_slot', NULL,   'builder-1', NULL, NULL,   2,    NULL,            'builder-1 extra'),
                ('free_build_slot', NULL,   'builder-2', NULL, NULL,   1,    NULL,            'builder-2 slots'),
                ('cloud_bucket',    NULL,   NULL,      'aws', NULL,   NULL, 'ff-aws-bucket', 'aws bucket'),
                ('cloud_bucket',    NULL,   NULL,      'gcp', NULL,   NULL, 'ff-gcp-bucket', 'gcp bucket')
            "#,
        )
        .execute(pool)
        .await
        .expect("insert fixtures");
    }

    #[tokio::test]
    async fn inference_pools_filters_by_family() {
        let Some(pool) = setup_pool().await else {
            return;
        };
        insert_fixtures(&pool).await;

        let snap = CapacitySnapshot::load(&pool).await.expect("load snapshot");

        let mut qwen = snap.inference_pools("qwen");
        qwen.sort();
        assert_eq!(qwen, vec!["chat".to_string(), "coder".to_string()]);

        assert_eq!(snap.inference_pools("gemma"), vec!["judge".to_string()]);
        assert!(snap.inference_pools("unknown").is_empty());
    }

    #[tokio::test]
    async fn free_build_slots_sums_for_computer() {
        let Some(pool) = setup_pool().await else {
            return;
        };
        insert_fixtures(&pool).await;

        let snap = CapacitySnapshot::load(&pool).await.expect("load snapshot");

        assert_eq!(snap.free_build_slots("builder-1"), 6);
        assert_eq!(snap.free_build_slots("builder-2"), 1);
        assert_eq!(snap.free_build_slots("missing"), 0);
    }

    #[tokio::test]
    async fn cloud_bucket_returns_provider_bucket() {
        let Some(pool) = setup_pool().await else {
            return;
        };
        insert_fixtures(&pool).await;

        let snap = CapacitySnapshot::load(&pool).await.expect("load snapshot");

        assert_eq!(snap.cloud_bucket("aws"), Some("ff-aws-bucket".to_string()));
        assert_eq!(snap.cloud_bucket("gcp"), Some("ff-gcp-bucket".to_string()));
        assert_eq!(snap.cloud_bucket("azure"), None);
    }

    #[tokio::test]
    async fn refresh_updates_snapshot() {
        let Some(pool) = setup_pool().await else {
            return;
        };
        insert_fixtures(&pool).await;

        let snap = CapacitySnapshot::load(&pool).await.expect("load snapshot");
        assert_eq!(snap.inference_pools("qwen").len(), 2);

        sqlx::query(
            "INSERT INTO v_fleet_capacity (kind, catalog_family, pool) VALUES ('inference_pool', 'qwen', 'reasoning')",
        )
        .execute(&pool)
        .await
        .expect("insert extra row");

        snap.refresh().await.expect("refresh");

        let mut qwen = snap.inference_pools("qwen");
        qwen.sort();
        assert_eq!(
            qwen,
            vec![
                "chat".to_string(),
                "coder".to_string(),
                "reasoning".to_string()
            ]
        );
    }
}
