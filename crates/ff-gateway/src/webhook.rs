use std::{collections::HashMap, str::FromStr};

use anyhow::Context;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    discord,
    message::{Channel, IncomingMessage, MessageMedia, Reaction},
    telegram,
};

#[derive(Debug, Clone, Serialize)]
pub struct WebhookAcceptedResponse {
    pub accepted: bool,
    pub channel: Channel,
    pub message_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum WebhookError {
    #[error("unsupported or invalid webhook payload")]
    UnsupportedPayload,
    #[error("payload parse error: {0}")]
    Parse(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct GenericWebhookPayload {
    pub channel: Option<String>,
    pub message_id: Option<String>,
    pub chat_id: Option<String>,
    pub thread_id: Option<String>,
    pub user_id: Option<String>,
    pub username: Option<String>,
    pub text: Option<String>,
    pub reply_to: Option<String>,
    #[serde(default)]
    pub mentions: Vec<String>,
    #[serde(default)]
    pub media: Vec<MessageMedia>,
    #[serde(default)]
    pub reactions: Vec<Reaction>,
    #[serde(flatten)]
    pub metadata: HashMap<String, Value>,
}

pub fn normalize_payload(
    payload: Value,
    fallback_channel: Option<Channel>,
) -> Result<IncomingMessage, WebhookError> {
    if telegram::looks_like_telegram_payload(&payload) {
        return telegram::normalize_update_value(payload)
            .map_err(|error| WebhookError::Parse(error.to_string()))?
            .ok_or(WebhookError::UnsupportedPayload);
    }

    if discord::looks_like_discord_payload(&payload) {
        return discord::normalize_payload(payload)
            .map_err(|error| WebhookError::Parse(error.to_string()))?
            .ok_or(WebhookError::UnsupportedPayload);
    }

    normalize_generic_payload(payload, fallback_channel)
}

pub async fn receive_webhook(
    payload: Value,
    fallback_channel: Option<Channel>,
) -> Result<WebhookAcceptedResponse, WebhookError> {
    let message = normalize_payload(payload, fallback_channel)?;

    Ok(WebhookAcceptedResponse {
        accepted: true,
        channel: message.channel,
        message_id: message.id.to_string(),
    })
}

pub async fn webhook_http_handler(
    Json(payload): Json<Value>,
) -> Result<Json<WebhookAcceptedResponse>, (StatusCode, Json<Value>)> {
    match receive_webhook(payload, None).await {
        Ok(response) => Ok(Json(response)),
        Err(error) => Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "message": error.to_string(),
                    "type": "invalid_webhook_payload"
                }
            })),
        )),
    }
}

fn normalize_generic_payload(
    payload: Value,
    fallback_channel: Option<Channel>,
) -> Result<IncomingMessage, WebhookError> {
    let parsed: GenericWebhookPayload = serde_json::from_value(payload)
        .context("payload does not match generic webhook schema")
        .map_err(|error| WebhookError::Parse(error.to_string()))?;

    let channel = parsed
        .channel
        .as_deref()
        .map(Channel::from_str)
        .transpose()
        .map_err(WebhookError::Parse)?
        .or(fallback_channel)
        .ok_or(WebhookError::UnsupportedPayload)?;

    let chat_id = parsed.chat_id.unwrap_or_else(|| "default".to_string());
    let user_id = parsed.user_id.unwrap_or_else(|| "anonymous".to_string());

    let mut message = IncomingMessage::new(channel, chat_id, user_id);
    message.external_id = parsed.message_id;
    message.thread_id = parsed.thread_id;
    message.from_username = parsed.username;
    message.text = parsed.text;
    message.reply_to = parsed.reply_to;
    message.mentions = parsed.mentions;
    message.media = parsed.media;
    message.reactions = parsed.reactions;
    message.metadata = parsed.metadata;

    Ok(message)
}
