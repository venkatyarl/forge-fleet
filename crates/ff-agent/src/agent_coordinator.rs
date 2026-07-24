//! Agent coordinator — fleet-wide task dispatch.
//!
//! Connects `work_items` → pick worker (sub_agent slot) → call local LLM
//! on the chosen computer → persist result into `work_outputs`.
//!
//! This is intentionally a minimal dispatch layer, not a full agent
//! runtime. The flow:
//!
//! 1. [`AgentCoordinator::pick_worker`] finds an idle `sub_agents` row
//!    (optionally constrained to a named computer).
//! 2. [`AgentCoordinator::claim_slot`] transitions it to `busy` via
//!    `UPDATE ... WHERE status='idle'` so concurrent dispatchers cannot
//!    grab the same slot.
//! 3. [`AgentCoordinator::dispatch_task`] ties it all together: claims a
//!    slot, HTTP-POSTs the prompt to the chosen computer's local LLM
//!    server (via Pulse beats), persists the response into
//!    `work_outputs` with provenance, and releases the slot.
//!
//! Slot seeding lives in [`ensure_sub_agent_rows`] — the daemon calls it
//! once at startup per computer, so every live computer has at least one
//! worker row ready.
//!
//! A special canonical slot (slot 99, kind='canonical') can be registered per
//! computer pointing at `~/projects/{project}`. It is treated as the lowest-
//! preference slot and is only usable when the tree is clean and no interactive
//! operator session owns it.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;
use thiserror::Error;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use uuid::Uuid;

use ff_pulse::reader::PulseReader;

/// Errors returned by [`AgentCoordinator`].
#[derive(Debug, Error)]
pub enum CoordError {
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),

    #[error("pulse: {0}")]
    Pulse(String),

    #[error("no idle sub-agent slot available{ctx}", ctx = .0.as_deref().map(|c| format!(" ({c})")).unwrap_or_default())]
    NoSlot(Option<String>),

    #[error("unknown computer '{0}'")]
    UnknownComputer(String),

    #[error("computer '{0}' has no active LLM server")]
    NoLlmServer(String),

    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    #[error("upstream LLM returned no response content")]
    EmptyResponse,

    #[error("internal: {0}")]
    Internal(String),
}

impl CoordError {
    /// A *transient* failure of the LLM CALL — the chosen endpoint was busy,
    /// unreachable, or returned nothing. Re-dispatching the same prompt to a
    /// DIFFERENT LLM-capable slot may succeed (GAP-G). Slot/DB/validation
    /// errors are NOT transient (retrying the same way won't help).
    fn is_transient(&self) -> bool {
        matches!(
            self,
            CoordError::Http(_) | CoordError::NoLlmServer(_) | CoordError::EmptyResponse
        )
    }
}

/// Result of a successful dispatch.
#[derive(Debug, Clone)]
pub struct DispatchReceipt {
    pub work_item_id: Uuid,
    pub sub_agent_id: Uuid,
    pub work_output_id: Option<Uuid>,
    pub computer_name: String,
    pub model_id: String,
    pub response_text: String,
    pub duration_ms: u64,
}

/// A worker slot returned by [`AgentCoordinator::pick_worker`].
#[derive(Debug, Clone)]
pub struct WorkerSlot {
    pub sub_agent_id: Uuid,
    pub computer_id: Uuid,
    pub computer_name: String,
    pub slot: i32,
}

/// Fleet-wide agent coordinator. Cheap to clone (holds `Arc`s).
#[derive(Clone)]
pub struct AgentCoordinator {
    pg: PgPool,
    pulse: Arc<PulseReader>,
    http: reqwest::Client,
    upstream_timeout: Duration,
}

impl AgentCoordinator {
    /// Build a coordinator on top of an existing Postgres pool and a
    /// `PulseReader` pointed at the fleet Redis.
    pub fn new(pg: PgPool, pulse: Arc<PulseReader>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .expect("build reqwest client");
        Self {
            pg,
            pulse,
            http,
            upstream_timeout: Duration::from_secs(120),
        }
    }

    /// Find an idle `sub_agents` row. If `target` is `Some(name)`, only
    /// considers slots on that computer and returns [`CoordError::NoSlot`]
    /// if none are idle there. With `None`, falls back to any idle slot
    /// across the fleet (prefers online computers).
    pub async fn pick_worker(
        &self,
        target: Option<String>,
        exclude: &[String],
    ) -> Result<Option<WorkerSlot>, CoordError> {
        if let Some(name) = target {
            // An explicitly-excluded target (a GAP-G retry already failed there)
            // has no usable slot — don't re-pick the same dead endpoint.
            if exclude.iter().any(|e| e.eq_ignore_ascii_case(&name)) {
                return Ok(None);
            }
            // Validate computer exists, then look for an idle slot on it.
            let computer: Option<(Uuid, String, String)> =
                sqlx::query_as("SELECT id, name, status FROM computers WHERE name = $1")
                    .bind(&name)
                    .fetch_optional(&self.pg)
                    .await?;
            let Some((computer_id, computer_name, _status)) = computer else {
                return Err(CoordError::UnknownComputer(name));
            };
            let row: Option<(Uuid, i32)> = sqlx::query_as(
                "SELECT id, slot FROM sub_agents \
                 WHERE computer_id = $1 AND status = 'idle' \
                 ORDER BY slot ASC LIMIT 1",
            )
            .bind(computer_id)
            .fetch_optional(&self.pg)
            .await?;
            Ok(row.map(|(sub_agent_id, slot)| WorkerSlot {
                sub_agent_id,
                computer_id,
                computer_name,
                slot,
            }))
        } else {
            // Any idle slot, preferring (GAP-F) computers that actually have a
            // healthy tool-capable LLM, then online; WITHIN that tier pick a
            // RANDOM idle slot (GAP-H), not the lowest. Without the LLM key the
            // picker landed on LLM-less hosts ("no active LLM"); with a
            // DETERMINISTIC final key (slot ASC) every concurrent dispatcher
            // picked the SAME top slot and marched in lockstep, exhausting the
            // claim-CAS retry budget ("pool contended" — 2/10 in a smoke test).
            // `random()` spreads N concurrent callers across the idle slots of
            // the preferred computers, collapsing CAS contention. Still a
            // PREFERENCE, not a filter: if only LLM-less hosts have idle slots
            // one is still returned (no regression).
            let row: Option<(Uuid, Uuid, String, i32)> = sqlx::query_as(
                "SELECT sa.id, sa.computer_id, c.name, sa.slot \
                 FROM sub_agents sa \
                 JOIN computers c ON c.id = sa.computer_id \
                 WHERE sa.status = 'idle' \
                   AND c.name <> ALL($1) \
                 ORDER BY \
                   EXISTS ( \
                     SELECT 1 FROM fleet_model_deployments d \
                     JOIN fleet_model_catalog cat ON cat.id = d.catalog_id \
                     WHERE d.worker_name = c.name \
                       AND d.health_status = 'healthy' \
                       AND d.desired_state = 'active' \
                       AND cat.tool_calling = true \
                   ) DESC, \
                   (c.status = 'online') DESC, random() \
                 LIMIT 1",
            )
            .bind(exclude)
            .fetch_optional(&self.pg)
            .await?;
            Ok(row.map(
                |(sub_agent_id, computer_id, computer_name, slot)| WorkerSlot {
                    sub_agent_id,
                    computer_id,
                    computer_name,
                    slot,
                },
            ))
        }
    }

    /// Transactionally mark a slot as busy for `work_item_id`. Returns
    /// `true` if we grabbed the slot, `false` if another dispatcher beat
    /// us to it.
    pub async fn claim_slot(
        &self,
        sub_agent_id: Uuid,
        work_item_id: Uuid,
    ) -> Result<bool, CoordError> {
        let now = Utc::now();
        let affected = sqlx::query(
            "UPDATE sub_agents \
             SET status = 'busy', current_work_item_id = $2, started_at = $3, last_heartbeat_at = $3 \
             WHERE id = $1 AND status = 'idle'",
        )
        .bind(sub_agent_id)
        .bind(work_item_id)
        .bind(now)
        .execute(&self.pg)
        .await?
        .rows_affected();
        Ok(affected == 1)
    }

    /// Release a slot back to `idle` (or `error` if the dispatch failed).
    /// `outcome` is one of `"ok" | "error"`.
    pub async fn release_slot(&self, sub_agent_id: Uuid, outcome: &str) -> Result<(), CoordError> {
        let final_status = if outcome == "ok" { "idle" } else { "error" };
        sqlx::query(
            "UPDATE sub_agents \
             SET status = $2, current_work_item_id = NULL, started_at = NULL, \
                 last_heartbeat_at = NOW() \
             WHERE id = $1",
        )
        .bind(sub_agent_id)
        .bind(final_status)
        .execute(&self.pg)
        .await?;
        Ok(())
    }

    /// End-to-end dispatch. `target_computer` is optional — if `None`, the
    /// coordinator picks any idle slot fleet-wide.
    pub async fn dispatch_task(
        &self,
        work_item_id: Uuid,
        prompt: String,
        target_computer: Option<String>,
    ) -> Result<DispatchReceipt, CoordError> {
        // GAP-G: retry the whole dispatch on a TRANSIENT LLM-call failure (a
        // busy/unreachable endpoint or empty response) by re-dispatching to
        // ANOTHER LLM-capable slot, excluding the computer that just failed. One
        // slow/overloaded endpoint under concurrent load no longer fails the
        // caller while other capable hosts are idle (observed 2/8 in a smoke
        // test). Bounded; non-transient errors (DB/validation) return at once.
        const MAX_LLM_ATTEMPTS: usize = 3;
        let mut excluded: Vec<String> = Vec::new();
        let mut last_transient: Option<CoordError> = None;

        for _ in 0..MAX_LLM_ATTEMPTS {
            // 1-2. Pick + claim an idle slot, retrying on CAS contention. Under
            // concurrent multi-caller dispatch several dispatchers race for the
            // same idle slot; `claim_slot`'s compare-and-swap lets exactly one
            // win, and the losers RE-PICK rather than spuriously failing (GAP-A).
            // The pick honours `excluded` so a GAP-G retry skips the dead host.
            const MAX_CLAIM_ATTEMPTS: usize = 8;
            let mut claimed_slot: Option<WorkerSlot> = None;
            for _ in 0..MAX_CLAIM_ATTEMPTS {
                let Some(candidate) = self.pick_worker(target_computer.clone(), &excluded).await?
                else {
                    // No idle slot (saturation, or all candidates excluded).
                    // Prefer surfacing the real LLM error over a bare NoSlot.
                    return Err(last_transient
                        .unwrap_or_else(|| CoordError::NoSlot(target_computer.clone())));
                };
                if self
                    .claim_slot(candidate.sub_agent_id, work_item_id)
                    .await?
                {
                    claimed_slot = Some(candidate);
                    break;
                }
                // Lost the CAS to a concurrent dispatcher; re-pick a fresh slot.
            }
            let Some(slot) = claimed_slot else {
                return Err(CoordError::NoSlot(Some(format!(
                    "all idle {} slot(s) lost to concurrent dispatchers after \
                     {MAX_CLAIM_ATTEMPTS} attempts (pool contended — retry shortly)",
                    target_computer.as_deref().unwrap_or("fleet"),
                ))));
            };

            // 3. Run the LLM call; always release the slot afterwards.
            let started = std::time::Instant::now();
            let result = self.run_and_persist(&slot, work_item_id, &prompt).await;
            match result {
                Ok(mut receipt) => {
                    if let Err(rel_err) = self.release_slot(slot.sub_agent_id, "ok").await {
                        tracing::warn!(sub_agent = %slot.sub_agent_id, error = %rel_err, "release_slot failed");
                    }
                    receipt.duration_ms =
                        started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                    return Ok(receipt);
                }
                Err(e) if e.is_transient() => {
                    // The SLOT is fine — the LLM endpoint was busy/unreachable.
                    // Free it (idle, not error) and try another capable host.
                    if let Err(rel_err) = self.release_slot(slot.sub_agent_id, "ok").await {
                        tracing::warn!(sub_agent = %slot.sub_agent_id, error = %rel_err, "release_slot failed");
                    }
                    tracing::warn!(
                        computer = %slot.computer_name, error = %e,
                        "dispatch: transient LLM-call failure — retrying on another LLM-capable slot"
                    );
                    excluded.push(slot.computer_name.clone());
                    last_transient = Some(e);
                    continue;
                }
                Err(e) => {
                    // Non-transient (DB/validation/internal): mark the slot
                    // `error` and surface immediately — a retry won't help.
                    if let Err(rel_err) = self.release_slot(slot.sub_agent_id, "error").await {
                        tracing::warn!(sub_agent = %slot.sub_agent_id, error = %rel_err, "release_slot failed");
                    }
                    return Err(e);
                }
            }
        }

        // Exhausted MAX_LLM_ATTEMPTS, every one a transient LLM failure.
        Err(last_transient.unwrap_or_else(|| CoordError::NoSlot(target_computer.clone())))
    }

    /// Internal: do the actual LLM call + `work_outputs` insert.
    async fn run_and_persist(
        &self,
        slot: &WorkerSlot,
        work_item_id: Uuid,
        prompt: &str,
    ) -> Result<DispatchReceipt, CoordError> {
        // Look up the computer's primary IP + an active LLM server from
        // Pulse. We pick the server with lowest queue_depth.
        let (endpoint, model_id) = self.pick_llm_server_for(&slot.computer_name).await?;

        // POST /v1/chat/completions with a minimal OpenAI-shape request.
        let url = ff_core::url::normalize_chat_completions_url(&endpoint);

        let body = json!({
            "model": model_id,
            "messages": [
                {"role": "user", "content": prompt}
            ],
            "stream": false,
            "max_tokens": 1024,
            "temperature": 0.2,
        });

        tracing::info!(
            %url,
            computer = %slot.computer_name,
            model = %model_id,
            work_item = %work_item_id,
            "agent_coordinator: dispatching to LLM"
        );

        let fut = self.http.post(&url).json(&body).send();
        let resp = tokio::time::timeout(self.upstream_timeout, fut)
            .await
            .map_err(|_| {
                CoordError::Internal(format!(
                    "upstream LLM timed out after {}s",
                    self.upstream_timeout.as_secs()
                ))
            })??;

        let status = resp.status();
        let v: Value = resp.json().await?;

        let response_text = v
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();

        if response_text.is_empty() {
            return Err(CoordError::EmptyResponse);
        }

        let prompt_tokens = v
            .get("usage")
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(|n| n.as_i64())
            .unwrap_or(0) as i32;
        let completion_tokens = v
            .get("usage")
            .and_then(|u| u.get("completion_tokens"))
            .and_then(|n| n.as_i64())
            .unwrap_or(0) as i32;

        // Persist into work_outputs. model_id is free-form (not FK'd) since
        // fleet-reported ids may not exist in model_catalog.
        let output_row: Option<(Uuid,)> = sqlx::query_as(
            "INSERT INTO work_outputs (\
                work_item_id, kind, title, \
                produced_by_agent, produced_on_computer, \
                llm_tokens_input, llm_tokens_output, \
                metadata \
             ) VALUES ($1, 'llm_response', $2, $3, $4, $5, $6, $7) \
             RETURNING id",
        )
        .bind(work_item_id)
        .bind(format!("agent dispatch: {}", truncate(prompt, 80)))
        .bind(format!("sub-agent-{}:{}", slot.computer_name, slot.slot))
        .bind(&slot.computer_name)
        .bind(prompt_tokens)
        .bind(completion_tokens)
        .bind(json!({
            "model_id": model_id,
            "upstream_status": status.as_u16(),
            "endpoint": url,
            "response_excerpt": truncate(&response_text, 400),
        }))
        .fetch_optional(&self.pg)
        .await?;

        // Best-effort: set the work_item status to 'done'.
        let _ = sqlx::query(
            "UPDATE work_items SET status = 'done', completed_at = NOW() \
             WHERE id = $1 AND status <> 'done'",
        )
        .bind(work_item_id)
        .execute(&self.pg)
        .await;

        Ok(DispatchReceipt {
            work_item_id,
            sub_agent_id: slot.sub_agent_id,
            work_output_id: output_row.map(|(id,)| id),
            computer_name: slot.computer_name.clone(),
            model_id,
            response_text,
            duration_ms: 0, // filled by caller
        })
    }

    /// Use Pulse beats to find an active+healthy LLM server on the named
    /// computer. Returns `(endpoint, model_id)`, with the endpoint's
    /// loopback host rewritten to the computer's primary IP.
    async fn pick_llm_server_for(
        &self,
        computer_name: &str,
    ) -> Result<(String, String), CoordError> {
        let beats = self
            .pulse
            .all_beats()
            .await
            .map_err(|e| CoordError::Pulse(e.to_string()))?;

        let Some(beat) = beats.into_iter().find(|b| b.computer_name == computer_name) else {
            return Err(CoordError::NoLlmServer(computer_name.to_string()));
        };

        // Pick the healthiest active server (lowest queue_depth).
        let mut servers: Vec<_> = beat
            .llm_servers
            .iter()
            .filter(|s| s.status == "active" && s.is_healthy)
            .collect();
        servers.sort_by_key(|s| s.queue_depth);
        let Some(server) = servers.first() else {
            return Err(CoordError::NoLlmServer(computer_name.to_string()));
        };

        let endpoint = rewrite_endpoint(&server.endpoint, &beat.network.primary_ip);
        Ok((endpoint, server.model.id.clone()))
    }
}

/// Replace a loopback host in `endpoint` with `primary_ip` so the caller
/// (running on a different machine) can reach the LLM server.
fn rewrite_endpoint(endpoint: &str, primary_ip: &str) -> String {
    if primary_ip.is_empty() {
        return endpoint.to_string();
    }
    for lb in ["127.0.0.1", "localhost", "0.0.0.0"] {
        let needle = format!("://{lb}");
        if let Some(idx) = endpoint.find(&needle) {
            let before = &endpoint[..idx + 3];
            let after = &endpoint[idx + needle.len()..];
            return format!("{before}{primary_ip}{after}");
        }
    }
    endpoint.to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.min(s.len())])
    }
}

/// Ensure at least `desired_count` sub_agent rows exist for the given
/// computer. Creates missing slots `0..desired_count` with workspace dirs
/// of the form `~/.forgefleet/sub-agents/sub-agent-{slot}/`. Existing rows (by
/// computer_id, slot) are left untouched.
pub async fn ensure_sub_agent_rows(
    pool: &PgPool,
    computer_id: Uuid,
    desired_count: u32,
) -> Result<u32, CoordError> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let mut created = 0u32;
    for slot in 0..desired_count as i32 {
        let workspace = format!("{home}/.forgefleet/sub-agents/sub-agent-{slot}");
        let result = sqlx::query(
            "INSERT INTO sub_agents (computer_id, slot, status, workspace_dir) \
             VALUES ($1, $2, 'idle', $3) \
             ON CONFLICT (computer_id, slot) DO NOTHING",
        )
        .bind(computer_id)
        .bind(slot)
        .bind(&workspace)
        .execute(pool)
        .await?;
        if result.rows_affected() == 1 {
            created += 1;
        }
    }
    Ok(created)
}

/// Reserved slot index for the canonical per-computer project checkout.
const CANONICAL_SLOT: i32 = 99;
/// Environment override for the canonical project name (defaults to "forge-fleet").
const CANONICAL_PROJECT_ENV: &str = "FORGEFLEET_CANONICAL_PROJECT";
/// How recently `.git/index` must have been touched before we consider an
/// operator actively editing the canonical tree (5 minutes).
const INTERACTIVE_SESSION_THRESHOLD_SECS: u64 = 300;

/// Expand a leading `~` to `$HOME`. Returns the input unchanged if there is no
/// leading tilde or `$HOME` is not set.
fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().to_string();
        }
    }
    p.to_string()
}

/// Check whether `path` (a `~/projects/{project}` style canonical checkout) is
/// safe to use as a build workspace.
///
/// Requirements:
/// * directory exists and contains a `.git` repo
/// * working tree is clean (`git status --porcelain` is empty)
/// * `.git/index` has not been touched recently (no active operator session)
/// * no `.operator_session` marker file is present
/// * on `adele`, no `.adele_operator_session` marker is present
///
/// This is a filesystem-only, synchronous guard. Callers running on the target
/// computer pass the local path; never call this for a remote path and trust
/// the result.
pub fn is_canonical_workspace_usable(path: &str, computer_name: Option<&str>) -> bool {
    let expanded = expand_tilde(path);
    let p = Path::new(&expanded);

    if !p.is_dir() {
        return false;
    }
    if !p.join(".git").is_dir() {
        return false;
    }

    // Check filesystem state BEFORE running `git status`, because `git status`
    // refreshes `.git/index` and would make the mtime check falsely recent.

    // Recent `.git/index` mtime implies an active interactive session.
    let index = p.join(".git/index");
    if let Ok(meta) = index.metadata() {
        if let Ok(mtime) = meta.modified() {
            let elapsed = std::time::SystemTime::now()
                .duration_since(mtime)
                .unwrap_or_default();
            if elapsed.as_secs() < INTERACTIVE_SESSION_THRESHOLD_SECS {
                return false;
            }
        }
    }

    // Explicit operator-session markers.
    if p.join(".operator_session").exists() {
        return false;
    }
    if computer_name == Some("adele") && p.join(".adele_operator_session").exists() {
        return false;
    }

    // Tree must be clean.
    let clean = match Command::new("git")
        .args(["-C", &expanded, "status", "--porcelain"])
        .output()
    {
        Ok(out) => out.status.success() && out.stdout.is_empty(),
        Err(_) => return false,
    };
    if !clean {
        return false;
    }

    true
}

/// Ensure a canonical slot (slot 99, kind='canonical') exists for `computer_id`
/// pointing at `~/projects/{project_name}`. The row's `status` is set to `idle`
/// only if the local tree passes [`is_canonical_workspace_usable`]; otherwise
/// it is `disabled` so the scheduler never assigns work to a dirty/owned tree.
///
/// Returns `true` if the upsert resulted in an idle (usable) canonical slot.
/// This must run on the target computer because the guard inspects the local
/// filesystem.
pub async fn ensure_canonical_sub_agent_row(
    pool: &PgPool,
    computer_id: Uuid,
    project_name: &str,
    computer_name: Option<&str>,
) -> Result<bool, CoordError> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let workspace = format!("{home}/projects/{project_name}");
    let workspace2 = workspace.clone();
    let computer_name_owned = computer_name.map(|s| s.to_string());
    let usable = tokio::task::spawn_blocking(move || {
        is_canonical_workspace_usable(&workspace2, computer_name_owned.as_deref())
    })
    .await
    .map_err(|e| CoordError::Internal(format!("canonical guard panicked: {e}")))?;
    let status = if usable { "idle" } else { "disabled" };

    let result = sqlx::query(
        "INSERT INTO sub_agents (computer_id, slot, kind, status, workspace_dir) \
         VALUES ($1, $2, 'canonical', $3, $4) \
         ON CONFLICT (computer_id, slot) DO UPDATE SET \
             kind = EXCLUDED.kind, \
             status = EXCLUDED.status, \
             workspace_dir = EXCLUDED.workspace_dir",
    )
    .bind(computer_id)
    .bind(CANONICAL_SLOT)
    .bind(status)
    .bind(&workspace)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() >= 1 && usable)
}

/// Resolve the canonical project name: `FORGEFLEET_CANONICAL_PROJECT`, then
/// fall back to the Cargo package name of this build, then "forge-fleet".
pub fn canonical_project_name() -> String {
    std::env::var(CANONICAL_PROJECT_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_NAME").to_string())
}

/// Seed slot-0 for every computer in `computers`. Idempotent.
pub async fn seed_slot_zero_for_all(pool: &PgPool) -> Result<u32, CoordError> {
    let rows: Vec<(Uuid, Option<i32>)> = sqlx::query_as("SELECT id, cpu_cores FROM computers")
        .fetch_all(pool)
        .await?;
    let mut total = 0u32;
    for (computer_id, cpu_cores) in rows {
        // One slot by default; scale with cpu_cores/4 (capped at 4).
        let desired = cpu_cores
            .map(|c| ((c / 4).max(1) as u32).min(4))
            .unwrap_or(1);
        total += ensure_sub_agent_rows(pool, computer_id, desired).await?;
    }
    Ok(total)
}

/// Find or create a transient "dispatch" work_item for ad-hoc prompts.
/// Returns the work_item id. Creates a sentinel project
/// `"ff-agent-dispatch"` on first use.
///
/// Created terminal (`status='done'`): this is a provenance *container* for a
/// run/output that has already completed, not pipeline work. It is NOT
/// lease-managed (only `kind='task'` items go through the scheduler), so leaving
/// it `in_progress` made the lease-less orphan reaper churn through it and
/// `ff pm doctor` flag it as orphaned — a false signal. The chat path separately
/// marks its container `done` afterward, so this is idempotent there.
pub async fn create_transient_work_item(
    pool: &PgPool,
    prompt: &str,
    created_by: &str,
) -> Result<Uuid, CoordError> {
    // Ensure sentinel project exists.
    sqlx::query(
        "INSERT INTO projects (id, display_name, default_branch, status) \
         VALUES ('ff-agent-dispatch', 'Agent Dispatch', 'main', 'active') \
         ON CONFLICT (id) DO NOTHING",
    )
    .execute(pool)
    .await?;

    let title = truncate(prompt, 120);
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO work_items (project_id, kind, title, description, status, priority, created_by) \
         VALUES ('ff-agent-dispatch', 'dispatch', $1, $2, 'done', 'normal', $3) \
         RETURNING id",
    )
    .bind(&title)
    .bind(prompt)
    .bind(created_by)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Record a commit-back-able `work_output` for an agent run that edited files
/// (GAP-D0). Creates a transient work_item, then inserts a `work_output`
/// carrying `agent_session_id` + `modified_files` so
/// `ff agent commit-back <session>` can find and lift the changes — the
/// producer half the V40 provenance columns were added for but nothing wrote.
/// Returns the new work_output id.
pub async fn record_agent_run_output(
    pool: &PgPool,
    session_id: &str,
    prompt: &str,
    modified_files: &[String],
    node: &str,
    model_id: &str,
    working_dir: &str,
) -> Result<Uuid, CoordError> {
    let work_item_id = create_transient_work_item(pool, prompt, "ff run").await?;
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO work_outputs (\
            work_item_id, kind, title, produced_by_agent, produced_on_computer, \
            agent_session_id, modified_files, metadata\
         ) VALUES ($1, 'agent_run', $2, 'ff run', $3, $4, $5, $6) \
         RETURNING id",
    )
    .bind(work_item_id)
    .bind(truncate(prompt, 120))
    .bind(node)
    .bind(session_id)
    .bind(json!(modified_files))
    .bind(json!({
        "model_id": model_id,
        "modified_file_count": modified_files.len(),
        // GAP-D1: record WHERE the run edited files so `commit-back` lifts from
        // the actual workspace, not a hardcoded per-worker path.
        "working_dir": working_dir,
    }))
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Snapshot row returned by [`list_sub_agents`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct SubAgentListRow {
    pub id: Uuid,
    pub computer: String,
    pub slot: i32,
    pub kind: String,
    pub status: String,
    pub workspace_dir: String,
    pub current_work_item_id: Option<Uuid>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_heartbeat_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// List every sub_agent row joined to its computer name.
#[allow(clippy::type_complexity)]
pub async fn list_sub_agents(pool: &PgPool) -> Result<Vec<SubAgentListRow>, CoordError> {
    let rows: Vec<(
        Uuid,
        String,
        i32,
        String,
        String,
        String,
        Option<Uuid>,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
    )> = sqlx::query_as(
        "SELECT sa.id, c.name, sa.slot, sa.kind, sa.status, sa.workspace_dir, \
                sa.current_work_item_id, sa.started_at, sa.last_heartbeat_at \
         FROM sub_agents sa \
         JOIN computers c ON c.id = sa.computer_id \
         ORDER BY c.name ASC, sa.slot ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                computer,
                slot,
                kind,
                status,
                workspace_dir,
                work_item,
                started_at,
                heartbeat,
            )| {
                SubAgentListRow {
                    id,
                    computer,
                    slot,
                    kind,
                    status,
                    workspace_dir,
                    current_work_item_id: work_item,
                    started_at,
                    last_heartbeat_at: heartbeat,
                }
            },
        )
        .collect())
}

/// How often [`spawn_stale_slot_reaper`] scans `sub_agents` for busy slots
/// with stale heartbeats.
const REAPER_SCAN_INTERVAL_SECS: u64 = 60;

/// Heartbeat age past which a `'busy'` slot is declared stale. There is no
/// periodic mid-task heartbeat on `sub_agents` (`last_heartbeat_at` is only
/// written on claim/release), so this must exceed the longest legitimate task
/// (cold builds run ~45 min) — a shorter ceiling would reset a live slot
/// mid-run and let the scheduler oversubscribe it (bug class #589).
const STALE_HEARTBEAT_SECS: i64 = 3600;

/// One slot reset by [`reap_stale_busy_slots`].
#[derive(Debug, Clone)]
pub struct ReapedSlot {
    pub sub_agent_id: Uuid,
    /// The work item the slot was tracking when it went stale, if any.
    pub work_item_id: Option<Uuid>,
    /// Whether that work item was re-queued (`status = 'ready'`) for another
    /// dispatch. `false` when the item had already reached a terminal status.
    pub requeued: bool,
}

/// Reset every `'busy'` sub_agent whose `last_heartbeat_at` is older than
/// `stale_after_secs` back to `'idle'`, clear its `current_work_item_id`, and
/// re-queue the orphaned work item (`status = 'ready'`, assignment cleared) so
/// the scheduler can dispatch it to another slot. All in ONE atomic statement,
/// so a crash between "free slot" and "re-queue item" cannot strand the item.
///
/// Slots holding an ACTIVE `work_item_leases` row are exempt: the lease
/// lifecycle owns those (lease takeover reclaims them when the lease dies),
/// and resetting them here would desync `busy` from the lease table (#1083).
/// Work items already in a terminal status are not re-queued.
pub async fn reap_stale_busy_slots(
    pool: &PgPool,
    stale_after_secs: i64,
) -> Result<Vec<ReapedSlot>, CoordError> {
    let rows: Vec<(Uuid, Option<Uuid>, bool)> = sqlx::query_as(
        "WITH stale AS ( \
             SELECT id, current_work_item_id \
               FROM sub_agents \
              WHERE status = 'busy' \
                AND (last_heartbeat_at IS NULL \
                     OR last_heartbeat_at < NOW() - make_interval(secs => $1)) \
                AND NOT EXISTS ( \
                     SELECT 1 FROM work_item_leases l \
                      WHERE l.sub_agent_id = sub_agents.id \
                        AND l.released_at IS NULL) \
                FOR UPDATE SKIP LOCKED \
         ), reaped AS ( \
             UPDATE sub_agents s \
                SET status = 'idle', \
                    current_work_item_id = NULL, \
                    started_at = NULL, \
                    last_heartbeat_at = NOW() \
               FROM stale \
              WHERE s.id = stale.id \
              RETURNING s.id AS sub_agent_id, stale.current_work_item_id AS work_item_id \
         ), requeued AS ( \
             UPDATE work_items w \
                SET status = 'ready', \
                    assigned_to = NULL, \
                    assigned_computer = NULL \
               FROM reaped r \
              WHERE w.id = r.work_item_id \
                AND w.status NOT IN ('done', 'failed', 'cancelled') \
              RETURNING w.id \
         ) \
         SELECT r.sub_agent_id, r.work_item_id, \
                EXISTS (SELECT 1 FROM requeued q WHERE q.id = r.work_item_id) AS requeued \
           FROM reaped r",
    )
    .bind(stale_after_secs as f64)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(sub_agent_id, work_item_id, requeued)| ReapedSlot {
            sub_agent_id,
            work_item_id,
            requeued,
        })
        .collect())
}

/// Age past which a `'disabled'` slot row counts as a deploy leftover
/// (2026-07-23: 34 stale disabled rows, rihanna alone 6). Age is anchored to
/// the row's last activity (`last_heartbeat_at`, then `started_at`) and falls
/// back to `created_at` (V251) when the row was never claimed — deploys upsert
/// disabled rows with NULL activity timestamps, so without the creation
/// fallback a just-written row would be indistinguishable from a months-old
/// leftover and could be deleted the moment it appeared.
const DISABLED_LEFTOVER_AGE_SECS: i64 = 24 * 3600;

/// How long a computer must have been continuously online before its old
/// `'disabled'` rows are deleted. A freshly-rebooted node gets an hour to
/// bring its slots up before the registry concludes the rows are leftovers.
const DISABLED_LEFTOVER_COMPUTER_ONLINE_SECS: i64 = 3600;

/// One `sub_agents` row joined with the lease + computer state the three
/// slot-registry correction predicates need. Fetched by
/// [`reconcile_slot_registry`]; also constructible as a plain fixture so the
/// predicates are unit-testable without a DB.
#[derive(Debug, Clone)]
struct SlotSnapshot {
    sub_agent_id: Uuid,
    computer: String,
    slot: i32,
    status: String,
    current_work_item_id: Option<Uuid>,
    last_heartbeat_at: Option<chrono::DateTime<Utc>>,
    started_at: Option<chrono::DateTime<Utc>>,
    /// Row creation time (NOT NULL since V251) — the age anchor of last resort
    /// for rows whose activity timestamps are still NULL. Used only by
    /// correction (c).
    created_at: chrono::DateTime<Utc>,
    /// `work_item_id` of an active (`released_at IS NULL`) lease claiming THIS
    /// slot, if any.
    active_lease_work_item: Option<Uuid>,
    /// Whether an active lease exists on `current_work_item_id` — any holder,
    /// not just this slot (a re-dispatched item keeps its old slot busy).
    current_item_has_active_lease: bool,
    /// `computers.status = 'online'`.
    computer_online: bool,
    /// When `computers.status` last changed (online-since, when online).
    computer_status_changed_at: Option<chrono::DateTime<Utc>>,
}

/// Correction (a): a `'busy'` slot whose current work item has NO active lease
/// is drift — the lease was released/expired without the slot being freed —
/// and is reset EVERY pass, no grace window (a stale `busy` count corrupts
/// fair-share math and dashboards immediately; 2026-07-23 saw 7 busy vs 11
/// active leases). Exempt only: slots an active lease still claims directly
/// (the lease lifecycle owns those, #1083, matching [`reap_stale_busy_slots`]).
///
/// Snapshot-side candidate filter ONLY — [`reconcile_slot_registry`] re-asserts
/// this same predicate inside the corrective write, under a row lock, before
/// touching anything.
fn busy_slot_should_reset(row: &SlotSnapshot) -> bool {
    row.status == "busy"
        && !row.current_item_has_active_lease
        && row.active_lease_work_item.is_none()
}

/// Correction (b): an active lease claims this slot but the row says `'idle'`
/// — the claim-side write was lost. The row must say `'busy'` or the scheduler
/// dispatches a second item into a workspace a lease already owns.
///
/// Snapshot-side candidate filter ONLY — the corrective write locks the live
/// lease row and re-reads it before flipping the slot.
fn idle_slot_should_mark_busy(row: &SlotSnapshot) -> bool {
    row.status == "idle" && row.active_lease_work_item.is_some()
}

/// Correction (c): a `'disabled'` row past [`DISABLED_LEFTOVER_AGE_SECS`]
/// (anchored to last activity, falling back to `created_at` — see the
/// constant) on a computer that has been online for
/// [`DISABLED_LEFTOVER_COMPUTER_ONLINE_SECS`] is a deploy leftover — the node
/// had ample time to re-enable its real slots. Never deletes a row an active
/// lease still references, and never a row younger than the age floor no
/// matter which timestamp proves its youth.
///
/// Snapshot-side candidate filter ONLY — the delete transaction locks the row
/// and re-asserts this whole predicate in a fresh snapshot first.
fn disabled_slot_is_deploy_leftover(row: &SlotSnapshot, now: chrono::DateTime<Utc>) -> bool {
    if row.status != "disabled" || row.active_lease_work_item.is_some() {
        return false;
    }
    let anchor = row
        .last_heartbeat_at
        .or(row.started_at)
        .unwrap_or(row.created_at);
    let old_enough = (now - anchor).num_seconds() >= DISABLED_LEFTOVER_AGE_SECS;
    let online_long_enough = row.computer_online
        && match row.computer_status_changed_at {
            Some(t) => (now - t).num_seconds() >= DISABLED_LEFTOVER_COMPUTER_ONLINE_SECS,
            None => false,
        };
    old_enough && online_long_enough
}

/// Counts from one [`reconcile_slot_registry`] pass.
#[derive(Debug, Clone, Default)]
pub struct SlotRegistrySummary {
    /// (a) busy rows reset to idle (no active lease on their work item).
    pub reset_to_idle: usize,
    /// (b) idle rows flipped to busy (an active lease claims them).
    pub marked_busy: usize,
    /// (c) stale disabled rows deleted (deploy leftovers).
    pub deleted_disabled: usize,
}

impl SlotRegistrySummary {
    /// Total corrections applied this pass.
    pub fn corrections(&self) -> usize {
        self.reset_to_idle + self.marked_busy + self.deleted_disabled
    }

    /// One-line drift summary for tracing + Telegram.
    pub fn drift_line(&self) -> String {
        format!(
            "slot-registry drift corrected: {} busy→idle (no active lease), \
             {} idle→busy (active lease), {} stale disabled row(s) deleted",
            self.reset_to_idle, self.marked_busy, self.deleted_disabled
        )
    }
}

#[allow(clippy::type_complexity)]
async fn fetch_slot_snapshots(pool: &PgPool) -> Result<Vec<SlotSnapshot>, CoordError> {
    let rows: Vec<(
        Uuid,
        String,
        i32,
        String,
        Option<Uuid>,
        Option<chrono::DateTime<Utc>>,
        Option<chrono::DateTime<Utc>>,
        chrono::DateTime<Utc>,
        Option<Uuid>,
        bool,
        bool,
        Option<chrono::DateTime<Utc>>,
    )> = sqlx::query_as(
        "SELECT sa.id, c.name, sa.slot, sa.status, sa.current_work_item_id, \
                sa.last_heartbeat_at, sa.started_at, sa.created_at, \
                (SELECT l.work_item_id FROM work_item_leases l \
                  WHERE l.sub_agent_id = sa.id AND l.released_at IS NULL \
                  ORDER BY l.created_at DESC LIMIT 1), \
                EXISTS (SELECT 1 FROM work_item_leases li \
                  WHERE li.work_item_id = sa.current_work_item_id \
                    AND li.released_at IS NULL), \
                (c.status = 'online'), \
                c.status_changed_at \
         FROM sub_agents sa \
         JOIN computers c ON c.id = sa.computer_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                sub_agent_id,
                computer,
                slot,
                status,
                current_work_item_id,
                last_heartbeat_at,
                started_at,
                created_at,
                active_lease_work_item,
                current_item_has_active_lease,
                computer_online,
                computer_status_changed_at,
            )| SlotSnapshot {
                sub_agent_id,
                computer,
                slot,
                status,
                current_work_item_id,
                last_heartbeat_at,
                started_at,
                created_at,
                active_lease_work_item,
                current_item_has_active_lease,
                computer_online,
                computer_status_changed_at,
            },
        )
        .collect())
}

/// Slot-registry reconciler (leader-only): make `sub_agents` mirror the live
/// lease table + node slot config EVERY pass. Applies the three corrections
/// ((a) busy-without-lease → idle, (b) idle-under-lease → busy, (c) stale
/// disabled deploy leftovers → deleted). Without this, fair-share math and
/// dashboards read wrong numbers (2026-07-23: 7 busy vs 11 active leases; 34
/// stale disabled rows).
///
/// Concurrency contract: the snapshot predicates only NOMINATE candidates
/// (which keeps them pure and fixture-testable) — they never justify a write
/// on their own, because `status` alone cannot distinguish "same busy slot"
/// from "re-leased since the snapshot". Each corrective write runs in its own
/// short transaction that (1) locks the governing row (`FOR UPDATE SKIP
/// LOCKED`, yielding to any in-flight dispatcher/releaser) and (2) re-asserts
/// the FULL predicate — lease existence, work-item identity — against a fresh
/// statement snapshot while the lock is held. Locking the `sub_agents` row also
/// conflicts with the FK `KEY SHARE` a new `work_item_leases` INSERT must take
/// on it, so a lease cannot attach to a slot between the re-check and the
/// write. If the world moved, the re-check misses, zero rows are touched, and a
/// still-drifted slot is retried next tick — the reconciler can only ever
/// converge state, never clobber a live claim or resurrect a released one.
pub async fn reconcile_slot_registry(pool: &PgPool) -> Result<SlotRegistrySummary, CoordError> {
    let now = Utc::now();
    let mut summary = SlotRegistrySummary::default();

    for row in &fetch_slot_snapshots(pool).await? {
        if busy_slot_should_reset(row) {
            let mut tx = pool.begin().await?;
            let locked = sqlx::query(
                "SELECT 1 FROM sub_agents WHERE id = $1 AND status = 'busy' \
                 FOR UPDATE SKIP LOCKED",
            )
            .bind(row.sub_agent_id)
            .fetch_optional(&mut *tx)
            .await?;
            if locked.is_none() {
                // Gone, no longer busy, or an in-flight dispatcher holds the
                // row — skip; next tick re-evaluates from a fresh snapshot.
                continue;
            }
            // Row is locked: no new lease can FK-attach to it, and this UPDATE
            // re-reads lease/work-item state in a fresh snapshot. A slot
            // re-dispatched since the snapshot (new lease, or a different
            // current_work_item_id) fails the re-check → 0 rows, no clobber.
            let affected = sqlx::query(
                "UPDATE sub_agents \
                 SET status = 'idle', current_work_item_id = NULL, started_at = NULL, \
                     last_heartbeat_at = NOW() \
                 WHERE id = $1 AND status = 'busy' \
                   AND current_work_item_id IS NOT DISTINCT FROM $2 \
                   AND NOT EXISTS (SELECT 1 FROM work_item_leases l \
                                    WHERE l.sub_agent_id = $1 AND l.released_at IS NULL) \
                   AND ($2::uuid IS NULL OR NOT EXISTS ( \
                            SELECT 1 FROM work_item_leases li \
                             WHERE li.work_item_id = $2 AND li.released_at IS NULL))",
            )
            .bind(row.sub_agent_id)
            .bind(row.current_work_item_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            tx.commit().await?;
            if affected == 1 {
                // Counted only — the caller emits the single drift line; the
                // task contract forbids per-row log spam.
                summary.reset_to_idle += 1;
            }
        } else if idle_slot_should_mark_busy(row) {
            // Lock the ACTIVE lease row itself before flipping the slot: a
            // lease released since the snapshot (or one a releaser holds
            // mid-release) yields no row here, so a dead lease can never leave
            // the slot falsely busy. While the lock is held the releaser's own
            // `released_at` write blocks, so it lands after ours and settles
            // the slot back to idle — the lease lifecycle always has the last
            // word. The work item comes from the locked lease, not the
            // snapshot.
            let mut tx = pool.begin().await?;
            let lease: Option<(Uuid,)> = sqlx::query_as(
                "SELECT work_item_id FROM work_item_leases \
                 WHERE sub_agent_id = $1 AND released_at IS NULL \
                 ORDER BY created_at DESC LIMIT 1 \
                 FOR UPDATE SKIP LOCKED",
            )
            .bind(row.sub_agent_id)
            .fetch_optional(&mut *tx)
            .await?;
            let Some((work_item_id,)) = lease else {
                continue;
            };
            let affected = sqlx::query(
                "UPDATE sub_agents \
                 SET status = 'busy', current_work_item_id = $2, \
                     started_at = COALESCE(started_at, NOW()), last_heartbeat_at = NOW() \
                 WHERE id = $1 AND status = 'idle'",
            )
            .bind(row.sub_agent_id)
            .bind(work_item_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            tx.commit().await?;
            if affected == 1 {
                summary.marked_busy += 1;
            }
        } else if disabled_slot_is_deploy_leftover(row, now) {
            // Lock the row and re-assert the FULL leftover predicate in a fresh
            // snapshot before touching anything — the snapshot only nominated
            // the candidate. Once locked, no new lease can FK-attach to the row
            // (KEY SHARE conflicts with our FOR UPDATE), so "no active lease"
            // verified here holds through the delete. work_item_leases /
            // work_item_worktrees FK-reference sub_agents with no ON DELETE
            // action, so detach the historical references before the delete or
            // it bounces. Active leases are never detached — the re-check skips
            // rows they reference. Per-row transaction so one bad row can't
            // wedge the whole pass.
            let delete = async {
                let mut tx = pool.begin().await?;
                let locked = sqlx::query(
                    "SELECT 1 FROM sub_agents sa \
                     JOIN computers c ON c.id = sa.computer_id \
                     WHERE sa.id = $1 AND sa.status = 'disabled' \
                       AND NOT EXISTS (SELECT 1 FROM work_item_leases l \
                                        WHERE l.sub_agent_id = sa.id \
                                          AND l.released_at IS NULL) \
                       AND COALESCE(sa.last_heartbeat_at, sa.started_at, sa.created_at) \
                           <= NOW() - make_interval(secs => $2) \
                       AND c.status = 'online' \
                       AND c.status_changed_at <= NOW() - make_interval(secs => $3) \
                     FOR UPDATE OF sa SKIP LOCKED",
                )
                .bind(row.sub_agent_id)
                .bind(DISABLED_LEFTOVER_AGE_SECS as f64)
                .bind(DISABLED_LEFTOVER_COMPUTER_ONLINE_SECS as f64)
                .fetch_optional(&mut *tx)
                .await?;
                if locked.is_none() {
                    return Ok(0);
                }
                sqlx::query(
                    "UPDATE work_item_leases SET sub_agent_id = NULL \
                     WHERE sub_agent_id = $1 AND released_at IS NOT NULL",
                )
                .bind(row.sub_agent_id)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "UPDATE work_item_worktrees SET sub_agent_id = NULL WHERE sub_agent_id = $1",
                )
                .bind(row.sub_agent_id)
                .execute(&mut *tx)
                .await?;
                let affected =
                    sqlx::query("DELETE FROM sub_agents WHERE id = $1 AND status = 'disabled'")
                        .bind(row.sub_agent_id)
                        .execute(&mut *tx)
                        .await?
                        .rows_affected();
                tx.commit().await?;
                Ok::<u64, sqlx::Error>(affected)
            };
            match delete.await {
                Ok(1) => {
                    summary.deleted_disabled += 1;
                }
                Ok(_) => {} // locked re-check missed: state moved since the snapshot
                Err(e) => {
                    // Failure diagnostics, not drift logging — the drift
                    // summary itself stays one line in the caller.
                    tracing::warn!(
                        computer = %row.computer,
                        slot = row.slot,
                        error = %e,
                        "slot registry: failed to delete stale disabled row"
                    );
                }
            }
        }
    }

    Ok(summary)
}

/// Spawn the background stale-slot reaper: every [`REAPER_SCAN_INTERVAL_SECS`]
/// seconds (leader-gated, so exactly one node sweeps the fleet-wide table) it
/// runs [`reap_stale_busy_slots`] with the [`STALE_HEARTBEAT_SECS`] ceiling,
/// then [`reconcile_slot_registry`] to keep `sub_agents` mirroring the lease
/// table every pass. `forgefleetd` starts this at boot alongside the other
/// subsystem ticks.
pub fn spawn_stale_slot_reaper(
    pg: PgPool,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(REAPER_SCAN_INTERVAL_SECS));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !crate::leader_cache::is_current_leader() {
                        continue;
                    }
                    match reap_stale_busy_slots(&pg, STALE_HEARTBEAT_SECS).await {
                        Ok(reaped) => {
                            for r in &reaped {
                                tracing::info!(
                                    sub_agent = %r.sub_agent_id,
                                    work_item = ?r.work_item_id,
                                    requeued = r.requeued,
                                    "stale slot reaper: reset busy slot with stale heartbeat"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "stale slot reaper tick failed");
                        }
                    }
                    match reconcile_slot_registry(&pg).await {
                        // One-line drift summary only when something was
                        // corrected — a clean pass every 60s must stay silent.
                        Ok(summary) if summary.corrections() > 0 => {
                            let line = summary.drift_line();
                            tracing::info!(
                                reset_to_idle = summary.reset_to_idle,
                                marked_busy = summary.marked_busy,
                                deleted_disabled = summary.deleted_disabled,
                                "{line}"
                            );
                            if let Err(e) = crate::telegram::send_telegram_from_secrets(
                                &pg,
                                "ForgeFleet slot registry",
                                &line,
                            )
                            .await
                            {
                                tracing::warn!(error = %e, "slot registry: telegram drift notice failed");
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(error = %e, "slot registry reconcile tick failed");
                        }
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        tracing::info!("stale slot reaper loop stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::CoordError;

    #[test]
    fn transient_errors_are_retryable() {
        // LLM-call failures: a re-dispatch to another capable host may succeed.
        assert!(CoordError::NoLlmServer("priya".into()).is_transient());
        assert!(CoordError::EmptyResponse.is_transient());
    }

    #[test]
    fn structural_errors_are_not_transient() {
        // Slot/validation/internal errors: retrying the same way won't help.
        assert!(!CoordError::NoSlot(None).is_transient());
        assert!(!CoordError::NoSlot(Some("contended".into())).is_transient());
        assert!(!CoordError::UnknownComputer("nope".into()).is_transient());
        assert!(!CoordError::Pulse("redis down".into()).is_transient());
        assert!(!CoordError::Internal("bug".into()).is_transient());
    }

    #[test]
    fn stale_heartbeat_ceiling_clears_longest_legitimate_task() {
        // `sub_agents` has no mid-task heartbeat, so the staleness ceiling
        // must exceed the ~45-min cold-build worst case or the reaper would
        // reset a LIVE slot mid-run and oversubscribe it (bug class #589).
        assert!(super::STALE_HEARTBEAT_SECS > 45 * 60);
        // And the scan cadence is the 60s the daemon wires in.
        assert_eq!(super::REAPER_SCAN_INTERVAL_SECS, 60);
    }

    mod slot_registry {
        use chrono::{DateTime, Duration, Utc};
        use uuid::Uuid;

        use super::super::{
            DISABLED_LEFTOVER_AGE_SECS, DISABLED_LEFTOVER_COMPUTER_ONLINE_SECS, SlotSnapshot,
            busy_slot_should_reset, disabled_slot_is_deploy_leftover, idle_slot_should_mark_busy,
        };

        fn ago(now: DateTime<Utc>, secs: i64) -> Option<DateTime<Utc>> {
            Some(now - Duration::seconds(secs))
        }

        /// Fixture: a slot on an online-for-2h computer, no lease anywhere,
        /// activity timestamps unset, row created 30 days ago. Tests override
        /// the fields under scrutiny.
        fn snap(now: DateTime<Utc>, status: &str) -> SlotSnapshot {
            SlotSnapshot {
                sub_agent_id: Uuid::new_v4(),
                computer: "rihanna".to_string(),
                slot: 0,
                status: status.to_string(),
                current_work_item_id: None,
                last_heartbeat_at: None,
                started_at: None,
                created_at: now - Duration::days(30),
                active_lease_work_item: None,
                current_item_has_active_lease: false,
                computer_online: true,
                computer_status_changed_at: ago(now, 2 * 3600),
            }
        }

        #[test]
        fn busy_without_active_lease_resets_every_pass() {
            let now = Utc::now();
            let mut row = snap(now, "busy");
            row.current_work_item_id = Some(Uuid::new_v4());
            // Busy, no active lease on the item, no lease on the slot → drift,
            // reset — and NO grace: even a slot claimed one second ago resets
            // (the reviewer-required every-pass correction; a stale busy count
            // corrupts fair-share math immediately).
            assert!(busy_slot_should_reset(&row));
            row.last_heartbeat_at = ago(now, 1);
            assert!(busy_slot_should_reset(&row));
            row.last_heartbeat_at = ago(now, DISABLED_LEFTOVER_AGE_SECS);
            assert!(busy_slot_should_reset(&row));
            // current_work_item_id NULL (busy with nothing to show) is still
            // drift — there is trivially no active lease on it.
            row.current_work_item_id = None;
            assert!(busy_slot_should_reset(&row));
        }

        #[test]
        fn busy_with_active_lease_is_never_reset() {
            let now = Utc::now();
            let item = Uuid::new_v4();
            let mut row = snap(now, "busy");
            row.current_work_item_id = Some(item);
            // An active lease on the current work item → in sync, keep busy.
            row.current_item_has_active_lease = true;
            assert!(!busy_slot_should_reset(&row));
            // A lease claiming the slot directly (even for another item) is
            // owned by the lease lifecycle (#1083) — never reset here.
            row.current_item_has_active_lease = false;
            row.active_lease_work_item = Some(Uuid::new_v4());
            assert!(!busy_slot_should_reset(&row));
            // Only 'busy' rows are candidates.
            row.active_lease_work_item = None;
            row.status = "idle".to_string();
            assert!(!busy_slot_should_reset(&row));
        }

        #[test]
        fn idle_under_active_lease_marks_busy() {
            let now = Utc::now();
            let mut row = snap(now, "idle");
            // Idle with no lease → in sync, leave alone.
            assert!(!idle_slot_should_mark_busy(&row));
            // An active lease claims the slot but the row says idle → flip to
            // busy or the scheduler double-books the workspace.
            row.active_lease_work_item = Some(Uuid::new_v4());
            assert!(idle_slot_should_mark_busy(&row));
            // Non-idle rows are not this correction's business.
            row.status = "busy".to_string();
            assert!(!idle_slot_should_mark_busy(&row));
            row.status = "disabled".to_string();
            assert!(!idle_slot_should_mark_busy(&row));
        }

        #[test]
        fn disabled_leftover_needs_age_and_online_computer() {
            let now = Utc::now();
            // Deploy leftover shape: disabled, NULL activity timestamps (never
            // active since it was written), row itself old, computer online
            // past the settle window.
            let row = snap(now, "disabled");
            assert!(disabled_slot_is_deploy_leftover(&row, now));
            // A JUST-CREATED disabled row with NULL activity timestamps is NOT
            // a leftover — created_at proves its youth. (Without the created_at
            // fallback this exact shape was wrongly deletable.)
            let mut newborn = snap(now, "disabled");
            newborn.created_at = now - Duration::seconds(60);
            assert!(!disabled_slot_is_deploy_leftover(&newborn, now));
            // A recorded heartbeat older than 24h is equally a leftover.
            let mut old = snap(now, "disabled");
            old.last_heartbeat_at = ago(now, DISABLED_LEFTOVER_AGE_SECS + 1);
            assert!(disabled_slot_is_deploy_leftover(&old, now));
            // Recent activity (< 24h) → could be a deliberate fresh disable;
            // keep, even though the row itself is old.
            let mut fresh = snap(now, "disabled");
            fresh.last_heartbeat_at = ago(now, 3600);
            assert!(!disabled_slot_is_deploy_leftover(&fresh, now));
            // Computer offline → the node never got a chance to re-enable.
            let mut offline = snap(now, "disabled");
            offline.computer_online = false;
            assert!(!disabled_slot_is_deploy_leftover(&offline, now));
            // Online but only briefly (or unknown since-when) → wait.
            let mut recent = snap(now, "disabled");
            recent.computer_status_changed_at =
                ago(now, DISABLED_LEFTOVER_COMPUTER_ONLINE_SECS - 60);
            assert!(!disabled_slot_is_deploy_leftover(&recent, now));
            recent.computer_status_changed_at = None;
            assert!(!disabled_slot_is_deploy_leftover(&recent, now));
            // An active lease referencing the row blocks deletion outright.
            let mut leased = snap(now, "disabled");
            leased.active_lease_work_item = Some(Uuid::new_v4());
            assert!(!disabled_slot_is_deploy_leftover(&leased, now));
            // Only 'disabled' rows are candidates.
            let idle = snap(now, "idle");
            assert!(!disabled_slot_is_deploy_leftover(&idle, now));
        }
    }

    #[test]
    fn canonical_project_name_defaults_to_cargo_pkg() {
        // CARGO_PKG_NAME in this crate is "ff-agent", but the env override wins.
        assert_eq!(super::canonical_project_name(), env!("CARGO_PKG_NAME"));
    }

    #[cfg(unix)]
    mod canonical_workspace_guard {
        use std::fs;
        use std::process::Command;

        use super::super::is_canonical_workspace_usable;

        fn tmp_dir() -> std::path::PathBuf {
            let base = std::env::temp_dir()
                .join(format!("ff-agent-canonical-guard-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&base).unwrap();
            base
        }

        // GNU `touch -d "1 hour ago"` isn't portable to BSD/macOS `touch`, so
        // backdate the mtime directly via `filetime` instead of shelling out.
        fn backdate_by_one_hour(path: &std::path::Path) {
            let one_hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
            let ft = filetime::FileTime::from_system_time(one_hour_ago);
            filetime::set_file_mtime(path, ft).expect("backdate mtime");
        }

        fn init_git(path: &std::path::Path) {
            let p = path.to_string_lossy();
            let out = Command::new("git")
                .args(["init", p.as_ref()])
                .output()
                .expect("git init");
            assert!(out.status.success(), "git init failed: {:?}", out.stderr);
            let out = Command::new("git")
                .args(["-C", p.as_ref(), "config", "user.email", "test@example.com"])
                .output()
                .expect("git config email");
            assert!(out.status.success());
            let out = Command::new("git")
                .args(["-C", p.as_ref(), "config", "user.name", "Test"])
                .output()
                .expect("git config name");
            assert!(out.status.success());
            // Create an initial commit so `.git/index` exists and `status` is clean.
            let readme = path.join("README");
            std::fs::write(&readme, "init\n").unwrap();
            let out = Command::new("git")
                .args(["-C", p.as_ref(), "add", "README"])
                .output()
                .expect("git add");
            assert!(out.status.success());
            let out = Command::new("git")
                .args(["-C", p.as_ref(), "commit", "-m", "init"])
                .output()
                .expect("git commit");
            assert!(out.status.success(), "git commit failed: {:?}", out.stderr);
            // Ignore operator-session markers so the dirty-tree check doesn't
            // trip on the marker files themselves.
            std::fs::write(
                path.join(".gitignore"),
                ".operator_session\n.adele_operator_session\n",
            )
            .unwrap();
            let out = Command::new("git")
                .args(["-C", p.as_ref(), "add", ".gitignore"])
                .output()
                .expect("git add gitignore");
            assert!(out.status.success());
            let out = Command::new("git")
                .args(["-C", p.as_ref(), "commit", "-m", "ignore markers"])
                .output()
                .expect("git commit gitignore");
            assert!(out.status.success());
        }

        #[test]
        fn missing_directory_is_unusable() {
            let path = tmp_dir().join("does-not-exist");
            assert!(!is_canonical_workspace_usable(
                path.to_string_lossy().as_ref(),
                None
            ));
        }

        #[test]
        fn non_git_directory_is_unusable() {
            let path = tmp_dir();
            assert!(!is_canonical_workspace_usable(
                path.to_string_lossy().as_ref(),
                None
            ));
        }

        #[test]
        fn dirty_tree_is_unusable() {
            let path = tmp_dir();
            init_git(&path);
            fs::write(path.join("untracked.txt"), "hello").unwrap();
            assert!(!is_canonical_workspace_usable(
                path.to_string_lossy().as_ref(),
                None
            ));
        }

        #[test]
        fn clean_tree_with_old_index_is_usable() {
            let path = tmp_dir();
            init_git(&path);
            // Make the index old enough to pass the interactive-session threshold.
            backdate_by_one_hour(&path.join(".git/index"));
            assert!(is_canonical_workspace_usable(
                path.to_string_lossy().as_ref(),
                None
            ));
        }

        #[test]
        fn clean_tree_with_recent_index_is_unusable() {
            let path = tmp_dir();
            init_git(&path);
            // A freshly-initialized repo has a recent index mtime.
            assert!(!is_canonical_workspace_usable(
                path.to_string_lossy().as_ref(),
                None
            ));
        }

        #[test]
        fn operator_session_marker_is_unusable() {
            let path = tmp_dir();
            init_git(&path);
            backdate_by_one_hour(&path.join(".git/index"));
            fs::write(path.join(".operator_session"), "").unwrap();
            assert!(!is_canonical_workspace_usable(
                path.to_string_lossy().as_ref(),
                None
            ));
        }

        #[test]
        fn adele_operator_session_marker_is_unusable_only_on_adele() {
            let path = tmp_dir();
            init_git(&path);
            backdate_by_one_hour(&path.join(".git/index"));
            fs::write(path.join(".adele_operator_session"), "").unwrap();
            // Non-adele computer ignores the adele-specific marker.
            assert!(is_canonical_workspace_usable(
                path.to_string_lossy().as_ref(),
                Some("priya")
            ));
            // Adele respects it.
            assert!(!is_canonical_workspace_usable(
                path.to_string_lossy().as_ref(),
                Some("adele")
            ));
        }
    }
}
