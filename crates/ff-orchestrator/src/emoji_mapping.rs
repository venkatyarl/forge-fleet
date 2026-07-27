use std::collections::HashMap;
use std::sync::LazyLock;

/// Mapping of project names to their custom emoji codes.
pub static PROJECT_EMOJI_MAPPING: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| HashMap::from([("forge-fleet", ":forge_fleet:")]));

#[cfg(test)]
mod tests {
    use super::PROJECT_EMOJI_MAPPING;

    #[test]
    fn maps_forge_fleet_to_its_custom_emoji() {
        assert_eq!(
            PROJECT_EMOJI_MAPPING.get("forge-fleet"),
            Some(&":forge_fleet:")
        );
    }
}
