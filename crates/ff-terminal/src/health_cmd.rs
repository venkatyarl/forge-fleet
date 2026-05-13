use anyhow::Result;
use ff_agent::agent_loop::AgentSessionConfig;
use crate::{GREEN, RED, RESET, YELLOW};

pub async fn handle_health(c: &AgentSessionConfig) -> Result<()> {
    static SHARED_HTTP: std::sync::LazyLock<reqwest::Client> =
        std::sync::LazyLock::new(|| reqwest::Client::new());

    let nodes = load_fleet_nodes_for_health(c).await;

    let futs: Vec<_> = nodes
        .iter()
        .map(|(name, ip, port)| {
            let client = &*SHARED_HTTP;
            let url = format!("http://{ip}:{port}/health");
            let agent_url = format!("http://{ip}:50002/health");
            let name = name.clone();
            let ip = ip.clone();
            let port = *port;
            async move {
                let daemon_ok = client
                    .get(&url)
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false);
                let agent_ok = client
                    .get(&agent_url)
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false);
                (name, ip, port, daemon_ok, agent_ok)
            }
        })
        .collect();

    let results = futures::future::join_all(futs).await;

    println!("{GREEN}✓ ForgeFleet Health{RESET}");
    for (name, ip, port, daemon_ok, agent_ok) in results {
        let daemon_str = if daemon_ok {
            format!("{GREEN}ONLINE{RESET}")
        } else {
            format!("{RED}OFFLINE{RESET}")
        };
        let agent_str = if agent_ok {
            format!("  agent{GREEN}✓{RESET}")
        } else {
            format!("  agent{YELLOW}✗{RESET}")
        };
        println!("  {name:<12} {ip}:{port}  {daemon_str}{agent_str}");
    }
    Ok(())
}

async fn load_fleet_nodes_for_health(_c: &AgentSessionConfig) -> Vec<(String, String, u16)> {
    let config_path = dirs::home_dir()
        .unwrap_or_default()
        .join(".forgefleet/fleet.toml");

    if let Ok(toml_str) = tokio::fs::read_to_string(&config_path).await
        && let Ok(cfg) = toml::from_str::<ff_core::config::FleetConfig>(&toml_str)
    {
        let db_url = cfg.database.url.trim().to_string();
        if !db_url.is_empty()
            && let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(std::time::Duration::from_secs(3))
                .connect(&db_url)
                .await
        {
            let rows: Vec<(String, String)> =
                sqlx::query_as("SELECT name, ip FROM fleet_workers ORDER BY election_priority, name")
                    .fetch_all(&pool)
                    .await
                    .unwrap_or_default();

            if !rows.is_empty() {
                return rows.into_iter().map(|(n, ip)| (n, ip, 51000u16)).collect();
            }
        }
    }

    vec![
        ("Taylor".into(), "192.168.5.100".into(), 51000),
        ("Marcus".into(), "192.168.5.102".into(), 51000),
        ("Sophie".into(), "192.168.5.103".into(), 51000),
        ("Priya".into(), "192.168.5.104".into(), 51000),
        ("James".into(), "192.168.5.108".into(), 51000),
        ("Logan".into(), "192.168.5.111".into(), 51000),
        ("Lily".into(), "192.168.5.113".into(), 51000),
        ("Veronica".into(), "192.168.5.112".into(), 51000),
        ("Duncan".into(), "192.168.5.114".into(), 51000),
        ("Aura".into(), "192.168.5.110".into(), 51000),
    ]
}
