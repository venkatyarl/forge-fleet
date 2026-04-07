//! `ff-voice` — ForgeFleet voice interface.
//!
//! Provides speech-to-text (Whisper), text-to-speech (ElevenLabs), a full-duplex
//! voice conversation pipeline, Twilio integration, audio utilities, and wake-word
//! detection for voice-first interaction with the fleet.
//!
//! # Modules
//!
//! - [`audio`] — Format detection, conversion (via ffmpeg), silence detection, WAV utils
//! - [`stt`] — Speech-to-text engines: OpenAI-compatible Whisper API + local whisper.cpp
//! - [`tts`] — Text-to-speech engines: ElevenLabs API with caching and streaming
//! - [`pipeline`] — Voice conversation pipeline: audio → STT → LLM → TTS → audio
//! - [`twilio`] — Twilio voice: TwiML generation, webhooks, outbound calls, media streams
//! - [`wake_word`] — Wake word detection via keyword spotting on transcribed text

pub mod audio;
pub mod pipeline;
pub mod stt;
pub mod tts;
pub mod twilio;
pub mod wake_word;

// ─── Error type ──────────────────────────────────────────────────────────────

/// Unified error type for all ff-voice operations.
#[derive(Debug, thiserror::Error)]
pub enum VoiceError {
    #[error("STT error: {0}")]
    Stt(String),

    #[error("TTS error: {0}")]
    Tts(String),

    #[error("audio processing error: {0}")]
    Audio(String),

    #[error("Twilio error: {0}")]
    Twilio(String),

    #[error("pipeline error: {0}")]
    Pipeline(String),

    #[error("wake word error: {0}")]
    WakeWord(String),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, VoiceError>;

// ─── Re-exports ──────────────────────────────────────────────────────────────

pub use audio::{AudioChunk, AudioFormat, SampleRate};
pub use pipeline::{
    ConversationTurn, LlmBackend, PipelineEvent, VoicePipeline, VoicePipelineConfig,
};
pub use stt::{
    LocalWhisperConfig, LocalWhisperEngine, SttEngine, Transcript, WhisperApiClient,
    WhisperApiConfig,
};
pub use tts::{ElevenLabsClient, ElevenLabsConfig, TtsCache, TtsEngine, VoiceConfig};
pub use twilio::{TwilioClient, TwilioConfig, TwimlBuilder};
pub use wake_word::{WakeWordConfig, WakeWordDetector, WakeWordEvent};

/// Crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
