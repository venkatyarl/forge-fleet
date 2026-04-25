use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use ff_core::config::TelegramTransportConfig;
use ff_db::{OperationalStore, PgPool, queries::TaskRow};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::watch;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::message::{Channel, IncomingMessage, MessageButton, MessageMediaKind, OutgoingMessage};
use crate::router::MessageRouter;
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
                // Use Debug format so the full anyhow cause chain (including
                // the underlying Telegram API status + body) is visible.
                warn!(error = ?error, "telegram update processing failed");
                self.set_runtime_status(Self::STATUS_LAST_ERROR_KEY, format!("{error:?}"))
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
        let _route = self.router.route(&incoming);
        let media_summary = self
            .persist_media_ingest(&incoming, downloaded_media)
            .await?;

        // Try to get Postgres pool for brain routing
        let pool = self.store.pg_pool();

        // ── Handle slash commands (legacy + brain thread commands) ────────
        if let Some(command) = incoming.parse_command(&['/', '!']) {
            let response = match command.command.as_str() {
                // Legacy commands
                "start" => {
                    format!(
                        "ForgeFleet Telegram transport online on node {}.",
                        self.node_name
                    )
                }
                "help" => "Commands:\n\
                     /start - Check bot status\n\
                     /help - This help\n\
                     /ping - Heartbeat\n\
                     /status - Node info\n\
                     /threads - List threads\n\
                     /new <slug> [title] - New thread\n\
                     /switch <slug> - Switch thread\n\
                     /archive <slug> - Archive thread\n\
                     /where - Current thread"
                    .to_string(),
                "ping" => "pong".to_string(),
                "status" => format!("node: {}\nroute: {:?}", self.node_name, _route.target),

                // ── Brain thread commands ──────────────────────────────
                "threads" => {
                    if let Some(pool) = pool {
                        self.handle_threads_command(pool, &incoming).await?;
                        return Ok(());
                    }
                    "Brain not available (no Postgres)".to_string()
                }
                "new" => {
                    if let Some(pool) = pool {
                        return self
                            .handle_new_thread_command(pool, &incoming, &command.args)
                            .await;
                    }
                    "Brain not available (no Postgres)".to_string()
                }
                "switch" => {
                    if let Some(pool) = pool {
                        return self
                            .handle_switch_command(pool, &incoming, &command.args)
                            .await;
                    }
                    "Brain not available (no Postgres)".to_string()
                }
                "archive" => {
                    if let Some(pool) = pool {
                        return self
                            .handle_archive_command(pool, &incoming, &command.args)
                            .await;
                    }
                    "Brain not available (no Postgres)".to_string()
                }
                "where" => {
                    if let Some(pool) = pool {
                        return self.handle_where_command(pool, &incoming).await;
                    }
                    "Brain not available (no Postgres)".to_string()
                }
                "callback" => {
                    if let Some(pool) = pool {
                        return self.handle_callback(pool, &incoming, &command.args).await;
                    }
                    "Brain not available (no Postgres)".to_string()
                }
                other => format!("Unknown command: /{other}"),
            };

            let full = if let Some(summary) = media_summary.as_ref() {
                format!("{response}\n{}", build_media_ack(summary))
            } else {
                response
            };

            let mut outgoing =
                OutgoingMessage::text(Channel::Telegram, incoming.chat_id.clone(), full);
            outgoing.reply_to = incoming.external_id.clone();
            outgoing.thread_id = incoming.thread_id.clone();

            self.client
                .send_message(&outgoing)
                .await
                .context("failed to send telegram command response")?;
            return Ok(());
        }

        // ── Regular message → brain chat with LLM ────────────────────────
        if let Some(pool) = pool {
            return self
                .handle_brain_message(pool, &incoming, media_summary.as_ref())
                .await;
        }

        // Fallback: no Postgres available, echo stub
        let text = incoming.text.as_deref().unwrap_or("(no text)").trim();
        let preview = if text.chars().count() > 120 {
            let mut clipped = text.chars().take(120).collect::<String>();
            clipped.push_str("...");
            clipped
        } else {
            text.to_string()
        };
        let fallback = format!("(brain offline) echo on {}: {preview}", self.node_name);

        let mut outgoing =
            OutgoingMessage::text(Channel::Telegram, incoming.chat_id.clone(), fallback);
        outgoing.reply_to = incoming.external_id.clone();
        outgoing.thread_id = incoming.thread_id.clone();
        self.client
            .send_message(&outgoing)
            .await
            .context("failed to send telegram fallback response")?;
        Ok(())
    }

    // ── Brain thread command handlers ────────────────────────────────────

    async fn ensure_brain_user_and_thread(
        &self,
        pool: &PgPool,
        incoming: &IncomingMessage,
    ) -> Result<(uuid::Uuid, uuid::Uuid, String)> {
        let chat_id = &incoming.chat_id;
        let display_name = incoming
            .from_username
            .as_deref()
            .unwrap_or(&incoming.from_user_id);

        // Resolve or create user
        let user_id = match ff_db::pg_resolve_channel_user(pool, "telegram", chat_id).await? {
            Some(uid) => uid,
            None => {
                let uid = ff_db::pg_create_brain_user(pool, display_name, Some(display_name))
                    .await
                    .context("failed to create brain user for telegram chat")?;
                ff_db::pg_upsert_channel_identity(pool, "telegram", chat_id, uid)
                    .await
                    .context("failed to upsert telegram channel identity")?;
                uid
            }
        };

        // Resolve or create attached thread
        let (thread_id, thread_slug) =
            match ff_db::pg_get_attached_thread(pool, "telegram", chat_id).await? {
                Some(tid) => {
                    let slug = ff_db::pg_get_brain_thread_by_id(pool, tid)
                        .await?
                        .map(|t| t.slug)
                        .unwrap_or_else(|| "inbox".to_string());
                    (tid, slug)
                }
                None => {
                    let tid =
                        ff_db::pg_create_brain_thread(pool, user_id, "inbox", Some("Inbox"), None)
                            .await
                            .context("failed to create default inbox thread")?;
                    ff_db::pg_attach_thread(pool, "telegram", chat_id, user_id, tid)
                        .await
                        .context("failed to attach default inbox thread")?;
                    (tid, "inbox".to_string())
                }
            };

        Ok((user_id, thread_id, thread_slug))
    }

    async fn handle_threads_command(
        &self,
        pool: &PgPool,
        incoming: &IncomingMessage,
    ) -> Result<()> {
        let (user_id, current_thread_id, _) =
            self.ensure_brain_user_and_thread(pool, incoming).await?;
        let threads = ff_db::pg_list_brain_threads(pool, user_id).await?;

        if threads.is_empty() {
            let mut outgoing = OutgoingMessage::text(
                Channel::Telegram,
                incoming.chat_id.clone(),
                "No threads yet. Create one with /new <slug>".to_string(),
            );
            outgoing.reply_to = incoming.external_id.clone();
            self.client.send_message(&outgoing).await?;
            return Ok(());
        }

        let mut text_lines = vec!["Your threads:".to_string()];
        let mut button_rows: Vec<Vec<MessageButton>> = Vec::new();

        for thread in &threads {
            let marker = if thread.id == current_thread_id {
                " (current)"
            } else {
                ""
            };
            let title = thread.title.as_deref().unwrap_or(&thread.slug);
            text_lines.push(format!("- {}{marker}", title));
            button_rows.push(vec![MessageButton::callback(
                format!("{}{marker}", title),
                format!("switch:{}", thread.slug),
            )]);
        }

        let mut outgoing = OutgoingMessage::text(
            Channel::Telegram,
            incoming.chat_id.clone(),
            text_lines.join("\n"),
        );
        outgoing.reply_to = incoming.external_id.clone();
        outgoing.buttons = button_rows;
        self.client.send_message(&outgoing).await?;
        Ok(())
    }

    async fn handle_new_thread_command(
        &self,
        pool: &PgPool,
        incoming: &IncomingMessage,
        args: &[String],
    ) -> Result<()> {
        let slug = args.first().map(|s| s.as_str()).unwrap_or("untitled");
        let title = if args.len() > 1 {
            Some(args[1..].join(" "))
        } else {
            None
        };

        let (user_id, _, _) = self.ensure_brain_user_and_thread(pool, incoming).await?;
        let tid = ff_db::pg_create_brain_thread(pool, user_id, slug, title.as_deref(), None)
            .await
            .context("failed to create brain thread")?;

        // Auto-switch to the new thread
        ff_db::pg_attach_thread(pool, "telegram", &incoming.chat_id, user_id, tid).await?;

        let reply = format!(
            "Created and switched to thread '{}'{}",
            slug,
            title
                .as_ref()
                .map(|t| format!(" ({})", t))
                .unwrap_or_default()
        );
        let mut outgoing =
            OutgoingMessage::text(Channel::Telegram, incoming.chat_id.clone(), reply);
        outgoing.reply_to = incoming.external_id.clone();
        self.client.send_message(&outgoing).await?;
        Ok(())
    }

    async fn handle_switch_command(
        &self,
        pool: &PgPool,
        incoming: &IncomingMessage,
        args: &[String],
    ) -> Result<()> {
        let slug = match args.first() {
            Some(s) => s.as_str(),
            None => {
                let mut outgoing = OutgoingMessage::text(
                    Channel::Telegram,
                    incoming.chat_id.clone(),
                    "Usage: /switch <thread-slug>".to_string(),
                );
                outgoing.reply_to = incoming.external_id.clone();
                self.client.send_message(&outgoing).await?;
                return Ok(());
            }
        };

        let (user_id, _, _) = self.ensure_brain_user_and_thread(pool, incoming).await?;
        let thread = ff_db::pg_get_brain_thread(pool, user_id, slug).await?;

        let reply = match thread {
            Some(t) => {
                ff_db::pg_attach_thread(pool, "telegram", &incoming.chat_id, user_id, t.id).await?;
                format!(
                    "Switched to thread '{}'",
                    t.title.as_deref().unwrap_or(&t.slug)
                )
            }
            None => format!("Thread '{slug}' not found. Use /threads to list."),
        };

        let mut outgoing =
            OutgoingMessage::text(Channel::Telegram, incoming.chat_id.clone(), reply);
        outgoing.reply_to = incoming.external_id.clone();
        self.client.send_message(&outgoing).await?;
        Ok(())
    }

    async fn handle_archive_command(
        &self,
        pool: &PgPool,
        incoming: &IncomingMessage,
        args: &[String],
    ) -> Result<()> {
        let slug = match args.first() {
            Some(s) => s.as_str(),
            None => {
                let mut outgoing = OutgoingMessage::text(
                    Channel::Telegram,
                    incoming.chat_id.clone(),
                    "Usage: /archive <thread-slug>".to_string(),
                );
                outgoing.reply_to = incoming.external_id.clone();
                self.client.send_message(&outgoing).await?;
                return Ok(());
            }
        };

        let (user_id, _, _) = self.ensure_brain_user_and_thread(pool, incoming).await?;
        let thread = ff_db::pg_get_brain_thread(pool, user_id, slug).await?;

        let reply = match thread {
            Some(t) => {
                ff_db::pg_archive_brain_thread(pool, t.id).await?;
                format!("Archived thread '{}'", t.title.as_deref().unwrap_or(slug))
            }
            None => format!("Thread '{slug}' not found."),
        };

        let mut outgoing =
            OutgoingMessage::text(Channel::Telegram, incoming.chat_id.clone(), reply);
        outgoing.reply_to = incoming.external_id.clone();
        self.client.send_message(&outgoing).await?;
        Ok(())
    }

    async fn handle_where_command(&self, pool: &PgPool, incoming: &IncomingMessage) -> Result<()> {
        let (_, _, thread_slug) = self.ensure_brain_user_and_thread(pool, incoming).await?;
        let reply = format!("You are in thread: {thread_slug}");
        let mut outgoing =
            OutgoingMessage::text(Channel::Telegram, incoming.chat_id.clone(), reply);
        outgoing.reply_to = incoming.external_id.clone();
        self.client.send_message(&outgoing).await?;
        Ok(())
    }

    async fn handle_callback(
        &self,
        pool: &PgPool,
        incoming: &IncomingMessage,
        args: &[String],
    ) -> Result<()> {
        let data = args.first().map(|s| s.as_str()).unwrap_or("");

        // Answer the callback query to dismiss the loading spinner.
        // The external_id for a callback is the callback_query_id.
        if let Some(cq_id) = incoming.external_id.as_deref() {
            let _ = self.client.answer_callback_query(cq_id, None).await;
        }

        if let Some(slug) = data.strip_prefix("switch:") {
            return self
                .handle_switch_command(pool, incoming, &[slug.to_string()])
                .await;
        }

        let reply = format!("Unknown callback: {data}");
        let mut outgoing =
            OutgoingMessage::text(Channel::Telegram, incoming.chat_id.clone(), reply);
        outgoing.reply_to = None; // callback external_id is not a message_id
        self.client.send_message(&outgoing).await?;
        Ok(())
    }

    // ── Brain chat with LLM ─────────────────────────────────────────────

    async fn handle_brain_message(
        &self,
        pool: &PgPool,
        incoming: &IncomingMessage,
        media_summary: Option<&MediaIngestSummary>,
    ) -> Result<()> {
        let (user_id, thread_id, thread_slug) =
            self.ensure_brain_user_and_thread(pool, incoming).await?;

        let user_text = incoming.text.as_deref().unwrap_or("").trim();
        let content = if let Some(summary) = media_summary {
            format!("{user_text}\n[media: {} file(s)]", summary.media.len())
        } else {
            user_text.to_string()
        };

        if content.is_empty() {
            return Ok(());
        }

        // Insert user message
        let ext_id = incoming
            .external_id
            .as_deref()
            .unwrap_or("telegram-unknown");
        ff_db::pg_insert_brain_message(
            pool, thread_id, user_id, "telegram", ext_id, "user", &content, None,
        )
        .await
        .context("failed to insert user brain message")?;
        let _ = ff_db::pg_touch_brain_thread(pool, thread_id).await;

        // Build conversation history for LLM
        let display_name = incoming.from_username.as_deref().unwrap_or("user");

        let assistant_text = match call_fleet_llm(pool, &thread_slug, display_name, thread_id).await
        {
            Ok(text) => text,
            Err(e) => {
                warn!(error = %e, "LLM call failed, returning error message");
                format!("(LLM error: {e})")
            }
        };

        // Insert assistant response
        ff_db::pg_insert_brain_message(
            pool,
            thread_id,
            user_id,
            "telegram",
            "brain",
            "assistant",
            &assistant_text,
            None,
        )
        .await
        .context("failed to insert assistant brain message")?;
        let _ = ff_db::pg_touch_brain_thread(pool, thread_id).await;

        // Send reply via Telegram
        let mut outgoing =
            OutgoingMessage::text(Channel::Telegram, incoming.chat_id.clone(), assistant_text);
        outgoing.reply_to = incoming.external_id.clone();
        outgoing.thread_id = incoming.thread_id.clone();
        self.client
            .send_message(&outgoing)
            .await
            .context("failed to send telegram brain response")?;
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

// ── Fleet LLM call ──────────────────────────────────────────────────────

const DEFAULT_LLM_ENDPOINT: &str = "http://127.0.0.1:55001/v1/chat/completions";
const LLM_HISTORY_LIMIT: i64 = 10;
const LLM_REQUEST_TIMEOUT_SECS: u64 = 120;

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatCompletionChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionChoice {
    message: ChatCompletionMessage,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionMessage {
    content: Option<String>,
}

async fn call_fleet_llm(
    pool: &PgPool,
    thread_slug: &str,
    display_name: &str,
    thread_id: uuid::Uuid,
) -> Result<String> {
    // Build conversation messages
    let mut messages = Vec::new();

    // System prompt
    let system_prompt = format!(
        "You are ForgeFleet's Virtual Brain assistant. \
         User: {display_name}. Thread: {thread_slug}. \
         Answer concisely and helpfully. Use markdown formatting sparingly \
         (Telegram supports basic markdown). Keep replies under 2000 chars."
    );
    messages.push(json!({"role": "system", "content": system_prompt}));

    // Load recent history (returned newest-first, so reverse)
    let mut history = ff_db::pg_list_brain_messages(pool, thread_id, LLM_HISTORY_LIMIT)
        .await
        .unwrap_or_default();
    history.reverse(); // oldest first

    for msg in &history {
        let role = match msg.role.as_str() {
            "user" => "user",
            "assistant" => "assistant",
            _ => continue,
        };
        messages.push(json!({"role": role, "content": msg.content}));
    }

    // Determine LLM endpoint: try fleet_model_deployments for an active
    // deployment, otherwise fall back to the default.
    let endpoint = resolve_llm_endpoint(pool).await;

    debug!(
        endpoint = %endpoint,
        history_msgs = history.len(),
        "sending brain chat request to fleet LLM"
    );

    let request_body = json!({
        "model": "default",
        "messages": messages,
        "max_tokens": 1024,
        "temperature": 0.7,
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(LLM_REQUEST_TIMEOUT_SECS))
        .build()
        .context("failed to build HTTP client for LLM")?;

    let resp = client
        .post(&endpoint)
        .json(&request_body)
        .send()
        .await
        .context("LLM HTTP request failed")?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "LLM returned HTTP {status}: {}",
            body.chars().take(200).collect::<String>()
        );
    }

    let body = resp
        .text()
        .await
        .context("failed to read LLM response body")?;
    let parsed: ChatCompletionResponse = serde_json::from_str(&body)
        .map_err(|e| anyhow::anyhow!("failed to parse LLM response: {e}"))?;

    let content = parsed
        .choices
        .first()
        .and_then(|c| c.message.content.clone())
        .unwrap_or_else(|| "(empty LLM response)".to_string());

    Ok(content)
}

/// Try to find an active LLM deployment from the fleet DB, otherwise
/// fall back to the hardcoded default endpoint.
async fn resolve_llm_endpoint(pool: &PgPool) -> String {
    // List all deployments and find a healthy one, preferring local node
    let deployments = match ff_db::pg_list_deployments(pool, None).await {
        Ok(d) => d,
        Err(_) => return DEFAULT_LLM_ENDPOINT.to_string(),
    };

    // Find first deployment with healthy status
    let deployment = deployments
        .iter()
        .find(|d| d.health_status == "healthy" || d.health_status == "ok");

    let deployment = match deployment {
        Some(d) => d,
        None => {
            // Fall back to any deployment
            match deployments.first() {
                Some(d) => d,
                None => return DEFAULT_LLM_ENDPOINT.to_string(),
            }
        }
    };

    // Resolve node_name → IP via fleet_nodes
    let ip = match ff_db::pg_list_nodes(pool).await {
        Ok(nodes) => nodes
            .iter()
            .find(|n| n.name == deployment.node_name)
            .map(|n| n.ip.clone())
            .unwrap_or_else(|| "127.0.0.1".to_string()),
        Err(_) => "127.0.0.1".to_string(),
    };

    format!("http://{}:{}/v1/chat/completions", ip, deployment.port)
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
