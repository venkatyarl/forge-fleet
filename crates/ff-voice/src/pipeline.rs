//! Voice conversation pipeline.
//!
//! audio -> STT -> LLM -> TTS

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::Mutex;

use crate::{
    Result, VoiceError,
    audio::AudioChunk,
    stt::{SttEngine, SttRequest, Transcript},
    tts::{TtsEngine, VoiceConfig},
    wake_word::{WakeWordDetector, WakeWordEvent},
};

/// Speaker role for transcript history.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationRole {
    User,
    Assistant,
    System,
}

/// One turn in the pipeline conversation history.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ConversationTurn {
    pub role: ConversationRole,
    pub text: String,
    pub timestamp: DateTime<Utc>,
}

impl ConversationTurn {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: ConversationRole::User,
            text: text.into(),
            timestamp: Utc::now(),
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: ConversationRole::Assistant,
            text: text.into(),
            timestamp: Utc::now(),
        }
    }
}

/// Event stream emitted by a single pipeline turn.
#[derive(Debug, Clone)]
pub enum PipelineEvent {
    Transcript(Transcript),
    WakeWordDetected(WakeWordEvent),
    WakeWordIgnored { transcript: String },
    ReplyText(String),
    AudioResponse(AudioChunk),
}

/// LLM backend abstraction used by the voice pipeline.
#[async_trait]
pub trait LlmBackend: Send + Sync {
    async fn generate_reply(
        &self,
        latest_user_text: &str,
        history: &[ConversationTurn],
    ) -> Result<String>;
}

/// Basic config for pipeline orchestration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VoicePipelineConfig {
    pub require_wake_word: bool,
    pub keep_history: bool,
    pub max_history_turns: usize,
    pub tts_voice: VoiceConfig,
}

impl Default for VoicePipelineConfig {
    fn default() -> Self {
        Self {
            require_wake_word: false,
            keep_history: true,
            max_history_turns: 32,
            tts_voice: VoiceConfig::default(),
        }
    }
}

/// Full-duplex voice pipeline coordinator.
pub struct VoicePipeline {
    stt: Arc<dyn SttEngine>,
    tts: Arc<dyn TtsEngine>,
    llm: Arc<dyn LlmBackend>,
    config: VoicePipelineConfig,
    wake_word: Option<WakeWordDetector>,
    history: Mutex<Vec<ConversationTurn>>,
}

impl VoicePipeline {
    pub fn new(
        stt: Arc<dyn SttEngine>,
        tts: Arc<dyn TtsEngine>,
        llm: Arc<dyn LlmBackend>,
        config: VoicePipelineConfig,
        wake_word: Option<WakeWordDetector>,
    ) -> Self {
        Self {
            stt,
            tts,
            llm,
            config,
            wake_word,
            history: Mutex::new(Vec::new()),
        }
    }

    pub fn config(&self) -> &VoicePipelineConfig {
        &self.config
    }

    pub async fn reset_history(&self) {
        self.history.lock().await.clear();
    }

    pub async fn history_snapshot(&self) -> Vec<ConversationTurn> {
        self.history.lock().await.clone()
    }

    /// Process one incoming audio turn and emit pipeline events.
    pub async fn process_audio_turn(
        &self,
        audio: &[u8],
        stt_request: SttRequest,
    ) -> Result<Vec<PipelineEvent>> {
        if audio.is_empty() {
            return Err(VoiceError::Pipeline(
                "received empty audio for pipeline turn".to_string(),
            ));
        }

        let transcript = self.stt.transcribe(audio, stt_request).await?;
        let mut events = vec![PipelineEvent::Transcript(transcript.clone())];

        if self.config.require_wake_word {
            let detector = self.wake_word.as_ref().ok_or_else(|| {
                VoiceError::Pipeline(
                    "wake-word required by config but no detector was provided".to_string(),
                )
            })?;

            if let Some(wake_event) = detector.detect(&transcript.text) {
                events.push(PipelineEvent::WakeWordDetected(wake_event));
            } else {
                events.push(PipelineEvent::WakeWordIgnored {
                    transcript: transcript.text,
                });
                return Ok(events);
            }
        }

        let mut history = if self.config.keep_history {
            self.history.lock().await.clone()
        } else {
            Vec::new()
        };

        history.push(ConversationTurn::user(transcript.text.clone()));

        let reply = self.llm.generate_reply(&transcript.text, &history).await?;
        events.push(PipelineEvent::ReplyText(reply.clone()));

        let audio_reply = self.tts.synthesize(&reply, &self.config.tts_voice).await?;
        events.push(PipelineEvent::AudioResponse(audio_reply));

        if self.config.keep_history {
            let mut shared_history = self.history.lock().await;
            shared_history.push(ConversationTurn::user(transcript.text));
            shared_history.push(ConversationTurn::assistant(reply));
            while shared_history.len() > self.config.max_history_turns {
                shared_history.remove(0);
            }
        }

        Ok(events)
    }
}
