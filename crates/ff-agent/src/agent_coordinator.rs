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

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
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
    let reaped: Vec<ReapedSlot> = rows
        .into_iter()
        .map(|(sub_agent_id, work_item_id, requeued)| ReapedSlot {
            sub_agent_id,
            work_item_id,
            requeued,
        })
        .collect();
    // The slot is free to dispatch again immediately (the SQL above already
    // committed that), but its on-disk clone/worktree from the crashed/hung
    // task is still sitting there — clean it up best-effort so it can't leak
    // across dispatches. A cleanup failure must never undo the slot reset.
    for slot in &reaped {
        cleanup_stale_slot_worktree(pool, slot.sub_agent_id).await;
    }
    Ok(reaped)
}

/// An on-disk worktree/clone row still associated with a slot that
/// [`reap_stale_busy_slots`] just reset, found via `sub_agent_id` rather than
/// `work_item_id` because the DB reset already cleared the slot's
/// `current_work_item_id`.
struct StaleSlotWorktree {
    id: Uuid,
    repo_path: String,
    worktree_path: String,
    task_branch: String,
}

async fn fetch_stale_slot_worktree(
    pool: &PgPool,
    sub_agent_id: Uuid,
) -> Result<Option<StaleSlotWorktree>, CoordError> {
    let row: Option<(Uuid, String, String, String)> = sqlx::query_as(
        "SELECT id, repo_path, worktree_path, task_branch \
           FROM work_item_worktrees \
          WHERE sub_agent_id = $1 AND status <> 'cleaned' \
          ORDER BY created_at DESC LIMIT 1",
    )
    .bind(sub_agent_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(
        |(id, repo_path, worktree_path, task_branch)| StaleSlotWorktree {
            id,
            repo_path,
            worktree_path,
            task_branch,
        },
    ))
}

/// Clean up the orphaned worktree left behind by a slot [`reap_stale_busy_slots`]
/// just reset to idle. Mirrors the terminal-state worktree reaper
/// (`work_item_dispatch::evaluate_worktree_reaper`): the clone-direct clone is
/// reset in place rather than deleted (it's the slot's persistent checkout,
/// reused by the next dispatch), while a legacy detached worktree dir is
/// removed outright and has its stray `target`/`node_modules`/`.venv` build
/// artifacts reclaimed. The abandoned task branch is dropped either way, and
/// the row is marked `cleaned` so this reaper doesn't retry it every tick.
/// Best-effort throughout: a failure here must not block the slot from being
/// dispatched to again — it only leaves bytes for the disk-pressure reaper.
async fn cleanup_stale_slot_worktree(pool: &PgPool, sub_agent_id: Uuid) {
    let worktree = match fetch_stale_slot_worktree(pool, sub_agent_id).await {
        Ok(Some(worktree)) => worktree,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(
                error = %e, %sub_agent_id,
                "stale slot reaper: failed to look up orphaned worktree"
            );
            return;
        }
    };

    let repo_path = PathBuf::from(&worktree.repo_path);
    let worktree_path = PathBuf::from(&worktree.worktree_path);
    if let Err(e) = crate::work_item_dispatch::remove_worktree(&repo_path, &worktree_path) {
        tracing::warn!(
            error = %e, %sub_agent_id, worktree = %worktree.worktree_path,
            "stale slot reaper: failed to reset orphaned worktree"
        );
    }
    if worktree_path != repo_path {
        let reclaimed = crate::work_item_dispatch::reclaim_build_artifacts(&worktree_path);
        if reclaimed > 0 {
            tracing::info!(
                %sub_agent_id, reclaimed_bytes = reclaimed,
                "stale slot reaper: reclaimed orphaned build artifacts"
            );
        }
    }
    let _ = crate::work_item_dispatch::run_git(
        &repo_path,
        [
            OsStr::new("branch"),
            OsStr::new("-D"),
            OsStr::new(&worktree.task_branch),
        ],
        Duration::from_secs(30),
    );
    if let Err(e) = sqlx::query(
        "UPDATE work_item_worktrees SET status = 'cleaned', cleaned_at = NOW() WHERE id = $1",
    )
    .bind(worktree.id)
    .execute(pool)
    .await
    {
        tracing::warn!(
            error = %e, %sub_agent_id,
            "stale slot reaper: failed to mark orphaned worktree cleaned"
        );
    }
}

/// Spawn the background stale-slot reaper: every [`REAPER_SCAN_INTERVAL_SECS`]
/// seconds (leader-gated, so exactly one node sweeps the fleet-wide table) it
/// runs [`reap_stale_busy_slots`] with the [`STALE_HEARTBEAT_SECS`] ceiling.
/// `forgefleetd` starts this at boot alongside the other subsystem ticks.
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
