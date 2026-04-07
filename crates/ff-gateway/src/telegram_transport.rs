use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use ff_core::config::TelegramTransportConfig;
use ff_db::{OperationalStore, queries::TaskRow};
use serde::Serialize;
use serde_json::json;
use tokio::sync::watch;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::message::{Channel, IncomingMessage, MessageMediaKind, OutgoingMessage};
use crate::router::{MessageRouter, RouteTarget};
use crate::telegram::{TelegramClient, TelegramUpdate, log_telegram_error};

const DEFAULT_DEDUPE_WINDOW: usize = 2048;

#[derive(Debug)]
pub struct TelegramPollingTransport {
    client: TelegramClient,
    config: TelegramTransportConfig,
    store: OperationalStore,
    router: Arc<MessageRouter>,
    node_name: String,
    poll_interval: Duration,
    poll_timeout_secs: u64,
    next_offset: Option<i64>,
    dedupe: UpdateDedupe,
    media_download_dir: Option<PathBuf>,
    max_media_size_bytes: u64,
    allowed_media_mime_types: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct DownloadedMediaIngest {
    #[serde(skip_serializing_if = "Option::is_none")]
    db_id: Option<i64>,
    media_kind: String,
    local_path: String,
    mime_type: String,
    size_bytes: u64,
}

#[derive(Debug, Clone)]
struct MediaIngestSummary {
    ingest_task_id: String,
    media: Vec<DownloadedMediaIngest>,
}

impl TelegramPollingTransport {
    const STATUS_ENABLED_KEY: &'static str = "transport.telegram.enabled";
    const STATUS_RUNNING_KEY: &'static str = "transport.telegram.running";
    const STATUS_STARTED_AT_KEY: &'static str = "transport.telegram.started_at";
    const STATUS_LAST_UPDATE_ID_KEY: &'static str = "transport.telegram.last_update_id";
    const STATUS_LAST_MESSAGE_AT_KEY: &'static str = "transport.telegram.last_message_at";
    const STATUS_LAST_ERROR_KEY: &'static str = "transport.telegram.last_error";

    pub fn new(
        config: TelegramTransportConfig,
        store: OperationalStore,
        node_name: String,
        router: MessageRouter,
    ) -> Result<Self> {
        let token = config
            .resolve_bot_token()
            .context("telegram transport enabled but bot token is missing")?;

        let client = TelegramClient::new(token)?;
        let poll_interval = Duration::from_secs(config.polling_interval_secs.max(1));
        let poll_timeout_secs = config.polling_timeout_secs.max(1);
        let max_media_size_bytes = config.media_max_file_size_bytes.max(1);
        let allowed_media_mime_types =
            normalize_allowed_media_mimes(&config.media_allowed_mime_types);

        let media_download_dir = if let Some(raw) = config.media_download_dir.as_deref() {
            let path = PathBuf::from(raw);
            std::fs::create_dir_all(&path).with_context(|| {
                format!(
                    "failed to create telegram media download dir {}",
                    path.display()
                )
            })?;
            Some(path)
        } else {
            None
        };

        Ok(Self {
            client,
            config,
            store,
            router: Arc::new(router),
            node_name,
            poll_interval,
            poll_timeout_secs,
            next_offset: None,
            dedupe: UpdateDedupe::new(DEFAULT_DEDUPE_WINDOW),
            media_download_dir,
            max_media_size_bytes,
            allowed_media_mime_types,
        })
    }

    pub async fn run(mut self, mut shutdown_rx: watch::Receiver<bool>) -> Result<()> {
        info!(
            node = %self.node_name,
            allowed_chat_ids = ?self.config.allowed_chat_ids,
            poll_interval_secs = self.poll_interval.as_secs(),
            poll_timeout_secs = self.poll_timeout_secs,
            media_download_dir = ?self.media_download_dir,
            media_max_file_size_bytes = self.max_media_size_bytes,
            allowed_media_mime_types = ?self.allowed_media_mime_types,
            "telegram polling transport started"
        );

        self.set_runtime_status(Self::STATUS_ENABLED_KEY, self.config.enabled.to_string())
            .await;
        self.set_runtime_status(Self::STATUS_RUNNING_KEY, "true".to_string())
            .await;
        self.set_runtime_status(Self::STATUS_STARTED_AT_KEY, Utc::now().to_rfc3339())
            .await;
        self.set_runtime_status(Self::STATUS_LAST_ERROR_KEY, String::new())
            .await;

        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        info!(node=%self.node_name, "telegram polling transport stopping");
                        break;
                    }
                }
                updates = self.client.get_updates(self.next_offset, Some(self.poll_timeout_secs)) => {
                    match updates {
                        Ok(updates) => self.handle_updates(updates).await,
                        Err(error) => {
                            log_telegram_error("get_updates", &error);
                            self.set_runtime_status(Self::STATUS_LAST_ERROR_KEY, error.to_string()).await;
                            tokio::time::sleep(self.poll_interval).await;
                        }
                    }
                }
            }
        }

        self.set_runtime_status(Self::STATUS_RUNNING_KEY, "false".to_string())
            .await;

        Ok(())
    }

    async fn handle_updates(&mut self, updates: Vec<TelegramUpdate>) {
        if updates.is_empty() {
            tokio::time::sleep(self.poll_interval).await;
            return;
        }

        for update in updates {
            let update_id = update.update_id;
            self.next_offset = Some(update_id.saturating_add(1));
            self.set_runtime_status(Self::STATUS_LAST_UPDATE_ID_KEY, update_id.to_string())
                .await;

            if self.dedupe.is_duplicate(update_id) {
                debug!(update_id, "skipping duplicate telegram update");
                continue;
            }
            self.dedupe.remember(update_id);

            let Some(mut incoming) = self.client.normalize_update(update) else {
                continue;
            };

            if !is_allowed_chat(&self.config, &incoming.chat_id) {
                warn!(chat_id = %incoming.chat_id, "ignoring telegram update from unauthorized chat");
                continue;
            }

            self.set_runtime_status(
                Self::STATUS_LAST_MESSAGE_AT_KEY,
                incoming.received_at.to_rfc3339(),
            )
            .await;
            self.set_runtime_status(Self::STATUS_LAST_ERROR_KEY, String::new())
                .await;

            let downloaded_media = match self.download_media_if_needed(&mut incoming).await {
                Ok(downloaded) => downloaded,
                Err(error) => {
                    warn!(error = %error, "failed to download telegram media attachments");
                    self.set_runtime_status(Self::STATUS_LAST_ERROR_KEY, error.to_string())
                        .await;
                    Vec::new()
                }
            };

            if let Err(error) = self.process_message(incoming, downloaded_media).await {
                warn!(error = %error, "telegram update processing failed");
                self.set_runtime_status(Self::STATUS_LAST_ERROR_KEY, error.to_string())
                    .await;
            }
        }
    }

    async fn download_media_if_needed(
        &self,
        incoming: &mut IncomingMessage,
    ) -> Result<Vec<DownloadedMediaIngest>> {
        let Some(base_dir) = self.media_download_dir.as_ref() else {
            return Ok(Vec::new());
        };

        let update_id = incoming
            .metadata
            .get("telegram_update_id")
            .and_then(|value| value.as_i64())
            .unwrap_or_default();
        let message_id = incoming.external_id.as_deref().unwrap_or("unknown-message");

        let mut downloaded = Vec::new();

        for (idx, media) in incoming.media.iter_mut().enumerate() {
            let Some(file_id) = media.file_id.clone() else {
                continue;
            };

            let file = self.client.get_file(&file_id).await?;
            let Some(file_path) = file.file_path else {
                continue;
            };

            let declared_size = media.size_bytes.or(file.file_size);
            if let Some(size) = declared_size
                && size > self.max_media_size_bytes
            {
                media.metadata.insert(
                    "ingest_skipped_reason".to_string(),
                    json!("file_size_exceeds_limit"),
                );
                continue;
            }

            let Some(mime_type) = infer_media_mime(
                media.mime_type.as_deref(),
                media.file_name.as_deref(),
                &file_path,
            ) else {
                media.metadata.insert(
                    "ingest_skipped_reason".to_string(),
                    json!("mime_type_unknown"),
                );
                continue;
            };

            if !is_media_mime_allowed(&self.allowed_media_mime_types, &mime_type) {
                media.metadata.insert(
                    "ingest_skipped_reason".to_string(),
                    json!("mime_type_not_allowed"),
                );
                continue;
            }

            let bytes = self.client.download_file_bytes(&file_path).await?;
            let downloaded_size = bytes.len() as u64;
            if downloaded_size > self.max_media_size_bytes {
                media.metadata.insert(
                    "ingest_skipped_reason".to_string(),
                    json!("file_size_exceeds_limit"),
                );
                continue;
            }

            let output =
                build_media_download_path(base_dir, update_id, message_id, idx, &file_path);
            tokio::fs::write(&output, &bytes).await.with_context(|| {
                format!("failed to write telegram media file {}", output.display())
            })?;

            let local_path = output.to_string_lossy().to_string();
            media.url = Some(format!("file://{}", output.display()));
            media
                .metadata
                .insert("download_path".to_string(), json!(local_path.clone()));
            media
                .metadata
                .insert("downloaded_bytes".to_string(), json!(downloaded_size));
            media
                .metadata
                .insert("telegram_file_id".to_string(), json!(file_id));
            media
                .metadata
                .insert("telegram_file_path".to_string(), json!(file_path));
            media
                .metadata
                .insert("detected_mime_type".to_string(), json!(mime_type.clone()));
            media.mime_type = Some(mime_type.clone());
            media.size_bytes = Some(downloaded_size);

            downloaded.push(DownloadedMediaIngest {
                db_id: None,
                media_kind: media_kind_label(&media.kind).to_string(),
                local_path,
                mime_type,
                size_bytes: downloaded_size,
            });
        }

        Ok(downloaded)
    }

    async fn process_message(
        &self,
        incoming: IncomingMessage,
        downloaded_media: Vec<DownloadedMediaIngest>,
    ) -> Result<()> {
        let route = self.router.route(&incoming);
        let media_summary = self
            .persist_media_ingest(&incoming, downloaded_media)
            .await?;
        let response_text = build_response_text(
            &incoming,
            route.target,
            &self.node_name,
            media_summary.as_ref(),
        );

        let mut outgoing =
            OutgoingMessage::text(Channel::Telegram, incoming.chat_id.clone(), response_text);
        outgoing.reply_to = incoming.external_id.clone();
        outgoing.thread_id = incoming.thread_id.clone();

        self.client
            .send_message(&outgoing)
            .await
            .context("failed to send telegram response")?;

        Ok(())
    }

    async fn persist_media_ingest(
        &self,
        incoming: &IncomingMessage,
        media: Vec<DownloadedMediaIngest>,
    ) -> Result<Option<MediaIngestSummary>> {
        if media.is_empty() {
            return Ok(None);
        }

        let chat_id = incoming.chat_id.clone();
        let message_id = incoming
            .external_id
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let node_name = self.node_name.clone();

        let mut persisted_media = Vec::with_capacity(media.len());
        for item in media {
            let row_id = self
                .store
                .insert_telegram_media_ingest(
                    &chat_id,
                    &message_id,
                    &item.media_kind,
                    &item.local_path,
                    Some(&item.mime_type),
                    Some(item.size_bytes),
                )
                .await
                .context("failed to insert telegram media ingest row")?;

            let mut item = item;
            item.db_id = Some(row_id);
            persisted_media.push(item);
        }

        let ingest_task_id = Uuid::new_v4().to_string();
        let payload = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "media": persisted_media,
        })
        .to_string();

        let task = TaskRow {
            id: ingest_task_id.clone(),
            kind: "telegram_media_ingest".to_string(),
            payload_json: payload.clone(),
            status: "pending".to_string(),
            assigned_node: Some(node_name.clone()),
            priority: 20,
            created_at: Utc::now().to_rfc3339(),
            started_at: None,
            completed_at: None,
        };

        self.store
            .insert_task(&task)
            .await
            .context("failed to insert telegram media ingest task")?;

        self.store
            .audit_log(
                "telegram_media_ingest_stored",
                "telegram_transport",
                Some(&ingest_task_id),
                &payload,
                Some(&node_name),
            )
            .await
            .context("failed to append telegram media ingest audit event")?;

        Ok(Some(MediaIngestSummary {
            ingest_task_id,
            media: persisted_media,
        }))
    }

    async fn set_runtime_status(&self, key: &str, value: String) {
        let status_key = key.to_string();
        if let Err(error) = self.store.config_set(&status_key, &value).await {
            warn!(status_key = %status_key, error = %error, "failed to write telegram runtime status");
        }
    }
}

fn normalize_allowed_media_mimes(raw: &[String]) -> Vec<String> {
    let mut normalized = Vec::new();

    for item in raw {
        let mime = item.trim().to_ascii_lowercase();
        if mime.is_empty() {
            continue;
        }
        if !normalized.contains(&mime) {
            normalized.push(mime);
        }
    }

    if normalized.is_empty() {
        normalized.push("image/*".to_string());
        normalized.push("video/*".to_string());
    }

    normalized
}

fn infer_media_mime(
    declared: Option<&str>,
    file_name: Option<&str>,
    file_path: &str,
) -> Option<String> {
    if let Some(mime) = declared.map(str::trim).filter(|mime| !mime.is_empty()) {
        return Some(mime.to_ascii_lowercase());
    }

    if let Some(name) = file_name
        && let Some(mime) = mime_guess::from_path(name).first_raw()
    {
        return Some(mime.to_ascii_lowercase());
    }

    mime_guess::from_path(file_path)
        .first_raw()
        .map(|mime| mime.to_ascii_lowercase())
}

fn is_media_mime_allowed(allowlist: &[String], mime_type: &str) -> bool {
    if allowlist.is_empty() {
        return true;
    }

    let mime = mime_type.trim().to_ascii_lowercase();
    allowlist.iter().any(|allowed| {
        if allowed == "*" || allowed == "*/*" {
            return true;
        }

        if allowed == &mime {
            return true;
        }

        if let Some(prefix) = allowed.strip_suffix("/*") {
            return mime.starts_with(&format!("{prefix}/"));
        }

        false
    })
}

fn build_media_download_path(
    base_dir: &Path,
    update_id: i64,
    message_id: &str,
    index: usize,
    file_path: &str,
) -> PathBuf {
    let file_name = Path::new(file_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("telegram-media.bin");

    let safe_file_name = file_name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();

    let safe_message_id = message_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();

    let prefix = format!("upd{update_id}_msg{safe_message_id}_{index}");
    base_dir.join(format!("{prefix}_{safe_file_name}"))
}

fn media_kind_label(kind: &MessageMediaKind) -> &'static str {
    match kind {
        MessageMediaKind::Image => "photo",
        MessageMediaKind::Video => "video",
        MessageMediaKind::Audio => "audio",
        MessageMediaKind::Document => "document",
        MessageMediaKind::Other => "other",
    }
}

fn is_allowed_chat(config: &TelegramTransportConfig, chat_id: &str) -> bool {
    if config.allowed_chat_ids.is_empty() {
        return true;
    }

    chat_id
        .trim()
        .parse::<i64>()
        .ok()
        .map(|id| config.is_chat_allowed(id))
        .unwrap_or(false)
}

fn build_media_ack(summary: &MediaIngestSummary) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "media_ingest_task: {} ({} file{})",
        summary.ingest_task_id,
        summary.media.len(),
        if summary.media.len() == 1 { "" } else { "s" }
    ));

    for item in &summary.media {
        lines.push(format!(
            "- {}: {} ({} bytes, {})",
            item.media_kind, item.local_path, item.size_bytes, item.mime_type
        ));
    }

    lines.join("\n")
}

fn build_response_text(
    incoming: &IncomingMessage,
    target: RouteTarget,
    node_name: &str,
    media_summary: Option<&MediaIngestSummary>,
) -> String {
    if let Some(command) = incoming.parse_command(&['/', '!']) {
        let base = match command.command.as_str() {
            "start" => format!("ForgeFleet Telegram transport online on node {node_name}."),
            "help" => "Commands: /start, /help, /ping, /status".to_string(),
            "ping" => "pong ✅".to_string(),
            "status" => format!("node: {node_name}\nroute: {:?}", target),
            other => format!("received /{other} on {node_name} (route: {:?})", target),
        };

        if let Some(summary) = media_summary {
            return format!("{base}\n{}", build_media_ack(summary));
        }

        return base;
    }

    let text = incoming.text.as_deref().unwrap_or("(no text)").trim();
    let preview = if text.chars().count() > 120 {
        let mut clipped = text.chars().take(120).collect::<String>();
        clipped.push('…');
        clipped
    } else {
        text.to_string()
    };

    let base = format!("routed as {:?} on {node_name}: {preview}", target);

    if let Some(summary) = media_summary {
        return format!("{base}\n{}", build_media_ack(summary));
    }

    base
}

#[derive(Debug)]
struct UpdateDedupe {
    seen: HashSet<i64>,
    order: VecDeque<i64>,
    max_size: usize,
}

impl UpdateDedupe {
    fn new(max_size: usize) -> Self {
        Self {
            seen: HashSet::new(),
            order: VecDeque::new(),
            max_size: max_size.max(1),
        }
    }

    fn is_duplicate(&self, update_id: i64) -> bool {
        self.seen.contains(&update_id)
    }

    fn remember(&mut self, update_id: i64) {
        if !self.seen.insert(update_id) {
            return;
        }

        self.order.push_back(update_id);
        while self.order.len() > self.max_size {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupe_rejects_seen_update_ids() {
        let mut dedupe = UpdateDedupe::new(3);
        dedupe.remember(100);
        dedupe.remember(101);

        assert!(dedupe.is_duplicate(100));
        assert!(!dedupe.is_duplicate(102));

        dedupe.remember(102);
        dedupe.remember(103);
        assert!(!dedupe.is_duplicate(100));
    }

    #[test]
    fn chat_filter_blocks_unlisted_chat_ids() {
        let config = TelegramTransportConfig {
            enabled: true,
            allowed_chat_ids: vec![8496613333, 8622294597],
            ..Default::default()
        };

        assert!(is_allowed_chat(&config, "8496613333"));
        assert!(!is_allowed_chat(&config, "1111111111"));
        assert!(!is_allowed_chat(&config, "not-a-number"));
    }

    #[test]
    fn mime_allowlist_supports_wildcards_and_exact_values() {
        let allowlist =
            normalize_allowed_media_mimes(&["image/*".to_string(), "video/mp4".to_string()]);

        assert!(is_media_mime_allowed(&allowlist, "image/jpeg"));
        assert!(is_media_mime_allowed(&allowlist, "video/mp4"));
        assert!(!is_media_mime_allowed(&allowlist, "video/quicktime"));
        assert!(!is_media_mime_allowed(&allowlist, "application/pdf"));
    }

    #[test]
    fn media_download_path_construction_is_stable() {
        let path = build_media_download_path(
            std::path::Path::new("/tmp/forgefleet-telegram"),
            42,
            "88/99",
            1,
            "photos/file.JPG",
        );

        assert_eq!(
            path.to_string_lossy(),
            "/tmp/forgefleet-telegram/upd42_msg88_99_1_file.JPG"
        );
    }
}
