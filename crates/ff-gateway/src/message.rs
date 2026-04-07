use std::{collections::HashMap, fmt, str::FromStr};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Telegram,
    Discord,
    Slack,
    Web,
    Voice,
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Telegram => write!(f, "telegram"),
            Self::Discord => write!(f, "discord"),
            Self::Slack => write!(f, "slack"),
            Self::Web => write!(f, "web"),
            Self::Voice => write!(f, "voice"),
        }
    }
}

impl FromStr for Channel {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "telegram" | "tg" => Ok(Self::Telegram),
            "discord" => Ok(Self::Discord),
            "slack" => Ok(Self::Slack),
            "web" | "webchat" | "widget" => Ok(Self::Web),
            "voice" | "call" => Ok(Self::Voice),
            other => Err(format!("unsupported channel: {other}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageMediaKind {
    Image,
    Video,
    Audio,
    Document,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageMedia {
    pub kind: MessageMediaKind,
    pub url: Option<String>,
    pub file_id: Option<String>,
    pub file_name: Option<String>,
    pub mime_type: Option<String>,
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub metadata: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReactionAction {
    Add,
    Remove,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reaction {
    pub emoji: String,
    #[serde(default = "default_reaction_action")]
    pub action: ReactionAction,
    pub user_id: Option<String>,
    pub message_id: Option<String>,
}

fn default_reaction_action() -> ReactionAction {
    ReactionAction::Add
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageButton {
    pub text: String,
    pub callback_data: Option<String>,
    pub url: Option<String>,
}

impl MessageButton {
    pub fn callback(text: impl Into<String>, callback_data: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            callback_data: Some(callback_data.into()),
            url: None,
        }
    }

    pub fn link(text: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            callback_data: None,
            url: Some(url.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedCommand {
    pub prefix: char,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub raw: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingMessage {
    pub id: Uuid,
    pub external_id: Option<String>,
    pub channel: Channel,
    pub chat_id: String,
    pub thread_id: Option<String>,
    pub from_user_id: String,
    pub from_username: Option<String>,
    pub text: Option<String>,
    pub reply_to: Option<String>,
    #[serde(default)]
    pub mentions: Vec<String>,
    #[serde(default)]
    pub media: Vec<MessageMedia>,
    #[serde(default)]
    pub reactions: Vec<Reaction>,
    #[serde(default)]
    pub metadata: HashMap<String, Value>,
    pub received_at: DateTime<Utc>,
}

impl IncomingMessage {
    pub fn new(
        channel: Channel,
        chat_id: impl Into<String>,
        from_user_id: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            external_id: None,
            channel,
            chat_id: chat_id.into(),
            thread_id: None,
            from_user_id: from_user_id.into(),
            from_username: None,
            text: None,
            reply_to: None,
            mentions: Vec::new(),
            media: Vec::new(),
            reactions: Vec::new(),
            metadata: HashMap::new(),
            received_at: Utc::now(),
        }
    }

    pub fn parse_command(&self, prefixes: &[char]) -> Option<ParsedCommand> {
        let text = self.text.as_deref()?.trim();
        let mut chars = text.chars();
        let prefix = chars.next()?;
        if !prefixes.contains(&prefix) {
            return None;
        }

        let remainder = chars.as_str().trim();
        if remainder.is_empty() {
            return None;
        }

        let mut segments = remainder.split_whitespace();
        let raw_command = segments.next()?.to_string();
        let command = raw_command
            .split('@')
            .next()
            .unwrap_or(raw_command.as_str())
            .to_ascii_lowercase();
        let args = segments.map(ToString::to_string).collect::<Vec<_>>();

        Some(ParsedCommand {
            prefix,
            command,
            args,
            raw: text.to_string(),
        })
    }

    pub fn mentions_any(&self, aliases: &[String]) -> bool {
        if aliases.is_empty() {
            return false;
        }

        let text = self
            .text
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        aliases
            .iter()
            .map(|alias| alias.trim().trim_start_matches('@').to_ascii_lowercase())
            .any(|alias| {
                self.mentions
                    .iter()
                    .any(|mention| mention.eq_ignore_ascii_case(&alias))
                    || text.contains(&format!("@{alias}"))
            })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingMessage {
    pub id: Uuid,
    pub channel: Channel,
    pub chat_id: String,
    pub thread_id: Option<String>,
    pub text: Option<String>,
    pub reply_to: Option<String>,
    #[serde(default)]
    pub media: Vec<MessageMedia>,
    #[serde(default)]
    pub reactions: Vec<Reaction>,
    #[serde(default)]
    pub buttons: Vec<Vec<MessageButton>>,
    #[serde(default)]
    pub metadata: HashMap<String, Value>,
    pub created_at: DateTime<Utc>,
}

impl OutgoingMessage {
    pub fn text(channel: Channel, chat_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            channel,
            chat_id: chat_id.into(),
            thread_id: None,
            text: Some(text.into()),
            reply_to: None,
            media: Vec::new(),
            reactions: Vec::new(),
            buttons: Vec::new(),
            metadata: HashMap::new(),
            created_at: Utc::now(),
        }
    }
}
