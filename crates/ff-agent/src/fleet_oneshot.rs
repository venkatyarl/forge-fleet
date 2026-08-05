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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use ff_core::llm_completion_policy::{
    CompletionBudget, WorkloadClass, apply_completion_policy, validate_completion_response,
};
use ff_db::queries::{RouteCandidate, RouteFilter, pg_route_deployments};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;

/// The outcome of a one-shot fleet dispatch — the text plus who served it.
#[derive(Debug, Clone)]
pub struct FleetOneshot {
    pub text: String,
    /// Immutable deployment row that served this request.
    pub deployment_id: sqlx::types::Uuid,
    /// Base endpoint that served the call (e.g. `http://192.168.5.103:55000`).
    pub endpoint: String,
    pub worker_name: String,
    /// Stable fleet catalog id that served the call (e.g. `glm-4.5-air`).
    pub catalog_id: Option<String>,
    /// The catalog model name that answered (best-effort).
    pub model: String,
    /// Whether the caller pinned this target or the router selected it.
    pub provenance: ResolvedTargetProvenance,
    pub latency_ms: u128,
    /// Prompt/completion tokens from the response `usage` block (0 when the
    /// server omits it), so callers can attribute the turn's cost in
    /// `ff_interactions` instead of logging 0/0.
    pub tokens_in: i32,
    pub tokens_out: i32,
}

/// Why an endpoint/model pair was selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedTargetProvenance {
    ExplicitCatalog,
    ExplicitUrl,
    Auto,
}

/// Result of proving the model identity served by an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointAttestationState {
    Pending,
    Verified,
    UnverifiedTimeout,
}

/// Exact served-model identity explicitly authorized by one catalog variant.
///
/// Keeping the source fields alongside the alias makes the otherwise opaque
/// server spelling auditable. Aliases are never inferred by normalizing model
/// names; they must be declared on the runtime/artifact variant that owns them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServedModelAliasProvenance {
    pub model_id: String,
    pub runtime: String,
    pub hf_repo: String,
    pub quant: String,
}

impl EndpointAttestationState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Verified => "verified",
            Self::UnverifiedTimeout => "unverified_timeout",
        }
    }
}

impl ResolvedTargetProvenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitCatalog => "explicit_catalog",
            Self::ExplicitUrl => "explicit_url",
            Self::Auto => "auto",
        }
    }

    pub fn is_explicit(self) -> bool {
        !matches!(self, Self::Auto)
    }
}

/// Canonical endpoint identity resolved before inference starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedFleetTarget {
    /// Immutable identity of the canonical fleet_model_deployments row.
    #[serde(default)]
    pub deployment_id: sqlx::types::Uuid,
    pub endpoint: String,
    pub catalog_id: String,
    pub model_label: String,
    pub worker_name: String,
    pub provenance: ResolvedTargetProvenance,
    pub router_enabled: bool,
    /// Exact identities accepted from deployment, catalog, and library authority.
    pub accepted_model_ids: Vec<String>,
    /// Explicit catalog-variant aliases included in `accepted_model_ids`, with
    /// their runtime/artifact provenance retained for route-decision audits.
    #[serde(default)]
    pub accepted_model_aliases: Vec<ServedModelAliasProvenance>,
    /// Strict filename prefixes derived from catalog repo + quant metadata.
    /// They accept only a complete GGUF filename or a complete split-shard
    /// suffix, never a substring/fuzzy family match.
    pub accepted_shard_prefixes: Vec<String>,
    /// Exact ID returned by the live endpoint after successful attestation.
    pub served_model_id: Option<String>,
    /// Every ID returned by the endpoint, retained for audit evidence.
    pub served_model_ids: Vec<String>,
    pub attestation: EndpointAttestationState,
}

impl ResolvedFleetTarget {
    pub fn engine_label(&self) -> String {
        match (&self.served_model_id, self.attestation) {
            (Some(served), EndpointAttestationState::Verified) => {
                format!("local:{served}")
            }
            (_, EndpointAttestationState::UnverifiedTimeout) => {
                format!("local:unverified:{}", self.catalog_id)
            }
            _ => format!("local:unattested:{}", self.catalog_id),
        }
    }

    pub fn inference_model(&self) -> &str {
        self.served_model_id
            .as_deref()
            .unwrap_or(self.catalog_id.as_str())
    }

    pub fn route_decision(&self) -> Value {
        json!({
            "deployment_id": self.deployment_id,
            "endpoint": self.endpoint,
            "worker_name": self.worker_name,
            "catalog_id": self.catalog_id,
            "model_label": self.model_label,
            "provenance": self.provenance.as_str(),
            "router_enabled": self.router_enabled,
            "accepted_model_ids": self.accepted_model_ids,
            "accepted_model_aliases": self.accepted_model_aliases,
            "accepted_shard_prefixes": self.accepted_shard_prefixes,
            "served_model_id": self.served_model_id,
            "served_model_ids": self.served_model_ids,
            "attestation": self.attestation.as_str(),
        })
    }
}

/// Pure construction boundary: do not let a caller-supplied model string become
/// endpoint identity. A target is usable only when the catalog id is canonical.
pub fn resolved_target_from_candidate(
    candidate: &RouteCandidate,
    provenance: ResolvedTargetProvenance,
    router_enabled: bool,
) -> Result<ResolvedFleetTarget> {
    let catalog_id = candidate
        .catalog_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow!("resolved endpoint has no canonical catalog_id"))?;
    let model_label = candidate
        .catalog_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(catalog_id);
    Ok(ResolvedFleetTarget {
        deployment_id: candidate.deployment_id,
        endpoint: normalize_base_endpoint(&candidate.endpoint),
        catalog_id: catalog_id.to_string(),
        model_label: model_label.to_string(),
        worker_name: candidate.worker_name.clone(),
        provenance,
        router_enabled,
        accepted_model_ids: [catalog_id, model_label]
            .into_iter()
            .map(str::trim)
            .filter(|identity| !identity.is_empty())
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        accepted_model_aliases: Vec::new(),
        accepted_shard_prefixes: Vec::new(),
        served_model_id: None,
        served_model_ids: Vec::new(),
        attestation: EndpointAttestationState::Pending,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointAttestation {
    pub served_model_id: Option<String>,
    pub served_model_ids: Vec<String>,
    pub state: EndpointAttestationState,
}

/// Parse every model identity exposed by llama.cpp, vLLM, or Ollama-style
/// /v1/models payloads. A response with no recognized array is malformed;
/// a recognized but empty response is a reachable fail-closed condition.
pub fn parse_served_model_ids(payload: &Value) -> Result<Vec<String>> {
    let data = payload.get("data");
    let models = payload.get("models");
    if data.is_none() && models.is_none() {
        return Err(anyhow!(
            "model attestation response has no data/models array"
        ));
    }
    if data.is_some_and(|value| !value.is_array()) || models.is_some_and(|value| !value.is_array())
    {
        return Err(anyhow!(
            "model attestation response has malformed data/models"
        ));
    }

    let mut identities = BTreeSet::new();
    for item in data.and_then(Value::as_array).into_iter().flatten() {
        if let Some(identity) = item.get("id").and_then(Value::as_str) {
            let identity = identity.trim();
            if !identity.is_empty() {
                identities.insert(identity.to_string());
            }
        }
    }
    for item in models.and_then(Value::as_array).into_iter().flatten() {
        for field in ["name", "model"] {
            if let Some(identity) = item.get(field).and_then(Value::as_str) {
                let identity = identity.trim();
                if !identity.is_empty() {
                    identities.insert(identity.to_string());
                }
            }
        }
    }
    if identities.is_empty() {
        return Err(anyhow!("model attestation response contains no served IDs"));
    }
    Ok(identities.into_iter().collect())
}

/// Prove the live server identity before any chat request. Only an actual
/// timeout is allowed to continue as explicitly unverified; reachable errors
/// and identity mismatches fail closed.
pub async fn attest_endpoint(
    client: &reqwest::Client,
    endpoint: &str,
    accepted_ids: &BTreeSet<String>,
    accepted_shard_prefixes: &BTreeSet<String>,
    timeout: Duration,
) -> Result<EndpointAttestation> {
    if accepted_ids.is_empty() && accepted_shard_prefixes.is_empty() {
        return Err(anyhow!("model attestation has no accepted identities"));
    }
    let url = format!("{}/v1/models", normalize_base_endpoint(endpoint));
    let response = match client.get(&url).timeout(timeout).send().await {
        Ok(response) => response,
        Err(error) if error.is_timeout() => {
            return Ok(EndpointAttestation {
                served_model_id: None,
                served_model_ids: Vec::new(),
                state: EndpointAttestationState::UnverifiedTimeout,
            });
        }
        Err(error) => return Err(anyhow!("GET {url}: {error}")),
    };
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!("GET {url} returned HTTP {status}"));
    }
    let payload: Value = response
        .json()
        .await
        .map_err(|error| anyhow!("decode model attestation from {url}: {error}"))?;
    let served_model_ids = parse_served_model_ids(&payload)?;
    let served_model_id = served_model_ids
        .iter()
        .find(|identity| {
            accepted_ids.contains(*identity)
                || accepted_shard_prefixes
                    .iter()
                    .any(|prefix| matches_strict_gguf_identity(identity, prefix))
        })
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "endpoint {} serves {:?}, none of accepted identities {:?} or shard prefixes {:?}",
                normalize_base_endpoint(endpoint),
                served_model_ids,
                accepted_ids,
                accepted_shard_prefixes
            )
        })?;
    Ok(EndpointAttestation {
        served_model_id: Some(served_model_id),
        served_model_ids,
        state: EndpointAttestationState::Verified,
    })
}

pub async fn attest_resolved_target(
    client: &reqwest::Client,
    mut target: ResolvedFleetTarget,
    timeout: Duration,
) -> Result<ResolvedFleetTarget> {
    let accepted = target
        .accepted_model_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let accepted_shard_prefixes = target
        .accepted_shard_prefixes
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let attestation = attest_endpoint(
        client,
        &target.endpoint,
        &accepted,
        &accepted_shard_prefixes,
        timeout,
    )
    .await?;
    target.served_model_id = attestation.served_model_id;
    target.served_model_ids = attestation.served_model_ids;
    target.attestation = attestation.state;
    Ok(target)
}

fn matches_strict_gguf_identity(identity: &str, prefix: &str) -> bool {
    // llama.cpp commonly reports the absolute path of the loaded first shard.
    // Match only the final path component so directory names cannot satisfy the
    // catalog-derived prefix. The basename rule remains exact and case-sensitive.
    let Some(identity) = Path::new(identity.trim())
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    if identity == format!("{prefix}.gguf") {
        return true;
    }
    let Some(shard) = identity
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('-'))
        .and_then(|rest| rest.strip_suffix(".gguf"))
    else {
        return false;
    };
    let Some((part, total)) = shard.split_once("-of-") else {
        return false;
    };
    if part.len() != 5
        || total.len() != 5
        || !part.bytes().all(|byte| byte.is_ascii_digit())
        || !total.bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    let Ok(part) = part.parse::<u32>() else {
        return false;
    };
    let Ok(total) = total.parse::<u32>() else {
        return false;
    };
    part > 0 && total >= 2 && part <= total
}

fn shard_prefixes_from_variants(variants: &Value) -> BTreeSet<String> {
    let mut prefixes = BTreeSet::new();
    let Some(variants) = variants.as_array() else {
        return prefixes;
    };
    for variant in variants {
        let Some(repo) = variant
            .get("hf_repo")
            .and_then(Value::as_str)
            .and_then(|repo| repo.rsplit('/').next())
            .map(str::trim)
            .filter(|repo| !repo.is_empty())
        else {
            continue;
        };
        let Some(quant) = variant
            .get("quant")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|quant| !quant.is_empty())
        else {
            continue;
        };
        let base = repo
            .strip_suffix("-GGUF")
            .or_else(|| repo.strip_suffix("_GGUF"))
            .unwrap_or(repo);
        prefixes.insert(format!("{base}-{quant}"));
    }
    prefixes
}

const MAX_SERVED_MODEL_ALIAS_BYTES: usize = 1024;

/// Read exact aliases from the catalog variant matching the deployment runtime.
///
/// Metadata shape:
/// `{ "runtime": "llama.cpp", "hf_repo": "org/repo", "quant": "Q4_K_M",
///    "served_model_aliases": ["Exact-Server-ID.gguf"] }`
///
/// The source fields are mandatory whenever aliases are declared. Malformed or
/// ambiguous declarations fail the entire resolution instead of silently
/// weakening identity attestation. A missing deployment runtime accepts no
/// aliases. Matching is exact and case-sensitive after this parsing boundary.
fn served_model_aliases_from_variants(
    variants: &Value,
    deployment_runtime: Option<&str>,
) -> Result<Vec<ServedModelAliasProvenance>> {
    let Some(variants) = variants.as_array() else {
        return Err(anyhow!("catalog variants must be an array"));
    };
    let deployment_runtime = deployment_runtime
        .map(str::trim)
        .filter(|runtime| !runtime.is_empty());
    let mut aliases = BTreeMap::<String, ServedModelAliasProvenance>::new();

    for (index, variant) in variants.iter().enumerate() {
        let Some(raw_aliases) = variant.get("served_model_aliases") else {
            continue;
        };
        let raw_aliases = raw_aliases.as_array().ok_or_else(|| {
            anyhow!("catalog variant {index} served_model_aliases must be an array")
        })?;
        let source = |field: &str| -> Result<&str> {
            let value = variant.get(field).and_then(Value::as_str).ok_or_else(|| {
                anyhow!("catalog variant {index} with served_model_aliases requires string {field}")
            })?;
            if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
                return Err(anyhow!(
                    "catalog variant {index} has invalid alias provenance field {field}"
                ));
            }
            Ok(value)
        };
        let runtime = source("runtime")?;
        let hf_repo = source("hf_repo")?;
        let quant = source("quant")?;
        // Runtime labels are routing metadata and compare case-insensitively;
        // the served-model aliases themselves remain exact and case-sensitive.
        let runtime_matches =
            deployment_runtime.is_some_and(|deployed| deployed.eq_ignore_ascii_case(runtime));

        for (alias_index, raw_alias) in raw_aliases.iter().enumerate() {
            let alias = raw_alias.as_str().ok_or_else(|| {
                anyhow!(
                    "catalog variant {index} served_model_aliases[{alias_index}] must be a string"
                )
            })?;
            if alias.is_empty()
                || alias.trim() != alias
                || alias.len() > MAX_SERVED_MODEL_ALIAS_BYTES
                || alias.chars().any(char::is_control)
            {
                return Err(anyhow!(
                    "catalog variant {index} served_model_aliases[{alias_index}] is invalid"
                ));
            }
            if !runtime_matches {
                continue;
            }
            let provenance = ServedModelAliasProvenance {
                model_id: alias.to_string(),
                runtime: runtime.to_string(),
                hf_repo: hf_repo.to_string(),
                quant: quant.to_string(),
            };
            if let Some(existing) = aliases.insert(alias.to_string(), provenance.clone())
                && existing != provenance
            {
                return Err(anyhow!(
                    "served model alias {alias:?} has ambiguous catalog variant provenance"
                ));
            }
        }
    }
    Ok(aliases.into_values().collect())
}

const CATALOG_LIBRARY_IDENTITIES_SQL: &str = "SELECT file_path \
       FROM fleet_model_library \
      WHERE catalog_id = $1 \
        AND file_path IS NOT NULL \
        AND BTRIM(file_path) <> ''";

fn library_path_basenames(paths: impl IntoIterator<Item = String>) -> BTreeSet<String> {
    paths
        .into_iter()
        .filter_map(|path| {
            Path::new(path.trim())
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        })
        .collect()
}

/// Resolve a router candidate into the canonical endpoint/model identity used
/// by every local dispatch path. This enriches the candidate with exact local
/// library filenames from every worker holding the same canonical catalog id,
/// plus catalog shard identities. Cross-worker paths matter when one worker's
/// library row is a directory label while another records the exact GGUF name.
/// Identity never crosses catalog ids and matching remains exact. This does not
/// perform live `/v1/models` attestation; callers must pass the result through
/// [`attest_resolved_target`] immediately before chat dispatch.
pub async fn resolve_candidate_target(
    pool: &PgPool,
    candidate: &RouteCandidate,
    provenance: ResolvedTargetProvenance,
    router_enabled: bool,
) -> Result<ResolvedFleetTarget> {
    let mut target = resolved_target_from_candidate(candidate, provenance, router_enabled)?;
    let paths = sqlx::query_scalar::<_, String>(CATALOG_LIBRARY_IDENTITIES_SQL)
        .bind(&target.catalog_id)
        .fetch_all(pool)
        .await
        .map_err(|error| {
            anyhow!(
                "load accepted library identities for catalog {}: {error}",
                target.catalog_id
            )
        })?;

    let mut accepted = target
        .accepted_model_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    accepted.extend(library_path_basenames(paths));
    let variants = sqlx::query_scalar::<_, Value>(
        "SELECT COALESCE(variants, '[]'::jsonb) \
           FROM fleet_model_catalog \
          WHERE id = $1",
    )
    .bind(&target.catalog_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| {
        anyhow!(
            "load catalog variant identities for {}: {error}",
            target.catalog_id
        )
    })?
    .unwrap_or_else(|| json!([]));
    let aliases = served_model_aliases_from_variants(&variants, candidate.runtime.as_deref())?;
    accepted.extend(aliases.iter().map(|alias| alias.model_id.clone()));
    target.accepted_model_ids = accepted.into_iter().collect();
    target.accepted_model_aliases = aliases;
    target.accepted_shard_prefixes = shard_prefixes_from_variants(&variants)
        .into_iter()
        .collect();
    Ok(target)
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
    fleet_oneshot_for(pool, prompt, model_hint, timeout, None).await
}

/// Like [`fleet_oneshot`] but constrains the candidate pool to deployments that
/// declare `workload` in `preferred_workloads` (via the router's synonym
/// clusters). Pass `Some("code")` for codegen/review so a build can NEVER land on
/// a non-coder deployment (a 1.7B research SLM or a 500M video model marked
/// healthy) — those produce no valid diff and the item fails "no diff to check"
/// (root-caused 2026-07-28: 12 codegen calls hit Lucy-1.7B, others hit
/// SmolVLM2-500M-video). This is capability-based, NOT a hardcoded model list:
/// glm-4.5-air / devstral / qwen3-coder declare "code"; Lucy/SmolVLM do not, so
/// they are excluded automatically as the roster changes. `None` preserves the
/// old unfiltered behavior for council/chat/planner callers.
pub async fn fleet_oneshot_for(
    pool: &PgPool,
    prompt: &str,
    model_hint: Option<&str>,
    timeout: Option<Duration>,
    workload: Option<&str>,
) -> Result<FleetOneshot> {
    fleet_oneshot_for_ctx(
        pool, prompt, model_hint, timeout, workload, None, None, 4096,
    )
    .await
}

/// Like [`fleet_oneshot_for`] but requires each candidate's per-slot usable
/// context to be at least `min_ctx` tokens. Codegen passes
/// prompt-estimate + reasoning reserve + max_tokens so a reasoning coder
/// (glm-4.5-air) never receives a prompt its slot can't hold — the overflow
/// truncates the reply into unusable prose. When `workload` is set, both it and
/// the context floor are hard requirements; an empty capable pool returns an
/// error for explicit cloud escalation. Only unfiltered conversational callers
/// retain the historical best-effort retry without a context floor.
///
/// `system` carries the output contract as a SYSTEM message (reasoning coders
/// obey it far better than mid-user-message instructions — the difference
/// between clean diffs and prose preamble, lab-proven). `max_tokens` lets
/// callers with multi-block outputs exceed the 4096 default.
#[allow(clippy::too_many_arguments)]
pub async fn fleet_oneshot_for_ctx(
    pool: &PgPool,
    prompt: &str,
    model_hint: Option<&str>,
    timeout: Option<Duration>,
    workload: Option<&str>,
    min_ctx: Option<i32>,
    system: Option<&str>,
    max_tokens: u32,
) -> Result<FleetOneshot> {
    fleet_oneshot_for_ctx_with_target(
        pool, prompt, model_hint, timeout, workload, min_ctx, system, max_tokens, None,
    )
    .await
}

/// Exact-target variant used when the operator supplied an explicit endpoint
/// or catalog. It revalidates the immutable deployment immediately before the
/// request and never enters the automatic candidate/failover loop.
#[allow(clippy::too_many_arguments)]
pub async fn fleet_oneshot_for_ctx_with_target(
    pool: &PgPool,
    prompt: &str,
    model_hint: Option<&str>,
    timeout: Option<Duration>,
    workload: Option<&str>,
    min_ctx: Option<i32>,
    system: Option<&str>,
    max_tokens: u32,
    explicit_target: Option<&ResolvedFleetTarget>,
) -> Result<FleetOneshot> {
    let completion_budget = CompletionBudget::new(max_tokens)
        .map_err(|error| anyhow!("invalid fleet one-shot completion budget: {error}"))?;
    let completion_workload = completion_workload_for_route(workload);
    let client = reqwest::Client::builder()
        .timeout(timeout.unwrap_or(Duration::from_secs(180)))
        .build()
        .map_err(|e| anyhow!("build http client: {e}"))?;

    if let Some(target) = explicit_target {
        revalidate_explicit_target(pool, target, workload, false, min_ctx).await?;
        let _guard = InFlightGuard::acquire(&target.endpoint);
        return dispatch_to_resolved_target(
            target.clone(),
            &client,
            prompt,
            system,
            completion_workload,
            completion_budget,
        )
        .await;
    }

    let ordered = resolve_route_candidates(pool, model_hint, workload, min_ctx).await?;

    let mut last_err: Option<anyhow::Error> = None;
    let mut attempted = false;

    // First pass: honour parallel_slots caps.
    for cand in &ordered {
        let slots = cand.parallel_slots.unwrap_or(1).max(1) as u32;
        let Some(_guard) = InFlightGuard::try_acquire(&cand.endpoint, slots) else {
            continue;
        };
        attempted = true;
        match dispatch_to_candidate(
            pool,
            cand,
            &client,
            prompt,
            model_hint,
            system,
            completion_workload,
            completion_budget,
        )
        .await
        {
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
            match dispatch_to_candidate(
                pool,
                cand,
                &client,
                prompt,
                model_hint,
                system,
                completion_workload,
                completion_budget,
            )
            .await
            {
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
    resolve_route_candidates(pool, Some(model_hint), None, None)
        .await?
        .into_iter()
        .find(|candidate| candidate_catalog_id_matches(candidate, model_hint))
        .ok_or_else(|| anyhow!("no healthy fleet deployment matches local:{model_hint}"))
}

pub async fn resolve_explicit_catalog_target(
    pool: &PgPool,
    catalog_id: &str,
) -> Result<ResolvedFleetTarget> {
    let candidate = resolve_route_candidate(pool, catalog_id).await?;
    resolve_candidate_target(
        pool,
        &candidate,
        ResolvedTargetProvenance::ExplicitCatalog,
        false,
    )
    .await
}

pub async fn resolve_auto_agent_target(pool: &PgPool, min_ctx: i32) -> Result<ResolvedFleetTarget> {
    let filter = RouteFilter {
        workload: None,
        require_tool_calling: true,
        min_ctx: Some(min_ctx),
        exclude_hosts: Vec::new(),
        max_health_age_sec: Some(180),
        prefer_least_loaded: true,
        limit: 8,
    };
    let candidate = pg_route_deployments(pool, &filter)
        .await
        .map_err(|e| anyhow!("route auto agent deployment: {e}"))?
        .into_iter()
        .find(has_model_name)
        .ok_or_else(|| anyhow!("no healthy canonical agent-capable fleet deployment"))?;
    resolve_candidate_target(pool, &candidate, ResolvedTargetProvenance::Auto, false).await
}

pub async fn resolve_endpoint_target(
    pool: &PgPool,
    endpoint: &str,
    requested_catalog_id: Option<&str>,
) -> Result<ResolvedFleetTarget> {
    let normalized = normalize_base_endpoint(endpoint);
    let filter = RouteFilter {
        workload: None,
        require_tool_calling: false,
        min_ctx: None,
        exclude_hosts: Vec::new(),
        max_health_age_sec: Some(180),
        prefer_least_loaded: false,
        limit: 128,
    };
    let mut matches = pg_route_deployments(pool, &filter)
        .await
        .map_err(|e| anyhow!("route explicit endpoint deployment: {e}"))?
        .into_iter()
        .filter(|candidate| normalize_base_endpoint(&candidate.endpoint) == normalized)
        .collect::<Vec<_>>();
    let Some(candidate) = matches.pop() else {
        return Err(anyhow!(
            "explicit --llm endpoint {normalized} does not match a healthy canonical fleet deployment"
        ));
    };
    if !matches.is_empty() {
        return Err(anyhow!(
            "explicit --llm endpoint {normalized} is ambiguous in fleet_model_deployments"
        ));
    }
    let target = resolve_candidate_target(
        pool,
        &candidate,
        ResolvedTargetProvenance::ExplicitUrl,
        false,
    )
    .await?;
    if let Some(requested) = requested_catalog_id
        && !resolved_target_accepts_request(&target, requested)
    {
        return Err(anyhow!(
            "explicit --llm endpoint {normalized} resolves catalog_id {}, accepted identities {:?} and shard prefixes {:?}, not requested model {requested}",
            target.catalog_id,
            target.accepted_model_ids,
            target.accepted_shard_prefixes
        ));
    }
    Ok(target)
}

/// Revalidate an explicitly pinned deployment immediately before dispatch.
///
/// The immutable deployment UUID, endpoint, worker, and catalog must still
/// identify the same canonical row, and that row must satisfy every requested
/// capability and context constraint. Unlike automatic routing this never
/// relaxes a constraint and never substitutes another deployment.
pub async fn revalidate_explicit_target(
    pool: &PgPool,
    target: &ResolvedFleetTarget,
    workload: Option<&str>,
    require_tool_calling: bool,
    min_ctx: Option<i32>,
) -> Result<RouteCandidate> {
    if !target.provenance.is_explicit() {
        return Err(anyhow!(
            "exact-target validation requires explicit provenance, got {}",
            target.provenance.as_str()
        ));
    }
    if target.deployment_id.is_nil() {
        return Err(anyhow!(
            "explicit target {} has no immutable deployment id",
            target.endpoint
        ));
    }

    let filter = explicit_target_filter(workload, require_tool_calling, min_ctx);
    let candidates = pg_route_deployments(pool, &filter)
        .await
        .map_err(|error| anyhow!("revalidate pinned deployment: {error}"))?;
    let candidate = candidates
        .into_iter()
        .find(|candidate| candidate.deployment_id == target.deployment_id)
        .ok_or_else(|| {
            anyhow!(
                "pinned deployment {} is unavailable or ineligible: required health=healthy/fresh<=180s, workload={workload:?}, tool_calling={require_tool_calling}, min_ctx={min_ctx:?}",
                target.deployment_id
            )
        })?;

    validate_candidate_identity(&candidate, target)?;
    Ok(candidate)
}

fn explicit_target_filter(
    workload: Option<&str>,
    require_tool_calling: bool,
    min_ctx: Option<i32>,
) -> RouteFilter {
    RouteFilter {
        workload: workload.map(str::to_string),
        require_tool_calling,
        min_ctx,
        exclude_hosts: Vec::new(),
        max_health_age_sec: Some(180),
        prefer_least_loaded: false,
        // This is an identity lookup after capability filtering, not a top-N
        // routing choice. Keep the requested deployment from being truncated.
        limit: 10_000,
    }
}

fn validate_candidate_identity(
    candidate: &RouteCandidate,
    target: &ResolvedFleetTarget,
) -> Result<()> {
    let candidate_catalog = candidate.catalog_id.as_deref().unwrap_or_default();
    if candidate.deployment_id != target.deployment_id
        || normalize_base_endpoint(&candidate.endpoint) != normalize_base_endpoint(&target.endpoint)
        || !candidate_catalog.eq_ignore_ascii_case(&target.catalog_id)
        || !candidate
            .worker_name
            .eq_ignore_ascii_case(&target.worker_name)
    {
        return Err(anyhow!(
            "pinned deployment {} changed identity: expected worker={} endpoint={} catalog={}, now worker={} endpoint={} catalog={}",
            target.deployment_id,
            target.worker_name,
            target.endpoint,
            target.catalog_id,
            candidate.worker_name,
            candidate.endpoint,
            candidate_catalog
        ));
    }
    Ok(())
}

fn resolved_target_accepts_request(target: &ResolvedFleetTarget, requested: &str) -> bool {
    target.catalog_id.eq_ignore_ascii_case(requested)
        || target
            .accepted_model_ids
            .iter()
            .any(|identity| identity == requested)
        || target
            .accepted_shard_prefixes
            .iter()
            .any(|prefix| matches_strict_gguf_identity(requested, prefix))
}

pub fn normalize_base_endpoint(endpoint: &str) -> String {
    let trimmed = endpoint.trim().trim_end_matches('/');
    let trimmed = trimmed
        .strip_suffix("/v1/chat/completions")
        .unwrap_or(trimmed)
        .trim_end_matches('/');
    let trimmed = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    trimmed.trim_end_matches('/').to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmptyRoutePolicy {
    RelaxUnfilteredContext,
    FailRequiredWorkload,
    FailUnavailable,
}

fn empty_route_policy(workload: Option<&str>, min_ctx: Option<i32>) -> EmptyRoutePolicy {
    if workload.is_some() {
        EmptyRoutePolicy::FailRequiredWorkload
    } else if min_ctx.is_some() {
        EmptyRoutePolicy::RelaxUnfilteredContext
    } else {
        EmptyRoutePolicy::FailUnavailable
    }
}

async fn resolve_route_candidates(
    pool: &PgPool,
    model_hint: Option<&str>,
    workload: Option<&str>,
    min_ctx: Option<i32>,
) -> Result<Vec<RouteCandidate>> {
    let filter = RouteFilter {
        // Capability filter: when set (e.g. "code" for codegen/review), the router
        // OR-matches the workload's synonym cluster against each deployment's
        // `preferred_workloads`, so only code-capable models are candidates.
        workload: workload.map(|w| w.to_string()),
        require_tool_calling: false,
        // Per-slot context floor: reasoning models (glm-4.5-air) need
        // prompt + think + max_tokens to FIT the slot, or the reply truncates
        // into unusable prose (the 2026-07-29 canary-2 failure on thalia's
        // 12K slots with a fat repo-context prompt).
        min_ctx,
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
    // Unfiltered conversational/council callers retain the historical
    // best-effort context fallback. A requested workload (especially code) is
    // a hard capability contract: never discard either its workload or its
    // context floor merely to find some endpoint.
    if all_candidates.is_empty() {
        match empty_route_policy(workload, min_ctx) {
            EmptyRoutePolicy::RelaxUnfilteredContext => {
                tracing::warn!(
                    min_ctx,
                    "fleet_oneshot: no unfiltered deployment satisfies the ctx floor — retrying without it"
                );
                return Box::pin(resolve_route_candidates(pool, model_hint, workload, None)).await;
            }
            EmptyRoutePolicy::FailRequiredWorkload => {
                return Err(anyhow!(
                    "no healthy fleet deployment satisfies required workload {:?} and min_ctx {min_ctx:?}",
                    workload.unwrap_or_default()
                ));
            }
            EmptyRoutePolicy::FailUnavailable => {}
        }
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
    pool: &PgPool,
    cand: &RouteCandidate,
    client: &reqwest::Client,
    prompt: &str,
    _model_hint: Option<&str>,
    system: Option<&str>,
    workload: WorkloadClass,
    budget: CompletionBudget,
) -> anyhow::Result<FleetOneshot> {
    let target =
        resolve_candidate_target(pool, cand, ResolvedTargetProvenance::Auto, false).await?;
    dispatch_to_resolved_target(target, client, prompt, system, workload, budget).await
}

async fn dispatch_to_resolved_target(
    target: ResolvedFleetTarget,
    client: &reqwest::Client,
    prompt: &str,
    system: Option<&str>,
    workload: WorkloadClass,
    budget: CompletionBudget,
) -> anyhow::Result<FleetOneshot> {
    let target = attest_resolved_target(client, target, Duration::from_secs(5)).await?;
    enforce_dispatch_attestation(&target)?;
    let worker_name = target.worker_name.clone();
    let endpoint = target.endpoint.clone();
    let catalog_id = Some(target.catalog_id.clone());
    let model = target.inference_model().to_string();
    let url = ff_core::url::normalize_chat_completions_url(&endpoint);
    // Reasoning coders (glm-4.5-air) start "thinking out loud" in `content`
    // when the format contract lives mid-user-message — and the prose eats
    // the completion budget before any edit block is emitted (canary-3,
    // 2026-07-29). A strict SYSTEM message carries the contract instead
    // (lab-proven on thalia: first-try clean diffs).
    let messages = match system {
        Some(sys) => json!([
            {"role": "system", "content": sys},
            {"role": "user", "content": prompt},
        ]),
        None => json!([{"role": "user", "content": prompt}]),
    };
    let body = completion_request_body(&model, messages, workload, budget)?;
    let start = std::time::Instant::now();

    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("POST {url}: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        // Provider bodies can echo prompts or private reasoning. Keep only
        // endpoint identity and status in errors/logs.
        return Err(anyhow!("{worker_name} ({model}) returned HTTP {status}"));
    }
    let payload: Value = resp
        .json()
        .await
        .map_err(|e| anyhow!("decode response from {worker_name}: {e}"))?;
    let text = validate_completion_response(&payload)
        .map_err(|error| anyhow!("{worker_name} ({model}) returned unsafe completion: {error}"))?
        .content;
    let (tokens_in, tokens_out) = usage_tokens_i32(&payload);
    Ok(FleetOneshot {
        text,
        deployment_id: target.deployment_id,
        endpoint: endpoint.clone(),
        worker_name: worker_name.clone(),
        catalog_id,
        model: model.clone(),
        provenance: target.provenance,
        latency_ms: start.elapsed().as_millis(),
        tokens_in,
        tokens_out,
    })
}

fn enforce_dispatch_attestation(target: &ResolvedFleetTarget) -> Result<()> {
    if target.provenance.is_explicit() && target.attestation != EndpointAttestationState::Verified {
        return Err(anyhow!(
            "explicit target {} could not be identity-attested ({})",
            target.endpoint,
            target.attestation.as_str()
        ));
    }
    Ok(())
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

fn completion_workload_for_route(workload: Option<&str>) -> WorkloadClass {
    if workload.is_some_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "code" | "codegen" | "coding" | "review" | "reviewer"
        )
    }) {
        WorkloadClass::CodeOneShot
    } else {
        WorkloadClass::Reasoning
    }
}

fn completion_request_body(
    model: &str,
    messages: Value,
    workload: WorkloadClass,
    budget: CompletionBudget,
) -> anyhow::Result<Value> {
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": false,
        "temperature": 0.2,
    });
    apply_completion_policy(&mut body, workload, budget)
        .map_err(|error| anyhow!("completion request policy rejected request: {error}"))?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn candidate(
        endpoint: &str,
        worker: &str,
        family: Option<&str>,
        slots: Option<i32>,
    ) -> RouteCandidate {
        RouteCandidate {
            deployment_id: sqlx::types::Uuid::nil(),
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
    fn normalizes_base_endpoint_forms() {
        assert_eq!(
            normalize_base_endpoint("http://192.168.5.111:55004/"),
            "http://192.168.5.111:55004"
        );
        assert_eq!(
            normalize_base_endpoint("http://192.168.5.111:55004/v1"),
            "http://192.168.5.111:55004"
        );
        assert_eq!(
            normalize_base_endpoint("http://192.168.5.111:55004/v1/chat/completions"),
            "http://192.168.5.111:55004"
        );
    }

    #[test]
    fn explicit_target_filter_preserves_every_hard_constraint() {
        let filter = explicit_target_filter(Some("code"), true, Some(32_768));
        assert_eq!(filter.workload.as_deref(), Some("code"));
        assert!(filter.require_tool_calling);
        assert_eq!(filter.min_ctx, Some(32_768));
        assert_eq!(filter.max_health_age_sec, Some(180));
        assert!(!filter.prefer_least_loaded);
        assert!(filter.limit >= 10_000);
    }

    #[test]
    fn explicit_target_identity_rejects_spoofed_or_replaced_deployment() {
        let mut original = candidate(
            "http://192.168.5.122:55008",
            "thalia",
            Some("qwen"),
            Some(1),
        );
        original.deployment_id = sqlx::types::Uuid::new_v4();
        original.catalog_id = Some("qwen3-coder-30b".to_string());
        original.catalog_name = Some("Qwen3 Coder 30B".to_string());
        let target =
            resolved_target_from_candidate(&original, ResolvedTargetProvenance::ExplicitUrl, false)
                .unwrap();
        validate_candidate_identity(&original, &target).unwrap();

        let mut replaced = original.clone();
        replaced.deployment_id = sqlx::types::Uuid::new_v4();
        assert!(validate_candidate_identity(&replaced, &target).is_err());

        let mut spoofed = original.clone();
        spoofed.endpoint = "http://192.168.5.119:55000".to_string();
        assert!(validate_candidate_identity(&spoofed, &target).is_err());

        let mut relabeled = original.clone();
        relabeled.catalog_id = Some("glm-4.5-air".to_string());
        assert!(validate_candidate_identity(&relabeled, &target).is_err());
    }

    #[test]
    fn explicit_dispatch_requires_verified_attestation_while_auto_keeps_timeout_policy() {
        let mut original = candidate(
            "http://192.168.5.122:55008",
            "thalia",
            Some("qwen"),
            Some(1),
        );
        original.deployment_id = sqlx::types::Uuid::new_v4();
        original.catalog_id = Some("qwen3-coder-30b".to_string());

        let mut explicit =
            resolved_target_from_candidate(&original, ResolvedTargetProvenance::ExplicitUrl, false)
                .unwrap();
        explicit.attestation = EndpointAttestationState::UnverifiedTimeout;
        assert!(enforce_dispatch_attestation(&explicit).is_err());
        explicit.attestation = EndpointAttestationState::Verified;
        enforce_dispatch_attestation(&explicit).unwrap();

        let mut automatic =
            resolved_target_from_candidate(&original, ResolvedTargetProvenance::Auto, false)
                .unwrap();
        automatic.attestation = EndpointAttestationState::UnverifiedTimeout;
        enforce_dispatch_attestation(&automatic).unwrap();
    }

    #[test]
    fn parses_full_served_identity_union_and_deduplicates() {
        let payload = json!({
            "data": [{"id": "wrong.gguf"}, {"id": "right.gguf"}],
            "models": [
                {"name": "right.gguf", "model": "right.gguf"},
                {"name": "also.gguf"}
            ]
        });
        assert_eq!(
            parse_served_model_ids(&payload).unwrap(),
            vec!["also.gguf", "right.gguf", "wrong.gguf"]
        );
    }

    #[test]
    fn malformed_or_empty_served_identity_payload_fails_closed() {
        assert!(parse_served_model_ids(&json!({})).is_err());
        assert!(parse_served_model_ids(&json!({"data": "not-an-array"})).is_err());
        assert!(parse_served_model_ids(&json!({"data": [], "models": []})).is_err());
    }

    #[test]
    fn catalog_variants_derive_only_strict_complete_gguf_prefixes() {
        let variants = json!([
            {
                "hf_repo": "bartowski/zai-org_GLM-4.5-Air-GGUF",
                "quant": "Q4_K_M"
            },
            {
                "hf_repo": "unsloth/Devstral-Small-2-24B-Instruct-2512-GGUF",
                "quant": "UD-Q4_K_XL"
            }
        ]);
        assert_eq!(
            shard_prefixes_from_variants(&variants),
            BTreeSet::from([
                "Devstral-Small-2-24B-Instruct-2512-UD-Q4_K_XL".to_string(),
                "zai-org_GLM-4.5-Air-Q4_K_M".to_string(),
            ])
        );
        assert!(matches_strict_gguf_identity(
            "zai-org_GLM-4.5-Air-Q4_K_M-00001-of-00002.gguf",
            "zai-org_GLM-4.5-Air-Q4_K_M"
        ));
        assert!(matches_strict_gguf_identity(
            "/home/shakira/models/llama-cpp/glm-4.5-air/zai-org_GLM-4.5-Air-Q4_K_M/zai-org_GLM-4.5-Air-Q4_K_M-00001-of-00002.gguf",
            "zai-org_GLM-4.5-Air-Q4_K_M"
        ));
        assert!(matches_strict_gguf_identity(
            "Devstral-Small-2-24B-Instruct-2512-UD-Q4_K_XL.gguf",
            "Devstral-Small-2-24B-Instruct-2512-UD-Q4_K_XL"
        ));
        assert!(!matches_strict_gguf_identity(
            "Lucy-Q4_K_M.gguf",
            "zai-org_GLM-4.5-Air-Q4_K_M"
        ));
        assert!(!matches_strict_gguf_identity(
            "zai-org_GLM-4.5-Air-Q4_K_M-extra.gguf",
            "zai-org_GLM-4.5-Air-Q4_K_M"
        ));
        for rejected in [
            "/models/zai-org_GLM-4.5-Air-Q4_K_M/unrelated.gguf",
            "evil-zai-org_GLM-4.5-Air-Q4_K_M-00001-of-00002.gguf",
            "zai-org_GLM-4.5-Air-Q5_K_M-00001-of-00002.gguf",
            "zai-org_GLM-4.5-Air-Q4_K_M-0001-of-0002.gguf",
            "zai-org_GLM-4.5-Air-Q4_K_M-00000-of-00002.gguf",
            "zai-org_GLM-4.5-Air-Q4_K_M-00003-of-00002.gguf",
            "zai-org_GLM-4.5-Air-Q4_K_M-00001-of-00001.gguf",
            "zai-org_GLM-4.5-Air-Q4_K_M-00001-of-00002.gguf.bak",
        ] {
            assert!(
                !matches_strict_gguf_identity(rejected, "zai-org_GLM-4.5-Air-Q4_K_M"),
                "unexpectedly accepted {rejected}"
            );
        }
    }

    #[test]
    fn catalog_variant_alias_is_exact_runtime_scoped_and_provenance_backed() {
        let variants = json!([
            {
                "runtime": "llama.cpp",
                "hf_repo": "Qwen/Qwen3-VL-30B-A3B-Instruct-GGUF",
                "quant": "Q4_K_M",
                "served_model_aliases": [
                    "Qwen3VL-30B-A3B-Instruct-Q4_K_M.gguf"
                ]
            },
            {
                "runtime": "vllm",
                "hf_repo": "Qwen/Qwen3-VL-30B-A3B-Instruct",
                "quant": "fp16",
                "served_model_aliases": ["qwen3-vl-vllm"]
            }
        ]);
        let aliases = served_model_aliases_from_variants(&variants, Some("llama.cpp")).unwrap();
        assert_eq!(
            aliases,
            vec![ServedModelAliasProvenance {
                model_id: "Qwen3VL-30B-A3B-Instruct-Q4_K_M.gguf".to_string(),
                runtime: "llama.cpp".to_string(),
                hf_repo: "Qwen/Qwen3-VL-30B-A3B-Instruct-GGUF".to_string(),
                quant: "Q4_K_M".to_string(),
            }]
        );
        assert!(
            served_model_aliases_from_variants(&variants, None)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            served_model_aliases_from_variants(&variants, Some("vllm")).unwrap()[0].model_id,
            "qwen3-vl-vllm"
        );
    }

    #[test]
    fn catalog_variant_aliases_never_infer_punctuation_or_cross_variant_runtime() {
        let no_alias = json!([{
            "runtime": "llama.cpp",
            "hf_repo": "Qwen/Qwen3-VL-30B-A3B-Instruct-GGUF",
            "quant": "Q4_K_M"
        }]);
        assert!(
            served_model_aliases_from_variants(&no_alias, Some("llama.cpp"))
                .unwrap()
                .is_empty()
        );

        let vllm_only = json!([{
            "runtime": "vllm",
            "hf_repo": "Qwen/Qwen3-VL-30B-A3B-Instruct",
            "quant": "fp16",
            "served_model_aliases": ["Qwen3VL-30B-A3B-Instruct-Q4_K_M.gguf"]
        }]);
        assert!(
            served_model_aliases_from_variants(&vllm_only, Some("llama.cpp"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn malformed_or_ambiguous_catalog_variant_aliases_fail_closed() {
        for variants in [
            json!({}),
            json!([{
                "runtime": "llama.cpp",
                "hf_repo": "Qwen/repo",
                "quant": "Q4_K_M",
                "served_model_aliases": "not-an-array"
            }]),
            json!([{
                "runtime": "llama.cpp",
                "hf_repo": "Qwen/repo",
                "quant": "Q4_K_M",
                "served_model_aliases": [" padded.gguf"]
            }]),
            json!([{
                "runtime": "llama.cpp",
                "hf_repo": "Qwen/repo",
                "quant": "Q4_K_M",
                "served_model_aliases": [42]
            }]),
            json!([{
                "runtime": "llama.cpp",
                "hf_repo": "Qwen/repo",
                "served_model_aliases": ["exact.gguf"]
            }]),
            json!([
                {
                    "runtime": "llama.cpp",
                    "hf_repo": "Qwen/repo-a",
                    "quant": "Q4_K_M",
                    "served_model_aliases": ["same.gguf"]
                },
                {
                    "runtime": "llama.cpp",
                    "hf_repo": "Qwen/repo-b",
                    "quant": "Q4_K_M",
                    "served_model_aliases": ["same.gguf"]
                }
            ]),
        ] {
            assert!(
                served_model_aliases_from_variants(&variants, Some("llama.cpp")).is_err(),
                "unexpectedly accepted {variants}"
            );
        }
    }

    #[tokio::test]
    async fn james_qwen_alias_attests_exactly_without_fuzzy_spelling() {
        let exact = "Qwen3VL-30B-A3B-Instruct-Q4_K_M.gguf";
        let (endpoint, chat_calls, server) =
            spawn_attestation_server(json!({"data": [{"id": exact}]}), Duration::ZERO).await;
        let variants = json!([{
            "runtime": "llama.cpp",
            "hf_repo": "Qwen/Qwen3-VL-30B-A3B-Instruct-GGUF",
            "quant": "Q4_K_M",
            "served_model_aliases": [exact]
        }]);
        let aliases = served_model_aliases_from_variants(&variants, Some("llama.cpp")).unwrap();
        let accepted = aliases
            .iter()
            .map(|alias| alias.model_id.clone())
            .collect::<BTreeSet<_>>();
        let result = attest_endpoint(
            &reqwest::Client::new(),
            &endpoint,
            &accepted,
            &BTreeSet::new(),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(result.served_model_id.as_deref(), Some(exact));
        assert!(!accepted.contains("Qwen3-VL-30B-A3B-Instruct-Q4_K_M.gguf"));
        assert!(!accepted.contains("qwen3vl-30b-a3b-instruct-q4_k_m.gguf"));
        assert_eq!(chat_calls.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[test]
    fn catalog_library_identity_authority_is_cross_worker_but_exact() {
        assert!(CATALOG_LIBRARY_IDENTITIES_SQL.contains("catalog_id = $1"));
        assert!(!CATALOG_LIBRARY_IDENTITIES_SQL.contains("worker_name"));

        let basenames = library_path_basenames([
            "/home/logan/models/lucy-1.7b".to_string(),
            "/home/aura/models/Lucy-Q4_K_M.gguf".to_string(),
            "  /models/Devstral-Exact.gguf  ".to_string(),
            "   ".to_string(),
        ]);
        assert_eq!(
            basenames,
            BTreeSet::from([
                "Devstral-Exact.gguf".to_string(),
                "Lucy-Q4_K_M.gguf".to_string(),
                "lucy-1.7b".to_string(),
            ])
        );
        assert!(basenames.contains("Lucy-Q4_K_M.gguf"));
        assert!(!basenames.contains("Lucy"));
        assert!(!basenames.contains("Lucy-Q4_K_M-extra.gguf"));
    }

    async fn spawn_attestation_server(
        payload: Value,
        models_delay: Duration,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let chat_calls = Arc::new(AtomicUsize::new(0));
        let models_payload = payload.clone();
        let models = move || {
            let payload = models_payload.clone();
            async move {
                tokio::time::sleep(models_delay).await;
                axum::Json(payload)
            }
        };
        let chat_counter = chat_calls.clone();
        let chat = move || {
            let chat_counter = chat_counter.clone();
            async move {
                chat_counter.fetch_add(1, Ordering::SeqCst);
                axum::Json(json!({"choices": [{"message": {"content": "unexpected"}}]}))
            }
        };
        let app = axum::Router::new()
            .route("/v1/models", axum::routing::get(models))
            .route("/v1/chat/completions", axum::routing::post(chat));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (endpoint, chat_calls, server)
    }

    #[tokio::test]
    async fn attestation_matches_any_exact_served_id_not_only_last() {
        let (endpoint, chat_calls, server) = spawn_attestation_server(
            json!({"data": [{"id": "right.gguf"}, {"id": "wrong.gguf"}]}),
            Duration::ZERO,
        )
        .await;
        let accepted = BTreeSet::from(["right.gguf".to_string()]);
        let result = attest_endpoint(
            &reqwest::Client::new(),
            &endpoint,
            &accepted,
            &BTreeSet::new(),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(result.served_model_id.as_deref(), Some("right.gguf"));
        assert_eq!(result.state, EndpointAttestationState::Verified);
        assert_eq!(chat_calls.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[tokio::test]
    async fn attestation_accepts_exact_split_gguf_basename_from_absolute_path() {
        let served = concat!(
            "/home/shakira/models/llama-cpp/glm-4.5-air/",
            "zai-org_GLM-4.5-Air-Q4_K_M/",
            "zai-org_GLM-4.5-Air-Q4_K_M-00001-of-00002.gguf"
        );
        let (endpoint, chat_calls, server) =
            spawn_attestation_server(json!({"data": [{"id": served}]}), Duration::ZERO).await;
        let result = attest_endpoint(
            &reqwest::Client::new(),
            &endpoint,
            &BTreeSet::new(),
            &BTreeSet::from(["zai-org_GLM-4.5-Air-Q4_K_M".to_string()]),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(result.served_model_id.as_deref(), Some(served));
        assert_eq!(result.state, EndpointAttestationState::Verified);
        assert_eq!(chat_calls.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[tokio::test]
    async fn lucy_mismatch_fails_before_any_chat_request() {
        let (endpoint, chat_calls, server) = spawn_attestation_server(
            json!({"data": [{"id": "Lucy-Q4_K_M.gguf"}]}),
            Duration::ZERO,
        )
        .await;
        let accepted =
            BTreeSet::from(["Devstral-Small-2-24B-Instruct-2512-UD-Q4_K_XL.gguf".to_string()]);
        let error = attest_endpoint(
            &reqwest::Client::new(),
            &endpoint,
            &accepted,
            &BTreeSet::new(),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("Lucy-Q4_K_M.gguf"), "got: {error}");
        assert_eq!(chat_calls.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[tokio::test]
    async fn only_timeout_can_continue_as_explicitly_unverified() {
        let (endpoint, chat_calls, server) = spawn_attestation_server(
            json!({"data": [{"id": "right.gguf"}]}),
            Duration::from_millis(100),
        )
        .await;
        let accepted = BTreeSet::from(["right.gguf".to_string()]);
        let result = attest_endpoint(
            &reqwest::Client::new(),
            &endpoint,
            &accepted,
            &BTreeSet::new(),
            Duration::from_millis(5),
        )
        .await
        .unwrap();
        assert_eq!(result.state, EndpointAttestationState::UnverifiedTimeout);
        assert!(result.served_model_id.is_none());
        assert_eq!(chat_calls.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[test]
    fn resolved_target_requires_canonical_catalog_id() {
        let candidate = candidate("http://logan:55004", "logan", None, Some(1));
        let err = resolved_target_from_candidate(&candidate, ResolvedTargetProvenance::Auto, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("canonical catalog_id"), "got: {err}");
    }

    #[test]
    fn resolved_target_uses_catalog_identity_not_caller_label() {
        let mut candidate = candidate("http://logan:55004/", "logan", Some("slm"), Some(1));
        candidate.catalog_id = Some("lucy-1-7b".to_string());
        candidate.catalog_name = Some("Lucy 1.7B".to_string());

        let target =
            resolved_target_from_candidate(&candidate, ResolvedTargetProvenance::Auto, false)
                .unwrap();
        assert_eq!(target.endpoint, "http://logan:55004");
        assert_eq!(target.catalog_id, "lucy-1-7b");
        assert_eq!(target.model_label, "Lucy 1.7B");
        assert_eq!(target.engine_label(), "local:unattested:lucy-1-7b");
        assert_eq!(target.route_decision()["provenance"], "auto");
    }

    #[test]
    fn route_decision_retains_exact_alias_provenance() {
        let mut candidate = candidate("http://james:55003", "james", Some("qwen"), Some(1));
        candidate.catalog_id = Some("qwen3-vl-30b-a3b".to_string());
        candidate.catalog_name = Some("Qwen3-VL-30B-A3B-Instruct".to_string());
        let mut target =
            resolved_target_from_candidate(&candidate, ResolvedTargetProvenance::Auto, false)
                .unwrap();
        target
            .accepted_model_ids
            .push("Qwen3VL-30B-A3B-Instruct-Q4_K_M.gguf".to_string());
        target.accepted_model_aliases = vec![ServedModelAliasProvenance {
            model_id: "Qwen3VL-30B-A3B-Instruct-Q4_K_M.gguf".to_string(),
            runtime: "llama.cpp".to_string(),
            hf_repo: "Qwen/Qwen3-VL-30B-A3B-Instruct-GGUF".to_string(),
            quant: "Q4_K_M".to_string(),
        }];

        let decision = target.route_decision();
        assert_eq!(
            decision["accepted_model_aliases"][0]["model_id"],
            "Qwen3VL-30B-A3B-Instruct-Q4_K_M.gguf"
        );
        assert_eq!(
            decision["accepted_model_aliases"][0]["hf_repo"],
            "Qwen/Qwen3-VL-30B-A3B-Instruct-GGUF"
        );
        assert_eq!(decision["accepted_model_aliases"][0]["quant"], "Q4_K_M");
    }

    #[test]
    fn explicit_model_matching_is_exact_catalog_id_only() {
        let mut candidate = candidate("http://glm:55008", "glm", Some("glm"), Some(1));
        candidate.catalog_id = Some("glm-4.5-air".to_string());
        candidate.catalog_name = Some("GLM 4.5 Air".to_string());

        assert!(candidate_catalog_id_matches(&candidate, "glm-4.5-air"));
        assert!(candidate_catalog_id_matches(&candidate, "GLM-4.5-AIR"));
        assert!(!candidate_catalog_id_matches(&candidate, "glm"));
        assert!(!candidate_catalog_id_matches(&candidate, "GLM 4.5 Air"));
    }

    #[test]
    fn explicit_endpoint_request_accepts_exact_file_or_complete_shard_only() {
        let mut candidate = candidate("http://glm:55008", "glm", Some("glm"), Some(1));
        candidate.catalog_id = Some("glm-4.5-air".to_string());
        candidate.catalog_name = Some("GLM-4.5-Air".to_string());
        let mut target = resolved_target_from_candidate(
            &candidate,
            ResolvedTargetProvenance::ExplicitUrl,
            false,
        )
        .unwrap();
        target
            .accepted_model_ids
            .push("exact-model-file.gguf".to_string());
        target
            .accepted_shard_prefixes
            .push("zai-org_GLM-4.5-Air-Q4_K_M".to_string());

        assert!(resolved_target_accepts_request(&target, "glm-4.5-air"));
        assert!(resolved_target_accepts_request(
            &target,
            "exact-model-file.gguf"
        ));
        assert!(resolved_target_accepts_request(
            &target,
            "zai-org_GLM-4.5-Air-Q4_K_M-00001-of-00002.gguf"
        ));
        assert!(!resolved_target_accepts_request(&target, "glm"));
        assert!(!resolved_target_accepts_request(
            &target,
            "zai-org_GLM-4.5-Air-Q4_K_M-extra.gguf"
        ));
    }

    #[test]
    fn explicit_workload_never_relaxes_capability_or_context() {
        for workload in ["code", "codegen", "review"] {
            assert_eq!(
                empty_route_policy(Some(workload), Some(32_768)),
                EmptyRoutePolicy::FailRequiredWorkload
            );
            assert_eq!(
                empty_route_policy(Some(workload), None),
                EmptyRoutePolicy::FailRequiredWorkload
            );
        }
        assert_eq!(
            empty_route_policy(None, Some(32_768)),
            EmptyRoutePolicy::RelaxUnfilteredContext
        );
        assert_eq!(
            empty_route_policy(None, None),
            EmptyRoutePolicy::FailUnavailable
        );
    }

    #[test]
    fn same_catalog_deployments_can_fail_over_without_model_change() {
        let mut c1 = candidate("http://glm-a:55008", "glm-a", Some("glm"), Some(1));
        c1.catalog_id = Some("glm-4.5-air".to_string());
        c1.catalog_name = Some("GLM 4.5 Air".to_string());
        let mut c2 = candidate("http://glm-b:55008", "glm-b", Some("glm"), Some(1));
        c2.catalog_id = Some("glm-4.5-air".to_string());
        c2.catalog_name = Some("GLM 4.5 Air".to_string());
        set_inflight_for_test("http://glm-a:55008", 1);

        let pool = vec![c1, c2];
        let ordered = rank_candidates(&pool, "sia", Some("glm"), false);
        assert_eq!(ordered[0].endpoint, "http://glm-b:55008");
        assert_eq!(ordered[0].catalog_id.as_deref(), Some("glm-4.5-air"));
        assert_eq!(ordered[1].catalog_id.as_deref(), Some("glm-4.5-air"));

        set_inflight_for_test("http://glm-a:55008", 0);
    }

    #[tokio::test]
    async fn live_model_endpoint_identity_regression_skips_without_db() {
        let Some(database_url) = std::env::var("FORGEFLEET_POSTGRES_URL")
            .ok()
            .or_else(|| std::env::var("FORGEFLEET_DATABASE_URL").ok())
        else {
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&database_url)
            .await
            .expect("connect live ForgeFleet Postgres");

        let logan =
            resolve_endpoint_target(&pool, "http://192.168.5.111:55004", Some("glm-4.5-air"))
                .await
                .unwrap_err()
                .to_string();
        assert!(logan.contains("not requested model glm-4.5-air"));

        let logan_auto = resolve_endpoint_target(&pool, "http://192.168.5.111:55004", None)
            .await
            .expect("Logan :55004 should resolve canonically");
        assert_eq!(logan_auto.catalog_id, "lucy-1-7b");
        assert_eq!(logan_auto.worker_name.to_lowercase(), "logan");
        assert_eq!(logan_auto.endpoint, "http://192.168.5.111:55004");
        assert_eq!(logan_auto.engine_label(), "local:unattested:lucy-1-7b");
        assert_eq!(logan_auto.route_decision()["endpoint"], logan_auto.endpoint);

        let glm = resolve_explicit_catalog_target(&pool, "glm-4.5-air")
            .await
            .expect("--model glm-4.5-air should resolve a healthy GLM endpoint");
        assert_eq!(glm.catalog_id, "glm-4.5-air");
        assert_ne!(glm.endpoint, "http://192.168.5.111:55004");

        let different_model = resolve_endpoint_target(
            &pool,
            "http://192.168.5.111:55004",
            Some("devstral-small-2-24b"),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(different_model.contains("not requested model devstral-small-2-24b"));
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
    fn code_route_builds_non_thinking_request_with_exact_budget() {
        for alias in ["code", "CODEGEN", "coding", "review", "reviewer"] {
            assert_eq!(
                completion_workload_for_route(Some(alias)),
                WorkloadClass::CodeOneShot,
                "{alias} must use the non-thinking one-shot policy"
            );
        }
        let workload = completion_workload_for_route(Some("code"));
        let body = completion_request_body(
            "glm-4.5-air",
            json!([{"role": "user", "content": "write code"}]),
            workload,
            CompletionBudget::new(777).unwrap(),
        )
        .unwrap();
        assert_eq!(body["max_tokens"], 777);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
    }

    #[test]
    fn council_route_preserves_reasoning_default_but_sets_budget() {
        let workload = completion_workload_for_route(None);
        assert_eq!(workload, WorkloadClass::Reasoning);
        let body = completion_request_body(
            "glm-4.5-air",
            json!([{"role": "user", "content": "deliberate"}]),
            workload,
            CompletionBudget::new(4_096).unwrap(),
        )
        .unwrap();
        assert_eq!(body["max_tokens"], 4_096);
        assert!(body.get("chat_template_kwargs").is_none());
    }

    #[test]
    fn compatibility_extractor_rejects_reasoning_only_and_truncation() {
        let secret = "private reasoning";
        let reasoning_only = json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {"content": null, "reasoning_content": secret}
            }]
        });
        assert!(validate_completion_response(&reasoning_only).is_err());

        let truncated = json!({
            "choices": [{
                "finish_reason": "length",
                "message": {"content": "partial", "reasoning_content": secret}
            }]
        });
        assert!(validate_completion_response(&truncated).is_err());
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
            "http://test-prefers-vinny:1",
            "vinny",
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
            "http://test-fallback-vinny:1",
            "vinny",
            Some("llama"),
            Some(4),
        );
        set_inflight_for_test("http://test-fallback-lily:1", 1);
        set_inflight_for_test("http://test-fallback-marcus:1", 1);

        let pool = vec![local, remote_coder, remote_other];
        let ordered = rank_candidates(&pool, "lily", Some("coder"), false);

        // coder family is full, so the free non-family deployment comes first.
        assert_eq!(ordered[0].endpoint, "http://test-fallback-vinny:1");

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
