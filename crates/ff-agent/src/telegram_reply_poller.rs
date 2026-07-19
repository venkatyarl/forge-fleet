//! Telegram reply → session routing (leader tick).
//!
//! Operator flow: any component sends a recorded message via
//! [`crate::telegram::send_telegram_recorded`], which stores
//! `(chat_id, tg_message_id) → session_id` in `telegram_messages`. This poller
//! runs on the leader, drains the bot's `getUpdates`, and for every incoming
//! message files a row in `telegram_replies` — with `session_id` resolved when
//! the operator used Telegram's reply feature on a recorded message. Sessions
//! consume their rows via `ff notify replies --session <id> --claim`.
//!
//! GATED: only one getUpdates consumer may exist per bot token, so the poller
//! runs only when the `telegram.reply_poller` secret is `on`. Leave it off
//! while any out-of-band consumer (e.g. a debugging shell poller) holds the
//! long-poll.
//!
//! The getUpdates offset lives in the single-row `telegram_poll_state` table,
//! not in memory, so a leader failover never double-delivers a reply
//! (`tg_update_id` is additionally UNIQUE in `telegram_replies`).

use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::{PgPool, Row};

use crate::notifications::SHARED_HTTP;

const POLLER_GATE_KEY: &str = "telegram.reply_poller";

/// One poll pass. Returns the number of replies filed. No-op (Ok(0)) when the
/// gate is off or telegram isn't configured.
pub async fn poll_telegram_replies_once(pool: &PgPool) -> Result<usize> {
    match ff_db::pg_get_secret(pool, POLLER_GATE_KEY).await? {
        Some(v) if v.trim().eq_ignore_ascii_case("on") => {}
        _ => return Ok(0),
    }
    let Some(token) = ff_db::pg_get_secret(pool, "openclaw.telegram_bot_token")
        .await?
        .or(ff_db::pg_get_secret(pool, "telegram_bot_token").await?)
    else {
        return Ok(0);
    };

    let offset: i64 = sqlx::query("SELECT last_update_id FROM telegram_poll_state WHERE singleton")
        .fetch_optional(pool)
        .await?
        .map(|r| r.get::<i64, _>(0))
        .unwrap_or(0);

    // Short poll (timeout=0): this runs inside a shared tick scheduler and must
    // not hold the runtime for the 25s long-poll window.
    let url = format!(
        "https://api.telegram.org/bot{token}/getUpdates?offset={}&timeout=0",
        offset + 1
    );
    let resp = SHARED_HTTP
        .get(&url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("GET telegram getUpdates")?;
    let json: serde_json::Value = resp.json().await.context("parse getUpdates response")?;
    let Some(updates) = json.get("result").and_then(|r| r.as_array()) else {
        return Ok(0);
    };

    let mut filed = 0usize;
    let mut max_update_id = offset;
    for u in updates {
        let Some(update_id) = u.get("update_id").and_then(|v| v.as_i64()) else {
            continue;
        };
        max_update_id = max_update_id.max(update_id);
        let Some(msg) = u.get("message") else {
            continue;
        };
        let Some(text) = msg.get("text").and_then(|t| t.as_str()) else {
            continue;
        };
        let Some(chat_id) = msg.pointer("/chat/id").and_then(|c| c.as_i64()) else {
            continue;
        };
        let chat_id = chat_id.to_string();
        let reply_to = msg
            .pointer("/reply_to_message/message_id")
            .and_then(|v| v.as_i64());
        let from_name = msg
            .pointer("/from/first_name")
            .and_then(|v| v.as_str())
            .unwrap_or("operator");

        // Bot commands are answered directly, never filed as replies.
        if text.trim().starts_with("/sessions") {
            let listing = sessions_listing(pool).await?;
            send_bot_response(&token, &chat_id, &listing).await;
            continue;
        }

        // Route: a reply to a recorded message inherits that message's session.
        let session_id: Option<String> = match reply_to {
            Some(rid) => sqlx::query(
                "SELECT session_id FROM telegram_messages \
                  WHERE chat_id = $1 AND tg_message_id = $2",
            )
            .bind(&chat_id)
            .bind(rid)
            .fetch_optional(pool)
            .await?
            .and_then(|r| r.get::<Option<String>, _>(0)),
            None => None,
        };

        let inserted = sqlx::query(
            "INSERT INTO telegram_replies \
                 (tg_update_id, chat_id, reply_to_tg_message_id, session_id, from_name, body) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (tg_update_id) DO NOTHING",
        )
        .bind(update_id)
        .bind(&chat_id)
        .bind(reply_to)
        .bind(&session_id)
        .bind(from_name)
        .bind(text)
        .execute(pool)
        .await?
        .rows_affected();
        if inserted > 0 {
            filed += 1;
            tracing::info!(
                update_id,
                routed_session = session_id.as_deref().unwrap_or("<unrouted>"),
                "telegram_reply_poller: filed operator reply"
            );
        }
    }

    if max_update_id > offset {
        sqlx::query(
            "INSERT INTO telegram_poll_state (singleton, last_update_id, updated_at) \
             VALUES (TRUE, $1, NOW()) \
             ON CONFLICT (singleton) DO UPDATE \
                 SET last_update_id = GREATEST(telegram_poll_state.last_update_id, $1), \
                     updated_at = NOW()",
        )
        .bind(max_update_id)
        .execute(pool)
        .await?;
    }
    Ok(filed)
}

/// `/sessions` — the sessions that sent recorded messages recently, newest
/// first. Session ids follow the `{computer}-{folder}[-{n}]` convention
/// (chosen by the sender at registration; `-{n}` only when several sessions
/// share a folder), so the listing is human-readable by construction — no
/// UUIDs shown to the operator.
async fn sessions_listing(pool: &PgPool) -> Result<String> {
    let rows: Vec<(String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT session_id, MAX(sent_at) AS last_seen FROM telegram_messages \
          WHERE session_id IS NOT NULL AND sent_at > NOW() - interval '48 hours' \
          GROUP BY session_id ORDER BY last_seen DESC",
    )
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok("no active sessions in the last 48h".to_string());
    }
    let mut out = String::from("Active sessions (last 48h):\n");
    for (sid, last) in rows {
        out.push_str(&format!("• {sid} — last update {}\n", last.format("%H:%M")));
    }
    out.push_str("\nReply to any session's message to send it your reply.");
    Ok(out)
}

/// Best-effort direct bot response (command answers). Failures are logged,
/// never propagated — a Telegram hiccup must not fail the poll tick.
async fn send_bot_response(token: &str, chat_id: &str, text: &str) {
    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    let payload = serde_json::json!({
        "chat_id": chat_id,
        "text": text,
        "disable_web_page_preview": true,
    });
    if let Err(e) = SHARED_HTTP
        .post(&url)
        .json(&payload)
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        tracing::warn!(error = %e, "telegram_reply_poller: bot response send failed");
    }
}
