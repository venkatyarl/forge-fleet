//! Memory-v2 M1 — the nightly **Dreamer** consolidation pass.
//!
//! Sibling of the error-miner: the miner turns failures into `work_items`;
//! the Dreamer turns EVERYTHING (errors, notable successes, completed/failed
//! work, scratchpad evictions) into durable knowledge. Once nightly it reads
//! the last 24h of episodic activity, distills it through a fleet reasoning
//! model into `{facts, rules, decays}` JSON, and writes:
//!   - **facts** → `brain_vault_nodes` (`node_type = 'distilled_fact'`,
//!     `provenance = 'dreamer'`), Mem0-style: a claim within
//!     [`FACT_DEDUP_SIMILARITY_THRESHOLD`] cosine similarity of an existing
//!     distilled fact is a NOOP (identical) or an UPDATE (supersedes the old
//!     node — `valid_until = NOW()` + `superseded_by`); otherwise it's an ADD.
//!     Each fact also gets an `about` edge to a matching subject node, when
//!     one exists.
//!   - **rules** → the Hive Mind's `learnings.json`, via the existing
//!     `ff_agent::learning::apply_entry` dedup-and-append path, confidence
//!     "medium" (`relevance = 0.5`).
//!   - **decays** → `valid_until = NOW()` on existing distilled-fact nodes the
//!     pass judges are no longer true (matched by [`decay_matches`]).
//!
//! ## Cadence
//! Clock-gated like the nightly Telegram digest ([`crate::ha`]-equivalent
//! pattern lives in `ff_agent::ha::periodic`): [`spawn_dreamer_loop`] wakes
//! every `interval_secs`, and [`run_nightly_dreamer_tick`] no-ops until
//! [`DREAMER_NIGHTLY_HOUR_LOCAL`]:00 local time and after today's pass has
//! already run (a `fleet_secrets` marker, not the Telegram send — a pass must
//! dedup even when Telegram isn't configured, unlike the digest). A pass that
//! errors leaves no marker, so the next due tick that day retries.
//!
//! This lives in `ff-brain` (not `ff-agent`) because `ff-agent` cannot depend
//! on `ff-brain` (the dependency runs the other way); the nightly tick is
//! wired into the daemon binary (`src/main.rs`) alongside this crate's other
//! leader-gated background loops (`spawn_reindex_loop`,
//! `spawn_embed_refresh_loop`, `spawn_summary_refresh_loop`), not the
//! `ff-agent`-internal `TickRegistry` (which only ever calls into `ff-agent`).

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Timelike;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

/// Local hour (0-23) the nightly pass becomes due. Due from this hour until
/// midnight (catch-up semantics identical to the nightly digest), and
/// deduped for the rest of the day by [`DREAMER_LAST_RUN_KEY`].
pub const DREAMER_NIGHTLY_HOUR_LOCAL: u32 = 2;

const DREAMER_SESSION_PREFIX: &str = "memory-dreamer-nightly";

/// `fleet_secrets` gate: `off`/`false`/`0`/`disabled`/`no` skips the pass body.
/// Distinct from `ff_agent::dreamer`'s `dreamer_mode` key — that's the
/// unrelated scratchpad session-archival dreamer.
const DREAMER_MODE_KEY: &str = "memory_dreamer_mode";
/// `fleet_secrets` marker recording the last calendar date (local) a pass
/// completed, so dedup does not depend on Telegram being configured.
const DREAMER_LAST_RUN_KEY: &str = "memory_dreamer_last_run_date";

const MAX_ERROR_INTERACTIONS: i64 = 60;
const MAX_SUCCESS_INTERACTIONS: i64 = 20;
const MAX_WORK_ITEMS: i64 = 30;
const MAX_EVICTIONS: i64 = 20;

/// Cap on facts written per night (task spec: 20 facts + 10 rules/night).
const MAX_FACTS_PER_NIGHT: usize = 20;
const MAX_RULES_PER_NIGHT: usize = 10;
const MAX_DECAYS_PER_NIGHT: usize = 20;

/// Mem0 two-phase dedup threshold: a candidate fact whose claim embeds within
/// this cosine similarity of an existing `distilled_fact` node is treated as
/// already-known (NOOP) or a supersession (UPDATE) rather than a fresh ADD.
const FACT_DEDUP_SIMILARITY_THRESHOLD: f32 = 0.92;

/// Research/thinking lane per Memory-v2 M1: local fleet models only, never
/// cloud. Tried in order; `fleet_oneshot`'s own candidate ranking already
/// widens the search when a hint has no exact match.
const PRIMARY_MODEL_HINT: &str = "deepseek-v3";
const FALLBACK_MODEL_HINT: &str = "qwen36-35b";
const DISTILL_TIMEOUT_SECS: u64 = 180;

/// Deterministic per-date session id — dedup marker for the Telegram summary
/// (parallels [`DREAMER_LAST_RUN_KEY`] but that's the pass-level dedup).
pub fn dreamer_nightly_session_id(date: chrono::NaiveDate) -> String {
    format!("{DREAMER_SESSION_PREFIX}-{}", date.format("%Y-%m-%d"))
}

/// Is the nightly pass due at this local time? Mirrors the nightly digest's
/// `digest_due`: due from [`DREAMER_NIGHTLY_HOUR_LOCAL`]:00 onward, so a
/// daemon that was down at 02:00 still catches up later the same day.
pub fn dreamer_nightly_due(now_local: chrono::NaiveTime) -> bool {
    now_local.hour() >= DREAMER_NIGHTLY_HOUR_LOCAL
}

/// `off`/`false`/`0`/`disabled`/`no` (case-insensitive) disable the pass;
/// anything else — including a missing secret — leaves it ON.
fn mode_is_off(v: Option<&str>) -> bool {
    matches!(
        v.map(|s| s.trim().to_lowercase()).as_deref(),
        Some("off" | "false" | "0" | "disabled" | "no")
    )
}

// ─── Episodic intake ────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct EpisodicIntake {
    /// (engine, request_text, error_text)
    errors: Vec<(String, String, Option<String>)>,
    /// (engine, request_text)
    successes: Vec<(String, String)>,
    /// (title, status, last_error)
    work_items: Vec<(String, String, Option<String>)>,
    /// (scope_type, scope_key, summary)
    evictions: Vec<(String, String, String)>,
}

impl EpisodicIntake {
    fn is_empty(&self) -> bool {
        self.errors.is_empty()
            && self.successes.is_empty()
            && self.work_items.is_empty()
            && self.evictions.is_empty()
    }
}

async fn fetch_error_interactions(pool: &PgPool) -> Result<Vec<(String, String, Option<String>)>> {
    sqlx::query_as(
        "SELECT COALESCE(engine, 'unknown'), request_text, error_text
           FROM ff_interactions
          WHERE outcome = 'error' AND ts >= NOW() - INTERVAL '24 hours'
          ORDER BY ts DESC
          LIMIT $1",
    )
    .bind(MAX_ERROR_INTERACTIONS)
    .fetch_all(pool)
    .await
    .context("fetch error interactions")
}

async fn fetch_notable_success_interactions(pool: &PgPool) -> Result<Vec<(String, String)>> {
    sqlx::query_as(
        "SELECT COALESCE(engine, 'unknown'), request_text
           FROM ff_interactions
          WHERE outcome = 'ok' AND ts >= NOW() - INTERVAL '24 hours'
          ORDER BY tokens_out DESC
          LIMIT $1",
    )
    .bind(MAX_SUCCESS_INTERACTIONS)
    .fetch_all(pool)
    .await
    .context("fetch notable success interactions")
}

async fn fetch_recent_work_items(pool: &PgPool) -> Result<Vec<(String, String, Option<String>)>> {
    // Status vocabulary is 'done'/'failed' (see pg_complete_parent_work_items),
    // not 'completed' — both statuses stamp completed_at.
    sqlx::query_as(
        "SELECT title, status, last_error
           FROM work_items
          WHERE status IN ('done', 'failed')
            AND completed_at >= NOW() - INTERVAL '24 hours'
          ORDER BY completed_at DESC
          LIMIT $1",
    )
    .bind(MAX_WORK_ITEMS)
    .fetch_all(pool)
    .await
    .context("fetch recent work items")
}

async fn fetch_recent_evictions(pool: &PgPool) -> Result<Vec<(String, String, String)>> {
    sqlx::query_as(
        "SELECT scope_type, scope_key, summary
           FROM agent_memory_evictions
          WHERE created_at >= NOW() - INTERVAL '24 hours'
          ORDER BY created_at DESC
          LIMIT $1",
    )
    .bind(MAX_EVICTIONS)
    .fetch_all(pool)
    .await
    .context("fetch recent memory evictions")
}

async fn gather_episodic_intake(pool: &PgPool) -> Result<EpisodicIntake> {
    Ok(EpisodicIntake {
        errors: fetch_error_interactions(pool).await?,
        successes: fetch_notable_success_interactions(pool).await?,
        work_items: fetch_recent_work_items(pool).await?,
        evictions: fetch_recent_evictions(pool).await?,
    })
}

/// Truncate to `max` chars on a char boundary, appending an ellipsis marker.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        format!("{}…", s.chars().take(max).collect::<String>())
    } else {
        s.to_string()
    }
}

/// Build the distillation prompt. Pure given the intake data (unit-tested
/// indirectly via [`parse_distilled_output`]; the prompt itself isn't
/// snapshot-tested since its exact wording is free to evolve).
fn build_distillation_prompt(intake: &EpisodicIntake) -> String {
    let errors_block = if intake.errors.is_empty() {
        "(none)".to_string()
    } else {
        intake
            .errors
            .iter()
            .map(|(engine, req, err)| {
                format!(
                    "- [{engine}] {} — error: {}",
                    truncate(req, 200),
                    truncate(err.as_deref().unwrap_or("(no message)"), 200)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let successes_block = if intake.successes.is_empty() {
        "(none)".to_string()
    } else {
        intake
            .successes
            .iter()
            .map(|(engine, req)| format!("- [{engine}] {}", truncate(req, 200)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let work_items_block = if intake.work_items.is_empty() {
        "(none)".to_string()
    } else {
        intake
            .work_items
            .iter()
            .map(|(title, status, last_error)| match last_error {
                Some(err) if !err.trim().is_empty() => {
                    format!("- [{status}] {title} — {}", truncate(err, 200))
                }
                _ => format!("- [{status}] {title}"),
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let evictions_block = if intake.evictions.is_empty() {
        "(none)".to_string()
    } else {
        intake
            .evictions
            .iter()
            .map(|(scope_type, scope_key, summary)| {
                format!("- [{scope_type}:{scope_key}] {}", truncate(summary, 200))
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        "You are the ForgeFleet memory Dreamer — a nightly consolidation pass that turns \
the last 24 hours of fleet activity into durable knowledge. Read the events below and \
extract ONLY what is genuinely reusable: stable facts about the system, and rules that \
should change future behavior. Do not restate one-off errors that carry no general lesson.

Respond with ONLY a single JSON object — no markdown, no prose, no code fence:
{{\"facts\": [{{\"subject\": \"...\", \"claim\": \"...\", \"evidence\": \"...\"}}], \
\"rules\": [{{\"when\": \"...\", \"do\": \"...\", \"why\": \"...\"}}], \
\"decays\": [\"<short description of an existing fact this activity proves is no longer true>\"]}}
Omit anything you have nothing to report for; empty arrays are fine.

=== Errors (last 24h) ===
{errors_block}

=== Notable successes (last 24h) ===
{successes_block}

=== Completed/failed work items (last 24h) ===
{work_items_block}

=== Scratchpad evictions (last 24h) ===
{evictions_block}
"
    )
}

// ─── Distillation JSON contract ────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
struct DistilledOutput {
    #[serde(default)]
    facts: Vec<DistilledFact>,
    #[serde(default)]
    rules: Vec<DistilledRule>,
    #[serde(default)]
    decays: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct DistilledFact {
    subject: String,
    claim: String,
    #[serde(default)]
    #[allow(dead_code)]
    evidence: String,
}

#[derive(Debug, Clone, Deserialize)]
struct DistilledRule {
    when: String,
    #[serde(rename = "do")]
    action: String,
    #[serde(default)]
    why: String,
}

/// Strip a leading `<think>…</think>` reasoning block and a surrounding code
/// fence, so the JSON extraction below only has to find balanced braces.
fn strip_reasoning_and_fences(raw: &str) -> String {
    let mut out = raw.to_string();
    if let Some(start) = out.find("<think>") {
        if let Some(rel_end) = out[start..].find("</think>") {
            let end = start + rel_end + "</think>".len();
            out.replace_range(start..end, "");
        }
    }
    let t = out.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t);
    t.trim().to_string()
}

/// Slice out the outermost `{...}` object from `s`, tolerating leading/
/// trailing prose a cooperative-but-chatty model adds despite instructions.
fn extract_json_object(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    if end < start {
        return None;
    }
    Some(&s[start..=end])
}

/// Parse the distiller's raw completion text into structured facts/rules/
/// decays. Unit-tested with think-blocks, code fences, and chatty preambles.
fn parse_distilled_output(raw: &str) -> Result<DistilledOutput> {
    let cleaned = strip_reasoning_and_fences(raw);
    let json_slice = extract_json_object(&cleaned)
        .ok_or_else(|| anyhow::anyhow!("no JSON object found in distiller output"))?;
    serde_json::from_str(json_slice).context("parse distilled JSON")
}

// ─── Fleet dispatch ─────────────────────────────────────────────────────────

async fn distill_via_fleet(
    pool: &PgPool,
    prompt: &str,
) -> Result<ff_agent::fleet_oneshot::FleetOneshot> {
    let timeout = Some(Duration::from_secs(DISTILL_TIMEOUT_SECS));
    match ff_agent::fleet_oneshot::fleet_oneshot(pool, prompt, Some(PRIMARY_MODEL_HINT), timeout)
        .await
    {
        Ok(r) => Ok(r),
        Err(primary_err) => {
            tracing::warn!(
                error = %primary_err,
                "memory dreamer: primary distill model failed; trying fallback"
            );
            ff_agent::fleet_oneshot::fleet_oneshot(pool, prompt, Some(FALLBACK_MODEL_HINT), timeout)
                .await
                .context("distill via fleet (primary and fallback both failed)")
        }
    }
}

// ─── Facts → brain_vault_nodes (Mem0-style ADD/UPDATE/NOOP) ────────────────

#[derive(Debug, Clone, Copy, Default)]
struct FactWriteStats {
    added: usize,
    updated: usize,
    noop: usize,
    failed: usize,
}

fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    format!("{:x}", hasher.finalize())
}

enum FactOutcome {
    Added,
    Updated,
    Noop,
}

/// Insert a `distilled_fact` node at a claim-derived deterministic path,
/// tolerating a same-night re-run (`ON CONFLICT (path) DO NOTHING`) by
/// falling back to the existing row's id.
async fn insert_fact_node(
    pool: &PgPool,
    path: &str,
    fact: &DistilledFact,
    content_hash: &str,
    embedding_str: &str,
) -> Result<Uuid> {
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO brain_vault_nodes
            (path, title, node_type, tags, confidence, content_hash, provenance, generation, embedding)
         VALUES ($1, $2, 'distilled_fact', $3, 0.7, $4, 'dreamer', EXTRACT(EPOCH FROM NOW())::BIGINT, $5::vector)
         ON CONFLICT (path) DO NOTHING
         RETURNING id",
    )
    .bind(path)
    .bind(&fact.claim)
    .bind(vec![fact.subject.clone()])
    .bind(content_hash)
    .bind(embedding_str)
    .fetch_optional(pool)
    .await
    .context("insert distilled fact node")?;

    match inserted {
        Some(id) => Ok(id),
        None => sqlx::query_scalar("SELECT id FROM brain_vault_nodes WHERE path = $1")
            .bind(path)
            .fetch_one(pool)
            .await
            .context("fetch existing fact node id after conflict"),
    }
}

/// Best-effort `about` edge from a fresh fact node to an existing node whose
/// title matches the fact's subject. Silently no-ops when no match exists —
/// subject linking is an enrichment, not a correctness requirement.
async fn link_subject_edge(pool: &PgPool, fact_node_id: Uuid, subject: &str) {
    let subject = subject.trim();
    if subject.is_empty() {
        return;
    }
    let target: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM brain_vault_nodes
          WHERE valid_until IS NULL AND title ILIKE $1
          ORDER BY references_ DESC
          LIMIT 1",
    )
    .bind(subject)
    .fetch_optional(pool)
    .await
    .unwrap_or(None);

    let Some(target) = target else { return };
    if target == fact_node_id {
        return;
    }
    let _ = sqlx::query(
        "INSERT INTO brain_vault_edges (src_id, dst_id, edge_type, confidence, provenance)
         VALUES ($1, $2, 'about', 1.0, 'dreamer')
         ON CONFLICT DO NOTHING",
    )
    .bind(fact_node_id)
    .bind(target)
    .execute(pool)
    .await;
}

async fn write_one_fact(pool: &PgPool, fact: &DistilledFact) -> Result<FactOutcome> {
    let claim = fact.claim.trim();
    let embedding = crate::embeddings::generate_embedding_with_pool(claim, pool).await;
    let embedding_str = crate::vector_search::embedding_to_pgvector(&embedding);
    let content_hash = sha256_hex(claim);
    let path = format!("dreamer/fact/{content_hash}");

    let nearest: Option<(Uuid, String, f64)> = sqlx::query_as(
        "SELECT id, title, (embedding <=> $1::vector)::float8
           FROM brain_vault_nodes
          WHERE valid_until IS NULL AND node_type = 'distilled_fact' AND embedding IS NOT NULL
          ORDER BY embedding <=> $1::vector
          LIMIT 1",
    )
    .bind(&embedding_str)
    .fetch_optional(pool)
    .await
    .context("nearest distilled_fact lookup")?;

    if let Some((existing_id, existing_title, distance)) = nearest {
        let similarity = 1.0 - distance as f32;
        if similarity >= FACT_DEDUP_SIMILARITY_THRESHOLD {
            if existing_title.trim() == claim {
                return Ok(FactOutcome::Noop);
            }
            let new_id = insert_fact_node(pool, &path, fact, &content_hash, &embedding_str).await?;
            if new_id != existing_id {
                sqlx::query(
                    "UPDATE brain_vault_nodes SET valid_until = NOW(), superseded_by = $1
                     WHERE id = $2 AND valid_until IS NULL",
                )
                .bind(new_id)
                .bind(existing_id)
                .execute(pool)
                .await
                .context("supersede old fact")?;
            }
            link_subject_edge(pool, new_id, &fact.subject).await;
            return Ok(FactOutcome::Updated);
        }
    }

    let new_id = insert_fact_node(pool, &path, fact, &content_hash, &embedding_str).await?;
    link_subject_edge(pool, new_id, &fact.subject).await;
    Ok(FactOutcome::Added)
}

async fn write_facts(pool: &PgPool, facts: &[DistilledFact]) -> FactWriteStats {
    let mut stats = FactWriteStats::default();
    for fact in facts.iter().take(MAX_FACTS_PER_NIGHT) {
        if fact.claim.trim().is_empty() {
            continue;
        }
        match write_one_fact(pool, fact).await {
            Ok(FactOutcome::Added) => stats.added += 1,
            Ok(FactOutcome::Updated) => stats.updated += 1,
            Ok(FactOutcome::Noop) => stats.noop += 1,
            Err(e) => {
                tracing::warn!(error = %e, claim = %fact.claim, "memory dreamer: write fact failed");
                stats.failed += 1;
            }
        }
    }
    stats
}

// ─── Rules → Hive Mind learnings.json ──────────────────────────────────────

async fn write_rules(rules: &[DistilledRule]) -> usize {
    if rules.is_empty() {
        return 0;
    }
    let hive = ff_agent::hive_sync::HiveSync::new();
    hive.ensure_initialized().await;
    let path = hive.local_path().join("learnings.json");

    let mut written = 0usize;
    for rule in rules.iter().take(MAX_RULES_PER_NIGHT) {
        let when = rule.when.trim();
        let action = rule.action.trim();
        if when.is_empty() || action.is_empty() {
            continue;
        }
        let why = rule.why.trim();
        let content = if why.is_empty() {
            format!("WHEN {when} THEN {action}")
        } else {
            format!("WHEN {when} THEN {action} — WHY {why}")
        };
        let entry = ff_agent::scoped_memory::MemoryEntry {
            id: Uuid::new_v4().to_string(),
            category: ff_agent::scoped_memory::MemoryCategory::Learning,
            content,
            // "medium" confidence per the Memory-v2 M1 spec.
            relevance: 0.5,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            source_session: None,
            tags: vec!["dreamer".to_string(), "rule".to_string()],
        };
        match ff_agent::learning::apply_entry(&path, &entry).await {
            Ok(()) => written += 1,
            Err(e) => tracing::warn!(error = %e, "memory dreamer: write hive rule failed"),
        }
    }
    if written > 0 {
        hive.auto_sync().await;
    }
    written
}

// ─── Decays ─────────────────────────────────────────────────────────────────

fn word_set(s: &str) -> HashSet<String> {
    s.split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|w| w.len() > 3)
        .collect()
}

/// Pure: does `decay_text` describe the same fact as `node_title`? Matches on
/// case-insensitive substring containment either direction, or on ≥0.5
/// Jaccard word overlap (mirrors `ff_agent::learning`'s tolerant hive-entry
/// dedup) so a rephrased decay still finds its target. Unit-tested.
fn decay_matches(decay_text: &str, node_title: &str) -> bool {
    let a = decay_text.trim().to_lowercase();
    let b = node_title.trim().to_lowercase();
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a.contains(&b) || b.contains(&a) {
        return true;
    }
    let wa = word_set(&a);
    let wb = word_set(&b);
    if wa.is_empty() || wb.is_empty() {
        return false;
    }
    let intersection = wa.intersection(&wb).count();
    let union = wa.union(&wb).count();
    (intersection as f64 / union as f64) >= 0.5
}

/// Pure: first candidate `(id, title)` whose title [`decay_matches`] the
/// given decay description. Unit-tested.
fn find_decay_target(decay_text: &str, candidates: &[(Uuid, String)]) -> Option<Uuid> {
    candidates
        .iter()
        .find(|(_, title)| decay_matches(decay_text, title))
        .map(|(id, _)| *id)
}

async fn apply_decays(pool: &PgPool, decays: &[String]) -> usize {
    if decays.is_empty() {
        return 0;
    }
    let candidates: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, title FROM brain_vault_nodes
          WHERE valid_until IS NULL AND node_type = 'distilled_fact'
          LIMIT 500",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let mut applied = 0usize;
    for decay in decays.iter().take(MAX_DECAYS_PER_NIGHT) {
        let Some(target_id) = find_decay_target(decay, &candidates) else {
            continue;
        };
        let result = sqlx::query(
            "UPDATE brain_vault_nodes SET valid_until = NOW() WHERE id = $1 AND valid_until IS NULL",
        )
        .bind(target_id)
        .execute(pool)
        .await;
        if matches!(result, Ok(r) if r.rows_affected() > 0) {
            applied += 1;
        }
    }
    applied
}

// ─── One pass, end to end ───────────────────────────────────────────────────

/// Report of one Dreamer pass — also the Telegram summary's data source.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DreamerReport {
    pub facts_added: usize,
    pub facts_updated: usize,
    pub facts_noop: usize,
    pub facts_failed: usize,
    pub rules_written: usize,
    pub decays_applied: usize,
    pub errors_seen: usize,
    pub successes_seen: usize,
    pub work_items_seen: usize,
    pub evictions_seen: usize,
    pub endpoint: String,
    pub model: String,
}

/// Run one Dreamer pass: gather the last 24h of episodic activity, distill it
/// via a fleet reasoning model, and write facts/rules/decays. Returns an
/// all-zero report (no fleet call made) when there is nothing to distill.
pub async fn run_dreamer_pass(pool: &PgPool) -> Result<DreamerReport> {
    let intake = gather_episodic_intake(pool).await?;
    let mut report = DreamerReport {
        errors_seen: intake.errors.len(),
        successes_seen: intake.successes.len(),
        work_items_seen: intake.work_items.len(),
        evictions_seen: intake.evictions.len(),
        ..Default::default()
    };
    if intake.is_empty() {
        return Ok(report);
    }

    let prompt = build_distillation_prompt(&intake);
    let oneshot = distill_via_fleet(pool, &prompt).await?;
    report.endpoint = oneshot.endpoint.clone();
    report.model = oneshot.model.clone();

    let distilled = parse_distilled_output(&oneshot.text)?;

    let fact_stats = write_facts(pool, &distilled.facts).await;
    report.facts_added = fact_stats.added;
    report.facts_updated = fact_stats.updated;
    report.facts_noop = fact_stats.noop;
    report.facts_failed = fact_stats.failed;

    report.rules_written = write_rules(&distilled.rules).await;
    report.decays_applied = apply_decays(pool, &distilled.decays).await;

    Ok(report)
}

/// One-line Telegram summary of a pass. Pure (unit-tested).
fn format_dreamer_report(report: &DreamerReport) -> String {
    format!(
        "Dreamer nightly pass: {} facts ({} added, {} updated, {} unchanged, {} failed), \
{} rules learned, {} facts decayed — distilled from {} errors, {} successes, \
{} work items, {} evictions.",
        report.facts_added + report.facts_updated,
        report.facts_added,
        report.facts_updated,
        report.facts_noop,
        report.facts_failed,
        report.rules_written,
        report.decays_applied,
        report.errors_seen,
        report.successes_seen,
        report.work_items_seen,
        report.evictions_seen,
    )
}

async fn read_secret(pool: &PgPool, key: &str) -> Option<String> {
    match ff_db::pg_get_secret(pool, key).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(key, error = %e, "memory dreamer: failed to read secret; using default");
            None
        }
    }
}

/// One scheduler pass: no-ops until due, dedups against the day's last-run
/// marker (survives Telegram being unconfigured), then runs the pass and
/// sends a one-line summary.
pub async fn run_nightly_dreamer_tick(pool: &PgPool, worker_name: &str) -> Result<()> {
    let now = chrono::Local::now();
    if !dreamer_nightly_due(now.time()) {
        return Ok(());
    }

    let today = now.date_naive().to_string();
    if read_secret(pool, DREAMER_LAST_RUN_KEY).await.as_deref() == Some(today.as_str()) {
        return Ok(());
    }
    if mode_is_off(read_secret(pool, DREAMER_MODE_KEY).await.as_deref()) {
        return Ok(());
    }

    let report = run_dreamer_pass(pool).await?;

    ff_db::pg_set_secret(pool, DREAMER_LAST_RUN_KEY, &today, None, Some(worker_name))
        .await
        .context("record dreamer last-run marker")?;

    tracing::info!(
        facts_added = report.facts_added,
        facts_updated = report.facts_updated,
        facts_noop = report.facts_noop,
        rules_written = report.rules_written,
        decays_applied = report.decays_applied,
        "memory dreamer: nightly pass complete"
    );

    let title = format!("ForgeFleet Dreamer — {today}");
    let body = format_dreamer_report(&report);
    let session_id = dreamer_nightly_session_id(now.date_naive());
    if let Err(e) =
        ff_agent::telegram::send_telegram_recorded(pool, &title, &body, &session_id).await
    {
        tracing::warn!(error = %e, "memory dreamer: telegram summary failed");
    }
    Ok(())
}

/// Spawn the leader-gated Dreamer background loop (mirrors
/// [`crate::community_summary::spawn_summary_refresh_loop`]'s leader-check +
/// interval shape). `interval_secs` is the due-check cadence, not the pass
/// interval — the pass itself only actually runs once per calendar day.
pub fn spawn_dreamer_loop(
    pg: PgPool,
    worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let is_leader: bool = sqlx::query_scalar(
                        r#"
                        SELECT EXISTS (
                            SELECT 1 FROM fleet_leader_state
                            WHERE member_name = $1
                              AND heartbeat_at > NOW() - INTERVAL '60 seconds'
                        )
                        "#,
                    )
                    .bind(&worker_name)
                    .fetch_one(&pg)
                    .await
                    .unwrap_or(false);

                    if !is_leader {
                        continue;
                    }

                    if let Err(e) = run_nightly_dreamer_tick(&pg, &worker_name).await {
                        tracing::warn!(error = %e, "memory dreamer: nightly tick failed");
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        tracing::info!("memory dreamer loop stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(h: u32, m: u32) -> chrono::NaiveTime {
        chrono::NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn nightly_due_only_from_hour_onward() {
        assert!(!dreamer_nightly_due(t(0, 0)));
        assert!(!dreamer_nightly_due(t(1, 59)));
        assert!(dreamer_nightly_due(t(2, 0)));
        assert!(dreamer_nightly_due(t(14, 30)));
        assert!(dreamer_nightly_due(t(23, 59)));
    }

    #[test]
    fn session_id_is_stable_per_date() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 7, 24).unwrap();
        assert_eq!(
            dreamer_nightly_session_id(date),
            "memory-dreamer-nightly-2026-07-24"
        );
        assert_eq!(
            dreamer_nightly_session_id(date),
            dreamer_nightly_session_id(date)
        );
    }

    #[test]
    fn mode_defaults_on_unless_explicit_off() {
        for v in ["off", "OFF", " Off ", "false", "0", "disabled", "no"] {
            assert!(mode_is_off(Some(v)), "{v} should disable");
        }
        for v in [None, Some(""), Some("on"), Some("auto"), Some("1800")] {
            assert!(!mode_is_off(v), "{v:?} should stay on");
        }
    }

    // ── JSON parse ──────────────────────────────────────────────────────

    #[test]
    fn parses_clean_json() {
        let raw =
            r#"{"facts":[{"subject":"s","claim":"c","evidence":"e"}],"rules":[],"decays":[]}"#;
        let out = parse_distilled_output(raw).unwrap();
        assert_eq!(out.facts.len(), 1);
        assert_eq!(out.facts[0].subject, "s");
        assert!(out.rules.is_empty());
        assert!(out.decays.is_empty());
    }

    #[test]
    fn parses_json_wrapped_in_code_fence() {
        let raw = "```json\n{\"facts\":[],\"rules\":[{\"when\":\"w\",\"do\":\"d\",\"why\":\"y\"}],\"decays\":[]}\n```";
        let out = parse_distilled_output(raw).unwrap();
        assert_eq!(out.rules.len(), 1);
        assert_eq!(out.rules[0].when, "w");
        assert_eq!(out.rules[0].action, "d");
        assert_eq!(out.rules[0].why, "y");
    }

    #[test]
    fn parses_json_after_think_block() {
        let raw = "<think>let me consider the events</think>{\"facts\":[],\"rules\":[],\"decays\":[\"old fact\"]}";
        let out = parse_distilled_output(raw).unwrap();
        assert_eq!(out.decays, vec!["old fact".to_string()]);
    }

    #[test]
    fn parses_json_with_chatty_preamble_and_trailing_text() {
        let raw = "Sure, here is the analysis:\n{\"facts\":[],\"rules\":[],\"decays\":[]}\nHope that helps!";
        let out = parse_distilled_output(raw).unwrap();
        assert!(out.facts.is_empty());
    }

    #[test]
    fn missing_arrays_default_to_empty() {
        let out = parse_distilled_output("{}").unwrap();
        assert!(out.facts.is_empty() && out.rules.is_empty() && out.decays.is_empty());
    }

    #[test]
    fn no_json_object_is_an_error() {
        assert!(parse_distilled_output("no json here at all").is_err());
    }

    // ── Decay matcher ───────────────────────────────────────────────────

    #[test]
    fn decay_matches_exact_substring_either_direction() {
        assert!(decay_matches(
            "the auth middleware stores tokens in headers",
            "auth middleware stores tokens in headers"
        ));
        assert!(decay_matches(
            "auth middleware stores tokens in headers",
            "the auth middleware stores tokens in headers now deprecated"
        ));
    }

    #[test]
    fn decay_matches_via_word_overlap() {
        assert!(decay_matches(
            "kimi is first in the builder rotation",
            "builder rotation puts kimi first before codex"
        ));
    }

    #[test]
    fn decay_does_not_match_unrelated_text() {
        assert!(!decay_matches(
            "the deploy pipeline uses blue-green releases",
            "auth middleware stores tokens in headers"
        ));
    }

    #[test]
    fn decay_matches_is_case_insensitive() {
        assert!(decay_matches(
            "AUTH MIDDLEWARE IS DEPRECATED",
            "auth middleware is deprecated"
        ));
    }

    #[test]
    fn find_decay_target_returns_first_match() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let candidates = vec![
            (a, "unrelated topic entirely".to_string()),
            (b, "auth middleware stores tokens in headers".to_string()),
        ];
        assert_eq!(
            find_decay_target("auth middleware stores tokens in headers", &candidates),
            Some(b)
        );
        assert_eq!(
            find_decay_target("nothing like either candidate", &candidates),
            None
        );
    }

    // ── Report formatting ───────────────────────────────────────────────

    #[test]
    fn report_format_includes_all_counts() {
        let report = DreamerReport {
            facts_added: 3,
            facts_updated: 1,
            facts_noop: 2,
            facts_failed: 0,
            rules_written: 4,
            decays_applied: 1,
            errors_seen: 10,
            successes_seen: 5,
            work_items_seen: 6,
            evictions_seen: 2,
            endpoint: String::new(),
            model: String::new(),
        };
        let body = format_dreamer_report(&report);
        assert!(body.contains("4 facts (3 added, 1 updated, 2 unchanged, 0 failed)"));
        assert!(body.contains("4 rules learned"));
        assert!(body.contains("1 facts decayed"));
        assert!(body.contains("10 errors, 5 successes, 6 work items, 2 evictions"));
    }

    #[test]
    fn truncate_appends_ellipsis_only_when_over_limit() {
        assert_eq!(truncate("short", 10), "short");
        let long = "a".repeat(20);
        let out = truncate(&long, 5);
        assert_eq!(out.chars().count(), 6); // 5 chars + ellipsis marker
    }
}
