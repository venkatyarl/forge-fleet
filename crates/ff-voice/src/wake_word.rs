//! Wake phrase matching utility.

use chrono::{DateTime, Utc};

use crate::{Result, VoiceError};

/// Wake-word detector configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WakeWordConfig {
    /// Accepted wake phrases (e.g. "hey taylor", "ok forge").
    pub phrases: Vec<String>,
    /// If false, matching is case-insensitive.
    pub case_sensitive: bool,
    /// If true, phrase can appear anywhere in the transcript.
    /// If false, transcript must begin with a wake phrase.
    pub allow_substring_match: bool,
    /// Strip punctuation before matching.
    pub normalize_punctuation: bool,
}

impl Default for WakeWordConfig {
    fn default() -> Self {
        Self {
            phrases: vec!["hey taylor".to_string(), "ok taylor".to_string()],
            case_sensitive: false,
            allow_substring_match: true,
            normalize_punctuation: true,
        }
    }
}

/// Event emitted when a wake phrase is detected.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WakeWordEvent {
    pub phrase: String,
    pub transcript: String,
    pub matched: bool,
    pub timestamp: DateTime<Utc>,
}

/// Utility for wake phrase detection.
#[derive(Debug, Clone)]
pub struct WakeWordDetector {
    config: WakeWordConfig,
}

impl WakeWordDetector {
    pub fn new(config: WakeWordConfig) -> Result<Self> {
        if config.phrases.is_empty() {
            return Err(VoiceError::WakeWord(
                "wake word config requires at least one phrase".to_string(),
            ));
        }

        Ok(Self { config })
    }

    pub fn config(&self) -> &WakeWordConfig {
        &self.config
    }

    fn normalize(&self, input: &str) -> String {
        let mut out = if self.config.case_sensitive {
            input.to_string()
        } else {
            input.to_ascii_lowercase()
        };

        if self.config.normalize_punctuation {
            out = out
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c.is_whitespace() {
                        c
                    } else {
                        ' '
                    }
                })
                .collect::<String>();
        }

        out.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Return an event if any phrase matches.
    pub fn detect(&self, transcript: &str) -> Option<WakeWordEvent> {
        let normalized_transcript = self.normalize(transcript);

        for phrase in &self.config.phrases {
            let normalized_phrase = self.normalize(phrase);
            let matched = if self.config.allow_substring_match {
                normalized_transcript.contains(&normalized_phrase)
            } else {
                normalized_transcript.starts_with(&normalized_phrase)
            };

            if matched {
                return Some(WakeWordEvent {
                    phrase: phrase.clone(),
                    transcript: transcript.to_string(),
                    matched: true,
                    timestamp: Utc::now(),
                });
            }
        }

        None
    }

    /// Fast bool-only check.
    pub fn is_wake_phrase(&self, transcript: &str) -> bool {
        self.detect(transcript).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_ignore_case_and_punctuation() {
        let detector = WakeWordDetector::new(WakeWordConfig::default()).unwrap();
        assert!(detector.is_wake_phrase("Hey, Taylor can you hear me?"));
    }

    #[test]
    fn supports_prefix_mode() {
        let detector = WakeWordDetector::new(WakeWordConfig {
            allow_substring_match: false,
            ..WakeWordConfig::default()
        })
        .unwrap();

        assert!(detector.is_wake_phrase("ok taylor open the pod bay doors"));
        assert!(!detector.is_wake_phrase("can you help me ok taylor"));
    }
}
