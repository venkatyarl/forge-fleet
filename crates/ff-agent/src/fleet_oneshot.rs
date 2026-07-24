//! Stateless one-shot dispatch to a fleet LLM endpoint.
//!
//! The reusable "prompt → text via a fleet model" primitive — no sub-agent slot
//! claim, no work_outputs persistence (that's `agent_coordinator::dispatch_task`),
//! no MCP JSON shape (that's `ff-mcp::handlers::fleet_run`). Just: pick a healthy
//! deployment from the live router, POST an OpenAI-shape chat completion, return
//! the assistant text plus the endpoint/worker/model that served it (so callers
//! can attribute the turn in `ff_interactions`).
//!
//! Execution-only: this module never inserts into `ff_interactions` itself.
//! Callers holding the semantic context do the logging — and callers with a
//! work item in scope stamp the V250 episodic tags (`work_item_id`, `purpose`)
//! on the row (see `codegen_apply::round_interaction`,
//! `work_item_dispatch::record_review_interaction`).
//!
//! Council verdict 2026-06-19 (codex decisive): put the shared primitive in
//! ff-agent (the right dependency direction — ff-terminal & ff-mcp both depend on
//! it) rather than forking an inline POST or making ff-terminal depend on ff-mcp.
//! First caller is `ff council --members local:<model>`; `fleet_run` can migrate
//! onto this later.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use ff_db::queries::{RouteCandidate, RouteFilter, pg_route_deployments};
use serde_json::{Value, json};
use sqlx::PgPool;

/// The outcome of a one-shot fleet dispatch — the text plus who served it.
#[derive(Debug, Clone)]
pub struct FleetOneshot {
    pub text: String,
    /// Base endpoint that served the call (e.g. `http://192.168.5.103:55000`).
    pub endpoint: String,
    pub worker_name: String,
    /// The catalog model name that answered (best-effort).
    pub model: String,
    pub latency_ms: u128,
    /// Prompt/completion tokens from the response `usage` block (0 when the
    /// server omits it), so callers can attribute the turn's cost in
    /// `ff_interactions` instead of logging 0/0.
    pub tokens_in: i32,
    pub tokens_out: i32,
}

/// In-memory, per-process count of in-flight `fleet_oneshot` requests keyed by
/// deployment endpoint. This lets us treat a catalog family as a pool and
/// respect each deployment's `parallel_slots` cap without relying on the DB's
/// sampled `llm_active_requests`, which is stale on the order of seconds.
static IN_FLIGHT: LazyLock<Mutex<HashMap<String, u32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// RAII token that increments a deployment's in-flight count while a request is
/// active and decrements it (removing the entry when it hits zero) on drop.
struct InFlightGuard {
    key: String,
}

impl InFlightGuard {
    /// Increment the counter unconditionally. Used only as a last-resort
    /// fallback when every healthy deployment is already at its cap.
    fn acquire(key: &str) -> Self {
        let mut map = IN_FLIGHT.lock().expect("in_flight lock poisoned");
        *map.entry(key.to_string()).or_insert(0) += 1;
        Self {
            key: key.to_string(),
        }
    }

    /// Increment the counter only if the deployment has free capacity.
    fn try_acquire(key: &str, slots: u32) -> Option<Self> {
        let mut map = IN_FLIGHT.lock().expect("in_flight lock poisoned");
        let count = map.get(key).copied().unwrap_or(0);
        if count >= slots {
            return None;
        }
        *map.entry(key.to_string()).or_insert(0) += 1;
        Some(Self {
            key: key.to_string(),
        })
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut map = IN_FLIGHT.lock().expect("in_flight lock poisoned");
        let entry = map.entry(self.key.clone()).or_insert(0);
        if *entry > 0 {
            *entry -= 1;
        }
        if *entry == 0 {
            map.remove(&self.key);
        }
    }
}

fn inflight_count(endpoint: &str) -> u32 {
    IN_FLIGHT
        .lock()
        .expect("in_flight lock poisoned")
        .get(endpoint)
        .copied()
        .unwrap_or(0)
}

/// Dispatch `prompt` to one healthy fleet deployment and return its answer.
///
/// `model_hint` (e.g. `qwen36-35b` from a `local:qwen36-35b` council member)
/// biases candidate selection toward deployments whose catalog id/name/family
/// contain it. When a family can be resolved, all healthy deployments of that
/// family are treated as a single pool: the least-loaded deployment that still
/// has free `parallel_slots` capacity is chosen first, and only when the pool
/// is saturated do we fall back to other healthy candidates.
///
/// If the calling node currently holds an active work-item build lease, local
/// deployments are deprioritised as a tiebreak — the node's cores are busy
/// compiling, so inference should be served elsewhere when possible.
pub async fn fleet_oneshot(
    pool: &PgPool,
    prompt: &str,
    model_hint: Option<&str>,
    timeout: Option<Duration>,
) -> Result<FleetOneshot> {
    let ordered = resolve_route_candidates(pool, model_hint).await?;

    let client = reqwest::Client::builder()
        .timeout(timeout.unwrap_or(Duration::from_secs(180)))
        .build()
        .map_err(|e| anyhow!("build http client: {e}"))?;

    let mut last_err: Option<anyhow::Error> = None;
    let mut attempted = false;

    // First pass: honour parallel_slots caps.
    for cand in &ordered {
        let slots = cand.parallel_slots.unwrap_or(1).max(1) as u32;
        let Some(_guard) = InFlightGuard::try_acquire(&cand.endpoint, slots) else {
            continue;
        };
        attempted = true;
        match dispatch_to_candidate(cand, &client, prompt, model_hint).await {
            Ok(ok) => return Ok(ok),
            Err(e) => {
                tracing::warn!(
                    worker = %cand.worker_name,
                    error = %e,
                    "fleet_oneshot: candidate failed — failing over to next"
                );
                last_err = Some(e);
            }
        }
    }

    // If every candidate was at its cap in this process, run an uncapped
    // fallback pass so a heavily loaded fleet still returns an answer.
    if !attempted {
        tracing::warn!(
            "fleet_oneshot: all healthy candidates at parallel_slots cap; running uncapped fallback"
        );
        for cand in &ordered {
            let _guard = InFlightGuard::acquire(&cand.endpoint);
            match dispatch_to_candidate(cand, &client, prompt, model_hint).await {
                Ok(ok) => return Ok(ok),
                Err(e) => {
                    tracing::warn!(
                        worker = %cand.worker_name,
                        error = %e,
                        "fleet_oneshot: uncapped fallback candidate failed"
                    );
                    last_err = Some(e);
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("all fleet candidates failed")))
}

/// Resolve a catalog model hint to the same best-first deployment candidate
/// ordering used by [`fleet_oneshot`].
pub async fn resolve_route_candidate(pool: &PgPool, model_hint: &str) -> Result<RouteCandidate> {
    resolve_route_candidates(pool, Some(model_hint))
        .await?
        .into_iter()
        .find(|candidate| candidate_catalog_id_matches(candidate, model_hint))
        .ok_or_else(|| anyhow!("no healthy fleet deployment matches local:{model_hint}"))
}

async fn resolve_route_candidates(
    pool: &PgPool,
    model_hint: Option<&str>,
) -> Result<Vec<RouteCandidate>> {
    let filter = RouteFilter {
        workload: None,
        require_tool_calling: false,
        min_ctx: None,
        exclude_hosts: Vec::new(),
        // Only dispatch to deployments whose health is fresh — never a wedged host
        // lingering as 'healthy' with a stale heartbeat (the priya-wedge class).
        max_health_age_sec: Some(180),
        prefer_least_loaded: true,
        // With a model hint, widen the candidate set so the match isn't truncated:
        // the best-scored top-8 may not include the requested model (e.g. a lower-
        // tier coder deployment), and we'd silently fall back. No hint → top-8.
        limit: if model_hint.is_some() { 64 } else { 8 },
    };
    let all_candidates = pg_route_deployments(pool, &filter)
        .await
        .map_err(|e| anyhow!("route deployments: {e}"))?;
    if all_candidates.is_empty() {
        return Err(anyhow!(
            "no healthy fleet deployment to serve a local council member"
        ));
    }
    // Drop deployments with no usable model name (empty catalog_id AND
    // catalog_name). Those are "unknown model" rows — e.g. ace's mlx:55000,
    // which is marked healthy but is NOT a real chat-completions server: sending
    // it `model="local"` makes it try to fetch a HF repo named "local" and
    // return an HTTP error, which masked as "fleet_oneshot round 1" and forced
    // every local codegen dispatch to fall back to slow cloud codex
    // (dogfooded 2026-07-01). Only keep them as a last resort so a fleet with
    // ONLY unknown-model deployments still attempts a call.
    let named: Vec<RouteCandidate> = all_candidates
        .iter()
        .filter(|c| has_model_name(c))
        .cloned()
        .collect();
    let candidates: &[RouteCandidate] = if named.is_empty() {
        &all_candidates
    } else {
        &named
    };

    let this_worker = crate::fleet_info::resolve_this_worker_name().await;
    let prefer_non_local = this_node_has_active_build_lease(pool).await;
    let family = resolve_hint_family(candidates, model_hint);
    let ordered = rank_candidates(
        candidates,
        &this_worker,
        family.as_deref(),
        prefer_non_local,
    );
    Ok(ordered.into_iter().cloned().collect())
}

async fn dispatch_to_candidate(
    cand: &RouteCandidate,
    client: &reqwest::Client,
    prompt: &str,
    model_hint: Option<&str>,
) -> anyhow::Result<FleetOneshot> {
    let worker_name = cand.worker_name.clone();
    let endpoint = cand.endpoint.clone();
    let model = cand
        .catalog_name
        .clone()
        .or_else(|| model_hint.map(|s| s.to_string()))
        .unwrap_or_else(|| "local".to_string());
    let url = ff_core::url::normalize_chat_completions_url(&endpoint);
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": false,
    });
    let start = std::time::Instant::now();

    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("POST {url}: {e}"))?;
    let status = resp.status();
    let payload: Value = resp
        .json()
        .await
        .map_err(|e| anyhow!("decode response from {worker_name}: {e}"))?;
    if !status.is_success() {
        return Err(anyhow!(
            "{worker_name} ({model}) returned HTTP {status}: {}",
            payload.to_string().chars().take(400).collect::<String>()
        ));
    }
    let text = extract_completion_text(&payload)
        .map(|t| strip_think_block(&t))
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| anyhow!("{worker_name} ({model}) returned an empty completion"))?;
    let (tokens_in, tokens_out) = usage_tokens_i32(&payload);
    Ok(FleetOneshot {
        text,
        endpoint: endpoint.clone(),
        worker_name: worker_name.clone(),
        model: model.clone(),
        latency_ms: start.elapsed().as_millis(),
        tokens_in,
        tokens_out,
    })
}

/// True when this node currently holds an unreleased work-item build lease.
/// That means its cores are busy compiling, so local inference should be a
/// tiebreak loser when a non-local deployment is equally available.
async fn this_node_has_active_build_lease(pool: &PgPool) -> bool {
    let worker = crate::fleet_info::resolve_this_worker_name().await;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (\
            SELECT 1 \
              FROM work_item_leases l \
              JOIN computers c ON c.id = l.computer_id \
             WHERE l.released_at IS NULL \
               AND LOWER(c.name) = LOWER($1)\
         )",
    )
    .bind(&worker)
    .fetch_one(pool)
    .await
    .inspect_err(|e| {
        tracing::warn!(
            error = %e,
            "fleet_oneshot: failed to check active build lease; assuming none"
        )
    })
    .unwrap_or(false);
    exists
}

/// Resolve the hinted catalog family, if possible. We look for the first
/// candidate whose catalog id, catalog name, or family contains the hint
/// (case-insensitive), then return that candidate's family so the whole
/// family pool can be load-balanced.
fn resolve_hint_family(candidates: &[RouteCandidate], hint: Option<&str>) -> Option<String> {
    let hint = hint?;
    if hint.is_empty() {
        return None;
    }
    candidates
        .iter()
        .find(|candidate| candidate_matches_hint(candidate, hint))
        .and_then(|c| c.family.clone())
}

fn candidate_matches_hint(candidate: &RouteCandidate, hint: &str) -> bool {
    let hint = hint.to_lowercase();
    let matches = |value: Option<&str>| {
        value
            .map(|value| value.to_lowercase().contains(&hint))
            .unwrap_or(false)
    };
    matches(candidate.catalog_id.as_deref())
        || matches(candidate.catalog_name.as_deref())
        || matches(candidate.family.as_deref())
}

fn candidate_catalog_id_matches(candidate: &RouteCandidate, catalog_id: &str) -> bool {
    candidate
        .catalog_id
        .as_deref()
        .is_some_and(|id| id.eq_ignore_ascii_case(catalog_id))
}

/// Order candidates for dispatch.
///
/// 1. In-family deployments with free `parallel_slots` capacity, sorted by
///    in-flight load fraction (ascending).
/// 2. In-family deployments that are already at capacity (failover within pool).
/// 3. Other healthy deployments (failover outside the family).
///
/// When `prefer_non_local` is true, a non-local deployment wins a load tie
/// against a local one.
fn rank_candidates<'a>(
    candidates: &'a [RouteCandidate],
    this_worker: &str,
    family: Option<&str>,
    prefer_non_local: bool,
) -> Vec<&'a RouteCandidate> {
    let is_local = |c: &RouteCandidate| c.worker_name.eq_ignore_ascii_case(this_worker);
    let in_family = |c: &RouteCandidate| {
        family
            .map(|f| {
                c.family
                    .as_deref()
                    .is_some_and(|cf| cf.eq_ignore_ascii_case(f))
            })
            .unwrap_or(true)
    };

    type Item<'b> = (usize, &'b RouteCandidate, f64, bool);
    let mut eligible_family: Vec<Item> = Vec::new();
    let mut eligible_other: Vec<Item> = Vec::new();
    let mut full_family: Vec<Item> = Vec::new();
    let mut full_other: Vec<Item> = Vec::new();

    for (idx, c) in candidates.iter().enumerate() {
        let slots = c.parallel_slots.unwrap_or(1).max(1) as u32;
        let inflight = inflight_count(&c.endpoint);
        let load = inflight as f64 / slots as f64;
        let local = is_local(c);
        let family_match = in_family(c);
        let at_cap = inflight >= slots;
        match (family_match, at_cap) {
            (true, false) => eligible_family.push((idx, c, load, local)),
            (true, true) => full_family.push((idx, c, load, local)),
            (false, false) => eligible_other.push((idx, c, load, local)),
            (false, true) => full_other.push((idx, c, load, local)),
        }
    }

    let cmp = |a: &Item, b: &Item| {
        let load_cmp = a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal);
        let local_cmp = if prefer_non_local {
            // false (non-local) sorts before true (local)
            a.3.cmp(&b.3)
        } else {
            std::cmp::Ordering::Equal
        };
        let idx_cmp = a.0.cmp(&b.0);
        load_cmp.then(local_cmp).then(idx_cmp)
    };

    eligible_family.sort_by(cmp);
    eligible_other.sort_by(cmp);
    full_family.sort_by(cmp);
    full_other.sort_by(cmp);

    eligible_family
        .into_iter()
        .chain(eligible_other)
        .chain(full_family)
        .chain(full_other)
        .map(|(_, c, _, _)| c)
        .collect()
}

/// Read `(tokens_in, tokens_out)` from a chat-completion `usage` block, clamped
/// into `i32` for the `ff_interactions` columns. Reuses the canonical
/// `research::parse_completion_usage` walk (no forked JSON parsing); a server
/// that omits `usage`, or absurd values, degrade to `0`/`i32::MAX`. Pure.
pub(crate) fn usage_tokens_i32(payload: &Value) -> (i32, i32) {
    let (pt, ct) = crate::research::parse_completion_usage(payload);
    let clamp = |n: u64| i32::try_from(n).unwrap_or(i32::MAX);
    (clamp(pt), clamp(ct))
}

/// True if the deployment carries a usable model name (non-empty catalog_id or
/// catalog_name). A candidate with neither can't be given a valid `model` value
/// and is often not a real chat server (see the ace mlx:55000 case), so
/// `fleet_oneshot` excludes these from selection except as a last resort. Pure.
fn has_model_name(c: &RouteCandidate) -> bool {
    model_name_present(c.catalog_id.as_deref(), c.catalog_name.as_deref())
}

/// Pure core of [`has_model_name`]: true when either field is non-empty.
fn model_name_present(catalog_id: Option<&str>, catalog_name: Option<&str>) -> bool {
    let present = |s: Option<&str>| s.map(|v| !v.trim().is_empty()).unwrap_or(false);
    present(catalog_id) || present(catalog_name)
}

/// Pull the assistant text out of an OpenAI-shape chat-completion payload,
/// tolerating both `message.content` and the legacy `text` field.
pub(crate) fn extract_completion_text(payload: &Value) -> Option<String> {
    let choice = payload.get("choices")?.as_array()?.first()?;
    if let Some(content) = choice
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        && !content.is_empty()
    {
        return Some(content.to_string());
    }
    choice
        .get("text")
        .and_then(|t| t.as_str())
        .map(String::from)
}

/// Strip a leading `<think>…</think>` reasoning block some local models emit so
/// the council sees only the answer.
pub(crate) fn strip_think_block(s: &str) -> String {
    let t = s.trim_start();
    if let Some(rest) = t.strip_prefix("<think>")
        && let Some(end) = rest.find("</think>")
    {
        return rest[end + "</think>".len()..].trim().to_string();
    }
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(
        endpoint: &str,
        worker: &str,
        family: Option<&str>,
        slots: Option<i32>,
    ) -> RouteCandidate {
        RouteCandidate {
            worker_name: worker.to_string(),
            endpoint: endpoint.to_string(),
            port: 0,
            runtime: None,
            catalog_id: None,
            catalog_name: None,
            family: family.map(String::from),
            tier: 2,
            tool_calling: true,
            context_window: None,
            usable_agent_ctx: None,
            parallel_slots: slots,
            health_status: "healthy".to_string(),
            health_age_sec: None,
            os_family: None,
            has_gpu: None,
            is_unified_memory: None,
            total_ram_gb: None,
            cpu_pct: None,
            llm_active_requests: None,
        }
    }

    #[test]
    fn extracts_message_then_text() {
        let p = json!({"choices":[{"message":{"content":"hello"}}]});
        assert_eq!(extract_completion_text(&p).as_deref(), Some("hello"));
        let p = json!({"choices":[{"text":"legacy"}]});
        assert_eq!(extract_completion_text(&p).as_deref(), Some("legacy"));
        assert_eq!(extract_completion_text(&json!({})), None);
    }

    // Authored by a fleet model (qwen36 on lily) via `ff offload`, hand-verified,
    // then integrated — dogfooding the fleet for test-gen (grows ff_interactions).
    // Pins the usage→i32 clamp that feeds council token attribution.
    #[test]
    fn usage_tokens_i32_reads_usage() {
        assert_eq!(
            usage_tokens_i32(&json!({"usage":{"prompt_tokens":123,"completion_tokens":45}})),
            (123, 45)
        );
        assert_eq!(usage_tokens_i32(&json!({})), (0, 0));
        assert_eq!(
            usage_tokens_i32(
                &json!({"usage":{"prompt_tokens":5000000000u64,"completion_tokens":0}})
            ),
            (i32::MAX, 0)
        );
    }

    #[test]
    fn strips_think_block() {
        assert_eq!(
            strip_think_block("<think>reasoning</think>  answer"),
            "answer"
        );
        assert_eq!(strip_think_block("plain"), "plain");
    }

    #[test]
    fn model_name_present_excludes_unknown_deployments() {
        // A named coder deployment passes.
        assert!(model_name_present(Some("qwen3-coder-30b"), None));
        assert!(model_name_present(None, Some("Qwen3 Coder")));
        // ace's mlx:55000 "unknown model" — empty/whitespace/None both ways — is
        // excluded so fleet_oneshot never routes local codegen to a non-chat
        // endpoint that returns HTTP errors (the Lane-1 root cause).
        assert!(!model_name_present(None, None));
        assert!(!model_name_present(Some(""), Some("  ")));
        assert!(!model_name_present(Some("   "), None));
    }

    #[test]
    fn resolve_hint_family_matches_catalog_or_name_or_family() {
        let mut c1 = candidate("http://a:1", "a", Some("qwen3-coder"), Some(2));
        c1.catalog_id = Some("qwen3-coder-480b".to_string());
        let c2 = candidate("http://b:1", "b", Some("qwen3-coder"), Some(2));
        let c3 = candidate("http://c:1", "c", Some("gemma"), Some(2));
        let pool = vec![c1, c2, c3];

        // Hint matches family substring of the first candidate.
        assert_eq!(
            resolve_hint_family(&pool, Some("coder")).as_deref(),
            Some("qwen3-coder")
        );
        // Exact family hit.
        assert_eq!(
            resolve_hint_family(&pool, Some("gemma")).as_deref(),
            Some("gemma")
        );
        // Unmatched or absent hint returns None.
        assert_eq!(resolve_hint_family(&pool, Some("unknown")), None);
        assert_eq!(resolve_hint_family(&pool, None), None);
        assert!(candidate_matches_hint(&pool[0], "qwen3-coder-480b"));
        assert!(!candidate_matches_hint(&pool[1], "qwen3-coder-480b"));
        assert!(candidate_catalog_id_matches(&pool[0], "QWEN3-CODER-480B"));
        assert!(!candidate_catalog_id_matches(&pool[0], "qwen3-coder"));
    }

    #[test]
    fn rank_prefers_least_loaded_and_non_local_when_building() {
        let c1 = candidate("http://test-prefers-lily:1", "lily", Some("coder"), Some(2));
        let c2 = candidate(
            "http://test-prefers-marcus:1",
            "marcus",
            Some("coder"),
            Some(2),
        );
        let c3 = candidate(
            "http://test-prefers-taylor:1",
            "taylor",
            Some("coder"),
            Some(2),
        );
        let pool = vec![c1, c2, c3];

        // With no active build lease, equal load preserves the original DB order.
        let ordered = rank_candidates(&pool, "lily", Some("coder"), false);
        assert_eq!(ordered[0].endpoint, "http://test-prefers-lily:1");

        // With an active build lease, the local node loses ties.
        let ordered = rank_candidates(&pool, "lily", Some("coder"), true);
        assert_eq!(ordered[0].endpoint, "http://test-prefers-marcus:1");
        assert!(
            ordered
                .iter()
                .last()
                .unwrap()
                .worker_name
                .eq_ignore_ascii_case("lily")
        );
    }

    #[test]
    fn rank_respects_parallel_slots_cap_within_family() {
        let local = candidate("http://test-cap-local:1", "lily", Some("coder"), Some(1));
        let remote = candidate("http://test-cap-remote:1", "marcus", Some("coder"), Some(2));
        set_inflight_for_test("http://test-cap-local:1", 1);
        set_inflight_for_test("http://test-cap-remote:1", 1);

        let pool = vec![local, remote];
        let ordered = rank_candidates(&pool, "lily", Some("coder"), false);

        // Remote has free capacity (1/2); local is at its cap (1/1).
        assert_eq!(ordered[0].endpoint, "http://test-cap-remote:1");
        assert_eq!(ordered[1].endpoint, "http://test-cap-local:1");

        set_inflight_for_test("http://test-cap-local:1", 0);
        set_inflight_for_test("http://test-cap-remote:1", 0);
    }

    #[test]
    fn rank_falls_back_outside_family_when_family_is_full() {
        let local = candidate(
            "http://test-fallback-lily:1",
            "lily",
            Some("coder"),
            Some(1),
        );
        let remote_coder = candidate(
            "http://test-fallback-marcus:1",
            "marcus",
            Some("coder"),
            Some(1),
        );
        let remote_other = candidate(
            "http://test-fallback-taylor:1",
            "taylor",
            Some("llama"),
            Some(4),
        );
        set_inflight_for_test("http://test-fallback-lily:1", 1);
        set_inflight_for_test("http://test-fallback-marcus:1", 1);

        let pool = vec![local, remote_coder, remote_other];
        let ordered = rank_candidates(&pool, "lily", Some("coder"), false);

        // coder family is full, so the free non-family deployment comes first.
        assert_eq!(ordered[0].endpoint, "http://test-fallback-taylor:1");

        set_inflight_for_test("http://test-fallback-lily:1", 0);
        set_inflight_for_test("http://test-fallback-marcus:1", 0);
    }

    #[cfg(test)]
    fn set_inflight_for_test(endpoint: &str, count: u32) {
        let mut map = IN_FLIGHT.lock().expect("in_flight lock poisoned");
        if count == 0 {
            map.remove(endpoint);
        } else {
            map.insert(endpoint.to_string(), count);
        }
    }
}
