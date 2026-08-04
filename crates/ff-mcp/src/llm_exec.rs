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

use ff_core::llm_completion_policy::{
    apply_completion_policy, validate_completion_response, CompletionBudget, WorkloadClass,
};
use ff_orchestrator::cascade_strategy::LlmExec;
use serde_json::json;

/// LlmExec impl that hits live fleet endpoints. See module docs for the
/// resolver behaviour.
pub struct GatewayLlmExec {
    client: reqwest::Client,
    pool: Option<sqlx::PgPool>,
    workload: WorkloadClass,
    completion_ceiling: Option<CompletionBudget>,
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
            workload: WorkloadClass::CodeOneShot,
            completion_ceiling: None,
        }
    }

    /// Attach a Postgres pool so the resolver can query
    /// `fleet_model_deployments`. Without this, the exec falls back to its
    /// hardcoded endpoint map.
    pub fn with_pool(mut self, pool: sqlx::PgPool) -> Self {
        self.pool = Some(pool);
        self
    }

    /// Apply the caller-selected completion policy to non-judge stages.
    pub fn with_workload(mut self, workload: WorkloadClass) -> Self {
        self.workload = workload;
        self
    }

    /// Bound non-judge stage budgets without silently increasing them.
    pub fn with_completion_ceiling(mut self, ceiling: Option<CompletionBudget>) -> Self {
        self.completion_ceiling = ceiling;
        self
    }

    /// Workload tag the cascade tier should resolve against. Multiple tags
    /// per tier act as an OR: the resolver tries each in order and stops on
    /// the first hit. Tier-1 wants code-capable scaffolders, tier-2 wants
    /// verifier-grade reasoning, tier-3 wants generalist synthesizers.
    pub fn workload_tags_for_tier(tier: u8) -> &'static [&'static str] {
        // NOTE: these MUST match the vocabulary actually used in
        // fleet_model_catalog.preferred_workloads. The live catalog uses
        // `code-gen` (not `code`), `research` (not `reasoning`), plus `agent`,
        // `tool_calling`, `chat`. The old tags (`code`/`reasoning`) matched
        // NOTHING for tier-2, so tier-2 always fell through to the (stale)
        // hardcoded endpoint — the lily:55001 "single dispatch failed" bug.
        // Keep the legacy tags too for forward-compat with any re-tagging.
        match tier {
            1 => &["code-gen", "code", "tool_calling", "chat"],
            2 => &["research", "reasoning", "code-gen", "agent"],
            3 => &["chat", "research", "tool_calling"],
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
            // Last-resort only (hit when the DB pool is entirely absent — the
            // any-healthy resolver covers the pool-present case). Point at the
            // stable leader, not a node whose per-slot config drifts: the old
            // value (lily:55001 deepseek) went dead when lily's config changed
            // and caused "single dispatch failed: POST …:55001".
            2 => ("http://192.168.5.100:55001".into(), "qwen36-35b-a3b".into()),
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

    /// Relaxed fallback: the most-recently-healthy CHAT-CAPABLE deployment of
    /// ANY model, ignoring the tier's specific workload tags. Used when the
    /// tier-specific resolver finds nothing, so `fleet_run` routes to a LIVE
    /// endpoint (any of the healthy servers) instead of a stale hardcoded IP.
    /// This is what prevents the "single dispatch failed: POST
    /// http://<dead-host>:<port>" class of failure when a tier's preferred
    /// model isn't currently deployed. Embedding/reranker-only endpoints are
    /// excluded (they can't answer a chat completion).
    async fn resolve_any_healthy(pool: &sqlx::PgPool) -> Option<(String, String)> {
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
               AND NOT (cat.preferred_workloads @> '["embedding"]'::jsonb
                        AND NOT (cat.preferred_workloads @> '["chat"]'::jsonb
                              OR cat.preferred_workloads @> '["code"]'::jsonb
                              OR cat.preferred_workloads @> '["reasoning"]'::jsonb
                              OR cat.preferred_workloads @> '["tool_calling"]'::jsonb))
             ORDER BY d.last_health_at DESC NULLS LAST
             LIMIT 1
            "#,
        )
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()?;
        use sqlx::Row;
        let port: i32 = row.try_get("port").ok()?;
        let host: String = row.try_get("host").ok()?;
        let catalog_id: String = row.try_get("catalog_id").ok()?;
        Some((format!("http://{host}:{port}"), catalog_id))
    }

    /// Tier-aware endpoint resolution: try the tier-specific live fleet match
    /// first, then ANY healthy chat-capable deployment, and only fall back to
    /// the hardcoded map if the pool is absent entirely. The any-healthy step
    /// is what keeps `fleet_run` working when a tier's preferred model isn't
    /// deployed — routing to a live server beats dialing a stale hardcoded IP.
    async fn endpoint_for_tier(&self, tier: u8) -> (String, String) {
        if let Some(pool) = &self.pool {
            if let Some(dynamic) = Self::resolve_dynamic(pool, tier).await {
                tracing::debug!(
                    tier,
                    endpoint = %dynamic.0,
                    model = %dynamic.1,
                    "GatewayLlmExec: dynamic resolution"
                );
                return dynamic;
            }
            if let Some(any) = Self::resolve_any_healthy(pool).await {
                tracing::warn!(
                    tier,
                    endpoint = %any.0,
                    model = %any.1,
                    "GatewayLlmExec: no tier-specific match; using any-healthy live endpoint"
                );
                return any;
            }
        }
        let fallback = Self::hardcoded_endpoint_for_tier(tier);
        tracing::warn!(
            tier,
            endpoint = %fallback.0,
            model = %fallback.1,
            "GatewayLlmExec: no live deployment resolvable; last-resort hardcoded endpoint"
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
        workload: WorkloadClass,
        budget: CompletionBudget,
        timeout: Duration,
    ) -> Result<String, String> {
        let url = ff_core::url::normalize_chat_completions_url(endpoint);
        let body = Self::completion_request_body(model, prompt, workload, budget)?;
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| format!("POST {url}: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("{url} returned HTTP {status}"));
        }
        let payload: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("decode completion response from {url}: {e}"))?;
        validate_completion_response(&payload)
            .map(|completion| completion.content)
            .map_err(|error| format!("{url}: invalid completion: {error}"))
    }

    fn completion_request_body(
        model: &str,
        prompt: &str,
        workload: WorkloadClass,
        budget: CompletionBudget,
    ) -> Result<serde_json::Value, String> {
        let mut body = json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "temperature": 0.3,
        });
        apply_completion_policy(&mut body, workload, budget)
            .map_err(|error| format!("completion request policy rejected request: {error}"))?;
        Ok(body)
    }

    fn stage_budget(&self, requested: u32) -> Result<CompletionBudget, String> {
        let effective = self
            .completion_ceiling
            .map(|ceiling| requested.min(ceiling.get()))
            .unwrap_or(requested);
        CompletionBudget::new(effective)
            .map_err(|error| format!("invalid completion budget: {error}"))
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
        let budget = self.stage_budget(max_tokens)?;
        self.http_complete(&endpoint, &model, prompt, self.workload, budget, timeout)
            .await
    }

    async fn judge(
        &self,
        prompt: &str,
        max_tokens: u32,
        timeout: Duration,
    ) -> Result<String, String> {
        let (endpoint, model) = self.judge_endpoint().await?;
        let budget = CompletionBudget::new(max_tokens)
            .map_err(|error| format!("invalid judge completion budget: {error}"))?;
        self.http_complete(
            &endpoint,
            &model,
            prompt,
            WorkloadClass::Reasoning,
            budget,
            timeout,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_1_prefers_code() {
        // Leads with the catalog's real code tag (`code-gen`), not legacy `code`.
        let tags = GatewayLlmExec::workload_tags_for_tier(1);
        assert_eq!(tags[0], "code-gen");
    }

    #[test]
    fn tier_2_prefers_research_reasoning() {
        // Must lead with a tag the live catalog actually uses (`research`),
        // not the legacy `reasoning` that matched nothing.
        let tags = GatewayLlmExec::workload_tags_for_tier(2);
        assert_eq!(tags[0], "research");
    }

    #[test]
    fn tier_1_and_2_use_catalog_vocab() {
        // Guard against re-introducing tags absent from the catalog.
        assert!(GatewayLlmExec::workload_tags_for_tier(1).contains(&"code-gen"));
        assert!(GatewayLlmExec::workload_tags_for_tier(2).contains(&"research"));
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

    #[test]
    fn code_request_disables_thinking_and_preserves_exact_budget() {
        let body = GatewayLlmExec::completion_request_body(
            "glm-4.5-air",
            "write code",
            WorkloadClass::CodeOneShot,
            CompletionBudget::new(777).unwrap(),
        )
        .unwrap();
        assert_eq!(body["max_tokens"], 777);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
    }

    #[test]
    fn reasoning_request_preserves_endpoint_thinking_default() {
        let body = GatewayLlmExec::completion_request_body(
            "glm-4.5-air",
            "judge this",
            WorkloadClass::Reasoning,
            CompletionBudget::new(256).unwrap(),
        )
        .unwrap();
        assert_eq!(body["max_tokens"], 256);
        assert!(body.get("chat_template_kwargs").is_none());
    }

    #[test]
    fn caller_ceiling_never_increases_stage_budget() {
        let exec = GatewayLlmExec::new()
            .with_completion_ceiling(Some(CompletionBudget::new(512).unwrap()));
        assert_eq!(exec.stage_budget(2_048).unwrap().get(), 512);
        assert_eq!(exec.stage_budget(128).unwrap().get(), 128);
    }
}
