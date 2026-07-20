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
    pub port: Option<i32>,
    pub health: Option<String>,
    pub free_slots: Option<i32>,
    pub bucket: Option<String>,
    pub label: Option<String>,
}

/// A healthy inference deployment advertised by the capacity registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceDeployment {
    pub catalog_id: String,
    pub catalog_family: Option<String>,
    pub computer: String,
    pub port: i32,
}

/// Cached inputs used by the cloud-backend router.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct BackendCapacity {
    pub computer_id: uuid::Uuid,
    pub backend: String,
    pub installed: bool,
    pub authenticated: bool,
    pub last_checked_at: DateTime<Utc>,
    pub remaining_pct: Option<f64>,
    pub breaker_state: String,
}

#[derive(Debug, Clone)]
struct CapacityData {
    rows: Vec<CapacityRow>,
    backends: Vec<BackendCapacity>,
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

    /// Healthy inference deployments in the current registry snapshot.
    ///
    /// Consumers may continue using this value after a refresh failure: the
    /// refresh loop only replaces the snapshot after a successful fetch.
    pub fn inference_deployments(&self) -> Vec<InferenceDeployment> {
        self.inner
            .data
            .load()
            .rows
            .iter()
            .filter(|r| r.kind == "inference_pool")
            .filter(|r| r.health.as_deref() == Some("healthy"))
            .filter_map(|r| {
                Some(InferenceDeployment {
                    catalog_id: r.pool.clone()?,
                    catalog_family: r.catalog_family.clone(),
                    computer: r.label.clone()?,
                    port: r.port?,
                })
            })
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

    /// Router inputs for one computer, read only from the in-process snapshot.
    ///
    /// The caller owns scoring so the policy remains shared with the legacy
    /// database picker during the gated migration.
    pub fn backend_capacity(&self, computer_id: uuid::Uuid) -> Vec<BackendCapacity> {
        self.inner
            .data
            .load()
            .backends
            .iter()
            .filter(|row| row.computer_id == computer_id)
            .cloned()
            .collect()
    }

    async fn fetch(pool: &PgPool) -> Result<CapacityData> {
        // Normalize the live registry view into the stable read-API shape.
        // The view deliberately uses one compact inference/build/cloud schema;
        // callers should not need to know that storage representation.
        let rows = sqlx::query_as::<_, CapacityRow>(
            r#"
            SELECT
                CASE v.kind
                    WHEN 'inference' THEN 'inference_pool'
                    WHEN 'build' THEN 'free_build_slot'
                    WHEN 'cloud' THEN 'cloud_bucket'
                    ELSE v.kind
                END AS kind,
                c.family AS catalog_family,
                CASE WHEN v.kind = 'build' THEN v.worker END AS computer,
                CASE WHEN v.kind = 'cloud' THEN v.worker END AS provider,
                CASE WHEN v.kind = 'inference' THEN v.catalog_id END AS pool,
                CASE WHEN v.kind = 'inference' THEN v.port END AS port,
                v.health,
                CASE WHEN v.kind = 'build' THEN v.parallel_slots::int END AS free_slots,
                CASE WHEN v.kind = 'cloud' THEN v.worker END AS bucket,
                v.worker AS label
            FROM v_fleet_capacity v
            LEFT JOIN fleet_model_catalog c ON c.id = v.catalog_id
            ORDER BY v.kind, v.worker
            "#,
        )
        .fetch_all(pool)
        .await?;

        let backends = sqlx::query_as::<_, BackendCapacity>(
            r#"
            SELECT cb.computer_id,
                   cb.backend,
                   cb.installed,
                   cb.authenticated,
                   cb.last_checked_at,
                   u.remaining_pct,
                   COALESCE(h.breaker_state, 'closed') AS breaker_state
              FROM computer_backends cb
              LEFT JOIN fleet_backend_health h
                ON h.computer_id = cb.computer_id AND h.provider = cb.backend
              LEFT JOIN LATERAL (
                   SELECT remaining_pct
                     FROM fleet_provider_usage fu
                    WHERE fu.computer_id = cb.computer_id AND fu.provider = cb.backend
                    ORDER BY fu.sampled_at DESC
                    LIMIT 1
              ) u ON true
            "#,
        )
        .fetch_all(pool)
        .await?;

        Ok(CapacityData {
            rows,
            backends,
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
            CREATE TABLE fleet_model_catalog (
                id TEXT PRIMARY KEY,
                family TEXT
            );
            CREATE TABLE v_fleet_capacity (
                kind TEXT NOT NULL,
                catalog_id TEXT,
                worker TEXT,
                port INTEGER,
                parallel_slots BIGINT,
                health TEXT,
                max_concurrent INTEGER,
                tokens_per_min BIGINT,
                spent_today NUMERIC
            );
            CREATE TABLE computer_backends (
                computer_id UUID NOT NULL,
                backend TEXT NOT NULL,
                installed BOOLEAN NOT NULL,
                authenticated BOOLEAN NOT NULL,
                last_checked_at TIMESTAMPTZ NOT NULL
            );
            CREATE TABLE fleet_backend_health (
                computer_id UUID NOT NULL,
                provider TEXT NOT NULL,
                breaker_state TEXT NOT NULL
            );
            CREATE TABLE fleet_provider_usage (
                computer_id UUID NOT NULL,
                provider TEXT NOT NULL,
                remaining_pct DOUBLE PRECISION,
                sampled_at TIMESTAMPTZ NOT NULL
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
            INSERT INTO fleet_model_catalog (id, family) VALUES
                ('coder', 'qwen'), ('chat', 'qwen'), ('judge', 'gemma');
            INSERT INTO v_fleet_capacity
                (kind, catalog_id, worker, port, parallel_slots, health)
            VALUES
                ('inference', 'coder', 'node-1', 8080, 1, 'healthy'),
                ('inference', 'chat',  'node-2', 8081, 1, 'healthy'),
                ('inference', 'judge', 'node-3', 8082, 1, 'healthy'),
                ('build', NULL, 'builder-1', NULL, 4, 'online'),
                ('build', NULL, 'builder-1', NULL, 2, 'online'),
                ('build', NULL, 'builder-2', NULL, 1, 'online'),
                ('cloud', NULL, 'aws', NULL, 3, 'healthy'),
                ('cloud', NULL, 'gcp', NULL, 3, 'healthy')
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

        assert_eq!(
            snap.inference_deployments()[0],
            InferenceDeployment {
                catalog_id: "coder".to_string(),
                catalog_family: Some("qwen".to_string()),
                computer: "node-1".to_string(),
                port: 8080,
            }
        );
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

        assert_eq!(snap.cloud_bucket("aws"), Some("aws".to_string()));
        assert_eq!(snap.cloud_bucket("gcp"), Some("gcp".to_string()));
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

        sqlx::query("INSERT INTO fleet_model_catalog (id, family) VALUES ('reasoning', 'qwen')")
            .execute(&pool)
            .await
            .expect("insert catalog row");
        sqlx::query(
            "INSERT INTO v_fleet_capacity (kind, catalog_id, worker, parallel_slots, health) VALUES ('inference', 'reasoning', 'node-4', 1, 'healthy')",
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
