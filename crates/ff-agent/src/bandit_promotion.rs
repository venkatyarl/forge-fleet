//! Autopilot-4 bandit A/B: daily promote/demote pass over `v_model_utilization`.
//!
//! [`crate::daemon`] wires [`run_bandit_promotion_tick`] into the tick
//! registry the same way [`crate::ha::periodic::run_nightly_digest_tick`] is
//! wired: leader-gated, clock-pinned to once per local day, deduped via a
//! deterministic per-date `telegram_messages` session id.
//!
//! The reward function is the GLM-vs-Devstral hand A/B (work_item
//! `8a3ec05e`) made mechanical: `ff-db`'s `pg_route_deployments` already
//! epsilon-greedy explores same-tier same-workload deployments (Autopilot-4's
//! routing half — see `ff_db::queries::apply_bandit_epsilon_greedy`), and
//! this pass reads the resulting build outcomes back from
//! `v_model_utilization` (migration V251) to decide whether the exploration
//! arm should become the new incumbent.
//!
//! Grouping key is `(tier, workload)`, not tier alone: two same-tier models
//! that serve different workloads never actually compete for routing
//! traffic (the router's bandit only explores within one requested
//! workload's same-tier pool), so pairing them for a promotion verdict would
//! judge a model against a competitor it never faces. `v_model_utilization`
//! unnests `fleet_model_catalog.preferred_workloads`, so a multi-workload
//! model contributes one reward row per workload it's tagged for.

use anyhow::Result;
use chrono::Timelike;
use sqlx::PgPool;

/// Local hour after which the daily bandit pass becomes due. Offset an hour
/// from the nightly digest's 08:00 ([`crate::ha::periodic::DIGEST_HOUR_LOCAL`])
/// so the two leader-gated daily passes don't contend for the same tick.
pub const BANDIT_PASS_HOUR_LOCAL: u32 = 9;

const BANDIT_SESSION_PREFIX: &str = "bandit-promotion";

/// Minimum samples a challenger needs before its `approve_pct` is trusted
/// enough to act on.
const MIN_BUILDS: i64 = 20;
/// Promote the challenger (tier - 1) once its `approve_pct` beats the
/// incumbent's by at least this many percentage points.
const PROMOTE_MARGIN_PTS: f64 = 10.0;
/// Demote the challenger (tier + 1) once its `approve_pct` trails the
/// incumbent's by at least this many percentage points.
const DEMOTE_MARGIN_PTS: f64 = 15.0;
/// Catalog tiers run 1 (cheapest/fastest SLM) .. 4 (largest/offload-only) —
/// see the `fleet_model_catalog` seed data. Promote/demote clamps to this
/// range so a runaway margin can never push a model outside the deployable
/// tier band.
const MIN_TIER: i32 = 1;
const MAX_TIER: i32 = 4;

/// One row of `v_model_utilization` (migration V251) — a model's build
/// outcomes for one `workload` it's tagged for, over the rolling 48h reward
/// window.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelUtilStats {
    pub catalog_id: String,
    pub catalog_name: String,
    pub tier: i32,
    /// One entry of `fleet_model_catalog.preferred_workloads` — the grouping
    /// key's second half. A multi-workload model appears as multiple rows,
    /// one per workload, since it only ever competes with other models
    /// within a single requested workload's routing pool.
    pub workload: String,
    pub builds: i64,
    pub approve_pct: f64,
}

/// Outcome of comparing one challenger against its tier+workload incumbent.
#[derive(Debug, Clone, PartialEq)]
pub enum BanditVerdict {
    Promote { catalog_id: String, new_tier: i32 },
    Demote { catalog_id: String, new_tier: i32 },
    Hold,
}

/// Pure promote/demote decision fn — unit-tested with fixture stats, no DB
/// required. A challenger needs `>= MIN_BUILDS` samples before its
/// `approve_pct` is trusted at all; below that it always holds regardless of
/// the margin. See module docs for the reward function's provenance.
pub fn decide_bandit_promotion(
    challenger: &ModelUtilStats,
    incumbent: &ModelUtilStats,
) -> BanditVerdict {
    if challenger.builds < MIN_BUILDS {
        return BanditVerdict::Hold;
    }
    if challenger.approve_pct >= incumbent.approve_pct + PROMOTE_MARGIN_PTS {
        return BanditVerdict::Promote {
            catalog_id: challenger.catalog_id.clone(),
            new_tier: (challenger.tier - 1).clamp(MIN_TIER, MAX_TIER),
        };
    }
    if challenger.approve_pct <= incumbent.approve_pct - DEMOTE_MARGIN_PTS {
        return BanditVerdict::Demote {
            catalog_id: challenger.catalog_id.clone(),
            new_tier: (challenger.tier + 1).clamp(MIN_TIER, MAX_TIER),
        };
    }
    BanditVerdict::Hold
}

/// A judged challenger: its stats, the incumbent it was compared against, and
/// the resulting verdict.
#[derive(Debug, Clone, PartialEq)]
pub struct BanditOutcome {
    pub challenger: ModelUtilStats,
    pub incumbent: ModelUtilStats,
    pub verdict: BanditVerdict,
}

/// Groups `stats` by `(tier, workload)` — NOT tier alone, so two models that
/// never actually compete for routing traffic (same tier, different
/// workload) never get judged against each other — treats the row with the
/// most builds in each group (ties broken by `catalog_id` for determinism) as
/// that group's incumbent, and judges every other row in the group against it
/// via [`decide_bandit_promotion`]. A group with fewer than 2 models present
/// has no A/B pool to judge and is skipped — mirroring
/// `apply_bandit_epsilon_greedy`'s "2+ same-tier (same-workload, implicitly,
/// via the single-workload query filter)" gate on the routing side. Pure —
/// unit-testable without a database.
pub fn compute_bandit_outcomes(stats: &[ModelUtilStats]) -> Vec<BanditOutcome> {
    let mut by_tier_workload: std::collections::BTreeMap<(i32, &str), Vec<&ModelUtilStats>> =
        std::collections::BTreeMap::new();
    for s in stats {
        by_tier_workload
            .entry((s.tier, s.workload.as_str()))
            .or_default()
            .push(s);
    }

    let mut outcomes = Vec::new();
    for group in by_tier_workload.into_values() {
        if group.len() < 2 {
            continue;
        }
        let mut group = group;
        group.sort_by(|a, b| {
            b.builds
                .cmp(&a.builds)
                .then_with(|| a.catalog_id.cmp(&b.catalog_id))
        });
        let incumbent = group[0].clone();
        for challenger in &group[1..] {
            let verdict = decide_bandit_promotion(challenger, &incumbent);
            outcomes.push(BanditOutcome {
                challenger: (*challenger).clone(),
                incumbent: incumbent.clone(),
                verdict,
            });
        }
    }
    outcomes
}

/// Render the Telegram report body — the numbers behind every promote/demote
/// (and, when nothing crossed the margin, an explicit no-op line so the
/// operator knows the pass ran). Pure so it unit-tests without a database.
pub fn format_bandit_report(outcomes: &[BanditOutcome]) -> String {
    let mut lines = Vec::new();
    for o in outcomes {
        let (verb, new_tier) = match &o.verdict {
            BanditVerdict::Promote { new_tier, .. } => ("PROMOTE", *new_tier),
            BanditVerdict::Demote { new_tier, .. } => ("DEMOTE", *new_tier),
            BanditVerdict::Hold => continue,
        };
        lines.push(format!(
            "{verb} {} [{}] (tier {} -> {new_tier}): {:.0}% approve over {} builds vs incumbent {} at {:.0}% over {} builds",
            o.challenger.catalog_name,
            o.challenger.workload,
            o.challenger.tier,
            o.challenger.approve_pct,
            o.challenger.builds,
            o.incumbent.catalog_name,
            o.incumbent.approve_pct,
            o.incumbent.builds,
        ));
    }
    if lines.is_empty() {
        "No promote/demote action — no challenger crossed the +10/-15pt margin (or lacked 20+ builds) yet.".to_string()
    } else {
        lines.join("\n")
    }
}

/// Deterministic session id for one calendar day's bandit pass. Doubles as
/// the fleet-wide "already ran today" marker in `telegram_messages`.
pub fn bandit_session_id(date: chrono::NaiveDate) -> String {
    format!("{BANDIT_SESSION_PREFIX}-{}", date.format("%Y-%m-%d"))
}

/// Is the daily bandit pass due at this local time? Due from
/// [`BANDIT_PASS_HOUR_LOCAL`]:00 until midnight, so a daemon that was down at
/// the top of the hour still catches up on its next tick the same day.
pub fn bandit_pass_due(now_local: chrono::NaiveTime) -> bool {
    now_local.hour() >= BANDIT_PASS_HOUR_LOCAL
}

/// Best-effort fetch of `v_model_utilization` — `None` when the view is
/// missing (fleet hasn't applied V251 yet) rather than failing the tick.
async fn fetch_model_utilization(pg: &PgPool) -> Option<Vec<ModelUtilStats>> {
    match sqlx::query_as::<_, (String, String, i32, String, i64, Option<f64>)>(
        "SELECT catalog_id, catalog_name, tier, workload, builds, approve_pct::float8 \
           FROM v_model_utilization",
    )
    .fetch_all(pg)
    .await
    {
        Ok(rows) => Some(
            rows.into_iter()
                .map(
                    |(catalog_id, catalog_name, tier, workload, builds, approve_pct)| {
                        ModelUtilStats {
                            catalog_id,
                            catalog_name,
                            tier,
                            workload,
                            builds,
                            approve_pct: approve_pct.unwrap_or(0.0),
                        }
                    },
                )
                .collect(),
        ),
        Err(e) => {
            tracing::debug!(error = %e, "bandit promotion: v_model_utilization unavailable");
            None
        }
    }
}

/// One scheduler pass of the daily bandit promote/demote check. Registered in
/// the daemon tick registry (leader-only), so by the time this runs the
/// caller has already established that this node is the live leader.
///
/// No-ops until [`BANDIT_PASS_HOUR_LOCAL`]:00 local time and after today's
/// pass has already run (dedup via the `telegram_messages` row the send
/// records). Tier updates are idempotent, so if Telegram isn't configured
/// (no dedup row gets written) a later tick the same day simply re-applies
/// the same decisions and retries the send.
pub async fn run_bandit_promotion_tick(pg: &PgPool, worker_name: &str) -> Result<()> {
    let now = chrono::Local::now();
    if !bandit_pass_due(now.time()) {
        return Ok(());
    }

    let session_id = bandit_session_id(now.date_naive());
    let already_ran: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM telegram_messages WHERE session_id = $1)")
            .bind(&session_id)
            .fetch_one(pg)
            .await?;
    if already_ran {
        return Ok(());
    }

    let Some(stats) = fetch_model_utilization(pg).await else {
        return Ok(());
    };
    let outcomes = compute_bandit_outcomes(&stats);
    if outcomes.iter().all(|o| o.verdict == BanditVerdict::Hold) {
        return Ok(());
    }

    for outcome in &outcomes {
        let (catalog_id, new_tier) = match &outcome.verdict {
            BanditVerdict::Promote {
                catalog_id,
                new_tier,
            }
            | BanditVerdict::Demote {
                catalog_id,
                new_tier,
            } => (catalog_id, *new_tier),
            BanditVerdict::Hold => continue,
        };
        sqlx::query("UPDATE fleet_model_catalog SET tier = $1, updated_at = NOW() WHERE id = $2")
            .bind(new_tier)
            .bind(catalog_id)
            .execute(pg)
            .await?;
    }

    let title = format!(
        "ForgeFleet bandit A/B — {}",
        now.date_naive().format("%Y-%m-%d")
    );
    let body = format_bandit_report(&outcomes);

    match crate::telegram::send_telegram_recorded(pg, &title, &body, &session_id).await? {
        Some(message_id) => {
            tracing::info!(
                leader = worker_name,
                session_id = %session_id,
                tg_message_id = message_id,
                "bandit promotion pass sent"
            );
        }
        None => {
            tracing::debug!("bandit promotion pass due but telegram not configured; skipping");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(
        catalog_id: &str,
        tier: i32,
        workload: &str,
        builds: i64,
        approve_pct: f64,
    ) -> ModelUtilStats {
        ModelUtilStats {
            catalog_id: catalog_id.to_string(),
            catalog_name: catalog_id.to_string(),
            tier,
            workload: workload.to_string(),
            builds,
            approve_pct,
        }
    }

    #[test]
    fn holds_below_min_builds_even_with_a_huge_margin() {
        let challenger = stats("glm-4.5-air", 2, "code", 19, 100.0);
        let incumbent = stats("devstral-small-2-24b", 2, "code", 50, 0.0);
        assert_eq!(
            decide_bandit_promotion(&challenger, &incumbent),
            BanditVerdict::Hold
        );
    }

    #[test]
    fn promotes_at_exactly_the_ten_point_margin_with_enough_builds() {
        let challenger = stats("glm-4.5-air", 2, "code", 20, 64.0);
        let incumbent = stats("devstral-small-2-24b", 2, "code", 50, 54.0);
        assert_eq!(
            decide_bandit_promotion(&challenger, &incumbent),
            BanditVerdict::Promote {
                catalog_id: "glm-4.5-air".to_string(),
                new_tier: 1,
            }
        );
    }

    #[test]
    fn demotes_at_exactly_the_fifteen_point_deficit_with_enough_builds() {
        let challenger = stats("glm-4.5-air", 2, "code", 20, 39.0);
        let incumbent = stats("devstral-small-2-24b", 2, "code", 50, 54.0);
        assert_eq!(
            decide_bandit_promotion(&challenger, &incumbent),
            BanditVerdict::Demote {
                catalog_id: "glm-4.5-air".to_string(),
                new_tier: 3,
            }
        );
    }

    #[test]
    fn holds_in_the_dead_zone_between_margins() {
        let challenger = stats("glm-4.5-air", 2, "code", 40, 58.0);
        let incumbent = stats("devstral-small-2-24b", 2, "code", 50, 54.0);
        assert_eq!(
            decide_bandit_promotion(&challenger, &incumbent),
            BanditVerdict::Hold
        );
    }

    #[test]
    fn promote_clamps_at_tier_one_floor() {
        let challenger = stats("qwen3-4b-instruct-2507", 1, "code", 30, 90.0);
        let incumbent = stats("gemma3-9b", 1, "code", 30, 50.0);
        assert_eq!(
            decide_bandit_promotion(&challenger, &incumbent),
            BanditVerdict::Promote {
                catalog_id: "qwen3-4b-instruct-2507".to_string(),
                new_tier: 1,
            }
        );
    }

    #[test]
    fn demote_clamps_at_tier_four_ceiling() {
        let challenger = stats("kimi-k3", 4, "code", 30, 10.0);
        let incumbent = stats("qwen3-coder-480b", 4, "code", 30, 90.0);
        assert_eq!(
            decide_bandit_promotion(&challenger, &incumbent),
            BanditVerdict::Demote {
                catalog_id: "kimi-k3".to_string(),
                new_tier: 4,
            }
        );
    }

    #[test]
    fn compute_outcomes_skips_tiers_with_a_lone_model() {
        let all = vec![stats("solo", 3, "code", 100, 99.0)];
        assert!(compute_bandit_outcomes(&all).is_empty());
    }

    #[test]
    fn compute_outcomes_picks_most_builds_as_incumbent_and_judges_the_rest() {
        let all = vec![
            stats("devstral-small-2-24b", 2, "code", 50, 54.0),
            stats("glm-4.5-air", 2, "code", 25, 70.0),
        ];
        let outcomes = compute_bandit_outcomes(&all);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].challenger.catalog_id, "glm-4.5-air");
        assert_eq!(outcomes[0].incumbent.catalog_id, "devstral-small-2-24b");
        assert_eq!(
            outcomes[0].verdict,
            BanditVerdict::Promote {
                catalog_id: "glm-4.5-air".to_string(),
                new_tier: 1,
            }
        );
    }

    /// The bug an in-place review caught (retry #4): same tier, DIFFERENT
    /// workload must never be pooled together. A tier-2 vision specialist
    /// with a terrible approve_pct must not drag down / get compared against
    /// an unrelated tier-2 code model — they never compete for the same
    /// routing traffic, so grouping by tier alone would produce a bogus
    /// promote/demote verdict.
    #[test]
    fn same_tier_different_workload_never_pooled_together() {
        let all = vec![
            stats("devstral-small-2-24b", 2, "code", 50, 54.0),
            stats("glm-4.5-air", 2, "code", 25, 70.0),
            stats("some-vision-model", 2, "vision", 40, 5.0),
        ];
        let outcomes = compute_bandit_outcomes(&all);
        // Only the code-workload pair is judged; the vision-only tier-2 model
        // has no same-workload peer and is skipped entirely.
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].challenger.catalog_id, "glm-4.5-air");
        assert_eq!(outcomes[0].incumbent.catalog_id, "devstral-small-2-24b");
    }

    /// A model tagged for multiple workloads (e.g. `["chat","code"]`)
    /// contributes one reward row per workload — its performance against the
    /// chat pool must not affect its verdict against the code pool.
    #[test]
    fn multi_workload_model_judged_independently_per_workload() {
        let all = vec![
            stats("qwen3.5-35b-a3b", 2, "code", 30, 80.0),
            stats("devstral-small-2-24b", 2, "code", 50, 54.0),
            stats("qwen3.5-35b-a3b", 2, "chat", 30, 20.0),
            stats("gemma-4-31b", 2, "chat", 50, 54.0),
        ];
        let outcomes = compute_bandit_outcomes(&all);
        assert_eq!(outcomes.len(), 2);
        let code_outcome = outcomes
            .iter()
            .find(|o| o.challenger.workload == "code")
            .expect("code outcome present");
        assert_eq!(
            code_outcome.verdict,
            BanditVerdict::Promote {
                catalog_id: "qwen3.5-35b-a3b".to_string(),
                new_tier: 1,
            }
        );
        let chat_outcome = outcomes
            .iter()
            .find(|o| o.challenger.workload == "chat")
            .expect("chat outcome present");
        assert_eq!(
            chat_outcome.verdict,
            BanditVerdict::Demote {
                catalog_id: "qwen3.5-35b-a3b".to_string(),
                new_tier: 3,
            }
        );
    }

    #[test]
    fn report_renders_promote_and_demote_lines_with_numbers() {
        let outcomes = vec![
            BanditOutcome {
                challenger: stats("glm-4.5-air", 2, "code", 25, 70.0),
                incumbent: stats("devstral-small-2-24b", 2, "code", 50, 54.0),
                verdict: BanditVerdict::Promote {
                    catalog_id: "glm-4.5-air".to_string(),
                    new_tier: 1,
                },
            },
            BanditOutcome {
                challenger: stats("mistral-small-3", 2, "code", 22, 30.0),
                incumbent: stats("devstral-small-2-24b", 2, "code", 50, 54.0),
                verdict: BanditVerdict::Demote {
                    catalog_id: "mistral-small-3".to_string(),
                    new_tier: 3,
                },
            },
        ];
        let body = format_bandit_report(&outcomes);
        assert!(
            body.contains("PROMOTE glm-4.5-air [code] (tier 2 -> 1): 70% approve over 25 builds")
        );
        assert!(
            body.contains(
                "DEMOTE mistral-small-3 [code] (tier 2 -> 3): 30% approve over 22 builds"
            )
        );
    }

    #[test]
    fn report_says_so_when_nothing_crossed_the_margin() {
        let outcomes = vec![BanditOutcome {
            challenger: stats("glm-4.5-air", 2, "code", 25, 58.0),
            incumbent: stats("devstral-small-2-24b", 2, "code", 50, 54.0),
            verdict: BanditVerdict::Hold,
        }];
        assert!(format_bandit_report(&outcomes).contains("No promote/demote action"));
    }

    #[test]
    fn bandit_pass_due_only_from_send_hour_onward() {
        let t = |h, m| chrono::NaiveTime::from_hms_opt(h, m, 0).unwrap();
        assert!(!bandit_pass_due(t(0, 0)));
        assert!(!bandit_pass_due(t(8, 59)));
        assert!(bandit_pass_due(t(9, 0)));
        assert!(bandit_pass_due(t(23, 59)));
    }

    #[test]
    fn session_id_is_stable_per_date() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 7, 24).unwrap();
        assert_eq!(bandit_session_id(date), "bandit-promotion-2026-07-24");
        assert_eq!(bandit_session_id(date), bandit_session_id(date));
    }
}
