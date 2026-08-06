//! Infra canary — leader-gated tick that TCP-probes the fleet's SHARED state
//! services (Redis + NATS) and alerts when one dies.
//!
//! Why: the NATS container on the infra host died for ~35 hours unnoticed
//! (2026-08-03) — the fleet event bus silently dead while every surface
//! looked healthy, and the only symptom was dashboards going quietly stale.
//! A 60s probe with a 2-failure threshold catches that class in ~2 minutes.

use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// Consecutive probe failures before we alert (avoids single-blip noise).
const FAIL_THRESHOLD: u32 = 2;

struct ServiceProbe {
    name: &'static str,
    host: String,
    port: u16,
    consecutive_failures: u32,
    alerted: bool,
}

impl ServiceProbe {
    async fn check(&mut self, pg: &sqlx::PgPool) {
        let up = tokio::time::timeout(
            Duration::from_secs(4),
            tokio::net::TcpStream::connect((self.host.as_str(), self.port)),
        )
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false);

        if up {
            if self.alerted {
                self.alerted = false;
                self.consecutive_failures = 0;
                let msg = format!("infra canary: {} recovered ({}:{})", self.name, self.host, self.port);
                info!("{msg}");
                let _ = crate::alert_evaluator::dispatch_alert(pg, "log", "info", &msg).await;
                let _ = crate::alert_evaluator::dispatch_alert(pg, "telegram", "info", &msg).await;
            } else {
                self.consecutive_failures = 0;
            }
            return;
        }

        self.consecutive_failures += 1;
        if self.consecutive_failures >= FAIL_THRESHOLD && !self.alerted {
            self.alerted = true;
            let msg = format!(
                "infra canary: {} DOWN ({}:{} unreachable, {} consecutive failures)",
                self.name, self.host, self.port, self.consecutive_failures
            );
            warn!("{msg}");
            let _ = crate::alert_evaluator::dispatch_alert(pg, "log", "critical", &msg).await;
            let _ = crate::alert_evaluator::dispatch_alert(pg, "telegram", "critical", &msg).await;
        }
    }
}

/// Parse `redis://host:port[/db]` / `nats://host:port` style URLs.
fn host_port(url: &str, default_port: u16) -> (String, u16) {
    let rest = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("");
    let (host, port) = rest.rsplit_once(':').unwrap_or((rest, ""));
    let host = if host.is_empty() { "127.0.0.1" } else { host };
    let port = port.parse().unwrap_or(default_port);
    (host.to_string(), port)
}

/// Spawn the canary. `redis_url` comes from the fleet config; NATS comes from
/// FORGEFLEET_NATS_URL, defaulting to the infra host (Redis's host) on 54222.
pub fn spawn_infra_canary_tick(
    pg: sqlx::PgPool,
    redis_url: String,
    check_secs: u64,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let (redis_host, redis_port) = host_port(&redis_url, 56379);
        let nats_url = std::env::var("FORGEFLEET_NATS_URL")
            .unwrap_or_else(|_| format!("nats://{redis_host}:54222"));
        let (nats_host, nats_port) = host_port(&nats_url, 54222);

        let mut probes = vec![
            ServiceProbe {
                name: "redis",
                host: redis_host,
                port: redis_port,
                consecutive_failures: 0,
                alerted: false,
            },
            ServiceProbe {
                name: "nats",
                host: nats_host,
                port: nats_port,
                consecutive_failures: 0,
                alerted: false,
            },
        ];
        info!(
            redis = %format!("{}:{}", probes[0].host, probes[0].port),
            nats = %format!("{}:{}", probes[1].host, probes[1].port),
            "infra canary started"
        );

        let mut ticker = tokio::time::interval(Duration::from_secs(check_secs.max(30)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !crate::leader_cache::is_current_leader() {
                        continue;
                    }
                    for probe in &mut probes {
                        probe.check(&pg).await;
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("infra canary shutting down");
                        return;
                    }
                }
            }
        }
    })
}

/// Shared reqwest-free endpoint resolution used by main.rs to pass config in.
pub fn resolve_redis_url(config: &ff_core::config::FleetConfig) -> String {
    let url = config.redis.url.trim();
    if url.is_empty() {
        "redis://127.0.0.1:56379".to_string()
    } else {
        url.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::host_port;

    #[test]
    fn parses_redis_urls() {
        assert_eq!(host_port("redis://192.168.5.104:56379", 1), ("192.168.5.104".into(), 56379));
        assert_eq!(host_port("redis://10.0.0.2:6380/0", 1), ("10.0.0.2".into(), 6380));
        assert_eq!(host_port("redis://localhost", 56379), ("localhost".into(), 56379));
    }

    #[test]
    fn parses_nats_urls() {
        assert_eq!(host_port("nats://192.168.5.104:54222", 1), ("192.168.5.104".into(), 54222));
    }
}
