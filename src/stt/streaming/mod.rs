//! Realtime STT over WebSocket (tungstenite + native-tls). TASK stt-stream:
//! deepgram.rs, assemblyai.rs, openai_realtime.rs, gemini_live.rs. Input is 16 kHz mono PCM
//! in 50 ms frames; OpenAI Realtime needs 24 kHz upsampling. See docs/PLAN.md §4a.
use super::StreamingProvider;
use crate::core::config::SttProviderId;

pub fn get(id: SttProviderId, model: &str) -> Option<&'static dyn StreamingProvider> {
    let _ = (id, model);
    todo!("stt-stream")
}
