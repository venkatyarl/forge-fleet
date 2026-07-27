//! Ships new lines from the local `forgefleetd.log` to the central
//! `fleet_logs` table.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};
use tokio_util::sync::CancellationToken;

const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);
const MAX_BATCH_LINES: usize = 1_000;
const MAX_MESSAGE_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub struct LogShippingService {
    pool: PgPool,
    node_id: String,
    log_path: PathBuf,
    cursor_path: PathBuf,
    interval: Duration,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct Cursor {
    file_id: u64,
    generation: u64,
    offset: u64,
}

#[derive(Debug, Eq, PartialEq)]
struct LogEntry {
    offset: u64,
    message: String,
}

impl LogShippingService {
    pub fn new(pool: PgPool, node_id: impl Into<String>, log_path: PathBuf) -> Self {
        let cursor_path = log_path.with_extension("log.shipper.cursor");
        Self {
            pool,
            node_id: node_id.into(),
            log_path,
            cursor_path,
            interval: DEFAULT_INTERVAL,
        }
    }

    pub fn spawn(self, shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(self.interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = ticker.tick() => {
                        if let Err(error) = self.ship_once().await {
                            tracing::warn!(%error, path = %self.log_path.display(), "log shipping pass failed");
                        }
                    }
                }
            }
        })
    }

    pub async fn ship_once(&self) -> Result<usize> {
        let cursor = load_cursor(&self.cursor_path).await.unwrap_or_default();
        let Some((entries, next_cursor)) = read_new_lines(&self.log_path, cursor).await? else {
            return Ok(0);
        };
        if entries.is_empty() {
            save_cursor(&self.cursor_path, next_cursor).await?;
            return Ok(0);
        }

        let mut tx = self.pool.begin().await?;
        for entry in &entries {
            let id = entry_id(&self.node_id, &self.log_path, next_cursor, entry);
            sqlx::query(
                "INSERT INTO fleet_logs (id, node_id, log_level, message) \
                 VALUES ($1, $2, $3, $4) ON CONFLICT (id) DO NOTHING",
            )
            .bind(id)
            .bind(&self.node_id)
            .bind(log_level(&entry.message))
            .bind(&entry.message)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        save_cursor(&self.cursor_path, next_cursor).await?;
        Ok(entries.len())
    }
}

async fn read_new_lines(path: &Path, cursor: Cursor) -> Result<Option<(Vec<LogEntry>, Cursor)>> {
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("open {}", path.display())),
    };
    let metadata = file.metadata().await?;
    let file_id = file_id(&metadata);
    let same_file = cursor.file_id == file_id;
    let truncated = same_file && cursor.offset > metadata.len();
    let offset = if same_file && !truncated {
        cursor.offset
    } else {
        0
    };
    let generation = cursor.generation + u64::from(truncated);

    let mut reader = BufReader::new(file);
    reader.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut entries = Vec::new();
    let mut line = String::new();
    let mut committed_offset = offset;
    while entries.len() < MAX_BATCH_LINES {
        line.clear();
        let bytes = reader.read_line(&mut line).await?;
        if bytes == 0 {
            break;
        }
        if !line.ends_with('\n') {
            // Do not ship a line that the daemon may still be writing.
            break;
        }
        let message = line.trim_end_matches(['\r', '\n']);
        let message = truncate_utf8(message, MAX_MESSAGE_BYTES).to_owned();
        entries.push(LogEntry {
            offset: committed_offset,
            message,
        });
        committed_offset += bytes as u64;
    }
    let next_cursor = Cursor {
        file_id,
        generation,
        offset: committed_offset,
    };
    Ok(Some((entries, next_cursor)))
}

async fn load_cursor(path: &Path) -> Result<Cursor> {
    let bytes = tokio::fs::read(path).await?;
    serde_json::from_slice(&bytes).context("parse log shipping cursor")
}

async fn save_cursor(path: &Path, cursor: Cursor) -> Result<()> {
    let tmp = path.with_extension("cursor.tmp");
    tokio::fs::write(&tmp, serde_json::to_vec(&cursor)?).await?;
    tokio::fs::rename(&tmp, path).await?;
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

fn log_level(line: &str) -> &'static str {
    for level in ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"] {
        if line
            .split(|c: char| c.is_whitespace() || c == ':')
            .any(|part| part == level)
        {
            return level;
        }
    }
    "INFO"
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn entry_id(node_id: &str, path: &Path, cursor: Cursor, entry: &LogEntry) -> uuid::Uuid {
    let mut hash = Sha256::new();
    hash.update(node_id.as_bytes());
    hash.update(path.as_os_str().as_encoded_bytes());
    hash.update(cursor.file_id.to_le_bytes());
    hash.update(cursor.generation.to_le_bytes());
    hash.update(entry.offset.to_le_bytes());
    hash.update(entry.message.as_bytes());
    let digest = hash.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    uuid::Uuid::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_only_complete_new_lines_and_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("forgefleetd.log");
        tokio::fs::write(&path, b"INFO first\nWARN second\npartial")
            .await
            .unwrap();

        let (entries, cursor) = read_new_lines(&path, Cursor::default())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.message.as_str())
                .collect::<Vec<_>>(),
            ["INFO first", "WARN second"]
        );

        tokio::fs::write(&path, b"INFO first\nWARN second\npartial done\n")
            .await
            .unwrap();
        let (entries, _) = read_new_lines(&path, cursor).await.unwrap().unwrap();
        assert_eq!(entries[0].message, "partial done");
    }

    #[tokio::test]
    async fn resets_cursor_after_log_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("forgefleetd.log");
        tokio::fs::write(&path, b"old\n").await.unwrap();
        let (_, cursor) = read_new_lines(&path, Cursor::default())
            .await
            .unwrap()
            .unwrap();
        tokio::fs::remove_file(&path).await.unwrap();
        tokio::fs::write(&path, b"new\n").await.unwrap();

        let (entries, _) = read_new_lines(&path, cursor).await.unwrap().unwrap();
        assert_eq!(entries[0].message, "new");
    }

    #[test]
    fn extracts_level_tokens() {
        assert_eq!(log_level("2026-07-27T12:00:00Z ERROR failed"), "ERROR");
        assert_eq!(log_level("plain message"), "INFO");
    }

    #[tokio::test]
    async fn bounds_each_batch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("forgefleetd.log");
        tokio::fs::write(&path, "line\n".repeat(MAX_BATCH_LINES + 1))
            .await
            .unwrap();
        let (entries, _) = read_new_lines(&path, Cursor::default())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entries.len(), MAX_BATCH_LINES);
    }

    #[test]
    fn replay_produces_the_same_entry_id() {
        let cursor = Cursor {
            file_id: 7,
            generation: 2,
            offset: 12,
        };
        let entry = LogEntry {
            offset: 4,
            message: "INFO stable".into(),
        };
        let path = Path::new("/tmp/forgefleetd.log");
        assert_eq!(
            entry_id("node-a", path, cursor, &entry),
            entry_id("node-a", path, cursor, &entry)
        );
        assert_ne!(
            entry_id("node-a", path, cursor, &entry),
            entry_id("node-b", path, cursor, &entry)
        );
    }
}
