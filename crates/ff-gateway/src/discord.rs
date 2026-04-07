use std::collections::HashMap;

use anyhow::Context;
use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use thiserror::Error;
use tracing::warn;

use crate::message::{
    Channel, IncomingMessage, MessageMedia, MessageMediaKind, OutgoingMessage, Reaction,
};

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

#[derive(Debug, Error)]
pub enum DiscordError {
    #[error("http request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("discord api error ({status}): {message}")]
    Api { status: StatusCode, message: String },
    #[error("discord payload parse error: {0}")]
    Parse(String),
}

#[derive(Debug, Clone)]
pub struct DiscordClient {
    bot_token: String,
    api_base: String,
    http_client: reqwest::Client,
}

impl DiscordClient {
    pub fn new(bot_token: impl Into<String>) -> anyhow::Result<Self> {
        let http_client = reqwest::Client::builder()
            .pool_idle_timeout(std::time::Duration::from_secs(30))
            .build()
            .context("failed to create discord reqwest client")?;

        Ok(Self {
            bot_token: bot_token.into(),
            api_base: DISCORD_API_BASE.to_string(),
            http_client,
        })
    }

    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Self {
        self.api_base = api_base.into();
        self
    }

    pub async fn send_message(
        &self,
        outgoing: &OutgoingMessage,
    ) -> Result<DiscordMessage, DiscordError> {
        let payload = json!({
            "content": outgoing.text,
            "message_reference": outgoing.reply_to.as_ref().map(|id| json!({"message_id": id})),
            "components": discord_components(&outgoing.buttons),
        });

        self.call(
            reqwest::Method::POST,
            &format!("/channels/{}/messages", outgoing.chat_id),
            Some(payload),
        )
        .await
    }

    pub async fn create_thread(
        &self,
        channel_id: &str,
        name: &str,
        auto_archive_duration_minutes: u16,
    ) -> Result<DiscordThread, DiscordError> {
        let payload = json!({
            "name": name,
            "auto_archive_duration": auto_archive_duration_minutes,
            "type": 11
        });

        self.call(
            reqwest::Method::POST,
            &format!("/channels/{channel_id}/threads"),
            Some(payload),
        )
        .await
    }

    pub async fn create_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        reaction: &Reaction,
    ) -> Result<(), DiscordError> {
        let encoded_emoji = percent_encode_component(&reaction.emoji);
        self.call_empty(
            reqwest::Method::PUT,
            &format!("/channels/{channel_id}/messages/{message_id}/reactions/{encoded_emoji}/@me"),
            None,
        )
        .await
    }

    pub fn normalize_payload(&self, payload: Value) -> anyhow::Result<Option<IncomingMessage>> {
        normalize_payload(payload)
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        payload: Option<Value>,
    ) -> Result<T, DiscordError> {
        let url = format!("{}{}", self.api_base.trim_end_matches('/'), path);
        let request = self
            .http_client
            .request(method, url)
            .header("Authorization", format!("Bot {}", self.bot_token))
            .header("Content-Type", "application/json");

        let response = match payload {
            Some(body) => request.json(&body).send().await?,
            None => request.send().await?,
        };

        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            return Err(DiscordError::Api {
                status,
                message: String::from_utf8_lossy(&bytes).to_string(),
            });
        }

        serde_json::from_slice(&bytes).map_err(|error| DiscordError::Parse(error.to_string()))
    }

    async fn call_empty(
        &self,
        method: reqwest::Method,
        path: &str,
        payload: Option<Value>,
    ) -> Result<(), DiscordError> {
        let url = format!("{}{}", self.api_base.trim_end_matches('/'), path);
        let request = self
            .http_client
            .request(method, url)
            .header("Authorization", format!("Bot {}", self.bot_token))
            .header("Content-Type", "application/json");

        let response = match payload {
            Some(body) => request.json(&body).send().await?,
            None => request.send().await?,
        };

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(DiscordError::Api {
                status,
                message: body,
            });
        }

        Ok(())
    }
}

fn discord_components(button_rows: &[Vec<crate::message::MessageButton>]) -> Vec<Value> {
    button_rows
        .iter()
        .map(|row| {
            let components = row
                .iter()
                .map(|button| {
                    let style = if button.url.is_some() { 5 } else { 1 };
                    json!({
                        "type": 2,
                        "style": style,
                        "label": button.text,
                        "custom_id": button.callback_data,
                        "url": button.url,
                    })
                })
                .collect::<Vec<_>>();

            json!({
                "type": 1,
                "components": components
            })
        })
        .collect::<Vec<_>>()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordMessage {
    pub id: String,
    pub channel_id: String,
    #[serde(default)]
    pub guild_id: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub author: Option<DiscordUser>,
    #[serde(default)]
    pub mentions: Vec<DiscordUser>,
    #[serde(default)]
    pub attachments: Vec<DiscordAttachment>,
    #[serde(default)]
    pub message_reference: Option<DiscordMessageReference>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordThread {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordUser {
    pub id: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub global_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordAttachment {
    pub id: String,
    pub url: String,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordMessageReference {
    #[serde(default)]
    pub message_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordWebhookEnvelope {
    #[serde(default)]
    pub t: Option<String>,
    #[serde(default)]
    pub d: Option<Value>,
}

pub fn normalize_payload(payload: Value) -> anyhow::Result<Option<IncomingMessage>> {
    if payload.get("channel_id").is_some() && payload.get("id").is_some() {
        let message: DiscordMessage =
            serde_json::from_value(payload).context("discord payload is not a valid message")?;
        return Ok(Some(normalize_message(message)));
    }

    if payload.get("t").is_some() && payload.get("d").is_some() {
        let envelope: DiscordWebhookEnvelope =
            serde_json::from_value(payload).context("discord gateway envelope parse failed")?;

        let event = envelope.t.unwrap_or_default();
        if event == "MESSAGE_CREATE"
            && let Some(data) = envelope.d
        {
            let message: DiscordMessage = serde_json::from_value(data)
                .context("discord MESSAGE_CREATE payload parse failed")?;
            return Ok(Some(normalize_message(message)));
        }

        return Ok(None);
    }

    Ok(None)
}

fn normalize_message(message: DiscordMessage) -> IncomingMessage {
    let author = message.author.unwrap_or(DiscordUser {
        id: "unknown".to_string(),
        username: None,
        global_name: None,
    });

    let mut incoming =
        IncomingMessage::new(Channel::Discord, message.channel_id.clone(), author.id);
    incoming.external_id = Some(message.id);
    incoming.from_username = author.global_name.or(author.username);
    incoming.text = message.content;
    incoming.reply_to = message
        .message_reference
        .and_then(|reference| reference.message_id);
    incoming.mentions = message
        .mentions
        .iter()
        .filter_map(|user| user.username.clone())
        .collect::<Vec<_>>();
    incoming.media = message
        .attachments
        .iter()
        .map(|attachment| MessageMedia {
            kind: media_kind_from_content_type(attachment.content_type.as_deref()),
            url: Some(attachment.url.clone()),
            file_id: Some(attachment.id.clone()),
            file_name: attachment.filename.clone(),
            mime_type: attachment.content_type.clone(),
            size_bytes: attachment.size,
            metadata: HashMap::new(),
        })
        .collect::<Vec<_>>();

    incoming.received_at = message
        .timestamp
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);

    incoming
}

fn media_kind_from_content_type(content_type: Option<&str>) -> MessageMediaKind {
    let content_type = content_type.unwrap_or_default();
    if content_type.starts_with("image/") {
        return MessageMediaKind::Image;
    }
    if content_type.starts_with("video/") {
        return MessageMediaKind::Video;
    }
    if content_type.starts_with("audio/") {
        return MessageMediaKind::Audio;
    }
    if !content_type.is_empty() {
        return MessageMediaKind::Document;
    }

    MessageMediaKind::Other
}

fn percent_encode_component(input: &str) -> String {
    let mut encoded = String::with_capacity(input.len());
    for byte in input.bytes() {
        let is_unreserved =
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~');
        if is_unreserved {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push_str(&format!("{byte:02X}"));
        }
    }
    encoded
}

pub fn looks_like_discord_payload(payload: &Value) -> bool {
    payload.get("channel_id").is_some() || payload.get("t").is_some()
}

pub fn log_discord_error(context: &str, error: &DiscordError) {
    warn!(target: "ff_gateway::discord", %context, %error, "discord operation failed");
}
