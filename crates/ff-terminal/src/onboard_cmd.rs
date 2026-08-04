use anyhow::Result;

const ONBOARDING_TRANSPORT_QUARANTINE: &str = "new-node onboarding is quarantined until the gateway has server-verified TLS transport; no bootstrap command was generated";

fn reject_onboarding_show() -> Result<()> {
    anyhow::bail!(ONBOARDING_TRANSPORT_QUARANTINE)
}

pub async fn handle_onboard(cmd: crate::OnboardCommand) -> Result<()> {
    // Fail before connecting to Postgres or resolving any secret. Until the
    // dedicated TLS listener exists, Show must not synthesize a command that
    // implies plaintext onboarding can work.
    if matches!(&cmd, crate::OnboardCommand::Show { .. }) {
        return reject_onboarding_show();
    }

    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        crate::OnboardCommand::Show { .. } => {
            unreachable!("Show is rejected before database access")
        }
        crate::OnboardCommand::List { limit } => {
            let nodes = ff_db::pg_list_nodes(&pool).await?;
            let mut sorted: Vec<&ff_db::FleetNodeRow> = nodes.iter().collect();
            sorted.sort_by(|a, b| b.election_priority.cmp(&a.election_priority));
            println!(
                "{:<15} {:<16} {:<10} {:<6} GH",
                "NAME", "IP", "RUNTIME", "PRIO"
            );
            for n in sorted.into_iter().take(limit as usize) {
                println!(
                    "{:<15} {:<16} {:<10} {:<6} {}",
                    n.name,
                    n.ip,
                    n.runtime,
                    n.election_priority,
                    n.gh_account.clone().unwrap_or_else(|| "-".into())
                );
            }
        }
        crate::OnboardCommand::Revoke { name, yes } => {
            if !yes {
                println!(
                    "This will DELETE fleet_workers row '{name}', all its SSH keys, and mesh-status rows."
                );
                println!("Re-run with --yes to confirm.");
                return Ok(());
            }
            let removed_keys = ff_db::pg_delete_node_ssh_keys(&pool, &name).await?;
            let removed_mesh = ff_db::pg_delete_mesh_status_for_node(&pool, &name).await?;
            let r = sqlx::query("DELETE FROM fleet_workers WHERE name = $1")
                .bind(&name)
                .execute(&pool)
                .await?;
            println!(
                "Revoked '{name}': {} ssh keys, {} mesh rows, {} node row(s)",
                removed_keys,
                removed_mesh,
                r.rows_affected()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const WEB_ONBOARDING_PAGE: &str =
        include_str!("../../../web-forge-fleet/app/(console)/onboarding/page.tsx");

    #[test]
    fn show_is_fail_closed_without_emitting_a_bootstrap_command() {
        let message = reject_onboarding_show()
            .expect_err("Show must remain quarantined")
            .to_string();
        assert!(message.contains("server-verified TLS"));
        assert!(message.contains("no bootstrap command was generated"));
        assert!(!message.contains("curl"));
        assert!(!message.contains(&["?", "token="].concat()));
    }

    #[test]
    fn web_onboarding_emits_neither_token_query_nor_runnable_bootstrap() {
        let token_query = ["?", "token="].concat();
        let shell_bootstrap = ["curl", " -fsSL"].concat();
        let powershell_bootstrap = ["iwr", " -useb"].concat();

        assert!(WEB_ONBOARDING_PAGE.contains("server-verified TLS"));
        assert!(WEB_ONBOARDING_PAGE.contains("No bootstrap command was generated"));
        assert!(!WEB_ONBOARDING_PAGE.contains(&token_query));
        assert!(!WEB_ONBOARDING_PAGE.contains(&shell_bootstrap));
        assert!(!WEB_ONBOARDING_PAGE.contains(&powershell_bootstrap));
        assert!(!WEB_ONBOARDING_PAGE.contains("enrollment.shared_secret"));
    }
}
