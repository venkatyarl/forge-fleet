use serde::{Deserialize, Serialize};

/// MinIO object storage settings, embedded in the CLI app config (`[minio]` in fleet.toml).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MinioConfig {
    /// Endpoint URL, e.g. `http://localhost:9000`.
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub access_key: Option<String>,
    #[serde(default)]
    pub secret_key: Option<String>,
    #[serde(default)]
    pub bucket: Option<String>,
    #[serde(default)]
    pub use_ssl: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_empty() {
        let cfg = MinioConfig::default();
        assert_eq!(cfg.endpoint, None);
        assert_eq!(cfg.access_key, None);
        assert_eq!(cfg.secret_key, None);
        assert_eq!(cfg.bucket, None);
        assert!(!cfg.use_ssl);
    }

    #[test]
    fn round_trips_through_toml() {
        let cfg = MinioConfig {
            endpoint: Some("http://localhost:9000".into()),
            access_key: Some("minioadmin".into()),
            secret_key: Some("minioadmin".into()),
            bucket: Some("forgefleet".into()),
            use_ssl: false,
        };
        let rendered = toml::to_string(&cfg).expect("serialize");
        let round_tripped: MinioConfig = toml::from_str(&rendered).expect("deserialize");
        assert_eq!(cfg, round_tripped);
    }

    #[test]
    fn missing_section_deserializes_to_default() {
        let cfg: MinioConfig = toml::from_str("").expect("deserialize empty");
        assert_eq!(cfg, MinioConfig::default());
    }
}
