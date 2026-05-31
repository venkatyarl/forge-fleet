# NATIVE JARVIS Voice Loop — Implementation Plan (Taylor)

> Status: DESIGN, ready for operator review (2026-05-31). Browser speech in the
> HUD already works today; this is the always-listening native "Jarvis" wake-word
> upgrade. Phase-1 end-to-end test needs two human-in-the-loop steps that cannot
> be automated: `brew install whisper-cpp` and approving the macOS microphone
> (TCC) permission prompt on first run.

## TL;DR

Ship Phase 1 with the abstractions that **already exist in `crates/ff-voice`** plus
a thin new `ff voice` subcommand. Stack:
- **mic capture** via `cpal` (pure-Rust CoreAudio, no Python/ffmpeg)
- **wake-word** via the existing transcript-keyword `WakeWordDetector` over a rolling
  whisper.cpp window (zero new dep; "jarvis" is just a configured phrase)
- **STT** via whisper.cpp `whisper-cli` + `ggml-base.en` through the crate's
  `LocalWhisperEngine` (its `{input}` temp-file adapter is built for this)
- **brain** = POST the live `/api/jarvis/ask` on `localhost:51002` (already returns a
  concise British "sir" answer; **request field is `query`**)
- **TTS** via `say -v Daniel` (macOS built-in, instant, British, zero install)

New install on Taylor: `brew install whisper-cpp` + one `ggml-base.en.bin`.
Phase 2 upgrades wake-word → Picovoice Porcupine (prebuilt "Jarvis" keyword, lower
idle CPU) and TTS → Piper British voice.

## What's already on Taylor

| Tool | Present? | Role |
|---|---|---|
| `say` (`/usr/bin/say`) | ✅ | Phase-1 TTS; `-v Daniel` (en_GB) verified |
| British voices | ✅ Daniel/Eddy/Flo/Reed/Sandy/Shelley | "Daniel" is the pick |
| `whisper-cpp` | ❌ but bottled `1.8.5` available | Phase-1 STT (`brew install whisper-cpp` → `whisper-cli`) |
| `whisper` (Python openai-whisper) | ⚠️ present | AVOID — slow cold-start |
| `ffmpeg` | ✅ 8.0.1 | fallback capture only; cpal preferred |
| cargo/rustc 1.93.1 | ✅ | toolchain |
| Mic devices | ✅ C920 webcam, iPhone mic | **No built-in mic** — select C920 by name |
| Gateway `:51002` `/api/jarvis/ask` | ✅ live | the brain, already done |
| `sox`/`piper`/`porcupine`/ggml models | ❌ | Phase-2 / must download |

**Pre-existing asset:** `crates/ff-voice` already ships `SttEngine`/`LocalWhisperEngine`
(`LocalWhisperConfig::default().command == "whisper-cli"`, args `["-f","{input}"]`),
`WakeWordDetector` (normalized keyword spotting), `VoicePipeline`, `AudioChunk`, and
`TtsEngine`. Currently cloud-oriented (TTS=ElevenLabs, a Twilio module). Phase 1 ADDS a
native `say` TTS engine + a cpal capture module; changes nothing existing.

## Component decisions

- **Wake-word** — Phase 1: always-on whisper.cpp + existing `WakeWordDetector` ("jarvis"
  is just config; ~always-on CPU, ~1-2s latency, false-trigger prone). Phase 2: Picovoice
  Porcupine `BuiltinKeywords::Jarvis` (`pv_porcupine` crate; free tier ≤3 users, needs an
  AccessKey in `fleet_secrets`, far lower idle CPU). Build wake-word behind a trait so the
  swap is a config flip.
- **STT** — whisper.cpp `whisper-cli` + `ggml-base.en` (faster-than-real-time on M3 Ultra;
  `small.en` is the accuracy upgrade). The crate already shells to it; we just supply `-m`.
- **TTS** — `say -v Daniel` (Phase 1, free/instant/British); Piper British ONNX voice
  (e.g. `en_GB-alan-medium`) in Phase 2, behind the existing `TtsEngine` trait.

## Architecture

A new **`ff voice` subcommand in `crates/ff-terminal`** (NOT a new crate, NOT inside
`forgefleetd`). Rationale: operator wants things "through `ff`"; the heavy lifting already
lives in `ff-voice`; keep always-on mic/CoreAudio threads + the TCC prompt OUT of the
headless core daemon. Voice runs only on Taylor where the operator sits (login LaunchAgent
later via `ff voice --daemon`, but the daemon does not own it).

```
crates/ff-voice/src/
  capture.rs      NEW — cpal CoreAudio mic → 16kHz mono i16 PCM ring buffer + energy VAD
  tts_native.rs   NEW — `SayTts` impl of TtsEngine (spawns `say -v Daniel`)
  lib.rs          EDIT — pub mod capture; pub mod tts_native; + re-exports
  (stt.rs, wake_word.rs, pipeline.rs, audio.rs reused as-is)

crates/ff-terminal/src/
  voice_cmd.rs    NEW — `ff voice` loop: capture → VAD segment → STT → wake-word →
                        strip wake-word → POST /api/jarvis/ask {"query":..} → say(answer)
  main.rs/lib.rs  EDIT — add `Voice {..}` clap variant + dispatch (mirror research_cmd)
```

Cargo deps (`crates/ff-voice/Cargo.toml`): `cpal = "0.16"`, `hound = "3.5"`; `reqwest`
already present. Phase 2: `pv_porcupine = "3"` + shell to `piper`.

## Phase 1 build steps (ordered)

1. `brew install whisper-cpp` on Taylor → `/opt/homebrew/bin/whisper-cli`.
2. Download `ggml-base.en.bin` into `~/models/whisper/`; pass via `-m`.
3. Add `cpal = "0.16"`, `hound = "3.5"` to `crates/ff-voice/Cargo.toml`.
4. `capture.rs`: open input by NAME (C920; `--device` override), cpal 16kHz mono stream
   (downmix/resample if device forces 48kHz stereo), ring buffer, energy-VAD cut on ~700ms
   trailing silence, emit `Vec<i16>` over a tokio channel. Keep the cpal `Stream` on its own
   dedicated thread (CoreAudio owns the callback thread).
5. `tts_native.rs`: `SayTts` impl of `TtsEngine`; `speak(text)` spawns `say -v Daniel`.
6. `voice_cmd.rs`: wire `LocalWhisperEngine{command:"whisper-cli", args:["-m","<model>","-f","{input}","-otxt","-nt"]}`,
   `WakeWordDetector` on `["jarvis"]`, loop segment→WAV(hound)→STT→detect→strip→POST→say.
   Flags: `--device --model --gateway http://localhost:51002 --voice Daniel --once`.
7. Register the subcommand in main.rs/lib.rs.
8. Build + **codesign** (mic makes this mandatory): `cargo build --release -p ff-terminal --bin ff`
   → `install -m 755 target/release/ff ~/.local/bin/ff` → `codesign --force --sign - ~/.local/bin/ff`.
9. **First run grants TCC mic permission** (HUMAN STEP): `ff voice --once` from Terminal,
   approve the mic prompt, say "Jarvis, status" → Daniel reads the live fleet summary.

## Risks / gotchas

- **TCC mic permission** — first capture prompts macOS, attributed to the launching app
  (Terminal/LaunchAgent), not `ff`. Cannot be auto-approved; do it interactively first.
  A headless cron/daemon with no UI session will silently fail capture.
- **Codesign / SIGKILL** — `cp` breaks the ad-hoc signature (Exit 137) AND drops the TCC
  grant. Always `install -m 755` + `codesign --force --sign -`. Re-signing may require
  re-granting mic permission.
- **No built-in mic** — system default may be a virtual device; select C920 by name,
  expose `--device`.
- **Always-on STT cost + false wakes** — continuous whisper keeps a model warm; "jarvis"
  matches mishear "Travis"/substrings. Gate on confidence/length + require wake-word near
  utterance start. This is why Phase 2 = Porcupine.
- **Latency** — base.en STT sub-second; built-in intents (status/fleet/tasks) answer from
  Postgres in ms; free-form questions hit a remote 30B (gateway 60s ceiling) — consider a
  spoken "one moment, sir" filler.
- **cpal sample-rate** — C920 may only offer 48kHz stereo; downmix→mono + resample→16kHz
  before whisper-cli or accuracy degrades.
- **Porcupine AccessKey (Phase 2)** — store in `fleet_secrets`, not a file.
- **Gateway URL** — make it a flag; don't hardcode the port (code default is `0.0.0.0:8787`,
  Taylor runs 51002).

## Files of record
- `crates/ff-voice/src/{stt.rs,wake_word.rs,pipeline.rs,tts.rs,audio.rs,lib.rs}` (reused)
- `crates/ff-gateway/src/jarvis_api.rs` — `/api/jarvis/ask` contract (`AskReq.query`)
- `crates/ff-gateway/src/server.rs` — route mounts
- `CLAUDE.md` — codesign rule
