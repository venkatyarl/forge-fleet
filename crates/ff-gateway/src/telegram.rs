use std::collections::HashMap;

use anyhow::Context;
use chrono::{TimeZone, Utc};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use thiserror::Error;
use tracing::warn;

use crate::message::{
    Channel, IncomingMessage, MessageButton, MessageMedia, MessageMediaKind, OutgoingMessage,
    Reaction,
};

const TELEGRAM_API_BASE: &str = "https://api.telegram.org";

#[derive(Debug, Error)]
pub enum TelegramError {
    #[error("http request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("telegram api error ({status}): {message}")]
    Api { status: StatusCode, message: String },
    #[error("telegram returned malformed payload: {0}")]
    Parse(String),
}

#[derive(Debug, Clone)]
pub struct TelegramClient {
    token: String,
    api_base: String,
    http_client: reqwest::Client,
}

impl TelegramClient {
    pub fn new(token: impl Into<String>) -> anyhow::Result<Self> {
        let http_client = reqwest::Client::builder()
            .pool_idle_timeout(std::time::Duration::from_secs(30))
            .build()
            .context("failed to create telegram reqwest client")?;

        Ok(Self {
            token: token.into(),
            api_base: TELEGRAM_API_BASE.to_string(),
            http_client,
        })
    }

    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Self {
        self.api_base = api_base.into();
        self
    }

    pub async fn set_webhook(&self, webhook_url: &str) -> Result<(), TelegramError> {
        let payload = json!({
            "url": webhook_url,
            "allowed_updates": [
                "message",
                "edited_message",
                "callback_query",
                "message_reaction"
            ]
        });

        let _: Value = self.call("setWebhook", payload).await?;
        Ok(())
    }

    pub async fn delete_webhook(&self, drop_pending_updates: bool) -> Result<(), TelegramError> {
        let payload = json!({ "drop_pending_updates": drop_pending_updates });
        let _: Value = self.call("deleteWebhook", payload).await?;
        Ok(())
    }

    pub async fn get_updates(
        &self,
        offset: Option<i64>,
        timeout_secs: Option<u64>,
    ) -> Result<Vec<TelegramUpdate>, TelegramError> {
        let payload = json!({
            "offset": offset,
            "timeout": timeout_secs.unwrap_or(15),
            "allowed_updates": ["message", "edited_message", "callback_query", "message_reaction"]
        });

        self.call("getUpdates", payload).await
    }

    pub async fn send_message(&self, outgoing: &OutgoingMessage) -> Result<Value, TelegramError> {
        let request = build_outgoing_request(outgoing)?;
        self.call(request.method, request.payload).await
    }

    pub async fn send_photo(&self, outgoing: &OutgoingMessage) -> Result<Value, TelegramError> {
        let request = build_media_request_for_kind(outgoing, MessageMediaKind::Image)?;
        self.call(request.method, request.payload).await
    }

    pub async fn send_document(&self, outgoing: &OutgoingMessage) -> Result<Value, TelegramError> {
        let request = build_media_request_for_kind(outgoing, MessageMediaKind::Document)?;
        self.call(request.method, request.payload).await
    }

    pub async fn get_file(&self, file_id: &str) -> Result<TelegramFile, TelegramError> {
        let payload = json!({ "file_id": file_id });
        self.call("getFile", payload).await
    }

    pub fn file_download_url(&self, file_path: &str) -> String {
        telegram_file_download_url(&self.api_base, &self.token, file_path)
    }

    pub async fn download_file_bytes(&self, file_path: &str) -> Result<Vec<u8>, TelegramError> {
        let url = self.file_download_url(file_path);

        let response = self.http_client.get(url).send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(TelegramError::Api {
                status,
                message: "failed to download telegram file".to_string(),
            });
        }

        let bytes = response.bytes().await?;
        Ok(bytes.to_vec())
    }

    pub async fn send_reaction(
        &self,
        chat_id: &str,
        message_id: &str,
        reaction: &Reaction,
    ) -> Result<Value, TelegramError> {
        let message_id = message_id
            .parse::<i64>()
            .map_err(|_| TelegramError::Parse("message_id must be numeric".to_string()))?;

        let payload = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "reaction": [{"type": "emoji", "emoji": reaction.emoji}],
            "is_big": false
        });

        self.call("setMessageReaction", payload).await
    }

    pub fn normalize_update(&self, update: TelegramUpdate) -> Option<IncomingMessage> {
        normalize_update(update)
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        payload: Value,
    ) -> Result<T, TelegramError> {
        let endpoint = format!(
            "{}/bot{}/{}",
            self.api_base.trim_end_matches('/'),
            self.token,
            method
        );

        let response = self
            .http_client
            .post(endpoint)
            .json(&payload)
            .send()
            .await?;

        let status = response.status();
        let bytes = response.bytes().await?;

        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).to_string();
            return Err(TelegramError::Api {
                status,
                message: body,
            });
        }

        let api_response: TelegramApiResponse<T> = serde_json::from_slice(&bytes)
            .map_err(|error| TelegramError::Parse(error.to_string()))?;

        if api_response.ok {
            api_response
                .result
                .ok_or_else(|| TelegramError::Parse("missing result field".to_string()))
        } else {
            Err(TelegramError::Api {
                status,
                message: api_response
                    .description
                    .unwrap_or_else(|| "unknown telegram error".to_string()),
            })
        }
    }
}

fn inline_keyboard_json(button_rows: &[Vec<MessageButton>]) -> Option<Value> {
    if button_rows.is_empty() {
        return None;
    }

    let inline_keyboard = button_rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|button| {
                    json!({
                        "text": button.text,
                        "callback_data": button.callback_data,
                        "url": button.url,
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    Some(json!({ "inline_keyboard": inline_keyboard }))
}

struct TelegramOutgoingRequest {
    method: &'static str,
    payload: Value,
}

fn build_outgoing_request(
    outgoing: &OutgoingMessage,
) -> Result<TelegramOutgoingRequest, TelegramError> {
    if outgoing.media.is_empty() {
        // Build the payload as a map so optional fields (reply_to_message_id,
        // message_thread_id, reply_markup) can be OMITTED when None. Telegram
        // rejects `null` for these with 400 "object expected as reply markup".
        let mut payload = serde_json::Map::new();
        payload.insert("chat_id".into(), json!(outgoing.chat_id));
        payload.insert("text".into(), json!(outgoing.text.clone().unwrap_or_default()));
        if let Some(id) = outgoing.reply_to.as_deref().and_then(|id| id.parse::<i64>().ok()) {
            payload.insert("reply_to_message_id".into(), json!(id));
        }
        if let Some(id) = outgoing.thread_id.as_deref().and_then(|id| id.parse::<i64>().ok()) {
            payload.insert("message_thread_id".into(), json!(id));
        }
        if let Some(kb) = inline_keyboard_json(&outgoing.buttons) {
            payload.insert("reply_markup".into(), kb);
        }
        return Ok(TelegramOutgoingRequest {
            method: "sendMessage",
            payload: Value::Object(payload),
        });
    }

    let media = outgoing
        .media
        .first()
        .ok_or_else(|| TelegramError::Parse("expected at least one media item".to_string()))?;

    Ok(build_media_request(outgoing, media))
}

fn build_media_request_for_kind(
    outgoing: &OutgoingMessage,
    kind: MessageMediaKind,
) -> Result<TelegramOutgoingRequest, TelegramError> {
    let media = outgoing
        .media
        .iter()
        .find(|media| media.kind == kind)
        .ok_or_else(|| TelegramError::Parse(format!("no media item found for {:?}", kind)))?;

    Ok(build_media_request(outgoing, media))
}

fn build_media_request(
    outgoing: &OutgoingMessage,
    media: &MessageMedia,
) -> TelegramOutgoingRequest {
    let (method, media_key, media_value) = telegram_media_payload(media);

    // Only insert optional fields when present — Telegram rejects explicit
    // null for reply_to_message_id, message_thread_id, and reply_markup.
    let mut payload = serde_json::Map::new();
    payload.insert("chat_id".to_string(), json!(outgoing.chat_id));
    payload.insert(media_key.to_string(), json!(media_value));
    if let Some(text) = outgoing.text.as_ref() {
        if !text.is_empty() {
            payload.insert("caption".to_string(), json!(text));
        }
    }
    if let Some(id) = outgoing.reply_to.as_deref().and_then(|id| id.parse::<i64>().ok()) {
        payload.insert("reply_to_message_id".to_string(), json!(id));
    }
    if let Some(id) = outgoing.thread_id.as_deref().and_then(|id| id.parse::<i64>().ok()) {
        payload.insert("message_thread_id".to_string(), json!(id));
    }
    if let Some(kb) = inline_keyboard_json(&outgoing.buttons) {
        payload.insert("reply_markup".to_string(), kb);
    }

    TelegramOutgoingRequest {
        method,
        payload: Value::Object(payload),
    }
}

fn telegram_media_payload(media: &MessageMedia) -> (&'static str, &'static str, String) {
    let source = media
        .file_id
        .clone()
        .or_else(|| media.url.clone())
        .unwrap_or_default();

    match media.kind {
        MessageMediaKind::Image => ("sendPhoto", "photo", source),
        MessageMediaKind::Video => ("sendVideo", "video", source),
        MessageMediaKind::Audio => ("sendAudio", "audio", source),
        MessageMediaKind::Document | MessageMediaKind::Other => {
            ("sendDocument", "document", source)
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TelegramApiResponse<T> {
    pub ok: bool,
    pub result: Option<T>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TelegramUpdate {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<TelegramMessage>,
    #[serde(default)]
    pub edited_message: Option<TelegramMessage>,
    #[serde(default)]
    pub callback_query: Option<TelegramCallbackQuery>,
    #[serde(default)]
    pub message_reaction: Option<TelegramMessageReaction>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TelegramMessage {
    pub message_id: i64,
    pub date: Option<i64>,
    pub chat: TelegramChat,
    #[serde(default)]
    pub message_thread_id: Option<i64>,
    #[serde(default)]
    pub from: Option<TelegramUser>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub caption: Option<String>,
    #[serde(default)]
    pub entities: Vec<TelegramEntity>,
    #[serde(default)]
    pub reply_to_message: Option<Box<TelegramMessage>>,
    #[serde(default)]
    pub photo: Vec<TelegramPhoto>,
    #[serde(default)]
    pub video: Option<TelegramAttachment>,
    #[serde(default)]
    pub audio: Option<TelegramAttachment>,
    #[serde(default)]
    pub document: Option<TelegramAttachment>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TelegramChat {
    pub id: i64,
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TelegramUser {
    pub id: i64,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub first_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TelegramEntity {
    pub offset: usize,
    pub length: usize,
    #[serde(rename = "type")]
    pub entity_type: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TelegramPhoto {
    pub file_id: String,
    #[serde(default)]
    pub file_unique_id: Option<String>,
    #[serde(default)]
    pub file_size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TelegramAttachment {
    pub file_id: String,
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub file_size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TelegramFile {
    pub file_id: String,
    #[serde(default)]
    pub file_unique_id: Option<String>,
    #[serde(default)]
    pub file_size: Option<u64>,
    #[serde(default)]
    pub file_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TelegramCallbackQuery {
    pub id: String,
    pub from: TelegramUser,
    #[serde(default)]
    pub data: Option<String>,
    #[serde(default)]
    pub message: Option<TelegramMessage>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TelegramMessageReaction {
    pub chat: TelegramChat,
    pub message_id: i64,
    #[serde(default)]
    pub user: Option<TelegramUser>,
    #[serde(default)]
    pub old_reaction: Vec<TelegramReactionType>,
    #[serde(default)]
    pub new_reaction: Vec<TelegramReactionType>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TelegramReactionType {
    #[serde(rename = "type")]
    pub reaction_type: String,
    #[serde(default)]
    pub emoji: Option<String>,
}

pub fn normalize_update(update: TelegramUpdate) -> Option<IncomingMessage> {
    if let Some(message) = update.message.or(update.edited_message) {
        return Some(normalize_message(update.update_id, message));
    }

    if let Some(callback) = update.callback_query {
        return normalize_callback(update.update_id, callback);
    }

    if let Some(reaction) = update.message_reaction {
        return Some(normalize_reaction(update.update_id, reaction));
    }

    None
}

pub fn normalize_update_value(payload: Value) -> anyhow::Result<Option<IncomingMessage>> {
    let update: TelegramUpdate =
        serde_json::from_value(payload).context("payload is not a valid telegram update")?;
    Ok(normalize_update(update))
}

fn normalize_message(update_id: i64, message: TelegramMessage) -> IncomingMessage {
    let from = message.from.clone().unwrap_or(TelegramUser {
        id: 0,
        username: None,
        first_name: None,
    });

    let content = message.text.clone().or(message.caption.clone());
    let mut metadata = HashMap::<String, Value>::new();
    metadata.insert("telegram_update_id".to_string(), json!(update_id));

    let mut incoming = IncomingMessage::new(
        Channel::Telegram,
        message.chat.id.to_string(),
        from.id.to_string(),
    );
    incoming.external_id = Some(message.message_id.to_string());
    incoming.thread_id = message.message_thread_id.map(|id| id.to_string());
    incoming.from_username = from.username.clone().or(from.first_name.clone());
    incoming.text = content.clone();
    incoming.reply_to = message
        .reply_to_message
        .as_ref()
        .map(|reply| reply.message_id.to_string());
    incoming.mentions = extract_mentions(content.as_deref().unwrap_or_default(), &message.entities);
    incoming.media = extract_media(&message);
    incoming.metadata = metadata;
    incoming.received_at = message
        .date
        .and_then(|epoch| Utc.timestamp_opt(epoch, 0).single())
        .unwrap_or_else(Utc::now);

    incoming
}

fn normalize_callback(update_id: i64, callback: TelegramCallbackQuery) -> Option<IncomingMessage> {
    let message = callback.message?;

    let mut incoming = IncomingMessage::new(
        Channel::Telegram,
        message.chat.id.to_string(),
        callback.from.id.to_string(),
    );

    incoming.external_id = Some(callback.id.clone());
    incoming.thread_id = message.message_thread_id.map(|id| id.to_string());
    incoming.from_username = callback.from.username;
    incoming.text = callback
        .data
        .clone()
        .map(|value| format!("/callback {value}"));
    incoming.reply_to = Some(message.message_id.to_string());
    incoming
        .metadata
        .insert("telegram_update_id".to_string(), json!(update_id));
    incoming
        .metadata
        .insert("telegram_callback_data".to_string(), json!(callback.data));

    Some(incoming)
}

fn normalize_reaction(update_id: i64, reaction: TelegramMessageReaction) -> IncomingMessage {
    let mut incoming = IncomingMessage::new(
        Channel::Telegram,
        reaction.chat.id.to_string(),
        reaction
            .user
            .as_ref()
            .map(|user| user.id.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
    );

    incoming.external_id = Some(reaction.message_id.to_string());
    incoming.reactions = reaction
        .new_reaction
        .into_iter()
        .filter_map(|item| item.emoji)
        .map(|emoji| Reaction {
            emoji,
            action: crate::message::ReactionAction::Add,
            user_id: reaction.user.as_ref().map(|user| user.id.to_string()),
            message_id: Some(reaction.message_id.to_string()),
        })
        .collect();
    incoming
        .metadata
        .insert("telegram_update_id".to_string(), json!(update_id));

    incoming
}

fn extract_mentions(text: &str, entities: &[TelegramEntity]) -> Vec<String> {
    let mut mentions = entities
        .iter()
        .filter(|entity| entity.entity_type == "mention")
        .filter_map(|entity| {
            text.get(entity.offset..entity.offset + entity.length)
                .map(|value| value.trim_start_matches('@').to_string())
        })
        .collect::<Vec<_>>();

    if mentions.is_empty() {
        mentions = text
            .split_whitespace()
            .filter_map(|part| part.strip_prefix('@'))
            .map(|value| {
                value
                    .trim_matches(|character: char| {
                        !character.is_ascii_alphanumeric() && character != '_'
                    })
                    .to_string()
            })
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
    }

    mentions
}

fn extract_media(message: &TelegramMessage) -> Vec<MessageMedia> {
    let mut media = Vec::new();

    if let Some(photo) = message.photo.last() {
        media.push(MessageMedia {
            kind: MessageMediaKind::Image,
            url: None,
            file_id: Some(photo.file_id.clone()),
            file_name: None,
            mime_type: Some("image/jpeg".to_string()),
            size_bytes: photo.file_size,
            metadata: HashMap::new(),
        });
    }

    if let Some(video) = message.video.as_ref() {
        media.push(MessageMedia {
            kind: MessageMediaKind::Video,
            url: None,
            file_id: Some(video.file_id.clone()),
            file_name: video.file_name.clone(),
            mime_type: media_mime_with_fallback(
                video.mime_type.clone(),
                video.file_name.as_deref(),
            ),
            size_bytes: video.file_size,
            metadata: HashMap::new(),
        });
    }

    if let Some(audio) = message.audio.as_ref() {
        media.push(MessageMedia {
            kind: MessageMediaKind::Audio,
            url: None,
            file_id: Some(audio.file_id.clone()),
            file_name: audio.file_name.clone(),
            mime_type: audio.mime_type.clone(),
            size_bytes: audio.file_size,
            metadata: HashMap::new(),
        });
    }

    if let Some(document) = message.document.as_ref()
        && let Some(document_mime) = classify_document_media_mime(document)
    {
        media.push(MessageMedia {
            kind: MessageMediaKind::Document,
            url: None,
            file_id: Some(document.file_id.clone()),
            file_name: document.file_name.clone(),
            mime_type: Some(document_mime),
            size_bytes: document.file_size,
            metadata: HashMap::new(),
        });
    }

    media
}

fn classify_document_media_mime(document: &TelegramAttachment) -> Option<String> {
    let mime = media_mime_with_fallback(document.mime_type.clone(), document.file_name.as_deref())?;
    if is_image_or_video_mime(&mime) {
        return Some(mime);
    }

    None
}

fn media_mime_with_fallback(mime_type: Option<String>, file_name: Option<&str>) -> Option<String> {
    let from_payload = mime_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase());

    if from_payload.is_some() {
        return from_payload;
    }

    file_name.and_then(|name| {
        mime_guess::from_path(name)
            .first_raw()
            .map(|mime| mime.to_ascii_lowercase())
    })
}

fn is_image_or_video_mime(mime: &str) -> bool {
    let normalized = mime.trim().to_ascii_lowercase();
    normalized.starts_with("image/") || normalized.starts_with("video/")
}

pub fn telegram_file_download_url(api_base: &str, token: &str, file_path: &str) -> String {
    format!(
        "{}/file/bot{}/{}",
        api_base.trim_end_matches('/'),
        token,
        file_path.trim_start_matches('/')
    )
}

pub fn looks_like_telegram_payload(payload: &Value) -> bool {
    payload.get("update_id").is_some()
}

pub fn log_telegram_error(context: &str, error: &TelegramError) {
    warn!(target: "ff_gateway::telegram", %context, %error, "telegram operation failed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn normalize_message_captures_commands_replies_and_threads() {
        let update = TelegramUpdate {
            update_id: 42,
            message: Some(TelegramMessage {
                message_id: 77,
                date: Some(1_700_000_000),
                chat: TelegramChat {
                    id: 8496613333,
                    r#type: Some("private".to_string()),
                    title: None,
                    username: Some("venkatyarl".to_string()),
                },
                message_thread_id: Some(9001),
                from: Some(TelegramUser {
                    id: 123,
                    username: Some("vinny".to_string()),
                    first_name: Some("Vinny".to_string()),
                }),
                text: Some("/status@forgefleet please".to_string()),
                caption: None,
                entities: vec![TelegramEntity {
                    offset: 0,
                    length: 20,
                    entity_type: "bot_command".to_string(),
                }],
                reply_to_message: Some(Box::new(TelegramMessage {
                    message_id: 76,
                    date: None,
                    chat: TelegramChat {
                        id: 8496613333,
                        r#type: Some("private".to_string()),
                        title: None,
                        username: None,
                    },
                    message_thread_id: Some(9001),
                    from: None,
                    text: Some("previous".to_string()),
                    caption: None,
                    entities: vec![],
                    reply_to_message: None,
                    photo: vec![],
                    video: None,
                    audio: None,
                    document: None,
                })),
                photo: vec![],
                video: None,
                audio: None,
                document: None,
            }),
            edited_message: None,
            callback_query: None,
            message_reaction: None,
        };

        let incoming = normalize_update(update).expect("message should normalize");
        let parsed = incoming
            .parse_command(&['/', '!'])
            .expect("command should parse");

        assert_eq!(incoming.chat_id, "8496613333");
        assert_eq!(incoming.thread_id.as_deref(), Some("9001"));
        assert_eq!(incoming.reply_to.as_deref(), Some("76"));
        assert_eq!(parsed.command, "status");
    }

    #[test]
    fn outgoing_payload_selects_send_message_and_media_methods() {
        let text = OutgoingMessage::text(Channel::Telegram, "8496613333", "hello");
        let text_request = build_outgoing_request(&text).expect("text payload");
        assert_eq!(text_request.method, "sendMessage");
        assert_eq!(text_request.payload["text"], json!("hello"));

        let mut photo = OutgoingMessage::text(Channel::Telegram, "8496613333", "photo");
        photo.media.push(MessageMedia {
            kind: MessageMediaKind::Image,
            url: Some("https://example.com/x.jpg".to_string()),
            file_id: None,
            file_name: None,
            mime_type: Some("image/jpeg".to_string()),
            size_bytes: None,
            metadata: HashMap::new(),
        });

        let photo_request = build_outgoing_request(&photo).expect("photo payload");
        assert_eq!(photo_request.method, "sendPhoto");
        assert_eq!(
            photo_request.payload["photo"],
            json!("https://example.com/x.jpg")
        );

        let mut document = OutgoingMessage::text(Channel::Telegram, "8496613333", "doc");
        document.media.push(MessageMedia {
            kind: MessageMediaKind::Document,
            url: None,
            file_id: Some("file-123".to_string()),
            file_name: Some("report.pdf".to_string()),
            mime_type: Some("application/pdf".to_string()),
            size_bytes: None,
            metadata: HashMap::new(),
        });

        let document_request = build_media_request_for_kind(&document, MessageMediaKind::Document)
            .expect("document payload");
        assert_eq!(document_request.method, "sendDocument");
        assert_eq!(document_request.payload["document"], json!("file-123"));
    }

    #[test]
    fn document_media_classification_accepts_image_video_like_and_rejects_pdf() {
        let image_doc = TelegramAttachment {
            file_id: "doc-image".to_string(),
            file_name: Some("photo.png".to_string()),
            mime_type: Some("image/png".to_string()),
            file_size: Some(100),
        };
        let video_doc = TelegramAttachment {
            file_id: "doc-video".to_string(),
            file_name: Some("clip.mov".to_string()),
            mime_type: None,
            file_size: Some(200),
        };
        let pdf_doc = TelegramAttachment {
            file_id: "doc-pdf".to_string(),
            file_name: Some("report.pdf".to_string()),
            mime_type: Some("application/pdf".to_string()),
            file_size: Some(300),
        };

        assert_eq!(
            classify_document_media_mime(&image_doc).as_deref(),
            Some("image/png")
        );
        assert_eq!(
            classify_document_media_mime(&video_doc).as_deref(),
            Some("video/quicktime")
        );
        assert_eq!(classify_document_media_mime(&pdf_doc), None);
    }

    #[test]
    fn file_download_url_normalizes_base_and_path() {
        let url = telegram_file_download_url(
            "https://api.telegram.org/",
            "token-123",
            "/photos/file.jpg",
        );
        assert_eq!(
            url,
            "https://api.telegram.org/file/bottoken-123/photos/file.jpg"
        );
    }
}
