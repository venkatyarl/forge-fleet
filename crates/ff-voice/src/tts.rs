//! Text-to-speech engines.
//!
//! Includes ElevenLabs API models/client and a lightweight in-memory phrase cache.

use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use tokio::sync::RwLock;

use crate::{
    Result, VoiceError,
    audio::{AudioChunk, AudioFormat, AudioMetadata},
};

/// Voice synthesis parameters for a single request.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VoiceConfig {
    pub voice_id: String,
    pub model_id: Option<String>,
    pub output_format: Option<String>,
    pub settings: Option<ElevenLabsVoiceSettings>,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            voice_id: "EXAVITQu4vr4xnSDxMaL".to_string(),
            model_id: Some("eleven_multilingual_v2".to_string()),
            output_format: Some("mp3_44100_128".to_string()),
            settings: None,
        }
    }
}

/// TTS interface used by pipeline and integrations.
#[async_trait]
pub trait TtsEngine: Send + Sync {
    async fn synthesize(&self, text: &str, voice: &VoiceConfig) -> Result<AudioChunk>;
}

// ───────────────────────────── Cache ─────────────────────────────────────────

#[derive(Debug, Clone)]
struct CachedPhrase {
    audio: Bytes,
    created_at: DateTime<Utc>,
    hits: u64,
}

/// Simple in-memory phrase cache for repeated prompts.
#[derive(Debug, Clone)]
pub struct TtsCache {
    inner: Arc<RwLock<HashMap<String, CachedPhrase>>>,
    max_entries: usize,
}

impl Default for TtsCache {
    fn default() -> Self {
        Self::new(1_000)
    }
}

impl TtsCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            max_entries: max_entries.max(1),
        }
    }

    fn make_key(text: &str, voice: &VoiceConfig) -> String {
        format!(
            "{}|{}|{}|{}",
            voice.voice_id,
            voice.model_id.as_deref().unwrap_or_default(),
            voice.output_format.as_deref().unwrap_or_default(),
            text.trim()
        )
    }

    pub async fn get(&self, text: &str, voice: &VoiceConfig) -> Option<Bytes> {
        let key = Self::make_key(text, voice);
        let mut guard = self.inner.write().await;
        if let Some(entry) = guard.get_mut(&key) {
            entry.hits = entry.hits.saturating_add(1);
            return Some(entry.audio.clone());
        }
        None
    }

    pub async fn insert(&self, text: &str, voice: &VoiceConfig, audio: Bytes) {
        let key = Self::make_key(text, voice);
        let mut guard = self.inner.write().await;

        if guard.len() >= self.max_entries {
            // Remove oldest entry first.
            if let Some(oldest_key) = guard
                .iter()
                .min_by_key(|(_, v)| v.created_at)
                .map(|(k, _)| k.clone())
            {
                let _ = guard.remove(&oldest_key);
            }
        }

        guard.insert(
            key,
            CachedPhrase {
                audio,
                created_at: Utc::now(),
                hits: 0,
            },
        );
    }

    pub async fn clear(&self) {
        self.inner.write().await.clear();
    }

    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

// ───────────────────────────── ElevenLabs models ─────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ElevenLabsConfig {
    pub base_url: String,
    pub api_key: String,
    pub timeout_ms: u64,
}

impl ElevenLabsConfig {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            base_url: "https://api.elevenlabs.io".to_string(),
            api_key: api_key.into(),
            timeout_ms: 45_000,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct ElevenLabsVoiceSettings {
    pub stability: Option<f32>,
    pub similarity_boost: Option<f32>,
    pub style: Option<f32>,
    pub use_speaker_boost: Option<bool>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ElevenLabsTtsRequest {
    pub text: String,
    pub model_id: Option<String>,
    pub voice_settings: Option<ElevenLabsVoiceSettings>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct ElevenLabsVoice {
    pub voice_id: String,
    pub name: Option<String>,
    pub category: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct ElevenLabsVoicesResponse {
    pub voices: Vec<ElevenLabsVoice>,
}

/// ElevenLabs API client.
#[derive(Debug, Clone)]
pub struct ElevenLabsClient {
    config: ElevenLabsConfig,
    http: reqwest::Client,
    cache: TtsCache,
}

impl ElevenLabsClient {
    pub fn new(config: ElevenLabsConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .map_err(VoiceError::Http)?;

        Ok(Self {
            config,
            http,
            cache: TtsCache::default(),
        })
    }

    pub fn with_cache(config: ElevenLabsConfig, cache: TtsCache) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .map_err(VoiceError::Http)?;

        Ok(Self {
            config,
            http,
            cache,
        })
    }

    pub fn cache(&self) -> &TtsCache {
        &self.cache
    }

    pub async fn list_voices(&self) -> Result<ElevenLabsVoicesResponse> {
        let url = format!("{}/v1/voices", self.config.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .get(url)
            .header("xi-api-key", &self.config.api_key)
            .send()
            .await
            .map_err(VoiceError::Http)?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(VoiceError::Tts(format!(
                "elevenlabs list voices failed {}: {}",
                status, body
            )));
        }

        resp.json::<ElevenLabsVoicesResponse>()
            .await
            .map_err(VoiceError::Http)
    }

    async fn synthesize_raw(&self, text: &str, voice: &VoiceConfig) -> Result<Bytes> {
        let url = format!(
            "{}/v1/text-to-speech/{}",
            self.config.base_url.trim_end_matches('/'),
            voice.voice_id
        );

        let body = ElevenLabsTtsRequest {
            text: text.to_string(),
            model_id: voice.model_id.clone(),
            voice_settings: voice.settings.clone(),
        };

        let mut req = self
            .http
            .post(url)
            .header("xi-api-key", &self.config.api_key)
            .header("accept", "audio/mpeg")
            .json(&body);

        if let Some(output_format) = &voice.output_format {
            req = req.query(&[("output_format", output_format)]);
        }

        let resp = req.send().await.map_err(VoiceError::Http)?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(VoiceError::Tts(format!(
                "elevenlabs tts failed {}: {}",
                status, body
            )));
        }

        resp.bytes().await.map_err(VoiceError::Http)
    }

    fn audio_metadata_for_voice(voice: &VoiceConfig) -> AudioMetadata {
        let mut metadata = AudioMetadata {
            format: AudioFormat::Mp3,
            sample_rate: 44_100,
            channels: 1,
            bits_per_sample: 16,
        };

        if let Some(of) = voice.output_format.as_ref().map(|s| s.to_ascii_lowercase()) {
            if of.starts_with("wav") {
                metadata.format = AudioFormat::Wav;
            } else if of.starts_with("pcm") {
                metadata.format = AudioFormat::PcmS16Le;
            } else if of.starts_with("mulaw") {
                metadata.format = AudioFormat::Mulaw;
            }

            // common format pattern: mp3_44100_128
            let parts: Vec<&str> = of.split('_').collect();
            if parts.len() >= 2
                && let Ok(sr) = parts[1].parse::<u32>()
            {
                metadata.sample_rate = sr;
            }
        }

        metadata
    }
}

#[async_trait]
impl TtsEngine for ElevenLabsClient {
    async fn synthesize(&self, text: &str, voice: &VoiceConfig) -> Result<AudioChunk> {
        if text.trim().is_empty() {
            return Err(VoiceError::Tts("cannot synthesize empty text".to_string()));
        }

        let bytes = if let Some(cached) = self.cache.get(text, voice).await {
            cached
        } else {
            let fresh = self.synthesize_raw(text, voice).await?;
            self.cache.insert(text, voice, fresh.clone()).await;
            fresh
        };

        Ok(AudioChunk::new(
            0,
            bytes,
            Self::audio_metadata_for_voice(voice),
        ))
    }
}
