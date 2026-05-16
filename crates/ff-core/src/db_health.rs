//! Postgres reachability + self-heal.
//!
//! Every `ff` command that needs the database calls [`ensure_postgres_up`]
//! before opening a pool. If Postgres is unreachable and we're on the DB
//! host (Taylor), we run `docker compose up -d postgres redis` and wait
//! for the port to come up — so operators don't have to remember the
//! recovery command.

use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{info, warn};

/// Probe Postgres with a short timeout. Returns `Ok(())` if a single
/// `SELECT 1` succeeds, `Err(msg)` otherwise.
pub async fn probe_postgres(db_url: &str, deadline: Duration) -> Result<(), String> {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .min_connections(0)
        .acquire_timeout(deadline)
        .connect(db_url)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    sqlx::query("SELECT 1")
        .execute(&pool)
        .await
        .map_err(|e| format!("query: {e}"))?;
    pool.close().await;
    Ok(())
}

/// Ensure Postgres is reachable at `db_url`. If unreachable and we're on
/// the same host as the DB, run `docker compose up -d` and wait for the
/// port. If we're not on the DB host, return the unreachable error
/// directly — only the host owning the data plane can start it.
pub async fn ensure_postgres_up(db_url: &str) -> Result<(), String> {
    // 1. Fast path — already up.
    if probe_postgres(db_url, Duration::from_secs(3)).await.is_ok() {
        return Ok(());
    }

    let (host, port) = parse_host_port(db_url)?;

    // 2. Are we on the DB host? If not, bail — we can't start Postgres
    //    on a remote box from here.
    if !is_local_host(&host).await {
        return Err(format!(
            "postgres unreachable at {host}:{port} and this isn't the DB host — \
             start the data plane on Taylor with `docker compose -f deploy/docker-compose.yml up -d`"
        ));
    }

    // 3. Locate docker-compose.yml relative to a known repo root.
    let compose = locate_compose_file().ok_or_else(|| {
        "postgres unreachable and could not locate deploy/docker-compose.yml — \
         set FORGEFLEET_REPO=/path/to/forge-fleet or run from the repo root"
            .to_string()
    })?;

    info!(
        compose = %compose.display(),
        host = %host,
        port,
        "postgres unreachable on DB host — running `docker compose up -d postgres redis`"
    );
    eprintln!(
        "ff: postgres unreachable at {host}:{port}, bringing data plane up via docker compose…"
    );

    let status = tokio::process::Command::new("docker")
        .arg("compose")
        .arg("-f")
        .arg(&compose)
        .arg("up")
        .arg("-d")
        .arg("postgres")
        .arg("redis")
        .status()
        .await
        .map_err(|e| format!("spawn docker: {e}"))?;
    if !status.success() {
        return Err(format!("`docker compose up -d` exited {}", status));
    }

    // 4. Wait up to ~30s for the port to accept queries.
    for attempt in 1..=15 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if probe_postgres(db_url, Duration::from_secs(2)).await.is_ok() {
            eprintln!("ff: postgres came up after {}s", attempt * 2);
            return Ok(());
        }
    }
    warn!("postgres still not reachable after docker compose up + 30s wait");
    Err("postgres still unreachable after docker compose up + 30s wait".into())
}

/// Parse `postgres://user:pass@host:port/db` → `(host, port)`. Falls back
/// to the standard Postgres port when none is specified.
fn parse_host_port(url: &str) -> Result<(String, u16), String> {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let after_auth = after_scheme.rsplit('@').next().unwrap_or(after_scheme);
    let host_port = after_auth.split('/').next().unwrap_or(after_auth);
    let mut parts = host_port.split(':');
    let host = parts
        .next()
        .ok_or_else(|| format!("no host in {url}"))?
        .to_string();
    let port: u16 = parts
        .next()
        .map(|p| p.parse().unwrap_or(5432))
        .unwrap_or(5432);
    Ok((host, port))
}

/// Best-effort check: is `host` reachable as "local" — meaning either
/// `localhost`, `127.0.0.1`, or an IP that matches one of our own
/// interfaces? We use this to decide whether `ff` can legitimately try
/// to start `docker compose` here.
async fn is_local_host(host: &str) -> bool {
    if host == "localhost" || host == "127.0.0.1" || host == "::1" {
        return true;
    }
    // Try binding a TCP socket to (host, 0). If we can bind, the host
    // resolves to one of our interfaces — i.e. we ARE that host.
    let probe = format!("{host}:0");
    let bound = timeout(Duration::from_millis(500), TcpStream::connect(&probe))
        .await
        .ok();
    // The above checks reachability, not ownership; check ownership by
    // comparing to our short hostname instead.
    drop(bound);
    if let Ok(local_name) = hostname_short() {
        // Allow the DB host either by exact hostname match or a known
        // alias mapping ("taylor" is Vinny's leader name).
        if local_name.eq_ignore_ascii_case(host) {
            return true;
        }
    }
    // Final check: enumerate local IPv4 addresses (Unix-only) and see
    // if `host` is one of them.
    local_ipv4_addresses()
        .iter()
        .any(|ip| ip.eq_ignore_ascii_case(host))
}

fn hostname_short() -> Result<String, String> {
    let out = std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .map_err(|e| format!("hostname: {e}"))?;
    if !out.status.success() {
        return Err(format!("hostname exit {}", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(unix)]
fn local_ipv4_addresses() -> Vec<String> {
    // Cheap shellout — `ifconfig`/`ip` is far simpler than libc getifaddrs
    // bindings and this only runs when Postgres is already down.
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg("ifconfig 2>/dev/null | awk '/inet /{print $2}' | grep -v '^127\\.'")
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(not(unix))]
fn local_ipv4_addresses() -> Vec<String> {
    Vec::new()
}

/// Find `deploy/docker-compose.yml` by checking known locations.
fn locate_compose_file() -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = [
        std::env::var("FORGEFLEET_REPO").ok().map(PathBuf::from),
        dirs_home().map(|h| h.join("projects/forge-fleet")),
        dirs_home().map(|h| h.join(".forgefleet/sub-agent-0/forge-fleet")),
        Some(PathBuf::from(".")),
    ]
    .into_iter()
    .flatten()
    .map(|root| root.join("deploy/docker-compose.yml"))
    .collect();

    candidates.into_iter().find(|p: &PathBuf| p.exists())
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

#[doc(hidden)]
pub fn _is_local_host_for_test(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

#[allow(dead_code)]
fn _force_path_use(_p: &Path) {}
