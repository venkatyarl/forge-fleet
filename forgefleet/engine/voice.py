"""Voice Interface — speech-to-text + text-to-speech for fleet control.

Item #11: "Hey ForgeFleet, what's the fleet status?"
Uses local whisper.cpp (STT) and llama-tts (TTS). No cloud APIs.
"""
import os
import subprocess
import tempfile
from dataclasses import dataclass


@dataclass
class VoiceInterface:
    """Local voice I/O for ForgeFleet.
    
    STT: whisper.cpp (local, no API)
    TTS: llama-tts or say (macOS) / espeak (Linux)
    """
    whisper_model: str = ""
    tts_engine: str = "system"  # "system" (say/espeak), "llama-tts"
    
    def speech_to_text(self, audio_path: str) -> str:
        """Convert audio file to text using whisper.cpp."""
        try:
            # Try whisper-cli first (whisper.cpp)
            r = subprocess.run(
                ["whisper-cli", "-m", self._whisper_model_path(),
                 "-f", audio_path, "--no-timestamps"],
                capture_output=True, text=True, timeout=30,
            )
            if r.returncode == 0:
                return r.stdout.strip()
        except FileNotFoundError:
            pass
        
        # Fallback to OpenAI whisper Python
        try:
            r = subprocess.run(
                ["whisper", audio_path, "--model", "base", "--output_format", "txt"],
                capture_output=True, text=True, timeout=60,
            )
            if r.returncode == 0:
                # Read the output txt file
                txt_path = audio_path.rsplit(".", 1)[0] + ".txt"
                if os.path.exists(txt_path):
                    text = open(txt_path).read().strip()
                    os.remove(txt_path)
                    return text
        except FileNotFoundError:
            pass
        
        return ""
    
    def text_to_speech(self, text: str, output_path: str = "") -> str:
        """Convert text to speech audio.
        
        Returns path to generated audio file.
        """
        if not output_path:
            output_path = tempfile.mktemp(suffix=".wav")
        
        if self.tts_engine == "system":
            return self._system_tts(text, output_path)
        elif self.tts_engine == "llama-tts":
            return self._llama_tts(text, output_path)
        
        return ""
    
    def _system_tts(self, text: str, output_path: str) -> str:
        """Use macOS 'say' or Linux 'espeak' for TTS."""
        import platform
        
        if platform.system() == "Darwin":
            # macOS say command
            subprocess.run(
                ["say", "-o", output_path, "--data-format=LEF32@22050", text],
                capture_output=True, timeout=30,
            )
        else:
            # Linux espeak
            subprocess.run(
                ["espeak", "-w", output_path, text],
                capture_output=True, timeout=30,
            )
        
        return output_path if os.path.exists(output_path) else ""
    
    def _llama_tts(self, text: str, output_path: str) -> str:
        """Use llama-tts (llama.cpp TTS) for local voice generation."""
        try:
            r = subprocess.run(
                ["llama-tts", "--text", text, "--output", output_path],
                capture_output=True, text=True, timeout=30,
            )
            return output_path if r.returncode == 0 and os.path.exists(output_path) else ""
        except FileNotFoundError:
            return self._system_tts(text, output_path)
    
    def _whisper_model_path(self) -> str:
        """Find the whisper model file."""
        if self.whisper_model:
            return self.whisper_model
        
        # Common locations
        for path in [
            os.path.expanduser("~/models/whisper/ggml-base.en.bin"),
            os.path.expanduser("~/whisper.cpp/models/ggml-base.en.bin"),
            "/usr/local/share/whisper/ggml-base.en.bin",
        ]:
            if os.path.exists(path):
                return path
        
        return "ggml-base.en.bin"
    
    def speak(self, text: str):
        """Quick speak — just output audio to speakers."""
        import platform
        if platform.system() == "Darwin":
            subprocess.run(["say", text], capture_output=True, timeout=30)
        else:
            subprocess.run(["espeak", text], capture_output=True, timeout=30)
    
    @staticmethod
    def is_available() -> dict:
        """Check which voice capabilities are available."""
        available = {"stt": False, "tts": False}
        
        try:
            subprocess.run(["whisper-cli", "--help"], capture_output=True, timeout=3)
            available["stt"] = True
        except Exception:
            try:
                subprocess.run(["whisper", "--help"], capture_output=True, timeout=3)
                available["stt"] = True
            except Exception:
                pass
        
        import platform
        if platform.system() == "Darwin":
            available["tts"] = True  # macOS always has 'say'
        else:
            try:
                subprocess.run(["espeak", "--version"], capture_output=True, timeout=3)
                available["tts"] = True
            except Exception:
                pass
        
        return available
