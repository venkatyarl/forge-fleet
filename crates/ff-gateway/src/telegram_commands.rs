//! ForgeFleet Telegram bot command registry (roadmap E6).
//!
//! Single source of truth for the bot's slash commands. Powers two things:
//!   - the `/commands` reply ([`commands_help`]), and
//!   - the Telegram `setMyCommands` sync ([`crate::telegram::TelegramClient::set_my_commands`])
//!     so that typing `/` actually shows the commands.
//!
//! WHY this exists: the ForgeFleet bot previously registered NO commands (its
//! `getMyCommands` was empty), so `/` showed nothing. Listing commands here +
//! syncing on startup
//! fixes that.
//!
//! KEEP IN SYNC with the command match arms in
//! [`crate::telegram_transport`]'s `process_message`. Only list IMPLEMENTED
//! commands here so `/` never advertises a dead one.

/// One bot command shown in the Telegram `/` menu.
pub struct BotCommand {
    pub command: &'static str,
    pub description: &'static str,
}

/// Every command the ForgeFleet bot exposes today.
pub const COMMANDS: &[BotCommand] = &[
    BotCommand {
        command: "help",
        description: "Show help",
    },
    BotCommand {
        command: "commands",
        description: "List every bot command",
    },
    BotCommand {
        command: "sessions",
        description: "List coding sessions connected to this bot",
    },
    BotCommand {
        command: "computers",
        description: "List all fleet computers + stats",
    },
    BotCommand {
        command: "llms",
        description: "List all LLMs in the swarm (local + cloud)",
    },
    BotCommand {
        command: "status",
        description: "This node + routing info",
    },
    BotCommand {
        command: "threads",
        description: "List brain threads",
    },
    BotCommand {
        command: "where",
        description: "Current brain thread",
    },
];

/// Human-readable reply for `/commands`.
pub fn commands_help() -> String {
    let mut out = String::from("ForgeFleet bot commands:\n");
    for c in COMMANDS {
        out.push_str(&format!("/{} — {}\n", c.command, c.description));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_help_lists_the_user_requested_commands() {
        let help = commands_help();
        // The two the operator explicitly asked for must be present + discoverable.
        assert!(help.contains("/commands"));
        assert!(help.contains("/sessions"));
    }

    #[test]
    fn every_command_has_a_description() {
        for c in COMMANDS {
            assert!(!c.command.is_empty());
            assert!(
                !c.description.is_empty(),
                "command /{} has no description",
                c.command
            );
        }
    }
}
