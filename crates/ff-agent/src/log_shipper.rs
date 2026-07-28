//! Agent task for shipping local logs to the fleet log store.

use std::{
    collections::HashMap,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::Duration,
};

use crate::config::LogShippingConfig;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Tail configured log files and ship WARN/ERROR entries until cancelled.
pub async fn run(
    pool: PgPool,
    node_id: String,
    config: LogShippingConfig,
    cancel: CancellationToken,
) {
    if !config.enabled {
        return;
    }

    let mut offsets = HashMap::new();
    let mut ticker = tokio::time::interval(Duration::from_secs(config.poll_interval_secs.max(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                for path in &config.log_paths {
                    match ship_file(&pool, &node_id, path, config.batch_size, &mut offsets).await {
                        Ok(shipped) if shipped > 0 => {
                            debug!(path = %path.display(), shipped, "shipped agent logs");
                        }
                        Ok(_) => {}
                        Err(error) => {
                            warn!(path = %path.display(), %error, "failed to ship agent logs");
                        }
                    }
                }
            }
            _ = cancel.cancelled() => break,
        }
    }
}

async fn ship_file(
    pool: &PgPool,
    node_id: &str,
    path: &Path,
    batch_size: usize,
    offsets: &mut HashMap<PathBuf, u64>,
) -> anyhow::Result<usize> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let length = file.metadata()?.len();
    let offset = offsets.entry(path.to_path_buf()).or_insert(length);
    if *offset > length {
        *offset = 0;
    }
    file.seek(SeekFrom::Start(*offset))?;

    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    let new_offset = file.stream_position()?;

    let entries: Vec<_> = contents.lines().filter_map(parse_line).collect();
    if entries.is_empty() {
        *offset = new_offset;
        return Ok(0);
    }

    for batch in entries.chunks(batch_size.max(1)) {
        let mut transaction = pool.begin().await?;
        for (level, message) in batch {
            sqlx::query("INSERT INTO fleet_logs (node_id, log_level, message) VALUES ($1, $2, $3)")
                .bind(node_id)
                .bind(level)
                .bind(message)
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
    }
    *offset = new_offset;
    Ok(entries.len())
}

fn parse_line(line: &str) -> Option<(&'static str, &str)> {
    if line.contains(" ERROR ") || line.starts_with("ERROR ") {
        Some(("ERROR", line))
    } else if line.contains(" WARN ") || line.starts_with("WARN ") {
        Some(("WARN", line))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::parse_line;

    #[test]
    fn filters_log_levels() {
        assert_eq!(
            parse_line("2026-07-28T00:00:00Z ERROR failed").unwrap().0,
            "ERROR"
        );
        assert_eq!(parse_line("WARN slow").unwrap().0, "WARN");
        assert!(parse_line("2026-07-28T00:00:00Z INFO ready").is_none());
    }
}
