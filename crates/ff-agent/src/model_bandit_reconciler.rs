//! Daily Autopilot-4 reward evaluation and tier adjustment.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use ff_db::queries::{ModelCatalogRow, ModelRewardStats};

const MIN_BUILDS: i64 = 20;
const PROMOTE_MARGIN: f64 = 10.0;
const DEMOTE_MARGIN: f64 = 15.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BanditDecision {
    Promote,
    Demote,
    Hold,
}

pub fn decide_promotion(
    incumbent: &ModelRewardStats,
    challenger: &ModelRewardStats,
) -> BanditDecision {
    if challenger.builds < MIN_BUILDS {
        return BanditDecision::Hold;
    }
    let (Some(incumbent_pct), Some(challenger_pct)) =
        (incumbent.approve_pct, challenger.approve_pct)
    else {
        return BanditDecision::Hold;
    };
    if challenger_pct >= incumbent_pct + PROMOTE_MARGIN {
        BanditDecision::Promote
    } else if challenger_pct <= incumbent_pct - DEMOTE_MARGIN {
        BanditDecision::Demote
    } else {
        BanditDecision::Hold
    }
}

#[derive(Debug)]
struct TierRow {
    id: String,
    tier: i32,
    workloads: BTreeSet<String>,
}

fn tier_rows(catalog: &[ModelCatalogRow], healthy: &BTreeSet<String>) -> Vec<TierRow> {
    catalog
        .iter()
        .filter(|row| healthy.contains(&row.id))
        .map(|row| TierRow {
            id: row.id.clone(),
            tier: row.tier,
            workloads: row
                .preferred_workloads
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect(),
        })
        .collect()
}

fn sibling_groups(rows: &[TierRow]) -> Vec<Vec<&TierRow>> {
    let mut groups: Vec<Vec<&TierRow>> = Vec::new();
    'next: for row in rows {
        if row.workloads.is_empty() {
            continue;
        }
        for group in &mut groups {
            if group.iter().any(|member| {
                member.tier == row.tier && !member.workloads.is_disjoint(&row.workloads)
            }) {
                group.push(row);
                continue 'next;
            }
        }
        groups.push(vec![row]);
    }
    groups.retain(|group| group.len() >= 2);
    groups
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BanditAction {
    pub challenger_id: String,
    pub incumbent_id: String,
    pub decision: BanditDecision,
    pub challenger_builds: i64,
    pub challenger_approve_pct: Option<f64>,
    pub incumbent_builds: i64,
    pub incumbent_approve_pct: Option<f64>,
    pub old_tier: i32,
    pub new_tier: i32,
}

#[derive(Debug, Default)]
pub struct BanditReport {
    pub groups_evaluated: usize,
    pub actions: Vec<BanditAction>,
}

pub struct ModelBanditReconciler {
    pool: PgPool,
}

impl ModelBanditReconciler {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn daily_pass(&self) -> Result<BanditReport> {
        let healthy: BTreeSet<String> = sqlx::query_scalar(
            "SELECT DISTINCT catalog_id FROM fleet_model_deployments \
              WHERE health_status = 'healthy' AND catalog_id IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .collect();
        let catalog = ff_db::queries::pg_list_catalog(&self.pool).await?;
        let rewards: BTreeMap<String, ModelRewardStats> =
            ff_db::queries::pg_model_reward_stats_48h(&self.pool)
                .await?
                .into_iter()
                .map(|stats| (stats.model_id.clone(), stats))
                .collect();
        let rows = tier_rows(&catalog, &healthy);
        let groups = sibling_groups(&rows);
        let mut report = BanditReport {
            groups_evaluated: groups.len(),
            ..Default::default()
        };

        for group in groups {
            let stats = |id: &str| {
                rewards.get(id).cloned().unwrap_or(ModelRewardStats {
                    model_id: id.to_owned(),
                    builds: 0,
                    approve_pct: None,
                })
            };
            let incumbent = group
                .iter()
                .max_by(|a, b| {
                    stats(&a.id)
                        .builds
                        .cmp(&stats(&b.id).builds)
                        .then_with(|| b.id.cmp(&a.id))
                })
                .expect("sibling groups are non-empty");
            let incumbent_stats = stats(&incumbent.id);
            for challenger in &group {
                if challenger.id == incumbent.id {
                    continue;
                }
                let challenger_stats = stats(&challenger.id);
                let decision = decide_promotion(&incumbent_stats, &challenger_stats);
                if decision == BanditDecision::Hold {
                    continue;
                }
                let new_tier = match decision {
                    BanditDecision::Promote => (challenger.tier - 1).max(1),
                    BanditDecision::Demote => challenger.tier + 1,
                    BanditDecision::Hold => unreachable!(),
                };
                ff_db::queries::pg_set_catalog_tier(&self.pool, &challenger.id, new_tier).await?;
                report.actions.push(BanditAction {
                    challenger_id: challenger.id.clone(),
                    incumbent_id: incumbent.id.clone(),
                    decision,
                    challenger_builds: challenger_stats.builds,
                    challenger_approve_pct: challenger_stats.approve_pct,
                    incumbent_builds: incumbent_stats.builds,
                    incumbent_approve_pct: incumbent_stats.approve_pct,
                    old_tier: challenger.tier,
                    new_tier,
                });
            }
        }

        if !report.actions.is_empty() {
            let body = format_report(&report);
            if let Err(error) = crate::telegram::send_telegram_from_secrets(
                &self.pool,
                "Autopilot-4 model bandit",
                &body,
            )
            .await
            {
                tracing::warn!(%error, "failed to send model bandit report");
            }
        }
        Ok(report)
    }
}

fn format_report(report: &BanditReport) -> String {
    report
        .actions
        .iter()
        .map(|action| {
            format!(
                "{:?} {} T{}→T{}: {} builds/{:.1}% vs {}: {} builds/{:.1}%",
                action.decision,
                action.challenger_id,
                action.old_tier,
                action.new_tier,
                action.challenger_builds,
                action.challenger_approve_pct.unwrap_or_default(),
                action.incumbent_id,
                action.incumbent_builds,
                action.incumbent_approve_pct.unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(builds: i64, approve_pct: Option<f64>) -> ModelRewardStats {
        ModelRewardStats {
            model_id: "fixture".into(),
            builds,
            approve_pct,
        }
    }

    #[test]
    fn promotion_boundary_requires_twenty_builds_and_ten_points() {
        assert_eq!(
            decide_promotion(&stats(40, Some(70.0)), &stats(20, Some(80.0))),
            BanditDecision::Promote
        );
        assert_eq!(
            decide_promotion(&stats(40, Some(70.0)), &stats(19, Some(100.0))),
            BanditDecision::Hold
        );
        assert_eq!(
            decide_promotion(&stats(40, Some(70.0)), &stats(20, Some(79.9))),
            BanditDecision::Hold
        );
    }

    #[test]
    fn demotion_boundary_is_fifteen_points() {
        assert_eq!(
            decide_promotion(&stats(40, Some(70.0)), &stats(20, Some(55.0))),
            BanditDecision::Demote
        );
        assert_eq!(
            decide_promotion(&stats(40, Some(70.0)), &stats(20, Some(55.1))),
            BanditDecision::Hold
        );
    }

    #[test]
    fn missing_verdict_stats_hold() {
        assert_eq!(
            decide_promotion(&stats(40, None), &stats(20, Some(90.0))),
            BanditDecision::Hold
        );
        assert_eq!(
            decide_promotion(&stats(40, Some(70.0)), &stats(20, None)),
            BanditDecision::Hold
        );
    }
}
