use anyhow::Result;
use crate::{GREEN, RESET, whoami_tag};

pub async fn handle_power(cmd: crate::PowerCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        crate::PowerCommand::Schedule { command } => match command {
            crate::PowerScheduleCommand::Create {
                computer,
                kind,
                cron,
                if_idle,
            } => {
                let computer_id = sqlx::query_scalar::<_, sqlx::types::Uuid>(
                    "SELECT id FROM computers WHERE name = $1",
                )
                .bind(&computer)
                .fetch_optional(&pool)
                .await?
                .ok_or_else(|| anyhow::anyhow!("computer '{computer}' not found"))?;

                let condition = if_idle.map(|m| format!("idle_minutes > {m}"));
                let id = ff_db::pg_create_schedule(
                    &pool,
                    computer_id,
                    &kind,
                    &cron,
                    condition.as_deref(),
                    Some(&whoami_tag()),
                )
                .await?;
                println!("{GREEN}✓ Created schedule {id}{RESET}");
                println!("  computer:   {computer}");
                println!("  kind:       {kind}");
                println!("  cron:       {cron}");
                if let Some(c) = condition {
                    println!("  condition:  {c}");
                }
                Ok(())
            }
            crate::PowerScheduleCommand::Delete { id } => {
                let uuid = sqlx::types::Uuid::parse_str(&id)
                    .map_err(|e| anyhow::anyhow!("bad uuid: {e}"))?;
                if ff_db::pg_delete_schedule(&pool, uuid).await? {
                    println!("{GREEN}✓ Deleted schedule {id}{RESET}");
                } else {
                    println!("No schedule with id '{id}'");
                }
                Ok(())
            }
        },
        crate::PowerCommand::Schedules { computer } => {
            let computer_id = if let Some(c) = computer {
                Some(
                    sqlx::query_scalar::<_, sqlx::types::Uuid>(
                        "SELECT id FROM computers WHERE name = $1",
                    )
                    .bind(&c)
                    .fetch_optional(&pool)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("computer '{c}' not found"))?,
                )
            } else {
                None
            };
            let rows = ff_db::pg_list_schedules(&pool, computer_id, false).await?;
            if rows.is_empty() {
                println!("(no power schedules)");
                return Ok(());
            }
            println!(
                "{:<38} {:<10} {:<9} {:<18} {:<10} LAST",
                "ID", "COMPUTER", "KIND", "CRON", "ENABLED"
            );
            for r in rows {
                let last = r
                    .last_fired_at
                    .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
                    .unwrap_or_else(|| "-".into());
                println!(
                    "{:<38} {:<10} {:<9} {:<18} {:<10} {}",
                    r.id,
                    r.computer_name.unwrap_or_else(|| "?".into()),
                    r.kind,
                    r.cron_expr,
                    if r.enabled { "yes" } else { "no" },
                    last
                );
            }
            Ok(())
        }
        crate::PowerCommand::Tick => {
            let sched = ff_agent::power_scheduler::PowerScheduler::new(pool.clone());
            let actions = sched
                .evaluate_once()
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if actions.is_empty() {
                println!("(no schedules matched this minute)");
            } else {
                for a in actions {
                    println!("{:<14} {:<9} {}", a.computer_name, a.kind, a.result);
                }
            }
            Ok(())
        }
    }
}
