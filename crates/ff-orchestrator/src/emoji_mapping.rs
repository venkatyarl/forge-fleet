//! Custom logo emoji configuration for known projects.

use std::collections::HashMap;
use std::sync::LazyLock;

/// Project names mapped to the custom emoji codes used for their logos.
pub static PROJECT_EMOJI_MAPPING: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| {
        HashMap::from([
            ("forge-fleet", ":forge_fleet:"),
            ("hireflow360", ":hireflow360:"),
        ])
    });

/// Return the custom logo emoji configured for a project.
pub fn project_emoji_code(project_name: &str) -> Option<&'static str> {
    let normalized = project_name.to_ascii_lowercase();
    PROJECT_EMOJI_MAPPING.get(normalized.as_str()).copied()
}

#[cfg(test)]
mod tests {
    use super::{PROJECT_EMOJI_MAPPING, project_emoji_code};

    #[test]
    fn loads_project_emoji_mapping() {
        assert_eq!(project_emoji_code("forge-fleet"), Some(":forge_fleet:"));
        assert_eq!(project_emoji_code("Forge-Fleet"), Some(":forge_fleet:"));
        assert_eq!(project_emoji_code("HireFlow360"), Some(":hireflow360:"));
        assert_eq!(project_emoji_code("unknown"), None);
        assert_eq!(
            PROJECT_EMOJI_MAPPING.get("forge-fleet"),
            Some(&":forge_fleet:")
        );
    }
}
