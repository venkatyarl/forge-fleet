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
    }
    Ok(())
}
