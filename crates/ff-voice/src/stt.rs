//! Speech-to-text engines.
//!
//! Includes:
//! - OpenAI-compatible Whisper HTTP client
//! - Local command adapter (e.g. whisper.cpp wrappers)

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::multipart::{Form, Part};
use tokio::io::AsyncWriteExt;

use crate::{Result, VoiceError, audio::AudioMetadata};

/// Generic STT request options used by pipeline callers.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SttRequest {
    pub language: Option<String>,
    pub prompt: Option<String>,
    pub temperature: Option<f32>,
    pub metadata: Option<AudioMetadata>,
}

/// Optional segment-level transcript metadata.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct TranscriptSegment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    pub confidence: Option<f32>,
}

/// Unified transcript output.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Transcript {
    pub text: String,
    pub language: Option<String>,
    pub confidence: Option<f32>,
    pub duration_ms: Option<u64>,
    pub segments: Vec<TranscriptSegment>,
    pub created_at: DateTime<Utc>,
}

impl Transcript {
    pub fn from_text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            language: None,
            confidence: None,
            duration_ms: None,
            segments: Vec::new(),
            created_at: Utc::now(),
        }
    }
}

/// Common trait for all speech-to-text backends.
#[async_trait]
pub trait SttEngine: Send + Sync {
    async fn transcribe(&self, audio: &[u8], request: SttRequest) -> Result<Transcript>;
}

// ───────────────────────── Whisper API (OpenAI-compatible) ──────────────────

/// Config for OpenAI-compatible Whisper HTTP APIs.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WhisperApiConfig {
    /// API base URL, e.g. `https://api.openai.com/v1` or local proxy.
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub timeout_ms: u64,
}

impl Default for WhisperApiConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: None,
            model: "whisper-1".to_string(),
            timeout_ms: 45_000,
        }
    }
}

/// OpenAI-compatible request model for `/audio/transcriptions`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct WhisperTranscriptionRequest {
    pub model: String,
    pub language: Option<String>,
    pub prompt: Option<String>,
    pub response_format: Option<String>,
    pub temperature: Option<f32>,
}

/// Basic response model returned by Whisper-compatible APIs.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct WhisperTranscriptionResponse {
    pub text: String,
    pub language: Option<String>,
}

/// HTTP Whisper client.
#[derive(Debug, Clone)]
pub struct WhisperApiClient {
    config: WhisperApiConfig,
    http: reqwest::Client,
}

impl WhisperApiClient {
    pub fn new(config: WhisperApiConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .map_err(VoiceError::Http)?;

        Ok(Self { config, http })
    }

    pub fn config(&self) -> &WhisperApiConfig {
        &self.config
    }

    fn build_endpoint(&self) -> String {
        format!(
            "{}/audio/transcriptions",
            self.config.base_url.trim_end_matches('/')
        )
    }

    fn build_multipart(
        &self,
        audio: &[u8],
        file_name: &str,
        request: &WhisperTranscriptionRequest,
    ) -> Form {
        let audio_part = Part::bytes(audio.to_vec())
            .file_name(file_name.to_string())
            .mime_str("audio/wav")
            .unwrap_or_else(|_| Part::bytes(audio.to_vec()));

        let mut form = Form::new()
            .part("file", audio_part)
            .text("model", request.model.clone());

        if let Some(v) = request.language.clone() {
            form = form.text("language", v);
        }
        if let Some(v) = request.prompt.clone() {
            form = form.text("prompt", v);
        }
        if let Some(v) = request.response_format.clone() {
            form = form.text("response_format", v);
        }
        if let Some(v) = request.temperature {
            form = form.text("temperature", v.to_string());
        }

        form
    }

    pub async fn transcribe_with_model(
        &self,
        audio: &[u8],
        request: WhisperTranscriptionRequest,
    ) -> Result<Transcript> {
        let endpoint = self.build_endpoint();
        let form = self.build_multipart(audio, "audio.wav", &request);

        let mut req = self.http.post(endpoint).multipart(form);
        if let Some(api_key) = &self.config.api_key {
            req = req.bearer_auth(api_key);
        }

        let resp = req.send().await.map_err(VoiceError::Http)?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(VoiceError::Stt(format!(
                "whisper api error {}: {}",
                status, body
            )));
        }

        let body = resp.text().await.map_err(VoiceError::Http)?;
        // Some providers return plain text when response_format=text.
        if !body.trim_start().starts_with('{') {
            return Ok(Transcript {
                text: body.trim().to_string(),
                language: request.language,
                confidence: None,
                duration_ms: None,
                segments: Vec::new(),
                created_at: Utc::now(),
            });
        }

        let parsed: WhisperTranscriptionResponse =
            serde_json::from_str(&body).map_err(VoiceError::Json)?;
        Ok(Transcript {
            text: parsed.text,
            language: parsed.language.or(request.language),
            confidence: None,
            duration_ms: None,
            segments: Vec::new(),
            created_at: Utc::now(),
        })
    }
}

#[async_trait]
impl SttEngine for WhisperApiClient {
    async fn transcribe(&self, audio: &[u8], request: SttRequest) -> Result<Transcript> {
        let model = WhisperTranscriptionRequest {
            model: self.config.model.clone(),
            language: request.language,
            prompt: request.prompt,
            response_format: Some("json".to_string()),
            temperature: request.temperature,
        };
        self.transcribe_with_model(audio, model).await
    }
}

// ───────────────────────────── Local whisper adapter ─────────────────────────

/// Config for invoking a local STT command.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LocalWhisperConfig {
    /// Binary path/name, e.g. `whisper-cli`.
    pub command: String,
    /// Args for the command.
    ///
    /// If an arg contains `{input}`, a temp audio file path will be injected.
    /// Otherwise audio bytes are piped to stdin.
    pub args: Vec<String>,
    pub working_dir: Option<PathBuf>,
    pub env: HashMap<String, String>,
    pub timeout_ms: u64,
}

impl Default for LocalWhisperConfig {
    fn default() -> Self {
        Self {
            command: "whisper-cli".to_string(),
            args: vec!["-f".to_string(), "{input}".to_string()],
            working_dir: None,
            env: HashMap::new(),
            timeout_ms: 60_000,
        }
    }
}

/// Local command-based STT engine.
#[derive(Debug, Clone)]
pub struct LocalWhisperEngine {
    config: LocalWhisperConfig,
}

impl LocalWhisperEngine {
    pub fn new(config: LocalWhisperConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &LocalWhisperConfig {
        &self.config
    }

    fn args_require_input_file(args: &[String]) -> bool {
        args.iter().any(|a| a.contains("{input}"))
    }

    fn build_temp_audio_path() -> PathBuf {
        std::env::temp_dir().join(format!("ff-voice-{}.wav", uuid::Uuid::new_v4()))
    }

    fn substitute_args(args: &[String], input_path: &Path) -> Vec<String> {
        let input = input_path.to_string_lossy();
        args.iter()
            .map(|a| a.replace("{input}", &input))
            .collect::<Vec<_>>()
    }
}

#[async_trait]
impl SttEngine for LocalWhisperEngine {
    async fn transcribe(&self, audio: &[u8], _request: SttRequest) -> Result<Transcript> {
        let use_temp_file = Self::args_require_input_file(&self.config.args);

        let temp_path = if use_temp_file {
            let p = Self::build_temp_audio_path();
            tokio::fs::write(&p, audio)
                .await
                .map_err(|e| VoiceError::Stt(format!("failed to write temp audio file: {e}")))?;
            Some(p)
        } else {
            None
        };

        let mut cmd = tokio::process::Command::new(&self.config.command);
        if let Some(path) = temp_path.as_ref() {
            cmd.args(Self::substitute_args(&self.config.args, path));
        } else {
            cmd.args(&self.config.args);
        }

        if let Some(dir) = &self.config.working_dir {
            cmd.current_dir(dir);
        }
        for (k, v) in &self.config.env {
            cmd.env(k, v);
        }

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        if !use_temp_file {
            cmd.stdin(Stdio::piped());
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| VoiceError::Stt(format!("failed to spawn local whisper command: {e}")))?;

        if !use_temp_file && let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(audio)
                .await
                .map_err(|e| VoiceError::Stt(format!("failed to write audio to stdin: {e}")))?;
            stdin
                .shutdown()
                .await
                .map_err(|e| VoiceError::Stt(format!("stdin shutdown failed: {e}")))?;
        }

        let output = tokio::time::timeout(
            Duration::from_millis(self.config.timeout_ms),
            child.wait_with_output(),
        )
        .await
        .map_err(|_| VoiceError::Stt("local whisper command timed out".to_string()))
        .and_then(|res| {
            res.map_err(|e| VoiceError::Stt(format!("local whisper command failed: {e}")))
        })?;

        if let Some(path) = temp_path {
            let _ = tokio::fs::remove_file(path).await;
        }

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(VoiceError::Stt(format!(
                "local whisper exited with status {}: {}",
                output.status, stderr
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if stdout.is_empty() {
            return Err(VoiceError::Stt(
                "local whisper returned empty transcript".to_string(),
            ));
        }

        if stdout.starts_with('{') {
            let parsed: WhisperTranscriptionResponse =
                serde_json::from_str(&stdout).map_err(VoiceError::Json)?;
            return Ok(Transcript {
                text: parsed.text,
                language: parsed.language,
                confidence: None,
                duration_ms: None,
                segments: Vec::new(),
                created_at: Utc::now(),
            });
        }

        Ok(Transcript::from_text(stdout))
    }
}
