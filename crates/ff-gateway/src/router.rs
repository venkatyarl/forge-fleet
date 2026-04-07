use serde::{Deserialize, Serialize};

use crate::message::{IncomingMessage, ParsedCommand};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RouteTarget {
    Chat,
    Command,
    ToolExecution,
    Ignore,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecutionIntent {
    pub tool: String,
    pub payload: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutedMessage {
    pub target: RouteTarget,
    pub mentions_bot: bool,
    pub command: Option<ParsedCommand>,
    pub tool: Option<ToolExecutionIntent>,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct MessageRouter {
    bot_aliases: Vec<String>,
    command_prefixes: Vec<char>,
}

impl Default for MessageRouter {
    fn default() -> Self {
        Self::new(
            vec!["forgefleet".to_string(), "taylor".to_string()],
            vec!['/', '!'],
        )
    }
}

impl MessageRouter {
    pub fn new(bot_aliases: Vec<String>, command_prefixes: Vec<char>) -> Self {
        let command_prefixes = if command_prefixes.is_empty() {
            vec!['/', '!']
        } else {
            command_prefixes
        };

        Self {
            bot_aliases,
            command_prefixes,
        }
    }

    pub fn route(&self, message: &IncomingMessage) -> RoutedMessage {
        let mentions_bot = message.mentions_any(&self.bot_aliases);

        if let Some(command) = message.parse_command(&self.command_prefixes) {
            if let Some(tool) = self.tool_intent_from_command(&command) {
                return RoutedMessage {
                    target: RouteTarget::ToolExecution,
                    mentions_bot,
                    command: Some(command),
                    tool: Some(tool),
                    reason: "command mapped to tool execution".to_string(),
                };
            }

            return RoutedMessage {
                target: RouteTarget::Command,
                mentions_bot,
                command: Some(command),
                tool: None,
                reason: "message starts with command prefix".to_string(),
            };
        }

        let text = message.text.as_deref().unwrap_or_default().trim();
        if text.is_empty() && !message.reactions.is_empty() {
            return RoutedMessage {
                target: RouteTarget::Ignore,
                mentions_bot,
                command: None,
                tool: None,
                reason: "reaction-only event".to_string(),
            };
        }

        if let Some(tool) = self.inline_tool_intent(text) {
            return RoutedMessage {
                target: RouteTarget::ToolExecution,
                mentions_bot,
                command: None,
                tool: Some(tool),
                reason: "message uses inline tool syntax".to_string(),
            };
        }

        RoutedMessage {
            target: RouteTarget::Chat,
            mentions_bot,
            command: None,
            tool: None,
            reason: if mentions_bot {
                "message mentions bot".to_string()
            } else {
                "default conversational route".to_string()
            },
        }
    }

    fn tool_intent_from_command(&self, command: &ParsedCommand) -> Option<ToolExecutionIntent> {
        let tool_command = matches!(command.command.as_str(), "tool" | "run" | "exec" | "bash");

        if !tool_command {
            return None;
        }

        let tool = command
            .args
            .first()
            .cloned()
            .unwrap_or_else(|| "shell".to_string());
        let payload = command
            .args
            .iter()
            .skip(1)
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");

        Some(ToolExecutionIntent { tool, payload })
    }

    fn inline_tool_intent(&self, text: &str) -> Option<ToolExecutionIntent> {
        let normalized = text.trim();
        let prefixes = ["tool:", "run:", "exec:"];
        let prefix = prefixes
            .iter()
            .find(|candidate| normalized.to_ascii_lowercase().starts_with(**candidate))?;

        let payload = normalized[prefix.len()..].trim();
        if payload.is_empty() {
            return None;
        }

        let mut parts = payload.splitn(2, ' ');
        let tool = parts
            .next()
            .map(ToString::to_string)
            .unwrap_or_else(|| "shell".to_string());
        let payload = parts.next().unwrap_or_default().trim().to_string();

        Some(ToolExecutionIntent { tool, payload })
    }
}
