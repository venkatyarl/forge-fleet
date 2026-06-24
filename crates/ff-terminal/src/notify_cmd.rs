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
    },
}

pub async fn handle_notify(cmd: NotifyCommand) -> Result<()> {
    match cmd {
        NotifyCommand::Send { message, title } => {
            if message.trim().is_empty() && title.as_deref().unwrap_or("").trim().is_empty() {
                anyhow::bail!("nothing to send — provide a message");
            }
            let pool = ff_agent::fleet_info::get_fleet_pool()
                .await
                .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;

            // send_telegram_from_secrets formats as `*<title>*\n<body>`; when
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
                .is_some();

            ff_agent::telegram::send_telegram_from_secrets(&pool, &t, &b).await?;
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
