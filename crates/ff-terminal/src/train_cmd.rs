use crate::{GREEN, RESET, truncate_str, whoami_tag};
use anyhow::Result;

pub async fn handle_train(cmd: crate::TrainCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    let orch = ff_agent::training_orchestrator::TrainingOrchestrator::new(pool.clone());

    match cmd {
        crate::TrainCommand::Create {
            name,
            base,
            dataset,
            output,
            training_type,
            computer,
            epochs,
            learning_rate,
            batch_size,
            lora_rank,
            max_seq_len,
        } => {
            let spec = ff_agent::training_orchestrator::TrainingJobSpec {
                name: name.clone(),
                base_model_id: base,
                training_data_path: dataset,
                adapter_output_path: output,
                training_type,
                computer_name: computer,
                epochs,
                learning_rate,
                batch_size,
                lora_rank,
                max_seq_len,
                created_by: Some(whoami_tag()),
            };
            let id = orch
                .create_job(spec)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("{GREEN}✓ Created training job {id}{RESET}");
            println!("  name:   {name}");
            println!("  status: queued");
            println!();
            println!("Start it with: ff train start {id}");
            Ok(())
        }
        crate::TrainCommand::Start { id } => {
            let uuid =
                sqlx::types::Uuid::parse_str(&id).map_err(|e| anyhow::anyhow!("bad uuid: {e}"))?;
            let deferred = orch
                .start_job(uuid)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("{GREEN}✓ Training job {id} dispatched{RESET}");
            println!("  deferred_task: {deferred}");
            Ok(())
        }
        crate::TrainCommand::List { status, limit } => {
            let rows = ff_db::pg_list_training_jobs(&pool, status.as_deref(), limit).await?;
            if rows.is_empty() {
                println!("(no training jobs)");
                return Ok(());
            }
            println!(
                "{:<38} {:<22} {:<12} {:<10} {:<10} CREATED",
                "ID", "NAME", "STATUS", "TYPE", "COMPUTER"
            );
            for r in rows {
                let created = r.created_at.format("%Y-%m-%d %H:%M").to_string();
                println!(
                    "{:<38} {:<22} {:<12} {:<10} {:<10} {}",
                    r.id,
                    truncate_str(&r.name, 22),
                    r.status,
                    r.training_type,
                    truncate_str(r.computer_name.as_deref().unwrap_or("-"), 10),
                    created
                );
            }
            Ok(())
        }
        crate::TrainCommand::Show { id } => {
            let uuid =
                sqlx::types::Uuid::parse_str(&id).map_err(|e| anyhow::anyhow!("bad uuid: {e}"))?;
            match ff_db::pg_get_training_job(&pool, uuid).await? {
                Some(r) => {
                    println!("ID:              {}", r.id);
                    println!("Name:            {}", r.name);
                    println!("Status:          {}", r.status);
                    println!("Type:            {}", r.training_type);
                    println!(
                        "Base model:      {}",
                        r.base_model_id.unwrap_or_else(|| "-".into())
                    );
                    println!("Dataset:         {}", r.training_data_path);
                    if let Some(out) = r.adapter_output_path {
                        println!("Adapter output:  {out}");
                    }
                    if let Some(c) = r.computer_name {
                        println!("Computer:        {c}");
                    }
                    if let Some(t) = r.started_at {
                        println!("Started:         {}", t.format("%Y-%m-%d %H:%M UTC"));
                    }
                    if let Some(t) = r.completed_at {
                        println!("Completed:       {}", t.format("%Y-%m-%d %H:%M UTC"));
                    }
                    if let Some(deferred) = r.deferred_task_id {
                        println!("Deferred task:   {deferred}");
                    }
                    if let Some(err) = r.error_message {
                        println!("Error:           {err}");
                    }
                    if let Some(rm) = r.result_model_id {
                        println!("Result model:    {rm}");
                    }
                    let loss_samples = r.loss_curve.as_array().map(|a| a.len()).unwrap_or(0);
                    println!("Loss samples:    {loss_samples}");
                    println!(
                        "Params:\n{}",
                        serde_json::to_string_pretty(&r.params).unwrap_or_default()
                    );
                }
                None => {
                    eprintln!("No training job with id '{id}'");
                    std::process::exit(1);
                }
            }
            Ok(())
        }
    }
}
