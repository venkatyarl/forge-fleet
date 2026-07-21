//! Software registry schema.

use serde::{Deserialize, Serialize};

/// Operating system targeted by a software registry entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OS {
    MacOS,
    Linux,
    Windows,
}

/// Mechanism used to obtain and update a software package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceType {
    PackageManager,
    Binary,
    Source,
}

/// A software package tracked by the fleet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoftwareEntry {
    pub name: String,
    pub current_version: String,
    pub desired_version: String,
    pub os: OS,
    pub source_type: SourceType,
    pub detection_cmd: String,
    pub upgrade_cmd: String,
}

impl SoftwareEntry {
    /// Serialize this entry as JSON.
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }

    /// Deserialize an entry from JSON.
    pub fn from_json(json: &str) -> serde_json::Result<Self> {
        serde_json::from_str(json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trip_preserves_entry() {
        let entry = SoftwareEntry {
            name: "forgefleet".into(),
            current_version: "1.0.0".into(),
            desired_version: "1.1.0".into(),
            os: OS::Linux,
            source_type: SourceType::Binary,
            detection_cmd: "ff --version".into(),
            upgrade_cmd: "ff update".into(),
        };

        let json = entry.to_json().expect("serialize software entry");
        assert_eq!(SoftwareEntry::from_json(&json).unwrap(), entry);
        assert!(json.contains("\"source_type\":\"binary\""));
    }
}
