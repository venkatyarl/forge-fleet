use anyhow::Result;
use crate::{CYAN, RESET};

pub async fn handle_onboard(cmd: crate::OnboardCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        crate::OnboardCommand::Show {
            name,
            ip,
            ssh_user,
            role,
            runtime,
        } => {
            let token = ff_agent::fleet_info::fetch_secret("enrollment.shared_secret")
                .await
                .or_else(|| std::env::var("FORGEFLEET_ENROLLMENT_TOKEN").ok())
                .unwrap_or_else(|| "<SET-TOKEN-FIRST>".into());
            let leader =
                std::env::var("FORGEFLEET_LEADER_HOST").unwrap_or_else(|_| "192.168.5.100".into());
            let ssh_user = ssh_user.unwrap_or_else(|| name.clone());
            let ip_q = ip.unwrap_or_else(|| "auto".into());
            println!("{CYAN}▶ On the new computer, paste:{RESET}\n");
            println!("curl -fsSL 'http://{leader}:51002/onboard/bootstrap.sh\\");
            println!("    ?token={token}&name={name}&ip={ip_q}\\");
            println!("    &ssh_user={ssh_user}&role={role}&runtime={runtime}' \\");
            println!("  | sudo bash");
            println!("\n  (Or open http://{leader}:51002/onboard in the browser.)");
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
                    "This will DELETE fleet_nodes row '{name}', all its SSH keys, and mesh-status rows."
                );
                println!("Re-run with --yes to confirm.");
                return Ok(());
            }
            let removed_keys = ff_db::pg_delete_node_ssh_keys(&pool, &name).await?;
            let removed_mesh = ff_db::pg_delete_mesh_status_for_node(&pool, &name).await?;
            let r = sqlx::query("DELETE FROM fleet_nodes WHERE name = $1")
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
