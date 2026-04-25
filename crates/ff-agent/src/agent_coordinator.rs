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

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;
use thiserror::Error;
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
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
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
    ) -> Result<Option<WorkerSlot>, CoordError> {
        if let Some(name) = target {
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
            // Any idle slot, preferring online computers.
            let row: Option<(Uuid, Uuid, String, i32)> = sqlx::query_as(
                "SELECT sa.id, sa.computer_id, c.name, sa.slot \
                 FROM sub_agents sa \
                 JOIN computers c ON c.id = sa.computer_id \
                 WHERE sa.status = 'idle' \
                 ORDER BY (c.status = 'online') DESC, sa.slot ASC \
                 LIMIT 1",
            )
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
        // 1. Pick a worker slot.
        let slot = self
            .pick_worker(target_computer.clone())
            .await?
            .ok_or_else(|| CoordError::NoSlot(target_computer.clone()))?;

        // 2. Claim it transactionally.
        let claimed = self.claim_slot(slot.sub_agent_id, work_item_id).await?;
        if !claimed {
            return Err(CoordError::NoSlot(Some(format!(
                "slot {} on {} lost to another dispatcher",
                slot.slot, slot.computer_name
            ))));
        }

        // 3. Run the LLM call, persist output, release slot. Catch any
        //    error below so the slot always gets released.
        let started = std::time::Instant::now();
        let result = self.run_and_persist(&slot, work_item_id, &prompt).await;

        let (outcome_tag, receipt_or_err) = match result {
            Ok(r) => ("ok", Ok(r)),
            Err(e) => ("error", Err(e)),
        };

        if let Err(rel_err) = self.release_slot(slot.sub_agent_id, outcome_tag).await {
            tracing::warn!(sub_agent = %slot.sub_agent_id, error = %rel_err, "release_slot failed");
        }

        let mut receipt = receipt_or_err?;
        receipt.duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        Ok(receipt)
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
        let url = if endpoint.contains("/chat/completions") {
            endpoint.clone()
        } else {
            format!("{}/v1/chat/completions", endpoint.trim_end_matches('/'))
        };

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
/// of the form `~/.forgefleet/sub-agent-{slot}/`. Existing rows (by
/// computer_id, slot) are left untouched.
pub async fn ensure_sub_agent_rows(
    pool: &PgPool,
    computer_id: Uuid,
    desired_count: u32,
) -> Result<u32, CoordError> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let mut created = 0u32;
    for slot in 0..desired_count as i32 {
        let workspace = format!("{home}/.forgefleet/sub-agent-{slot}");
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
         VALUES ('ff-agent-dispatch', 'dispatch', $1, $2, 'in_progress', 'normal', $3) \
         RETURNING id",
    )
    .bind(&title)
    .bind(prompt)
    .bind(created_by)
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
    pub status: String,
    pub workspace_dir: String,
    pub current_work_item_id: Option<Uuid>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_heartbeat_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// List every sub_agent row joined to its computer name.
pub async fn list_sub_agents(pool: &PgPool) -> Result<Vec<SubAgentListRow>, CoordError> {
    let rows: Vec<(
        Uuid,
        String,
        i32,
        String,
        String,
        Option<Uuid>,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
    )> = sqlx::query_as(
        "SELECT sa.id, c.name, sa.slot, sa.status, sa.workspace_dir, \
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
            |(id, computer, slot, status, workspace_dir, work_item, started_at, heartbeat)| {
                SubAgentListRow {
                    id,
                    computer,
                    slot,
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
