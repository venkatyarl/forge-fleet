//! ForgeFleet Terminal — rich interactive TUI for the ForgeFleet agent platform.
//!
//! Provides a full-featured terminal experience with:
//! - Split-panel layout (messages, fleet status, input)
//! - Tool execution cards with collapsible output
//! - Syntax highlighting for code blocks
//! - Slash command autocomplete
//! - Fleet node status display
//! - Token/context usage visualization
//! - Session management

pub mod api_client;
pub mod app;
pub mod data_cmd;
pub mod input;
pub mod jira_types;
pub mod messages;
pub mod render;
pub mod theme;
pub mod widgets;

const PROJECT_EMOJI_CODES: &[(&str, &str)] = &[("forge-fleet", "🚀"), ("hireflow360", "💼")];

/// Returns the emoji associated with a project, or the project name when unmapped.
pub fn project_emoji_code(project_name: &str) -> &str {
    PROJECT_EMOJI_CODES
        .iter()
        .find_map(|(project, emoji)| project.eq_ignore_ascii_case(project_name).then_some(*emoji))
        .unwrap_or(project_name)
}

#[cfg(test)]
mod tests {
    use super::project_emoji_code;

    #[test]
    fn maps_project_names_to_emoji_codes() {
        assert_eq!(project_emoji_code("forge-fleet"), "🚀");
        assert_eq!(project_emoji_code("HireFlow360"), "💼");
    }

    #[test]
    fn preserves_unmapped_project_names() {
        assert_eq!(project_emoji_code("unknown-project"), "unknown-project");
    }
}
