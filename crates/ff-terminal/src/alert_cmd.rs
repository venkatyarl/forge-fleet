use crate::truncate_str;
use anyhow::Result;

pub async fn handle_alert(cmd: crate::AlertCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        crate::AlertCommand::List => {
            let rows = sqlx::query_as::<
                _,
                (
                    uuid::Uuid,
                    String,
                    Option<String>,
                    String,
                    String,
                    String,
                    i32,
                    String,
                    i32,
                    String,
                    bool,
                ),
            >(
                "SELECT id, name, description, metric, scope, condition,
                        duration_secs, severity, cooldown_secs, channel, enabled
                 FROM alert_policies
                 ORDER BY name",
            )
            .fetch_all(&pool)
            .await?;

            if rows.is_empty() {
                println!("(no alert policies — run `ff alert policy seed`)");
                return Ok(());
            }
            println!(
                "{:<28} {:<10} {:<22} {:<15} {:<15} {:<10} {:<5}",
                "NAME", "SEVERITY", "METRIC", "CONDITION", "SCOPE", "CHANNEL", "ON?"
            );
            for (
                _id,
                name,
                _desc,
                metric,
                scope,
                condition,
                _duration,
                severity,
                _cooldown,
                channel,
                enabled,
            ) in rows
            {
                println!(
                    "{:<28} {:<10} {:<22} {:<15} {:<15} {:<10} {:<5}",
                    name,
                    severity,
                    metric,
                    condition,
                    scope,
                    channel,
                    if enabled { "yes" } else { "no" }
                );
            }
        }
        crate::AlertCommand::Events { active, limit } => {
            let sql = if active {
                "SELECT e.id, p.name, c.name, e.fired_at, e.resolved_at,
                        e.value, e.value_text, e.message, e.channel_result
                 FROM alert_events e
                 JOIN alert_policies p ON p.id = e.policy_id
                 LEFT JOIN computers c ON c.id = e.computer_id
                 WHERE e.resolved_at IS NULL
                 ORDER BY e.fired_at DESC
                 LIMIT $1"
            } else {
                "SELECT e.id, p.name, c.name, e.fired_at, e.resolved_at,
                        e.value, e.value_text, e.message, e.channel_result
                 FROM alert_events e
                 JOIN alert_policies p ON p.id = e.policy_id
                 LEFT JOIN computers c ON c.id = e.computer_id
                 ORDER BY e.fired_at DESC
                 LIMIT $1"
            };

            let rows = sqlx::query_as::<
                _,
                (
                    uuid::Uuid,
                    String,
                    Option<String>,
                    chrono::DateTime<chrono::Utc>,
                    Option<chrono::DateTime<chrono::Utc>>,
                    Option<f64>,
                    Option<String>,
                    Option<String>,
                    Option<String>,
                ),
            >(sql)
            .bind(limit)
            .fetch_all(&pool)
            .await?;

            if rows.is_empty() {
                if active {
                    println!("(no active alerts)");
                } else {
                    println!("(no alert events recorded yet)");
                }
                return Ok(());
            }
            println!(
                "{:<20} {:<18} {:<12} {:<10} MESSAGE",
                "FIRED", "POLICY", "COMPUTER", "STATE"
            );
            for (_id, policy, computer, fired_at, resolved_at, _v, _vt, message, _cr) in rows {
                let state = if resolved_at.is_some() {
                    "resolved"
                } else {
                    "firing"
                };
                println!(
                    "{:<20} {:<18} {:<12} {:<10} {}",
                    fired_at.format("%Y-%m-%d %H:%M:%S"),
                    truncate_str(&policy, 18),
                    truncate_str(&computer.unwrap_or_else(|| "-".into()), 12),
                    state,
                    message.unwrap_or_default()
                );
            }
        }
        crate::AlertCommand::Doctor { json } => {
            let rows = sqlx::query_as::<_, (String, String, String, bool)>(
                "SELECT name, metric, condition, enabled FROM alert_policies ORDER BY name",
            )
            .fetch_all(&pool)
            .await?;
            let report: Vec<DoctorRow> = rows
                .into_iter()
                .map(|(name, metric, condition, enabled)| {
                    let f =
                        ff_agent::alert_evaluator::classify_policy_fireability(&metric, &condition);
                    DoctorRow {
                        name,
                        metric,
                        condition,
                        enabled,
                        status: fireability_label(f).to_string(),
                        can_fire: f.can_fire(),
                    }
                })
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", render_alert_doctor(&report));
            }
        }
    }
    Ok(())
}

/// One policy's fireability verdict for `ff alert doctor`.
#[derive(Debug, Clone, serde::Serialize)]
struct DoctorRow {
    name: String,
    metric: String,
    condition: String,
    enabled: bool,
    /// Human label for the [`PolicyFireability`] classification.
    status: String,
    can_fire: bool,
}

/// Stable short label for a fireability classification.
fn fireability_label(f: ff_agent::alert_evaluator::PolicyFireability) -> &'static str {
    use ff_agent::alert_evaluator::PolicyFireability as P;
    match f {
        P::EvaluatorPolled => "evaluator-polled",
        P::ImperativelyFired => "imperatively-fired",
        P::Unparseable => "UNPARSEABLE-CONDITION",
        P::UnknownMetric => "UNKNOWN-METRIC",
        P::UnsatisfiableValue => "UNSATISFIABLE-VALUE",
    }
}

/// Render the `ff alert doctor` report. Pure (no I/O / color) so it is
/// unit-testable. The headline is whether any ENABLED policy cannot fire — a
/// disabled dead policy is informational, not a coverage hole.
fn render_alert_doctor(rows: &[DoctorRow]) -> String {
    let mut out = String::new();
    let total = rows.len();
    let dead_enabled: Vec<&DoctorRow> = rows.iter().filter(|r| r.enabled && !r.can_fire).collect();
    let dead_disabled: Vec<&DoctorRow> =
        rows.iter().filter(|r| !r.enabled && !r.can_fire).collect();

    out.push_str(&format!("alert policy doctor — {total} policies\n"));

    if dead_enabled.is_empty() {
        out.push_str("\n  ✓ every enabled policy can fire\n");
    } else {
        out.push_str(&format!(
            "\n  ⚠ {} ENABLED policy(ies) CANNOT fire (false coverage):\n",
            dead_enabled.len()
        ));
        for r in &dead_enabled {
            out.push_str(&format!(
                "    {} [{}] metric='{}' condition='{}'\n",
                r.name, r.status, r.metric, r.condition
            ));
        }
    }

    if !dead_disabled.is_empty() {
        out.push_str(&format!(
            "\n  · {} disabled policy(ies) also cannot fire (informational):\n",
            dead_disabled.len()
        ));
        for r in &dead_disabled {
            out.push_str(&format!("    {} [{}]\n", r.name, r.status));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, metric: &str, condition: &str, enabled: bool, can_fire: bool) -> DoctorRow {
        DoctorRow {
            name: name.into(),
            metric: metric.into(),
            condition: condition.into(),
            enabled,
            status: if can_fire {
                "evaluator-polled"
            } else {
                "UNKNOWN-METRIC"
            }
            .into(),
            can_fire,
        }
    }

    #[test]
    fn doctor_all_clear() {
        let rows = vec![
            row("high_cpu", "cpu_pct", "> 90", true, true),
            row("member_beat_dead", "beat_age_secs", "> 1800", true, true),
        ];
        let out = render_alert_doctor(&rows);
        assert!(out.contains("alert policy doctor — 2 policies"));
        assert!(out.contains("✓ every enabled policy can fire"));
        assert!(!out.contains("CANNOT fire"));
    }

    #[test]
    fn doctor_flags_enabled_dead_policy() {
        let rows = vec![
            row("high_cpu", "cpu_pct", "> 90", true, true),
            // Enabled but typo'd metric → must be flagged.
            row("typo_alert", "cpu_percent", "> 90", true, false),
        ];
        let out = render_alert_doctor(&rows);
        assert!(out.contains("1 ENABLED policy(ies) CANNOT fire"));
        assert!(out.contains("typo_alert"));
        assert!(out.contains("metric='cpu_percent'"));
    }

    #[test]
    fn doctor_disabled_dead_is_informational_only() {
        let rows = vec![
            row("high_cpu", "cpu_pct", "> 90", true, true),
            // Disabled dead policy (e.g. computer_offline after V146).
            row(
                "computer_offline",
                "computer_status",
                "== 'odown'",
                false,
                false,
            ),
        ];
        let out = render_alert_doctor(&rows);
        // Not counted as an enabled coverage hole...
        assert!(out.contains("✓ every enabled policy can fire"));
        // ...but surfaced as informational.
        assert!(out.contains("1 disabled policy(ies) also cannot fire"));
        assert!(out.contains("computer_offline"));
    }
}
