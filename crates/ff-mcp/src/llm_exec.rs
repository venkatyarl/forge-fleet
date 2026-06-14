//! Production `LlmExec` impl that backs `fleet_cascade` and the cascade-aware
//! path of `fleet_run`.
//!
//! Hits live fleet endpoints over HTTP, resolving (host, model) per tier
//! dynamically from `fleet_model_deployments` so the cascade auto-adapts when
//! a node goes down — no hardcoded SHAs anywhere in the hot path.
//!
//! Fallback chain:
//!
//!   1. Dynamic resolution: pick the best healthy deployment whose catalog
//!      has the workload tag for this cascade tier (`code`, `reasoning`,
//!      `chat`, ...).
//!   2. If DB resolution fails (no pool, no rows, no catalog linkage), fall
//!      back to a hardcoded preferred-endpoint map.
//!   3. If even the hardcoded fallback's endpoint is unreachable, the
//!      cascade surfaces the network error and run_cascade reports it.
//!
//! Lifted out of `handlers.rs` (Path 3) so both `fleet_run` and
//! `fleet_cascade` share the same dispatch primitive. Before this move,
//! cascade-aware routing was only available on `fleet_cascade`; now
//! `fleet_run` with `strategy="auto"` reaches the same code.

use std::time::Duration;

use ff_orchestrator::cascade_strategy::LlmExec;
use serde_json::json;

/// LlmExec impl that hits live fleet endpoints. See module docs for the
/// resolver behaviour.
pub struct GatewayLlmExec {
    client: reqwest::Client,
    pool: Option<sqlx::PgPool>,
}

impl GatewayLlmExec {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                // Match the per-tier ceiling in cascade_strategy::run_cascade.
                .timeout(Duration::from_secs(600))
                .build()
                .expect("reqwest client"),
            pool: None,
        }
    }

    /// Attach a Postgres pool so the resolver can query
    /// `fleet_model_deployments`. Without this, the exec falls back to its
    /// hardcoded endpoint map.
    pub fn with_pool(mut self, pool: sqlx::PgPool) -> Self {
        self.pool = Some(pool);
        self
    }

    /// Workload tag the cascade tier should resolve against. Multiple tags
    /// per tier act as an OR: the resolver tries each in order and stops on
    /// the first hit. Tier-1 wants code-capable scaffolders, tier-2 wants
    /// verifier-grade reasoning, tier-3 wants generalist synthesizers.
    pub fn workload_tags_for_tier(tier: u8) -> &'static [&'static str] {
        match tier {
            1 => &["code", "tool_calling", "chat"],
            2 => &["reasoning", "code"],
            3 => &["chat", "reasoning", "tool_calling"],
            _ => &["chat", "tool_calling"],
        }
    }

    /// Hardcoded fallback endpoints — last resort if DB resolution fails.
    /// Identical to the pre-resolver behaviour so a missing pool degrades
    /// cleanly to "what we had yesterday."
    pub fn hardcoded_endpoint_for_tier(tier: u8) -> (String, String) {
        match tier {
            1 => (
                "http://192.168.5.102:55000".into(),
                "qwen3-coder-30b-a3b".into(),
            ),
            2 => (
                "http://192.168.5.113:55001".into(),
                "deepseek-r1-distill-qwen-32b".into(),
            ),
            _ => (
                "http://192.168.5.100:55001".into(),
                "/Users/venkat/models/qwen36-35b-a3b".into(),
            ),
        }
    }

    /// Resolve the best healthy deployment for `tier`. See module docs.
    async fn resolve_dynamic(pool: &sqlx::PgPool, tier: u8) -> Option<(String, String)> {
        for tag in Self::workload_tags_for_tier(tier) {
            let arr = serde_json::json!([tag]);
            // Some catalog rows use plural tags ("embeddings" vs "embedding").
            // Try the literal then a `*s` variant.
            let pluralized = format!("{tag}s");
            let arr_plural = serde_json::json!([pluralized]);

            let row = sqlx::query(
                r#"
                SELECT d.port,
                       COALESCE(c.primary_ip, w.name) AS host,
                       d.catalog_id
                  FROM fleet_model_deployments d
                  JOIN fleet_model_catalog cat ON cat.id = d.catalog_id
                  LEFT JOIN fleet_workers w     ON w.name = d.worker_name
                  LEFT JOIN computers c         ON LOWER(c.name) = LOWER(d.worker_name)
                 WHERE d.health_status = 'healthy'
                   AND (cat.preferred_workloads @> $1::jsonb
                     OR cat.preferred_workloads @> $2::jsonb)
                 ORDER BY d.last_health_at DESC NULLS LAST
                 LIMIT 1
                "#,
            )
            .bind(&arr)
            .bind(&arr_plural)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();

            if let Some(row) = row {
                use sqlx::Row;
                // Decode all three fields together; if ANY fails we want to
                // try the next workload tag, not abandon the whole resolver.
                let decoded = (|| -> Option<(i32, String, String)> {
                    let port: i32 = row.try_get("port").ok()?;
                    let host: String = row.try_get("host").ok()?;
                    let catalog_id: String = row.try_get("catalog_id").ok()?;
                    Some((port, host, catalog_id))
                })();
                if let Some((port, host, catalog_id)) = decoded {
                    return Some((format!("http://{host}:{port}"), catalog_id));
                }
                tracing::warn!(
                    tier,
                    tag = %tag,
                    "resolve_dynamic: matched row but failed to decode fields, trying next tag"
                );
            }
        }
        None
    }

    /// Tier-aware endpoint resolution: try the live fleet first, fall back
    /// to the hardcoded map if the pool is absent or no eligible deployment
    /// exists.
    async fn endpoint_for_tier(&self, tier: u8) -> (String, String) {
        if let Some(pool) = &self.pool
            && let Some(dynamic) = Self::resolve_dynamic(pool, tier).await
        {
            tracing::debug!(
                tier,
                endpoint = %dynamic.0,
                model = %dynamic.1,
                "GatewayLlmExec: dynamic resolution"
            );
            return dynamic;
        }
        let fallback = Self::hardcoded_endpoint_for_tier(tier);
        tracing::debug!(
            tier,
            endpoint = %fallback.0,
            model = %fallback.1,
            "GatewayLlmExec: fallback to hardcoded endpoint (no dynamic match)"
        );
        fallback
    }

    /// Judge endpoint resolver — picks any healthy `family='gemma'`
    /// deployment ordered by most-recent health check (HA: logan first
    /// then duncan today). Family-based selection is correct here: we
    /// explicitly want a *third-party-family* judge (independent of
    /// Qwen-family generation tiers) to avoid same-family bias.
    ///
    /// Returns Err when no healthy gemma deployment exists, so callers
    /// can surface "no judge available" instead of silently routing
    /// to a dead fallback endpoint.
    async fn judge_endpoint(&self) -> Result<(String, String), String> {
        let pool = self.pool.as_ref().ok_or("judge_endpoint: no DB pool")?;
        let row = sqlx::query(
            r#"
            SELECT d.port,
                   COALESCE(c.primary_ip, w.name) AS host,
                   d.catalog_id
              FROM fleet_model_deployments d
              JOIN fleet_model_catalog cat ON cat.id = d.catalog_id
              LEFT JOIN fleet_workers w     ON w.name = d.worker_name
              LEFT JOIN computers c         ON LOWER(c.name) = LOWER(d.worker_name)
             WHERE d.health_status = 'healthy'
               AND cat.family = 'gemma'
             ORDER BY d.last_health_at DESC NULLS LAST
             LIMIT 1
            "#,
        )
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("judge_endpoint: query failed: {e}"))?
        .ok_or("judge_endpoint: no healthy gemma deployment in fleet_model_deployments")?;
        use sqlx::Row;
        let port: i32 = row
            .try_get("port")
            .map_err(|e| format!("judge_endpoint: decode port: {e}"))?;
        let host: String = row
            .try_get("host")
            .map_err(|e| format!("judge_endpoint: decode host: {e}"))?;
        let catalog_id: String = row
            .try_get("catalog_id")
            .map_err(|e| format!("judge_endpoint: decode catalog_id: {e}"))?;
        Ok((format!("http://{host}:{port}"), catalog_id))
    }

    async fn http_complete(
        &self,
        endpoint: &str,
        model: &str,
        prompt: &str,
        max_tokens: u32,
        timeout: Duration,
    ) -> Result<String, String> {
        let url = ff_core::url::normalize_chat_completions_url(endpoint);
        let body = json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": max_tokens,
            "temperature": 0.3,
        });
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| format!("POST {url}: {e}"))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| format!("read body from {url}: {e}"))?;
        if !status.is_success() {
            return Err(format!("{url} returned {status}: {text}"));
        }
        let payload: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            format!(
                "parse {url} body: {e}; raw: {}",
                &text[..text.len().min(200)]
            )
        })?;
        let content = payload
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("{url}: no choices[0].message.content"))?
            .to_string();
        Ok(content)
    }
}

impl Default for GatewayLlmExec {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl LlmExec for GatewayLlmExec {
    async fn complete(
        &self,
        tier: u8,
        prompt: &str,
        max_tokens: u32,
        timeout: Duration,
    ) -> Result<String, String> {
        let (endpoint, model) = self.endpoint_for_tier(tier).await;
        // Qwen3 family always emits <think> blocks and silently truncates if
        // max_tokens < 1024 (see llm_routing::QWEN3_MAX_TOKENS_FLOOR).
        let effective_max = if model.to_lowercase().contains("qwen3") && max_tokens < 1024 {
            1024
        } else {
            max_tokens
        };
        self.http_complete(&endpoint, &model, prompt, effective_max, timeout)
            .await
    }

    async fn judge(
        &self,
        prompt: &str,
        max_tokens: u32,
        timeout: Duration,
    ) -> Result<String, String> {
        let (endpoint, model) = self.judge_endpoint().await?;
        self.http_complete(&endpoint, &model, prompt, max_tokens, timeout)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_1_prefers_code() {
        let tags = GatewayLlmExec::workload_tags_for_tier(1);
        assert_eq!(tags[0], "code");
    }

    #[test]
    fn tier_2_prefers_reasoning() {
        let tags = GatewayLlmExec::workload_tags_for_tier(2);
        assert_eq!(tags[0], "reasoning");
    }

    #[test]
    fn tier_3_prefers_chat() {
        let tags = GatewayLlmExec::workload_tags_for_tier(3);
        assert_eq!(tags[0], "chat");
    }

    #[test]
    fn unknown_tier_falls_back_safely() {
        for tier in [0u8, 4u8, 9u8] {
            let tags = GatewayLlmExec::workload_tags_for_tier(tier);
            assert!(!tags.is_empty(), "tier {tier} must have fallback tags");
        }
    }

    #[test]
    fn hardcoded_fallback_returns_valid_url() {
        for tier in 1u8..=3 {
            let (endpoint, model) = GatewayLlmExec::hardcoded_endpoint_for_tier(tier);
            assert!(endpoint.starts_with("http://"));
            assert!(!model.is_empty());
        }
    }
}
