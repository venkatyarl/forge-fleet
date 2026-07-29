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

use std::collections::VecDeque;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use sqlx::PgPool;

use crate::notifications::SHARED_HTTP;

const TELEGRAM_BOT_TOKEN_KEY: &str = "telegram_bot_token";
const TELEGRAM_CHAT_ID_KEY: &str = "telegram_chat_id";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TelegramMessageIdentity {
    pub chat_id: String,
    pub message_id: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TelegramDigestOutcome {
    Acknowledged {
        messages: Vec<TelegramMessageIdentity>,
    },
    DefinitelyNotDelivered {
        error: String,
    },
    Ambiguous {
        error: String,
    },
}

/// Whether a send can actually be attempted. Callers that own durable cursors
/// must not interpret the legacy sender's no-op-on-missing-config behavior as
/// confirmed delivery.
pub async fn telegram_is_configured(pool: &PgPool) -> Result<bool> {
    Ok(ff_db::pg_get_secret(pool, TELEGRAM_BOT_TOKEN_KEY)
        .await
        .context("lookup telegram bot token")?
        .is_some()
        && ff_db::pg_get_secret(pool, TELEGRAM_CHAT_ID_KEY)
            .await
            .context("lookup telegram chat id")?
            .is_some())
}

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

/// Send a Telegram message with an optional inline logo/photo. When
/// `photo` is `Some(non-empty bytes)` this uploads via `sendPhoto` (multipart)
/// with `caption = title\n\nbody`; the image renders above the text. When
/// `photo` is `None`/empty it falls back to the plain `sendMessage` path so a
/// project without a logo still gets its digest. Silently `Ok(())` when
/// telegram isn't configured. Used by the per-project digest framework so each
/// project's update carries its own logo.
pub async fn send_telegram_photo_from_secrets(
    pool: &PgPool,
    title: &str,
    body: &str,
    photo: Option<&[u8]>,
) -> Result<()> {
    let bytes = match photo {
        Some(b) if !b.is_empty() => b.to_vec(),
        _ => return send_telegram_from_secrets(pool, title, body).await,
    };

    let token = ff_db::pg_get_secret(pool, TELEGRAM_BOT_TOKEN_KEY)
        .await
        .context("lookup telegram bot token")?;
    let chat_id = ff_db::pg_get_secret(pool, TELEGRAM_CHAT_ID_KEY)
        .await
        .context("lookup telegram chat id")?;
    let (Some(token), Some(chat_id)) = (token, chat_id) else {
        tracing::debug!("telegram not fully configured; skipping photo send");
        return Ok(());
    };

    // Telegram photo captions are capped at 1024 chars; keep the header and
    // trim the body if the combined caption would overflow.
    let mut caption = if body.is_empty() {
        title.to_string()
    } else {
        format!("{title}\n\n{body}")
    };
    if caption.chars().count() > 1024 {
        caption = caption.chars().take(1021).collect::<String>() + "...";
    }

    let url = format!("https://api.telegram.org/bot{token}/sendPhoto");
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name("logo.png")
        .mime_str("image/png")
        .context("build telegram photo part")?;
    let form = reqwest::multipart::Form::new()
        .text("chat_id", chat_id)
        .text("caption", caption)
        .part("photo", part);

    let resp = SHARED_HTTP
        .post(&url)
        .multipart(form)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .context("POST telegram sendPhoto")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        // Fall back to text so the operator still gets the digest even if the
        // image upload is rejected (bad bytes, size limit, etc.).
        tracing::warn!(%status, err = %text.trim(), "telegram sendPhoto failed; falling back to text");
        return send_telegram_from_secrets(pool, title, body).await;
    }
    Ok(())
}

/// Split `s` into chunks no longer than `max` chars, breaking on line
/// boundaries so a digest never splits mid-line. A single line longer than
/// `max` is hard-split by chars as a last resort.
fn chunk_text(s: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for line in s.lines() {
        // a single over-long line: flush current, then hard-split the line.
        if line.chars().count() > max {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            let mut buf = String::new();
            for ch in line.chars() {
                if buf.chars().count() + 1 > max {
                    out.push(std::mem::take(&mut buf));
                }
                buf.push(ch);
            }
            if !buf.is_empty() {
                out.push(buf);
            }
            continue;
        }
        if cur.chars().count() + line.chars().count() + 1 > max && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(line);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Send a digest that NEVER truncates. If a logo is present it's posted as a
/// photo captioned with just the (short) `title`; the full `body` then follows
/// as one or more plain-text messages, chunked to Telegram's 4096-char text
/// limit on line boundaries. Without a logo, `title`+`body` are sent as chunked
/// text. This fixes the cut-off caused by cramming a long digest into a
/// 1024-char photo caption.
pub async fn send_telegram_digest(
    pool: &PgPool,
    title: &str,
    body: &str,
    photo: Option<&[u8]>,
) -> Result<()> {
    if let Some(bytes) = photo.filter(|b| !b.is_empty()) {
        // ONE MESSAGE when it fits: if logo + name + full status is within
        // Telegram's 1024-char photo-caption limit, send it as a single photo
        // message (logo, project name, and the whole status together) instead of
        // splitting the logo from the body. Longer digests still fall back to a
        // photo (title caption) + chunked text below (until inline custom-emoji
        // logos land, which remove the caption limit entirely).
        let combined_len = title.chars().count() + 2 + body.chars().count();
        if combined_len <= 1024 {
            return send_telegram_photo_from_secrets(pool, title, body, Some(bytes)).await;
        }
        // Logo photo with a short, cutoff-proof caption (title only).
        send_telegram_photo_from_secrets(pool, title, "", Some(bytes)).await?;
        // Full body as chunked text (headerless — the photo already showed the title).
        for chunk in chunk_text(body, 3800) {
            if !chunk.trim().is_empty() {
                send_telegram_from_secrets(pool, &chunk, "").await?;
            }
        }
    } else {
        let full = if body.is_empty() {
            title.to_string()
        } else {
            format!("{title}\n{body}")
        };
        for chunk in chunk_text(&full, 3800) {
            if !chunk.trim().is_empty() {
                send_telegram_from_secrets(pool, &chunk, "").await?;
            }
        }
    }
    Ok(())
}

/// Classified delivery boundary for durable callers. A successful result
/// includes every Telegram identity. Transport errors and response parse loss
/// are ambiguous because Telegram may already have accepted the message.
pub async fn send_telegram_digest_classified(
    pool: &PgPool,
    title: &str,
    body: &str,
    photo: Option<&[u8]>,
) -> TelegramDigestOutcome {
    let token = match ff_db::pg_get_secret(pool, TELEGRAM_BOT_TOKEN_KEY).await {
        Ok(Some(value)) => value,
        Ok(None) => {
            return TelegramDigestOutcome::DefinitelyNotDelivered {
                error: "telegram bot token is not configured".into(),
            };
        }
        Err(err) => {
            return TelegramDigestOutcome::DefinitelyNotDelivered {
                error: format!("lookup telegram bot token: {err}"),
            };
        }
    };
    let chat_id = match ff_db::pg_get_secret(pool, TELEGRAM_CHAT_ID_KEY).await {
        Ok(Some(value)) => value,
        Ok(None) => {
            return TelegramDigestOutcome::DefinitelyNotDelivered {
                error: "telegram chat id is not configured".into(),
            };
        }
        Err(err) => {
            return TelegramDigestOutcome::DefinitelyNotDelivered {
                error: format!("lookup telegram chat id: {err}"),
            };
        }
    };

    let mut messages = Vec::new();
    let mut requests: VecDeque<TelegramDigestRequest> = VecDeque::new();
    if let Some(bytes) = photo.filter(|bytes| !bytes.is_empty()) {
        let combined_len = title.chars().count() + 2 + body.chars().count();
        if combined_len <= 1024 {
            requests.push_back(TelegramDigestRequest::Photo {
                caption: if body.is_empty() {
                    title.to_string()
                } else {
                    format!("{title}\n\n{body}")
                },
                bytes: bytes.to_vec(),
                fallback_text: if body.is_empty() {
                    title.to_string()
                } else {
                    format!("{title}\n{body}")
                },
            });
        } else {
            requests.push_back(TelegramDigestRequest::Photo {
                caption: title.to_string(),
                bytes: bytes.to_vec(),
                fallback_text: title.to_string(),
            });
            requests.extend(
                chunk_text(body, 3800)
                    .into_iter()
                    .filter(|chunk| !chunk.trim().is_empty())
                    .map(TelegramDigestRequest::Text),
            );
        }
    } else {
        let full = if body.is_empty() {
            title.to_string()
        } else {
            format!("{title}\n{body}")
        };
        requests.extend(
            chunk_text(&full, 3800)
                .into_iter()
                .filter(|chunk| !chunk.trim().is_empty())
                .map(TelegramDigestRequest::Text),
        );
    }

    while let Some(request) = requests.pop_front() {
        let photo_fallback = match &request {
            TelegramDigestRequest::Photo { fallback_text, .. } => Some(fallback_text.clone()),
            TelegramDigestRequest::Text(_) => None,
        };
        match send_classified_request(&token, &chat_id, request).await {
            Ok(identity) => messages.push(identity),
            Err(ClassifiedRequestError::Definite(_)) if photo_fallback.is_some() => {
                // Telegram explicitly rejected the photo before accepting it.
                // Preserve the established logo behavior by safely falling
                // back to text; the rejection makes this retry non-ambiguous.
                requests.push_front(TelegramDigestRequest::Text(photo_fallback.unwrap()));
            }
            Err(ClassifiedRequestError::Definite(error)) if messages.is_empty() => {
                return TelegramDigestOutcome::DefinitelyNotDelivered { error };
            }
            Err(ClassifiedRequestError::Definite(error)) => {
                return TelegramDigestOutcome::Ambiguous {
                    error: format!(
                        "partial Telegram digest: {} chunk(s) acknowledged before rejection: {error}",
                        messages.len()
                    ),
                };
            }
            Err(ClassifiedRequestError::Ambiguous(error)) => {
                return TelegramDigestOutcome::Ambiguous {
                    error: if messages.is_empty() {
                        error
                    } else {
                        format!(
                            "partial Telegram digest: {} chunk(s) acknowledged before ambiguous outcome: {error}",
                            messages.len()
                        )
                    },
                };
            }
        }
    }

    TelegramDigestOutcome::Acknowledged { messages }
}

enum TelegramDigestRequest {
    Text(String),
    Photo {
        caption: String,
        bytes: Vec<u8>,
        fallback_text: String,
    },
}

enum ClassifiedRequestError {
    Definite(String),
    Ambiguous(String),
}

fn classify_non_success(status: reqwest::StatusCode, body: &str) -> ClassifiedRequestError {
    let error = format!("telegram HTTP {status}: {}", body.trim());
    if status.is_client_error() {
        // Telegram conclusively rejected a 4xx request, including rate limits.
        ClassifiedRequestError::Definite(error)
    } else {
        // A proxy/server failure or unexpected redirect cannot prove whether
        // Telegram accepted the message before the response failed.
        ClassifiedRequestError::Ambiguous(error)
    }
}

async fn send_classified_request(
    token: &str,
    chat_id: &str,
    request: TelegramDigestRequest,
) -> std::result::Result<TelegramMessageIdentity, ClassifiedRequestError> {
    let (method, response) = match request {
        TelegramDigestRequest::Text(text) => {
            let url = format!("https://api.telegram.org/bot{token}/sendMessage");
            let payload = telegram_payload(chat_id, &text, "");
            (
                "sendMessage",
                SHARED_HTTP
                    .post(url)
                    .json(&payload)
                    .timeout(Duration::from_secs(10))
                    .send()
                    .await,
            )
        }
        TelegramDigestRequest::Photo {
            caption,
            bytes,
            fallback_text: _,
        } => {
            let part = reqwest::multipart::Part::bytes(bytes)
                .file_name("logo.png")
                .mime_str("image/png")
                .map_err(|err| ClassifiedRequestError::Definite(err.to_string()))?;
            let form = reqwest::multipart::Form::new()
                .text("chat_id", chat_id.to_string())
                .text("caption", caption)
                .part("photo", part);
            let url = format!("https://api.telegram.org/bot{token}/sendPhoto");
            (
                "sendPhoto",
                SHARED_HTTP
                    .post(url)
                    .multipart(form)
                    .timeout(Duration::from_secs(20))
                    .send()
                    .await,
            )
        }
    };
    let response = response.map_err(|err| {
        ClassifiedRequestError::Ambiguous(format!("POST Telegram {method}: {err}"))
    })?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(classify_non_success(status, &body));
    }
    let json: serde_json::Value = response.json().await.map_err(|err| {
        ClassifiedRequestError::Ambiguous(format!(
            "parse successful Telegram {method} response: {err}"
        ))
    })?;
    let message_id = json
        .pointer("/result/message_id")
        .and_then(|value| value.as_i64())
        .ok_or_else(|| {
            ClassifiedRequestError::Ambiguous(format!(
                "successful Telegram {method} response missing result.message_id"
            ))
        })?;
    let returned_chat_id = json
        .pointer("/result/chat/id")
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string())
        })
        .unwrap_or_else(|| chat_id.to_string());
    Ok(TelegramMessageIdentity {
        chat_id: returned_chat_id,
        message_id,
    })
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
    use super::{ClassifiedRequestError, classify_non_success, telegram_payload};

    #[test]
    fn telegram_payload_uses_plain_text() {
        let payload = telegram_payload("123", "Fleet alert", "work_items #42: ff_interactions");

        assert_eq!(
            payload["text"],
            "Fleet alert\nwork_items #42: ff_interactions"
        );
        assert!(payload.get("parse_mode").is_none());
    }

    #[test]
    fn telegram_http_failures_retry_only_conclusive_client_rejections() {
        assert!(matches!(
            classify_non_success(reqwest::StatusCode::BAD_REQUEST, "bad request"),
            ClassifiedRequestError::Definite(_)
        ));
        assert!(matches!(
            classify_non_success(reqwest::StatusCode::TOO_MANY_REQUESTS, "retry later"),
            ClassifiedRequestError::Definite(_)
        ));
        assert!(matches!(
            classify_non_success(reqwest::StatusCode::INTERNAL_SERVER_ERROR, "server failed"),
            ClassifiedRequestError::Ambiguous(_)
        ));
        assert!(matches!(
            classify_non_success(reqwest::StatusCode::FOUND, "unexpected redirect"),
            ClassifiedRequestError::Ambiguous(_)
        ));
    }
}
