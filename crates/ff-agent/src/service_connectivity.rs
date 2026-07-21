//! Read-only, bounded connectivity probes used by `ff health`.

use ff_core::config::FleetConfig;
use futures::future::join_all;
use serde::Serialize;
use sqlx::PgPool;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectivityStatus {
    Healthy,
    Unavailable,
    Unconfigured,
}

impl ConnectivityStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Unavailable => "unavailable",
            Self::Unconfigured => "unconfigured",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConnectivityCheck {
    pub service: String,
    pub status: ConnectivityStatus,
    pub latency_ms: Option<u64>,
}

impl ConnectivityCheck {
    fn unconfigured(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            status: ConnectivityStatus::Unconfigured,
            latency_ms: None,
        }
    }
}

async fn bounded<F>(service: String, timeout: Duration, future: F) -> ConnectivityCheck
where
    F: Future<Output = bool>,
{
    let started = Instant::now();
    let status = match tokio::time::timeout(timeout, future).await {
        Ok(true) => ConnectivityStatus::Healthy,
        Ok(false) | Err(_) => ConnectivityStatus::Unavailable,
    };
    ConnectivityCheck {
        service,
        status,
        latency_ms: Some(started.elapsed().as_millis() as u64),
    }
}

fn first_env(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
}

/// Probe node-local service dependencies without writing data or returning
/// connection strings, tokens, command output, or library error messages.
pub async fn check_services(
    config: &FleetConfig,
    http: &reqwest::Client,
    timeout: Duration,
) -> Vec<ConnectivityCheck> {
    check_services_with_leader(config, http, timeout, None).await
}

async fn check_services_with_leader(
    config: &FleetConfig,
    http: &reqwest::Client,
    timeout: Duration,
    leader: Option<&ff_db::leader_state::LeaderEndpoints>,
) -> Vec<ConnectivityCheck> {
    let op_configured =
        first_env(&["OP_SERVICE_ACCOUNT_TOKEN", "OP_CONNECT_TOKEN", "OP_SESSION"]).is_some();
    let op = if op_configured {
        bounded("1password".into(), timeout, async {
            let mut command = tokio::process::Command::new("op");
            command.args(["whoami", "--format=json"]);
            command.kill_on_drop(true);
            command.stdin(std::process::Stdio::null());
            command.stdout(std::process::Stdio::null());
            command.stderr(std::process::Stdio::null());
            command.status().await.is_ok_and(|status| status.success())
        })
        .await
    } else {
        ConnectivityCheck::unconfigured("1password")
    };

    let github_token = first_env(&["GH_TOKEN", "GITHUB_TOKEN"]);
    let github = if let Some(token) = github_token {
        bounded("github".into(), timeout, async {
            http.get("https://api.github.com/user")
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .bearer_auth(token)
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
        })
        .await
    } else {
        ConnectivityCheck::unconfigured("github")
    };

    let postgres_url = first_env(&["FORGEFLEET_POSTGRES_URL", "FORGEFLEET_DATABASE_URL"])
        .unwrap_or_else(|| config.database.url.clone());
    let postgres = if postgres_url.trim().is_empty() {
        ConnectivityCheck::unconfigured("postgres")
    } else {
        bounded("postgres".into(), timeout, async {
            let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(timeout)
                .connect(&postgres_url)
                .await
            else {
                return false;
            };
            let ok = sqlx::query_scalar::<_, i32>("SELECT 1")
                .fetch_one(&pool)
                .await
                .is_ok_and(|value| value == 1);
            pool.close().await;
            ok
        })
        .await
    };

    let redis_url = leader
        .and_then(|e| e.redis_url.clone())
        .or_else(|| first_env(&["FORGEFLEET_REDIS_URL"]))
        .unwrap_or_else(|| config.redis.url.clone());
    let redis = if redis_url.trim().is_empty() {
        ConnectivityCheck::unconfigured("redis")
    } else {
        bounded("redis".into(), timeout, async {
            let Ok(client) = redis::Client::open(redis_url) else {
                return false;
            };
            let Ok(mut connection) = client.get_multiplexed_async_connection().await else {
                return false;
            };
            redis::cmd("PING")
                .query_async::<String>(&mut connection)
                .await
                .is_ok_and(|reply| reply == "PONG")
        })
        .await
    };

    let nats_url = leader
        .and_then(|e| e.nats_url.clone())
        .or_else(|| first_env(&["FORGEFLEET_NATS_URL"]))
        .unwrap_or_else(crate::nats_client::resolve_nats_url);
    let nats = bounded("nats".into(), timeout, async {
        let Ok(client) = async_nats::connect(nats_url).await else {
            return false;
        };
        client.flush().await.is_ok()
    })
    .await;

    let llms = join_all(config.llm.ports.iter().copied().map(|port| async move {
        let service = format!("llm:{port}");
        bounded(service, timeout, async move {
            let health = format!("http://127.0.0.1:{port}/health");
            let models = format!("http://127.0.0.1:{port}/v1/models");
            for url in [health, models] {
                if http
                    .get(url)
                    .send()
                    .await
                    .is_ok_and(|response| response.status().is_success())
                {
                    return true;
                }
            }
            false
        })
        .await
    }))
    .await;

    let mut checks = vec![op, github, postgres, redis, nats];
    if llms.is_empty() {
        checks.push(ConnectivityCheck::unconfigured("llm"));
    } else {
        checks.extend(llms);
    }
    checks
}

/// Run this node's probes and retain one current, sanitized result per service.
pub async fn check_and_persist(
    pg: &PgPool,
    worker_name: &str,
    config: &FleetConfig,
    http: &reqwest::Client,
    timeout: Duration,
) -> Result<usize, sqlx::Error> {
    let leader = ff_db::leader_state::pg_get_leader_endpoints(pg).await?;
    let checks = check_services_with_leader(config, http, timeout, Some(&leader)).await;
    let mut tx = pg.begin().await?;
    let mut written = 0;

    for check in checks {
        let result = sqlx::query(
            r#"
            INSERT INTO service_connectivity_status
                (computer_id, service, status, latency_ms, checked_at)
            SELECT id, $2, $3, $4, NOW()
              FROM computers
             WHERE LOWER(name) = LOWER($1)
            ON CONFLICT (computer_id, service) DO UPDATE SET
                status = EXCLUDED.status,
                latency_ms = EXCLUDED.latency_ms,
                checked_at = EXCLUDED.checked_at
            "#,
        )
        .bind(worker_name)
        .bind(&check.service)
        .bind(check.status.as_str())
        .bind(check.latency_ms.map(|value| value as i64))
        .execute(&mut *tx)
        .await?;
        written += result.rows_affected() as usize;
    }

    tx.commit().await?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unconfigured_check_never_has_latency() {
        let check = ConnectivityCheck::unconfigured("github");
        assert_eq!(check.status, ConnectivityStatus::Unconfigured);
        assert_eq!(check.latency_ms, None);
        assert_eq!(check.status.as_str(), "unconfigured");
    }

    #[tokio::test]
    async fn bounded_probe_reports_success_and_timeout() {
        let ok = bounded("test".into(), Duration::from_millis(20), async { true }).await;
        assert_eq!(ok.status, ConnectivityStatus::Healthy);

        let timed_out = bounded("test".into(), Duration::from_millis(1), async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            true
        })
        .await;
        assert_eq!(timed_out.status, ConnectivityStatus::Unavailable);
    }
}
