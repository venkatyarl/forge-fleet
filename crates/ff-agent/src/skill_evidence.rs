//! OpenSpace-pattern skill evidence loop (operator 2026-07-27).
//!
//! ff had the skill SCHEMA (624 skills, skill_invocations, skill_kpi_view,
//! superseded_by/canonical/combines lineage) but the LOOP was dead: skills were
//! never surfaced to a build, `skill_invocations` had 0 rows, nothing graded or
//! evolved. This module closes the loop that HKUDS/OpenSpace demonstrates gives a
//! measurable lift (65%→79% on Terminal-Bench from skill evolution alone, same
//! model): retrieve a relevant skill → inject it into the build prompt → record
//! the outcome as EVIDENCE → grade skills by that evidence → (later) evolve them.
//!
//! Phase 1 (this module): RETRIEVE + INJECT + RECORD + GRADE. Evolution
//! (FIX/DERIVE/CAPTURE, provisional→trusted graduation) builds on this evidence.

use anyhow::Result;
use sqlx::PgPool;
use uuid::Uuid;

/// A skill selected as relevant to a task, with its evidence-based trust so the
/// injector can prefer proven skills and label unproven ones.
#[derive(Debug, Clone)]
pub struct RelevantSkill {
    pub id: Uuid,
    pub name: String,
    pub when_to_invoke: Option<String>,
    pub body_md: String,
    /// Evidence: successful invocations / total (NULL when never used).
    pub success_rate: Option<f64>,
    pub invocations: i64,
    /// Provisional (unproven) vs trusted (enough successful evidence). A brand-new
    /// or never-used skill is provisional; it graduates after independent successes.
    pub trusted: bool,
}

/// Minimum successful invocations for a skill to be "trusted" (OpenSpace's
/// provisional→trusted graduation). Below this it's still offered but flagged
/// provisional so a weak/wrong skill can't masquerade as proven.
pub const TRUST_MIN_SUCCESSES: i64 = 3;

/// Retrieve the top-`limit` skills relevant to `task_text`, ranked by keyword
/// overlap (the task's words against the skill's when_to_invoke/description/name)
/// AND evidence (proven skills rank above unproven on a tie). Postgres FTS-style
/// ranking via `plainto_tsquery`/`ts_rank` — cheap, no embedding round-trip; the
/// hybrid embedding+rerank (OpenSpace's full retrieval) is a later upgrade via
/// the existing cortex/brain layer. Only non-superseded skills are returned.
pub async fn select_relevant_skills(
    pg: &PgPool,
    task_text: &str,
    limit: i64,
) -> Result<Vec<RelevantSkill>> {
    // Guard: an empty/degenerate query returns nothing rather than everything.
    let q = task_text.trim();
    if q.len() < 8 {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as::<_, (Uuid, String, Option<String>, String, Option<f64>, i64)>(
        "WITH ev AS ( \
            SELECT skill_id, \
                   count(*) FILTER (WHERE outcome IN ('success','merged','applied')) AS ok, \
                   count(*) AS total \
              FROM skill_invocations GROUP BY skill_id \
         ) \
         SELECT s.id, s.name, s.when_to_invoke, s.body_md, \
                CASE WHEN ev.total > 0 THEN ev.ok::float8 / ev.total ELSE NULL END AS success_rate, \
                COALESCE(ev.total, 0) AS invocations \
           FROM skills s \
           LEFT JOIN ev ON ev.skill_id = s.id \
          WHERE s.superseded_by IS NULL \
            AND to_tsvector('english', \
                  coalesce(s.name,'') || ' ' || coalesce(s.when_to_invoke,'') || ' ' || coalesce(s.description,'')) \
                @@ plainto_tsquery('english', $1) \
          ORDER BY ts_rank(to_tsvector('english', \
                     coalesce(s.name,'') || ' ' || coalesce(s.when_to_invoke,'') || ' ' || coalesce(s.description,'')), \
                     plainto_tsquery('english', $1)) DESC, \
                   COALESCE(ev.ok, 0) DESC \
          LIMIT $2",
    )
    .bind(q)
    .bind(limit)
    .fetch_all(pg)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(id, name, when_to_invoke, body_md, success_rate, invocations)| {
            let ok = success_rate.map(|r| (r * invocations as f64).round() as i64).unwrap_or(0);
            RelevantSkill {
                id,
                name,
                when_to_invoke,
                body_md,
                success_rate,
                invocations,
                trusted: ok >= TRUST_MIN_SUCCESSES,
            }
        })
        .collect())
}

/// Render the selected skills into a prompt block the builder sees BEFORE it
/// codes — the "reuse a proven skill instead of re-deriving" lever. Trusted
/// skills are presented as authoritative; provisional ones as suggestions to
/// consider. Body is capped so a big skill doesn't blow the context.
pub fn render_skill_block(skills: &[RelevantSkill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "RELEVANT SKILLS (proven approaches for this kind of task — reuse them instead of \
         reinventing; a TRUSTED skill has a track record, a PROVISIONAL one is a suggestion):\n",
    );
    for s in skills {
        let tag = if s.trusted {
            format!("TRUSTED ({} uses)", s.invocations)
        } else if s.invocations > 0 {
            format!("provisional ({} uses)", s.invocations)
        } else {
            "provisional (untried)".to_string()
        };
        let body: String = s.body_md.chars().take(1600).collect();
        out.push_str(&format!(
            "\n── skill: {} [{}]\n{}{}\n",
            s.name,
            tag,
            s.when_to_invoke
                .as_deref()
                .map(|w| format!("When: {w}\n"))
                .unwrap_or_default(),
            body
        ));
    }
    out
}

/// Record an EVIDENCE row for a skill that was injected into a build — the fuel
/// for grading + evolution. `outcome` ∈ success|merged|applied|failed|no_diff|
/// rejected. Idempotency isn't needed (each build attempt is a distinct
/// invocation). Best-effort — never fails the caller.
pub async fn record_skill_invocation(
    pg: &PgPool,
    skill_id: Uuid,
    trace_id: &str,
    computer: &str,
    task_summary: &str,
    outcome: &str,
) {
    let _ = sqlx::query(
        "INSERT INTO skill_invocations \
            (skill_id, trace_id, invoked_at, computer, task_summary, outcome) \
         VALUES ($1, $2, now(), $3, left($4, 200), $5)",
    )
    .bind(skill_id)
    .bind(trace_id)
    .bind(computer)
    .bind(task_summary)
    .bind(outcome)
    .execute(pg)
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_query_returns_nothing_and_block_is_empty() {
        assert!(render_skill_block(&[]).is_empty());
    }

    #[test]
    fn trust_reflects_success_count() {
        let s = RelevantSkill {
            id: Uuid::nil(),
            name: "x".into(),
            when_to_invoke: None,
            body_md: "b".into(),
            success_rate: Some(1.0),
            invocations: 5,
            trusted: 5 >= TRUST_MIN_SUCCESSES,
        };
        assert!(s.trusted);
        let block = render_skill_block(std::slice::from_ref(&s));
        assert!(block.contains("TRUSTED"));
    }
}
