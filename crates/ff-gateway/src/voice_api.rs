//! Voice routes — wires the existing `ff-voice` library (Whisper STT,
//! ElevenLabs TTS) into the gateway's HTTP surface.
//!
//! Routes:
//!   `POST /api/voice/transcribe` — multipart `audio` file → `{text, language?}`
//!   `POST /api/voice/speak`      — JSON `{text, voice_id?}` → audio/mpeg bytes
//!
//! Pillar 2 (Voice) of the multi-LLM CLI integration roadmap. ff-voice
//! already implements STT + TTS + wake-word + Twilio bridge; this file
//! exposes two of those capabilities so the dashboard chat input can
//! grow a microphone button.
//!
//! Future work:
//!   - WebSocket `/api/voice/stream` for full-duplex live conversation.
//!   - Wake-word listener daemon spawned in `src/main.rs` (PR-V1).

use std::sync::Arc;

use axum::{
    Json,
    extract::{Multipart, State},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use ff_voice::stt::{WhisperApiClient, WhisperApiConfig, WhisperTranscriptionRequest};
use ff_voice::tts::{ElevenLabsClient, ElevenLabsConfig, TtsEngine, VoiceConfig};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::server::GatewayState;

/// `POST /api/voice/transcribe`
///
/// Multipart body:
///   * `audio` (file): the audio to transcribe (WAV, MP3, Ogg, etc.)
///   * `language` (text, optional)
///   * `prompt` (text, optional)
///   * `model` (text, optional): override Whisper model name
///
/// Backend selection: prefers `voice.whisper.api_key` if set in
/// `fleet_secrets` (uses OpenAI-compatible Whisper API). Else falls
/// back to local whisper.cpp if `voice.whisper.local_model` is set.
/// 503 if neither configured.
pub async fn transcribe(
    State(state): State<Arc<GatewayState>>,
    mut multipart: Multipart,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(pool) = state.operational_store.as_ref().and_then(|os| os.pg_pool()) else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"postgres not configured"})),
        ));
    };

    let mut audio_bytes: Option<Vec<u8>> = None;
    let mut language: Option<String> = None;
    let mut prompt: Option<String> = None;
    let mut model_override: Option<String> = None;

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("multipart: {e}")})),
        )
    })? {
        match field.name().unwrap_or("").to_string().as_str() {
            "audio" => {
                let bytes = field.bytes().await.map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": format!("read audio: {e}")})),
                    )
                })?;
                audio_bytes = Some(bytes.to_vec());
            }
            "language" => language = field.text().await.ok(),
            "prompt" => prompt = field.text().await.ok(),
            "model" => model_override = field.text().await.ok(),
            _ => {}
        }
    }

    let Some(bytes) = audio_bytes else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"missing 'audio' field"})),
        ));
    };

    // API path first (cleaner error messages, no model on disk).
    if let Some(key) = ff_db::pg_get_secret(pool, "voice.whisper.api_key")
        .await
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
    {
        let cfg = WhisperApiConfig {
            api_key: Some(key),
            model: model_override.unwrap_or_else(|| "whisper-1".to_string()),
            ..Default::default()
        };
        let client = WhisperApiClient::new(cfg.clone()).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("whisper init: {e}")})),
            )
        })?;
        let req = WhisperTranscriptionRequest {
            model: cfg.model.clone(),
            language: language.clone(),
            prompt,
            response_format: Some("json".to_string()),
            temperature: None,
        };
        let transcript = client.transcribe_with_model(&bytes, req).await.map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("transcribe failed: {e}")})),
            )
        })?;
        return Ok(Json(json!({
            "text": transcript.text,
            "language": transcript.language,
            "backend": "whisper-api",
        })));
    }

    // Local whisper.cpp fallback is a follow-up PR — the existing
    // ff-voice LocalWhisperEngine wraps a subprocess (whisper-cli),
    // which we'll wire when the cli + ggml model are present on each
    // member.
    Err((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": "no whisper backend configured",
            "hint": "set `voice.whisper.api_key` (OpenAI-compatible) via `ff secrets set voice.whisper.api_key <key>`"
        })),
    ))
}

#[derive(Debug, Deserialize)]
pub struct SpeakRequest {
    pub text: String,
    pub voice_id: Option<String>,
}

/// `POST /api/voice/speak`
///
/// JSON body: `{"text":"…","voice_id":"<elevenlabs_voice>"}`
/// Response: `audio/mpeg` bytes.
///
/// Reads `voice.elevenlabs.api_key` from fleet_secrets. 503 if unset.
pub async fn speak(
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<SpeakRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<Value>)> {
    let Some(pool) = state.operational_store.as_ref().and_then(|os| os.pg_pool()) else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"postgres not configured"})),
        ));
    };
    let api_key = ff_db::pg_get_secret(pool, "voice.elevenlabs.api_key")
        .await
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error":"voice.elevenlabs.api_key not set",
                    "hint":"set with `ff secrets set voice.elevenlabs.api_key <key>`"
                })),
            )
        })?;
    let cfg = ElevenLabsConfig::new(api_key);
    let client = ElevenLabsClient::new(cfg).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("elevenlabs init: {e}")})),
        )
    })?;
    let voice = VoiceConfig {
        voice_id: req
            .voice_id
            .unwrap_or_else(|| "EXAVITQu4vr4xnSDxMaL".to_string()),
        ..Default::default()
    };
    let chunk = client.synthesize(&req.text, &voice).await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("tts: {e}")})),
        )
    })?;

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "audio/mpeg".parse().unwrap());
    Ok((StatusCode::OK, headers, chunk.data.to_vec()))
}
