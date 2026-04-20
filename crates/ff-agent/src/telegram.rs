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

const TELEGRAM_BOT_TOKEN_KEY: &str = "openclaw.telegram_bot_token";
const TELEGRAM_CHAT_ID_KEY: &str = "openclaw.telegram_chat_id";

/// Fire-and-forget Telegram send. `title` is bolded at the top,
/// `body` follows as a new paragraph.
///
/// Silently returns `Ok(())` if either secret is missing — we don't
/// consider that a runtime error, it's just "telegram not configured."
pub async fn send_telegram_from_secrets(pool: &PgPool, title: &str, body: &str) -> Result<()> {
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
        return Ok(());
    };

    // Telegram's Markdown is "legacy" — we use plain text with a small
    // bolded title to keep escaping simple. Body can contain anything.
    let text = if body.is_empty() {
        title.to_string()
    } else {
        format!("*{}*\n{}", title, body)
    };

    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    let payload = serde_json::json!({
        "chat_id": chat_id,
        "text": text,
        "parse_mode": "Markdown",
        "disable_web_page_preview": true,
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build reqwest client")?;

    let resp = client
        .post(&url)
        .json(&payload)
        .send()
        .await
        .context("POST telegram sendMessage")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("telegram HTTP {status}: {}", body.trim()));
    }
    Ok(())
}
