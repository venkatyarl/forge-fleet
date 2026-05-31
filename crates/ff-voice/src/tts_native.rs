//! Native macOS text-to-speech via the `say` command.
//!
//! This is the Phase-1 JARVIS voice output: it shells out to `say -v <voice>`
//! which plays straight to the default audio device. Unlike the ElevenLabs
//! engine, `say` produces no audio bytes for the caller — it plays directly.
//!
//! The shared [`TtsEngine`] trait is
//! `async fn synthesize(&self, text, voice) -> Result<AudioChunk>`. Since `say`
//! returns no bytes, [`SayTts`] satisfies the contract by returning an **empty**
//! [`AudioChunk`] (zero-length data, sequence 0) after the process exits. The
//! `voice: &VoiceConfig` arg from the trait is ignored — `SayTts` carries its
//! own macOS voice name (default `"Daniel"`) and optional rate.

use async_trait::async_trait;
use bytes::Bytes;

use crate::{
    Result, VoiceError,
    audio::{AudioChunk, AudioMetadata},
    tts::{TtsEngine, VoiceConfig},
};

/// macOS `say`-backed TTS engine.
#[derive(Debug, Clone)]
pub struct SayTts {
    /// macOS voice name passed to `say -v`. Defaults to `"Daniel"`.
    pub voice: String,
    /// Optional words-per-minute rate passed to `say -r`.
    pub rate: Option<u32>,
}

impl Default for SayTts {
    fn default() -> Self {
        Self {
            voice: "Daniel".to_string(),
            rate: None,
        }
    }
}

impl SayTts {
    pub fn new(voice: impl Into<String>, rate: Option<u32>) -> Self {
        Self {
            voice: voice.into(),
            rate,
        }
    }

    /// Speak `text` via `say` and wait for playback to finish.
    ///
    /// Convenience wrapper around the trait method for callers that don't have
    /// a [`VoiceConfig`] handy (e.g. the `ff voice` loop). Returns once the
    /// `say` process exits.
    pub async fn speak(&self, text: &str) -> Result<()> {
        self.run_say(text).await
    }

    async fn run_say(&self, text: &str) -> Result<()> {
        if text.trim().is_empty() {
            return Err(VoiceError::Tts("cannot speak empty text".to_string()));
        }

        let mut cmd = tokio::process::Command::new("say");
        cmd.args(["-v", &self.voice]);
        if let Some(rate) = self.rate {
            cmd.args(["-r", &rate.to_string()]);
        }
        cmd.arg(text);

        let status = cmd
            .status()
            .await
            .map_err(|e| VoiceError::Tts(format!("failed to spawn `say`: {e}")))?;

        if !status.success() {
            return Err(VoiceError::Tts(format!(
                "`say` exited with status {status}"
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl TtsEngine for SayTts {
    async fn synthesize(&self, text: &str, _voice: &VoiceConfig) -> Result<AudioChunk> {
        // `say` plays directly to the audio device; there are no bytes to hand
        // back. Run it, then return an empty AudioChunk to satisfy the trait.
        self.run_say(text).await?;
        Ok(AudioChunk::new(0, Bytes::new(), AudioMetadata::default()))
    }
}
