//! pgcat read/write-splitting configuration for the ForgeFleet gateway.
//!
//! pgcat (<https://github.com/postgresml/pgcat>) fronts the fleet Postgres
//! cluster as a connection pooler. This module models the subset of its TOML
//! configuration the gateway cares about and renders a `pgcat.toml` that
//! routes read traffic (SELECTs) to replicas and all writes to the primary:
//!
//! - `query_parser_enabled` + `query_parser_read_write_splitting` turn on
//!   pgcat's statement inspection so reads and writes are split per-query.
//! - Each backend is registered in the pool's shard with an explicit
//!   `primary` / `replica` role, which is what pgcat routes on.
//! - `default_role` is pinned to `primary` so any statement the parser cannot
//!   classify is routed somewhere it is always safe to execute.
//! - `primary_reads_enabled` is only turned on when no replicas are
//!   configured (otherwise reads would have nowhere to go).
//!
//! Build a config with [`PgcatConfig::from_urls`] (explicit URLs) or
//! [`PgcatConfig::from_env`] (`FORGEFLEET_POSTGRES_URL` / `FORGEFLEET_DATABASE_URL`
//! for the primary, `FORGEFLEET_POSTGRES_REPLICA_URLS` comma-separated for
//! replicas), then render it with [`PgcatConfig::to_toml`] or persist it with
//! [`PgcatConfig::write_to`].

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

/// Default port pgcat listens on for client connections.
pub const DEFAULT_PGCAT_PORT: u16 = 6432;
/// Default per-user server connection pool size.
pub const DEFAULT_POOL_SIZE: u32 = 20;
const DEFAULT_POSTGRES_PORT: u16 = 5432;

/// Errors produced while building or rendering a pgcat configuration.
#[derive(Debug, thiserror::Error)]
pub enum PgcatConfigError {
    #[error("invalid postgres url `{url}`: {reason}")]
    InvalidUrl { url: String, reason: &'static str },
    #[error(
        "no primary database url configured (set FORGEFLEET_POSTGRES_URL or FORGEFLEET_DATABASE_URL)"
    )]
    MissingPrimary,
    #[error("failed to serialize pgcat config: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("failed to write pgcat config: {0}")]
    Io(#[from] std::io::Error),
}

/// Role a backend server plays in a pgcat shard; pgcat routes reads/writes on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerRole {
    Primary,
    Replica,
}

/// Where pgcat sends statements its query parser cannot classify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultRole {
    Any,
    Primary,
    Replica,
}

/// pgcat pooling mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PoolMode {
    Transaction,
    Session,
}

/// How pgcat balances read traffic across candidate servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum LoadBalancingMode {
    #[serde(rename = "random")]
    Random,
    /// Least outstanding connections ("loc" in pgcat's config grammar).
    #[serde(rename = "loc")]
    LeastOutstandingConnections,
}

/// One parsed Postgres endpoint (from a `postgres://` connection URL).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgEndpoint {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub password: String,
}

/// `[general]` section — pgcat's own listener and health settings.
#[derive(Debug, Clone, Serialize)]
pub struct GeneralConfig {
    pub host: String,
    pub port: u16,
    /// Milliseconds to wait for a server connection to establish.
    pub connect_timeout: u64,
    /// Milliseconds an idle server connection may live before being reaped.
    pub idle_timeout: u64,
    /// Milliseconds allotted to a health-check query.
    pub healthcheck_timeout: u64,
    /// Milliseconds between health checks of idle servers.
    pub healthcheck_delay: u64,
    /// Seconds a misbehaving server is banned from the pool.
    pub ban_time: i64,
    pub admin_username: String,
    pub admin_password: String,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: DEFAULT_PGCAT_PORT,
            connect_timeout: 5_000,
            idle_timeout: 30_000,
            healthcheck_timeout: 1_000,
            healthcheck_delay: 30_000,
            ban_time: 60,
            admin_username: "pgcat".to_string(),
            admin_password: String::new(),
        }
    }
}

/// One `[pools.<name>.users.<n>]` entry.
#[derive(Debug, Clone, Serialize)]
pub struct UserConfig {
    pub username: String,
    pub password: String,
    pub pool_size: u32,
}

/// One `[pools.<name>.shards.<n>]` entry: the database plus its servers.
#[derive(Debug, Clone, Serialize)]
pub struct ShardConfig {
    pub database: String,
    /// `[host, port, role]` triples, exactly as pgcat expects them.
    pub servers: Vec<(String, u16, ServerRole)>,
}

/// One `[pools.<name>]` section with its read/write split rules.
///
/// Field order matters for TOML serialization: scalar values must precede the
/// nested `users` / `shards` tables.
#[derive(Debug, Clone, Serialize)]
pub struct PoolConfig {
    pub pool_mode: PoolMode,
    pub load_balancing_mode: LoadBalancingMode,
    pub default_role: DefaultRole,
    pub query_parser_enabled: bool,
    pub query_parser_read_write_splitting: bool,
    pub primary_reads_enabled: bool,
    pub users: BTreeMap<String, UserConfig>,
    pub shards: BTreeMap<String, ShardConfig>,
}

/// Full pgcat configuration document.
#[derive(Debug, Clone, Serialize)]
pub struct PgcatConfig {
    pub general: GeneralConfig,
    pub pools: BTreeMap<String, PoolConfig>,
}

impl PgcatConfig {
    /// Build a read/write-splitting config from a primary URL and zero or
    /// more replica URLs. The pool is named after the primary's database and
    /// authenticates with the primary's credentials.
    pub fn from_urls(primary_url: &str, replica_urls: &[String]) -> Result<Self, PgcatConfigError> {
        let primary = parse_postgres_url(primary_url)?;
        let replicas = replica_urls
            .iter()
            .map(|u| parse_postgres_url(u))
            .collect::<Result<Vec<_>, _>>()?;

        let mut servers = vec![(primary.host.clone(), primary.port, ServerRole::Primary)];
        servers.extend(
            replicas
                .iter()
                .map(|r| (r.host.clone(), r.port, ServerRole::Replica)),
        );

        let mut general = GeneralConfig::default();
        // Non-empty admin credential so pgcat's admin console isn't wide open.
        general.admin_password = primary.password.clone();

        let mut users = BTreeMap::new();
        users.insert(
            "0".to_string(),
            UserConfig {
                username: primary.username.clone(),
                password: primary.password.clone(),
                pool_size: DEFAULT_POOL_SIZE,
            },
        );

        let mut shards = BTreeMap::new();
        shards.insert(
            "0".to_string(),
            ShardConfig {
                database: primary.database.clone(),
                servers,
            },
        );

        let pool = PoolConfig {
            pool_mode: PoolMode::Transaction,
            load_balancing_mode: LoadBalancingMode::Random,
            default_role: DefaultRole::Primary,
            query_parser_enabled: true,
            query_parser_read_write_splitting: true,
            // With no replicas the primary must serve reads too.
            primary_reads_enabled: replicas.is_empty(),
            users,
            shards,
        };

        let mut pools = BTreeMap::new();
        pools.insert(primary.database, pool);
        Ok(Self { general, pools })
    }

    /// Build from the gateway's standard environment:
    /// `FORGEFLEET_POSTGRES_URL` (or `FORGEFLEET_DATABASE_URL`) for the
    /// primary, `FORGEFLEET_POSTGRES_REPLICA_URLS` (comma-separated) for
    /// replicas, plus optional `FORGEFLEET_PGCAT_PORT`,
    /// `FORGEFLEET_PGCAT_POOL_SIZE`, `FORGEFLEET_PGCAT_ADMIN_USER` and
    /// `FORGEFLEET_PGCAT_ADMIN_PASSWORD` overrides.
    pub fn from_env() -> Result<Self, PgcatConfigError> {
        let primary = std::env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
            .map_err(|_| PgcatConfigError::MissingPrimary)?;
        let replicas: Vec<String> = std::env::var("FORGEFLEET_POSTGRES_REPLICA_URLS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();

        let mut cfg = Self::from_urls(&primary, &replicas)?;
        if let Some(port) = env_parse::<u16>("FORGEFLEET_PGCAT_PORT") {
            cfg.general.port = port;
        }
        if let Some(size) = env_parse::<u32>("FORGEFLEET_PGCAT_POOL_SIZE") {
            for pool in cfg.pools.values_mut() {
                for user in pool.users.values_mut() {
                    user.pool_size = size;
                }
            }
        }
        if let Ok(user) = std::env::var("FORGEFLEET_PGCAT_ADMIN_USER") {
            cfg.general.admin_username = user;
        }
        if let Ok(pass) = std::env::var("FORGEFLEET_PGCAT_ADMIN_PASSWORD") {
            cfg.general.admin_password = pass;
        }
        Ok(cfg)
    }

    /// Render the configuration as a `pgcat.toml` document.
    pub fn to_toml(&self) -> Result<String, PgcatConfigError> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Render and write the configuration to `path`.
    pub fn write_to(&self, path: &Path) -> Result<(), PgcatConfigError> {
        std::fs::write(path, self.to_toml()?)?;
        Ok(())
    }
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok()?.trim().parse().ok()
}

/// Parse a `postgres://` / `postgresql://` connection URL into its parts.
///
/// Handles optional credentials, optional port (default 5432), bracketed
/// IPv6 hosts, percent-encoded userinfo/database, and ignores query params.
pub fn parse_postgres_url(url: &str) -> Result<PgEndpoint, PgcatConfigError> {
    let err = |reason: &'static str| PgcatConfigError::InvalidUrl {
        url: url.to_string(),
        reason,
    };
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .ok_or_else(|| err("expected postgres:// or postgresql:// scheme"))?;
    let rest = rest.split('?').next().unwrap_or_default();
    let rest = rest.split('#').next().unwrap_or_default();

    let (userinfo, host_part) = match rest.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, rest),
    };
    let (host_port, database) = match host_part.split_once('/') {
        Some((hp, db)) if !db.is_empty() => (hp, db),
        _ => return Err(err("missing database name")),
    };

    let (host, port) = if let Some(bracketed) = host_port.strip_prefix('[') {
        let (host, after) = bracketed
            .split_once(']')
            .ok_or_else(|| err("unclosed ipv6 host"))?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| err("invalid port"))?,
            None => DEFAULT_POSTGRES_PORT,
        };
        (host, port)
    } else {
        match host_port.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().map_err(|_| err("invalid port"))?),
            None => (host_port, DEFAULT_POSTGRES_PORT),
        }
    };
    if host.is_empty() {
        return Err(err("missing host"));
    }

    let (username, password) = match userinfo {
        Some(ui) => match ui.split_once(':') {
            Some((u, p)) => (u, p),
            None => (ui, ""),
        },
        None => ("postgres", ""),
    };

    Ok(PgEndpoint {
        host: host.to_string(),
        port,
        database: percent_decode(database),
        username: percent_decode(username),
        password: percent_decode(password),
    })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests are pure config-building/parsing — no Postgres connection —
    // so they are safe to run in CI without a database.

    #[test]
    fn parses_full_url() {
        let ep = parse_postgres_url(
            "postgres://ff:p%40ss@db.fleet.local:6001/forgefleet?sslmode=disable",
        )
        .unwrap();
        assert_eq!(ep.username, "ff");
        assert_eq!(ep.password, "p@ss");
        assert_eq!(ep.host, "db.fleet.local");
        assert_eq!(ep.port, 6001);
        assert_eq!(ep.database, "forgefleet");
    }

    #[test]
    fn parses_minimal_url_with_defaults() {
        let ep = parse_postgres_url("postgresql://localhost/forgefleet").unwrap();
        assert_eq!(ep.username, "postgres");
        assert_eq!(ep.password, "");
        assert_eq!(ep.host, "localhost");
        assert_eq!(ep.port, 5432);
        assert_eq!(ep.database, "forgefleet");
    }

    #[test]
    fn parses_ipv6_host() {
        let ep = parse_postgres_url("postgres://u:p@[::1]:5433/ff").unwrap();
        assert_eq!(ep.host, "::1");
        assert_eq!(ep.port, 5433);
    }

    #[test]
    fn rejects_bad_urls() {
        assert!(parse_postgres_url("mysql://localhost/ff").is_err());
        assert!(parse_postgres_url("postgres://localhost").is_err());
        assert!(parse_postgres_url("postgres://localhost:notaport/ff").is_err());
    }

    #[test]
    fn splits_reads_to_replicas() {
        let cfg = PgcatConfig::from_urls(
            "postgres://ff:secret@primary.fleet:5432/forgefleet",
            &[
                "postgres://ff:secret@replica-a.fleet:5432/forgefleet".to_string(),
                "postgres://ff:secret@replica-b.fleet:5433/forgefleet".to_string(),
            ],
        )
        .unwrap();

        let pool = &cfg.pools["forgefleet"];
        assert!(pool.query_parser_enabled);
        assert!(pool.query_parser_read_write_splitting);
        // Replicas exist, so the primary is dedicated to writes...
        assert!(!pool.primary_reads_enabled);
        // ...and unclassifiable statements fall back to the primary (safe).
        assert_eq!(pool.default_role, DefaultRole::Primary);

        let servers = &pool.shards["0"].servers;
        assert_eq!(
            servers,
            &vec![
                ("primary.fleet".to_string(), 5432, ServerRole::Primary),
                ("replica-a.fleet".to_string(), 5432, ServerRole::Replica),
                ("replica-b.fleet".to_string(), 5433, ServerRole::Replica),
            ]
        );
        assert_eq!(pool.users["0"].username, "ff");
        assert_eq!(pool.users["0"].password, "secret");
    }

    #[test]
    fn primary_serves_reads_when_no_replicas() {
        let cfg = PgcatConfig::from_urls("postgres://ff:secret@primary.fleet:5432/forgefleet", &[])
            .unwrap();
        let pool = &cfg.pools["forgefleet"];
        assert!(pool.primary_reads_enabled);
        assert_eq!(pool.shards["0"].servers.len(), 1);
    }

    #[test]
    fn renders_valid_pgcat_toml() {
        let cfg = PgcatConfig::from_urls(
            "postgres://ff:secret@primary.fleet:5432/forgefleet",
            &["postgres://ff:secret@replica-a.fleet:5432/forgefleet".to_string()],
        )
        .unwrap();
        let toml_str = cfg.to_toml().unwrap();

        // Must parse back as TOML and keep pgcat's expected shape.
        let doc: toml::Value = toml::from_str(&toml_str).unwrap();
        assert_eq!(doc["general"]["port"].as_integer(), Some(6432));
        let pool = &doc["pools"]["forgefleet"];
        assert_eq!(pool["pool_mode"].as_str(), Some("transaction"));
        assert_eq!(pool["load_balancing_mode"].as_str(), Some("random"));
        assert_eq!(pool["default_role"].as_str(), Some("primary"));
        assert_eq!(
            pool["query_parser_read_write_splitting"].as_bool(),
            Some(true)
        );
        let servers = pool["shards"]["0"]["servers"].as_array().unwrap();
        assert_eq!(servers[0][2].as_str(), Some("primary"));
        assert_eq!(servers[1][2].as_str(), Some("replica"));
        assert_eq!(pool["users"]["0"]["username"].as_str(), Some("ff"));
    }
}
