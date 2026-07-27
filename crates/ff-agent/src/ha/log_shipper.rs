//! Per-node shipping of WARN/ERROR daemon log entries to `fleet_logs`.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use super::log_monitor::{LogLevel, detect_log_level};

const LOG_PATH_ENV: &str = "FORGEFLEETD_LOG_PATH";
const CURSOR_PATH_ENV: &str = "FORGEFLEET_LOG_SHIPPER_CURSOR_PATH";

#[derive(Debug, Clone, PartialEq, Eq)]
struct FleetLogEntry {
    ts: DateTime<Utc>,
    level: &'static str,
    message: String,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
struct Cursor {
    offset: u64,
    file_id: u64,
}

/// Create the shipper's Postgres pool using the same fleet URL precedence as
/// `ff-db` and its database tests.
pub async fn connect_database() -> Result<PgPool> {
    let url = std::env::var("FORGEFLEET_POSTGRES_URL")
        .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
        .context("FORGEFLEET_POSTGRES_URL or FORGEFLEET_DATABASE_URL must be set")?;
    PgPoolOptions::new()
        .max_connections(2)
        .min_connections(0)
        .connect(&url)
        .await
        .context("connect log shipper to Postgres")
}

/// Ship newly appended entries from the configured daemon log.
pub async fn run_log_shipper_tick(pg: &PgPool, node_id: &str) -> Result<usize> {
    ship_once(
        pg,
        node_id,
        &configured_log_path(),
        &configured_cursor_path(),
    )
    .await
}

fn configured_log_path() -> PathBuf {
    std::env::var_os(LOG_PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| forgefleet_home().join("logs/forgefleetd.log"))
}

fn configured_cursor_path() -> PathBuf {
    std::env::var_os(CURSOR_PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| forgefleet_home().join("log_shipper.cursor"))
}

fn forgefleet_home() -> PathBuf {
    std::env::var_os("FORGEFLEET_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".forgefleet")))
        .unwrap_or_else(|| PathBuf::from(".forgefleet"))
}

async fn ship_once(
    pg: &PgPool,
    node_id: &str,
    log_path: &Path,
    cursor_path: &Path,
) -> Result<usize> {
    let mut file = match std::fs::File::open(log_path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err).with_context(|| format!("open {}", log_path.display())),
    };
    let metadata = file.metadata()?;
    let file_id = file_id(&metadata);

    let Some(mut cursor) = load_cursor(cursor_path)? else {
        persist_cursor(
            cursor_path,
            Cursor {
                offset: metadata.len(),
                file_id,
            },
        )?;
        return Ok(0);
    };

    if cursor.file_id != file_id || cursor.offset > metadata.len() {
        cursor.offset = 0;
        cursor.file_id = file_id;
    }

    file.seek(SeekFrom::Start(cursor.offset))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let consumed = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1);
    if consumed == 0 {
        return Ok(0);
    }

    let text = String::from_utf8_lossy(&bytes[..consumed]);
    let entries: Vec<_> = text.lines().filter_map(parse_entry).collect();
    let mut tx = pg.begin().await?;
    for entry in &entries {
        sqlx::query(
            "INSERT INTO fleet_logs (id, ts, node_id, log_level, message) \
             VALUES (gen_random_uuid(), $1, $2, $3, $4)",
        )
        .bind(entry.ts)
        .bind(node_id)
        .bind(entry.level)
        .bind(&entry.message)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    cursor.offset += consumed as u64;
    persist_cursor(cursor_path, cursor)?;
    Ok(entries.len())
}

fn parse_entry(line: &str) -> Option<FleetLogEntry> {
    let cleaned = strip_ansi(line);
    let (level, canonical) = detect_log_level(&cleaned)?;
    let level = match level {
        LogLevel::Error => "ERROR",
        LogLevel::Warn => "WARN",
    };
    let ts = cleaned
        .split_whitespace()
        .next()
        .and_then(|token| DateTime::parse_from_rfc3339(token).ok())
        .map(|ts| ts.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    Some(FleetLogEntry {
        ts,
        level,
        message: canonical.to_string(),
    })
}

fn strip_ansi(line: &str) -> String {
    let mut output = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for code in chars.by_ref() {
                if code.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            output.push(ch);
        }
    }
    output
}

fn load_cursor(path: &Path) -> Result<Option<Cursor>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn persist_cursor(path: &Path, cursor: Cursor) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("cursor.tmp");
    std::fs::write(&temporary, serde_json::to_vec(&cursor)?)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(unix)]
fn file_id(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.dev().wrapping_mul(31).wrapping_add(metadata.ino())
}

#[cfg(not(unix))]
fn file_id(_metadata: &std::fs::Metadata) -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_warn_and_error_but_not_info() {
        let error = parse_entry("2026-07-27T12:00:00Z ERROR ff_agent: failed").unwrap();
        assert_eq!(error.level, "ERROR");
        assert_eq!(error.message, "ERROR ff_agent: failed");
        assert_eq!(
            error.ts,
            "2026-07-27T12:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );

        let warn =
            parse_entry("\u{1b}[33m2026-07-27T12:00:01Z WARN ff_agent: slow\u{1b}[0m").unwrap();
        assert_eq!(warn.level, "WARN");
        assert_eq!(warn.message, "WARN ff_agent: slow");
        assert!(parse_entry("2026-07-27T12:00:02Z INFO ff_agent: ready").is_none());
    }

    #[test]
    fn cursor_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shipper.cursor");
        let cursor = Cursor {
            offset: 42,
            file_id: 7,
        };
        persist_cursor(&path, cursor).unwrap();
        assert_eq!(load_cursor(&path).unwrap().unwrap().offset, 42);
        assert_eq!(load_cursor(&path).unwrap().unwrap().file_id, 7);
    }
}
