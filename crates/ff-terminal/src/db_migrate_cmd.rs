//! Explicit, bounded PostgreSQL migration control.
//!
//! `status` is strictly read-only. `apply` requires an exact target, full
//! source commit and affirmative acknowledgement; there is deliberately no
//! force flag and no daemon/startup path to explicit-only migrations.

use anyhow::{Context, Result};

use crate::{DbMigrateCommand, GREEN, RESET, YELLOW, whoami_tag};

pub async fn handle(command: DbMigrateCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|error| anyhow::anyhow!("connect Postgres: {error}"))?;
    match command {
        DbMigrateCommand::Status { json } => {
            let status = ff_db::postgres_migration_status(&pool)
                .await
                .context("read PostgreSQL migration status")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_status(&status);
            }
            Ok(())
        }
        DbMigrateCommand::Apply {
            to,
            source_commit,
            yes,
        } => {
            ensure_apply_confirmation(yes)?;
            let status = ff_db::apply_explicit_postgres_migrations(
                &pool,
                to,
                &source_commit,
                env!("FF_GIT_SHA"),
                env!("FF_GIT_STATE"),
                &whoami_tag(),
            )
            .await
            .context("bounded explicit PostgreSQL migration failed")?;
            println!(
                "{GREEN}PostgreSQL migration authority is exact at v{} ({}){RESET}",
                status.current_version, source_commit
            );
            print_status(&status);
            Ok(())
        }
    }
}

fn ensure_apply_confirmation(yes: bool) -> Result<()> {
    if !yes {
        anyhow::bail!(
            "explicit migration not applied; inspect `ff db migrate status`, then repeat with --yes"
        );
    }
    Ok(())
}

fn print_status(status: &ff_db::PostgresMigrationStatus) {
    println!("Current version:    {}", status.current_version);
    println!("Automatic ceiling:  {}", status.automatic_ceiling);
    println!("Explicit ceiling:   {}", status.explicit_ceiling);
    if let Some(valid) = status.rollout_schema_valid {
        println!("V295 schema exact:   {valid}");
    }
    if status.reviewed_v247_repair_pending {
        println!("Legacy repair:       reviewed V247 forward repair pending");
    } else if let Some(valid) = status.reconciliation_schema_valid {
        println!("Reconciliation exact:{valid:>6}");
    }
    if status.pending_automatic.is_empty() {
        println!("Pending automatic:  none");
    } else {
        let pending = status
            .pending_automatic
            .iter()
            .map(|migration| format!("v{} {}", migration.version, migration.name))
            .collect::<Vec<_>>()
            .join(", ");
        println!("Pending automatic:  {pending}");
    }
    if status.pending_explicit.is_empty() {
        println!("Pending explicit:   none");
    } else {
        let pending = status
            .pending_explicit
            .iter()
            .map(|migration| format!("v{} {}", migration.version, migration.name))
            .collect::<Vec<_>>()
            .join(", ");
        println!("Pending explicit:   {pending}");
    }
    if status.drift.is_empty() {
        println!("Drift:              none");
    } else {
        println!("{YELLOW}Drift (apply is blocked):{RESET}");
        for item in &status.drift {
            println!("  - {item}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn apply_requires_explicit_yes() {
        let error = ensure_apply_confirmation(false).expect_err("missing --yes must fail closed");
        assert!(error.to_string().contains("--yes"));
        ensure_apply_confirmation(true).unwrap();
    }

    #[test]
    fn cli_parses_exact_bounded_apply_and_has_no_force_bypass() {
        const SOURCE: &str = "39b017341b7536df64b61f42672ab33fb62343f8";
        let parse = |args: Vec<&'static str>| {
            std::thread::Builder::new()
                .stack_size(16 * 1024 * 1024)
                .spawn(move || crate::Cli::try_parse_from(args))
                .expect("spawn parser thread")
                .join()
                .expect("parser thread panicked")
        };
        let cli = parse(vec![
            "ff",
            "db",
            "migrate",
            "apply",
            "--to",
            "295",
            "--source-commit",
            SOURCE,
            "--yes",
        ])
        .expect("documented bounded apply syntax must parse");
        assert!(matches!(
            cli.command,
            Some(crate::Command::Db {
                command: crate::DbCommand::Migrate {
                    command: crate::DbMigrateCommand::Apply {
                        to: 295,
                        source_commit,
                        yes: true,
                    },
                },
            }) if source_commit == SOURCE
        ));
        assert!(
            parse(vec![
                "ff",
                "db",
                "migrate",
                "apply",
                "--to",
                "295",
                "--source-commit",
                SOURCE,
                "--yes",
                "--force",
            ])
            .is_err(),
            "there must be no force bypass"
        );
    }
}
