//! Realtime STT over WebSocket (tungstenite + native-tls). TASK stt-stream:
//! deepgram.rs, assemblyai.rs, openai_realtime.rs, gemini_live.rs. Input is 16 kHz mono PCM
//! in 50 ms frames; OpenAI Realtime needs 24 kHz upsampling. See docs/PLAN.md §4a.
use super::{SttError, StreamingProvider, StreamingSession};
use crate::core::config::Config;
use crate::core::config::SttProviderId;

pub fn get(id: SttProviderId, model: &str) -> Option<&'static dyn StreamingProvider> {
    let _ = (id, model);
    todo!("stt-stream")
}

/// Start a session for the active provider when `cfg.stt.prefer_streaming` (or the model is
/// streaming-only) and the model supports streaming; None = use batch. Uses `super::credentials`
/// and `super::options` for keys/options.
pub fn start(cfg: &Config) -> Option<Result<Box<dyn StreamingSession>, SttError>> {
    let _ = cfg;
    todo!("stt-stream")
}
