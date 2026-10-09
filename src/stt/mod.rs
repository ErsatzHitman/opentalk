//! Cloud speech-to-text. Batch providers implement `BatchProvider`; realtime ones
//! implement `StreamingProvider`. `transcribe()` dispatches on the active config.
//! TASK stt-batch owns batch providers + errors + dispatch; TASK stt-stream owns streaming + Corti.
pub mod errors;
pub mod providers;
pub mod registry;
pub mod streaming;

use crate::core::config::{Config, SttProviderId};
#[allow(unused_imports)]
pub use errors::{SttError, SttErrorCode};

/// 16-bit PCM mono audio.
#[derive(Clone, Debug, Default)]
pub struct Audio {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
}

impl Audio {
    pub fn duration_ms(&self) -> u64 {
        if self.sample_rate == 0 { 0 } else { self.samples.len() as u64 * 1000 / self.sample_rate as u64 }
    }

    /// Canonical 44-byte-header PCM WAV.
    pub fn to_wav(&self) -> Vec<u8> {
        let data_len = (self.samples.len() * 2) as u32;
        let mut v = Vec::with_capacity(44 + data_len as usize);
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data_len).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // PCM
        v.extend_from_slice(&1u16.to_le_bytes()); // mono
        v.extend_from_slice(&self.sample_rate.to_le_bytes());
        v.extend_from_slice(&(self.sample_rate * 2).to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        for s in &self.samples {
            v.extend_from_slice(&s.to_le_bytes());
        }
        v
    }

    /// Parse a PCM16 WAV (multi-channel is averaged to mono). Used by `--transcribe-file` and tests.
    pub fn from_wav(bytes: &[u8]) -> Option<Audio> {
        if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
            return None;
        }
        let (mut pos, mut rate, mut channels, mut bits) = (12usize, 0u32, 1u16, 16u16);
        while pos + 8 <= bytes.len() {
            let id = &bytes[pos..pos + 4];
            let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().ok()?) as usize;
            let body = pos + 8;
            if id == b"fmt " && body + 16 <= bytes.len() {
                channels = u16::from_le_bytes([bytes[body + 2], bytes[body + 3]]);
                rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().ok()?);
                bits = u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]);
            } else if id == b"data" {
                if bits != 16 || channels == 0 {
                    return None;
                }
                let end = (body + len).min(bytes.len());
                let frames: Vec<i16> = bytes[body..end]
                    .chunks_exact(2 * channels as usize)
                    .map(|f| {
                        let sum: i32 = f.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as i32).sum();
                        (sum / channels as i32) as i16
                    })
                    .collect();
                return Some(Audio { samples: frames, sample_rate: rate });
            }
            pos = body + len + (len & 1);
        }
        None
    }
}

/// Per-request options derived from config.
#[derive(Clone, Debug, Default)]
pub struct SttOptions {
    pub model: String,
    /// None = auto-detect.
    pub language: Option<String>,
    /// Dictionary + snippet trigger hints (raw words; providers apply their own budgets).
    pub hints: Vec<String>,
    pub timeout_ms: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Transcript {
    pub text: String,
    pub detected_language: Option<String>,
}

/// Credentials resolved from `secrets` + config for one provider.
#[derive(Clone, Debug, Default)]
pub struct Credentials {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    /// Provider-specific extras (e.g. corti client_secret/environment/tenant).
    pub extra: Vec<(String, String)>,
}

pub trait BatchProvider: Send + Sync {
    fn id(&self) -> SttProviderId;
    /// URL whose origin should be pre-warmed when recording starts.
    fn prewarm_url(&self, creds: &Credentials) -> Option<String>;
    fn transcribe(&self, audio: &Audio, opts: &SttOptions, creds: &Credentials) -> Result<Transcript, SttError>;
}

/// A live streaming session: feed 16 kHz mono PCM while recording, then finish.
pub trait StreamingSession: Send {
    fn push(&mut self, pcm: &[i16]);
    fn finish(self: Box<Self>) -> Result<Transcript, SttError>;
    fn cancel(self: Box<Self>);
}

pub trait StreamingProvider: Send + Sync {
    fn id(&self) -> SttProviderId;
    fn start(&self, opts: &SttOptions, creds: &Credentials) -> Result<Box<dyn StreamingSession>, SttError>;
}

/// Resolve credentials for the active provider from secrets + config.
pub fn credentials(cfg: &Config) -> Credentials {
    let _ = cfg;
    todo!("stt-batch")
}

pub fn options(cfg: &Config) -> SttOptions {
    let _ = cfg;
    todo!("stt-batch")
}

/// Batch transcription with the active provider (streaming-only providers return
/// `SttErrorCode::StreamingOnly`). Applies the dictionary-echo filter.
pub fn transcribe(cfg: &Config, audio: &Audio) -> Result<Transcript, SttError> {
    let _ = (cfg, audio);
    todo!("stt-batch")
}

/// Start a streaming session if the active provider/model streams and the config prefers it.
pub fn start_stream(cfg: &Config) -> Option<Result<Box<dyn StreamingSession>, SttError>> {
    let _ = cfg;
    todo!("stt-stream")
}

/// Pre-warm the active provider's connection (called at hotkey press).
pub fn prewarm(cfg: &Config) {
    let _ = cfg;
    todo!("stt-batch")
}

pub fn provider_for(id: SttProviderId) -> &'static dyn BatchProvider {
    providers::get(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wav_roundtrip() {
        let a = Audio { samples: vec![0, 1000, -1000, i16::MAX, i16::MIN], sample_rate: 16000 };
        let b = Audio::from_wav(&a.to_wav()).unwrap();
        assert_eq!(b.samples, a.samples);
        assert_eq!(b.sample_rate, 16000);
    }
}
