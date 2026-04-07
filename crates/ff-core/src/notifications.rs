//! Notification delivery primitives for ForgeFleet.
//!
//! Provides:
//! - [`NotificationSender`] trait (async)
//! - [`TelegramNotifier`] implementation via Telegram Bot API
//! - Severity-aware formatting with emoji prefixes
//! - Per-event rate limiting (default: 1 message per event type / 5 minutes)
//! - Batch summary mode (default: collect events for 30 seconds)

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::warn;

use crate::config::{NotificationsConfig, TelegramNotification};
use crate::error::{ForgeFleetError, Result};

const TELEGRAM_API_BASE: &str = "https://api.telegram.org";

/// Notification severity used across monitoring and alerting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotificationLevel {
    Critical,
    Warning,
    Info,
    Report,
}

impl NotificationLevel {
    /// Emoji prefix used in user-facing messages.
    pub fn emoji(self) -> &'static str {
        match self {
            Self::Critical => "🔴",
            Self::Warning => "🟡",
            Self::Info => "🟢",
            Self::Report => "📊",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::Warning => "warning",
            Self::Info => "info",
            Self::Report => "report",
        }
    }
}

impl std::fmt::Display for NotificationLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Contract for sending notifications to one or more channels.
#[async_trait]
pub trait NotificationSender: Send + Sync {
    /// Send a notification event.
    async fn send(&self, level: NotificationLevel, title: &str, body: &str) -> Result<()>;
}

#[derive(Debug, Clone)]
struct NotificationEvent {
    level: NotificationLevel,
    title: String,
    body: String,
}

#[derive(Default, Debug)]
struct NotifierState {
    pending: Vec<NotificationEvent>,
    last_sent_by_event_type: HashMap<String, Instant>,
    flush_scheduled: bool,
}

/// Telegram notification sender.
///
/// Uses `POST https://api.telegram.org/bot{token}/sendMessage`.
#[derive(Clone)]
pub struct TelegramNotifier {
    bot_token: String,
    chat_id: String,
    channel: Option<String>,
    api_base: String,
    client: reqwest::Client,
    rate_limit_window: Duration,
    batch_window: Duration,
    state: Arc<Mutex<NotifierState>>,
}

impl TelegramNotifier {
    /// Create a notifier from raw bot token + chat ID.
    pub fn new(bot_token: impl Into<String>, chat_id: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| {
                ForgeFleetError::Runtime(format!("failed to create reqwest client: {error}"))
            })?;

        Ok(Self {
            bot_token: bot_token.into(),
            chat_id: chat_id.into(),
            channel: Some("telegram".to_string()),
            api_base: TELEGRAM_API_BASE.to_string(),
            client,
            rate_limit_window: Duration::from_secs(5 * 60),
            batch_window: Duration::from_secs(30),
            state: Arc::new(Mutex::new(NotifierState::default())),
        })
    }

    /// Build from loaded fleet notification config.
    pub fn from_config(
        config: &NotificationsConfig,
        bot_token: impl Into<String>,
    ) -> Result<Option<Self>> {
        let Some(telegram) = config.telegram.as_ref() else {
            return Ok(None);
        };
        let mut notifier = Self::new(bot_token, telegram.chat_id.clone())?;
        notifier.channel = telegram.channel.clone();
        Ok(Some(notifier))
    }

    /// Build directly from telegram config section.
    pub fn from_telegram_config(
        telegram: &TelegramNotification,
        bot_token: impl Into<String>,
    ) -> Result<Self> {
        let mut notifier = Self::new(bot_token, telegram.chat_id.clone())?;
        notifier.channel = telegram.channel.clone();
        Ok(notifier)
    }

    /// Override Telegram API base URL (useful for testing/proxy).
    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Self {
        self.api_base = api_base.into();
        self
    }

    /// Override rate limit and batch windows.
    pub fn with_windows(mut self, rate_limit_window: Duration, batch_window: Duration) -> Self {
        self.rate_limit_window = rate_limit_window;
        self.batch_window = batch_window;
        self
    }

    /// Flush queued events immediately.
    pub async fn flush_pending(&self) -> Result<()> {
        let pending = {
            let mut state = self.state.lock().await;
            state.flush_scheduled = false;
            std::mem::take(&mut state.pending)
        };

        if pending.is_empty() {
            return Ok(());
        }

        let text = self.build_batch_message(&pending);
        self.send_telegram_message(&text).await
    }

    fn build_batch_message(&self, events: &[NotificationEvent]) -> String {
        if events.len() == 1 {
            let event = &events[0];
            return format_single_event(event.level, &event.title, &event.body);
        }

        let mut critical = 0usize;
        let mut warning = 0usize;
        let mut info = 0usize;
        let mut report = 0usize;

        for event in events {
            match event.level {
                NotificationLevel::Critical => critical += 1,
                NotificationLevel::Warning => warning += 1,
                NotificationLevel::Info => info += 1,
                NotificationLevel::Report => report += 1,
            }
        }

        let mut lines = Vec::with_capacity(events.len() + 3);
        lines.push(format!(
            "📊 ForgeFleet notification summary ({} events in {}s)",
            events.len(),
            self.batch_window.as_secs()
        ));
        lines.push(format!(
            "Counts: 🔴{}  🟡{}  🟢{}  📊{}",
            critical, warning, info, report
        ));
        lines.push(String::new());

        for event in events {
            lines.push(format!(
                "{} {} — {}",
                event.level.emoji(),
                event.title,
                truncate_line(&event.body, 220)
            ));
        }

        lines.join("\n")
    }

    async fn send_telegram_message(&self, text: &str) -> Result<()> {
        let endpoint = format!(
            "{}/bot{}/sendMessage",
            self.api_base.trim_end_matches('/'),
            self.bot_token
        );

        let response = self
            .client
            .post(endpoint)
            .json(&serde_json::json!({
                "chat_id": self.chat_id,
                "text": text,
                "disable_web_page_preview": true,
            }))
            .send()
            .await
            .map_err(|error| {
                ForgeFleetError::Runtime(format!("telegram request failed: {error}"))
            })?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<failed to read error body>".to_string());
            return Err(ForgeFleetError::Runtime(format!(
                "telegram API returned {}: {}",
                status, body
            )));
        }

        let parsed: TelegramApiResponse = response.json().await.map_err(|error| {
            ForgeFleetError::Runtime(format!("telegram response parse failed: {error}"))
        })?;

        if parsed.ok {
            Ok(())
        } else {
            Err(ForgeFleetError::Runtime(format!(
                "telegram API reported failure: {}",
                parsed
                    .description
                    .unwrap_or_else(|| "unknown telegram error".to_string())
            )))
        }
    }

    async fn queue_event(&self, event: NotificationEvent) -> bool {
        let mut state = self.state.lock().await;
        let event_type_key = normalize_event_key(&event.title);

        if let Some(last_sent) = state.last_sent_by_event_type.get(&event_type_key)
            && last_sent.elapsed() < self.rate_limit_window
        {
            return false;
        }

        state
            .last_sent_by_event_type
            .insert(event_type_key, Instant::now());

        state.pending.push(event);

        if !state.flush_scheduled {
            state.flush_scheduled = true;
            return true;
        }

        false
    }

    #[cfg(test)]
    async fn pending_len_for_test(&self) -> usize {
        let state = self.state.lock().await;
        state.pending.len()
    }
}

#[async_trait]
impl NotificationSender for TelegramNotifier {
    async fn send(&self, level: NotificationLevel, title: &str, body: &str) -> Result<()> {
        if title.trim().is_empty() {
            return Err(ForgeFleetError::Config(
                "notification title cannot be empty".to_string(),
            ));
        }

        let event = NotificationEvent {
            level,
            title: title.trim().to_string(),
            body: body.trim().to_string(),
        };

        let should_schedule_flush = self.queue_event(event).await;

        if should_schedule_flush {
            let notifier = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(notifier.batch_window).await;
                if let Err(error) = notifier.flush_pending().await {
                    warn!(error = %error, "failed to flush telegram notifications");
                }
            });
        }

        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct TelegramApiResponse {
    ok: bool,
    #[serde(default)]
    description: Option<String>,
}

fn normalize_event_key(title: &str) -> String {
    title.trim().to_lowercase()
}

fn truncate_line(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let mut out = input.chars().take(max_chars).collect::<String>();
    out.push('…');
    out
}

/// Format a single event into user-facing telegram text.
pub fn format_single_event(level: NotificationLevel, title: &str, body: &str) -> String {
    if body.trim().is_empty() {
        return format!("{} {}", level.emoji(), title.trim());
    }

    format!("{} {}\n{}", level.emoji(), title.trim(), body.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_formatting_has_expected_emojis() {
        let critical = format_single_event(
            NotificationLevel::Critical,
            "Node went offline",
            "marcus did not respond",
        );
        let warning = format_single_event(NotificationLevel::Warning, "High memory", "89% used");
        let info = format_single_event(NotificationLevel::Info, "Node recovered", "james is back");
        let report = format_single_event(NotificationLevel::Report, "Hourly summary", "all good");

        assert!(critical.starts_with("🔴"));
        assert!(warning.starts_with("🟡"));
        assert!(info.starts_with("🟢"));
        assert!(report.starts_with("📊"));
    }

    #[tokio::test]
    async fn test_rate_limiting_allows_one_event_per_window() {
        let notifier = TelegramNotifier::new("test-token", "12345")
            .unwrap()
            .with_windows(Duration::from_secs(300), Duration::from_secs(3600));

        notifier
            .send(
                NotificationLevel::Critical,
                "Node went offline",
                "taylor unreachable",
            )
            .await
            .unwrap();

        notifier
            .send(
                NotificationLevel::Critical,
                "Node went offline",
                "taylor unreachable again",
            )
            .await
            .unwrap();

        assert_eq!(notifier.pending_len_for_test().await, 1);
    }

    #[test]
    fn test_batch_message_summary_contains_counts() {
        let notifier = TelegramNotifier::new("token", "chat")
            .unwrap()
            .with_windows(Duration::from_secs(300), Duration::from_secs(30));

        let events = vec![
            NotificationEvent {
                level: NotificationLevel::Critical,
                title: "Node offline".to_string(),
                body: "james".to_string(),
            },
            NotificationEvent {
                level: NotificationLevel::Warning,
                title: "Disk high".to_string(),
                body: "84%".to_string(),
            },
        ];

        let text = notifier.build_batch_message(&events);
        assert!(text.contains("ForgeFleet notification summary"));
        assert!(text.contains("🔴1"));
        assert!(text.contains("🟡1"));
        assert!(text.contains("Node offline"));
    }
}
