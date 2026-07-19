//! `ff notify` — send fleet notifications through the configured channel.
//!
//! Today this is the operator's Telegram DM (bot token + chat id in
//! `fleet_secrets`, keys `openclaw.telegram_bot_token` /
//! `openclaw.telegram_chat_id`). Reuses the same sender the daemon's
//! alert/upgrade ticks use (`ff_agent::telegram::send_telegram_from_secrets`)
//! so there is ONE notification path, not a fork. If Telegram isn't
//! configured the send is a no-op (the underlying fn skips), and we say so
//! rather than pretending it went out.

use anyhow::Result;
use clap::Subcommand;

#[derive(Debug, Clone, Subcommand)]
pub enum NotifyCommand {
    /// Send a one-off message to the configured notification channel.
    Send {
        /// Message body (quote multi-word messages).
        message: String,
        /// Optional bold title prepended to the message.
        #[arg(long)]
        title: Option<String>,
        /// Record the send under this session id so an operator REPLY to this
        /// exact Telegram message is routed back to that session (see
        /// `ff notify replies`).
        #[arg(long)]
        session: Option<String>,
    },
    /// List (and optionally claim) operator Telegram replies routed to a
    /// session. Replies are filed by the leader's reply poller; claiming
    /// marks them consumed so they are delivered exactly once.
    Replies {
        /// Session id to fetch replies for. Use `--unrouted` instead for
        /// messages that were not replies to any recorded message.
        #[arg(long, required_unless_present = "unrouted")]
        session: Option<String>,
        /// Fetch messages that could not be routed to any session.
        #[arg(long)]
        unrouted: bool,
        /// Mark the returned replies as claimed (consumed).
        #[arg(long)]
        claim: bool,
    },
}

pub async fn handle_notify(cmd: NotifyCommand) -> Result<()> {
    match cmd {
        NotifyCommand::Replies {
            session,
            unrouted,
            claim,
        } => {
            let pool = ff_agent::fleet_info::get_fleet_pool()
                .await
                .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
            // --unrouted lists messages the poller could not attribute to any
            // session (session_id IS NULL); otherwise filter by the session.
            let session_filter = session.filter(|_| !unrouted);
            let rows: Vec<(i64, Option<String>, String, chrono::DateTime<chrono::Utc>)> =
                sqlx::query_as(
                    "SELECT id, from_name, body, received_at FROM telegram_replies \
                      WHERE claimed_at IS NULL \
                        AND (($1::text IS NOT NULL AND session_id = $1) \
                             OR ($1::text IS NULL AND session_id IS NULL)) \
                      ORDER BY id",
                )
                .bind(&session_filter)
                .fetch_all(&pool)
                .await?;
            let rows: Vec<(i64, String, String, chrono::DateTime<chrono::Utc>)> = rows
                .into_iter()
                .map(|(id, from, body, at)| {
                    (id, from.unwrap_or_else(|| "operator".into()), body, at)
                })
                .collect();
            if rows.is_empty() {
                println!("no unclaimed replies");
                return Ok(());
            }
            for (id, from, body, at) in &rows {
                println!("[{id}] {at} {from}: {body}");
            }
            if claim {
                let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
                sqlx::query("UPDATE telegram_replies SET claimed_at = NOW() WHERE id = ANY($1)")
                    .bind(&ids)
                    .execute(&pool)
                    .await?;
                println!("claimed {} replies", ids.len());
            }
            Ok(())
        }
        NotifyCommand::Send {
            message,
            title,
            session,
        } => {
            if message.trim().is_empty() && title.as_deref().unwrap_or("").trim().is_empty() {
                anyhow::bail!("nothing to send — provide a message");
            }
            let pool = ff_agent::fleet_info::get_fleet_pool()
                .await
                .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;

            // send_telegram_from_secrets formats as `<title>\n<body>`; when
            // there's no explicit title we send the message as the title with an
            // empty body so it renders as a single plain line.
            let (t, b) = match title {
                Some(t) => (t, message),
                None => (message, String::new()),
            };

            // Detect configured-ness for an honest report (the sender itself
            // silently skips when unconfigured).
            let configured = ff_db::pg_get_secret(&pool, "openclaw.telegram_bot_token")
                .await
                .ok()
                .flatten()
                .is_some()
                || ff_db::pg_get_secret(&pool, "telegram_bot_token")
                    .await
                    .ok()
                    .flatten()
                    .is_some();

            match session.as_deref() {
                Some(sid) => {
                    // Recorded send: an operator REPLY to this Telegram message
                    // routes back to `sid` via the leader's reply poller.
                    ff_agent::telegram::send_telegram_recorded(&pool, &t, &b, sid).await?;
                }
                None => ff_agent::telegram::send_telegram_from_secrets(&pool, &t, &b).await?,
            }
            if configured {
                println!("notification sent");
            } else {
                println!(
                    "notification skipped — Telegram not configured (set openclaw.telegram_bot_token / openclaw.telegram_chat_id)"
                );
            }
            Ok(())
        }
    }
}
