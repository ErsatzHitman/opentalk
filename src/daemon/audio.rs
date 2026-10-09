//! Microphone capture via raw WASAPI (shared mode, event-driven, AUTOCONVERTPCM to 16 kHz mono
//! i16 so the OS does the resampling). TASK audio implements.
//!
//! - `Recorder::start` must return in a few ms; device activation cost is logged.
//! - The capture thread publishes RMS of the latest ~43 ms window into `level` (f32 bits).
//! - Optional `sink` receives each PCM packet for streaming STT.
//! - `vad` offers energy-based speech detection + silence trimming (150 ms padding).
use crate::stt::Audio;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

pub type PcmSink = Box<dyn FnMut(&[i16]) + Send>;

pub struct Recorder {
    _private: (),
}

impl Recorder {
    /// Start capturing from `device_id` (None = default communications/console capture device).
    pub fn start(device_id: Option<&str>, level: Arc<AtomicU32>, sink: Option<PcmSink>) -> Result<Recorder, String> {
        let _ = (device_id, level, sink);
        todo!("audio")
    }
    /// Stop and return everything captured (16 kHz mono).
    pub fn stop(self) -> Audio {
        todo!("audio")
    }
    pub fn cancel(self) {
        todo!("audio")
    }
    /// Milliseconds captured so far.
    pub fn elapsed_ms(&self) -> u64 {
        todo!("audio")
    }
}

/// Active capture endpoints with friendly names.
pub fn list_devices() -> Vec<Device> {
    todo!("audio")
}

/// Initialise COM + cache the device enumerator so the first `start` is fast.
pub fn warmup() {
    todo!("audio")
}

/// Read the published level.
pub fn read_level(level: &AtomicU32) -> f32 {
    f32::from_bits(level.load(std::sync::atomic::Ordering::Relaxed))
}

pub mod vad {
    use crate::stt::Audio;

    #[derive(Clone, Copy, Debug, Default, PartialEq)]
    pub struct VadInfo {
        /// Total milliseconds classified as speech.
        pub speech_ms: u64,
        pub first_speech_ms: Option<u64>,
        pub last_speech_ms: Option<u64>,
        pub peak_rms: f32,
    }

    /// Energy-based analysis over 20 ms frames (thresholds inspired by OpenWhispr localSpeechGate:
    /// silence < 0.002 rms, speech window rms >= 0.003 && peak >= 0.02).
    pub fn analyze(audio: &Audio) -> VadInfo {
        let _ = audio;
        todo!("audio")
    }

    /// Trim leading/trailing non-speech keeping `pad_ms` around speech. No speech -> unchanged.
    pub fn trim(audio: &Audio, pad_ms: u64) -> Audio {
        let _ = (audio, pad_ms);
        todo!("audio")
    }
}
