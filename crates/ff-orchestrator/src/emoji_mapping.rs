//! Custom logo emoji configuration for known projects.

use std::collections::HashMap;
use std::sync::LazyLock;

/// Project names mapped to the custom emoji codes used for their logos.
pub static PROJECT_EMOJI_MAPPING: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| HashMap::from([("forge-fleet", ":forge_fleet:")]));

/// Return the custom logo emoji configured for a project.
pub fn project_emoji_code(project_name: &str) -> Option<&'static str> {
    PROJECT_EMOJI_MAPPING.get(project_name).copied()
}

#[cfg(test)]
mod tests {
    use super::{PROJECT_EMOJI_MAPPING, project_emoji_code};

    #[test]
    fn loads_project_emoji_mapping() {
        assert_eq!(project_emoji_code("forge-fleet"), Some(":forge_fleet:"));
        assert_eq!(project_emoji_code("unknown"), None);
        assert_eq!(
            PROJECT_EMOJI_MAPPING.get("forge-fleet"),
            Some(&":forge_fleet:")
        );
    }
}
