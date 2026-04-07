//! Audio helpers for chunking and metadata.

use bytes::Bytes;

use crate::{Result, VoiceError};

/// Sample-rate in Hertz.
pub type SampleRate = u32;

/// Supported audio container/encoding formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioFormat {
    Wav,
    Mp3,
    Ogg,
    PcmS16Le,
    Mulaw,
    Alaw,
    Unknown,
}

impl AudioFormat {
    /// Guess format from common MIME types.
    pub fn from_mime(mime: &str) -> Self {
        let m = mime.trim().to_ascii_lowercase();
        match m.as_str() {
            "audio/wav" | "audio/wave" | "audio/x-wav" => Self::Wav,
            "audio/mpeg" | "audio/mp3" => Self::Mp3,
            "audio/ogg" | "audio/opus" => Self::Ogg,
            "audio/pcm" | "audio/l16" | "audio/raw" => Self::PcmS16Le,
            "audio/mulaw" | "audio/ulaw" => Self::Mulaw,
            "audio/alaw" => Self::Alaw,
            _ => Self::Unknown,
        }
    }

    /// Guess format from a file extension (with or without leading dot).
    pub fn from_extension(ext: &str) -> Self {
        let e = ext.trim_start_matches('.').to_ascii_lowercase();
        match e.as_str() {
            "wav" | "wave" => Self::Wav,
            "mp3" => Self::Mp3,
            "ogg" | "opus" => Self::Ogg,
            "pcm" | "raw" | "s16le" => Self::PcmS16Le,
            "mulaw" | "ulaw" => Self::Mulaw,
            "alaw" => Self::Alaw,
            _ => Self::Unknown,
        }
    }
}

/// Basic audio metadata needed for chunking and duration estimation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AudioMetadata {
    pub format: AudioFormat,
    pub sample_rate: SampleRate,
    pub channels: u16,
    pub bits_per_sample: u16,
}

impl Default for AudioMetadata {
    fn default() -> Self {
        Self {
            format: AudioFormat::Unknown,
            sample_rate: 16_000,
            channels: 1,
            bits_per_sample: 16,
        }
    }
}

impl AudioMetadata {
    /// Estimated number of bytes per second for linear PCM-like formats.
    pub fn bytes_per_second(&self) -> Option<usize> {
        if self.sample_rate == 0 || self.channels == 0 || self.bits_per_sample == 0 {
            return None;
        }

        let bytes_per_sample = (self.bits_per_sample as usize).div_ceil(8);
        Some(self.sample_rate as usize * self.channels as usize * bytes_per_sample)
    }

    /// Estimate duration in milliseconds for a byte slice.
    pub fn estimate_duration_ms(&self, byte_len: usize) -> Option<u64> {
        let bps = self.bytes_per_second()? as u64;
        if bps == 0 {
            return None;
        }
        Some((byte_len as u64 * 1_000) / bps)
    }
}

/// A binary audio chunk used by streaming and pipeline modules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioChunk {
    pub sequence: u64,
    pub data: Bytes,
    pub metadata: AudioMetadata,
}

impl AudioChunk {
    pub fn new(sequence: u64, data: impl Into<Bytes>, metadata: AudioMetadata) -> Self {
        Self {
            sequence,
            data: data.into(),
            metadata,
        }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// Split a byte buffer into fixed-size chunks.
pub fn chunk_audio_bytes(
    audio: &[u8],
    chunk_size: usize,
    metadata: AudioMetadata,
) -> Result<Vec<AudioChunk>> {
    if chunk_size == 0 {
        return Err(VoiceError::Audio("chunk_size must be > 0".to_string()));
    }

    Ok(audio
        .chunks(chunk_size)
        .enumerate()
        .map(|(idx, part)| {
            AudioChunk::new(idx as u64, Bytes::copy_from_slice(part), metadata.clone())
        })
        .collect())
}

/// Split audio into chunks based on duration (for PCM-like audio where timing can be estimated).
pub fn chunk_audio_by_duration(
    audio: &[u8],
    chunk_duration_ms: u32,
    metadata: AudioMetadata,
) -> Result<Vec<AudioChunk>> {
    if chunk_duration_ms == 0 {
        return Err(VoiceError::Audio(
            "chunk_duration_ms must be > 0".to_string(),
        ));
    }

    let bps = metadata.bytes_per_second().ok_or_else(|| {
        VoiceError::Audio("cannot estimate bytes/second from metadata".to_string())
    })?;

    let chunk_size = ((bps as u64 * chunk_duration_ms as u64) / 1_000) as usize;
    if chunk_size == 0 {
        return Err(VoiceError::Audio(
            "calculated chunk size is zero; check metadata".to_string(),
        ));
    }

    chunk_audio_bytes(audio, chunk_size, metadata)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_detection_works() {
        assert_eq!(AudioFormat::from_extension("wav"), AudioFormat::Wav);
        assert_eq!(AudioFormat::from_extension(".mp3"), AudioFormat::Mp3);
        assert_eq!(AudioFormat::from_extension("unknown"), AudioFormat::Unknown);
    }

    #[test]
    fn chunking_works() {
        let metadata = AudioMetadata::default();
        let chunks = chunk_audio_bytes(&[1, 2, 3, 4, 5], 2, metadata).unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].data.as_ref(), &[1, 2]);
        assert_eq!(chunks[2].data.as_ref(), &[5]);
    }
}
