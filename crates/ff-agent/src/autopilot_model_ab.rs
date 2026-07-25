//! Autopilot model A/B reconciler.
//!
//! Routing explores same-tier, same-workload deployments in `ff-db`; this
//! leader-only pass consumes the rolling reward view and adjusts catalog tiers
//! when a challenger has enough build evidence.

use std::collections::{BTreeMap, HashSet};

use anyhow::Result;
use ff_db::{ModelAbRewardStat, ModelAbTierAction, ModelAbTierDecision};
use sqlx::PgPool;
use tracing::{info, warn};

const MIN_CHALLENGER_BUILDS: i64 = 20;
const PROMOTE_MARGIN_PCT: f64 = 10.0;
const DEMOTE_MARGIN_PCT: f64 = 15.0;

pub async fn reconcile_model_ab_once(pool: &PgPool) -> Result<Vec<ModelAbTierDecision>> {
    let stats = ff_db::pg_model_ab_reward_stats(pool).await?;
    let decisions = decide_model_ab_tier_changes(&stats);

    for decision in &decisions {
        ff_db::pg_apply_model_ab_tier_decision(pool, decision).await?;
        info!(
            model_id = %decision.model_id,
            workload = %decision.workload,
            action = decision.action.as_str(),
            old_tier = decision.old_tier,
            new_tier = decision.new_tier,
            builds = decision.builds,
            approve_pct = decision.approve_pct,
            incumbent_model_id = %decision.incumbent_model_id,
            incumbent_builds = decision.incumbent_builds,
            incumbent_approve_pct = decision.incumbent_approve_pct,
            "autopilot model A/B tier decision applied"
        );
    }

    if !decisions.is_empty() {
        let body = render_model_ab_report(&decisions);
        if let Err(error) =
            crate::telegram::send_telegram_from_secrets(pool, "Autopilot model A/B", &body).await
        {
            warn!(%error, "autopilot model A/B telegram report failed");
        }
    }

    Ok(decisions)
}

pub fn decide_model_ab_tier_changes(stats: &[ModelAbRewardStat]) -> Vec<ModelAbTierDecision> {
    let mut groups: BTreeMap<(String, i32), Vec<&ModelAbRewardStat>> = BTreeMap::new();
    for stat in stats {
        if stat.approve_pct.is_some() {
            groups
                .entry((stat.workload.clone(), stat.tier))
                .or_default()
                .push(stat);
        }
    }

    let mut decisions = Vec::new();
    let mut decided_models = HashSet::new();
    for ((workload, tier), mut group) in groups {
        if group.len() < 2 {
            continue;
        }
        group.sort_by(|a, b| {
            b.builds
                .cmp(&a.builds)
                .then_with(|| a.model_id.cmp(&b.model_id))
        });
        let incumbent = group[0];
        let Some(incumbent_approve_pct) = incumbent.approve_pct else {
            continue;
        };

        for challenger in group.into_iter().skip(1) {
            if !decided_models.insert(challenger.model_id.clone()) {
                continue;
            }
            let Some(challenger_approve_pct) = challenger.approve_pct else {
                continue;
            };
            if challenger.builds < MIN_CHALLENGER_BUILDS {
                continue;
            }

            let delta = challenger_approve_pct - incumbent_approve_pct;
            let (action, new_tier) = if delta >= PROMOTE_MARGIN_PCT && tier > 1 {
                (ModelAbTierAction::Promote, tier - 1)
            } else if delta <= -DEMOTE_MARGIN_PCT {
                (ModelAbTierAction::Demote, tier + 1)
            } else {
                continue;
            };

            decisions.push(ModelAbTierDecision {
                model_id: challenger.model_id.clone(),
                workload: workload.clone(),
                action,
                old_tier: tier,
                new_tier,
                builds: challenger.builds,
                approve_pct: challenger_approve_pct,
                incumbent_model_id: incumbent.model_id.clone(),
                incumbent_builds: incumbent.builds,
                incumbent_approve_pct,
            });
        }
    }
    decisions
}

fn render_model_ab_report(decisions: &[ModelAbTierDecision]) -> String {
    decisions
        .iter()
        .map(|d| {
            format!(
                "{} {}: {} tier {} -> {} ({} builds, {:.1}% approve) vs {} ({} builds, {:.1}% approve)",
                d.workload,
                d.action.as_str(),
                d.model_id,
                d.old_tier,
                d.new_tier,
                d.builds,
                d.approve_pct,
                d.incumbent_model_id,
                d.incumbent_builds,
                d.incumbent_approve_pct
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(model_id: &str, workload: &str, tier: i32, builds: i64, pct: f64) -> ModelAbRewardStat {
        ModelAbRewardStat {
            model_id: model_id.to_string(),
            workload: workload.to_string(),
            tier,
            builds,
            approve_pct: Some(pct),
        }
    }

    #[test]
    fn promotes_challenger_after_twenty_builds_and_ten_point_win() {
        let decisions = decide_model_ab_tier_changes(&[
            stat("devstral", "code-gen", 2, 44, 62.0),
            stat("glm-4.5-air", "code-gen", 2, 23, 72.0),
        ]);

        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].action, ModelAbTierAction::Promote);
        assert_eq!(decisions[0].model_id, "glm-4.5-air");
        assert_eq!(decisions[0].old_tier, 2);
        assert_eq!(decisions[0].new_tier, 1);
    }

    #[test]
    fn demotes_challenger_after_twenty_builds_and_fifteen_point_loss() {
        let decisions = decide_model_ab_tier_changes(&[
            stat("devstral", "code-gen", 2, 40, 70.0),
            stat("glm-4.5-air", "code-gen", 2, 20, 55.0),
        ]);

        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].action, ModelAbTierAction::Demote);
        assert_eq!(decisions[0].model_id, "glm-4.5-air");
        assert_eq!(decisions[0].old_tier, 2);
        assert_eq!(decisions[0].new_tier, 3);
    }

    #[test]
    fn waits_for_twenty_challenger_builds() {
        let decisions = decide_model_ab_tier_changes(&[
            stat("devstral", "code-gen", 2, 40, 60.0),
            stat("glm-4.5-air", "code-gen", 2, 19, 95.0),
        ]);

        assert!(decisions.is_empty());
    }
}
