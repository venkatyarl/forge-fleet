//! Local-only execution boundary for the strategy-aware `fleet_run` path.
//!
//! A request takes one immutable, best-first snapshot from the canonical
//! `ff_db::pg_route_deployments` scorer. Every retry is then pinned to a
//! distinct deployment UUID from that snapshot and passes through the shared
//! ForgeFleet target-resolution and live-attestation boundary. There is no
//! hard-coded endpoint and no cloud fallback in this module.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use ff_core::config::LlmTimeouts;
use ff_core::llm_completion_policy::{
    CompletionBudget, WorkloadClass, apply_completion_policy, validate_completion_response,
};
use ff_db::queries::DISPATCH_HEALTH_MAX_AGE_SEC;
use ff_db::{RouteCandidate, RouteFilter};
use ff_orchestrator::cascade_strategy::LlmExec;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

/// Existing operational-config key used for Path-3 controls. The value is a
/// JSON object matching [`LocalRoutePolicyOverride`]; no schema change is
/// required and an absent key keeps conservative defaults.
pub const LOCAL_ROUTE_POLICY_KEY: &str = "ff_mcp.local_route_policy.v1";

/// Keep the server's absolute deadline 15 seconds below the observed
/// 120-second MCP client request envelope. This margin lets us return typed
/// failure evidence instead of having the client tear down the transport first.
const SERVER_HARD_TOTAL_TIMEOUT_MS: u64 = 105_000;
const SERVER_HARD_MAX_ATTEMPT_TIMEOUT_MS: u64 = 90_000;
const DEFAULT_TOTAL_TIMEOUT_MS: u64 = SERVER_HARD_TOTAL_TIMEOUT_MS;
const DEFAULT_MIN_START_BUDGET_MS: u64 = 5_000;
const DEFAULT_MAX_ATTEMPT_TIMEOUT_MS: u64 = SERVER_HARD_MAX_ATTEMPT_TIMEOUT_MS;
const DEFAULT_MAX_DISTINCT_ATTEMPTS: usize = 3;
const DEFAULT_COOLDOWN_MS: u64 = 60_000;
const DEFAULT_MAX_COOLDOWN_ENTRIES: usize = 512;
const DEFAULT_ATTESTATION_TIMEOUT_MS: u64 = 5_000;
const INDEPENDENT_JUDGE_FAMILY: &str = "gemma";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SoleEndpointPolicy {
    /// A cooling deployment is unavailable until its TTL expires.
    RespectCooldown,
    /// When it is the only candidate, allow one atomically-reserved probe even
    /// before the TTL expires. This is process-local and never fleet authority.
    HalfOpen,
}

impl Default for SoleEndpointPolicy {
    fn default() -> Self {
        Self::RespectCooldown
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct LocalRoutePolicyOverride {
    pub total_timeout_ms: Option<u64>,
    pub min_start_budget_ms: Option<u64>,
    pub max_attempt_timeout_ms: Option<u64>,
    pub max_distinct_attempts: Option<usize>,
    pub cooldown_ms: Option<u64>,
    pub max_cooldown_entries: Option<usize>,
    pub attestation_timeout_ms: Option<u64>,
    pub sole_endpoint_policy: Option<SoleEndpointPolicy>,
}

/// Request policy. Tier ceilings come from the existing `[llm.timeouts]`
/// config; the remaining controls may be overlaid through
/// [`LOCAL_ROUTE_POLICY_KEY`].
#[derive(Debug, Clone, Serialize)]
pub struct LocalRoutePolicy {
    #[serde(with = "duration_millis")]
    pub total_timeout: Duration,
    #[serde(with = "duration_millis")]
    pub min_start_budget: Duration,
    #[serde(with = "duration_millis")]
    pub max_attempt_timeout: Duration,
    pub max_distinct_attempts: usize,
    #[serde(with = "duration_millis")]
    pub cooldown: Duration,
    pub max_cooldown_entries: usize,
    #[serde(with = "duration_millis")]
    pub attestation_timeout: Duration,
    pub sole_endpoint_policy: SoleEndpointPolicy,
    #[serde(with = "duration_millis_array")]
    tier_timeouts: [Duration; 4],
}

impl LocalRoutePolicy {
    pub fn from_config(
        timeouts: &LlmTimeouts,
        stored_override: Option<&str>,
    ) -> Result<Self, String> {
        let overlay = match stored_override.map(str::trim).filter(|raw| !raw.is_empty()) {
            Some(raw) => serde_json::from_str::<LocalRoutePolicyOverride>(raw)
                .map_err(|error| format!("invalid {LOCAL_ROUTE_POLICY_KEY}: {error}"))?,
            None => LocalRoutePolicyOverride::default(),
        };
        let mut policy = Self {
            total_timeout: Duration::from_millis(
                overlay.total_timeout_ms.unwrap_or(DEFAULT_TOTAL_TIMEOUT_MS),
            ),
            min_start_budget: Duration::from_millis(
                overlay
                    .min_start_budget_ms
                    .unwrap_or(DEFAULT_MIN_START_BUDGET_MS),
            ),
            max_attempt_timeout: Duration::from_millis(
                overlay
                    .max_attempt_timeout_ms
                    .unwrap_or(DEFAULT_MAX_ATTEMPT_TIMEOUT_MS),
            ),
            max_distinct_attempts: overlay
                .max_distinct_attempts
                .unwrap_or(DEFAULT_MAX_DISTINCT_ATTEMPTS),
            cooldown: Duration::from_millis(overlay.cooldown_ms.unwrap_or(DEFAULT_COOLDOWN_MS)),
            max_cooldown_entries: overlay
                .max_cooldown_entries
                .unwrap_or(DEFAULT_MAX_COOLDOWN_ENTRIES),
            attestation_timeout: Duration::from_millis(
                overlay
                    .attestation_timeout_ms
                    .unwrap_or(DEFAULT_ATTESTATION_TIMEOUT_MS),
            ),
            sole_endpoint_policy: overlay.sole_endpoint_policy.unwrap_or_default(),
            tier_timeouts: [
                Duration::from_secs(timeouts.tier1.unwrap_or(30)),
                Duration::from_secs(timeouts.tier2.unwrap_or(60)),
                Duration::from_secs(timeouts.tier3.unwrap_or(120)),
                Duration::from_secs(timeouts.tier4.unwrap_or(300)),
            ],
        };
        policy.enforce_server_ceilings();
        policy.validate()?;
        Ok(policy)
    }

    /// Operational configuration may make a request stricter, but it cannot
    /// expand the MCP server's safety envelope. This is also applied at the
    /// executor boundary so a future programmatic caller cannot bypass the
    /// same hard limits by constructing `LocalRoutePolicy` directly.
    fn enforce_server_ceilings(&mut self) {
        self.total_timeout = self
            .total_timeout
            .min(Duration::from_millis(SERVER_HARD_TOTAL_TIMEOUT_MS));
        self.max_attempt_timeout = self
            .max_attempt_timeout
            .min(Duration::from_millis(SERVER_HARD_MAX_ATTEMPT_TIMEOUT_MS));
    }

    fn validate(&self) -> Result<(), String> {
        if self.total_timeout.is_zero() {
            return Err("local route total_timeout_ms must be positive".into());
        }
        if self.min_start_budget.is_zero() {
            return Err("local route min_start_budget_ms must be positive".into());
        }
        if self.min_start_budget > self.total_timeout {
            return Err("local route min_start_budget_ms exceeds total_timeout_ms".into());
        }
        if self.max_attempt_timeout.is_zero() {
            return Err("local route max_attempt_timeout_ms must be positive".into());
        }
        if self.max_distinct_attempts == 0 {
            return Err("local route max_distinct_attempts must be positive".into());
        }
        if self.max_cooldown_entries == 0 {
            return Err("local route max_cooldown_entries must be positive".into());
        }
        if self.attestation_timeout.is_zero() {
            return Err("local route attestation_timeout_ms must be positive".into());
        }
        if self.tier_timeouts.iter().any(Duration::is_zero) {
            return Err("local route tier timeouts must be positive".into());
        }
        Ok(())
    }

    fn tier_timeout(&self, tier: u8) -> Duration {
        self.tier_timeouts[usize::from(tier.clamp(1, 4) - 1)]
    }
}

mod duration_millis {
    use std::time::Duration;

    use serde::Serializer;

    pub fn serialize<S>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(value.as_millis().min(u64::MAX as u128) as u64)
    }
}

mod duration_millis_array {
    use std::time::Duration;

    use serde::{Serialize, Serializer};

    pub fn serialize<S>(value: &[Duration; 4], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let millis = value.map(|duration| duration.as_millis().min(u64::MAX as u128) as u64);
        millis.serialize(serializer)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureReasonCode {
    Timeout,
    InvalidResponse,
    Unavailable,
    DeadlineExceeded,
}

impl FailureReasonCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::InvalidResponse => "invalid_response",
            Self::Unavailable => "unavailable",
            Self::DeadlineExceeded => "deadline_exceeded",
        }
    }
}

#[derive(Debug, Clone)]
struct ExecutionFailure {
    code: FailureReasonCode,
    message: String,
}

impl ExecutionFailure {
    fn new(code: FailureReasonCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptRole {
    Completion,
    Judge,
}

#[derive(Debug, Clone, Serialize)]
pub struct AttemptLedgerEntry {
    pub sequence: usize,
    pub role: AttemptRole,
    /// Logical tier requested by the strategy stage.
    pub tier: u8,
    /// Actual catalog tier selected after upward-only local escalation.
    pub catalog_tier: i32,
    pub deployment_id: Uuid,
    pub endpoint: String,
    pub worker_name: String,
    pub catalog_id: Option<String>,
    pub attempt_timeout_ms: u64,
    pub started_offset_ms: u64,
    pub latency_ms: u64,
    pub outcome: String,
    pub reason_code: Option<FailureReasonCode>,
    pub error: Option<String>,
    pub tokens_in: i32,
    pub tokens_out: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct CandidateSnapshotEntry {
    pub ordinal: usize,
    pub deployment_id: Uuid,
    pub endpoint: String,
    pub worker_name: String,
    pub catalog_id: Option<String>,
    pub catalog_tier: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct WinningRoute {
    pub deployment_id: Uuid,
    pub endpoint: String,
    pub worker_name: String,
    pub catalog_id: String,
    pub served_model_id: Option<String>,
    pub engine: String,
    pub route_decision: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecutionEvidence {
    pub attempts: Vec<AttemptLedgerEntry>,
    pub candidate_snapshot: Vec<CandidateSnapshotEntry>,
    pub winner: Option<WinningRoute>,
    pub last_failure: Option<FailureReasonCode>,
    pub latency_ms: u64,
    pub local_authority: &'static str,
    pub cloud_fallback: bool,
}

#[derive(Debug, Clone)]
struct RegistryEntry {
    reserved: bool,
    cooldown_until: Option<Instant>,
    touched_at: Instant,
}

/// Process-local availability hint. It deliberately makes no fleet-global
/// claim; Postgres routing and endpoint attestation remain authoritative.
#[derive(Debug, Default)]
struct ReservationRegistry {
    entries: Mutex<HashMap<Uuid, RegistryEntry>>,
}

impl ReservationRegistry {
    fn reserve_next(
        self: &Arc<Self>,
        ordered: &[Uuid],
        already_attempted: &HashSet<Uuid>,
        now: Instant,
        policy: &LocalRoutePolicy,
    ) -> Option<ReservationLease> {
        let eligible = ordered
            .iter()
            .copied()
            .filter(|id| !already_attempted.contains(id))
            .collect::<Vec<_>>();
        if eligible.is_empty() {
            return None;
        }

        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::evict_to_bound(&mut entries, now, policy.max_cooldown_entries, &eligible);

        let sole_half_open =
            eligible.len() == 1 && policy.sole_endpoint_policy == SoleEndpointPolicy::HalfOpen;
        let chosen = eligible.into_iter().find(|id| match entries.get(id) {
            None => true,
            Some(entry) if entry.reserved => false,
            Some(entry) => {
                entry.cooldown_until.is_none()
                    || entry.cooldown_until.is_some_and(|until| until <= now)
                    || sole_half_open
            }
        })?;

        if !entries.contains_key(&chosen) && entries.len() >= policy.max_cooldown_entries {
            return None;
        }
        entries.insert(
            chosen,
            RegistryEntry {
                reserved: true,
                cooldown_until: None,
                touched_at: now,
            },
        );
        drop(entries);
        Some(ReservationLease {
            registry: Arc::clone(self),
            deployment_id: chosen,
            cooldown: policy.cooldown,
            finished: false,
        })
    }

    fn evict_to_bound(
        entries: &mut HashMap<Uuid, RegistryEntry>,
        now: Instant,
        max_entries: usize,
        current: &[Uuid],
    ) {
        entries.retain(|_, entry| {
            entry.reserved || entry.cooldown_until.is_none_or(|until| until > now)
        });
        while entries.len() >= max_entries {
            let victim = entries
                .iter()
                .filter(|(id, entry)| !entry.reserved && !current.contains(id))
                .min_by_key(|(id, entry)| (entry.touched_at, **id))
                .map(|(id, _)| *id)
                .or_else(|| {
                    entries
                        .iter()
                        .filter(|(_, entry)| !entry.reserved)
                        .min_by_key(|(id, entry)| (entry.touched_at, **id))
                        .map(|(id, _)| *id)
                });
            let Some(victim) = victim else {
                break;
            };
            entries.remove(&victim);
        }
    }

    fn finish(&self, deployment_id: Uuid, success: bool, cooldown: Duration, now: Instant) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if success {
            entries.remove(&deployment_id);
            return;
        }
        entries.insert(
            deployment_id,
            RegistryEntry {
                reserved: false,
                cooldown_until: now.checked_add(cooldown),
                touched_at: now,
            },
        );
    }
}

struct ReservationLease {
    registry: Arc<ReservationRegistry>,
    deployment_id: Uuid,
    cooldown: Duration,
    finished: bool,
}

impl ReservationLease {
    fn finish(mut self, success: bool, now: Instant) {
        self.registry
            .finish(self.deployment_id, success, self.cooldown, now);
        self.finished = true;
    }
}

impl Drop for ReservationLease {
    fn drop(&mut self) {
        if !self.finished {
            self.registry
                .finish(self.deployment_id, false, self.cooldown, Instant::now());
        }
    }
}

static PROCESS_LOCAL_RESERVATIONS: LazyLock<Arc<ReservationRegistry>> =
    LazyLock::new(|| Arc::new(ReservationRegistry::default()));

trait MonotonicClock: Send + Sync {
    fn now(&self) -> Instant;
}

struct SystemClock;

impl MonotonicClock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[async_trait::async_trait]
trait CandidateSource: Send + Sync {
    async fn snapshot(&self, workload: WorkloadClass) -> Result<CanonicalRouteSnapshot, String>;
    async fn resolve(
        &self,
        candidate: &RouteCandidate,
    ) -> Result<ff_agent::fleet_oneshot::ResolvedFleetTarget, String>;
}

struct PgCandidateSource {
    pool: sqlx::PgPool,
}

#[derive(Debug, Clone)]
struct CanonicalRouteSnapshot {
    candidates: Vec<RouteCandidate>,
    generation_eligible: HashSet<Uuid>,
}

fn workload_affinity_tags(workload: WorkloadClass) -> &'static [&'static str] {
    match workload {
        WorkloadClass::CodeOneShot => &[
            "code",
            "code-gen",
            "codegen",
            "coder",
            "coding",
            "code-generation",
            "review",
            "code-review",
            "reviewer",
        ],
        WorkloadClass::Reasoning => &["reason", "reasoning", "thinking", "chain-of-thought"],
    }
}

#[async_trait::async_trait]
impl CandidateSource for PgCandidateSource {
    async fn snapshot(&self, workload: WorkloadClass) -> Result<CanonicalRouteSnapshot, String> {
        // One broad canonical snapshot is required so generation candidates
        // and the independent Gemma-family judge are frozen to the same
        // request-time deployment view. Role-specific capability filters are
        // applied below; taking a second judge query would permit route drift.
        let filter = RouteFilter {
            workload: None,
            require_tool_calling: false,
            min_ctx: None,
            exclude_hosts: Vec::new(),
            max_health_age_sec: Some(DISPATCH_HEALTH_MAX_AGE_SEC),
            prefer_least_loaded: true,
            limit: 128,
        };
        let candidates = ff_db::pg_route_deployments(&self.pool, &filter)
            .await
            .map_err(|error| format!("canonical route snapshot failed: {error}"))?;
        let catalog_ids = candidates
            .iter()
            .filter_map(|candidate| candidate.catalog_id.clone())
            .collect::<Vec<_>>();
        let affinity_tags = workload_affinity_tags(workload);
        let eligible_catalog_ids = sqlx::query_scalar::<_, String>(
            r#"
            SELECT id
              FROM fleet_model_catalog
             WHERE id = ANY($1::text[])
               AND EXISTS (
                   SELECT 1
                     FROM jsonb_array_elements_text(
                              CASE
                                  WHEN jsonb_typeof(preferred_workloads) = 'array'
                                  THEN preferred_workloads
                                  ELSE '[]'::jsonb
                              END
                          ) AS workload(value)
                    WHERE LOWER(workload.value) = ANY($2::text[])
               )
            "#,
        )
        .bind(&catalog_ids)
        .bind(affinity_tags)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| format!("canonical route workload annotation failed: {error}"))?
        .into_iter()
        .collect::<HashSet<_>>();
        let generation_eligible = candidates
            .iter()
            .filter(|candidate| {
                candidate
                    .catalog_id
                    .as_ref()
                    .is_some_and(|id| eligible_catalog_ids.contains(id))
            })
            .map(|candidate| candidate.deployment_id)
            .collect();
        Ok(CanonicalRouteSnapshot {
            candidates,
            generation_eligible,
        })
    }

    async fn resolve(
        &self,
        candidate: &RouteCandidate,
    ) -> Result<ff_agent::fleet_oneshot::ResolvedFleetTarget, String> {
        ff_agent::fleet_oneshot::resolve_candidate_target(
            &self.pool,
            candidate,
            ff_agent::fleet_oneshot::ResolvedTargetProvenance::Auto,
            false,
        )
        .await
        .map_err(|error| format!("canonical target resolution failed: {error}"))
    }
}

struct TransportResponse {
    target: ff_agent::fleet_oneshot::ResolvedFleetTarget,
    payload: Value,
}

#[async_trait::async_trait]
trait CompletionTransport: Send + Sync {
    async fn complete(
        &self,
        target: ff_agent::fleet_oneshot::ResolvedFleetTarget,
        prompt: &str,
        workload: WorkloadClass,
        budget: CompletionBudget,
        attestation_timeout: Duration,
        attempt_timeout: Duration,
    ) -> Result<TransportResponse, ExecutionFailure>;
}

struct HttpCompletionTransport {
    client: reqwest::Client,
}

#[async_trait::async_trait]
impl CompletionTransport for HttpCompletionTransport {
    async fn complete(
        &self,
        target: ff_agent::fleet_oneshot::ResolvedFleetTarget,
        prompt: &str,
        workload: WorkloadClass,
        budget: CompletionBudget,
        attestation_timeout: Duration,
        attempt_timeout: Duration,
    ) -> Result<TransportResponse, ExecutionFailure> {
        let target = ff_agent::fleet_oneshot::attest_resolved_target(
            &self.client,
            target,
            attestation_timeout.min(attempt_timeout),
        )
        .await
        .map_err(|error| {
            let code = classify_attestation_failure(&error);
            let message = error.to_string();
            ExecutionFailure::new(code, format!("endpoint attestation failed: {message}"))
        })?;

        let model = target.inference_model().to_string();
        let url = ff_core::url::normalize_chat_completions_url(&target.endpoint);
        let mut body = json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "temperature": 0.3,
        });
        apply_completion_policy(&mut body, workload, budget).map_err(|error| {
            ExecutionFailure::new(
                FailureReasonCode::InvalidResponse,
                format!("completion request policy rejected request: {error}"),
            )
        })?;
        let response = self
            .client
            .post(&url)
            .timeout(attempt_timeout)
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                let code = if error.is_timeout() {
                    FailureReasonCode::Timeout
                } else {
                    FailureReasonCode::Unavailable
                };
                ExecutionFailure::new(code, format!("POST {url}: {error}"))
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(ExecutionFailure::new(
                FailureReasonCode::Unavailable,
                format!("{} returned HTTP {status}", target.endpoint),
            ));
        }
        let payload = response.json::<Value>().await.map_err(|error| {
            ExecutionFailure::new(
                if error.is_timeout() {
                    FailureReasonCode::Timeout
                } else {
                    FailureReasonCode::InvalidResponse
                },
                format!(
                    "decode completion response from {}: {error}",
                    target.endpoint
                ),
            )
        })?;
        Ok(TransportResponse { target, payload })
    }
}

fn classify_attestation_failure(error: &anyhow::Error) -> FailureReasonCode {
    if let Some(request_error) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<reqwest::Error>())
    {
        return if request_error.is_timeout() {
            FailureReasonCode::Timeout
        } else {
            FailureReasonCode::Unavailable
        };
    }
    // The shared attestation primitive currently formats some reqwest errors
    // into anyhow messages, erasing the concrete source. Never infer type from
    // provider text: an erased error is deterministically fail-closed.
    FailureReasonCode::InvalidResponse
}

fn is_independent_judge(candidate: &RouteCandidate) -> bool {
    candidate
        .family
        .as_deref()
        .is_some_and(|family| family.eq_ignore_ascii_case(INDEPENDENT_JUDGE_FAMILY))
}

/// Compute the next attempt's entire budget from one caller-owned deadline.
/// No attempt starts unless the configured minimum remains.
pub fn remaining_attempt_budget(
    now: Instant,
    deadline: Instant,
    min_start: Duration,
    tier_cap: Duration,
    requested_cap: Duration,
    max_attempt: Duration,
) -> Option<Duration> {
    let remaining = deadline.checked_duration_since(now)?;
    if remaining < min_start {
        return None;
    }
    Some(remaining.min(tier_cap).min(requested_cap).min(max_attempt))
}

/// Request-scoped strategy executor. All state below (snapshot, failed IDs,
/// deadline, and ledger) belongs to one MCP request; only reservations/cooldown
/// are intentionally shared within this process.
pub struct GatewayLlmExec {
    source: Arc<dyn CandidateSource>,
    transport: Arc<dyn CompletionTransport>,
    reservations: Arc<ReservationRegistry>,
    clock: Arc<dyn MonotonicClock>,
    workload: WorkloadClass,
    completion_ceiling: Option<CompletionBudget>,
    policy: LocalRoutePolicy,
    started_at: Instant,
    deadline: Instant,
    snapshot: tokio::sync::OnceCell<Result<Arc<CanonicalRouteSnapshot>, ExecutionFailure>>,
    ledger: Mutex<Vec<AttemptLedgerEntry>>,
    failed_ids: Mutex<HashSet<Uuid>>,
    winner: Mutex<Option<WinningRoute>>,
    last_failure: Mutex<Option<ExecutionFailure>>,
    role_failures: Mutex<HashMap<AttemptRole, ExecutionFailure>>,
}

impl GatewayLlmExec {
    pub fn new(
        pool: sqlx::PgPool,
        workload: WorkloadClass,
        completion_ceiling: Option<CompletionBudget>,
        mut policy: LocalRoutePolicy,
    ) -> Self {
        policy.enforce_server_ceilings();
        let clock: Arc<dyn MonotonicClock> = Arc::new(SystemClock);
        let started_at = clock.now();
        let deadline = started_at
            .checked_add(policy.total_timeout)
            .unwrap_or(started_at);
        Self {
            source: Arc::new(PgCandidateSource { pool }),
            transport: Arc::new(HttpCompletionTransport {
                client: reqwest::Client::new(),
            }),
            reservations: Arc::clone(&PROCESS_LOCAL_RESERVATIONS),
            clock,
            workload,
            completion_ceiling,
            policy,
            started_at,
            deadline,
            snapshot: tokio::sync::OnceCell::new(),
            ledger: Mutex::new(Vec::new()),
            failed_ids: Mutex::new(HashSet::new()),
            winner: Mutex::new(None),
            last_failure: Mutex::new(None),
            role_failures: Mutex::new(HashMap::new()),
        }
    }

    fn stage_budget(&self, requested: u32) -> Result<CompletionBudget, String> {
        let effective = self
            .completion_ceiling
            .map(|ceiling| requested.min(ceiling.get()))
            .unwrap_or(requested);
        CompletionBudget::new(effective)
            .map_err(|error| format!("invalid completion budget: {error}"))
    }

    async fn candidate_snapshot(&self) -> Result<Arc<CanonicalRouteSnapshot>, ExecutionFailure> {
        let result = self
            .snapshot
            .get_or_init(|| async {
                let now = self.clock.now();
                let remaining = self.deadline.checked_duration_since(now).ok_or_else(|| {
                    ExecutionFailure::new(
                        FailureReasonCode::DeadlineExceeded,
                        "deadline elapsed before canonical route snapshot",
                    )
                })?;
                let mut snapshot =
                    tokio::time::timeout(remaining, self.source.snapshot(self.workload))
                        .await
                        .map_err(|_| {
                            ExecutionFailure::new(
                                FailureReasonCode::DeadlineExceeded,
                                "deadline elapsed while taking canonical route snapshot",
                            )
                        })?
                        .map_err(|error| {
                            ExecutionFailure::new(FailureReasonCode::Unavailable, error)
                        })?;
                let mut seen = HashSet::new();
                snapshot.candidates = snapshot
                    .candidates
                    .into_iter()
                    .filter(|candidate| seen.insert(candidate.deployment_id))
                    .collect::<Vec<_>>();
                snapshot
                    .generation_eligible
                    .retain(|deployment_id| seen.contains(deployment_id));
                if snapshot.candidates.is_empty() {
                    return Err(ExecutionFailure::new(
                        FailureReasonCode::Unavailable,
                        "canonical route snapshot contains no healthy local deployments",
                    ));
                }
                Ok(Arc::new(snapshot))
            })
            .await;
        result.clone()
    }

    async fn execute_role(
        &self,
        role: AttemptRole,
        tier: u8,
        prompt: &str,
        max_tokens: u32,
        requested_timeout: Duration,
    ) -> Result<String, String> {
        let budget = self.stage_budget(max_tokens)?;
        let snapshot = match self.candidate_snapshot().await {
            Ok(snapshot) => snapshot,
            Err(failure) => return Err(self.remember_failure(role, failure)),
        };
        let candidates = &snapshot.candidates;
        let requested_tier = tier.clamp(1, 4);
        // Generation and judge pools are deliberately disjoint. Generation
        // requires a tool-capable non-Gemma chat model; Judge requires Gemma
        // and later excludes the winning deployment. This preserves the old
        // third-party-family judge and makes a sole generator fail closed
        // instead of self-grading.
        let mut eligible = candidates
            .iter()
            .enumerate()
            .filter(|(_, candidate)| match role {
                AttemptRole::Completion => {
                    snapshot
                        .generation_eligible
                        .contains(&candidate.deployment_id)
                        && candidate.tool_calling
                        && !is_independent_judge(candidate)
                        && candidate.tier >= i32::from(requested_tier)
                }
                AttemptRole::Judge => is_independent_judge(candidate),
            })
            .collect::<Vec<_>>();
        eligible.sort_by_key(|(ordinal, candidate)| (candidate.tier, *ordinal));
        let ordered_ids = eligible
            .into_iter()
            .map(|(_, candidate)| candidate.deployment_id)
            .collect::<Vec<_>>();
        if ordered_ids.is_empty() {
            let message = match role {
                AttemptRole::Completion => format!(
                    "canonical route snapshot has no tool-capable local deployment at or above requested tier {requested_tier}"
                ),
                AttemptRole::Judge => format!(
                    "canonical route snapshot has no independent {INDEPENDENT_JUDGE_FAMILY}-family judge"
                ),
            };
            return Err(self.remember_failure(
                role,
                ExecutionFailure::new(FailureReasonCode::Unavailable, message),
            ));
        }
        let mut attempted = self
            .failed_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if role == AttemptRole::Judge
            && let Some(winner) = self
                .winner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref()
        {
            attempted.insert(winner.deployment_id);
        }

        let mut last_failure: Option<ExecutionFailure> = None;
        for _ in 0..self.policy.max_distinct_attempts {
            let now = self.clock.now();
            if self
                .deadline
                .checked_duration_since(now)
                .is_none_or(|remaining| remaining < self.policy.min_start_budget)
            {
                let failure = ExecutionFailure::new(
                    FailureReasonCode::DeadlineExceeded,
                    "insufficient remaining request budget to start another local attempt",
                );
                return Err(self.remember_failure(role, failure));
            };

            let Some(lease) =
                self.reservations
                    .reserve_next(&ordered_ids, &attempted, now, &self.policy)
            else {
                let failure = ExecutionFailure::new(
                    FailureReasonCode::Unavailable,
                    "no distinct, unreserved local deployment is currently eligible",
                );
                last_failure = Some(failure);
                break;
            };
            let deployment_id = lease.deployment_id;
            attempted.insert(deployment_id);
            let candidate = candidates
                .iter()
                .find(|candidate| candidate.deployment_id == deployment_id)
                .expect("reserved deployment came from candidate snapshot")
                .clone();
            let attempt_tier = u8::try_from(candidate.tier).unwrap_or(4).clamp(1, 4);
            let Some(attempt_timeout) = remaining_attempt_budget(
                now,
                self.deadline,
                self.policy.min_start_budget,
                self.policy.tier_timeout(attempt_tier),
                requested_timeout,
                self.policy.max_attempt_timeout,
            ) else {
                lease.finish(true, now);
                let failure = ExecutionFailure::new(
                    FailureReasonCode::DeadlineExceeded,
                    "insufficient remaining request budget for selected local deployment tier",
                );
                return Err(self.remember_failure(role, failure));
            };
            let attempt_started = self.clock.now();
            let attempt_future = async {
                let target = self.source.resolve(&candidate).await.map_err(|error| {
                    ExecutionFailure::new(FailureReasonCode::Unavailable, error)
                })?;
                self.transport
                    .complete(
                        target,
                        prompt,
                        self.workload,
                        budget,
                        self.policy.attestation_timeout,
                        attempt_timeout,
                    )
                    .await
            };
            let transport_result = tokio::time::timeout(attempt_timeout, attempt_future).await;
            let transport_completed_at = self.clock.now();

            let result = match transport_result {
                Err(_) => {
                    let code = if transport_completed_at >= self.deadline {
                        FailureReasonCode::DeadlineExceeded
                    } else {
                        FailureReasonCode::Timeout
                    };
                    Err(ExecutionFailure::new(
                        code,
                        format!("local attempt exceeded {}ms", attempt_timeout.as_millis()),
                    ))
                }
                Ok(Err(failure)) => Err(failure),
                Ok(Ok(response)) => {
                    // Validation is deliberately completed before the final
                    // clock read. This makes parse/strict finish-reason and
                    // nonblank validation consume the same absolute budget as
                    // snapshot, resolution, attestation, connect, and body.
                    let validated =
                        validate_completion_response(&response.payload).map_err(|error| {
                            ExecutionFailure::new(
                                FailureReasonCode::InvalidResponse,
                                format!("invalid local completion: {error}"),
                            )
                        });
                    if self.clock.now() >= self.deadline {
                        Err(ExecutionFailure::new(
                            FailureReasonCode::DeadlineExceeded,
                            "request deadline elapsed before response validation completed",
                        ))
                    } else {
                        validated.map(|completion| (response, completion.content))
                    }
                }
            };
            let completed_at = self.clock.now();
            let elapsed = completed_at.saturating_duration_since(attempt_started);

            match result {
                Ok((response, output)) => {
                    let (tokens_in, tokens_out) = usage_tokens(&response.payload);
                    self.push_attempt(AttemptLedgerEntry {
                        sequence: 0,
                        role,
                        tier: requested_tier,
                        catalog_tier: candidate.tier,
                        deployment_id,
                        endpoint: candidate.endpoint.clone(),
                        worker_name: candidate.worker_name.clone(),
                        catalog_id: candidate.catalog_id.clone(),
                        attempt_timeout_ms: millis(attempt_timeout),
                        started_offset_ms: millis(
                            attempt_started.saturating_duration_since(self.started_at),
                        ),
                        latency_ms: millis(elapsed),
                        outcome: "ok".into(),
                        reason_code: None,
                        error: None,
                        tokens_in,
                        tokens_out,
                    });
                    lease.finish(true, completed_at);
                    if role == AttemptRole::Completion {
                        let target = response.target;
                        *self
                            .winner
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                            Some(WinningRoute {
                                deployment_id: target.deployment_id,
                                endpoint: target.endpoint.clone(),
                                worker_name: target.worker_name.clone(),
                                catalog_id: target.catalog_id.clone(),
                                served_model_id: target.served_model_id.clone(),
                                engine: target.engine_label(),
                                route_decision: target.route_decision(),
                            });
                    }
                    self.role_failures
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&role);
                    return Ok(output);
                }
                Err(failure) => {
                    self.push_attempt(AttemptLedgerEntry {
                        sequence: 0,
                        role,
                        tier: requested_tier,
                        catalog_tier: candidate.tier,
                        deployment_id,
                        endpoint: candidate.endpoint.clone(),
                        worker_name: candidate.worker_name.clone(),
                        catalog_id: candidate.catalog_id.clone(),
                        attempt_timeout_ms: millis(attempt_timeout),
                        started_offset_ms: millis(
                            attempt_started.saturating_duration_since(self.started_at),
                        ),
                        latency_ms: millis(elapsed),
                        outcome: "error".into(),
                        reason_code: Some(failure.code),
                        error: Some(failure.message.clone()),
                        tokens_in: 0,
                        tokens_out: 0,
                    });
                    lease.finish(false, completed_at);
                    self.failed_ids
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(deployment_id);
                    last_failure = Some(failure);
                }
            }
        }

        let failure = last_failure.unwrap_or_else(|| {
            ExecutionFailure::new(
                FailureReasonCode::Unavailable,
                "all distinct local route candidates were exhausted",
            )
        });
        Err(self.remember_failure(role, failure))
    }

    fn push_attempt(&self, mut entry: AttemptLedgerEntry) {
        let mut ledger = self
            .ledger
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entry.sequence = ledger.len() + 1;
        ledger.push(entry);
    }

    fn remember_failure(&self, role: AttemptRole, failure: ExecutionFailure) -> String {
        self.role_failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(role, failure.clone());
        *self
            .last_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(failure.clone());
        format!("{}: {}", failure.code.as_str(), failure.message)
    }

    pub(crate) fn last_role_failure(&self, role: AttemptRole) -> Option<FailureReasonCode> {
        self.role_failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&role)
            .map(|failure| failure.code)
    }

    pub fn evidence(&self) -> ExecutionEvidence {
        let snapshot = self
            .snapshot
            .get()
            .and_then(|result| result.as_ref().ok())
            .map(|snapshot| {
                snapshot
                    .candidates
                    .iter()
                    .enumerate()
                    .map(|(ordinal, candidate)| CandidateSnapshotEntry {
                        ordinal,
                        deployment_id: candidate.deployment_id,
                        endpoint: candidate.endpoint.clone(),
                        worker_name: candidate.worker_name.clone(),
                        catalog_id: candidate.catalog_id.clone(),
                        catalog_tier: candidate.tier,
                    })
                    .collect()
            })
            .unwrap_or_default();
        ExecutionEvidence {
            attempts: self
                .ledger
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
            candidate_snapshot: snapshot,
            winner: self
                .winner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
            last_failure: self
                .last_failure
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref()
                .map(|failure| failure.code),
            latency_ms: millis(self.clock.now().saturating_duration_since(self.started_at)),
            local_authority: "process_local_hint_only",
            cloud_fallback: false,
        }
    }

    #[cfg(test)]
    fn with_test_components(
        source: Arc<dyn CandidateSource>,
        transport: Arc<dyn CompletionTransport>,
        reservations: Arc<ReservationRegistry>,
        clock: Arc<dyn MonotonicClock>,
        workload: WorkloadClass,
        mut policy: LocalRoutePolicy,
    ) -> Self {
        policy.enforce_server_ceilings();
        let started_at = clock.now();
        let deadline = started_at.checked_add(policy.total_timeout).unwrap();
        Self {
            source,
            transport,
            reservations,
            clock,
            workload,
            completion_ceiling: None,
            policy,
            started_at,
            deadline,
            snapshot: tokio::sync::OnceCell::new(),
            ledger: Mutex::new(Vec::new()),
            failed_ids: Mutex::new(HashSet::new()),
            winner: Mutex::new(None),
            last_failure: Mutex::new(None),
            role_failures: Mutex::new(HashMap::new()),
        }
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
        self.execute_role(AttemptRole::Completion, tier, prompt, max_tokens, timeout)
            .await
    }

    async fn judge(
        &self,
        prompt: &str,
        max_tokens: u32,
        timeout: Duration,
    ) -> Result<String, String> {
        self.execute_role(AttemptRole::Judge, 1, prompt, max_tokens, timeout)
            .await
    }
}

fn usage_tokens(payload: &Value) -> (i32, i32) {
    let read = |key: &str| {
        payload
            .get("usage")
            .and_then(|usage| usage.get(key))
            .and_then(Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
            .unwrap_or(0)
    };
    (read("prompt_tokens"), read("completion_tokens"))
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ff_agent::fleet_oneshot::{
        EndpointAttestationState, ResolvedFleetTarget, ResolvedTargetProvenance,
    };

    use super::*;

    #[derive(Clone)]
    struct FakeClock {
        now: Arc<Mutex<Instant>>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                now: Arc::new(Mutex::new(Instant::now())),
            }
        }

        fn advance(&self, duration: Duration) {
            let mut now = self.now.lock().unwrap();
            *now = now.checked_add(duration).unwrap();
        }
    }

    impl MonotonicClock for FakeClock {
        fn now(&self) -> Instant {
            *self.now.lock().unwrap()
        }
    }

    struct FakeSource {
        candidates: Vec<RouteCandidate>,
        generation_eligible: HashSet<Uuid>,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl CandidateSource for FakeSource {
        async fn snapshot(
            &self,
            _workload: WorkloadClass,
        ) -> Result<CanonicalRouteSnapshot, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(CanonicalRouteSnapshot {
                candidates: self.candidates.clone(),
                generation_eligible: self.generation_eligible.clone(),
            })
        }

        async fn resolve(&self, candidate: &RouteCandidate) -> Result<ResolvedFleetTarget, String> {
            Ok(target(candidate))
        }
    }

    #[derive(Clone)]
    enum FakeReply {
        Payload(Value),
        Failure(FailureReasonCode, &'static str),
        AdvanceThenPayload(Duration, Value),
        Never,
    }

    struct FakeTransport {
        replies: Mutex<VecDeque<FakeReply>>,
        seen: Mutex<Vec<Uuid>>,
        clock: FakeClock,
    }

    #[async_trait::async_trait]
    impl CompletionTransport for FakeTransport {
        async fn complete(
            &self,
            target: ResolvedFleetTarget,
            _prompt: &str,
            _workload: WorkloadClass,
            _budget: CompletionBudget,
            _attestation_timeout: Duration,
            _attempt_timeout: Duration,
        ) -> Result<TransportResponse, ExecutionFailure> {
            self.seen.lock().unwrap().push(target.deployment_id);
            let reply = self.replies.lock().unwrap().pop_front().unwrap();
            match reply {
                FakeReply::Payload(payload) => Ok(TransportResponse { target, payload }),
                FakeReply::Failure(code, message) => Err(ExecutionFailure::new(code, message)),
                FakeReply::AdvanceThenPayload(duration, payload) => {
                    self.clock.advance(duration);
                    Ok(TransportResponse { target, payload })
                }
                FakeReply::Never => std::future::pending().await,
            }
        }
    }

    fn candidate(index: u128) -> RouteCandidate {
        candidate_at_tier(index, 2)
    }

    fn candidate_at_tier(index: u128, tier: i32) -> RouteCandidate {
        RouteCandidate {
            deployment_id: Uuid::from_u128(index),
            worker_name: format!("worker-{index}"),
            endpoint: format!("http://192.0.2.{index}:55000"),
            port: 55000,
            runtime: Some("llama.cpp".into()),
            catalog_id: Some("glm-4.5-air".into()),
            catalog_name: Some("GLM-4.5-Air".into()),
            family: Some("glm".into()),
            tier,
            tool_calling: true,
            context_window: Some(32768),
            usable_agent_ctx: Some(32768),
            parallel_slots: Some(1),
            health_status: "healthy".into(),
            health_age_sec: Some(1),
            os_family: Some("linux".into()),
            has_gpu: Some(true),
            is_unified_memory: Some(true),
            total_ram_gb: Some(128),
            cpu_pct: Some(1.0),
            llm_active_requests: Some(0),
        }
    }

    fn judge_candidate(index: u128) -> RouteCandidate {
        let mut candidate = candidate_at_tier(index, 1);
        candidate.catalog_id = Some("gemma-judge".into());
        candidate.catalog_name = Some("Gemma Judge".into());
        candidate.family = Some("gemma".into());
        candidate.tool_calling = false;
        candidate
    }

    fn target(candidate: &RouteCandidate) -> ResolvedFleetTarget {
        ResolvedFleetTarget {
            deployment_id: candidate.deployment_id,
            endpoint: candidate.endpoint.clone(),
            catalog_id: candidate.catalog_id.clone().unwrap(),
            model_label: candidate.catalog_name.clone().unwrap(),
            worker_name: candidate.worker_name.clone(),
            provenance: ResolvedTargetProvenance::Auto,
            router_enabled: false,
            accepted_model_ids: vec!["glm-4.5-air".into()],
            accepted_model_aliases: Vec::new(),
            accepted_shard_prefixes: Vec::new(),
            served_model_id: Some("glm-4.5-air".into()),
            served_model_ids: vec!["glm-4.5-air".into()],
            attestation: EndpointAttestationState::Verified,
        }
    }

    fn valid(content: &str) -> Value {
        json!({
            "choices": [{"message": {"content": content}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 5}
        })
    }

    fn test_policy() -> LocalRoutePolicy {
        LocalRoutePolicy {
            total_timeout: Duration::from_secs(10),
            min_start_budget: Duration::from_millis(100),
            max_attempt_timeout: Duration::from_secs(2),
            max_distinct_attempts: 3,
            cooldown: Duration::from_secs(1),
            max_cooldown_entries: 16,
            attestation_timeout: Duration::from_millis(50),
            sole_endpoint_policy: SoleEndpointPolicy::RespectCooldown,
            tier_timeouts: [Duration::from_secs(2); 4],
        }
    }

    fn executor(
        candidates: Vec<RouteCandidate>,
        replies: Vec<FakeReply>,
        clock: FakeClock,
        reservations: Arc<ReservationRegistry>,
        policy: LocalRoutePolicy,
    ) -> (GatewayLlmExec, Arc<FakeSource>, Arc<FakeTransport>) {
        let generation_eligible = candidates
            .iter()
            .filter(|candidate| candidate.tool_calling && !is_independent_judge(candidate))
            .map(|candidate| candidate.deployment_id)
            .collect();
        executor_with_generation_ids(
            candidates,
            generation_eligible,
            replies,
            clock,
            reservations,
            policy,
        )
    }

    fn executor_with_generation_ids(
        candidates: Vec<RouteCandidate>,
        generation_eligible: HashSet<Uuid>,
        replies: Vec<FakeReply>,
        clock: FakeClock,
        reservations: Arc<ReservationRegistry>,
        policy: LocalRoutePolicy,
    ) -> (GatewayLlmExec, Arc<FakeSource>, Arc<FakeTransport>) {
        let source = Arc::new(FakeSource {
            candidates,
            generation_eligible,
            calls: AtomicUsize::new(0),
        });
        let transport = Arc::new(FakeTransport {
            replies: Mutex::new(replies.into()),
            seen: Mutex::new(Vec::new()),
            clock: clock.clone(),
        });
        let exec = GatewayLlmExec::with_test_components(
            source.clone(),
            transport.clone(),
            reservations,
            Arc::new(clock),
            WorkloadClass::CodeOneShot,
            policy,
        );
        (exec, source, transport)
    }

    #[test]
    fn remaining_budget_refuses_short_start_and_caps_attempt() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(5);
        assert_eq!(
            remaining_attempt_budget(
                now,
                deadline,
                Duration::from_secs(1),
                Duration::from_secs(4),
                Duration::from_secs(3),
                Duration::from_secs(2),
            ),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            remaining_attempt_budget(
                deadline - Duration::from_millis(50),
                deadline,
                Duration::from_millis(100),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
            ),
            None
        );
    }

    #[tokio::test]
    async fn timeout_retries_a_distinct_deployment_then_succeeds() {
        let clock = FakeClock::new();
        let (exec, source, transport) = executor(
            vec![candidate(1), candidate(2)],
            vec![
                FakeReply::Failure(FailureReasonCode::Timeout, "slow"),
                FakeReply::Payload(valid("generated code")),
            ],
            clock,
            Arc::new(ReservationRegistry::default()),
            test_policy(),
        );
        let output = exec
            .complete(1, "write code", 256, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(output, "generated code");
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *transport.seen.lock().unwrap(),
            vec![Uuid::from_u128(1), Uuid::from_u128(2)]
        );
        let evidence = exec.evidence();
        assert_eq!(evidence.attempts.len(), 2);
        assert_eq!(
            evidence.attempts[0].reason_code,
            Some(FailureReasonCode::Timeout)
        );
        assert_eq!(evidence.attempts[1].outcome, "ok");
        assert!(!evidence.cloud_fallback);
    }

    #[tokio::test]
    async fn single_strategy_is_confined_to_the_requested_tier() {
        let (exec, source, _) = executor(
            vec![candidate_at_tier(3, 3)],
            vec![FakeReply::Payload(valid("one answer"))],
            FakeClock::new(),
            Arc::new(ReservationRegistry::default()),
            test_policy(),
        );
        let result = crate::strategy_dispatch::dispatch_strategy(
            &exec,
            "answer once",
            "single",
            Some(3),
            ff_orchestrator::cascade_strategy::ValidatorKind::None,
        )
        .await
        .unwrap();
        assert_eq!(result.output, "one answer");
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
        assert_eq!(result.execution.attempts.len(), 1);
        assert_eq!(result.execution.attempts[0].tier, 3);
        assert_eq!(result.execution.attempts[0].catalog_tier, 3);
        assert_eq!(result.execution.attempts[0].role, AttemptRole::Completion);
    }

    #[tokio::test]
    async fn tier_selection_is_exact_then_upward_only() {
        let (exact, _, exact_transport) = executor(
            vec![
                candidate_at_tier(32, 3),
                candidate_at_tier(30, 1),
                candidate_at_tier(31, 2),
            ],
            vec![FakeReply::Payload(valid("tier two"))],
            FakeClock::new(),
            Arc::new(ReservationRegistry::default()),
            test_policy(),
        );
        assert_eq!(
            exact
                .complete(2, "write", 256, Duration::from_secs(5))
                .await
                .unwrap(),
            "tier two"
        );
        assert_eq!(
            *exact_transport.seen.lock().unwrap(),
            vec![Uuid::from_u128(31)]
        );
        assert_eq!(exact.evidence().attempts[0].catalog_tier, 2);

        let (upward, _, upward_transport) = executor(
            vec![candidate_at_tier(33, 1), candidate_at_tier(34, 3)],
            vec![FakeReply::Payload(valid("tier three"))],
            FakeClock::new(),
            Arc::new(ReservationRegistry::default()),
            test_policy(),
        );
        upward
            .complete(2, "write", 256, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(
            *upward_transport.seen.lock().unwrap(),
            vec![Uuid::from_u128(34)]
        );
        assert_eq!(upward.evidence().attempts[0].catalog_tier, 3);

        let (no_downgrade, _, no_downgrade_transport) = executor(
            vec![candidate_at_tier(35, 2)],
            Vec::new(),
            FakeClock::new(),
            Arc::new(ReservationRegistry::default()),
            test_policy(),
        );
        let error = no_downgrade
            .complete(3, "write", 256, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(error.starts_with("unavailable:"), "{error}");
        assert!(no_downgrade_transport.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn code_generation_keeps_workload_affinity_over_lower_tier_research() {
        let mut lucy = candidate_at_tier(46, 1);
        lucy.catalog_id = Some("lucy-1-7b".into());
        lucy.catalog_name = Some("Lucy 1 7B".into());
        lucy.family = Some("lucy".into());
        let glm = candidate_at_tier(47, 2);
        let (exec, _, transport) = executor_with_generation_ids(
            vec![lucy, glm],
            HashSet::from([Uuid::from_u128(47)]),
            vec![FakeReply::Payload(valid("code answer"))],
            FakeClock::new(),
            Arc::new(ReservationRegistry::default()),
            test_policy(),
        );
        exec.complete(1, "write code", 256, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(*transport.seen.lock().unwrap(), vec![Uuid::from_u128(47)]);
        assert_eq!(exec.evidence().attempts[0].catalog_tier, 2);
    }

    #[tokio::test]
    async fn upward_escalation_uses_the_selected_catalog_tier_timeout() {
        let mut policy = test_policy();
        policy.max_attempt_timeout = Duration::from_secs(1);
        policy.tier_timeouts = [
            Duration::from_millis(100),
            Duration::from_millis(200),
            Duration::from_millis(700),
            Duration::from_millis(900),
        ];
        let (exec, _, _) = executor(
            vec![candidate_at_tier(36, 3)],
            vec![FakeReply::Payload(valid("tier three"))],
            FakeClock::new(),
            Arc::new(ReservationRegistry::default()),
            policy,
        );
        exec.complete(1, "write", 256, Duration::from_secs(5))
            .await
            .unwrap();
        let attempt = &exec.evidence().attempts[0];
        assert_eq!(attempt.tier, 1);
        assert_eq!(attempt.catalog_tier, 3);
        assert_eq!(attempt.attempt_timeout_ms, 700);
    }

    #[tokio::test]
    async fn judge_requires_an_independent_gemma_deployment_and_never_self_grades() {
        let generator = candidate(37);
        let (without_judge, _, without_judge_transport) = executor(
            vec![generator.clone()],
            vec![FakeReply::Payload(valid("draft"))],
            FakeClock::new(),
            Arc::new(ReservationRegistry::default()),
            test_policy(),
        );
        without_judge
            .complete(1, "write", 256, Duration::from_secs(5))
            .await
            .unwrap();
        let error = without_judge
            .judge("score", 256, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(error.starts_with("unavailable:"), "{error}");
        assert_eq!(without_judge_transport.seen.lock().unwrap().len(), 1);

        let judge = judge_candidate(38);
        let (with_judge, _, with_judge_transport) = executor(
            vec![generator, judge],
            vec![
                FakeReply::Payload(valid("draft")),
                FakeReply::Payload(valid("8")),
            ],
            FakeClock::new(),
            Arc::new(ReservationRegistry::default()),
            test_policy(),
        );
        with_judge
            .complete(1, "write", 256, Duration::from_secs(5))
            .await
            .unwrap();
        with_judge
            .judge("score", 256, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(
            *with_judge_transport.seen.lock().unwrap(),
            vec![Uuid::from_u128(37), Uuid::from_u128(38)]
        );
        assert_eq!(with_judge.evidence().attempts[1].role, AttemptRole::Judge);
    }

    #[tokio::test]
    async fn request_global_failures_are_safe_across_disjoint_role_pools() {
        let mut policy = test_policy();
        policy.max_distinct_attempts = 1;
        let (exec, _, transport) = executor(
            vec![candidate(48), judge_candidate(49)],
            vec![
                FakeReply::Failure(FailureReasonCode::Timeout, "judge timeout"),
                FakeReply::Payload(valid("generator remains eligible")),
            ],
            FakeClock::new(),
            Arc::new(ReservationRegistry::default()),
            policy,
        );
        assert!(
            exec.judge("score", 256, Duration::from_secs(5))
                .await
                .is_err()
        );
        assert_eq!(
            exec.complete(1, "write", 256, Duration::from_secs(5))
                .await
                .unwrap(),
            "generator remains eligible"
        );
        assert_eq!(
            *transport.seen.lock().unwrap(),
            vec![Uuid::from_u128(49), Uuid::from_u128(48)]
        );
    }

    #[tokio::test]
    async fn judge_escalate_uses_independent_judge_and_types_terminal_failures() {
        let (success, _, success_transport) = executor(
            vec![candidate(50), candidate_at_tier(51, 3), judge_candidate(52)],
            vec![
                FakeReply::Payload(valid("answer")),
                FakeReply::Payload(valid("9")),
            ],
            FakeClock::new(),
            Arc::new(ReservationRegistry::default()),
            test_policy(),
        );
        let result = crate::strategy_dispatch::dispatch_strategy(
            &success,
            "answer",
            "judge_escalate",
            Some(2),
            ff_orchestrator::cascade_strategy::ValidatorKind::None,
        )
        .await
        .unwrap();
        assert_eq!(result.output, "answer");
        assert_eq!(
            *success_transport.seen.lock().unwrap(),
            vec![Uuid::from_u128(50), Uuid::from_u128(52)]
        );

        let (unparseable, _, _) = executor(
            vec![candidate(53), candidate_at_tier(54, 3), judge_candidate(55)],
            vec![
                FakeReply::Payload(valid("first")),
                FakeReply::Payload(valid("not a score")),
                FakeReply::Payload(valid("second")),
                FakeReply::Payload(valid("still not a score")),
            ],
            FakeClock::new(),
            Arc::new(ReservationRegistry::default()),
            test_policy(),
        );
        let stale_completion_failure = unparseable
            .complete(4, "unavailable tier", 256, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(stale_completion_failure.starts_with("unavailable:"));
        let failure = crate::strategy_dispatch::dispatch_strategy(
            &unparseable,
            "answer",
            "judge_escalate",
            Some(2),
            ff_orchestrator::cascade_strategy::ValidatorKind::None,
        )
        .await
        .unwrap_err();
        assert_eq!(failure.reason_code, FailureReasonCode::InvalidResponse);

        let (unavailable, _, _) = executor(
            vec![candidate(56), candidate_at_tier(57, 3)],
            vec![
                FakeReply::Payload(valid("first")),
                FakeReply::Payload(valid("second")),
            ],
            FakeClock::new(),
            Arc::new(ReservationRegistry::default()),
            test_policy(),
        );
        let failure = crate::strategy_dispatch::dispatch_strategy(
            &unavailable,
            "answer",
            "judge_escalate",
            Some(2),
            ff_orchestrator::cascade_strategy::ValidatorKind::None,
        )
        .await
        .unwrap_err();
        assert_eq!(failure.reason_code, FailureReasonCode::Unavailable);
    }

    #[test]
    fn erased_attestation_errors_fail_closed_without_message_inference() {
        assert_eq!(
            classify_attestation_failure(&anyhow::anyhow!("GET endpoint timed out")),
            FailureReasonCode::InvalidResponse
        );
        assert_eq!(
            classify_attestation_failure(&anyhow::anyhow!("connection refused")),
            FailureReasonCode::InvalidResponse
        );
        assert_eq!(
            classify_attestation_failure(&anyhow::anyhow!("served model identity mismatch")),
            FailureReasonCode::InvalidResponse
        );
    }

    #[tokio::test]
    async fn cascade_advances_with_one_snapshot_and_full_ledger() {
        let (exec, source, _) = executor(
            vec![candidate(4), candidate(5), judge_candidate(104)],
            vec![
                FakeReply::Payload(valid(r#"{"draft":1}"#)),
                FakeReply::Payload(valid("5")),
                FakeReply::Payload(valid(r#"{"final":2}"#)),
                FakeReply::Payload(valid("9")),
            ],
            FakeClock::new(),
            Arc::new(ReservationRegistry::default()),
            test_policy(),
        );
        let result = crate::strategy_dispatch::dispatch_strategy(
            &exec,
            "return JSON",
            "cascade",
            None,
            ff_orchestrator::cascade_strategy::ValidatorKind::Json,
        )
        .await
        .unwrap();
        assert_eq!(result.output, r#"{"final":2}"#);
        assert_eq!(result.early_exit_at_tier, Some(2));
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
        assert_eq!(result.execution.attempts.len(), 4);
        assert_eq!(
            result
                .execution
                .attempts
                .iter()
                .map(|attempt| (attempt.role, attempt.tier))
                .collect::<Vec<_>>(),
            vec![
                (AttemptRole::Completion, 1),
                (AttemptRole::Judge, 1),
                (AttemptRole::Completion, 2),
                (AttemptRole::Judge, 1),
            ]
        );
    }

    #[tokio::test]
    async fn cascade_fails_closed_without_an_independent_judge() {
        let (exec, source, _) = executor(
            vec![candidate(44), candidate_at_tier(45, 3)],
            vec![
                FakeReply::Payload(valid(r#"{"draft":1}"#)),
                FakeReply::Payload(valid(r#"{"refined":2}"#)),
                FakeReply::Payload(valid(r#"{"final":3}"#)),
            ],
            FakeClock::new(),
            Arc::new(ReservationRegistry::default()),
            test_policy(),
        );
        let failure = crate::strategy_dispatch::dispatch_strategy(
            &exec,
            "return JSON",
            "cascade",
            None,
            ff_orchestrator::cascade_strategy::ValidatorKind::Json,
        )
        .await
        .unwrap_err();
        assert_eq!(failure.reason_code, FailureReasonCode::Unavailable);
        assert!(failure.message.contains("independent judge score"));
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
        assert!(
            failure
                .execution
                .attempts
                .iter()
                .all(|attempt| attempt.role == AttemptRole::Completion)
        );
    }

    #[tokio::test]
    async fn cascade_uses_one_absolute_deadline_across_generation_and_judge() {
        let clock = FakeClock::new();
        let mut policy = test_policy();
        policy.total_timeout = Duration::from_millis(1_500);
        policy.min_start_budget = Duration::from_millis(50);
        let (exec, source, _) = executor(
            vec![candidate(6), candidate(7), judge_candidate(106)],
            vec![
                FakeReply::AdvanceThenPayload(Duration::from_millis(700), valid(r#"{"draft":1}"#)),
                FakeReply::AdvanceThenPayload(Duration::from_millis(700), valid("5")),
                FakeReply::AdvanceThenPayload(Duration::from_millis(200), valid(r#"{"late":2}"#)),
            ],
            clock,
            Arc::new(ReservationRegistry::default()),
            policy,
        );
        let failure = crate::strategy_dispatch::dispatch_strategy(
            &exec,
            "return JSON",
            "cascade",
            None,
            ff_orchestrator::cascade_strategy::ValidatorKind::Json,
        )
        .await
        .unwrap_err();
        assert_eq!(failure.reason_code, FailureReasonCode::DeadlineExceeded);
        assert_eq!(failure.execution.attempts.len(), 3);
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cascade_final_semantic_validator_fails_closed() {
        let (exec, _, _) = executor(
            vec![candidate(8), candidate_at_tier(9, 3), judge_candidate(108)],
            vec![
                FakeReply::Payload(valid("not json one")),
                FakeReply::Payload(valid("5")),
                FakeReply::Payload(valid("not json two")),
                FakeReply::Payload(valid("5")),
                FakeReply::Payload(valid("not json three")),
            ],
            FakeClock::new(),
            Arc::new(ReservationRegistry::default()),
            test_policy(),
        );
        let failure = crate::strategy_dispatch::dispatch_strategy(
            &exec,
            "return JSON",
            "cascade",
            None,
            ff_orchestrator::cascade_strategy::ValidatorKind::Json,
        )
        .await
        .unwrap_err();
        assert_eq!(failure.reason_code, FailureReasonCode::InvalidResponse);
        assert!(failure.message.contains("semantic validation"));
        assert_eq!(failure.execution.attempts.len(), 5);
    }

    #[tokio::test]
    async fn semantic_failure_is_not_mislabeled_by_an_earlier_judge_failure() {
        let mut policy = test_policy();
        policy.max_distinct_attempts = 2;
        let (exec, _, _) = executor(
            vec![
                candidate_at_tier(40, 2),
                candidate_at_tier(42, 3),
                judge_candidate(41),
                judge_candidate(43),
            ],
            vec![
                FakeReply::Payload(valid("not json one")),
                FakeReply::Failure(FailureReasonCode::Timeout, "judge timed out"),
                FakeReply::Payload(valid("5")),
                FakeReply::Payload(valid("not json two")),
                FakeReply::Payload(valid("5")),
                FakeReply::Payload(valid("not json three")),
            ],
            FakeClock::new(),
            Arc::new(ReservationRegistry::default()),
            policy,
        );
        let failure = crate::strategy_dispatch::dispatch_strategy(
            &exec,
            "return JSON",
            "cascade",
            None,
            ff_orchestrator::cascade_strategy::ValidatorKind::Json,
        )
        .await
        .unwrap_err();
        assert_eq!(failure.reason_code, FailureReasonCode::InvalidResponse);
        assert!(failure.execution.attempts.iter().any(|attempt| {
            attempt.role == AttemptRole::Judge
                && attempt.reason_code == Some(FailureReasonCode::Timeout)
        }));
        assert!(failure.message.contains("semantic validation"));
    }

    #[tokio::test]
    async fn empty_malformed_and_length_responses_fail_closed() {
        for payload in [
            json!({"choices": []}),
            json!({"choices": [{"message": {"content": "   "}, "finish_reason": "stop"}]}),
            json!({"choices": [{"message": {"content": "partial"}, "finish_reason": "length"}]}),
        ] {
            let mut policy = test_policy();
            policy.max_distinct_attempts = 1;
            let (exec, _, _) = executor(
                vec![candidate(10)],
                vec![FakeReply::Payload(payload)],
                FakeClock::new(),
                Arc::new(ReservationRegistry::default()),
                policy,
            );
            let error = exec
                .complete(1, "write code", 256, Duration::from_secs(5))
                .await
                .unwrap_err();
            assert!(error.starts_with("invalid_response:"), "{error}");
            assert_eq!(
                exec.evidence().attempts[0].reason_code,
                Some(FailureReasonCode::InvalidResponse)
            );
        }
    }

    #[tokio::test]
    async fn no_candidate_is_typed_unavailable() {
        let (exec, _, _) = executor(
            Vec::new(),
            Vec::new(),
            FakeClock::new(),
            Arc::new(ReservationRegistry::default()),
            test_policy(),
        );
        let error = exec
            .complete(2, "reason", 256, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(error.starts_with("unavailable:"));
        assert!(exec.evidence().attempts.is_empty());
    }

    #[tokio::test]
    async fn body_completion_after_absolute_deadline_is_rejected() {
        let clock = FakeClock::new();
        let mut policy = test_policy();
        policy.total_timeout = Duration::from_secs(1);
        policy.max_attempt_timeout = Duration::from_secs(2);
        let (exec, _, _) = executor(
            vec![candidate(20)],
            vec![FakeReply::AdvanceThenPayload(
                Duration::from_secs(2),
                valid("late"),
            )],
            clock,
            Arc::new(ReservationRegistry::default()),
            policy,
        );
        let error = exec
            .complete(1, "write", 256, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(error.starts_with("deadline_exceeded:"), "{error}");
    }

    #[tokio::test]
    async fn cooldown_is_visible_to_two_executor_instances_then_expires() {
        let clock = FakeClock::new();
        let reservations = Arc::new(ReservationRegistry::default());
        let mut policy = test_policy();
        policy.max_distinct_attempts = 1;
        let (first, _, first_transport) = executor(
            vec![candidate(21)],
            vec![FakeReply::Failure(
                FailureReasonCode::Unavailable,
                "server down",
            )],
            clock.clone(),
            reservations.clone(),
            policy.clone(),
        );
        assert!(
            first
                .complete(1, "write", 256, Duration::from_secs(5))
                .await
                .is_err()
        );
        assert_eq!(first_transport.seen.lock().unwrap().len(), 1);

        let (second, _, second_transport) = executor(
            vec![candidate(21)],
            vec![FakeReply::Payload(valid("must not run"))],
            clock.clone(),
            reservations.clone(),
            policy.clone(),
        );
        let second_error = second
            .complete(1, "write", 256, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(second_error.starts_with("unavailable:"));
        assert!(second_transport.seen.lock().unwrap().is_empty());

        clock.advance(policy.cooldown + Duration::from_millis(1));
        let (third, _, third_transport) = executor(
            vec![candidate(21)],
            vec![FakeReply::Payload(valid("recovered"))],
            clock,
            reservations,
            policy,
        );
        assert_eq!(
            third
                .complete(1, "write", 256, Duration::from_secs(5))
                .await
                .unwrap(),
            "recovered"
        );
        assert_eq!(third_transport.seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn concurrent_reservations_are_distinct_and_release() {
        let registry = Arc::new(ReservationRegistry::default());
        let policy = test_policy();
        let now = Instant::now();
        let ids = vec![Uuid::from_u128(31), Uuid::from_u128(32)];
        let first = registry
            .reserve_next(&ids, &HashSet::new(), now, &policy)
            .unwrap();
        let second = registry
            .reserve_next(&ids, &HashSet::new(), now, &policy)
            .unwrap();
        assert_ne!(first.deployment_id, second.deployment_id);
        let first_id = first.deployment_id;
        first.finish(true, now);
        second.finish(true, now);
        assert_eq!(
            registry
                .reserve_next(&ids, &HashSet::new(), now, &policy)
                .unwrap()
                .deployment_id,
            first_id
        );
    }

    #[test]
    fn cooldown_is_shared_across_executor_registrants_and_expires_half_open() {
        let registry = Arc::new(ReservationRegistry::default());
        let policy = test_policy();
        let id = Uuid::from_u128(40);
        let ids = vec![id];
        let now = Instant::now();
        registry
            .reserve_next(&ids, &HashSet::new(), now, &policy)
            .unwrap()
            .finish(false, now);
        assert!(
            registry
                .reserve_next(&ids, &HashSet::new(), now, &policy)
                .is_none()
        );
        let after = now + policy.cooldown + Duration::from_millis(1);
        let half_open = registry
            .reserve_next(&ids, &HashSet::new(), after, &policy)
            .unwrap();
        assert!(
            registry
                .reserve_next(&ids, &HashSet::new(), after, &policy)
                .is_none()
        );
        half_open.finish(true, after);
    }

    #[test]
    fn registry_eviction_is_bounded() {
        let registry = Arc::new(ReservationRegistry::default());
        let mut policy = test_policy();
        policy.max_cooldown_entries = 2;
        let now = Instant::now();
        for value in 50..55 {
            let ids = vec![Uuid::from_u128(value)];
            if let Some(lease) = registry.reserve_next(&ids, &HashSet::new(), now, &policy) {
                lease.finish(false, now);
            }
        }
        assert!(registry.entries.lock().unwrap().len() <= 2);
    }

    #[test]
    fn policy_overlay_uses_existing_tier_timeouts() {
        let timeouts = LlmTimeouts {
            tier1: Some(11),
            tier2: Some(22),
            tier3: Some(33),
            tier4: Some(44),
        };
        let policy = LocalRoutePolicy::from_config(
            &timeouts,
            Some(
                r#"{"total_timeout_ms":9000,"min_start_budget_ms":100,"max_distinct_attempts":2}"#,
            ),
        )
        .unwrap();
        assert_eq!(policy.total_timeout, Duration::from_secs(9));
        assert_eq!(policy.tier_timeout(3), Duration::from_secs(33));
        assert_eq!(policy.max_distinct_attempts, 2);
    }

    #[test]
    fn policy_defaults_and_oversized_overrides_obey_server_hard_ceilings() {
        let timeouts = LlmTimeouts {
            tier1: None,
            tier2: None,
            tier3: None,
            tier4: None,
        };
        let defaults = LocalRoutePolicy::from_config(&timeouts, None).unwrap();
        assert_eq!(defaults.total_timeout, Duration::from_millis(105_000));
        assert_eq!(defaults.max_attempt_timeout, Duration::from_millis(90_000));

        let clamped = LocalRoutePolicy::from_config(
            &timeouts,
            Some(
                r#"{"total_timeout_ms":999999,"max_attempt_timeout_ms":999999,"min_start_budget_ms":100}"#,
            ),
        )
        .unwrap();
        assert_eq!(clamped.total_timeout, defaults.total_timeout);
        assert_eq!(clamped.max_attempt_timeout, defaults.max_attempt_timeout);
        assert_eq!(
            remaining_attempt_budget(
                Instant::now(),
                Instant::now() + clamped.total_timeout,
                clamped.min_start_budget,
                Duration::from_secs(300),
                Duration::from_secs(300),
                clamped.max_attempt_timeout,
            ),
            Some(Duration::from_millis(SERVER_HARD_MAX_ATTEMPT_TIMEOUT_MS))
        );
    }

    #[test]
    fn policy_preserves_lower_overrides_and_rejects_invalid_clamped_combinations() {
        let timeouts = LlmTimeouts {
            tier1: None,
            tier2: None,
            tier3: None,
            tier4: None,
        };
        let lower = LocalRoutePolicy::from_config(
            &timeouts,
            Some(
                r#"{"total_timeout_ms":7000,"max_attempt_timeout_ms":6000,"min_start_budget_ms":100}"#,
            ),
        )
        .unwrap();
        assert_eq!(lower.total_timeout, Duration::from_secs(7));
        assert_eq!(lower.max_attempt_timeout, Duration::from_secs(6));

        let invalid = LocalRoutePolicy::from_config(
            &timeouts,
            Some(r#"{"total_timeout_ms":999999,"min_start_budget_ms":110000}"#),
        )
        .unwrap_err();
        assert!(invalid.contains("min_start_budget_ms exceeds total_timeout_ms"));
    }

    #[test]
    fn executor_reapplies_hard_ceilings_to_programmatic_policy() {
        let mut policy = test_policy();
        policy.total_timeout = Duration::from_secs(180);
        policy.max_attempt_timeout = Duration::from_secs(120);
        let (exec, _, _) = executor(
            vec![candidate(70)],
            vec![FakeReply::Payload(valid("unused"))],
            FakeClock::new(),
            Arc::new(ReservationRegistry::default()),
            policy,
        );
        assert_eq!(
            exec.policy.total_timeout,
            Duration::from_millis(SERVER_HARD_TOTAL_TIMEOUT_MS)
        );
        assert_eq!(
            exec.policy.max_attempt_timeout,
            Duration::from_millis(SERVER_HARD_MAX_ATTEMPT_TIMEOUT_MS)
        );
    }

    #[tokio::test]
    async fn hanging_transport_is_bounded_and_recorded_as_typed_timeout() {
        let mut policy = test_policy();
        policy.total_timeout = Duration::from_millis(100);
        policy.min_start_budget = Duration::from_millis(1);
        policy.max_attempt_timeout = Duration::from_millis(10);
        policy.max_distinct_attempts = 1;
        policy.tier_timeouts = [Duration::from_secs(1); 4];
        let (exec, _, _) = executor(
            vec![candidate(71)],
            vec![FakeReply::Never],
            FakeClock::new(),
            Arc::new(ReservationRegistry::default()),
            policy,
        );

        let error = tokio::time::timeout(
            Duration::from_secs(1),
            exec.complete(1, "write", 256, Duration::from_secs(1)),
        )
        .await
        .expect("the server-side attempt timeout must resolve first")
        .unwrap_err();
        assert!(error.starts_with("timeout:"), "{error}");
        let evidence = exec.evidence();
        assert_eq!(evidence.attempts.len(), 1);
        assert_eq!(evidence.attempts[0].attempt_timeout_ms, 10);
        assert_eq!(
            evidence.attempts[0].reason_code,
            Some(FailureReasonCode::Timeout)
        );
        assert_eq!(evidence.last_failure, Some(FailureReasonCode::Timeout));
    }
}
