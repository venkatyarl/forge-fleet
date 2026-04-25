//! Pulse v2 reader — observes fleet state from Redis.
//!
//! The publisher writes `pulse:computer:{name}` keys holding the full
//! [`PulseBeatV2`] JSON with a 45s TTL. This reader provides lookup,
//! scan, sdown/odown detection, and simple LLM-server selection on top
//! of that key-space.

use std::collections::HashMap;

use futures::StreamExt;
use redis::AsyncCommands;
use thiserror::Error;

use crate::beat_v2::{LlmServer, PulseBeatV2};

const KEY_PREFIX: &str = "pulse:computer:";

/// Errors the reader can return. Scoped to this module so it does not
/// collide with [`crate::error::PulseError`].
#[derive(Debug, Error)]
pub enum PulseError {
    #[error("redis: {0}")]
    Redis(#[from] redis::RedisError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Redis-backed reader for Pulse v2 beats.
///
/// Holds a single `ConnectionManager` (cheap to clone, auto-reconnecting)
/// built on first use and reused for every read. Prior to 2026-04-23 the
/// reader created a brand-new `MultiplexedConnection` per call — ~500
/// new TCP handshakes/sec from forgefleetd under normal operation, which
/// pushed macOS past 16K TIME_WAIT source ports and caused
/// `EADDRNOTAVAIL` for every other outbound connect() on Taylor.
pub struct PulseReader {
    client: redis::Client,
    conn: tokio::sync::OnceCell<redis::aio::ConnectionManager>,
}

impl PulseReader {
    /// Build a new reader pointed at `redis_url`.
    pub fn new(redis_url: &str) -> Result<Self, PulseError> {
        let client = redis::Client::open(redis_url)?;
        Ok(Self {
            client,
            conn: tokio::sync::OnceCell::new(),
        })
    }

    async fn conn(&self) -> Result<redis::aio::ConnectionManager, PulseError> {
        if let Some(c) = self.conn.get() {
            return Ok(c.clone());
        }
        let client = self.client.clone();
        let mgr = self
            .conn
            .get_or_try_init(|| async move { redis::aio::ConnectionManager::new(client).await })
            .await?;
        Ok(mgr.clone())
    }

    fn key_for(name: &str) -> String {
        format!("{KEY_PREFIX}{name}")
    }

    fn strip_prefix(key: &str) -> Option<&str> {
        key.strip_prefix(KEY_PREFIX)
    }

    /// Fetch the latest beat for a single computer. Returns `None` if the
    /// key is missing (i.e. TTL expired or never published) OR if the
    /// beat fails HMAC verification.
    pub async fn latest_beat(
        &self,
        computer_name: &str,
    ) -> Result<Option<PulseBeatV2>, PulseError> {
        let mut conn = self.conn().await?;
        let key = Self::key_for(computer_name);
        let raw: Option<String> = conn.get(&key).await?;
        match raw {
            Some(s) => {
                if !Self::beat_signature_ok(computer_name, &s).await {
                    return Ok(None);
                }
                Ok(Some(serde_json::from_str(&s)?))
            }
            None => Ok(None),
        }
    }

    /// SCAN all `pulse:computer:*` keys and return every parseable beat.
    /// Keys that vanish between SCAN and GET, fail HMAC verification, or
    /// do not parse are silently skipped.
    pub async fn all_beats(&self) -> Result<Vec<PulseBeatV2>, PulseError> {
        let keys = self.scan_keys().await?;
        if keys.is_empty() {
            return Ok(Vec::new());
        }

        let mut conn = self.conn().await?;
        let mut beats = Vec::with_capacity(keys.len());
        for key in &keys {
            let raw: Option<String> = conn.get(key).await?;
            if let Some(s) = raw {
                let name = Self::strip_prefix(key).unwrap_or("unknown");
                if !Self::beat_signature_ok(name, &s).await {
                    continue;
                }
                match serde_json::from_str::<PulseBeatV2>(&s) {
                    Ok(b) => beats.push(b),
                    Err(_) => continue, // skip malformed entries
                }
            }
        }
        Ok(beats)
    }

    /// Verify the HMAC on a beat. Returns true iff we should accept it.
    async fn beat_signature_ok(computer_name: &str, raw: &str) -> bool {
        let key = crate::pulse_hmac::KeyCache::global().get().await;
        let outcome = crate::pulse_hmac::verify_json(key.as_deref(), raw);
        crate::pulse_hmac::log_verify(computer_name, outcome)
    }

    /// Returns true if the computer's beat key is missing/expired.
    /// The 45s Redis TTL already enforces "stale ⇒ key gone", so this is
    /// equivalent to "no beat present".
    pub async fn is_sdown(&self, computer_name: &str) -> Result<bool, PulseError> {
        let mut conn = self.conn().await?;
        let key = Self::key_for(computer_name);
        let exists: bool = conn.exists(&key).await?;
        Ok(!exists)
    }

    /// Objectively-down: `target` is sdown AND a majority of currently-alive
    /// peers ALSO report the target as `sdown` in their `peers_seen`.
    pub async fn is_odown(&self, target: &str) -> Result<bool, PulseError> {
        if !self.is_sdown(target).await? {
            return Ok(false);
        }

        let beats = self.all_beats().await?;
        // Alive peers are any peer whose own beat is present — TTL has
        // already filtered out dead ones. We exclude the target itself.
        let alive: Vec<&PulseBeatV2> = beats
            .iter()
            .filter(|b| b.computer_name != target && !b.going_offline)
            .collect();

        if alive.is_empty() {
            // No witnesses. Can't declare objectively-down.
            return Ok(false);
        }

        let concurring = alive
            .iter()
            .filter(|b| {
                b.peers_seen
                    .iter()
                    .any(|p| p.name == target && p.status == "sdown")
            })
            .count();

        // Strict majority of alive witnesses.
        Ok(concurring * 2 > alive.len())
    }

    /// Number of live `pulse:computer:*` keys.
    pub async fn count_members(&self) -> Result<usize, PulseError> {
        Ok(self.scan_keys().await?.len())
    }

    /// Returns `(name, is_healthy, going_offline)` triples for every beat
    /// currently in Redis. Used by election code to see who is eligible.
    pub async fn computer_health_for_election(
        &self,
    ) -> Result<Vec<(String, bool, bool)>, PulseError> {
        let beats = self.all_beats().await?;
        Ok(beats
            .into_iter()
            .map(|b| {
                let healthy = !b.going_offline;
                (b.computer_name, healthy, b.going_offline)
            })
            .collect())
    }

    /// Pick the best `(computer_name, LlmServer)` serving `model_id`.
    ///
    /// Filters: `status == "active"`, `is_healthy == true`, `model.id == model_id`.
    /// Sorts by `queue_depth` ASC, then `tokens_per_sec_last_min` DESC.
    pub async fn pick_llm_server_for(
        &self,
        model_id: &str,
    ) -> Result<Option<(String, LlmServer)>, PulseError> {
        let beats = self.all_beats().await?;
        let mut candidates: Vec<(String, LlmServer)> = Vec::new();
        for b in beats {
            if b.going_offline {
                continue;
            }
            for s in &b.llm_servers {
                if s.status == "active" && s.is_healthy && s.model.id == model_id {
                    candidates.push((b.computer_name.clone(), s.clone()));
                }
            }
        }

        candidates.sort_by(|(_, a), (_, b)| {
            a.queue_depth.cmp(&b.queue_depth).then_with(|| {
                b.tokens_per_sec_last_min
                    .partial_cmp(&a.tokens_per_sec_last_min)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });

        Ok(candidates.into_iter().next())
    }

    /// Pick the best `(computer_name, LlmServer)` for `requested`, expanding
    /// pool aliases via `fleet_task_coverage.alias` (schema V27).
    ///
    /// Flow:
    /// 1. If `requested` matches a row's `alias`, expand to that row's
    ///    `preferred_model_ids` and treat every id as a candidate.
    /// 2. Otherwise fall back to the single-model path (`requested` itself).
    /// 3. Across every beat, collect active+healthy servers whose
    ///    `model.id` equals any candidate, then rank by `queue_depth` ASC,
    ///    `tokens_per_sec_last_min` DESC.
    ///
    /// Keeps the original single-model [`pick_llm_server_for`] intact for
    /// callers that don't have a Postgres pool handy.
    pub async fn pick_llm_server_for_with_pools(
        &self,
        pg: &sqlx::PgPool,
        requested: &str,
    ) -> Result<Option<(String, LlmServer)>, PulseError> {
        // 1. Alias lookup. Swallow DB errors so an unreachable Postgres
        //    still lets exact-id routing work.
        let pool_members: Option<Vec<String>> = sqlx::query_scalar::<_, String>(
            "SELECT preferred_model_ids::text
               FROM fleet_task_coverage
              WHERE alias = $1
              LIMIT 1",
        )
        .bind(requested)
        .fetch_optional(pg)
        .await
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok());

        let candidate_ids: Vec<String> = match pool_members {
            Some(members) if !members.is_empty() => members,
            _ => vec![requested.to_string()],
        };

        // 2. Collect matches across every beat.
        let beats = self.all_beats().await?;
        let mut candidates: Vec<(String, LlmServer)> = Vec::new();
        for b in beats {
            if b.going_offline {
                continue;
            }
            for s in &b.llm_servers {
                if s.status == "active"
                    && s.is_healthy
                    && candidate_ids.iter().any(|id| id == &s.model.id)
                {
                    candidates.push((b.computer_name.clone(), s.clone()));
                }
            }
        }

        // 3. Rank lowest load first, fastest tokens/sec on ties.
        candidates.sort_by(|(_, a), (_, b)| {
            a.queue_depth.cmp(&b.queue_depth).then_with(|| {
                b.tokens_per_sec_last_min
                    .partial_cmp(&a.tokens_per_sec_last_min)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });

        Ok(candidates.into_iter().next())
    }

    /// Enumerate every active+healthy LLM server across the fleet.
    pub async fn list_llm_servers(&self) -> Result<Vec<(String, LlmServer)>, PulseError> {
        let beats = self.all_beats().await?;
        let mut out = Vec::new();
        for b in beats {
            if b.going_offline {
                continue;
            }
            for s in &b.llm_servers {
                if s.status == "active" && s.is_healthy {
                    out.push((b.computer_name.clone(), s.clone()));
                }
            }
        }
        Ok(out)
    }

    /// Internal: SCAN the keyspace for `pulse:computer:*`.
    async fn scan_keys(&self) -> Result<Vec<String>, PulseError> {
        let mut conn = self.conn().await?;
        let pattern = format!("{KEY_PREFIX}*");
        let mut iter = conn.scan_match::<_, String>(pattern).await?;

        let mut keys = Vec::new();
        while let Some(k) = iter.next().await {
            keys.push(k);
        }
        Ok(keys)
    }

    /// Helper (not in the public spec, but handy for diagnostics): return
    /// every computer name currently reporting.
    pub async fn list_computers(&self) -> Result<Vec<String>, PulseError> {
        let keys = self.scan_keys().await?;
        Ok(keys
            .iter()
            .filter_map(|k| Self::strip_prefix(k).map(str::to_string))
            .collect())
    }

    /// Helper: shape an `all_beats()` result as a `HashMap` keyed by name.
    pub async fn beats_by_name(&self) -> Result<HashMap<String, PulseBeatV2>, PulseError> {
        let beats = self.all_beats().await?;
        Ok(beats
            .into_iter()
            .map(|b| (b.computer_name.clone(), b))
            .collect())
    }
}
