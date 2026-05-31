//! `ff voice` — Phase-1 native JARVIS voice loop.
//!
//! Pipeline: mic capture → energy-VAD utterance → STT (whisper.cpp) →
//! wake-word "jarvis" → strip wake-word → POST the live gateway
//! `/api/jarvis/ask` → speak the answer with macOS `say -v Daniel`.
//!
//! Reuses the existing `ff-voice` crate: [`ff_voice::MicCapture`] for capture+VAD,
//! [`ff_voice::LocalWhisperEngine`] for STT, [`ff_voice::WakeWordDetector`] for the
//! wake word, and [`ff_voice::SayTts`] for native output. Nothing in `ff-voice` is
//! modified — this only wires the pieces together.
//!
//! **macOS only.** Mic capture goes through cpal's CoreAudio backend, and the loop
//! runs on the leader (Taylor) where the operator sits. On other platforms the
//! subcommand exists but returns a clear "macOS only" error (the cpal/ALSA stack
//! isn't compiled on Linux/CI — see `ff-voice/Cargo.toml`).

use anyhow::Result;

/// Gateway request body — matches the gateway's `AskReq` struct (field `query`).
#[cfg(target_os = "macos")]
#[derive(serde::Serialize)]
struct AskReq {
    query: String,
}

/// Gateway response — we only need the `answer` string field.
#[cfg(target_os = "macos")]
#[derive(serde::Deserialize)]
struct AskResp {
    #[serde(default)]
    answer: String,
}

/// Run the voice loop.
///
/// * `device`      — input device name substring (e.g. "C920"); None = default.
/// * `model`       — ggml whisper model path (tilde-expanded by caller).
/// * `gateway`     — gateway base URL, e.g. `http://localhost:51002`.
/// * `voice`       — macOS `say` voice name (default "Daniel").
/// * `once`        — process a single answered utterance, then return.
/// * `whisper_cli` — whisper-cli binary path/name (resolved on PATH).
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
pub async fn handle_voice(
    device: Option<String>,
    model: String,
    gateway: String,
    voice: String,
    once: bool,
    whisper_cli: String,
) -> Result<()> {
    use crate::{CYAN, GREEN, RESET, YELLOW};
    use anyhow::Context;
    use ff_voice::{
        LocalWhisperConfig, LocalWhisperEngine, MicCapture, MicCaptureConfig, SayTts, SttEngine,
        WakeWordConfig, WakeWordDetector, stt::SttRequest,
    };

    eprintln!(
        "{CYAN}▶ ff voice{RESET}  \x1b[2mgateway={gateway} model={model} voice={voice} \
         whisper-cli={whisper_cli}{RESET}"
    );

    // ── STT: whisper.cpp via LocalWhisperEngine ────────────────────────────
    // LocalWhisperConfig fields: command, args (with `{input}` placeholder),
    // working_dir, env, timeout_ms. whisper-cli flags:
    //   -m <model>  -f {input}  -otxt (write .txt)  -nt (no timestamps).
    // We capture stdout for the transcript; -nt keeps it clean.
    let stt = LocalWhisperEngine::new(LocalWhisperConfig {
        command: whisper_cli.clone(),
        args: vec![
            "-m".to_string(),
            model.clone(),
            "-f".to_string(),
            "{input}".to_string(),
            "-otxt".to_string(),
            "-nt".to_string(),
        ],
        ..Default::default()
    });

    // ── Wake word: "jarvis" anywhere in the transcript ─────────────────────
    let detector = WakeWordDetector::new(WakeWordConfig {
        phrases: vec!["jarvis".to_string()],
        case_sensitive: false,
        allow_substring_match: true,
        normalize_punctuation: true,
    })
    .context("build wake-word detector")?;

    // ── Native TTS output ──────────────────────────────────────────────────
    let tts = SayTts::new(voice, None);

    // ── HTTP client for the gateway ────────────────────────────────────────
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .context("build http client")?;
    let ask_url = format!("{}/api/jarvis/ask", gateway.trim_end_matches('/'));

    // ── Mic capture (dedicated thread owns the !Send cpal stream) ──────────
    let mic = MicCapture::new(MicCaptureConfig {
        device,
        ..Default::default()
    });
    let (mut utterances, _capture) = mic
        .start()
        .map_err(|e| anyhow::anyhow!("start mic capture: {e}"))?;

    eprintln!("{GREEN}● listening{RESET}  \x1b[2m(say \"jarvis ...\"; Ctrl-C to stop){RESET}");

    while let Some(pcm) = utterances.recv().await {
        eprintln!(
            "{CYAN}[heard]{RESET} \x1b[2m{} samples (~{} ms){RESET}",
            pcm.len(),
            pcm.len() as u64 * 1000 / ff_voice::TARGET_SAMPLE_RATE as u64
        );

        // Encode the 16 kHz mono PCM to WAV bytes; LocalWhisperEngine writes a
        // temp `.wav` for the `{input}` arg internally.
        let wav = match ff_voice::encode_wav_bytes(&pcm) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("{YELLOW}[warn]{RESET} encode wav failed: {e}");
                continue;
            }
        };

        let transcript = match stt.transcribe(&wav, SttRequest::default()).await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{YELLOW}[warn]{RESET} stt failed: {e}");
                continue;
            }
        };
        let text = transcript.text.trim().to_string();
        if text.is_empty() {
            continue;
        }
        eprintln!("{CYAN}[transcript]{RESET} {text}");

        // Wake-word gate.
        let Some(event) = detector.detect(&text) else {
            eprintln!("\x1b[2m[no-wake] ignoring{RESET}");
            continue;
        };
        eprintln!("{GREEN}[wake]{RESET} matched '{}'", event.phrase);

        // Strip the leading "jarvis[,]" token from the query.
        let query = strip_wake_word(&text);
        if query.is_empty() {
            eprintln!("\x1b[2m[wake] nothing after wake word; waiting{RESET}");
            continue;
        }

        // POST to the gateway.
        eprintln!("{CYAN}[asking]{RESET} {query}");
        let resp = http
            .post(&ask_url)
            .json(&AskReq {
                query: query.clone(),
            })
            .send()
            .await;
        let answer = match resp {
            Ok(r) => {
                let status = r.status();
                if !status.is_success() {
                    let body = r.text().await.unwrap_or_default();
                    eprintln!("{YELLOW}[warn]{RESET} gateway {status}: {body}");
                    continue;
                }
                match r.json::<AskResp>().await {
                    Ok(a) => a.answer,
                    Err(e) => {
                        eprintln!("{YELLOW}[warn]{RESET} parse gateway response: {e}");
                        continue;
                    }
                }
            }
            Err(e) => {
                eprintln!("{YELLOW}[warn]{RESET} gateway request failed: {e}");
                continue;
            }
        };

        if answer.trim().is_empty() {
            eprintln!("{YELLOW}[warn]{RESET} empty answer from gateway");
            continue;
        }

        eprintln!("{GREEN}[speaking]{RESET} {answer}");
        if let Err(e) = tts.speak(&answer).await {
            eprintln!("{YELLOW}[warn]{RESET} tts failed: {e}");
        }

        if once {
            break;
        }
    }

    Ok(())
}

/// Non-macOS stub: mic capture (cpal/CoreAudio) is macOS-only, so the loop is
/// unavailable on Linux/Windows fleet nodes. Run `ff voice` on the leader (Taylor).
#[cfg(not(target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
pub async fn handle_voice(
    _device: Option<String>,
    _model: String,
    _gateway: String,
    _voice: String,
    _once: bool,
    _whisper_cli: String,
) -> Result<()> {
    anyhow::bail!(
        "`ff voice` is supported on macOS only (CoreAudio mic capture). \
         Run it on the leader (Taylor)."
    )
}

/// Strip a leading "jarvis" (with optional trailing comma) from a transcript,
/// returning the remaining query, trimmed.
#[cfg(target_os = "macos")]
fn strip_wake_word(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    if let Some(idx) = lower.find("jarvis") {
        let after = &text[idx + "jarvis".len()..];
        return after
            .trim_start()
            .trim_start_matches([',', '.', '!', '?'])
            .trim()
            .to_string();
    }
    text.trim().to_string()
}
