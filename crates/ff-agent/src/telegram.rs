//! Zero-ceremony Telegram sender.
//!
//! Reads the bot token + chat id from `fleet_secrets` and POSTs a message
//! to the Telegram Bot API. Used by the fully-automatic upgrade loop so
//! the operator hears about every fleet change without any setup past
//! `ff secrets set openclaw.telegram_bot_token ...`.
//!
//! Returns `Ok(())` on successful send; returns `Err` with a human-readable
//! reason on any failure (missing secret, HTTP error, timeout) so callers
//! can log without crashing.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use sqlx::PgPool;

use crate::notifications::SHARED_HTTP;

const TELEGRAM_BOT_TOKEN_KEY: &str = "telegram_bot_token";
const TELEGRAM_CHAT_ID_KEY: &str = "telegram_chat_id";

fn telegram_payload(chat_id: &str, title: &str, body: &str) -> serde_json::Value {
    let text = if body.is_empty() {
        title.to_string()
    } else {
        format!("{title}\n{body}")
    };

    serde_json::json!({
        "chat_id": chat_id,
        "text": text,
        "disable_web_page_preview": true,
    })
}

/// Fire-and-forget Telegram send. `title` is placed at the top and `body`
/// follows on the next line. Both are sent as plain text.
///
/// Silently returns `Ok(())` if either secret is missing — we don't
/// consider that a runtime error, it's just "telegram not configured."
pub async fn send_telegram_from_secrets(pool: &PgPool, title: &str, body: &str) -> Result<()> {
    send_returning_id(pool, title, body).await.map(|_| ())
}

/// Like [`send_telegram_from_secrets`] but records the sent message in
/// `telegram_messages` keyed to `session_id`, so an operator REPLY to this
/// exact message can be routed back to the session that sent it (the reply
/// poller resolves `reply_to_message.message_id` against this table).
/// Returns the Telegram message id when the send happened and was recorded.
pub async fn send_telegram_recorded(
    pool: &PgPool,
    title: &str,
    body: &str,
    session_id: &str,
) -> Result<Option<i64>> {
    let Some((chat_id, message_id)) = send_returning_id(pool, title, body).await? else {
        return Ok(None);
    };
    sqlx::query(
        "INSERT INTO telegram_messages (chat_id, tg_message_id, session_id, title) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (chat_id, tg_message_id) DO NOTHING",
    )
    .bind(&chat_id)
    .bind(message_id)
    .bind(session_id)
    .bind(title)
    .execute(pool)
    .await
    .context("record telegram_messages row")?;
    Ok(Some(message_id))
}

/// Shared send path: returns `None` when telegram isn't configured, else
/// `(chat_id, message_id)` of the delivered message.
async fn send_returning_id(
    pool: &PgPool,
    title: &str,
    body: &str,
) -> Result<Option<(String, i64)>> {
    let token = ff_db::pg_get_secret(pool, TELEGRAM_BOT_TOKEN_KEY)
        .await
        .context("lookup telegram bot token")?;
    let chat_id = ff_db::pg_get_secret(pool, TELEGRAM_CHAT_ID_KEY)
        .await
        .context("lookup telegram chat id")?;

    let has_token = token.is_some();
    let has_chat = chat_id.is_some();
    let (Some(token), Some(chat_id)) = (token, chat_id) else {
        tracing::debug!(
            has_token,
            has_chat,
            "telegram not fully configured; skipping send"
        );
        return Ok(None);
    };

    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    let payload = telegram_payload(&chat_id, title, body);

    let resp = SHARED_HTTP
        .post(&url)
        .json(&payload)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("POST telegram sendMessage")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("telegram HTTP {status}: {}", body.trim()));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .context("parse telegram sendMessage response")?;
    let message_id = json
        .pointer("/result/message_id")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow!("telegram response missing result.message_id"))?;
    Ok(Some((chat_id, message_id)))
}

#[cfg(test)]
mod tests {
    use super::telegram_payload;

    #[test]
    fn telegram_payload_uses_plain_text() {
        let payload = telegram_payload("123", "Fleet alert", "work_items #42: ff_interactions");

        assert_eq!(
            payload["text"],
            "Fleet alert\nwork_items #42: ff_interactions"
        );
        assert!(payload.get("parse_mode").is_none());
    }
}
