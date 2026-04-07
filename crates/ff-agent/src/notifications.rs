//! Notification system — desktop notifications, webhooks (Slack/Discord/Telegram).
//!
//! Notifies users when agents complete tasks, encounter errors, or need input.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// Notification event types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationType {
    TaskCompleted,
    TaskFailed,
    UserInputNeeded,
    ReviewReady,
    AgentError,
    FleetAlert,
    SessionSaved,
}

/// A notification to send.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub notification_type: NotificationType,
    pub title: String,
    pub message: String,
    pub session_id: Option<String>,
    pub task_id: Option<String>,
}

/// Notification channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NotificationChannel {
    Desktop,
    Webhook { url: String, headers: HashMap<String, String> },
    Slack { webhook_url: String, channel: Option<String> },
    Discord { webhook_url: String },
    Telegram { bot_token: String, chat_id: String },
}

/// Notification manager.
#[derive(Debug, Clone, Default)]
pub struct NotificationManager {
    channels: Vec<NotificationChannel>,
    /// Suppression filter — skip notifications matching these types.
    suppressed: Vec<NotificationType>,
}

impl NotificationManager {
    pub fn new() -> Self { Self::default() }

    pub fn add_channel(&mut self, channel: NotificationChannel) {
        self.channels.push(channel);
    }

    pub fn suppress(&mut self, notification_type: NotificationType) {
        self.suppressed.push(notification_type);
    }

    /// Send a notification to all configured channels.
    pub async fn notify(&self, notification: &Notification) {
        if self.suppressed.contains(&notification.notification_type) {
            return;
        }

        for channel in &self.channels {
            match channel {
                NotificationChannel::Desktop => {
                    send_desktop_notification(&notification.title, &notification.message).await;
                }
                NotificationChannel::Webhook { url, headers } => {
                    send_webhook(url, headers, notification).await;
                }
                NotificationChannel::Slack { webhook_url, channel: _ } => {
                    send_slack(webhook_url, notification).await;
                }
                NotificationChannel::Discord { webhook_url } => {
                    send_discord(webhook_url, notification).await;
                }
                NotificationChannel::Telegram { bot_token, chat_id } => {
                    send_telegram(bot_token, chat_id, notification).await;
                }
            }
        }
    }
}

async fn send_desktop_notification(title: &str, message: &str) {
    // macOS: osascript
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            r#"display notification "{}" with title "{}""#,
            message.replace('"', "\\\""),
            title.replace('"', "\\\""),
        );
        let _ = tokio::process::Command::new("osascript").arg("-e").arg(&script).output().await;
    }

    // Linux: notify-send
    #[cfg(target_os = "linux")]
    {
        let _ = tokio::process::Command::new("notify-send").arg(title).arg(message).output().await;
    }

    debug!(title, "desktop notification sent");
}

async fn send_webhook(url: &str, headers: &HashMap<String, String>, notification: &Notification) {
    let client = reqwest::Client::new();
    let mut req = client.post(url).json(notification);
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    if let Err(e) = req.send().await {
        warn!(url, error = %e, "webhook notification failed");
    }
}

async fn send_slack(webhook_url: &str, notification: &Notification) {
    let payload = serde_json::json!({
        "text": format!("*{}*\n{}", notification.title, notification.message),
    });
    let client = reqwest::Client::new();
    if let Err(e) = client.post(webhook_url).json(&payload).send().await {
        warn!(error = %e, "slack notification failed");
    }
}

async fn send_discord(webhook_url: &str, notification: &Notification) {
    let payload = serde_json::json!({
        "content": format!("**{}**\n{}", notification.title, notification.message),
    });
    let client = reqwest::Client::new();
    if let Err(e) = client.post(webhook_url).json(&payload).send().await {
        warn!(error = %e, "discord notification failed");
    }
}

async fn send_telegram(bot_token: &str, chat_id: &str, notification: &Notification) {
    let url = format!("https://api.telegram.org/bot{bot_token}/sendMessage");
    let payload = serde_json::json!({
        "chat_id": chat_id,
        "text": format!("*{}*\n{}", notification.title, notification.message),
        "parse_mode": "Markdown",
    });
    let client = reqwest::Client::new();
    if let Err(e) = client.post(&url).json(&payload).send().await {
        warn!(error = %e, "telegram notification failed");
    }
}
