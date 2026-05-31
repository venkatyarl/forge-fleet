//! Native microphone capture with energy-based VAD for the `ff voice` loop.
//!
//! Uses [`cpal`] to open an input device, downmixes to mono, resamples to
//! 16 kHz `i16` PCM, and emits each finished utterance (delimited by trailing
//! silence) as a `Vec<i16>` over a [`tokio::sync::mpsc`] channel.
//!
//! ## Threading
//!
//! cpal's [`cpal::Stream`] is `!Send` and CoreAudio requires the thread that
//! built the stream to keep owning it for the lifetime of the audio callback.
//! We therefore build and hold the `Stream` on a dedicated `std::thread` that
//! never moves it across an `.await`. The callback pushes resampled mono i16
//! samples into the VAD state machine; completed utterances are forwarded to
//! the async world through an `mpsc` channel.
//!
//! ## Resampling (Phase 1, naive)
//!
//! The Mac Studio's real input device (a "C920" webcam, since the Studio has no
//! built-in mic) typically only offers 48 kHz, possibly stereo. We downmix to
//! mono by averaging channels and resample to 16 kHz with a simple
//! integer-decimation / linear step. This is intentionally naive — good enough
//! to feed whisper.cpp for Phase 1, not a high-quality SRC.

use std::sync::mpsc as std_mpsc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use hound::{SampleFormat, WavSpec, WavWriter};

use crate::{Result, VoiceError};

/// Target sample rate fed to whisper.cpp.
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Configuration for [`MicCapture`].
#[derive(Debug, Clone)]
pub struct MicCaptureConfig {
    /// Substring to match against input device names (case-insensitive).
    /// e.g. `Some("C920")` to pick the webcam mic. `None` uses the default
    /// input device.
    pub device: Option<String>,
    /// RMS energy threshold (on i16 amplitude scale) above which a frame is
    /// considered speech. Tune per-mic; 500 is a reasonable starting point.
    pub energy_threshold: f32,
    /// Trailing silence (ms) that ends an utterance once speech has started.
    pub silence_tail_ms: u32,
    /// Minimum utterance length (ms); shorter blips are discarded as noise.
    pub min_utterance_ms: u32,
}

impl Default for MicCaptureConfig {
    fn default() -> Self {
        Self {
            device: None,
            energy_threshold: 500.0,
            silence_tail_ms: 700,
            min_utterance_ms: 300,
        }
    }
}

/// Microphone capture handle.
///
/// Call [`MicCapture::start`] to spin up the dedicated audio thread and obtain
/// a receiver of finished utterances.
pub struct MicCapture {
    config: MicCaptureConfig,
}

impl MicCapture {
    pub fn new(config: MicCaptureConfig) -> Self {
        Self { config }
    }

    /// Start capturing. Spawns a dedicated `std::thread` that owns the cpal
    /// `Stream` (which is `!Send`) and returns a `tokio::sync::mpsc::Receiver`
    /// that yields one `Vec<i16>` (16 kHz mono) per detected utterance.
    ///
    /// The returned [`CaptureHandle`] keeps the audio thread alive; dropping it
    /// signals the thread to tear down the stream and exit.
    pub fn start(&self) -> Result<(tokio::sync::mpsc::Receiver<Vec<i16>>, CaptureHandle)> {
        // Async channel out to the consumer (the voice command loop).
        let (utterance_tx, utterance_rx) = tokio::sync::mpsc::channel::<Vec<i16>>(8);
        // Sync channel to report the stream-build result back from the thread.
        let (ready_tx, ready_rx) = std_mpsc::channel::<std::result::Result<(), String>>();
        // Stop signal: dropping CaptureHandle closes this, waking the thread.
        let (stop_tx, stop_rx) = std_mpsc::channel::<()>();

        let config = self.config.clone();

        let join = std::thread::Builder::new()
            .name("ff-voice-mic".to_string())
            .spawn(move || {
                run_capture_thread(config, utterance_tx, ready_tx, stop_rx);
            })
            .map_err(|e| VoiceError::Audio(format!("failed to spawn mic thread: {e}")))?;

        // Wait for the thread to report whether the stream built successfully.
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(VoiceError::Audio(e)),
            Err(_) => {
                return Err(VoiceError::Audio(
                    "mic thread exited before reporting readiness".to_string(),
                ));
            }
        }

        Ok((
            utterance_rx,
            CaptureHandle {
                _stop_tx: stop_tx,
                join: Some(join),
            },
        ))
    }
}

/// RAII handle that keeps the audio thread (and its `!Send` cpal stream) alive.
/// Dropping it signals the thread to stop and joins it.
pub struct CaptureHandle {
    _stop_tx: std_mpsc::Sender<()>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        // Dropping `_stop_tx` closes the channel; the thread's `recv` returns
        // Err and it tears down the stream. Join to be tidy.
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Body of the dedicated audio thread. Builds + owns the cpal `Stream` for its
/// whole lifetime (never crosses an await — this is a plain std thread).
fn run_capture_thread(
    config: MicCaptureConfig,
    utterance_tx: tokio::sync::mpsc::Sender<Vec<i16>>,
    ready_tx: std_mpsc::Sender<std::result::Result<(), String>>,
    stop_rx: std_mpsc::Receiver<()>,
) {
    let host = cpal::default_host();

    // Resolve the input device by name substring, else default input.
    let device = match resolve_input_device(&host, config.device.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    let supported = match device.default_input_config() {
        Ok(c) => c,
        Err(e) => {
            let _ = ready_tx.send(Err(format!("default_input_config failed: {e}")));
            return;
        }
    };

    let in_sample_rate = supported.sample_rate().0;
    let in_channels = supported.channels() as usize;
    let stream_config: cpal::StreamConfig = supported.config();

    // VAD state lives in the closure; emits utterances via utterance_tx.
    let mut vad = VadState::new(&config, in_sample_rate, in_channels, utterance_tx);

    let err_fn = |e| {
        tracing::error!("ff-voice mic stream error: {e}");
    };

    // Build a stream matching the device's native sample format. The callback
    // converts to f32 mono, then the VAD resamples to 16 kHz i16.
    let build_result = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &stream_config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                vad.push_f32(data);
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            &stream_config,
            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                let f: Vec<f32> = data.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
                vad.push_f32(&f);
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            &stream_config,
            move |data: &[u16], _: &cpal::InputCallbackInfo| {
                let f: Vec<f32> = data
                    .iter()
                    .map(|&s| (s as f32 - u16::MAX as f32 / 2.0) / (u16::MAX as f32 / 2.0))
                    .collect();
                vad.push_f32(&f);
            },
            err_fn,
            None,
        ),
        other => {
            let _ = ready_tx.send(Err(format!("unsupported sample format: {other:?}")));
            return;
        }
    };

    let stream = match build_result {
        Ok(s) => s,
        Err(e) => {
            let _ = ready_tx.send(Err(format!("build_input_stream failed: {e}")));
            return;
        }
    };

    if let Err(e) = stream.play() {
        let _ = ready_tx.send(Err(format!("stream.play failed: {e}")));
        return;
    }

    // Stream built and playing — tell the spawner we're live.
    let _ = ready_tx.send(Ok(()));

    // Keep the !Send stream alive on THIS thread until the handle is dropped.
    // `stop_rx.recv()` blocks until the CaptureHandle drops its sender.
    let _ = stop_rx.recv();
    drop(stream);
}

/// Pick an input device by name substring, falling back to the default input.
fn resolve_input_device(
    host: &cpal::Host,
    name_substr: Option<&str>,
) -> std::result::Result<cpal::Device, String> {
    if let Some(substr) = name_substr {
        let needle = substr.to_ascii_lowercase();
        let devices = host
            .input_devices()
            .map_err(|e| format!("enumerate input devices: {e}"))?;
        for dev in devices {
            if let Ok(name) = dev.name()
                && name.to_ascii_lowercase().contains(&needle)
            {
                tracing::info!("ff-voice mic: matched input device '{name}'");
                return Ok(dev);
            }
        }
        return Err(format!(
            "no input device name contains '{substr}'; check `ff voice` --device"
        ));
    }

    host.default_input_device()
        .ok_or_else(|| "no default input device available".to_string())
}

/// Energy-VAD + downmix + naive resampler state machine.
///
/// Fed mono-or-multichannel f32 frames from the cpal callback; accumulates
/// resampled 16 kHz mono i16 samples and emits a complete utterance once
/// trailing silence is observed.
struct VadState {
    energy_threshold: f32,
    silence_tail_samples: usize,
    min_utterance_samples: usize,
    in_channels: usize,
    // Decimation step: in_rate / TARGET_SAMPLE_RATE as a float accumulator.
    resample_ratio: f32,
    resample_pos: f32,

    in_speech: bool,
    silence_run: usize,
    current: Vec<i16>,
    tx: tokio::sync::mpsc::Sender<Vec<i16>>,
}

impl VadState {
    fn new(
        config: &MicCaptureConfig,
        in_sample_rate: u32,
        in_channels: usize,
        tx: tokio::sync::mpsc::Sender<Vec<i16>>,
    ) -> Self {
        // Silence/min thresholds are in 16 kHz output samples.
        let silence_tail_samples =
            (TARGET_SAMPLE_RATE as u64 * config.silence_tail_ms as u64 / 1_000) as usize;
        let min_utterance_samples =
            (TARGET_SAMPLE_RATE as u64 * config.min_utterance_ms as u64 / 1_000) as usize;
        Self {
            energy_threshold: config.energy_threshold,
            silence_tail_samples,
            min_utterance_samples,
            in_channels: in_channels.max(1),
            resample_ratio: in_sample_rate as f32 / TARGET_SAMPLE_RATE as f32,
            resample_pos: 0.0,
            in_speech: false,
            silence_run: 0,
            current: Vec::new(),
            tx,
        }
    }

    /// Push a block of interleaved f32 samples (channels interleaved).
    fn push_f32(&mut self, data: &[f32]) {
        // Downmix to mono by averaging channels.
        let ch = self.in_channels;
        let frames = if ch == 0 { 0 } else { data.len() / ch };
        for frame in 0..frames {
            let mut sum = 0.0f32;
            for c in 0..ch {
                sum += data[frame * ch + c];
            }
            let mono = sum / ch as f32;

            // Naive resample: emit one output sample every `resample_ratio`
            // input frames via a float accumulator (decimation / nearest).
            self.resample_pos += 1.0;
            if self.resample_pos >= self.resample_ratio {
                self.resample_pos -= self.resample_ratio;
                let s = (mono.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                self.feed_sample(s);
            }
        }
    }

    /// Run the VAD state machine over a single 16 kHz mono sample.
    fn feed_sample(&mut self, sample: i16) {
        let amp = (sample as f32).abs();
        let is_voiced = amp > self.energy_threshold;

        if is_voiced {
            self.in_speech = true;
            self.silence_run = 0;
            self.current.push(sample);
        } else if self.in_speech {
            // Trailing silence — keep buffering so we don't clip word tails.
            self.silence_run += 1;
            self.current.push(sample);
            if self.silence_run >= self.silence_tail_samples {
                self.finish_utterance();
            }
        }
        // else: pre-speech silence, drop the sample.
    }

    fn finish_utterance(&mut self) {
        let utterance = std::mem::take(&mut self.current);
        self.in_speech = false;
        self.silence_run = 0;
        if utterance.len() >= self.min_utterance_samples {
            // Non-blocking send; if the consumer is gone or full we drop it.
            if let Err(e) = self.tx.try_send(utterance) {
                tracing::warn!("ff-voice: dropped utterance ({e})");
            }
        }
    }
}

/// Write a 16 kHz mono `i16` PCM buffer to a temp `.wav` via `hound` and return
/// its path. whisper-cli reads `-f <file>`.
pub fn write_temp_wav(samples: &[i16]) -> Result<std::path::PathBuf> {
    let path = std::env::temp_dir().join(format!("ff-voice-cap-{}.wav", uuid::Uuid::new_v4()));
    let spec = WavSpec {
        channels: 1,
        sample_rate: TARGET_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut writer = WavWriter::create(&path, spec)
        .map_err(|e| VoiceError::Audio(format!("create wav: {e}")))?;
    for &s in samples {
        writer
            .write_sample(s)
            .map_err(|e| VoiceError::Audio(format!("write wav sample: {e}")))?;
    }
    writer
        .finalize()
        .map_err(|e| VoiceError::Audio(format!("finalize wav: {e}")))?;
    Ok(path)
}

/// Encode a 16 kHz mono `i16` PCM buffer to an in-memory WAV byte vector.
/// Useful when feeding `SttEngine::transcribe`, which takes `&[u8]`.
pub fn encode_wav_bytes(samples: &[i16]) -> Result<Vec<u8>> {
    let spec = WavSpec {
        channels: 1,
        sample_rate: TARGET_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
    {
        let mut writer = WavWriter::new(&mut cursor, spec)
            .map_err(|e| VoiceError::Audio(format!("create in-memory wav: {e}")))?;
        for &s in samples {
            writer
                .write_sample(s)
                .map_err(|e| VoiceError::Audio(format!("write wav sample: {e}")))?;
        }
        writer
            .finalize()
            .map_err(|e| VoiceError::Audio(format!("finalize wav: {e}")))?;
    }
    Ok(cursor.into_inner())
}
