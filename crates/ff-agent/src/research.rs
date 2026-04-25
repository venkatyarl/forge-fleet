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

use crate::multi_agent::{
    AgentTask, AgentTaskResult, MultiAgentOrchestrator, OrchestratorEvent, TaskStatus,
};

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
    if let Ok(v) = std::env::var("FORGEFLEET_GATEWAY_URL") {
        if !v.is_empty() {
            return v;
        }
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
    if let Ok(host) = std::env::var("FORGEFLEET_LEADER_HOST") {
        if !host.is_empty() {
            return format!("http://{host}:{port}");
        }
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
        sqlx::query(
            "INSERT INTO research_sessions
                (id, query, status, depth, parallel, output_path, initiated_by,
                 planner_model, synth_model, started_at, metadata)
             VALUES ($1, $2, 'planning', $3, $4, $5, $6, $7, $7, NOW(),
                     jsonb_build_object('gateway_url', $8::text,
                                        'subagent_model', $9::text))",
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

    pub async fn run(
        &self,
        progress: Option<mpsc::UnboundedSender<ResearchProgress>>,
    ) -> Result<ResearchReport> {
        let start = Instant::now();
        let _ = progress.as_ref().map(|tx| {
            tx.send(ResearchProgress::Planning {
                query: self.config.query.clone(),
            })
        });

        // Phase 1 — planner decomposes the query.
        let plan = self.plan().await.context("planner phase")?;
        self.store_plan(&plan).await?;
        let _ = progress.as_ref().map(|tx| {
            tx.send(ResearchProgress::Dispatching {
                sub_count: plan.sub_questions.len(),
            })
        });

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

        let mut handles: Vec<tokio::task::JoinHandle<(Uuid, Result<(String, u64)>)>> =
            Vec::with_capacity(plan.sub_questions.len());
        for (i, ((q, row), backend)) in plan
            .sub_questions
            .iter()
            .zip(subtask_rows.iter())
            .zip(backends.iter())
            .enumerate()
        {
            let prompt = self.build_subagent_prompt(i, q, &plan.sub_questions);
            let endpoint = backend.endpoint.clone();
            let model = backend.model_id.clone();
            let row_id = row.id;
            handles.push(tokio::spawn(async move {
                let t0 = Instant::now();
                let out = openai_single_completion(&endpoint, &model, &prompt, 8192)
                    .await
                    .map(|s| (s, t0.elapsed().as_millis() as u64));
                (row_id, out)
            }));
        }

        // Collect; map each finished handle to AgentTaskResult shape so the
        // existing persist + synthesize code stays unchanged.
        let mut results: Vec<AgentTaskResult> = Vec::with_capacity(plan.sub_questions.len());
        for (row, handle) in subtask_rows.iter().zip(handles.into_iter()) {
            let (_row_id, res) = handle
                .await
                .unwrap_or_else(|e| (row.id, Err(anyhow::anyhow!("subagent panic: {e}"))));
            let (status, output, dur) = match res {
                Ok((text, dur)) => (TaskStatus::Completed, text, dur),
                Err(e) => {
                    warn!(subtask = %row.id, error = %e, "research sub-agent failed");
                    (TaskStatus::Failed, format!("(sub-agent error: {e})"), 0)
                }
            };
            results.push(AgentTaskResult {
                task_id: row.id.to_string(),
                status,
                output,
                events: Vec::new(),
                duration_ms: dur,
                turn_count: 1,
            });
        }

        if let Some(prog_tx) = progress.clone() {
            let _ = prog_tx.send(ResearchProgress::Synthesizing);
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
            let (tin, tout) = extract_token_counts(&result.events);
            total_tokens_in += tin;
            total_tokens_out += tout;
            self.store_subtask_result(row.id, result, tin, tout).await?;
        }

        // Phase 5 — synthesizer merges sub-agent outputs into a report.
        update_session_status(&self.pool, self.session_id, "synthesizing").await?;
        let _ = progress
            .as_ref()
            .map(|tx| tx.send(ResearchProgress::Synthesizing));

        let markdown = self
            .synthesize(&plan, &subtask_rows, &results)
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

    // ─── Planner turn ────────────────────────────────────────────────────

    async fn plan(&self) -> Result<PlanDecomposition> {
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

    async fn pick_distinct_backends(&self, n: usize) -> Result<Vec<FleetBackend>> {
        // Query active OpenAI-compatible deployments. Normalize endpoints —
        // the materializer records `http://127.0.0.1:<port>` (the loopback
        // view from each node's own forgefleetd) but we're dispatching
        // FROM Taylor so we need the LAN IP. We also join against
        // port_registry so the port in the endpoint string is always
        // authoritative (parses the raw endpoint's port, not hardcoded).
        let rows = sqlx::query(
            "SELECT DISTINCT ON (c.name, d.endpoint)
                    c.name, c.primary_ip, d.endpoint, d.model_id,
                    c.gpu_model
               FROM computer_model_deployments d
               JOIN computers c ON c.id = d.computer_id
              WHERE d.status = 'active'
                AND d.openai_compatible = true
              ORDER BY c.name, d.endpoint, d.started_at DESC NULLS LAST",
        )
        .fetch_all(&self.pool)
        .await
        .context("query active LLM deployments")?;

        // Fallback port if the deployment's endpoint string is malformed —
        // prefer the registry's `vllm` primary-slot port over a literal.
        let fallback_port = resolve_port(&self.pool, "vllm", 55000).await;

        if rows.is_empty() {
            anyhow::bail!(
                "no active OpenAI-compatible LLM deployments in the fleet — \
                 start at least one with `ff model load` or via vLLM docker"
            );
        }

        // Build normalized list: one entry per (computer, model) with the
        // LAN-reachable endpoint. Prefer GB10 boxes first.
        let mut seen: std::collections::HashSet<(String, String)> = Default::default();
        let mut backends: Vec<FleetBackend> = Vec::new();
        for r in &rows {
            let name: String = r.get("name");
            let primary_ip: String = r.get("primary_ip");
            let raw_endpoint: String = r.get("endpoint");
            let model_id: String = r.get("model_id");

            let endpoint =
                if raw_endpoint.contains("127.0.0.1") || raw_endpoint.contains("localhost") {
                    // Pull the port from the raw endpoint, rebuild with primary_ip.
                    // Fall back to the port registry's `vllm` entry if the
                    // endpoint string doesn't parse — never hardcode a literal.
                    let port = raw_endpoint
                        .rsplit(':')
                        .next()
                        .and_then(|p| p.trim_end_matches('/').parse::<u16>().ok())
                        .unwrap_or(fallback_port);
                    format!("http://{primary_ip}:{port}")
                } else {
                    raw_endpoint
                };

            if seen.insert((name.clone(), model_id.clone())) {
                backends.push(FleetBackend {
                    computer_name: name,
                    endpoint,
                    model_id,
                    is_gb10: r
                        .get::<Option<String>, _>("gpu_model")
                        .map(|g| g.contains("GB10"))
                        .unwrap_or(false),
                });
            }
        }

        // Sort: GB10 first, then alphabetical by computer name.
        backends.sort_by(|a, b| {
            b.is_gb10
                .cmp(&a.is_gb10)
                .then_with(|| a.computer_name.cmp(&b.computer_name))
        });

        // Round-robin across DISTINCT COMPUTERS first to maximize parallelism.
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
        // If n > distinct computers, cycle the full list.
        let mut i = 0;
        while out.len() < n {
            out.push(backends[i % backends.len()].clone());
            i += 1;
        }
        Ok(out)
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
                assigned_endpoint: b.endpoint.clone(),
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
             1. Use WebSearch and WebFetch liberally. Bias toward primary sources \
                (papers, repos, vendor docs) over blog posts.\n\
             2. Use Grep/Glob on the current workspace ({cwd}) when the \
                question touches code.\n\
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
            cwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
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
        openai_single_completion(
            &self.config.gateway_url,
            &self.config.planner_model, // reuse planner alias for synthesis
            &prompt,
            32768,
        )
        .await
        .context("synthesizer OpenAI call")
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
    is_gb10: bool,
}

#[derive(Debug, Clone)]
struct SubtaskRow {
    id: Uuid,
    ordinal: u32,
    sub_question: String,
    assigned_computer: String,
    assigned_endpoint: String,
    assigned_model: String,
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
async fn openai_single_completion(
    gateway_url: &str,
    model: &str,
    prompt: &str,
    max_tokens: u32,
) -> Result<String> {
    let url = format!("{}/v1/chat/completions", gateway_url.trim_end_matches('/'));
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": max_tokens,
        "temperature": 0.2,
    });
    let resp = reqwest::Client::new()
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
    let content = v
        .pointer("/choices/0/message/content")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing choices[0].message.content in {v}"))?;
    Ok(content.to_string())
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
        let s = t.trim_start_matches(|c: char| c == '-' || c == '*').trim();
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

/// Token counts per sub-agent are not yet surfaced through the AgentEvent
/// stream. Return zeros — the synthesizer doesn't depend on these numbers
/// and the overall session token count is captured at the HTTP layer
/// (planner + synthesizer call openai_single_completion, whose response
/// includes usage). Wire this up properly in a follow-up by extending
/// `AgentEvent::TurnComplete` with prompt/completion token fields.
fn extract_token_counts(_events: &[crate::agent_loop::AgentEvent]) -> (u64, u64) {
    (0, 0)
}

fn whoami_tag() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".into())
}
