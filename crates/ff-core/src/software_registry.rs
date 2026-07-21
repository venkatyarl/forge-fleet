//! Management service for the software registry schema.

use std::collections::HashMap;

use crate::schema::software::SoftwareEntry;

/// In-memory manager for software registry entries and detection results.
#[derive(Debug, Default)]
pub struct SoftwareRegistry {
    entries: Vec<SoftwareEntry>,
}

impl SoftwareRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the registry contents with `entries`.
    pub fn load_entries(&mut self, entries: Vec<SoftwareEntry>) {
        self.entries = entries;
    }

    /// Return all loaded entries.
    pub fn entries(&self) -> &[SoftwareEntry] {
        &self.entries
    }

    /// Apply detected versions and return the entries that need an upgrade.
    ///
    /// Detection results are keyed by [`SoftwareEntry::name`]. Entries without
    /// a detection result retain their previously known `current_version`.
    pub fn check_for_updates(
        &mut self,
        detected_versions: &HashMap<String, String>,
    ) -> Vec<&SoftwareEntry> {
        for entry in &mut self.entries {
            if let Some(version) = detected_versions.get(&entry.name) {
                entry.current_version.clone_from(version);
            }
        }

        self.list_pending_upgrades()
    }

    /// List entries whose detected/current version differs from the desired version.
    pub fn list_pending_upgrades(&self) -> Vec<&SoftwareEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.current_version != entry.desired_version)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::software::{OS, SourceType};

    fn entry(name: &str, current_version: &str, desired_version: &str) -> SoftwareEntry {
        SoftwareEntry {
            name: name.into(),
            current_version: current_version.into(),
            desired_version: desired_version.into(),
            os: OS::Linux,
            source_type: SourceType::Binary,
            detection_cmd: format!("{name} --version"),
            upgrade_cmd: format!("upgrade {name}"),
        }
    }

    #[test]
    fn loads_and_lists_pending_upgrades() {
        let mut registry = SoftwareRegistry::new();
        registry.load_entries(vec![entry("ff", "1.0", "1.1"), entry("git", "2", "2")]);

        assert_eq!(registry.entries().len(), 2);
        assert_eq!(registry.list_pending_upgrades()[0].name, "ff");
    }

    #[test]
    fn detection_results_update_schema_entries() {
        let mut registry = SoftwareRegistry::new();
        registry.load_entries(vec![entry("ff", "unknown", "1.1"), entry("git", "2", "2")]);
        let detected = HashMap::from([
            ("ff".to_owned(), "1.1".to_owned()),
            ("git".to_owned(), "2.1".to_owned()),
        ]);

        let pending = registry.check_for_updates(&detected);

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].name, "git");
        assert_eq!(registry.entries()[0].current_version, "1.1");
    }

    #[test]
    fn missing_detection_preserves_current_version() {
        let mut registry = SoftwareRegistry::new();
        registry.load_entries(vec![entry("ff", "1.0", "1.1")]);

        let pending = registry.check_for_updates(&HashMap::new());

        assert_eq!(pending.len(), 1);
        assert_eq!(registry.entries()[0].current_version, "1.0");
    }
}
