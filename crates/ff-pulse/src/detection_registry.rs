//! DetectionRegistry — DB-driven cache of `software_registry.detection` rules.
//!
//! The SoftwareCollector used to hardcode 17 detection blocks (one per
//! known software_id) directly in Rust. Adding a new tool meant a code
//! change. V66 moves that data into `software_registry.detection JSONB`
//! and this module owns the runtime cache: load all rules once at daemon
//! startup, refresh every 5 minutes, expose a sync read for the
//! collector's hot path.
//!
//! Mirrors the pattern used by `pulse_hmac::KeyCache` — one `OnceCell`
//! holds an `RwLock<Vec<DetectionRule>>`. Readers take the read lock and
//! clone what they need; writes happen only from the refresher task.

use std::sync::{OnceLock, RwLock};
use std::time::Duration;

use serde::Deserialize;
use sqlx::PgPool;
use tracing::{debug, info, warn};

/// A single detection rule loaded from `software_registry.detection`.
/// `software_id` and `display_name` come from sibling columns of the
/// same row, joined in at load time.
#[derive(Debug, Clone, Deserialize)]
pub struct DetectionRule {
    pub software_id: String,
    pub display_name: String,
    /// Inner detection JSONB. Shape varies per `method`; see
    /// [`DetectionMethod`] in `software_collector.rs`.
    pub detection: serde_json::Value,
}

/// Global registry cache. Populated by `spawn_refresher`; readable via
/// `current_rules()`.
static REGISTRY: OnceLock<RwLock<Vec<DetectionRule>>> = OnceLock::new();

fn cell() -> &'static RwLock<Vec<DetectionRule>> {
    REGISTRY.get_or_init(|| RwLock::new(Vec::new()))
}

/// Read a snapshot of the current rules. Returns empty Vec if the cache
/// hasn't been populated yet (e.g. daemon just started, or running in a
/// test without DB plumbing). Callers must handle the empty case
/// gracefully — fall back to the no-op path.
pub fn current_rules() -> Vec<DetectionRule> {
    cell().read().map(|g| g.clone()).unwrap_or_default()
}

/// Load all rows from `software_registry` where `detection IS NOT NULL`
/// and replace the cache atomically. Per-row parse failures are skipped
/// with a warning — the cache stays consistent.
pub async fn refresh_from_pool(pool: &PgPool) -> Result<usize, sqlx::Error> {
    let rows: Vec<(String, String, serde_json::Value)> = sqlx::query_as(
        r#"
        SELECT id, display_name, detection
          FROM software_registry
         WHERE detection IS NOT NULL
        "#,
    )
    .fetch_all(pool)
    .await?;

    let n = rows.len();
    let rules: Vec<DetectionRule> = rows
        .into_iter()
        .map(|(software_id, display_name, detection)| DetectionRule {
            software_id,
            display_name,
            detection,
        })
        .collect();

    if let Ok(mut g) = cell().write() {
        *g = rules;
    } else {
        warn!("detection_registry: cache poisoned, skipping refresh");
    }
    debug!(count = n, "detection_registry: refreshed");
    Ok(n)
}

/// Spawn a background task that loads the registry once immediately, then
/// refreshes every 5 minutes. Idempotent — calling twice spawns two
/// loops, which is wasteful but not incorrect (writes are atomic).
pub fn spawn_refresher(pool: PgPool) {
    tokio::spawn(async move {
        // Initial load. If it fails, log and keep retrying on the tick.
        match refresh_from_pool(&pool).await {
            Ok(n) => info!(count = n, "detection_registry: initial load"),
            Err(e) => warn!(error = %e, "detection_registry: initial load failed"),
        }
        let mut tick = tokio::time::interval(Duration::from_secs(300));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await; // absorb the immediate-fire
        loop {
            tick.tick().await;
            match refresh_from_pool(&pool).await {
                Ok(n) => debug!(count = n, "detection_registry: refresh"),
                Err(e) => warn!(error = %e, "detection_registry: refresh failed"),
            }
        }
    });
}
