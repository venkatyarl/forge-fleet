//! Research subsystem — parallel multi-agent research via fleet LLMs.
//!
//! Entry point: [`ResearchSession::run`]. Given a query, decomposes it into
//! sub-questions (planner turn), dispatches each to a different fleet LLM
//! in parallel ([`MultiAgentOrchestrator::run_parallel`]), then synthesizes
//! the outputs into a final markdown report with citations.
//!
//! All state is persisted to Postgres (Schema V42: `research_sessions`,
//! `research_subtasks`, `research_findings`). Re-running the same session
//! ID is idempotent — the session resumes from whatever status it reached.
//!
//! ## Why it exists
//!
//! Single-agent research is bounded by one LLM's depth in one turn. The
//! fleet has 4 DGX Sparks + 10 other computers running LLMs — we can
//! parallelize 5-10 sub-investigations concurrently and each sub-agent
//! can iterate deep on its own sub-question. The planner + synthesizer
//! are the two most quality-sensitive calls; they use the reserve
//! thinking model (Qwen3.5-35B-A3B). Sub-agents can use any available
//! LLM since their output is cross-verified at synthesis time.
//!
//! ## Persistence
//!
//! Every session creates one `research_sessions` row, N `research_subtasks`
//! rows (one per decomposed sub-question), and 0+ `research_findings`
//! rows per subtask (for citation tracking). Findings are extracted
//! heuristically from sub-agent outputs today; later we'll have the
//! sub-agent emit structured JSON.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use tokio::sync::mpsc;
use tracing::{info, warn};
use uuid::Uuid;

use crate::multi_agent::{AgentTaskResult, OrchestratorEvent, TaskStatus};

// ─── Public types ───────────────────────────────────────────────────────────

/// Configuration for one research run.
#[derive(Debug, Clone)]
pub struct ResearchConfig {
    pub query: String,
    /// How deep each sub-agent can iterate. Maps to agent max_turns.
    pub depth: u32,
    /// How many sub-questions to decompose the query into (= parallelism).
    pub parallel: u32,
    /// Optional path to write the final markdown report.
    pub output_path: Option<PathBuf>,
    /// Who initiated (logged to `research_sessions.initiated_by`).
    pub initiated_by: String,
    /// Model hint for the planner + synthesizer turns. Defaults to "thinking"
    /// which routes to whichever deployment is registered for that pool alias
    /// (usually Qwen3.5-35B-A3B on Taylor:55001 per fleet_task_coverage).
    pub planner_model: String,
    /// Model hint for the sub-agent turns. Defaults to "coder" (Qwen3.6-35B).
    pub subagent_model: String,
    /// Gateway base URL for all LLM calls. Defaults to http://192.168.5.100:51002.
    pub gateway_url: String,
    /// Ground each sub-agent with a live DuckDuckGo web search for its
    /// sub-question, injecting the results into its prompt. Sub-agents run as
    /// plain chat completions (no live tools — see Phase 3), so without this
    /// they can only answer from training data and the prompt's "Use WebSearch"
    /// instruction is a lie. Default ON; `--no-web` disables it. Degrades
    /// gracefully: a failed/empty search just falls back to ungrounded.
    pub web_grounding: bool,
    /// Detached mode. When true, [`ResearchSession::new`] inserts the row with
    /// status `queued` and returns WITHOUT running it — the leader's
    /// `forgefleetd` [`ResearchRunnerTick`] claims it and drives the run inside
    /// the daemon, so it survives the originating CLI being killed. The report
    /// lands in `research_sessions.report_markdown` (read it with
    /// `ff research --show <id>`). Default false = run in the foreground.
    pub detached: bool,
    /// Worker names that must NOT receive any sub-agent, e.g.
    /// `["sia", "adele"]` to keep research off the DGX pairs or `["taylor"]`
    /// to spare the leader. Passed straight to the routing scorer's
    /// `exclude_hosts` (matched case-insensitively against `worker_name`), so
    /// excluded hosts are dropped from the candidate pool before the
    /// round-robin spread. Persisted into the session metadata so a detached
    /// run honors it inside the daemon too. Default empty = use every healthy
    /// deployment.
    pub exclude_hosts: Vec<String>,
}

impl Default for ResearchConfig {
    fn default() -> Self {
        // Empty model + gateway strings ask [`ResearchSession::new`] to
        // resolve them from the live DB at session start — see
        // [`resolve_default_research_model`] and [`resolve_gateway_url`].
        // This keeps the defaults data-driven: as the fleet's model
        // portfolio rotates, new research sessions automatically pick
        // up whatever's actively deployed.
        Self {
            query: String::new(),
            depth: 6,
            parallel: 5,
            output_path: None,
            initiated_by: whoami_tag(),
            planner_model: String::new(),
            subagent_model: String::new(),
            gateway_url: String::new(),
            web_grounding: true,
            detached: false,
            exclude_hosts: Vec::new(),
        }
    }
}

/// Pick a sane default planner/synthesizer/sub-agent model from whatever
/// the fleet is currently serving. Priority:
///
/// 1. A pool alias from `fleet_task_coverage` whose `task` column matches
///    `preferred_task` (e.g. "chain-of-thought" for planner). If the alias
///    has at least one active deployment, use the alias — the gateway
///    resolves it.
/// 2. Any active `computer_model_deployments.model_id` served from a
///    GB10 host (fastest hardware). If multiple, pick the shortest model
///    id (heuristic: shorter = more generic alias like "qwen3-30b").
/// 3. Any active deployment, same ordering.
/// 4. Fallback literal `"qwen3-30b"` — last-resort default matching the
///    Sept 2026 fleet baseline.
///
/// Never hardcodes a specific version in the default path; the fallback
/// is just there so the system degrades gracefully on an empty DB.
pub async fn resolve_default_research_model(pool: &PgPool, preferred_task: &str) -> String {
    // 1) Pool alias with at least one backing deployment.
    let alias_row: Option<(String,)> = sqlx::query_as(
        "SELECT ftc.alias
           FROM fleet_task_coverage ftc
          WHERE ftc.task = $1
            AND ftc.alias IS NOT NULL
            AND EXISTS (
              SELECT 1 FROM computer_model_deployments d
               WHERE d.status = 'active'
                 AND d.openai_compatible = true
            )
          LIMIT 1",
    )
    .bind(preferred_task)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    if let Some((alias,)) = alias_row {
        return alias;
    }

    // 2) GB10-served model.
    let gb10_row: Option<(String,)> = sqlx::query_as(
        "SELECT d.model_id
           FROM computer_model_deployments d
           JOIN computers c ON c.id = d.computer_id
          WHERE d.status = 'active'
            AND d.openai_compatible = true
            AND c.gpu_model LIKE '%GB10%'
          ORDER BY LENGTH(d.model_id), d.started_at DESC NULLS LAST
          LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    if let Some((m,)) = gb10_row {
        return m;
    }

    // 3) Any active.
    let any_row: Option<(String,)> = sqlx::query_as(
        "SELECT d.model_id
           FROM computer_model_deployments d
          WHERE d.status = 'active'
            AND d.openai_compatible = true
          ORDER BY LENGTH(d.model_id), d.started_at DESC NULLS LAST
          LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    if let Some((m,)) = any_row {
        return m;
    }

    // 4) Last-resort literal.
    "qwen3-30b".into()
}

/// Resolve a port number from `port_registry` by service name. Returns
/// `fallback` if the row is missing (graceful degradation — operator may
/// not have seeded the registry yet).
pub async fn resolve_port(pool: &PgPool, service: &str, fallback: u16) -> u16 {
    sqlx::query_scalar::<_, i32>(
        "SELECT port FROM port_registry WHERE service = $1 AND status = 'active' LIMIT 1",
    )
    .bind(service)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .map(|p| p as u16)
    .unwrap_or(fallback)
}

/// Resolve the gateway URL for research LLM calls. Priority order:
///
/// 1. `FORGEFLEET_GATEWAY_URL` env var (operator override).
/// 2. Current fleet leader's `primary_ip` from `fleet_leader_state` +
///    `computers.primary_ip`, with port from `port_registry[forgefleetd]`
///    (fallback 51002 if the registry row is missing).
/// 3. `FORGEFLEET_LEADER_HOST` env var + same port lookup.
/// 4. Loopback at the registry-resolved port.
///
/// No hardcoded ports; the 51002 literal is a last-resort fallback only
/// used when the DB can't answer.
pub async fn resolve_gateway_url(pool: &PgPool) -> String {
    if let Ok(v) = std::env::var("FORGEFLEET_GATEWAY_URL")
        && !v.is_empty()
    {
        return v;
    }
    let port = resolve_port(pool, "forgefleetd", 51002).await;
    let leader_ip: Option<String> = sqlx::query_scalar(
        "SELECT c.primary_ip
           FROM fleet_leader_state fls
           JOIN computers c ON c.id = fls.computer_id
          LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    if let Some(ip) = leader_ip {
        return format!("http://{ip}:{port}");
    }
    if let Ok(host) = std::env::var("FORGEFLEET_LEADER_HOST")
        && !host.is_empty()
    {
        return format!("http://{host}:{port}");
    }
    format!("http://127.0.0.1:{port}")
}

/// Outcome of a research run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchReport {
    pub session_id: Uuid,
    pub query: String,
    pub markdown: String,
    pub subtask_count: usize,
    pub subtasks_succeeded: usize,
    pub subtasks_failed: usize,
    pub duration_ms: u64,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
}

// ─── Planner output shape ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlanDecomposition {
    sub_questions: Vec<String>,
    /// Optional: planner's justification for its decomposition.
    rationale: Option<String>,
}

// ─── Session orchestration ─────────────────────────────────────────────────

pub struct ResearchSession {
    pool: PgPool,
    config: ResearchConfig,
    session_id: Uuid,
}

impl ResearchSession {
    pub async fn new(pool: PgPool, mut config: ResearchConfig) -> Result<Self> {
        // Resolve dynamic defaults if the caller left them empty.
        if config.gateway_url.is_empty() {
            config.gateway_url = resolve_gateway_url(&pool).await;
        }
        if config.planner_model.is_empty() {
            config.planner_model = resolve_default_research_model(&pool, "chain-of-thought").await;
        }
        if config.subagent_model.is_empty() {
            config.subagent_model = resolve_default_research_model(&pool, "code").await;
        }
        let id = Uuid::new_v4();
        // Detached runs are inserted `queued` (started_at NULL until claimed) so
        // the leader's ResearchRunnerTick picks them up and drives the run inside
        // forgefleetd; foreground runs start `planning` immediately. `web_grounding`
        // is persisted so a daemon-claimed run can faithfully reconstruct the
        // config — see [`ResearchSession::claim_next_queued`].
        let initial_status = research_initial_status(config.detached);
        sqlx::query(
            "INSERT INTO research_sessions
                (id, query, status, depth, parallel, output_path, initiated_by,
                 planner_model, synth_model, started_at, metadata)
             VALUES ($1, $2, $10, $3, $4, $5, $6, $7, $7,
                     CASE WHEN $10 = 'queued' THEN NULL ELSE NOW() END,
                     jsonb_build_object('gateway_url', $8::text,
                                        'subagent_model', $9::text,
                                        'web_grounding', $11::bool,
                                        'exclude_hosts', $12::jsonb))",
        )
        .bind(id)
        .bind(&config.query)
        .bind(config.depth as i32)
        .bind(config.parallel as i32)
        .bind(
            config
                .output_path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
        )
        .bind(&config.initiated_by)
        .bind(&config.planner_model)
        .bind(&config.gateway_url)
        .bind(&config.subagent_model)
        .bind(initial_status)
        .bind(config.web_grounding)
        .bind(serde_json::to_value(&config.exclude_hosts).unwrap_or(Value::Null))
        .execute(&pool)
        .await
        .context("insert research_session")?;
        Ok(Self {
            pool,
            config,
            session_id: id,
        })
    }

    pub fn id(&self) -> Uuid {
        self.session_id
    }

    /// Atomically claim the oldest `queued` (detached) research session and
    /// rebuild a runnable [`ResearchSession`] from its persisted row, flipping
    /// it to `planning` (and stamping `started_at`). Returns `Ok(None)` when no
    /// session is queued.
    ///
    /// `FOR UPDATE SKIP LOCKED` makes the claim safe even if more than one
    /// daemon ever runs this (only the live leader does today). The config is
    /// reconstructed entirely from the row + `metadata` so the run is faithful
    /// to what the originating `ff research --detach` requested.
    pub async fn claim_next_queued(pool: PgPool) -> Result<Option<Self>> {
        let row = sqlx::query(
            "UPDATE research_sessions
                SET status = 'planning', started_at = NOW()
              WHERE id = (
                  SELECT id FROM research_sessions
                   WHERE status = 'queued'
                   ORDER BY created_at ASC
                   LIMIT 1
                   FOR UPDATE SKIP LOCKED
              )
            RETURNING id, query, depth, parallel, output_path,
                      planner_model, initiated_by, metadata",
        )
        .fetch_optional(&pool)
        .await
        .context("claim queued research session")?;

        let Some(r) = row else {
            return Ok(None);
        };

        let id: Uuid = r.get("id");
        let query: String = r.get("query");
        let depth: i32 = r.try_get("depth").unwrap_or(6);
        let parallel: i32 = r.try_get("parallel").unwrap_or(5);
        let output_path: Option<String> = r.try_get("output_path").unwrap_or(None);
        let planner_model: Option<String> = r.try_get("planner_model").unwrap_or(None);
        let initiated_by: Option<String> = r.try_get("initiated_by").unwrap_or(None);
        let metadata: Value = r.try_get("metadata").unwrap_or(Value::Null);

        let gateway_url = metadata
            .get("gateway_url")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let subagent_model = metadata
            .get("subagent_model")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        // Default ON to match ResearchConfig::default for any pre-detach rows
        // that lack the key (none exist today, but be defensive).
        let web_grounding = metadata
            .get("web_grounding")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        // Rebuild the exclusion set the originating `--exclude` requested so a
        // daemon-claimed detached run keeps sub-agents off the same hosts.
        // Absent/old rows → empty (no exclusion), matching the default.
        let exclude_hosts = metadata
            .get("exclude_hosts")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let config = ResearchConfig {
            query,
            depth: depth.max(0) as u32,
            parallel: parallel.max(0) as u32,
            output_path: output_path.map(PathBuf::from),
            initiated_by: initiated_by.unwrap_or_else(whoami_tag),
            planner_model: planner_model.unwrap_or_default(),
            subagent_model,
            gateway_url,
            web_grounding,
            detached: true,
            exclude_hosts,
        };

        Ok(Some(Self {
            pool,
            config,
            session_id: id,
        }))
    }

    pub async fn run(
        &self,
        progress: Option<mpsc::Sender<ResearchProgress>>,
    ) -> Result<ResearchReport> {
        let start = Instant::now();
        if let Some(tx) = &progress {
            let _ = tx
                .send(ResearchProgress::Planning {
                    query: self.config.query.clone(),
                })
                .await;
        }

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            // DuckDuckGo's HTML endpoint 202-blocks the default reqwest UA
            // (no User-Agent header), returning an empty bot page — which
            // silently disabled web grounding. A non-curl UA gets 200 + real
            // results. Matches WebSearchTool's client. This same client also
            // makes the LLM gateway calls; a UA header is harmless there.
            .user_agent("ForgeFleet-Agent/0.1")
            .build()
            .expect("build reqwest client");

        // Phase 1 — planner decomposes the query.
        let plan = self.plan(&client).await.context("planner phase")?;
        self.store_plan(&plan).await?;
        if let Some(tx) = &progress {
            let _ = tx
                .send(ResearchProgress::Dispatching {
                    sub_count: plan.sub_questions.len(),
                })
                .await;
        }

        // Phase 2 — pick distinct fleet backends, build AgentTasks.
        let backends = self
            .pick_distinct_backends(plan.sub_questions.len())
            .await
            .context("backend picker")?;
        let subtask_rows = self.insert_subtasks(&plan.sub_questions, &backends).await?;

        // Phase 3 — run sub-agents in parallel. V1: simple chat
        // completions hitting each backend directly (no tools). This
        // gets REAL LLM output, not the AgentSession's empty-loop
        // failure mode we saw with Qwen3-30B + tool-call format.
        // V2 (follow-up): swap back to MultiAgentOrchestrator once the
        // AgentSession + Qwen3 tool-call interop is fixed — then
        // sub-agents can actually call WebSearch / Grep / etc.
        update_session_status(&self.pool, self.session_id, "dispatching").await?;

        // Resolve the optional self-hosted SearXNG grounding backend once (DB
        // `fleet_secrets[searxng.url]` → `SEARXNG_URL` env → None). When unset,
        // grounding falls back to the existing DuckDuckGo → Wikipedia chain, so
        // this is a zero-regression opt-in.
        let searxng_url: Option<String> = if self.config.web_grounding {
            crate::fleet_info::fetch_secret("searxng.url").await
        } else {
            None
        };

        #[allow(clippy::type_complexity)]
        let mut handles: Vec<
            tokio::task::JoinHandle<(Uuid, Result<(String, u64, u64, u64)>)>,
        > = Vec::with_capacity(plan.sub_questions.len());
        for (i, ((q, row), backend)) in plan
            .sub_questions
            .iter()
            .zip(subtask_rows.iter())
            .zip(backends.iter())
            .enumerate()
        {
            let base_prompt = self.build_subagent_prompt(i, q, &plan.sub_questions);
            let endpoint = backend.endpoint.clone();
            let model = backend.model_id.clone();
            let row_id = row.id;
            let web_grounding = self.config.web_grounding;
            let sub_question = q.clone();
            let searxng_url = searxng_url.clone();
            handles.push(tokio::spawn({
                let client = client.clone();
                async move {
                    let t0 = Instant::now();
                    // Sub-agents run as plain completions (no live tools), so we
                    // search the web FOR them here and inject the results. Each
                    // sub-agent's search runs concurrently with the others (one
                    // per spawned task). Falls back to ungrounded on failure.
                    let prompt = if web_grounding {
                        // Stagger the searches so a `--parallel N` run doesn't
                        // fire N DuckDuckGo requests in the same instant (which
                        // trips DDG's 202 throttle); combined with retry-on-202
                        // inside fetch_search_results this keeps sub-agents
                        // grounded instead of falling back to model memory.
                        tokio::time::sleep(search_stagger(i)).await;
                        let ctx = crate::tools::web_search::fetch_web_context(
                            &client,
                            searxng_url.as_deref(),
                            &sub_question,
                            8,
                        )
                        .await;
                        info!(
                            sub = i,
                            grounded = ctx.is_some(),
                            "research sub-agent web grounding"
                        );
                        match ctx {
                            Some(ctx) => format!(
                                "{base_prompt}\n\n\
                                 ── Live web search results for your sub-question \
                                 ──\nThese were fetched for you (you cannot run \
                                 more searches). Treat them as your primary \
                                 sources and cite the URLs:\n\n{ctx}"
                            ),
                            None => base_prompt,
                        }
                    } else {
                        base_prompt
                    };
                    let out = openai_single_completion_with_usage(
                        &endpoint, &model, &prompt, 8192, &client,
                    )
                    .await
                    .map(|(s, tin, tout)| (s, t0.elapsed().as_millis() as u64, tin, tout));
                    (row_id, out)
                }
            }));
        }

        // Collect; map each finished handle to AgentTaskResult shape so the
        // existing persist + synthesize code stays unchanged.
        let mut results: Vec<AgentTaskResult> = Vec::with_capacity(plan.sub_questions.len());
        for (row, handle) in subtask_rows.iter().zip(handles.into_iter()) {
            let (_row_id, res) = handle
                .await
                .unwrap_or_else(|e| (row.id, Err(anyhow::anyhow!("subagent panic: {e}"))));
            let (status, output, dur, tin, tout) = match res {
                Ok((text, dur, tin, tout)) => (TaskStatus::Completed, text, dur, tin, tout),
                Err(e) => {
                    warn!(subtask = %row.id, error = %e, "research sub-agent failed");
                    (
                        TaskStatus::Failed,
                        format!("(sub-agent error: {e})"),
                        0,
                        0,
                        0,
                    )
                }
            };
            results.push(AgentTaskResult {
                task_id: row.id.to_string(),
                status,
                output,
                events: Vec::new(),
                duration_ms: dur,
                turn_count: 1,
                tokens_in: tin,
                tokens_out: tout,
            });
        }

        if let Some(tx) = &progress {
            let _ = tx.send(ResearchProgress::Synthesizing).await;
        }

        // Phase 4 — persist each sub-agent's output.
        let mut succeeded = 0usize;
        let mut failed = 0usize;
        let mut total_tokens_in: u64 = 0;
        let mut total_tokens_out: u64 = 0;
        for (row, result) in subtask_rows.iter().zip(results.iter()) {
            // MaxTurns produces useful (if truncated) output — count as
            // success so the session doesn't get marked "failed" just
            // because the agent used up its turn budget. Only Cancelled
            // or Failed count as failures.
            let useful = matches!(result.status, TaskStatus::Completed | TaskStatus::MaxTurns);
            if useful {
                succeeded += 1;
            } else {
                failed += 1;
            }
            let (tin, tout) = (result.tokens_in, result.tokens_out);
            total_tokens_in += tin;
            total_tokens_out += tout;
            self.store_subtask_result(row.id, result, tin, tout).await?;

            // Log each sub-agent turn to ff_interactions (training corpus): a
            // sub-question -> web-grounded answer pair from a specific fleet
            // model + computer — the granular signal the session-level research
            // log (#447) can't capture. Best-effort; never fails the run.
            let rec = ff_db::InteractionRecord {
                channel: "research_subtask".to_string(),
                request_text: row.sub_question.chars().take(16000).collect(),
                engine: Some(row.assigned_model.clone()),
                response_text: result.output.chars().take(16000).collect(),
                tokens_in: i32::try_from(tin).unwrap_or(0),
                tokens_out: i32::try_from(tout).unwrap_or(0),
                latency_ms: i32::try_from(result.duration_ms).ok(),
                outcome: if useful { "success" } else { "error" }.to_string(),
                worker_name: Some(row.assigned_computer.clone()),
                ..Default::default()
            };
            if let Err(e) = ff_db::pg_record_interaction(&self.pool, &rec).await {
                tracing::warn!(error = %e, "research: subtask interaction log failed (non-fatal)");
            }
        }

        // Phase 5 — synthesizer merges sub-agent outputs into a report.
        update_session_status(&self.pool, self.session_id, "synthesizing").await?;
        if let Some(tx) = &progress {
            let _ = tx.send(ResearchProgress::Synthesizing).await;
        }

        let markdown = self
            .synthesize(&plan, &subtask_rows, &results, &client)
            .await
            .context("synthesizer phase")?;
        let duration_ms = start.elapsed().as_millis() as u64;

        sqlx::query(
            "UPDATE research_sessions
                SET status         = CASE WHEN $3 > 0 AND $4 = 0 THEN 'failed' ELSE 'done' END,
                    report_markdown   = $1,
                    completed_at      = NOW(),
                    duration_ms       = $2,
                    total_tokens_in   = $5,
                    total_tokens_out  = $6
              WHERE id = $7",
        )
        .bind(&markdown)
        .bind(duration_ms as i64)
        .bind(failed as i64)
        .bind(succeeded as i64)
        .bind(total_tokens_in as i64)
        .bind(total_tokens_out as i64)
        .bind(self.session_id)
        .execute(&self.pool)
        .await
        .context("mark session done")?;

        // Log the research turn to ff_interactions (the ff-LLM training corpus).
        // The headline signal — query → web-grounded multi-step synthesis —
        // otherwise lived ONLY in research_findings/sessions, invisible to ff's
        // own training data (the same gap closed for council #442 / dispatch
        // #430). Best-effort: a log failure never fails the research run.
        let outcome = if failed > 0 && succeeded == 0 {
            "error"
        } else {
            "success"
        };
        let rec = ff_db::InteractionRecord {
            channel: "research".to_string(),
            request_text: self.config.query.chars().take(16000).collect(),
            engine: Some(self.config.planner_model.clone()),
            response_text: markdown.chars().take(16000).collect(),
            tokens_in: i32::try_from(total_tokens_in).unwrap_or(0),
            tokens_out: i32::try_from(total_tokens_out).unwrap_or(0),
            latency_ms: i32::try_from(duration_ms).ok(),
            outcome: outcome.to_string(),
            endpoint: Some(self.config.gateway_url.clone()),
            ..Default::default()
        };
        if let Err(e) = ff_db::pg_record_interaction(&self.pool, &rec).await {
            tracing::warn!(error = %e, "research: failed to log interaction (non-fatal)");
        }

        // Write the report to disk if an output path was provided.
        if let Some(path) = &self.config.output_path {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(path, &markdown)
                .with_context(|| format!("write report to {}", path.display()))?;
        }

        Ok(ResearchReport {
            session_id: self.session_id,
            query: self.config.query.clone(),
            markdown,
            subtask_count: subtask_rows.len(),
            subtasks_succeeded: succeeded,
            subtasks_failed: failed,
            duration_ms,
            total_tokens_in,
            total_tokens_out,
        })
    }

    /// Re-synthesize a research report from already-persisted sub-agent
    /// outputs — recovery for a run that died after its sub-agents finished.
    ///
    /// The live [`run`](Self::run) flow persists every sub-agent's output to
    /// `research_subtasks` (Phase 4) BEFORE the synthesizer turn (Phase 5).
    /// The orchestrator + synthesizer live in ONE foreground CLI process (they
    /// are NOT daemon-managed), so killing/crashing the CLI mid-run loses the
    /// synthesis even though the EXPENSIVE sub-agent work already landed in
    /// Postgres; the job sweeper (#330) then reaps the orphaned session to
    /// `failed`. This recovers that work: reload the stored plan + the
    /// per-subtask outputs and run ONLY the synthesizer turn — no sub-agents
    /// are re-dispatched — then write the report and flip the session to
    /// `done`.
    ///
    /// Idempotent: safe to re-run on an already-`done` session (re-synthesizes
    /// from the same inputs). Bails with a clear message when the session never
    /// got past planning (no `planner_output`) or produced zero usable
    /// sub-agent outputs, since there is nothing to synthesize in those cases.
    pub async fn recover(pool: PgPool, session_id: Uuid) -> Result<ResearchReport> {
        let start = Instant::now();

        // 1. Load the session: query + planner config + stored plan + gateway.
        let srow = sqlx::query(
            "SELECT query, planner_model, planner_output, metadata
               FROM research_sessions WHERE id = $1",
        )
        .bind(session_id)
        .fetch_optional(&pool)
        .await
        .context("load research_session")?
        .ok_or_else(|| anyhow::anyhow!("no research_session with id {session_id}"))?;

        let query: String = srow.get("query");
        let planner_model: Option<String> = srow.try_get("planner_model").unwrap_or(None);
        let planner_output: Option<Value> = srow.try_get("planner_output").unwrap_or(None);
        let metadata: Value = srow.try_get("metadata").unwrap_or(Value::Null);

        let Some(plan_json) = planner_output else {
            anyhow::bail!(
                "session {session_id} has no planner_output — it never got past \
                 planning; nothing to synthesize"
            );
        };
        let plan: PlanDecomposition =
            serde_json::from_value(plan_json).context("parse stored planner_output")?;

        // Rebuild the config from what was persisted; fall back to live
        // resolution for anything the row didn't carry (older rows, NULLs).
        let gateway_from_meta = metadata
            .get("gateway_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let mut config = ResearchConfig {
            query: query.clone(),
            gateway_url: gateway_from_meta,
            planner_model: planner_model.unwrap_or_default(),
            ..Default::default()
        };
        if config.gateway_url.is_empty() {
            config.gateway_url = resolve_gateway_url(&pool).await;
        }
        if config.planner_model.is_empty() {
            config.planner_model = resolve_default_research_model(&pool, "chain-of-thought").await;
        }

        // 2. Load the persisted sub-task outputs in order.
        let subrows = sqlx::query(
            "SELECT id, ordinal, sub_question, assigned_computer,
                    assigned_endpoint, assigned_model, status, output_markdown
               FROM research_subtasks
              WHERE session_id = $1
              ORDER BY ordinal",
        )
        .bind(session_id)
        .fetch_all(&pool)
        .await
        .context("load research_subtasks")?;
        if subrows.is_empty() {
            anyhow::bail!("session {session_id} has no sub-tasks — nothing to synthesize");
        }

        // 3. Reconstruct the SubtaskRow + AgentTaskResult shapes the synthesizer
        //    consumes, inverting store_subtask_result's status mapping.
        let mut subtask_rows: Vec<SubtaskRow> = Vec::with_capacity(subrows.len());
        let mut results: Vec<AgentTaskResult> = Vec::with_capacity(subrows.len());
        let mut succeeded = 0usize;
        let mut failed = 0usize;
        let mut usable_outputs = 0usize;
        for r in &subrows {
            let id: Uuid = r.get("id");
            let ordinal: i32 = r.get("ordinal");
            let sub_question: String = r.get("sub_question");
            let assigned_computer: Option<String> = r.try_get("assigned_computer").unwrap_or(None);
            let assigned_endpoint: Option<String> = r.try_get("assigned_endpoint").unwrap_or(None);
            let assigned_model: Option<String> = r.try_get("assigned_model").unwrap_or(None);
            let status_str: String = r.get("status");
            let output: String = r
                .try_get::<Option<String>, _>("output_markdown")
                .unwrap_or(None)
                .unwrap_or_default();

            let status = subtask_status_from_db(&status_str);
            if !output.trim().is_empty() {
                usable_outputs += 1;
            }
            if matches!(status, TaskStatus::Completed | TaskStatus::MaxTurns) {
                succeeded += 1;
            } else {
                failed += 1;
            }

            subtask_rows.push(SubtaskRow {
                id,
                ordinal: ordinal as u32,
                sub_question,
                assigned_computer: assigned_computer.unwrap_or_else(|| "-".into()),
                _assigned_endpoint: assigned_endpoint.unwrap_or_default(),
                assigned_model: assigned_model.unwrap_or_else(|| "-".into()),
            });
            results.push(AgentTaskResult {
                task_id: id.to_string(),
                status,
                output,
                events: Vec::new(),
                duration_ms: 0,
                turn_count: 1,
                // Reconstructed from persisted rows for the synthesizer; not
                // re-logged to ff_interactions, so token counts aren't needed.
                tokens_in: 0,
                tokens_out: 0,
            });
        }
        if usable_outputs == 0 {
            anyhow::bail!(
                "session {session_id}: all {} sub-task(s) have empty output — \
                 nothing to synthesize (re-run the query instead)",
                subrows.len()
            );
        }

        // 4. Synthesize ONLY — no sub-agents re-dispatched.
        let session = ResearchSession {
            pool,
            config,
            session_id,
        };
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("build reqwest client");
        let markdown = session
            .synthesize(&plan, &subtask_rows, &results, &client)
            .await
            .context("recover: synthesizer phase")?;
        let duration_ms = start.elapsed().as_millis() as u64;

        // 5. Persist the recovered report. Mirror run()'s done/failed rule:
        //    'failed' only when every sub-task failed. Preserve the original
        //    duration_ms (the recovery synth time is not the run time) and
        //    clear any reaper error now that we have a real report.
        sqlx::query(
            "UPDATE research_sessions
                SET status          = CASE WHEN $3 > 0 AND $4 = 0 THEN 'failed' ELSE 'done' END,
                    report_markdown = $1,
                    completed_at    = NOW(),
                    duration_ms     = COALESCE(duration_ms, $2),
                    error           = NULL
              WHERE id = $5",
        )
        .bind(&markdown)
        .bind(duration_ms as i64)
        .bind(failed as i64)
        .bind(succeeded as i64)
        .bind(session.session_id)
        .execute(&session.pool)
        .await
        .context("recover: mark session done")?;

        // Log the recovered turn to ff_interactions (training corpus) — recover()
        // re-synthesizes a killed run's report and was the one research path not
        // logged (run() does so at #447). Best-effort; never fails the recovery.
        let outcome = if failed > 0 && succeeded == 0 {
            "error"
        } else {
            "success"
        };
        let rec = ff_db::InteractionRecord {
            channel: "research".to_string(),
            request_text: query.chars().take(16000).collect(),
            engine: Some(session.config.planner_model.clone()),
            response_text: markdown.chars().take(16000).collect(),
            latency_ms: i32::try_from(duration_ms).ok(),
            outcome: outcome.to_string(),
            endpoint: Some(session.config.gateway_url.clone()),
            ..Default::default()
        };
        if let Err(e) = ff_db::pg_record_interaction(&session.pool, &rec).await {
            tracing::warn!(error = %e, "research recover: failed to log interaction (non-fatal)");
        }

        Ok(ResearchReport {
            session_id: session.session_id,
            query,
            markdown,
            subtask_count: subtask_rows.len(),
            subtasks_succeeded: succeeded,
            subtasks_failed: failed,
            duration_ms,
            total_tokens_in: 0,
            total_tokens_out: 0,
        })
    }

    // ─── Planner turn ────────────────────────────────────────────────────

    async fn plan(&self, client: &reqwest::Client) -> Result<PlanDecomposition> {
        let prompt = format!(
            "You are the research planner for a multi-agent investigation.\n\n\
             The operator's question:\n{}\n\n\
             Decompose this into EXACTLY {} focused sub-questions that, taken \
             together, would give the operator a complete, well-grounded answer. \
             Each sub-question should be:\n\
             - answerable independently (parallelism = no cross-dependency)\n\
             - specific enough that one researcher could deep-dive it in a single \
               session\n\
             - written as a question (ends with '?')\n\
             - free of overlap with the other sub-questions\n\n\
             Return ONLY valid JSON of shape:\n\
             {{\"sub_questions\": [\"Q1?\", \"Q2?\", ...], \"rationale\": \"one \
             paragraph on why this decomposition\"}}\n\n\
             No prose outside the JSON. No markdown fences. Just JSON.",
            self.config.query, self.config.parallel
        );
        // Thinking-mode pool aliases burn tokens on internal reasoning
        // BEFORE producing content. 16384 budget covers both the thinking
        // scratch + the actual JSON output. Standard instruct models use
        // a small fraction of this.
        let raw = openai_single_completion(
            &self.config.gateway_url,
            &self.config.planner_model,
            &prompt,
            16384,
            client,
        )
        .await
        .context("planner OpenAI call")?;
        let trimmed = strip_json_fences(&raw);
        let plan: PlanDecomposition = serde_json::from_str(trimmed)
            .with_context(|| format!("parse planner output: {raw}"))?;
        if plan.sub_questions.is_empty() {
            anyhow::bail!("planner returned empty sub_questions");
        }
        info!(
            session = %self.session_id,
            count = plan.sub_questions.len(),
            "research planner decomposed query"
        );
        Ok(plan)
    }

    async fn store_plan(&self, plan: &PlanDecomposition) -> Result<()> {
        sqlx::query(
            "UPDATE research_sessions
                SET planner_output = $1,
                    status = 'dispatching'
              WHERE id = $2",
        )
        .bind(serde_json::to_value(plan)?)
        .bind(self.session_id)
        .execute(&self.pool)
        .await
        .context("store planner output")?;
        Ok(())
    }

    // ─── Sub-agent dispatch ──────────────────────────────────────────────

    /// Pick `n` fleet backends for the sub-agents, one per sub-question.
    ///
    /// Routes through the SAME health-floored scorer the agent + offload
    /// pickers use (`ff_db::pg_route_deployments` with
    /// `max_health_age_sec = DISPATCH_HEALTH_MAX_AGE_SEC`), so a wedged/offline
    /// host whose deployment still reads `healthy` with a stale `last_health_at`
    /// is skipped instead of hanging a sub-agent for the full request timeout
    /// (the priya-wedge failure mode — PR #332/#333). The previous query hit
    /// `computer_model_deployments` filtered only on `status = 'active'`, which
    /// carried NO health/freshness signal and routinely dispatched to dead
    /// endpoints — the likely reason research runs so often died mid-dispatch.
    ///
    /// `require_tool_calling = true` is the fleet's reliable "this is a
    /// chat-completion model, not an embedding/reranking server" filter (the
    /// offload picker relies on the same flag): without it a healthy `bge-m3`
    /// embedding deployment would be a candidate and a sub-agent POSTing a chat
    /// completion to it would 4xx. Every agent-grade chat model on the fleet is
    /// tool-calling, so this loses no usable research backend. Candidates come
    /// back ordered tier-ASC then freshest-first; we then round-robin across
    /// DISTINCT computers to maximize real parallelism.
    async fn pick_distinct_backends(&self, n: usize) -> Result<Vec<FleetBackend>> {
        let filter = ff_db::RouteFilter {
            workload: None,
            require_tool_calling: true,
            min_ctx: None,
            exclude_hosts: self.config.exclude_hosts.clone(),
            max_health_age_sec: Some(ff_db::queries::DISPATCH_HEALTH_MAX_AGE_SEC),
            // Among equal-tier hosts, order least-loaded first so the distinct-
            // computer round-robin below picks the idlest boxes first (and the
            // cycle phase, when fan-out > distinct hosts, doubles up on the
            // least-busy rather than whichever last heartbeated).
            prefer_least_loaded: true,
            // Generous cap: we want every healthy deployment so the round-robin
            // can spread across as many distinct computers as the fleet has.
            limit: 256,
        };
        let candidates = ff_db::pg_route_deployments(&self.pool, &filter)
            .await
            .context("route healthy LLM deployments for research sub-agents")?;

        if candidates.is_empty() {
            anyhow::bail!(
                "no healthy OpenAI-compatible LLM deployments within the {}s \
                 health-freshness window — start one with `ff model load`, or \
                 check `ff fleet route chat` for wedged hosts",
                ff_db::queries::DISPATCH_HEALTH_MAX_AGE_SEC,
            );
        }

        // `pg_route_deployments` already builds `http://{host}:{port}` from the
        // LAN primary_ip and resolves the port from the deployment row, so no
        // loopback rewrite is needed here.
        let backends: Vec<FleetBackend> = candidates
            .into_iter()
            .map(|c| FleetBackend {
                computer_name: c.worker_name,
                endpoint: c.endpoint,
                // Same identifier offload/MCP dispatch sends as the OpenAI
                // `model` field (catalog id, name fallback). llama.cpp ignores
                // it; vLLM matches it.
                model_id: c.catalog_id.or(c.catalog_name).unwrap_or_default(),
            })
            .collect();

        Ok(select_distinct_round_robin(backends, n))
    }

    async fn insert_subtasks(
        &self,
        qs: &[String],
        backends: &[FleetBackend],
    ) -> Result<Vec<SubtaskRow>> {
        let mut out = Vec::with_capacity(qs.len());
        for (i, q) in qs.iter().enumerate() {
            let b = &backends[i];
            let id: Uuid = sqlx::query_scalar(
                "INSERT INTO research_subtasks
                    (session_id, ordinal, sub_question, assigned_computer,
                     assigned_endpoint, assigned_model, status, started_at)
                 VALUES ($1, $2, $3, $4, $5, $6, 'running', NOW())
                 RETURNING id",
            )
            .bind(self.session_id)
            .bind(i as i32)
            .bind(q)
            .bind(&b.computer_name)
            .bind(&b.endpoint)
            .bind(&b.model_id)
            .fetch_one(&self.pool)
            .await
            .context("insert research_subtask")?;
            out.push(SubtaskRow {
                id,
                ordinal: i as u32,
                sub_question: q.clone(),
                assigned_computer: b.computer_name.clone(),
                _assigned_endpoint: b.endpoint.clone(),
                assigned_model: b.model_id.clone(),
            });
        }
        Ok(out)
    }

    fn build_subagent_prompt(&self, i: usize, sub: &str, all: &[String]) -> String {
        let peers: String = all
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, q)| format!("- {q}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "You are research sub-agent #{idx} of {total} on a multi-agent \
             investigation.\n\n\
             Overall operator question: {overall}\n\n\
             Your specific sub-question:\n{sub}\n\n\
             Other sub-agents are handling:\n{peers}\n\n\
             Guidelines:\n\
             1. Live web search results for your sub-question are provided below \
                (when available). Treat them as your PRIMARY sources and cite \
                their URLs. You cannot run additional searches or open files, so \
                reason from the provided results plus your own knowledge; bias \
                toward primary sources (papers, repos, vendor docs).\n\
             2. If the provided results don't cover something, reason from your \
                training knowledge but mark it \"unverified\" — never invent a URL.\n\
             3. Quote specific snippets with URLs. A claim without a source is \
                worth less than no claim.\n\
             4. If you're uncertain on something, say so — mark it \
                \"unverified\" or \"uncertain\". Do NOT fabricate.\n\
             5. Your output is MERGED WITH OTHER AGENTS' by a synthesizer, so \
                stay tightly on your sub-question. Don't answer the others.\n\
             6. End with a structured section:\n\
                ## Findings\n\
                Each finding on one line: `[confidence 0.0-1.0] <claim> \
                <URL>`\n\n\
             Return your full reasoning followed by the structured Findings \
             section.",
            idx = i + 1,
            total = all.len(),
            overall = self.config.query,
            sub = sub,
            peers = if peers.is_empty() {
                "(none — you're alone)".into()
            } else {
                peers
            },
        )
    }

    async fn store_subtask_result(
        &self,
        subtask_id: Uuid,
        result: &AgentTaskResult,
        tokens_in: u64,
        tokens_out: u64,
    ) -> Result<()> {
        let status = match result.status {
            TaskStatus::Completed => "done",
            TaskStatus::MaxTurns => "max_turns",
            TaskStatus::Cancelled => "cancelled",
            TaskStatus::Failed => "failed",
        };
        sqlx::query(
            "UPDATE research_subtasks
                SET status        = $1,
                    output_markdown = $2,
                    turn_count    = $3,
                    completed_at  = NOW(),
                    duration_ms   = $4,
                    tokens_in     = $5,
                    tokens_out    = $6
              WHERE id = $7",
        )
        .bind(status)
        .bind(&result.output)
        .bind(result.turn_count as i32)
        .bind(result.duration_ms as i64)
        .bind(tokens_in as i64)
        .bind(tokens_out as i64)
        .bind(subtask_id)
        .execute(&self.pool)
        .await
        .context("update research_subtask")?;

        // Best-effort: parse `[0.85] Claim <URL>` lines from the output and
        // write them to research_findings.
        for (claim, conf, url) in parse_findings(&result.output) {
            let _ = sqlx::query(
                "INSERT INTO research_findings
                    (session_id, subtask_id, claim, source_url, confidence,
                     source_kind)
                 VALUES ($1, $2, $3, $4, $5,
                         CASE WHEN $4 IS NULL THEN 'model_memory' ELSE 'web' END)",
            )
            .bind(self.session_id)
            .bind(subtask_id)
            .bind(&claim)
            .bind(&url)
            .bind(conf)
            .execute(&self.pool)
            .await;
        }
        Ok(())
    }

    // ─── Synthesizer turn ────────────────────────────────────────────────

    async fn synthesize(
        &self,
        plan: &PlanDecomposition,
        subtasks: &[SubtaskRow],
        results: &[AgentTaskResult],
        client: &reqwest::Client,
    ) -> Result<String> {
        let mut sub_section = String::new();
        for (row, result) in subtasks.iter().zip(results.iter()) {
            let status = match result.status {
                TaskStatus::Completed => "✓",
                TaskStatus::MaxTurns => "⧗ (max_turns)",
                TaskStatus::Cancelled => "✗ cancelled",
                TaskStatus::Failed => "✗ failed",
            };
            sub_section.push_str(&format!(
                "\n\n### Sub-question {n} (handled by {computer} / {model}, status={status})\n\n\
                 **Q:** {q}\n\n\
                 **Output:**\n\n{out}\n",
                n = row.ordinal + 1,
                computer = row.assigned_computer,
                model = row.assigned_model,
                status = status,
                q = row.sub_question,
                out = &result.output,
            ));
        }

        let prompt = format!(
            "You are the research synthesizer. Your job is to merge N \
             sub-agent reports into ONE cohesive, well-cited answer to \
             the operator's original question.\n\n\
             Original question: {query}\n\n\
             Planner's rationale for the decomposition: {rationale}\n\n\
             Sub-agent reports:\n{subs}\n\n\
             Produce a markdown report with these sections:\n\
             1. **TL;DR** — 3-5 bullet answer for the operator.\n\
             2. **Detailed findings** — the substance, organized thematically \
                (NOT per-sub-question; merge overlaps).\n\
             3. **Disagreements + uncertainty** — where sub-agents disagreed \
                or a finding is unverified, call it out explicitly.\n\
             4. **Citations** — numbered list of all URLs referenced, in \
                order of appearance. Inline, use [1], [2], etc.\n\
             5. **Open questions** — what the investigation did NOT answer \
                and would need follow-up.\n\n\
             Rules:\n\
             - If a sub-agent produced garbage / didn't run, IGNORE it.\n\
             - Prefer claims with higher confidence from multiple sources.\n\
             - Be honest about limits — do NOT fabricate citations.\n\
             - Length: no artificial limit, but prefer dense over padded.",
            query = self.config.query,
            rationale = plan.rationale.as_deref().unwrap_or("(none supplied)"),
            subs = sub_section,
        );

        // Synthesizer produces a long markdown report + cites many
        // sub-agent outputs. Thinking-mode models again can spend ~half
        // their budget on internal reasoning, so the 32k cap leaves room.
        let raw = openai_single_completion(
            &self.config.gateway_url,
            &self.config.planner_model, // reuse planner alias for synthesis
            &prompt,
            32768,
            client,
        )
        .await
        .context("synthesizer OpenAI call")?;
        // Strip any `<think>…</think>` blocks the synthesizer emitted (Qwen3-
        // family and DeepSeek-R1-distill always emit them; llama.cpp's
        // `enable_thinking=false` flag is non-functional per GH #13189). The
        // reasoning is useful to the model but noise to the operator reading
        // the final report. Applied defensively — no-op when no block present.
        Ok(strip_think_blocks(&raw))
    }
}

/// Outcome of one [`auto_recover_stale`] pass.
#[derive(Debug, Clone, Default)]
pub struct AutoRecoverSummary {
    /// Sessions a recovery synthesis was attempted on this pass.
    pub attempted: usize,
    /// Sessions that produced a report (status flipped to `done`/`failed`
    /// with `report_markdown` set).
    pub recovered: usize,
    /// Sessions whose recovery synthesis errored (gateway down, etc.) — they
    /// stay `failed` and are retried on a later pass until `max_attempts`.
    pub failed: usize,
}

/// Default cap on autonomous recovery attempts per session. Manual
/// `ff research --recover <id>` always works regardless of this cap.
pub const MAX_AUTO_RECOVER_ATTEMPTS: i64 = 2;

/// Autonomous counterpart to `ff research --recover`: find `failed` research
/// sessions that have persisted sub-task output but never got a synthesized
/// report, and run the synthesizer-only recovery on each — no operator needed.
///
/// A run whose foreground CLI was killed (or whose synthesis timed out) is
/// reaped to `failed` by the stale-job sweeper, but its expensive sub-agent
/// work survives in `research_subtasks`. `recover()` turns that latent work
/// into a finished report; this fn finds every such session and drives it.
///
/// Bounded so a permanently-unsynthesizable session can't burn LLM calls
/// forever: each attempt bumps `metadata.auto_recover_attempts` BEFORE the
/// synthesis runs (so a hang/panic still counts), and sessions at or above
/// `max_attempts` are skipped. A successful recovery sets `report_markdown`,
/// which removes the session from the candidate set on its own.
///
/// Leader-gate this at the call site — it is a fleet-wide DB scan plus N
/// gateway calls; only one daemon should drive it.
pub async fn auto_recover_stale(
    pool: &PgPool,
    max_attempts: i64,
    limit: i64,
) -> Result<AutoRecoverSummary> {
    let candidates = sqlx::query_scalar::<_, Uuid>(
        "SELECT s.id
           FROM research_sessions s
          WHERE s.status = 'failed'
            AND s.report_markdown IS NULL
            AND s.planner_output IS NOT NULL
            AND COALESCE((s.metadata->>'auto_recover_attempts')::int, 0) < $1
            AND EXISTS (
                SELECT 1 FROM research_subtasks st
                 WHERE st.session_id = s.id
                   AND COALESCE(st.output_markdown, '') <> ''
            )
          ORDER BY s.created_at
          LIMIT $2",
    )
    .bind(max_attempts)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("auto_recover: select candidates")?;

    let mut summary = AutoRecoverSummary::default();
    for id in candidates {
        // Record the attempt FIRST — if recovery hangs or the process dies
        // mid-synthesis, this session is still counted toward max_attempts and
        // can't be retried forever.
        if let Err(e) = sqlx::query(
            "UPDATE research_sessions
                SET metadata = jsonb_set(
                        metadata,
                        '{auto_recover_attempts}',
                        to_jsonb(COALESCE((metadata->>'auto_recover_attempts')::int, 0) + 1),
                        true)
              WHERE id = $1",
        )
        .bind(id)
        .execute(pool)
        .await
        {
            warn!(session = %id, error = %e, "auto_recover: bump attempt counter failed; skipping");
            continue;
        }
        summary.attempted += 1;
        match ResearchSession::recover(pool.clone(), id).await {
            Ok(_) => {
                summary.recovered += 1;
                info!(session = %id, "auto_recover: synthesized report for reaped research session");
            }
            Err(e) => {
                summary.failed += 1;
                warn!(session = %id, error = %e, "auto_recover: synthesis failed (retried up to max_attempts)");
            }
        }
    }
    Ok(summary)
}

/// Initial `research_sessions.status` for a new session. Detached runs MUST be
/// `queued` so the leader's [`ResearchRunnerTick`] is the only thing that runs
/// them; a foreground run starts `planning` immediately. Getting this wrong is a
/// silent black hole — a detached row inserted as `planning` would never be
/// claimed by the runner (it only claims `queued`) and never run by a CLI.
pub fn research_initial_status(detached: bool) -> &'static str {
    if detached { "queued" } else { "planning" }
}

/// A persisted research session's current state, for read-only display
/// (`ff research --show <id>`). `report` is `Some` once synthesis finished.
#[derive(Debug, Clone)]
pub struct ResearchStatus {
    pub id: Uuid,
    pub query: String,
    pub status: String,
    pub report: Option<String>,
    pub error: Option<String>,
    pub subtask_total: i64,
    pub subtask_done: i64,
}

/// Fetch a research session's status + (if finished) report for read-only
/// display. Returns `Ok(None)` for an unknown id. Unlike `recover()` this never
/// re-dispatches or re-synthesizes — it just reads what's in the DB, which is
/// what you want for polling a detached (`--detach`) run.
pub async fn fetch_status(pool: &PgPool, id: Uuid) -> Result<Option<ResearchStatus>> {
    let row = sqlx::query(
        "SELECT query, status, report_markdown, error,
                (SELECT COUNT(*) FROM research_subtasks st WHERE st.session_id = s.id) AS sub_total,
                (SELECT COUNT(*) FROM research_subtasks st
                  WHERE st.session_id = s.id
                    -- usable outputs: store_subtask_result maps Completed→'done',
                    -- MaxTurns→'max_turns' (see the status match in run()).
                    AND st.status IN ('done', 'max_turns')) AS sub_done
           FROM research_sessions s
          WHERE s.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("fetch research session status")?;

    let Some(r) = row else {
        return Ok(None);
    };
    Ok(Some(ResearchStatus {
        id,
        query: r.try_get("query").unwrap_or_default(),
        status: r.try_get("status").unwrap_or_default(),
        report: r.try_get("report_markdown").unwrap_or(None),
        error: r.try_get("error").unwrap_or(None),
        subtask_total: r.try_get("sub_total").unwrap_or(0),
        subtask_done: r.try_get("sub_done").unwrap_or(0),
    }))
}

/// How often `forgefleetd` checks for `queued` (detached) research sessions.
const RESEARCH_RUNNER_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Max detached sessions to launch per tick. Each launched run gets its own
/// detached task and proceeds to completion independently; this only bounds how
/// many we *start* per 30s so a sudden burst of `--detach` submissions doesn't
/// spike the leader. A backlog drains across successive ticks.
const RESEARCH_RUNNER_MAX_PER_TICK: usize = 2;

/// Production tick that drives detached (`ff research --detach`) runs to
/// completion inside `forgefleetd` on the leader.
///
/// `ff research --detach` inserts a `queued` session and exits, so the run no
/// longer dies with the originating CLI. This tick claims those sessions
/// ([`ResearchSession::claim_next_queued`]) and spawns each [`ResearchSession::run`]
/// as a detached task. If the *leader itself* dies mid-run, the stale-job
/// sweeper + [`auto_recover_stale`] still salvage any completed sub-agent work —
/// detach closes the remaining gap where a killed CLI lost the whole run.
///
/// Leader-gated on every fire (NOT at spawn): claiming + driving runs is a
/// single-owner operation, so only the live leader does it. Safe to start on
/// every daemon; followers no-op. New ticks live in `src/main.rs` (forgefleetd)
/// per [`feedback_two_daemons`].
pub struct ResearchRunnerTick {
    pg: PgPool,
}

impl ResearchRunnerTick {
    pub fn new(pg: PgPool, _my_name: String) -> Self {
        Self { pg }
    }

    /// True iff this process currently owns leadership.
    async fn is_live_leader(&self) -> bool {
        crate::leader_cache::is_current_leader()
    }

    /// Spawn the 30s detached-run loop. Leadership is gated inside the loop on
    /// every fire, so this is safe to start on every daemon.
    pub fn spawn(
        self,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(RESEARCH_RUNNER_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        if !self.is_live_leader().await {
                            continue;
                        }
                        for _ in 0..RESEARCH_RUNNER_MAX_PER_TICK {
                            match ResearchSession::claim_next_queued(self.pg.clone()).await {
                                Ok(Some(session)) => {
                                    let id = session.id();
                                    info!(session = %id, "research runner: launching detached session");
                                    // Detached: the run drives itself to completion
                                    // (writing report_markdown + terminal status)
                                    // independent of this tick's cadence.
                                    tokio::spawn(async move {
                                        match session.run(None).await {
                                            Ok(rep) => info!(
                                                session = %id,
                                                subtasks = rep.subtask_count,
                                                succeeded = rep.subtasks_succeeded,
                                                "research runner: detached session complete"
                                            ),
                                            Err(e) => warn!(
                                                session = %id, error = %e,
                                                "research runner: detached session failed \
                                                 (sweeper/auto-recover will salvage partial work)"
                                            ),
                                        }
                                    });
                                }
                                Ok(None) => break,
                                Err(e) => {
                                    warn!(error = %e, "research runner: claim failed");
                                    break;
                                }
                            }
                        }
                    }
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            info!("research runner shutting down");
                            break;
                        }
                    }
                }
            }
        })
    }
}

/// Remove `<think>…</think>` (and `<thinking>…</thinking>`) blocks from a
/// reasoning model's output. Lazy match, multiline. Leaves the rest intact
/// and trims leading whitespace left behind after the strip.
///
/// Idempotent and safe on output that doesn't contain a think block.
pub fn strip_think_blocks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        let lower = rest.to_ascii_lowercase();
        let open = lower
            .find("<think>")
            .map(|i| (i, "<think>", "</think>"))
            .or_else(|| {
                lower
                    .find("<thinking>")
                    .map(|i| (i, "<thinking>", "</thinking>"))
            });
        let Some((start, open_tag, close_tag)) = open else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        let after_open = start + open_tag.len();
        let close_off = lower[after_open..].find(close_tag);
        match close_off {
            Some(rel_end) => {
                let abs_end = after_open + rel_end + close_tag.len();
                rest = &rest[abs_end..];
            }
            None => {
                // Unterminated <think> — drop the rest of the string.
                break;
            }
        }
    }
    out.trim_start().to_string()
}

#[cfg(test)]
mod think_strip_tests {
    use super::strip_think_blocks;

    #[test]
    fn strips_single_block() {
        let s = "<think>internal reasoning</think>final answer";
        assert_eq!(strip_think_blocks(s), "final answer");
    }

    #[test]
    fn strips_thinking_variant() {
        let s = "<thinking>x</thinking>answer";
        assert_eq!(strip_think_blocks(s), "answer");
    }

    #[test]
    fn handles_no_block() {
        assert_eq!(strip_think_blocks("clean output"), "clean output");
    }

    #[test]
    fn handles_multiple_blocks() {
        let s = "<think>a</think>mid<think>b</think>end";
        assert_eq!(strip_think_blocks(s), "midend");
    }

    #[test]
    fn handles_unterminated_block() {
        let s = "<think>never closes\nstill in think";
        assert_eq!(strip_think_blocks(s), "");
    }

    #[test]
    fn handles_uppercase_tags() {
        // Some models emit <THINK> uppercase.
        let s = "<THINK>x</THINK>answer";
        assert_eq!(strip_think_blocks(s), "answer");
    }
}

#[cfg(test)]
mod auto_recover_tests {
    use super::{AutoRecoverSummary, MAX_AUTO_RECOVER_ATTEMPTS};

    #[test]
    fn attempt_cap_is_bounded_and_positive() {
        // Must allow at least one retry (gateway blips) but never be unbounded —
        // a permanently-unsynthesizable session must stop burning LLM calls.
        assert!(MAX_AUTO_RECOVER_ATTEMPTS >= 1);
        assert!(MAX_AUTO_RECOVER_ATTEMPTS <= 5);
    }

    #[test]
    fn summary_defaults_to_zero() {
        let s = AutoRecoverSummary::default();
        assert_eq!((s.attempted, s.recovered, s.failed), (0, 0, 0));
    }
}

#[cfg(test)]
mod detach_tests {
    use super::{ResearchConfig, research_initial_status};

    #[test]
    fn detached_inserts_queued_foreground_planning() {
        // The runner ONLY claims `queued`; a foreground run starts `planning`.
        // If these ever diverge, detached sessions silently never run.
        assert_eq!(research_initial_status(true), "queued");
        assert_eq!(research_initial_status(false), "planning");
    }

    #[test]
    fn default_config_is_foreground() {
        // `--detach` is opt-in: every existing caller (and the default) must keep
        // running in the foreground so behavior is unchanged unless asked.
        assert!(!ResearchConfig::default().detached);
    }
}

#[cfg(test)]
mod recover_status_tests {
    use super::subtask_status_from_db;
    use crate::multi_agent::TaskStatus;

    #[test]
    fn maps_terminal_statuses_back_from_db() {
        // Must invert store_subtask_result's TaskStatus -> &str mapping so a
        // recovered run scores its sub-tasks the same way the live run did.
        assert!(matches!(
            subtask_status_from_db("done"),
            TaskStatus::Completed
        ));
        assert!(matches!(
            subtask_status_from_db("max_turns"),
            TaskStatus::MaxTurns
        ));
        assert!(matches!(
            subtask_status_from_db("cancelled"),
            TaskStatus::Cancelled
        ));
        assert!(matches!(
            subtask_status_from_db("failed"),
            TaskStatus::Failed
        ));
    }

    #[test]
    fn non_terminal_or_unknown_collapses_to_failed() {
        // A sub-task the reaper left mid-flight (or any future status string)
        // has no trustworthy output — treat it as failed, never as success,
        // so it can't inflate the done/failed rollup on recovery.
        for s in ["running", "pending", "dispatching", "", "weird"] {
            assert!(
                matches!(subtask_status_from_db(s), TaskStatus::Failed),
                "status {s:?} should map to Failed"
            );
        }
    }
}

#[cfg(test)]
mod backend_pick_tests {
    use super::{FleetBackend, select_distinct_round_robin};

    fn b(computer: &str, model: &str) -> FleetBackend {
        FleetBackend {
            computer_name: computer.into(),
            endpoint: format!("http://{computer}:55000"),
            model_id: model.into(),
        }
    }

    #[test]
    fn empty_or_zero_yields_nothing() {
        assert!(select_distinct_round_robin(vec![], 3).is_empty());
        assert!(select_distinct_round_robin(vec![b("a", "m")], 0).is_empty());
    }

    #[test]
    fn prefers_distinct_computers_first() {
        // Two deployments on `a` (first in best-first order) but `b`/`c` exist —
        // a 3-way fan-out must spread across all three boxes, not stack on `a`.
        let backends = vec![b("a", "m1"), b("a", "m2"), b("bb", "m1"), b("cc", "m1")];
        let got = select_distinct_round_robin(backends, 3);
        let computers: Vec<&str> = got.iter().map(|x| x.computer_name.as_str()).collect();
        assert_eq!(computers, vec!["a", "bb", "cc"]);
    }

    #[test]
    fn cycles_when_more_subagents_than_computers() {
        // 4 sub-agents, 2 distinct computers: take each distinct once, then
        // cycle the full list — every backend stays usable.
        let backends = vec![b("a", "m1"), b("bb", "m1")];
        let got = select_distinct_round_robin(backends, 4);
        assert_eq!(got.len(), 4);
        let computers: Vec<&str> = got.iter().map(|x| x.computer_name.as_str()).collect();
        assert_eq!(computers, vec!["a", "bb", "a", "bb"]);
    }

    #[test]
    fn preserves_best_first_order_within_distinct_pass() {
        // Input order is the router's tier-ASC/freshest-first ranking; the
        // distinct pass must not reorder it.
        let backends = vec![b("c1", "m"), b("c2", "m"), b("c3", "m")];
        let got = select_distinct_round_robin(backends, 2);
        let computers: Vec<&str> = got.iter().map(|x| x.computer_name.as_str()).collect();
        assert_eq!(computers, vec!["c1", "c2"]);
    }

    #[test]
    fn search_stagger_grows_per_index_and_caps() {
        use super::search_stagger;
        // First sub-agent searches immediately; each later one waits 350ms more.
        assert_eq!(search_stagger(0), std::time::Duration::from_millis(0));
        assert_eq!(search_stagger(1), std::time::Duration::from_millis(350));
        assert_eq!(search_stagger(3), std::time::Duration::from_millis(1050));
        // Cap so a large fan-out doesn't stall the whole run.
        assert_eq!(search_stagger(100), std::time::Duration::from_millis(4000));
    }
}

// ─── Progress events for callers ────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum ResearchProgress {
    Planning { query: String },
    Dispatching { sub_count: usize },
    Event(OrchestratorEvent),
    Synthesizing,
}

// ─── Helpers ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct FleetBackend {
    computer_name: String,
    endpoint: String,
    model_id: String,
}

/// Select `n` backends from a best-first-ordered list, preferring DISTINCT
/// computers first (one sub-agent per box → real parallelism), then cycling the
/// full list when more sub-agents than distinct computers were requested. The
/// input order (tier-ASC, freshest-first from `pg_route_deployments`) is
/// preserved within each pass. Pure — unit-tested.
/// Per-sub-agent delay before its web-grounding search, so a `--parallel N`
/// research run spreads its DuckDuckGo requests over time instead of firing
/// them all in the same instant (which trips DDG's 202 anomaly throttle and
/// drops sub-agents to ungrounded model memory). Sub-agent `i` waits `i*350ms`,
/// capped at 4s so a large fan-out doesn't stall the whole run.
fn search_stagger(index: usize) -> std::time::Duration {
    std::time::Duration::from_millis((index as u64 * 350).min(4000))
}

fn select_distinct_round_robin(backends: Vec<FleetBackend>, n: usize) -> Vec<FleetBackend> {
    if backends.is_empty() || n == 0 {
        return Vec::new();
    }
    let mut out: Vec<FleetBackend> = Vec::with_capacity(n);
    let mut seen_computer: std::collections::HashSet<String> = Default::default();
    for b in &backends {
        if out.len() >= n {
            break;
        }
        if seen_computer.insert(b.computer_name.clone()) {
            out.push(b.clone());
        }
    }
    // n > distinct computers: cycle the full list (a backend can host >1 sub-agent).
    let mut i = 0;
    while out.len() < n {
        out.push(backends[i % backends.len()].clone());
        i += 1;
    }
    out
}

#[derive(Debug, Clone)]
struct SubtaskRow {
    id: Uuid,
    ordinal: u32,
    sub_question: String,
    assigned_computer: String,
    _assigned_endpoint: String,
    assigned_model: String,
}

/// Inverse of [`ResearchSession::store_subtask_result`]'s status mapping:
/// turn a persisted `research_subtasks.status` string back into a
/// [`TaskStatus`] so `recover` can re-feed the synthesizer. Any non-terminal
/// or unknown status (`running`, `pending`, …) collapses to `Failed` — a
/// subtask that never reached a terminal state has no trustworthy output, and
/// `Failed` is exactly how the synthesizer flags "didn't run" output.
fn subtask_status_from_db(s: &str) -> TaskStatus {
    match s {
        "done" => TaskStatus::Completed,
        "max_turns" => TaskStatus::MaxTurns,
        "cancelled" => TaskStatus::Cancelled,
        _ => TaskStatus::Failed,
    }
}

async fn update_session_status(pool: &PgPool, session_id: Uuid, status: &str) -> Result<()> {
    sqlx::query("UPDATE research_sessions SET status = $1 WHERE id = $2")
        .bind(status)
        .bind(session_id)
        .execute(pool)
        .await
        .context("update session status")?;
    Ok(())
}

/// One-shot OpenAI-compatible completion call via the fleet gateway.
/// Uses the chat-completions endpoint with a single user message. Returns
/// the assistant's text content.
/// Extract `(prompt_tokens, completion_tokens)` from an OpenAI-compatible
/// chat-completion response's `usage` block; `(0, 0)` when absent. Pure.
///
/// `pub(crate)` so other in-crate dispatchers (e.g. `fleet_oneshot`) reuse the
/// one canonical usage parser instead of forking the JSON walk.
pub(crate) fn parse_completion_usage(v: &Value) -> (u64, u64) {
    let g = |k: &str| {
        v.get("usage")
            .and_then(|u| u.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    (g("prompt_tokens"), g("completion_tokens"))
}

/// Back-compat wrapper: returns just the completion text (drops usage).
pub async fn openai_single_completion(
    gateway_url: &str,
    model: &str,
    prompt: &str,
    max_tokens: u32,
    client: &reqwest::Client,
) -> Result<String> {
    openai_single_completion_with_usage(gateway_url, model, prompt, max_tokens, client)
        .await
        .map(|(text, _, _)| text)
}

/// Single chat completion that ALSO returns the server-reported token usage
/// `(text, prompt_tokens, completion_tokens)`. The usage feeds ff_interactions
/// (the training corpus) — research sub-agents previously logged 0 tokens
/// because the usage block was parsed-then-discarded here (a dead
/// `extract_token_counts` stub downstream).
pub async fn openai_single_completion_with_usage(
    gateway_url: &str,
    model: &str,
    prompt: &str,
    max_tokens: u32,
    client: &reqwest::Client,
) -> Result<(String, u64, u64)> {
    let url = ff_core::url::normalize_chat_completions_url(gateway_url);
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": max_tokens,
        "temperature": 0.2,
    });
    let resp = client
        .post(&url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(600))
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let txt = resp.text().await.unwrap_or_default();
        anyhow::bail!("{url}: HTTP {status}: {txt}");
    }
    let v: Value = resp
        .json()
        .await
        .with_context(|| format!("parse JSON from {url}"))?;
    // Some local reasoning-model servers (mlx_lm.server / vLLM with Qwen3
    // "thinking" builds) put the visible answer in `reasoning_content` (or
    // `reasoning`) and leave `content` empty when the response is truncated
    // by the token cap mid-thought. Try the OpenAI-standard `content` first,
    // then fall back to the reasoning fields so the planner/synthesizer still
    // gets usable text instead of erroring on empty content.
    let msg = v
        .pointer("/choices/0/message")
        .ok_or_else(|| anyhow::anyhow!("missing choices[0].message in {v}"))?;
    let pick = |key: &str| {
        msg.get(key)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let content = pick("content")
        .or_else(|| pick("reasoning_content"))
        .or_else(|| pick("reasoning"))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "missing/empty choices[0].message.content, .reasoning_content, \
                 and .reasoning in {v}"
            )
        })?;
    let (tin, tout) = parse_completion_usage(&v);
    Ok((content, tin, tout))
}

fn strip_json_fences(s: &str) -> &str {
    let trimmed = s.trim();
    let t = trimmed.strip_prefix("```json").unwrap_or(trimmed);
    let t = t.strip_prefix("```").unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t);
    t.trim()
}

/// Extract `[confidence] Claim URL` lines from an agent output's Findings
/// section. Very permissive — if the format isn't exact we just skip.
/// Returns (claim, confidence, url?).
fn parse_findings(output: &str) -> Vec<(String, Option<f64>, Option<String>)> {
    let mut out = Vec::new();
    let mut in_findings = false;
    for line in output.lines() {
        let t = line.trim();
        if t.starts_with("## Findings")
            || t.starts_with("### Findings")
            || t.eq_ignore_ascii_case("findings:")
        {
            in_findings = true;
            continue;
        }
        if !in_findings {
            continue;
        }
        if t.starts_with("##") || t.starts_with("# ") {
            // Next section — stop scanning.
            break;
        }
        if !t.starts_with('[') && !t.starts_with('-') && !t.starts_with('*') {
            continue;
        }
        // Strip leading list markers.
        let s = t.trim_start_matches(['-', '*']).trim();
        // Try to pull [0.xx] prefix.
        let (conf, rest) = if let Some(end) = s.find(']') {
            let inner = &s[1..end];
            let num = inner.trim().parse::<f64>().ok();
            (num, s[end + 1..].trim())
        } else {
            (None, s)
        };
        // URL: last whitespace-separated token that looks like http(s)://…
        let mut url: Option<String> = None;
        for tok in rest.split_whitespace().rev() {
            if tok.starts_with("http://") || tok.starts_with("https://") {
                url = Some(
                    tok.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '/')
                        .into(),
                );
                break;
            }
        }
        let claim = rest.to_string();
        if !claim.is_empty() {
            out.push((claim, conf, url));
        }
    }
    out
}

fn whoami_tag() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".into())
}

#[cfg(test)]
mod usage_tests {
    use super::parse_completion_usage;
    use serde_json::json;

    #[test]
    fn parses_openai_usage_block() {
        let v = json!({
            "choices": [{"message": {"content": "hi"}}],
            "usage": {"prompt_tokens": 123, "completion_tokens": 45, "total_tokens": 168}
        });
        assert_eq!(parse_completion_usage(&v), (123, 45));
    }

    #[test]
    fn degrades_to_zero_when_usage_absent_or_partial() {
        // No usage block at all (some servers omit it).
        assert_eq!(parse_completion_usage(&json!({"choices": []})), (0, 0));
        // Partial: only prompt_tokens present.
        assert_eq!(
            parse_completion_usage(&json!({"usage": {"prompt_tokens": 7}})),
            (7, 0)
        );
    }
}
